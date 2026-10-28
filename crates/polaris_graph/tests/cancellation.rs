//! Integration tests for cooperative run cancellation.
//!
//! Covers the executor's two check points — before starting a node, and
//! before letting a node-attributable error propagate — plus the three places
//! the executor owns inside a node (retry, the inter-attempt sleep, parallel
//! branches), the outcome's accounting, and the two rules that keep an
//! interrupt from lying: a completed run stays completed, and a structural
//! fault is never reported as an interrupt.

mod test_utils;

use polaris_graph::cancel::{CancellationSource, RunCancellation};
use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::hooks::events::{GraphEvent, RunId};
use polaris_graph::hooks::schedule::{
    OnGraphComplete, OnGraphFailure, OnGraphStart, OnLoopIteration, OnParallelComplete,
    OnSystemComplete,
};
use polaris_graph::middleware::{MiddlewareAPI, MiddlewareError};
use polaris_graph::node::{ContextPolicy, DynamicSlot, NodeId, RetryPolicy};
use polaris_graph::{CaughtError, ExecutionError};
use polaris_system::param::{ParamError, Res, SystemContext};
use polaris_system::system;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use test_utils::{FlagSystem, SlowSystem, SwitchOutput, branch};

// ═══════════════════════════════════════════════════════════════════════════
// Test systems
// ═══════════════════════════════════════════════════════════════════════════

/// Raises the run's signal from a source it captured at construction.
///
/// Capturing the source rather than reading it from the context is what lets
/// the same system raise from inside an isolated scope, whose policy hides
/// every parent local.
struct Raise {
    source: CancellationSource,
    /// Delay before raising. Non-zero lets a concurrent sibling reach its own
    /// await first, so the raise lands while it is genuinely in flight.
    after: Duration,
}

impl Raise {
    fn now(source: CancellationSource) -> Self {
        Self {
            source,
            after: Duration::ZERO,
        }
    }
}

impl System for Raise {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            if !self.after.is_zero() {
                tokio::time::sleep(self.after).await;
            }
            self.source.cancel();
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "raise"
    }
}

/// Runs to completion regardless of the signal, then reports its own output.
struct SleepThenProduce {
    duration: Duration,
    value: i32,
    finished: Option<Arc<AtomicBool>>,
}

impl SleepThenProduce {
    fn new(duration: Duration, value: i32) -> Self {
        Self {
            duration,
            value,
            finished: None,
        }
    }

    fn tracked(duration: Duration, value: i32, finished: Arc<AtomicBool>) -> Self {
        Self {
            duration,
            value,
            finished: Some(finished),
        }
    }
}

impl System for SleepThenProduce {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            tokio::time::sleep(self.duration).await;
            if let Some(finished) = &self.finished {
                finished.store(true, Ordering::SeqCst);
            }
            Ok(self.value)
        })
    }

    fn name(&self) -> &'static str {
        "sleep_then_produce"
    }
}

/// Raises from inside a slow system, then ignores the signal and finishes.
struct RaiseThenSleepAndProduce {
    source: CancellationSource,
    duration: Duration,
    value: i32,
}

impl System for RaiseThenSleepAndProduce {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            self.source.cancel();
            tokio::time::sleep(self.duration).await;
            Ok(self.value)
        })
    }

    fn name(&self) -> &'static str {
        "raise_then_sleep_and_produce"
    }
}

/// Races a long sleep against the signal and returns promptly when it wins.
///
/// The `Err` is diagnostic only: the executor reports `Interrupted` whatever
/// the message says, and does not follow the node's error edge.
struct RacesTheToken {
    sleep: Duration,
}

impl System for RacesTheToken {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let cancel = ctx
                .get_resource::<RunCancellation>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            match cancel
                .run_until_cancelled(tokio::time::sleep(self.sleep))
                .await
            {
                Some(()) => Ok(()),
                None => Err(SystemError::ExecutionError("interrupted".into())),
            }
        })
    }

    fn name(&self) -> &'static str {
        "races_the_token"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

/// Always fails, counting its attempts, and raises on the first one.
struct FailsAndRaisesOnce {
    source: CancellationSource,
    attempts: Arc<AtomicU32>,
}

/// Fails once, scheduling the cancellation task that will run at backoff.
struct FailsThenSchedulesRaise {
    source: CancellationSource,
    attempts: Arc<AtomicU32>,
    raiser_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl System for FailsThenSchedulesRaise {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                let source = self.source.clone();
                // Tokio does not poll a spawned task synchronously. After this
                // attempt returns, the executor therefore passes the retry
                // pre-check and reaches the backoff await before this runs.
                let task = tokio::spawn(async move {
                    source.cancel();
                });
                *self.raiser_task.lock().unwrap() = Some(task);
            }
            Err(SystemError::ExecutionError(format!(
                "attempt {attempt} failed"
            )))
        })
    }

    fn name(&self) -> &'static str {
        "fails_then_schedules_raise"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

impl System for FailsAndRaisesOnce {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                self.source.cancel();
            }
            Err(SystemError::ExecutionError(format!(
                "attempt {attempt} failed"
            )))
        })
    }

    fn name(&self) -> &'static str {
        "fails_and_raises_once"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

/// Raises on the nth invocation; otherwise succeeds. Counts every call.
struct RaiseOnNthCall {
    source: CancellationSource,
    raise_on: u32,
    calls: Arc<AtomicU32>,
}

impl System for RaiseOnNthCall {
    type Output = bool;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == self.raise_on {
                self.source.cancel();
            }
            Ok(false)
        })
    }

    fn name(&self) -> &'static str {
        "raise_on_nth_call"
    }
}

/// Sets a flag when dropped before its body finished — the drop guard that
/// proves a parallel sibling's future was never dropped by the executor.
struct CompletionGuard {
    completed: bool,
    dropped_early: Arc<AtomicBool>,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.dropped_early.store(true, Ordering::SeqCst);
        }
    }
}

/// Sleeps across an await while holding a drop guard, ignoring the signal.
struct GuardedSleep {
    duration: Duration,
    dropped_early: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl System for GuardedSleep {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut guard = CompletionGuard {
                completed: false,
                dropped_early: Arc::clone(&self.dropped_early),
            };
            tokio::time::sleep(self.duration).await;
            guard.completed = true;
            self.finished.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "guarded_sleep"
    }
}

/// Raises, then reports what `Res<RunCancellation>` resolved to from where it
/// stands — the scope-visibility probe.
struct RaiseThenReadToken {
    source: CancellationSource,
    observed: Arc<Mutex<Option<Result<bool, ParamError>>>>,
}

impl System for RaiseThenReadToken {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            self.source.cancel();
            let observed = ctx
                .get_resource::<RunCancellation>()
                .map(|cancel| cancel.is_cancelled());
            *self.observed.lock().unwrap() = Some(observed);
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "raise_then_read_token"
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Collects the `run_id` of every event the run emits, plus which terminal
/// schedules fired.
#[derive(Clone, Default)]
struct EventLog {
    run_ids: Arc<Mutex<Vec<RunId>>>,
    graph_complete: Arc<AtomicBool>,
    graph_failure: Arc<Mutex<Option<ExecutionError>>>,
    system_completes: Arc<AtomicU32>,
    loop_iterations: Arc<AtomicU32>,
    parallel_complete: Arc<AtomicBool>,
}

impl EventLog {
    fn register(hooks: &HooksAPI) -> Self {
        let log = Self::default();

        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnGraphStart, _>("collect_start", move |event: &GraphEvent| {
                ids.lock().unwrap().push(event.run_id().clone());
            })
            .expect("hook registration should succeed");

        let complete = Arc::clone(&log.graph_complete);
        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnGraphComplete, _>(
                "collect_complete",
                move |event: &GraphEvent| {
                    ids.lock().unwrap().push(event.run_id().clone());
                    complete.store(true, Ordering::SeqCst);
                },
            )
            .expect("hook registration should succeed");

        let failure = Arc::clone(&log.graph_failure);
        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnGraphFailure, _>("collect_failure", move |event: &GraphEvent| {
                ids.lock().unwrap().push(event.run_id().clone());
                if let GraphEvent::GraphFailure { error, .. } = event {
                    *failure.lock().unwrap() = Some(error.clone());
                }
            })
            .expect("hook registration should succeed");

        let completes = Arc::clone(&log.system_completes);
        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnSystemComplete, _>(
                "collect_system_complete",
                move |event: &GraphEvent| {
                    ids.lock().unwrap().push(event.run_id().clone());
                    completes.fetch_add(1, Ordering::SeqCst);
                },
            )
            .expect("hook registration should succeed");

        let iterations = Arc::clone(&log.loop_iterations);
        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnLoopIteration, _>(
                "collect_loop_iteration",
                move |event: &GraphEvent| {
                    ids.lock().unwrap().push(event.run_id().clone());
                    iterations.fetch_add(1, Ordering::SeqCst);
                },
            )
            .expect("hook registration should succeed");

        let parallel = Arc::clone(&log.parallel_complete);
        let ids = Arc::clone(&log.run_ids);
        hooks
            .register_observer::<OnParallelComplete, _>(
                "collect_parallel_complete",
                move |event: &GraphEvent| {
                    ids.lock().unwrap().push(event.run_id().clone());
                    parallel.store(true, Ordering::SeqCst);
                },
            )
            .expect("hook registration should succeed");

        log
    }

    fn failure(&self) -> Option<ExecutionError> {
        self.graph_failure.lock().unwrap().clone()
    }
}

/// Unwraps an `Interrupted` outcome, failing the test with the actual error
/// otherwise.
#[track_caller]
fn expect_interrupted(err: ExecutionError) -> (RunId, NodeId, usize) {
    match err {
        ExecutionError::Interrupted {
            run_id,
            node,
            nodes_executed,
            ..
        } => (run_id, node, nodes_executed),
        other => panic!("expected ExecutionError::Interrupted, got {other:?}"),
    }
}

#[track_caller]
fn assert_graph_valid(graph: &Graph) {
    let validation = graph.validate();
    assert!(
        validation.is_ok(),
        "graph should be structurally valid before execution: {:?}",
        validation.errors
    );
}

/// Returns the id of the node `name` in `graph`.
#[track_caller]
fn node_named(graph: &Graph, name: &str) -> NodeId {
    graph
        .nodes()
        .iter()
        .find(|node| node.name() == name)
        .unwrap_or_else(|| panic!("graph has no node named `{name}`"))
        .id()
        .clone()
}

fn flag_is_set(flag: &Arc<Mutex<bool>>) -> bool {
    *flag.lock().unwrap()
}

// ═══════════════════════════════════════════════════════════════════════════
// 1 — the between-node stop on a sequential chain
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_raise_between_nodes_declines_the_next_node() {
    let source = CancellationSource::new();
    let third_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));
    graph.add_boxed_system(Box::new(Raise::now(source.clone())));
    let third = graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&third_ran),
    }));

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(node, third, "the stop is reported at the declined node");
    assert_eq!(
        nodes_executed, 2,
        "the two nodes that started are counted; the declined one is not"
    );
    assert!(!flag_is_set(&third_ran), "the third node must never start");
    assert!(
        !log.graph_complete.load(Ordering::SeqCst),
        "OnGraphComplete must not fire for an interrupted run"
    );
    assert!(
        matches!(log.failure(), Some(ExecutionError::Interrupted { .. })),
        "OnGraphFailure must fire carrying the Interrupted variant, got {:?}",
        log.failure()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2 — a system that races the token
// ═══════════════════════════════════════════════════════════════════════════

/// The racing system's `Err` must not reach its error edge, and the handler
/// it selects must never start.
#[tokio::test]
async fn a_racing_system_returns_promptly_and_its_error_handler_does_not_run() {
    let source = CancellationSource::new();
    let handler_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let racing = graph.add_boxed_system(Box::new(RacesTheToken {
        sleep: Duration::from_secs(10),
    }));
    let handler_flag = Arc::clone(&handler_ran);
    graph.add_error_handler_for([racing], move |g| {
        g.add_boxed_system(Box::new(FlagSystem { flag: handler_flag }));
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let raiser = source.clone();
    let raiser_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        raiser.cancel();
    });

    let started = Instant::now();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");
    let elapsed = started.elapsed();
    raiser_task.await.expect("the raiser task should complete");

    expect_interrupted(err);
    assert!(
        !flag_is_set(&handler_ran),
        "the error handler must never start"
    );
    // The system's own future was racing a 10 s sleep. Anything under a
    // second proves the race was won and the sleep abandoned; the bound is
    // deliberately loose so a loaded machine cannot make this flaky.
    assert!(
        elapsed < Duration::from_secs(1),
        "the racing system must return promptly, took {elapsed:?}"
    );
    assert!(
        ctx.get_output::<CaughtError>().is_ok(),
        "the system's CaughtError is a truthful record of what it did and stays"
    );
}

#[tokio::test]
async fn a_racing_system_without_an_error_edge_still_ends_interrupted() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(RacesTheToken {
        sleep: Duration::from_secs(10),
    }));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let raiser = source.clone();
    let raiser_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        raiser.cancel();
    });

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");
    raiser_task.await.expect("the raiser task should complete");

    // Without the second check point this would surface as SystemError, and a
    // driver could not tell a user's interrupt from a failing provider call.
    let (_, _, nodes_executed) = expect_interrupted(err);
    assert_eq!(nodes_executed, 1, "the failed node is counted");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3 — a system that ignores the token
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_system_that_ignores_the_token_runs_to_completion() {
    let source = CancellationSource::new();
    let second_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_millis(200),
        value: 42,
    }));
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&second_ran),
    }));

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let started = Instant::now();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");
    let elapsed = started.elapsed();

    let (_, _, nodes_executed) = expect_interrupted(err);
    assert_eq!(nodes_executed, 1);
    assert!(!flag_is_set(&second_ran), "the next node must never start");
    assert!(
        elapsed >= Duration::from_millis(200),
        "latency is bounded by the node that ignores the signal, took {elapsed:?}"
    );
    assert_eq!(
        ctx.get_output::<i32>().ok().map(|value| *value),
        Some(42),
        "the ignoring system's output is stored exactly as today"
    );
    assert_eq!(
        log.system_completes.load(Ordering::SeqCst),
        1,
        "OnSystemComplete fires for the system that ran"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 4 — a per-node timeout handler
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_timeout_handler_does_not_start_when_the_token_is_raised() {
    let source = CancellationSource::new();
    let handler_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let slow = graph.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_millis(300),
        value: 1,
    }));
    graph.set_timeout(slow.clone(), Duration::from_millis(50));
    let handler_flag = Arc::clone(&handler_ran);
    graph.add_timeout_handler([slow], move |g| {
        g.add_boxed_system(Box::new(FlagSystem { flag: handler_flag }));
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    expect_interrupted(err);
    assert!(
        !flag_is_set(&handler_ran),
        "the timeout edge still selects the handler; the boundary check declines to start it"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 5 — retries and the inter-attempt sleep
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_raise_stops_the_retry_sequence_without_waiting_for_the_backoff() {
    let source = CancellationSource::new();
    let attempts = Arc::new(AtomicU32::new(0));

    let mut graph = Graph::new();
    let node = graph.add_boxed_system(Box::new(FailsAndRaisesOnce {
        source: source.clone(),
        attempts: Arc::clone(&attempts),
    }));
    // The first attempt raises before it returns, so the pre-check before the
    // next attempt stops the sequence before the backoff begins.
    graph.set_retry_policy(node, RetryPolicy::fixed(2, Duration::from_secs(5)));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let started = Instant::now();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");
    let elapsed = started.elapsed();

    expect_interrupted(err);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "the attempt that was running finished; no further attempt started"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the inter-attempt sleep must not delay the stop, took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_raise_during_retry_backoff_stops_the_sleep() {
    let source = CancellationSource::new();
    let attempts = Arc::new(AtomicU32::new(0));
    let raiser_task = Arc::new(Mutex::new(None));

    let mut graph = Graph::new();
    let node = graph.add_boxed_system(Box::new(FailsThenSchedulesRaise {
        source: source.clone(),
        attempts: Arc::clone(&attempts),
        raiser_task: Arc::clone(&raiser_task),
    }));
    graph.set_retry_policy(node, RetryPolicy::fixed(2, Duration::from_secs(5)));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let started = Instant::now();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("cancellation during retry backoff must stop the run");
    let elapsed = started.elapsed();

    let task = raiser_task
        .lock()
        .unwrap()
        .take()
        .expect("the first failure should schedule the raiser task");
    task.await.expect("the raiser task should complete");

    expect_interrupted(err);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "no retry should start after cancellation wins the backoff race"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the five-second backoff must be interrupted in milliseconds, took {elapsed:?}"
    );
}

#[tokio::test]
async fn an_unraised_run_still_retries_and_sleeps_as_before() {
    // Regression guard for the gate added above: the retry path is unchanged
    // for a run that is never interrupted.
    let source = CancellationSource::new();
    let attempts = Arc::new(AtomicU32::new(0));

    let mut graph = Graph::new();
    let node = graph.add_boxed_system(Box::new(FailsAndRaisesOnce {
        // A source nobody ever raises.
        source: CancellationSource::new(),
        attempts: Arc::clone(&attempts),
    }));
    graph.set_retry_policy(node, RetryPolicy::fixed(2, Duration::from_millis(1)));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the system always fails");

    assert!(
        matches!(err, ExecutionError::SystemError(_)),
        "an uninterrupted failure is still a SystemError, got {err:?}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "all three attempts run when the token is never raised"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 6 — loops
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_raise_stops_the_loop_before_the_next_iteration() {
    let source = CancellationSource::new();
    let calls = Arc::new(AtomicU32::new(0));

    let mut graph = Graph::new();
    let body_calls = Arc::clone(&calls);
    let body_source = source.clone();
    graph.add_loop_n("work_loop", 100, move |g| {
        g.add_boxed_system(Box::new(RaiseOnNthCall {
            source: body_source,
            raise_on: 2,
            calls: body_calls,
        }));
    });

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, _, nodes_executed) = expect_interrupted(err);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "iteration three's body must never start"
    );
    assert_eq!(
        log.loop_iterations.load(Ordering::SeqCst),
        2,
        "an iteration that never starts must not be reported as one that did"
    );
    // The loop node itself plus the two bodies that ran — the same total
    // `ExecutionResult::nodes_executed` would have reported.
    assert_eq!(
        nodes_executed, 3,
        "the iterations that already ran stay in the count"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7 — parallel branches are never dropped
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_parallel_sibling_is_allowed_to_finish() {
    let source = CancellationSource::new();
    let dropped_early = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let declined_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let fast_source = source.clone();
    let slow_dropped = Arc::clone(&dropped_early);
    let slow_finished = Arc::clone(&finished);
    let declined_flag = Arc::clone(&declined_ran);
    graph.add_parallel(
        "fan_out",
        [
            branch(move |g| {
                g.add_boxed_system(Box::new(Raise {
                    source: fast_source,
                    after: Duration::from_millis(20),
                }));
                g.add_boxed_system(Box::new(FlagSystem {
                    flag: declined_flag,
                }));
            }),
            branch(move |g| {
                g.add_boxed_system(Box::new(GuardedSleep {
                    duration: Duration::from_millis(100),
                    dropped_early: slow_dropped,
                    finished: slow_finished,
                }));
            }),
        ],
    );

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    let started = Instant::now();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");
    let elapsed = started.elapsed();

    expect_interrupted(err);
    assert!(
        !flag_is_set(&declined_ran),
        "the node after the raise must be declined"
    );
    assert!(
        !dropped_early.load(Ordering::SeqCst),
        "the executor must never drop a running sibling's future"
    );
    assert!(
        finished.load(Ordering::SeqCst),
        "the slow branch must run to completion"
    );
    assert!(
        elapsed >= Duration::from_millis(100),
        "interrupt latency is the slowest branch's — the price of never dropping a future, took {elapsed:?}"
    );
    assert!(
        !log.parallel_complete.load(Ordering::SeqCst),
        "OnParallelComplete must not fire, consistent with any other propagating error"
    );
}

#[tokio::test]
async fn an_interrupted_parallel_branch_does_not_drop_its_sibling() {
    let source = CancellationSource::new();
    let tracker_ran = Arc::new(Mutex::new(false));
    let sibling_finished = Arc::new(AtomicBool::new(false));

    let mut graph = Graph::new();
    let fast_source = source.clone();
    let tracker_flag = Arc::clone(&tracker_ran);
    let finished = Arc::clone(&sibling_finished);
    graph.add_parallel(
        "fan_out",
        [
            branch(move |g| {
                g.add_boxed_system(Box::new(SleepThenProduce::tracked(
                    Duration::from_millis(60),
                    22,
                    finished,
                )));
            }),
            branch(move |g| {
                g.add_boxed_system(Box::new(Raise::now(fast_source)));
                g.add_boxed_system(Box::new(FlagSystem { flag: tracker_flag }));
            }),
        ],
    );
    let fan_out = graph
        .nodes()
        .iter()
        .find(|node| node.name() == "fan_out")
        .expect("the parallel node exists")
        .id()
        .clone();

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the interrupted branch must stop the parallel node after the join");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert!(
        !flag_is_set(&tracker_ran),
        "the node after the raise must be declined"
    );
    assert!(
        sibling_finished.load(Ordering::SeqCst),
        "the sleeping sibling must finish rather than be dropped"
    );
    assert_eq!(
        ctx.get_output::<i32>().ok().map(|value| *value),
        Some(22),
        "the completed sibling's output must merge into the parent"
    );
    assert_eq!(node, fan_out, "the parallel node must report the stop");
    assert_eq!(
        nodes_executed, 3,
        "the fan-out, raise, and sleeping sibling are counted; the declined tracker is not"
    );
}

/// The merge happens before the stop is reported, so the work the executor
/// deliberately waited for is not thrown away.
#[tokio::test]
async fn completed_parallel_branch_outputs_reach_the_parent_after_an_interrupt() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let fast_source = source.clone();
    graph.add_parallel(
        "fan_out",
        [
            branch(move |g| {
                g.add_boxed_system(Box::new(SleepThenProduce::new(
                    Duration::from_millis(20),
                    11,
                )));
                g.add_boxed_system(Box::new(Raise::now(fast_source)));
                // Declined once the signal is raised, so the branch stops early.
                g.add_boxed_system(Box::new(FlagSystem {
                    flag: Arc::new(Mutex::new(false)),
                }));
            }),
            branch(move |g| {
                g.add_boxed_system(Box::new(SleepThenProduce::new(
                    Duration::from_millis(60),
                    22,
                )));
            }),
        ],
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, _) = expect_interrupted(err);
    assert_eq!(
        node.to_string(),
        graph
            .nodes()
            .iter()
            .find(|n| n.name() == "fan_out")
            .expect("the parallel node exists")
            .id()
            .to_string(),
        "the parallel node reports the stop; the innermost branch boundary is not recoverable"
    );
    // Both branches produce `i32`; the merge is branch-order deterministic, so
    // the later branch's value is the one left. Its presence is what proves the
    // outputs crossed at all.
    assert_eq!(
        ctx.get_output::<i32>().ok().map(|value| *value),
        Some(22),
        "branch outputs must be merged into the parent before the stop is reported"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7a — a parallel node whose branches all finished
// ═══════════════════════════════════════════════════════════════════════════

/// The completion rule holds for a final parallel node: the raise lands while a
/// sibling is still running, no branch declines anything, and there is no
/// successor to decline.
#[tokio::test]
async fn a_final_parallel_node_completes_when_no_branch_stopped_early() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let (_, dropped_early, finished) = raise_beside_a_guarded_sibling(&mut graph, &source);

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect("a run whose last node finished all its work stays completed");

    assert!(source.is_cancelled(), "the signal really was raised");
    assert!(
        finished.load(Ordering::SeqCst) && !dropped_early.load(Ordering::SeqCst),
        "the sibling the raise landed beside ran to completion"
    );
    assert_eq!(
        result.nodes_executed(),
        3,
        "the fan-out and both branch systems are counted"
    );
    assert!(
        log.parallel_complete.load(Ordering::SeqCst),
        "the parallel node completed, so OnParallelComplete fires"
    );
    assert!(
        log.graph_complete.load(Ordering::SeqCst),
        "OnGraphComplete fires for a completed run"
    );
}

/// A parallel node that completed under a raised signal is not what stops the
/// run: the node after it is declined, and that node is where the stop is
/// reported.
#[tokio::test]
async fn a_parallel_node_whose_branches_all_finished_declines_its_successor() {
    let source = CancellationSource::new();
    let successor_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let (_, dropped_early, finished) = raise_beside_a_guarded_sibling(&mut graph, &source);
    let successor = graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&successor_ran),
    }));

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run before the successor");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(node, successor, "the stop is reported at the declined node");
    assert_eq!(
        nodes_executed, 3,
        "the fan-out and both branch systems are counted; the declined successor is not"
    );
    assert!(
        !flag_is_set(&successor_ran),
        "the successor must never start"
    );
    assert!(
        finished.load(Ordering::SeqCst) && !dropped_early.load(Ordering::SeqCst),
        "the sibling ran to completion"
    );
    assert!(
        log.parallel_complete.load(Ordering::SeqCst),
        "the parallel node completed, so OnParallelComplete fires"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7b — a branch's own middleware fails while the run is being interrupted
// ═══════════════════════════════════════════════════════════════════════════

/// Registers branch middleware that lets branch `failing_branch` run, then
/// fails it with `error` — the shape of an exporter or span closer that errors
/// after the branch body finished.
fn fail_branch_after_it_runs(
    middleware: &MiddlewareAPI,
    failing_branch: usize,
    error: impl Fn(&NodeId) -> MiddlewareError + Send + Sync + 'static,
) {
    middleware.register_parallel_branch("failing_after_run", move |info, ctx, next| {
        let failure = (info.branch_index == failing_branch).then(|| error(&info.node_id));
        Box::pin(async move {
            next.run(ctx).await?;
            match failure {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        })
    });
}

/// A two-branch fan-out: branch 0 raises after 20 ms, branch 1 holds a drop
/// guard across a 100 ms sleep. Returns the fan-out's id and the sibling's
/// `(dropped_early, finished)` flags.
fn raise_beside_a_guarded_sibling(
    graph: &mut Graph,
    source: &CancellationSource,
) -> (NodeId, Arc<AtomicBool>, Arc<AtomicBool>) {
    let dropped_early = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));

    let raiser = source.clone();
    let sibling_dropped = Arc::clone(&dropped_early);
    let sibling_finished = Arc::clone(&finished);
    graph.add_parallel(
        "fan_out",
        [
            branch(move |g| {
                g.add_boxed_system(Box::new(Raise {
                    source: raiser,
                    after: Duration::from_millis(20),
                }));
            }),
            branch(move |g| {
                g.add_boxed_system(Box::new(GuardedSleep {
                    duration: Duration::from_millis(100),
                    dropped_early: sibling_dropped,
                    finished: sibling_finished,
                }));
            }),
        ],
    );
    (node_named(graph, "fan_out"), dropped_early, finished)
}

#[tokio::test]
async fn a_branch_middleware_error_after_the_raise_does_not_drop_a_running_sibling() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let (fan_out, dropped_early, finished) = raise_beside_a_guarded_sibling(&mut graph, &source);

    let middleware = MiddlewareAPI::new();
    fail_branch_after_it_runs(&middleware, 0, |_| {
        MiddlewareError::Layer("export failed".into())
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("a raised token must stop the run");

    assert!(
        !dropped_early.load(Ordering::SeqCst),
        "a branch's middleware failing must not drop a sibling that is still running"
    );
    assert!(
        finished.load(Ordering::SeqCst),
        "the sibling must run to completion before the stop is reported"
    );
    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(node, fan_out, "the parallel node reports the stop");
    assert_eq!(
        nodes_executed, 3,
        "the fan-out and both branch systems are counted, including the branch \
         whose middleware failed"
    );
}

#[tokio::test]
async fn outputs_of_every_branch_are_merged_when_a_branch_middleware_fails_after_the_raise() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let raiser = source.clone();
    graph.add_parallel(
        "fan_out",
        [
            branch(move |g| {
                g.add_boxed_system(Box::new(Raise {
                    source: raiser,
                    after: Duration::from_millis(20),
                }));
            }),
            branch(|g| {
                g.add_boxed_system(Box::new(SleepThenProduce::new(
                    Duration::from_millis(80),
                    22,
                )));
            }),
        ],
    );

    let middleware = MiddlewareAPI::new();
    fail_branch_after_it_runs(&middleware, 0, |_| {
        MiddlewareError::Layer("export failed".into())
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("a raised token must stop the run");

    expect_interrupted(err);
    assert_eq!(
        ctx.get_output::<i32>().ok().map(|value| *value),
        Some(22),
        "the sibling the executor waited for must have its output merged"
    );
}

#[tokio::test]
async fn a_structural_fault_from_branch_middleware_is_reported_after_the_siblings_finish() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let (_, dropped_early, finished) = raise_beside_a_guarded_sibling(&mut graph, &source);

    let middleware = MiddlewareAPI::new();
    fail_branch_after_it_runs(&middleware, 0, |node| {
        MiddlewareError::Inner(ExecutionError::NodeNotFound(node.clone()))
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("the branch reported a structural fault");

    assert!(
        matches!(err, ExecutionError::NodeNotFound(_)),
        "a structural fault is reported as itself, not as an interrupt; got {err:?}"
    );
    assert!(
        !dropped_early.load(Ordering::SeqCst) && finished.load(Ordering::SeqCst),
        "a raised signal never drops a running sibling, even when the stop is not an interrupt"
    );
}

#[tokio::test]
async fn a_branch_middleware_error_without_a_raise_still_fails_fast() {
    let source = CancellationSource::new();
    let dropped_early = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));

    let mut graph = Graph::new();
    let sibling_dropped = Arc::clone(&dropped_early);
    let sibling_finished = Arc::clone(&finished);
    graph.add_parallel(
        "fan_out",
        [
            branch(|g| {
                g.add_boxed_system(Box::new(SlowSystem {
                    duration: Duration::from_millis(1),
                }));
            }),
            branch(move |g| {
                g.add_boxed_system(Box::new(GuardedSleep {
                    duration: Duration::from_secs(5),
                    dropped_early: sibling_dropped,
                    finished: sibling_finished,
                }));
            }),
        ],
    );

    let middleware = MiddlewareAPI::new();
    fail_branch_after_it_runs(&middleware, 0, |_| {
        MiddlewareError::Layer("export failed".into())
    });

    // A token is present but never raised: a branch failure keeps its
    // documented fail-fast behaviour and aborts the siblings.
    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let started = Instant::now();
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("the branch middleware failed");

    assert!(
        matches!(err, ExecutionError::MiddlewareError { .. }),
        "an uninterrupted middleware failure is still a MiddlewareError, got {err:?}"
    );
    assert!(
        dropped_early.load(Ordering::SeqCst) && !finished.load(Ordering::SeqCst),
        "without a raise, a failing branch still aborts its siblings"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the failure must not wait for the 5 s sibling, took {:?}",
        started.elapsed()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 8 — isolated scopes
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_isolated_interior_stops_at_its_boundary_even_though_it_cannot_read_the_token() {
    let source = CancellationSource::new();
    let observed = Arc::new(Mutex::new(None));
    let second_ran = Arc::new(Mutex::new(false));

    let mut interior = Graph::new();
    interior.add_boxed_system(Box::new(RaiseThenReadToken {
        source: source.clone(),
        observed: Arc::clone(&observed),
    }));
    interior.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&second_ran),
    }));

    let mut graph = Graph::new();
    graph.add_scope("isolated", interior, ContextPolicy::new());

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    expect_interrupted(err);
    assert!(
        matches!(
            observed.lock().unwrap().as_ref(),
            Some(Err(ParamError::ResourceOutOfScope(_)))
        ),
        "pure isolation hides the token from in-node reads, got {:?}",
        observed.lock().unwrap()
    );
    assert!(
        !flag_is_set(&second_ran),
        "the between-node stop needs no crossing verb: the executor holds the token from the root"
    );
}

#[tokio::test]
async fn sharing_the_token_lets_an_isolated_interior_observe_it() {
    let source = CancellationSource::new();
    let observed = Arc::new(Mutex::new(None));
    let second_ran = Arc::new(Mutex::new(false));

    let mut interior = Graph::new();
    interior.add_boxed_system(Box::new(RaiseThenReadToken {
        source: source.clone(),
        observed: Arc::clone(&observed),
    }));
    interior.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&second_ran),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "isolated",
        interior,
        ContextPolicy::new().share::<RunCancellation>(),
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    expect_interrupted(err);
    assert!(
        matches!(observed.lock().unwrap().as_ref(), Some(Ok(true))),
        "with `.share::<RunCancellation>()` the interior reads the same latch the executor holds, got {:?}",
        observed.lock().unwrap()
    );
    assert!(!flag_is_set(&second_ran));
}

// ═══════════════════════════════════════════════════════════════════════════
// 9 — raised before the run starts
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_token_raised_before_execute_stops_at_the_entry_node() {
    let source = CancellationSource::new();
    let entry_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let entry = graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&entry_ran),
    }));

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());
    source.cancel();

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(node, entry);
    assert_eq!(nodes_executed, 0);
    assert!(!flag_is_set(&entry_ran), "not one node starts");
    assert!(
        !log.run_ids.lock().unwrap().is_empty(),
        "OnGraphStart still fires before the stop"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 10 — no token, and the never-firing token
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_run_without_a_token_completes_exactly_as_before() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SlowSystem {
        duration: Duration::from_millis(1),
    }));
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let mut ctx = SystemContext::new();
    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("a run with no token in context is not cancellable and completes");

    assert_eq!(result.nodes_executed(), 2);
}

#[tokio::test]
async fn a_never_firing_token_behaves_like_no_token_at_all() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SlowSystem {
        duration: Duration::from_millis(1),
    }));
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let mut ctx = SystemContext::new();
    ctx.insert(RunCancellation::never());

    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("a never-firing token never stops a run");

    assert_eq!(result.nodes_executed(), 2);
}

/// A run that reaches its terminal node completes, even though the signal was
/// raised while the last node ran. There is no check after the last node, and a
/// run that did all its work is not retroactively reported as stopped.
#[tokio::test]
async fn a_run_that_reaches_its_terminal_node_completes_despite_a_raise() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(Raise::now(source.clone())));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("a completed run stays completed");

    assert_eq!(result.nodes_executed(), 1);
    assert!(source.is_cancelled(), "the signal really was raised");
}

#[tokio::test]
async fn a_completed_runs_duration_includes_graph_middleware_entry() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let middleware = MiddlewareAPI::new();
    middleware.register_graph_execution("entry_delay", |_info, ctx, next| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            next.run(ctx).await
        })
    });

    let mut ctx = SystemContext::new();
    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect("the graph should complete");

    assert!(
        result.duration() >= Duration::from_millis(40),
        "completed and interrupted runs must measure from the same pre-middleware origin; got {:?}",
        result.duration()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 11 — run identity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_variants_run_id_matches_every_event_of_that_run() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(Raise::now(source.clone())));
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let hooks = HooksAPI::new();
    let log = EventLog::register(&hooks);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect_err("a raised token must stop the run");

    let (run_id, _, _) = expect_interrupted(err);
    let ids = log.run_ids.lock().unwrap();
    assert!(!ids.is_empty(), "the run emitted events");
    assert!(
        ids.iter().all(|id| *id == run_id),
        "every event of the run carries the run id the variant reports"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 12 — the signature cost, recorded
// ═══════════════════════════════════════════════════════════════════════════

#[system]
async fn reads_the_token(cancel: Res<RunCancellation>) -> bool {
    cancel.is_cancelled()
}

#[tokio::test]
async fn reading_the_token_appears_in_the_derived_signature() {
    let mut graph = Graph::new();
    graph.add_system(reads_the_token);

    let signature = graph.signature();
    assert!(
        signature
            .requires()
            .iter()
            .any(|access| access.type_id == std::any::TypeId::of::<RunCancellation>()),
        "a cancellable graph declares the token in its interface; requires = {:?}",
        signature.requires()
    );
}

#[tokio::test]
async fn reading_the_token_without_providing_it_names_the_missing_resource() {
    let mut graph = Graph::new();
    graph.add_system(reads_the_token);

    let mut ctx = SystemContext::new();
    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a declared RunCancellation read requires the resource at runtime");

    match err {
        ExecutionError::SystemError(message) => {
            assert!(
                message.contains("resource not found"),
                "the parameter failure must retain the ResourceNotFound variant's message: {message}"
            );
            assert!(
                message.contains(std::any::type_name::<RunCancellation>()),
                "the missing resource message must name RunCancellation: {message}"
            );
        }
        other => panic!("expected SystemError wrapping ResourceNotFound, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 13 — an interrupt never masks a structural fault
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_raised_token_does_not_mask_max_iterations_exceeded() {
    let source = CancellationSource::new();
    let calls = Arc::new(AtomicU32::new(0));

    let mut graph = Graph::new();
    let body_calls = Arc::clone(&calls);
    let body_source = source.clone();
    // The predicate never terminates the loop, so exhausting the limit is a
    // reportable fault rather than a normal exit.
    graph.add_loop::<bool, _, _>(
        "never_done",
        |done: &bool| *done,
        move |g| {
            g.add_boxed_system(Box::new(RaiseOnNthCall {
                source: body_source,
                raise_on: 2,
                calls: body_calls,
            }));
        },
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());
    ctx.insert_output(false);

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .with_default_max_iterations(2)
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the loop exhausts its iteration limit");

    assert!(source.is_cancelled(), "the token really is raised");
    assert!(
        matches!(err, ExecutionError::MaxIterationsExceeded { .. }),
        "a structural fault must be reported as itself, not as an interrupt; got {err:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 14 — an interrupt reaches the dispatch loop through every construct
// ═══════════════════════════════════════════════════════════════════════════

/// Builds an interior that raises and then holds one more node, which the
/// executor must decline. The declined node's id is stored in `declined`.
fn raise_then_decline(
    g: &mut Graph,
    source: CancellationSource,
    ran: Arc<Mutex<bool>>,
    declined: &Arc<Mutex<Option<NodeId>>>,
) {
    g.add_boxed_system(Box::new(Raise::now(source)));
    let id = g.add_boxed_system(Box::new(FlagSystem { flag: ran }));
    *declined.lock().unwrap() = Some(id);
}

#[tokio::test]
async fn a_raise_inside_a_decision_branch_declines_the_next_node_in_that_branch() {
    let source = CancellationSource::new();
    let ran = Arc::new(Mutex::new(false));
    let declined = Arc::new(Mutex::new(None));

    let mut graph = Graph::new();
    let (raiser, flag, slot) = (source.clone(), Arc::clone(&ran), Arc::clone(&declined));
    graph.add_conditional_branch::<bool, _, _, _>(
        "decide",
        |take_true| *take_true,
        move |g| raise_then_decline(g, raiser, flag, &slot),
        |g| {
            g.add_boxed_system(Box::new(FlagSystem {
                flag: Arc::new(Mutex::new(false)),
            }));
        },
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());
    ctx.insert_output(true);

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(
        Some(node),
        declined.lock().unwrap().clone(),
        "the stop is reported at the node declined inside the branch"
    );
    assert_eq!(
        nodes_executed, 2,
        "the decision and the system that raised are counted; the declined node is not"
    );
    assert!(!flag_is_set(&ran), "the declined node must never start");
}

#[tokio::test]
async fn a_raise_inside_a_switch_case_declines_the_next_node_in_that_case() {
    let source = CancellationSource::new();
    let ran = Arc::new(Mutex::new(false));
    let declined = Arc::new(Mutex::new(None));

    let mut graph = Graph::new();
    let (raiser, flag, slot) = (source.clone(), Arc::clone(&ran), Arc::clone(&declined));
    graph.add_switch::<SwitchOutput, _, _, _>(
        "route",
        |output| output.key,
        [(
            "go",
            branch(move |g| raise_then_decline(g, raiser, flag, &slot)),
        )],
        None::<Box<dyn FnOnce(&mut Graph)>>,
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());
    ctx.insert_output(SwitchOutput { key: "go" });

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(
        Some(node),
        declined.lock().unwrap().clone(),
        "the stop is reported at the node declined inside the case"
    );
    assert_eq!(
        nodes_executed, 2,
        "the switch and the system that raised are counted; the declined node is not"
    );
    assert!(!flag_is_set(&ran), "the declined node must never start");
}

#[tokio::test]
async fn a_raise_inside_a_dynamic_candidate_declines_the_next_node_in_that_candidate() {
    let source = CancellationSource::new();
    let ran = Arc::new(Mutex::new(false));
    let declined = Arc::new(Mutex::new(None));

    let mut candidate = Graph::new();
    raise_then_decline(&mut candidate, source.clone(), Arc::clone(&ran), &declined);
    let contract = candidate.signature();

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| "only",
        [("only", candidate)],
        DynamicSlot::new(contract, ContextPolicy::shared()),
    );

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");

    let (_, node, nodes_executed) = expect_interrupted(err);
    assert_eq!(
        Some(node),
        declined.lock().unwrap().clone(),
        "the stop is reported at the node declined inside the candidate"
    );
    assert_eq!(
        nodes_executed, 2,
        "the dynamic node and the system that raised are counted; the declined node is not"
    );
    assert!(!flag_is_set(&ran), "the declined node must never start");
}

// ═══════════════════════════════════════════════════════════════════════════
// 15 — what an interrupt supersedes, and what it does not
// ═══════════════════════════════════════════════════════════════════════════

/// Registers system middleware that lets the node run, then fails it.
fn fail_every_system_after_it_runs(middleware: &MiddlewareAPI) {
    middleware.register_system("failing_after_run", |_info, ctx, next| {
        Box::pin(async move {
            next.run(ctx).await?;
            Err(MiddlewareError::Layer("export failed".into()))
        })
    });
}

#[tokio::test]
async fn a_middleware_failure_after_the_raise_is_reported_as_an_interrupt() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let node = graph.add_boxed_system(Box::new(Raise::now(source.clone())));

    let middleware = MiddlewareAPI::new();
    fail_every_system_after_it_runs(&middleware);

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("the middleware failed the only node");

    let (_, at, nodes_executed) = expect_interrupted(err);
    assert_eq!(
        at, node,
        "the stop is reported at the node whose error it superseded"
    );
    assert_eq!(nodes_executed, 1, "the failed node is counted");
}

#[tokio::test]
async fn a_middleware_failure_without_a_raise_is_still_a_middleware_failure() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let middleware = MiddlewareAPI::new();
    fail_every_system_after_it_runs(&middleware);

    // A token is present but never raised.
    let mut ctx = SystemContext::new();
    ctx.insert(CancellationSource::new().token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect_err("the middleware failed the only node");

    assert!(
        matches!(err, ExecutionError::MiddlewareError { .. }),
        "an uninterrupted middleware failure keeps its own variant, got {err:?}"
    );
}

#[tokio::test]
async fn a_timeout_without_a_handler_after_the_raise_is_reported_as_an_interrupt() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    let slow = graph.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_millis(300),
        value: 1,
    }));
    graph.set_timeout(slow.clone(), Duration::from_millis(50));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the node times out");

    let (_, at, nodes_executed) = expect_interrupted(err);
    assert_eq!(at, slow, "the stop is reported at the node that timed out");
    assert_eq!(nodes_executed, 1, "the timed-out node is counted");
}

#[tokio::test]
async fn a_timeout_without_a_raise_is_still_a_timeout() {
    let mut graph = Graph::new();
    let slow = graph.add_boxed_system(Box::new(SlowSystem {
        duration: Duration::from_millis(300),
    }));
    graph.set_timeout(slow.clone(), Duration::from_millis(50));

    // A token is present but never raised.
    let mut ctx = SystemContext::new();
    ctx.insert(CancellationSource::new().token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the node times out");

    assert!(
        matches!(&err, ExecutionError::Timeout { node, .. } if *node == slow),
        "an uninterrupted timeout keeps its own variant, got {err:?}"
    );
}

#[tokio::test]
async fn a_graph_timeout_is_not_masked_by_a_raised_token() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    graph.with_max_duration(Duration::from_millis(50));
    graph.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_secs(5),
        value: 1,
    }));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the graph exceeds its maximum duration");

    assert!(source.is_cancelled(), "the token really is raised");
    assert!(
        matches!(err, ExecutionError::GraphTimeout { .. }),
        "a graph timeout is a hard abort and is reported as itself, got {err:?}"
    );
}

#[tokio::test]
async fn a_nested_graph_timeout_is_not_masked_by_a_raised_token() {
    let source = CancellationSource::new();

    let mut interior = Graph::new();
    interior.with_max_duration(Duration::from_millis(50));
    interior.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_secs(5),
        value: 1,
    }));

    let mut graph = Graph::new();
    graph.add_scope("bounded", interior, ContextPolicy::shared());

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the scope exceeds its maximum duration");

    assert!(source.is_cancelled(), "the token really is raised");
    assert!(
        matches!(err, ExecutionError::GraphTimeout { .. }),
        "a nested graph timeout propagates through the dispatch check unchanged, got {err:?}"
    );
}

#[tokio::test]
async fn an_unraised_token_does_not_stop_the_error_handler() {
    let handler_ran = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    let failing = graph.add_boxed_system(Box::new(FailsAndRaisesOnce {
        // A source nobody ever raises.
        source: CancellationSource::new(),
        attempts: Arc::new(AtomicU32::new(0)),
    }));
    let handler_flag = Arc::clone(&handler_ran);
    graph.add_error_handler_for([failing], move |g| {
        g.add_boxed_system(Box::new(FlagSystem { flag: handler_flag }));
    });

    let mut ctx = SystemContext::new();
    ctx.insert(CancellationSource::new().token());

    assert_graph_valid(&graph);
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("the error handler recovers the run");

    assert!(
        flag_is_set(&handler_ran),
        "a token that is present but never raised leaves the error edge alone"
    );
    assert_eq!(
        result.nodes_executed(),
        2,
        "the failing node and its handler are counted"
    );
}

#[tokio::test]
async fn an_interrupted_outcome_reports_how_long_the_run_took_to_stop() {
    let source = CancellationSource::new();

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(RaiseThenSleepAndProduce {
        source: source.clone(),
        duration: Duration::from_millis(100),
        value: 1,
    }));
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::new(Mutex::new(false)),
    }));

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());

    assert_graph_valid(&graph);
    let started = Instant::now();
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a raised token must stop the run");
    let measured = started.elapsed();

    let message = err.to_string();
    let ExecutionError::Interrupted { elapsed, .. } = err else {
        panic!("expected ExecutionError::Interrupted, got {err:?}");
    };
    assert!(
        message.starts_with("run interrupted at node ") && message.contains("after 1 nodes"),
        "the message names where the run stopped and how much of it ran, got {message:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed <= measured,
        "elapsed covers the node that ignored the signal and no more than the whole call; \
         got {elapsed:?} of {measured:?}"
    );
}
