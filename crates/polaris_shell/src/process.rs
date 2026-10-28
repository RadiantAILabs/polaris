//! Managed shell process lifecycle and streaming primitives.

use crate::error::ShellError;
use crate::executor::{ExecutionResult, ExecutorInner, ShellExecutor, ShellRequest};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{Notify, mpsc, watch};

const READ_BUFFER_BYTES: usize = 8 * 1024;

/// Stable identity of one run accepted by a [`ShellExecutor`].
///
/// Identities are monotonic within an executor and are not operating-system
/// process identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ShellProcessId(pub(crate) u64);

/// Why a managed shell process stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutcome {
    /// The command exited without shell intervention.
    Exited,
    /// The configured command timeout elapsed.
    TimedOut,
    /// External cancellation or an explicit stop request ended the command.
    Cancelled,
}

/// Current lifecycle state of a managed shell process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellProcessState {
    /// The child is running.
    Running,
    /// Termination was requested and cleanup is still in progress.
    Terminating,
    /// The process reached a terminal outcome.
    Finished(ShellOutcome),
}

/// Which operating-system output pipe produced a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Incremental event emitted by a managed shell process.
///
/// `Started` is always the first event and `Finished` is always the last:
/// once the terminal event has been handed to a sink, no further event for
/// that run is delivered. Events suppressed by that rule are counted in
/// [`ExecutionResult::dropped_events`](crate::ExecutionResult::dropped_events).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShellEvent {
    /// The managed run was accepted and identified.
    Started {
        /// Executor-local process identity.
        process: ShellProcessId,
        /// Operating-system process id, absent for cancellation before spawn.
        os_pid: Option<u32>,
        /// Command supplied by the caller.
        command: String,
    },
    /// Raw bytes read from one output pipe.
    Chunk {
        /// Executor-local process identity.
        process: ShellProcessId,
        /// Pipe that produced the bytes.
        stream: ShellStream,
        /// Bytes from one read operation.
        bytes: Vec<u8>,
    },
    /// In-memory capture reached its per-stream limit.
    CaptureLimitReached {
        /// Executor-local process identity.
        process: ShellProcessId,
        /// Pipe whose capture reached the limit.
        stream: ShellStream,
    },
    /// The overflow file for one pipe reached its per-stream spill quota.
    ///
    /// Later bytes from that pipe are neither captured nor spilled, so the
    /// overflow file no longer holds the complete stream.
    OverflowLimitReached {
        /// Executor-local process identity.
        process: ShellProcessId,
        /// Pipe whose overflow file reached the quota.
        stream: ShellStream,
    },
    /// Terminal lifecycle event, emitted after the readers stop.
    Finished {
        /// Executor-local process identity.
        process: ShellProcessId,
        /// Terminal outcome.
        outcome: ShellOutcome,
        /// Child exit code when one is available.
        exit_code: Option<i32>,
    },
}

/// Truthful disposition returned by a non-blocking [`ShellSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEventDelivery {
    /// The sink accepted the event.
    Accepted,
    /// The sink dropped the event.
    Dropped,
}

/// Non-blocking destination for managed shell events.
pub trait ShellSink: Send + Sync + 'static {
    /// Reserves this sink for a run before the executor spawns a child.
    ///
    /// The default permits reuse, suitable for sinks that distinguish runs by
    /// process identity. Single-run sinks must atomically return `true` only
    /// for their first claim; later claims are rejected before spawning.
    fn try_claim(&self) -> bool {
        true
    }

    /// Attempts to deliver one event without blocking the process reader.
    fn on_event(&self, event: ShellEvent) -> ShellEventDelivery;
}

/// Cloneable type-erased shell event sink for requests and tool contexts.
#[derive(Clone)]
pub struct ShellSinkHandle(Arc<dyn ShellSink>);

impl ShellSinkHandle {
    /// Wraps a sink for use by a [`ShellRequest`].
    #[must_use]
    pub fn new(sink: impl ShellSink) -> Self {
        Self(Arc::new(sink))
    }

    pub(crate) fn deliver(&self, event: ShellEvent) -> ShellEventDelivery {
        self.0.on_event(event)
    }
}

impl std::fmt::Debug for ShellSinkHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("ShellSinkHandle").finish()
    }
}

struct ChannelLifecycle {
    started: Mutex<Option<ShellEvent>>,
    finished: Mutex<Option<ShellEvent>>,
    notify: Notify,
    closed: AtomicBool,
    claimed: AtomicBool,
}

struct ChannelProducer {
    sender: mpsc::Sender<ShellEvent>,
    lifecycle: Arc<ChannelLifecycle>,
}

impl Drop for ChannelProducer {
    fn drop(&mut self) {
        self.lifecycle.closed.store(true, Ordering::Release);
        self.lifecycle.notify.notify_waiters();
    }
}

/// Bounded event sink with reserved lifecycle storage for exactly one run.
///
/// All clones share the same reservation. Reusing one in another accepted
/// request is rejected before allocating a process identity or spawning.
/// Create a new channel for each run.
#[derive(Clone)]
pub struct ChannelSink {
    producer: Arc<ChannelProducer>,
    lifecycle: Arc<ChannelLifecycle>,
}

impl std::fmt::Debug for ChannelSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChannelSink")
            .finish_non_exhaustive()
    }
}

impl ChannelSink {
    /// Creates a sink and its single receiver.
    ///
    /// `capacity` bounds data events and is taken as a [`NonZeroUsize`] because
    /// a zero-capacity queue cannot hold an event: rather than silently raising
    /// a zero to one, the type makes the unusable value unrepresentable. To
    /// observe lifecycle events only, pass the smallest capacity and ignore
    /// [`ShellEvent::Chunk`]; the drops are still reported through
    /// [`ExecutionResult::dropped_events`](crate::ExecutionResult::dropped_events).
    ///
    /// `Started` and `Finished` use two reserved lifecycle slots and therefore
    /// remain observable even when the data queue is full.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_shell::ChannelSink;
    /// use std::num::NonZeroUsize;
    ///
    /// let (sink, events) = ChannelSink::new(NonZeroUsize::new(32).expect("non-zero"));
    /// # let _ = (sink, events);
    /// ```
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> (Self, ShellEventReceiver) {
        let (sender, receiver) = mpsc::channel(capacity.get());
        let lifecycle = Arc::new(ChannelLifecycle {
            started: Mutex::new(None),
            finished: Mutex::new(None),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
        });
        (
            Self {
                producer: Arc::new(ChannelProducer {
                    sender,
                    lifecycle: Arc::clone(&lifecycle),
                }),
                lifecycle: Arc::clone(&lifecycle),
            },
            ShellEventReceiver {
                receiver,
                lifecycle,
                started_delivered: false,
                finished_delivered: false,
                pending_finished: None,
            },
        )
    }
}

impl ShellSink for ChannelSink {
    fn try_claim(&self) -> bool {
        !self.lifecycle.claimed.swap(true, Ordering::AcqRel)
    }

    fn on_event(&self, event: ShellEvent) -> ShellEventDelivery {
        match event {
            ShellEvent::Started { .. } => {
                *lock(&self.lifecycle.started) = Some(event);
                self.lifecycle.notify.notify_waiters();
                ShellEventDelivery::Accepted
            }
            ShellEvent::Finished { .. } => {
                *lock(&self.lifecycle.finished) = Some(event);
                self.lifecycle.notify.notify_waiters();
                ShellEventDelivery::Accepted
            }
            data => match self.producer.sender.try_send(data) {
                Ok(()) => ShellEventDelivery::Accepted,
                Err(_) => ShellEventDelivery::Dropped,
            },
        }
    }
}

/// Receiver returned by [`ChannelSink`] that preserves lifecycle ordering.
pub struct ShellEventReceiver {
    receiver: mpsc::Receiver<ShellEvent>,
    lifecycle: Arc<ChannelLifecycle>,
    started_delivered: bool,
    finished_delivered: bool,
    pending_finished: Option<ShellEvent>,
}

impl std::fmt::Debug for ShellEventReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellEventReceiver")
            .field("started_delivered", &self.started_delivered)
            .field("finished_delivered", &self.finished_delivered)
            .finish_non_exhaustive()
    }
}

impl ShellEventReceiver {
    /// Receives the next event.
    ///
    /// `Started` is returned before queued data, and `Finished` is returned
    /// only after the bounded data queue is drained. Nothing is returned after
    /// `Finished`: the producing side stops handing data events to the sink
    /// once the terminal event is delivered, so the next call yields `None`.
    /// If all sink clones are dropped before completion, buffered events are
    /// drained and then `None` is returned, without synthesizing `Finished`.
    pub async fn recv(&mut self) -> Option<ShellEvent> {
        loop {
            let notified = self.lifecycle.notify.notified();
            // Register before inspecting shared state: notifications from a
            // producer on another runtime thread must not be lost.
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.finished_delivered {
                return None;
            }
            let closed = self.lifecycle.closed.load(Ordering::Acquire);
            if !self.started_delivered {
                if let Some(event) = lock(&self.lifecycle.started).take() {
                    self.started_delivered = true;
                    return Some(event);
                }
                if closed {
                    // An incomplete stream can have data without Started.
                    self.started_delivered = true;
                    continue;
                }
                notified.await;
                continue;
            }
            if let Ok(event) = self.receiver.try_recv() {
                return Some(event);
            }
            if self.pending_finished.is_none() {
                self.pending_finished = lock(&self.lifecycle.finished).take();
            }
            if self.pending_finished.is_some() {
                // Seeing Finished synchronizes with the producer's final
                // writes. A chunk can have arrived after the first empty
                // check, so drain again before exposing the terminal event.
                if let Ok(event) = self.receiver.try_recv() {
                    return Some(event);
                }
                self.finished_delivered = true;
                return self.pending_finished.take();
            }
            if closed {
                return None;
            }
            tokio::select! {
                biased;
                event = self.receiver.recv() => {
                    if let Some(event) = event {
                        return Some(event);
                    }
                    // The final producer may have published lifecycle events
                    // immediately before dropping. Inspect those once more.
                }
                () = &mut notified => {}
            }
        }
    }
}

type CancelFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct CancelInner {
    wait: Arc<dyn Fn() -> CancelFuture + Send + Sync>,
}

/// Observer of an externally owned **cancellation signal**.
///
/// One concept, one name throughout this crate: the host owns the cancellation
/// signal and raises it; a `ShellCancel` is the *cancellation observer* that
/// notices. It has no way to raise anything. ("Signal" on its own means a
/// POSIX signal in this crate — always "cancellation signal" for this one.)
///
/// Every observation is a fresh future built by the adapter, so one value can
/// be observed by the pre-spawn check and by the supervisor at the same time.
///
/// # Example
///
/// ```no_run
/// use polaris_shell::{ShellCancel, ShellConfig, ShellExecutor, ShellRequest};
///
/// # async fn example() -> Result<(), polaris_shell::ShellError> {
/// let executor = ShellExecutor::new(ShellConfig::new().disable_cache());
/// let (stop, observer) = tokio::sync::watch::channel(false);
/// let process = executor.start_confirmed(
///     ShellRequest::new("sleep 30").with_cancel(ShellCancel::from_watch(observer)),
/// ).await?;
/// stop.send_replace(true); // The host owns and raises the cancellation signal.
/// let result = process.wait().await.expect("unconsumed result")?;
/// assert_eq!(result.outcome, polaris_shell::ShellOutcome::Cancelled);
/// # Ok(())
/// # }
/// ```
///
/// # Already-raised cancellation
///
/// The pre-spawn check polls one freshly built observation exactly once, so a
/// cancellation signal that was already raised is detected before spawning
/// only if that observation resolves on its first poll. Both
/// [`from_watch`](Self::from_watch) and the intended
/// [`from_fn`](Self::from_fn) sources — an owned `cancelled` future from the
/// host's cancellation source — satisfy that. A custom factory that needs more
/// than one poll to report an already-raised cancellation is still noticed by
/// the supervisor, but the child is spawned first.
#[derive(Clone)]
pub struct ShellCancel(Arc<CancelInner>);

impl std::fmt::Debug for ShellCancel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("ShellCancel").finish()
    }
}

impl ShellCancel {
    /// Observes a latched Tokio `watch` channel as the cancellation signal.
    ///
    /// An already-latched receiver resolves on the first poll, so a
    /// cancellation raised before the call is caught before any child is
    /// spawned. Dropping every sender also resolves the observer and requests
    /// cancellation, even when the last value was `false`.
    #[must_use]
    pub fn from_watch(receiver: watch::Receiver<bool>) -> Self {
        let wait = Arc::new(move || {
            let mut receiver = receiver.clone();
            Box::pin(async move {
                loop {
                    if *receiver.borrow_and_update() {
                        return;
                    }
                    if receiver.changed().await.is_err() {
                        return;
                    }
                }
            }) as CancelFuture
        });
        Self(Arc::new(CancelInner { wait }))
    }

    /// Observes an owned awaitable created for each observation.
    ///
    /// The factory is called once per observation and must return a future
    /// that resolves when the externally owned cancellation signal is raised —
    /// for example `move || cancel.clone().cancelled_owned()`. See
    /// [Already-raised cancellation](Self#already-raised-cancellation) for
    /// what the pre-spawn check can detect.
    #[must_use]
    pub fn from_fn<F, Fut>(factory: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self(Arc::new(CancelInner {
            wait: Arc::new(move || Box::pin(factory())),
        }))
    }

    /// Returns a cancellation observer that never fires.
    #[must_use]
    pub fn never() -> Self {
        Self::from_fn(std::future::pending::<()>)
    }

    pub(crate) fn cancelled(&self) -> CancelFuture {
        (self.0.wait)()
    }

    pub(crate) async fn cancelled_before_spawn(&self) -> bool {
        tokio::select! {
            biased;
            () = self.cancelled() => true,
            () = async {} => false,
        }
    }
}

/// Platform guarantee provided when stopping a process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTreeGuarantee {
    /// Every descendant remaining in the spawned Unix process group is signalled.
    Group,
    /// Only the direct child is terminated; descendants may survive.
    DirectChildOnly,
}

/// Snapshot of one managed process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ShellProcessStatus {
    /// Executor-local identity.
    pub id: ShellProcessId,
    /// Operating-system process id, absent before spawn.
    pub os_pid: Option<u32>,
    /// Command supplied by the caller.
    pub command: String,
    /// Validated working directory.
    pub working_dir: PathBuf,
    /// Time at which the run identity was accepted.
    pub started_at: SystemTime,
    /// Current lifecycle state.
    pub state: ShellProcessState,
    /// Total stdout bytes observed, including uncaptured bytes.
    pub stdout_bytes: u64,
    /// Total stderr bytes observed, including uncaptured bytes.
    pub stderr_bytes: u64,
    /// Whether the terminal result is reserved for a foreground caller.
    ///
    /// `true` for runs started by
    /// [`ShellExecutor::execute`](crate::ShellExecutor::execute) and
    /// [`execute_confirmed`](crate::ShellExecutor::execute_confirmed): the
    /// run is observable and stoppable, but
    /// [`ShellExecutor::wait_for`](crate::ShellExecutor::wait_for) returns
    /// `None` for it rather than taking the result out from under the
    /// foreground caller.
    pub reserved: bool,
}

pub(crate) struct ProcessEntry {
    id: ShellProcessId,
    os_pid: Option<u32>,
    command: String,
    working_dir: PathBuf,
    started_at: SystemTime,
    reserved: bool,
    state: Mutex<ShellProcessState>,
    result: Mutex<Option<Result<ExecutionResult, ShellError>>>,
    notify: Notify,
    stop: watch::Sender<bool>,
    stdout_bytes: AtomicU64,
    stderr_bytes: AtomicU64,
    dropped_events: AtomicU64,
    sink: Option<ShellSinkHandle>,
    /// `true` once the terminal event has been handed to the sink. Guards the
    /// data-event path so no chunk can follow `Finished`.
    terminal_delivered: Mutex<bool>,
}

impl ProcessEntry {
    fn new(
        id: ShellProcessId,
        os_pid: Option<u32>,
        request: &ShellRequest,
        working_dir: PathBuf,
        reserved: bool,
    ) -> Arc<Self> {
        let (stop, _) = watch::channel(false);
        Arc::new(Self {
            id,
            os_pid,
            command: request.command.clone(),
            working_dir,
            started_at: SystemTime::now(),
            reserved,
            state: Mutex::new(ShellProcessState::Running),
            result: Mutex::new(None),
            notify: Notify::new(),
            stop,
            stdout_bytes: AtomicU64::new(0),
            stderr_bytes: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            sink: request.sink.clone(),
            terminal_delivered: Mutex::new(false),
        })
    }

    fn status(&self) -> ShellProcessStatus {
        ShellProcessStatus {
            id: self.id,
            os_pid: self.os_pid,
            command: self.command.clone(),
            working_dir: self.working_dir.clone(),
            started_at: self.started_at,
            state: *lock(&self.state),
            stdout_bytes: self.stdout_bytes.load(Ordering::Relaxed),
            stderr_bytes: self.stderr_bytes.load(Ordering::Relaxed),
            reserved: self.reserved,
        }
    }

    /// Delivers the first lifecycle event, which always precedes any data.
    fn emit_started(&self, event: ShellEvent) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        if sink.deliver(event) == ShellEventDelivery::Dropped {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Delivers a data event, unless the terminal event has already gone out.
    ///
    /// The event is built lazily so a run with no sink — the default for
    /// [`ShellExecutor::execute`](crate::ShellExecutor::execute) and for
    /// `run_command` — allocates nothing per read. The gate is held across
    /// delivery, which is sound because [`ShellSink::on_event`] is
    /// non-blocking by contract.
    fn emit_data_with(&self, event: impl FnOnce() -> ShellEvent) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let terminal_delivered = lock(&self.terminal_delivered);
        let delivery = if *terminal_delivered {
            ShellEventDelivery::Dropped
        } else {
            sink.deliver(event())
        };
        drop(terminal_delivered);
        if delivery == ShellEventDelivery::Dropped {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Delivers the terminal event and closes the data path behind it.
    fn emit_finished(&self, event: ShellEvent) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let mut terminal_delivered = lock(&self.terminal_delivered);
        *terminal_delivered = true;
        let delivery = sink.deliver(event);
        drop(terminal_delivered);
        if delivery == ShellEventDelivery::Dropped {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn request_stop(&self) -> bool {
        let mut state = lock(&self.state);
        if *state != ShellProcessState::Running {
            return false;
        }
        *state = ShellProcessState::Terminating;
        drop(state);
        self.stop.send_replace(true);
        true
    }

    fn mark_terminating(&self) {
        let mut state = lock(&self.state);
        if *state == ShellProcessState::Running {
            *state = ShellProcessState::Terminating;
        }
    }

    fn finish(&self, outcome: ShellOutcome, result: Result<ExecutionResult, ShellError>) {
        *lock(&self.result) = Some(result);
        *lock(&self.state) = ShellProcessState::Finished(outcome);
        self.notify.notify_waiters();
    }
}

pub(crate) struct ProcessTable {
    next_id: AtomicU64,
    entries: Mutex<HashMap<ShellProcessId, Arc<ProcessEntry>>>,
    completed: Mutex<VecDeque<ShellProcessId>>,
}

impl ProcessTable {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            entries: Mutex::new(HashMap::new()),
            completed: Mutex::new(VecDeque::new()),
        }
    }

    fn allocate(&self) -> ShellProcessId {
        ShellProcessId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn insert(&self, entry: Arc<ProcessEntry>) {
        lock(&self.entries).insert(entry.id, entry);
    }

    fn entry(&self, id: ShellProcessId) -> Option<Arc<ProcessEntry>> {
        lock(&self.entries).get(&id).cloned()
    }

    /// Counts discoverable entries without cloning any of them.
    pub(crate) fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    fn statuses(&self) -> Vec<ShellProcessStatus> {
        let mut statuses: Vec<_> = lock(&self.entries)
            .values()
            .map(|entry| entry.status())
            .collect();
        statuses.sort_by_key(|status| status.id);
        statuses
    }

    fn completed(&self, id: ShellProcessId, retained: usize) {
        let mut completed = lock(&self.completed);
        completed.push_back(id);
        let mut entries = lock(&self.entries);
        while completed.len() > retained {
            if let Some(evicted) = completed.pop_front() {
                entries.remove(&evicted);
            }
        }
    }

    fn consumed(&self, id: ShellProcessId) {
        lock(&self.entries).remove(&id);
        lock(&self.completed).retain(|candidate| *candidate != id);
    }
}

/// Handle to one accepted managed shell run.
///
/// # Dropping requests a stop
///
/// This handle is the unique owner of the run's foreground lifetime: dropping
/// it requests termination of the child, exactly as
/// [`stop`](Self::stop) would. That keeps the common case honest — if the
/// future awaiting a command is dropped (a graph node's `max_duration`
/// elapsing, a cancelled HTTP request, an early `return`), the `cargo build`
/// it started does not outlive the caller that wanted it.
///
/// [`detach`](Self::detach) is the explicit opt-out: it hands the run to the
/// executor and returns its identity, and the child keeps running.
/// [`ShellPlugin::cleanup`](crate::ShellPlugin) is the backstop that stops
/// detached runs at server shutdown.
///
/// Dropping the handle after the run reached a terminal state — including
/// after [`wait`](Self::wait) resolves — does nothing.
///
/// # Example
///
/// ```no_run
/// use polaris_shell::{ShellConfig, ShellExecutor, ShellRequest};
///
/// # async fn example() -> Result<(), polaris_shell::ShellError> {
/// let executor = ShellExecutor::new(ShellConfig::new().disable_cache());
/// let process = executor.start_confirmed(ShellRequest::new("printf hello")).await?;
/// let result = process.wait().await.expect("unconsumed result")?;
/// assert_eq!(result.stdout, "hello");
///
/// let process = executor.start_confirmed(ShellRequest::new("sleep 30")).await?;
/// let id = process.detach(); // Explicitly transfer lifetime to the executor.
/// executor.stop(id).await;
/// let result = executor.wait_for(id).await.expect("unconsumed result")?;
/// assert_eq!(result.outcome, polaris_shell::ShellOutcome::Cancelled);
/// # Ok(())
/// # }
/// ```
pub struct ShellProcess {
    executor: ShellExecutor,
    entry: Arc<ProcessEntry>,
    /// Cleared by [`ShellProcess::detach`] so the run outlives this handle.
    stop_on_drop: bool,
}

impl std::fmt::Debug for ShellProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellProcess")
            .field("id", &self.entry.id)
            .field("os_pid", &self.entry.os_pid)
            .finish_non_exhaustive()
    }
}

impl ShellProcess {
    /// Returns the executor-local run identity.
    #[must_use]
    pub fn id(&self) -> ShellProcessId {
        self.entry.id
    }

    /// Returns the operating-system process id, if a child was spawned.
    #[must_use]
    pub fn os_pid(&self) -> Option<u32> {
        self.entry.os_pid
    }

    /// Waits for and consumes the terminal result exactly once.
    ///
    /// Dropping the returned future drops this handle, which requests a stop —
    /// see [Dropping requests a stop](Self#dropping-requests-a-stop).
    ///
    /// # Errors
    ///
    /// The outer `Option` reports *ownership*, the inner `Result` reports the
    /// run:
    ///
    /// - `None` — the terminal result was already consumed, by
    ///   [`ShellExecutor::wait_for`](crate::ShellExecutor::wait_for) on an
    ///   unreserved run. It cannot be returned twice; the run's status is still
    ///   readable through [`ShellExecutor::process`](crate::ShellExecutor::process)
    ///   until the entry is evicted.
    /// - `Some(Err(_))` — the supervisor recorded a terminal management error.
    ///   The current implementation reports pipe/reaping incompleteness through
    ///   [`ExecutionResult::cleanup_completed`](crate::ExecutionResult::cleanup_completed)
    ///   instead, so no post-start error is currently produced; the inner
    ///   `Result` preserves that distinction for future supervisors.
    /// - `Some(Ok(_))` — the run finished. A non-zero exit, a timeout, and a
    ///   cancellation are all `Ok`; read
    ///   [`ExecutionResult::outcome`](crate::ExecutionResult::outcome) to tell
    ///   them apart.
    pub async fn wait(self) -> Option<Result<ExecutionResult, ShellError>> {
        let result = wait_entry(&self.entry).await;
        if result.is_some() {
            self.executor.0.processes.consumed(self.entry.id);
        }
        result
    }

    /// Leaves the process under executor ownership and returns its identity.
    ///
    /// This is the explicit opt-out from
    /// [drop-requests-a-stop](Self#dropping-requests-a-stop): the child keeps
    /// running after the returned identity is the only thing left. Reach the
    /// run again through [`ShellExecutor::process`](crate::ShellExecutor::process),
    /// [`stop`](crate::ShellExecutor::stop), and
    /// [`wait_for`](crate::ShellExecutor::wait_for).
    #[must_use]
    pub fn detach(mut self) -> ShellProcessId {
        self.stop_on_drop = false;
        self.entry.id
    }

    /// Requests termination without awaiting.
    ///
    /// Returns `true` only for the transition that wins: the first caller to
    /// move the run out of [`ShellProcessState::Running`] gets `true`, and a
    /// request against a run that is already terminating or finished gets
    /// `false`. `true` therefore means "this call raised the stop", not "the
    /// child is gone" — the supervisor still runs the configured graceful and
    /// forced termination sequence, and
    /// [`wait`](Self::wait) is what observes the outcome.
    ///
    /// This is the synchronous path used by [`Drop`], and is callable from any
    /// context that cannot await.
    pub fn request_stop(&self) -> bool {
        self.entry.request_stop()
    }

    /// Requests termination. Only the first transition returns `true`.
    ///
    /// Async wrapper around [`request_stop`](Self::request_stop) with the same
    /// return contract; the body does not await.
    pub async fn stop(&self) -> bool {
        self.request_stop()
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        if self.stop_on_drop {
            self.entry.request_stop();
        }
    }
}

async fn wait_entry(entry: &Arc<ProcessEntry>) -> Option<Result<ExecutionResult, ShellError>> {
    loop {
        let notified = entry.notify.notified();
        if let Some(result) = lock(&entry.result).take() {
            return Some(result);
        }
        if matches!(*lock(&entry.state), ShellProcessState::Finished(_)) {
            return None;
        }
        notified.await;
    }
}

#[derive(Default)]
struct CaptureState {
    retained: Vec<u8>,
    truncated: bool,
    overflow_path: Option<PathBuf>,
    overflow_truncated: bool,
    io_complete: bool,
}

struct CaptureSnapshot {
    text: String,
    truncated: bool,
    overflow_path: Option<PathBuf>,
    overflow_truncated: bool,
    io_complete: bool,
}

impl CaptureState {
    fn snapshot(&self, max: usize) -> CaptureSnapshot {
        let text = String::from_utf8_lossy(&self.retained).into_owned();
        CaptureSnapshot {
            text: if self.truncated {
                format!("{text}\n...[truncated, {max} byte limit]")
            } else {
                text
            },
            truncated: self.truncated,
            overflow_path: self.overflow_path.clone(),
            overflow_truncated: self.overflow_truncated,
            io_complete: self.io_complete,
        }
    }
}

/// Per-stream byte ceilings applied by one output reader.
#[derive(Debug, Clone, Copy)]
struct CaptureLimits {
    /// Bytes retained in memory before the capture is marked truncated.
    max_output_bytes: usize,
    /// Bytes written to the overflow file before spilling stops.
    max_overflow_bytes: u64,
}

impl CaptureLimits {
    fn from_config(config: &crate::ShellConfig) -> Self {
        Self {
            max_output_bytes: config.max_output_bytes,
            max_overflow_bytes: config.max_overflow_bytes,
        }
    }
}

/// An overflow file plus the bytes already spilled into it.
struct Overflow {
    file: tokio::fs::File,
    written: u64,
}

/// Environment variables copied from the host when the child does not inherit.
///
/// These name the session rather than its credentials, and a shell that has
/// none of them behaves surprisingly (no `HOME`, no locale). `PATH` is handled
/// separately because it always has a value, from
/// [`ShellConfig::path`](crate::ShellConfig::path) or the built-in default.
const TRUSTED_ENV_NAMES: [&str; 7] = ["HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR"];

/// Applies the configured environment policy to a command about to be spawned.
///
/// With [`ShellConfig::inherit_env`](crate::ShellConfig::inherit_env) the host
/// environment is passed through unchanged and the explicit map is layered on
/// top. Otherwise the environment is cleared and rebuilt in precedence order:
/// trusted base, then allowlist, then the explicit map.
fn apply_env(command: &mut tokio::process::Command, config: &crate::ShellConfig) {
    if !config.inherit_env {
        command.env_clear();
        command.env("PATH", config.resolved_path());
        for name in TRUSTED_ENV_NAMES {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for name in &config.env_allowlist {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    for (name, value) in &config.env {
        command.env(name, value);
    }
}

pub(crate) async fn start_managed(
    executor: ShellExecutor,
    request: ShellRequest,
    working_dir: PathBuf,
    timeout: Duration,
    reserved: bool,
) -> Result<ShellProcess, ShellError> {
    let working_dir = executor.pin_working_dir(&working_dir)?;
    if let Some(sink) = &request.sink
        && !sink.0.try_claim()
    {
        return Err(ShellError::SinkAlreadyUsed);
    }
    let id = executor.0.processes.allocate();
    let cancel = request.cancel.clone().unwrap_or_else(ShellCancel::never);

    if cancel.cancelled_before_spawn().await {
        let entry = ProcessEntry::new(id, None, &request, working_dir.path, reserved);
        executor.0.processes.insert(Arc::clone(&entry));
        entry.emit_started(ShellEvent::Started {
            process: id,
            os_pid: None,
            command: request.command.clone(),
        });
        entry.emit_finished(ShellEvent::Finished {
            process: id,
            outcome: ShellOutcome::Cancelled,
            exit_code: None,
        });
        let result = ExecutionResult::cancelled_before_spawn(
            id,
            entry.dropped_events.load(Ordering::Relaxed),
        );
        entry.finish(ShellOutcome::Cancelled, Ok(result));
        executor
            .0
            .processes
            .completed(id, executor.0.config.retained_completed_results);
        return Ok(ShellProcess {
            executor,
            entry,
            stop_on_drop: true,
        });
    }

    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&request.command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let directory = working_dir.directory;
        // SAFETY: the closure owns the validated directory descriptor until
        // spawn completes. fchdir is async-signal-safe; errno conversion does
        // not allocate or acquire locks in the child after fork.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(directory.as_raw_fd()) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    command.current_dir(&working_dir.path);
    apply_env(&mut command, &executor.0.config);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|spawn_err| ShellError::SpawnFailed(spawn_err.to_string()))?;
    let os_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ShellError::IoError("spawned child did not expose stdout".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ShellError::IoError("spawned child did not expose stderr".to_owned()))?;
    let entry = ProcessEntry::new(id, os_pid, &request, working_dir.path, reserved);
    executor.0.processes.insert(Arc::clone(&entry));
    entry.emit_started(ShellEvent::Started {
        process: id,
        os_pid,
        command: request.command.clone(),
    });

    let supervisor_executor = executor.clone();
    let supervisor_entry = Arc::clone(&entry);
    tokio::spawn(async move {
        supervise(
            supervisor_executor,
            supervisor_entry,
            child,
            stdout,
            stderr,
            cancel,
            timeout,
        )
        .await;
    });

    Ok(ShellProcess {
        executor,
        entry,
        stop_on_drop: true,
    })
}

async fn supervise(
    executor: ShellExecutor,
    entry: Arc<ProcessEntry>,
    mut child: Child,
    stdout: impl AsyncRead + Unpin + Send + 'static,
    stderr: impl AsyncRead + Unpin + Send + 'static,
    cancel: ShellCancel,
    timeout: Duration,
) {
    let stdout_state = Arc::new(Mutex::new(CaptureState::default()));
    let stderr_state = Arc::new(Mutex::new(CaptureState::default()));
    let mut stdout_task = tokio::spawn(read_stream(
        stdout,
        ShellStream::Stdout,
        Arc::clone(&entry),
        Arc::clone(&stdout_state),
        CaptureLimits::from_config(&executor.0.config),
        executor.0.config.cache_dir.clone(),
    ));
    let mut stderr_task = tokio::spawn(read_stream(
        stderr,
        ShellStream::Stderr,
        Arc::clone(&entry),
        Arc::clone(&stderr_state),
        CaptureLimits::from_config(&executor.0.config),
        executor.0.config.cache_dir.clone(),
    ));
    let mut manual_stop = entry.stop.subscribe();
    let started = tokio::time::Instant::now();

    enum Cause {
        Exit(std::io::Result<()>),
        Timeout,
        Cancelled,
    }

    let cause = tokio::select! {
        status = wait_for_exit(&mut child) => Cause::Exit(status),
        () = tokio::time::sleep(timeout) => Cause::Timeout,
        () = cancel.cancelled() => Cause::Cancelled,
        () = stop_requested(&mut manual_stop) => Cause::Cancelled,
    };

    let (outcome, status, child_cleaned) = match cause {
        Cause::Exit(Ok(())) => {
            // Keep the exited leader unreaped until the final group signal.
            // Detached work within this group must not outlive a managed run.
            #[cfg(unix)]
            let group_cleaned = entry
                .os_pid
                .is_some_and(|pid| signal_group(pid, libc::SIGKILL));
            #[cfg(not(unix))]
            let group_cleaned = true;
            let (status, reaped) = reap(&mut child, executor.0.config.drain_deadline).await;
            (ShellOutcome::Exited, status, group_cleaned && reaped)
        }
        Cause::Exit(Err(error)) => {
            tracing::warn!(%error, "failed to observe shell leader exit");
            // Without an unreaped leader we cannot safely address its group.
            let _ = child.start_kill();
            let (status, _) = reap(&mut child, executor.0.config.drain_deadline).await;
            (ShellOutcome::Exited, status, false)
        }
        Cause::Timeout => {
            entry.mark_terminating();
            let (status, cleaned) = terminate_child(
                &mut child,
                entry.os_pid,
                executor.0.config.terminate_grace,
                executor.0.config.drain_deadline,
            )
            .await;
            (ShellOutcome::TimedOut, status, cleaned)
        }
        Cause::Cancelled => {
            entry.mark_terminating();
            let (status, cleaned) = terminate_child(
                &mut child,
                entry.os_pid,
                executor.0.config.terminate_grace,
                executor.0.config.drain_deadline,
            )
            .await;
            (ShellOutcome::Cancelled, status, cleaned)
        }
    };

    let readers_complete = tokio::time::timeout(executor.0.config.drain_deadline, async {
        let (stdout_join, stderr_join) = tokio::join!(&mut stdout_task, &mut stderr_task);
        stdout_join.is_ok() && stderr_join.is_ok()
    })
    .await
    .unwrap_or(false);
    if !readers_complete {
        stdout_task.abort();
        stderr_task.abort();
    }

    let stdout = lock(&stdout_state).snapshot(executor.0.config.max_output_bytes);
    let mut stderr = lock(&stderr_state).snapshot(executor.0.config.max_output_bytes);
    if outcome == ShellOutcome::TimedOut {
        if !stderr.text.is_empty() {
            stderr.text.push('\n');
        }
        stderr
            .text
            .push_str(&format!("Command timed out after {}s", timeout.as_secs()));
    }
    let cleanup_completed =
        child_cleaned && readers_complete && stdout.io_complete && stderr.io_complete;
    let exit_code = status.and_then(|status| status.code());
    entry.emit_finished(ShellEvent::Finished {
        process: entry.id,
        outcome,
        exit_code,
    });
    let result = ExecutionResult {
        exit_code,
        stdout: stdout.text,
        stderr: stderr.text,
        timed_out: outcome == ShellOutcome::TimedOut,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout_overflow_path: stdout.overflow_path,
        stderr_overflow_path: stderr.overflow_path,
        stdout_overflow_truncated: stdout.overflow_truncated,
        stderr_overflow_truncated: stderr.overflow_truncated,
        outcome,
        process: entry.id,
        duration: started.elapsed(),
        dropped_events: entry.dropped_events.load(Ordering::Relaxed),
        cleanup_completed,
    };
    entry.finish(outcome, Ok(result));
    executor
        .0
        .processes
        .completed(entry.id, executor.0.config.retained_completed_results);
}

/// Waits until the process stop latch is raised.
///
/// The synchronous check is essential: a caller can request a stop after
/// [`start_managed`] returns but before the spawned supervisor first runs.
/// Calling only [`watch::Receiver::changed`] would wait for a *later* change
/// and lose that already-raised request.
async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn read_stream(
    mut reader: impl AsyncRead + Unpin,
    stream: ShellStream,
    entry: Arc<ProcessEntry>,
    state: Arc<Mutex<CaptureState>>,
    limits: CaptureLimits,
    cache_dir: Option<PathBuf>,
) {
    let max = limits.max_output_bytes;
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES];
    let mut overflow: Option<Overflow> = None;
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => {
                lock(&state).io_complete = true;
                break;
            }
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(?stream, %error, "failed to read shell output");
                break;
            }
        };
        let bytes = &buffer[..read];
        match stream {
            ShellStream::Stdout => {
                entry.stdout_bytes.fetch_add(read as u64, Ordering::Relaxed);
            }
            ShellStream::Stderr => {
                entry.stderr_bytes.fetch_add(read as u64, Ordering::Relaxed);
            }
        }
        entry.emit_data_with(|| ShellEvent::Chunk {
            process: entry.id,
            stream,
            bytes: bytes.to_vec(),
        });

        let (crossed, prefix, remainder) = {
            let mut capture = lock(&state);
            if capture.retained.len() < max {
                let keep = (max - capture.retained.len()).min(bytes.len());
                capture.retained.extend_from_slice(&bytes[..keep]);
                if keep < bytes.len() {
                    capture.truncated = true;
                    (true, capture.retained.clone(), bytes[keep..].to_vec())
                } else {
                    (false, Vec::new(), Vec::new())
                }
            } else if !capture.truncated {
                capture.truncated = true;
                (true, capture.retained.clone(), bytes.to_vec())
            } else {
                (false, Vec::new(), bytes.to_vec())
            }
        };

        if crossed {
            entry.emit_data_with(|| ShellEvent::CaptureLimitReached {
                process: entry.id,
                stream,
            });
            if let Some(path) = overflow_path(cache_dir.as_ref(), stream).await {
                match tokio::fs::File::create(&path).await {
                    Ok(file) => {
                        let mut opened = Overflow { file, written: 0 };
                        if spill(&mut opened, &prefix, limits, stream, &entry, &state).await
                            && spill(&mut opened, &remainder, limits, stream, &entry, &state).await
                        {
                            lock(&state).overflow_path = Some(path);
                            overflow = Some(opened);
                        } else {
                            tracing::warn!(?stream, "failed to initialize shell overflow file");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(?stream, %error, "failed to create shell overflow file");
                    }
                }
            }
        } else if !remainder.is_empty()
            && let Some(opened) = overflow.as_mut()
            && !spill(opened, &remainder, limits, stream, &entry, &state).await
        {
            overflow = None;
            lock(&state).overflow_path = None;
        }
    }

    if let Some(mut opened) = overflow
        && let Err(error) = opened.file.flush().await
    {
        tracing::warn!(?stream, %error, "failed to flush shell overflow file");
        lock(&state).overflow_path = None;
    }
}

/// Appends `bytes` to the overflow file, honouring the per-stream spill quota.
///
/// Returns `false` only when the write failed and the file should be
/// abandoned. Reaching the quota is not a failure: the surplus is discarded,
/// the capture is marked [`CaptureState::overflow_truncated`], and a single
/// [`ShellEvent::OverflowLimitReached`] is emitted, but the bytes already on
/// disk stay valid and readable.
async fn spill(
    overflow: &mut Overflow,
    bytes: &[u8],
    limits: CaptureLimits,
    stream: ShellStream,
    entry: &Arc<ProcessEntry>,
    state: &Arc<Mutex<CaptureState>>,
) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let remaining = limits.max_overflow_bytes.saturating_sub(overflow.written);
    let writable = usize::try_from(remaining)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    if writable > 0 {
        if let Err(error) = overflow.file.write_all(&bytes[..writable]).await {
            tracing::warn!(?stream, %error, "failed to append shell overflow file");
            return false;
        }
        overflow.written += writable as u64;
    }
    if writable < bytes.len() {
        let first_time = {
            let mut capture = lock(state);
            let first_time = !capture.overflow_truncated;
            capture.overflow_truncated = true;
            first_time
        };
        if first_time {
            tracing::warn!(
                ?stream,
                quota = limits.max_overflow_bytes,
                "shell overflow spill quota reached; later output is not persisted"
            );
            entry.emit_data_with(|| ShellEvent::OverflowLimitReached {
                process: entry.id,
                stream,
            });
        }
    }
    true
}

async fn overflow_path(cache_dir: Option<&PathBuf>, stream: ShellStream) -> Option<PathBuf> {
    let dir = cache_dir?;
    if let Err(error) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(dir = %dir.display(), %error, "failed to create shell overflow directory");
        return None;
    }
    let label = match stream {
        ShellStream::Stdout => "stdout",
        ShellStream::Stderr => "stderr",
    };
    let name = names::Generator::default()
        .next()
        .unwrap_or_else(|| "output".to_owned());
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Some(dir.join(format!("shell_{label}_{name}_{nanos}.txt")))
}

/// Observes exit without releasing the Unix leader's process identity.
async fn wait_for_exit(child: &mut Child) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("missing child pid"))?;
        // Install the listener before checking the child, so exit between the
        // non-blocking check and recv cannot lose the SIGCHLD wakeup.
        let mut exited = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        loop {
            let observed = {
                // SAFETY: siginfo_t is a C record whose all-zero value is valid.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                // SAFETY: info is writable and correctly aligned. WNOWAIT observes
                // only this owned child's exit and leaves it available for reaping.
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &raw mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                // SAFETY: successful waitid initialized the SIGCHLD record; a zero
                // si_pid means no child has exited under WNOHANG.
                (unsafe { info.si_pid() }) != 0
            };
            if observed {
                return Ok(());
            }
            if exited.recv().await.is_none() {
                return Err(std::io::Error::other("SIGCHLD listener closed"));
            }
        }
    }
    #[cfg(not(unix))]
    child.wait().await.map(|_| ())
}

/// Stops a child and its process group, then reaps the leader.
///
/// On Unix the leader is deliberately **not** reaped until after the final
/// group `SIGKILL`. The process-group id is the leader's pid, and the kernel
/// cannot recycle a pid while the process is an unreaped zombie — so leaving
/// the leader unreaped across the grace period keeps `kill(-pgid, SIGKILL)`
/// pointed at the group we spawned instead of at whatever group inherited the
/// number in the meantime.
///
/// Returns the leader's exit status when it was reaped, and whether reaping
/// completed within `reap_deadline`.
async fn terminate_child(
    child: &mut Child,
    os_pid: Option<u32>,
    grace: Duration,
    reap_deadline: Duration,
) -> (Option<ExitStatus>, bool) {
    #[cfg(unix)]
    {
        let Some(pid) = os_pid else {
            // No pid to address the group with — fall back to the direct child.
            let _ = child.start_kill();
            return reap(child, grace + reap_deadline).await;
        };
        signal_group(pid, libc::SIGTERM);
        // Deliberately no `child.wait()` here: reaping the leader now would free
        // its pid, and therefore the group id, before the SIGKILL below.
        tokio::time::sleep(grace).await;
        let signalled = signal_group(pid, libc::SIGKILL);
        // Both a leader that already exited during the grace period and one
        // that just took the SIGKILL are reaped by this single wait.
        let (status, reaped) = reap(child, reap_deadline).await;
        (status, signalled && reaped)
    }

    #[cfg(not(unix))]
    {
        let _ = os_pid;
        let _ = child.start_kill();
        reap(child, grace + reap_deadline).await
    }
}

/// Reaps a stopped child within `deadline`, reporting whether it succeeded.
async fn reap(child: &mut Child, deadline: Duration) -> (Option<ExitStatus>, bool) {
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => (Some(status), true),
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to reap killed shell child");
            (None, false)
        }
        Err(_) => (None, false),
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` receives a valid negative process-group id derived from a
    // spawned child pid and a valid libc signal constant. No pointer is used.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        true
    } else {
        tracing::warn!(pid, signal, %error, "failed to signal shell process group");
        false
    }
}

pub(crate) fn processes(inner: &ExecutorInner) -> Vec<ShellProcessStatus> {
    inner.processes.statuses()
}

pub(crate) fn process(inner: &ExecutorInner, id: ShellProcessId) -> Option<ShellProcessStatus> {
    inner.processes.entry(id).map(|entry| entry.status())
}

pub(crate) async fn wait_for(
    executor: &ShellExecutor,
    id: ShellProcessId,
) -> Option<Result<ExecutionResult, ShellError>> {
    let entry = executor.0.processes.entry(id)?;
    if entry.reserved {
        // A foreground run's result belongs to the caller inside `execute` /
        // `execute_confirmed`. Taking it here would leave that caller with
        // nothing to return, so the run is reported as not waitable from here.
        return None;
    }
    let result = wait_entry(&entry).await;
    if result.is_some() {
        executor.0.processes.consumed(id);
    }
    result
}

pub(crate) async fn stop(inner: &ExecutorInner, id: ShellProcessId) -> bool {
    inner
        .processes
        .entry(id)
        .is_some_and(|entry| entry.request_stop())
}

pub(crate) async fn stop_all(inner: &ExecutorInner) -> usize {
    let entries: Vec<_> = lock(&inner.processes.entries).values().cloned().collect();
    let mut stopped = 0;
    for entry in &entries {
        if entry.request_stop() {
            stopped += 1;
        }
    }
    for entry in entries {
        while !matches!(*lock(&entry.state), ShellProcessState::Finished(_)) {
            let notified = entry.notify.notified();
            if matches!(*lock(&entry.state), ShellProcessState::Finished(_)) {
                break;
            }
            notified.await;
        }
    }
    stopped
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn capacity(slots: usize) -> NonZeroUsize {
        NonZeroUsize::new(slots).expect("test capacity must be non-zero")
    }

    fn id(raw: u64) -> ShellProcessId {
        ShellProcessId(raw)
    }

    fn chunk(process: ShellProcessId, byte: u8) -> ShellEvent {
        ShellEvent::Chunk {
            process,
            stream: ShellStream::Stdout,
            bytes: vec![byte],
        }
    }

    fn started(process: ShellProcessId) -> ShellEvent {
        ShellEvent::Started {
            process,
            os_pid: None,
            command: "true".to_owned(),
        }
    }

    fn finished(process: ShellProcessId) -> ShellEvent {
        ShellEvent::Finished {
            process,
            outcome: ShellOutcome::Exited,
            exit_code: Some(0),
        }
    }

    /// Builds an entry without spawning anything, for the pure bookkeeping.
    fn entry(process: ShellProcessId, sink: Option<ShellSinkHandle>) -> Arc<ProcessEntry> {
        let mut request = ShellRequest::new("true");
        request.sink = sink;
        ProcessEntry::new(process, Some(4242), &request, PathBuf::from("/tmp"), false)
    }

    // -- ShellEventReceiver ordering --

    #[tokio::test]
    async fn recv_returns_started_then_data_then_finished() {
        let process = id(1);
        let (sink, mut events) = ChannelSink::new(capacity(4));
        sink.on_event(chunk(process, b'a'));
        sink.on_event(started(process));
        sink.on_event(chunk(process, b'b'));
        sink.on_event(finished(process));

        assert_eq!(events.recv().await, Some(started(process)));
        assert_eq!(events.recv().await, Some(chunk(process, b'a')));
        assert_eq!(events.recv().await, Some(chunk(process, b'b')));
        assert_eq!(events.recv().await, Some(finished(process)));
        assert_eq!(events.recv().await, None);
    }

    #[tokio::test]
    async fn a_full_data_queue_still_delivers_both_lifecycle_events() {
        let process = id(2);
        let (sink, mut events) = ChannelSink::new(capacity(1));
        sink.on_event(started(process));
        assert_eq!(
            sink.on_event(chunk(process, b'a')),
            ShellEventDelivery::Accepted
        );
        assert_eq!(
            sink.on_event(chunk(process, b'b')),
            ShellEventDelivery::Dropped,
            "the second chunk has nowhere to go"
        );
        assert_eq!(
            sink.on_event(finished(process)),
            ShellEventDelivery::Accepted,
            "the terminal event uses a reserved slot"
        );

        assert_eq!(events.recv().await, Some(started(process)));
        assert_eq!(events.recv().await, Some(chunk(process, b'a')));
        assert_eq!(events.recv().await, Some(finished(process)));
        assert_eq!(events.recv().await, None);
    }

    #[tokio::test]
    async fn recv_ends_when_all_sinks_drop_before_started() {
        let (sink, mut events) = ChannelSink::new(capacity(4));
        drop(sink);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn recv_drains_then_ends_when_producer_drops_without_finished() {
        let process = id(1);
        let (sink, mut events) = ChannelSink::new(capacity(4));
        sink.on_event(started(process));
        sink.on_event(chunk(process, b'a'));
        drop(sink);
        assert_eq!(events.recv().await, Some(started(process)));
        assert_eq!(events.recv().await, Some(chunk(process, b'a')));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_terminal_delivery_never_overtakes_buffered_chunks() {
        tokio::time::timeout(Duration::from_secs(30), async {
            for _ in 0..20_000 {
                let process = id(1);
                let (sink, mut events) = ChannelSink::new(capacity(1));
                sink.on_event(started(process));
                assert_eq!(events.recv().await, Some(started(process)));
                let barrier = Arc::new(std::sync::Barrier::new(2));
                let producer_barrier = Arc::clone(&barrier);
                let producer = tokio::spawn(async move {
                    // Hold lifecycle publication while the receiver checks
                    // the empty queue, then enqueue before publishing Finished.
                    let mut terminal = lock(&sink.lifecycle.finished);
                    producer_barrier.wait();
                    std::thread::yield_now();
                    sink.on_event(chunk(process, b'a'));
                    *terminal = Some(finished(process));
                    drop(terminal);
                    sink.lifecycle.notify.notify_waiters();
                });
                barrier.wait();
                assert_eq!(events.recv().await, Some(chunk(process, b'a')));
                assert_eq!(events.recv().await, Some(finished(process)));
                assert_eq!(events.recv().await, None);
                producer.await.unwrap();
            }
        })
        .await
        .expect("concurrent delivery must complete");
    }

    #[tokio::test]
    async fn recv_wakes_when_the_last_producer_closes_in_either_phase() {
        for send_started in [false, true] {
            let (sink, mut events) = ChannelSink::new(capacity(4));
            if send_started {
                sink.on_event(started(id(1)));
                assert_eq!(events.recv().await, Some(started(id(1))));
            }
            let (event, ()) = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(events.recv(), async move {
                    tokio::task::yield_now().await;
                    drop(sink);
                })
            })
            .await
            .expect("producer closure must wake a waiting receiver");
            assert_eq!(event, None);
        }
    }

    #[tokio::test]
    async fn incomplete_stream_without_started_still_drains_buffered_data() {
        let (sink, mut events) = ChannelSink::new(capacity(1));
        sink.on_event(chunk(id(1), b'a'));
        drop(sink);
        tokio::time::timeout(Duration::from_secs(1), async {
            assert_eq!(events.recv().await, Some(chunk(id(1), b'a')));
            assert_eq!(events.recv().await, None);
        })
        .await
        .expect("an incomplete stream must drain and close");
    }

    #[tokio::test]
    async fn sequential_sink_reuse_is_rejected_before_spawn() {
        let executor =
            ShellExecutor::new(crate::ShellConfig::new().disable_sandbox().disable_cache());
        let (sink, mut events) = ChannelSink::new(capacity(4));
        let first = executor
            .start_confirmed(
                ShellRequest::new("true").with_sink(ShellSinkHandle::new(sink.clone())),
            )
            .await
            .unwrap();
        first.wait().await.unwrap().unwrap();
        let second = executor
            .start_confirmed(ShellRequest::new("true").with_sink(ShellSinkHandle::new(sink)))
            .await;
        assert!(
            matches!(second, Err(ShellError::SinkAlreadyUsed)),
            "one receiver must never silently merge runs"
        );
        assert!(executor.processes().is_empty());
        assert!(matches!(
            events.recv().await,
            Some(ShellEvent::Started { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ShellEvent::Finished { .. })
        ));
        assert_eq!(events.recv().await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_sink_reuse_accepts_only_one_run() {
        let executor =
            ShellExecutor::new(crate::ShellConfig::new().disable_sandbox().disable_cache());
        let (sink, _) = ChannelSink::new(capacity(4));
        let request_a = ShellRequest::new("true").with_sink(ShellSinkHandle::new(sink.clone()));
        let request_b = ShellRequest::new("true").with_sink(ShellSinkHandle::new(sink));
        let (first, second) = tokio::join!(
            executor.start_confirmed(request_a),
            executor.start_confirmed(request_b)
        );
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "exactly one run may claim the sink"
        );
        let accepted = first.or(second).unwrap();
        assert_eq!(
            accepted.id(),
            id(1),
            "rejection must not allocate an identity"
        );
        accepted.wait().await.unwrap().unwrap();
    }

    // -- ProcessEntry emit gate --

    #[tokio::test]
    async fn no_data_event_is_delivered_after_the_terminal_event() {
        let process = id(3);
        let (sink, mut events) = ChannelSink::new(capacity(8));
        let entry = entry(process, Some(ShellSinkHandle::new(sink)));

        entry.emit_started(started(process));
        entry.emit_data_with(|| chunk(process, b'a'));
        entry.emit_finished(finished(process));
        // A reader aborted mid-read can still reach this point.
        entry.emit_data_with(|| chunk(process, b'z'));

        assert_eq!(events.recv().await, Some(started(process)));
        assert_eq!(events.recv().await, Some(chunk(process, b'a')));
        assert_eq!(events.recv().await, Some(finished(process)));
        assert_eq!(
            events.recv().await,
            None,
            "the late chunk must be suppressed"
        );
        assert_eq!(
            entry.dropped_events.load(Ordering::Relaxed),
            1,
            "a suppressed event is reported, not hidden"
        );
    }

    #[test]
    fn a_run_without_a_sink_never_builds_an_event() {
        let process = id(4);
        let entry = entry(process, None);
        let mut built = 0_u32;
        entry.emit_data_with(|| {
            built += 1;
            chunk(process, b'a')
        });
        assert_eq!(built, 0, "the no-sink path must not allocate a chunk");
        assert_eq!(entry.dropped_events.load(Ordering::Relaxed), 0);
    }

    // -- ProcessEntry state transitions --

    #[test]
    fn only_the_first_stop_request_wins() {
        let entry = entry(id(5), None);
        assert!(entry.request_stop());
        assert!(!entry.request_stop());
        assert_eq!(*lock(&entry.state), ShellProcessState::Terminating);
    }

    #[test]
    fn a_finished_run_cannot_be_stopped() {
        let entry = entry(id(6), None);
        entry.finish(
            ShellOutcome::Exited,
            Ok(ExecutionResult::cancelled_before_spawn(id(6), 0)),
        );
        assert!(!entry.request_stop());
        assert_eq!(
            *lock(&entry.state),
            ShellProcessState::Finished(ShellOutcome::Exited)
        );
    }

    #[test]
    fn status_reports_the_observed_byte_counts_and_reservation() {
        let process = id(7);
        let mut request = ShellRequest::new("echo hi");
        request.sink = None;
        let entry = ProcessEntry::new(process, Some(99), &request, PathBuf::from("/tmp"), true);
        entry.stdout_bytes.fetch_add(12, Ordering::Relaxed);
        entry.stderr_bytes.fetch_add(3, Ordering::Relaxed);

        let status = entry.status();
        assert_eq!(status.id, process);
        assert_eq!(status.os_pid, Some(99));
        assert_eq!(status.command, "echo hi");
        assert_eq!(status.working_dir, PathBuf::from("/tmp"));
        assert_eq!(status.state, ShellProcessState::Running);
        assert_eq!(status.stdout_bytes, 12);
        assert_eq!(status.stderr_bytes, 3);
        assert!(status.reserved);
    }

    // -- ProcessTable retention --

    #[test]
    fn completed_eviction_keeps_only_the_newest_entries() {
        let table = ProcessTable::new();
        let ids: Vec<_> = (0..3)
            .map(|_| {
                let allocated = table.allocate();
                table.insert(entry(allocated, None));
                table.completed(allocated, 2);
                allocated
            })
            .collect();

        assert_eq!(table.len(), 2);
        assert!(table.entry(ids[0]).is_none(), "the oldest is evicted");
        assert!(table.entry(ids[1]).is_some());
        assert!(table.entry(ids[2]).is_some());
    }

    #[test]
    fn consuming_a_result_removes_the_entry_from_both_views() {
        let table = ProcessTable::new();
        let allocated = table.allocate();
        table.insert(entry(allocated, None));
        table.completed(allocated, 64);
        assert_eq!(table.len(), 1);

        table.consumed(allocated);

        assert_eq!(table.len(), 0);
        assert!(table.entry(allocated).is_none());
        assert!(lock(&table.completed).is_empty());
    }

    #[test]
    fn allocated_identities_are_monotonic() {
        let table = ProcessTable::new();
        let first = table.allocate();
        let second = table.allocate();
        assert!(second > first);
    }

    // -- CaptureState --

    #[test]
    fn a_truncated_snapshot_names_the_limit() {
        let state = CaptureState {
            retained: b"0123".to_vec(),
            truncated: true,
            ..CaptureState::default()
        };
        let snapshot = state.snapshot(4);
        assert_eq!(snapshot.text, "0123\n...[truncated, 4 byte limit]");
        assert!(snapshot.truncated);
        assert!(!snapshot.overflow_truncated);
    }

    #[test]
    fn an_untruncated_snapshot_is_the_bytes_verbatim() {
        let state = CaptureState {
            retained: b"hello".to_vec(),
            io_complete: true,
            ..CaptureState::default()
        };
        let snapshot = state.snapshot(1024);
        assert_eq!(snapshot.text, "hello");
        assert!(!snapshot.truncated);
        assert!(snapshot.io_complete);
    }

    // -- ShellCancel pre-spawn detection --

    #[tokio::test]
    async fn a_latched_watch_is_detected_before_spawn() {
        let (_source, receiver) = watch::channel(true);
        assert!(
            ShellCancel::from_watch(receiver)
                .cancelled_before_spawn()
                .await
        );
    }

    #[tokio::test]
    async fn an_unraised_watch_is_not_detected_before_spawn() {
        let (_source, receiver) = watch::channel(false);
        assert!(
            !ShellCancel::from_watch(receiver)
                .cancelled_before_spawn()
                .await
        );
    }

    #[tokio::test]
    async fn an_already_raised_process_stop_is_not_lost() {
        let (source, mut receiver) = watch::channel(false);
        source.send_replace(true);
        tokio::time::timeout(Duration::from_millis(50), stop_requested(&mut receiver))
            .await
            .expect("an already-raised process stop must resolve immediately");
    }

    #[tokio::test]
    async fn an_immediately_ready_factory_is_detected_before_spawn() {
        // A factory ready on its first poll must prevent spawning.
        assert!(
            ShellCancel::from_fn(|| std::future::ready(()))
                .cancelled_before_spawn()
                .await
        );
    }

    #[tokio::test]
    async fn a_never_firing_observer_is_not_detected_before_spawn() {
        assert!(!ShellCancel::never().cancelled_before_spawn().await);
    }
}
