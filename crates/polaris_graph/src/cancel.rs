//! Cooperative cancellation for a single graph run.
//!
//! One run carries at most one cancellation signal, split into two halves so
//! the authority to *raise* it and the ability to *notice* it are separate
//! capabilities:
//!
//! - [`CancellationSource`] is held by whoever drives the executor — a session
//!   manager, a task listening for `SIGINT`, a bespoke host — and can raise the
//!   signal.
//! - [`RunCancellation`] is a [`LocalResource`] inserted into the run's
//!   context. Systems read it with [`Res<RunCancellation>`](polaris_system::param::Res)
//!   to notice the stop. It has no `cancel`, so a system cannot end the run it
//!   is inside.
//!
//! # How a run becomes cancellable
//!
//! Whoever owns the run inserts the observer half into the run's root context
//! before execution starts:
//!
//! ```
//! # use polaris_graph::cancel::CancellationSource;
//! # use polaris_system::param::SystemContext;
//! let source = CancellationSource::new();
//! let mut ctx = SystemContext::new();
//! ctx.insert(source.token());
//! // `source.cancel()` from any task now stops the run at its next node
//! // boundary.
//! ```
//!
//! A caller that inserts nothing gets today's behaviour: the run is not
//! cancellable. The `polaris_sessions` turn driver inserts one on every turn;
//! see the [sessions reference documentation] for its host-facing interruption
//! contract.
//!
//! [sessions reference documentation]: https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/sessions.md#interrupting-a-turn
//!
//! # What the executor guarantees
//!
//! The executor snapshots the observer once at run start and checks it at the
//! node boundary — before starting any node, and before letting a
//! node-attributable error propagate. It **never drops a running future**,
//! including a parallel sibling's. A raised signal surfaces as
//! [`ExecutionError::Interrupted`](crate::ExecutionError::Interrupted) on the
//! normal failure path, so `OnGraphFailure` fires and no error or timeout
//! handler is entered.
//!
//! A run that reaches its terminal node **completes**, even if the signal was
//! raised while the last node ran: there is nothing left to decline to start.
//! A terminal parallel node completes the same way unless one of its branches
//! declined a node.
//!
//! # Granularity is the system's choice
//!
//! A system that never reads the token runs to completion and the run stops at
//! the next boundary — so interrupt latency is bounded by the slowest node
//! that ignores the signal. A system that wants to return promptly races its
//! long await against the signal:
//!
//! ```
//! # use polaris_graph::cancel::RunCancellation;
//! # use polaris_system::param::Res;
//! # use polaris_system::system;
//! # use polaris_system::system::SystemError;
//! # async fn call_provider() -> String { String::new() }
//! #[system]
//! async fn act(cancel: Res<RunCancellation>) -> Result<String, SystemError> {
//!     let Some(response) = cancel.run_until_cancelled(call_provider()).await else {
//!         return Err(SystemError::ExecutionError("interrupted".into()));
//!     };
//!     Ok(response)
//! }
//! ```
//!
//! Neither choice corrupts framework state: the executor stores exactly what a
//! system returns. The returned `Err` above is diagnostic only — the boundary
//! check reports `Interrupted` regardless of the message, and the system's
//! error edge is not followed.

use polaris_system::resource::LocalResource;
use std::fmt;
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// The authority to interrupt one run.
///
/// Created by whoever drives the executor. Cloneable, so a signal-listener task
/// and a UI callback can share one source; every clone raises the same signal.
///
/// Hand the observer half out with [`token`](Self::token) and insert it into
/// the run's context — see the [module docs](self) for the full shape.
///
/// # Examples
///
/// ```
/// use polaris_graph::cancel::CancellationSource;
///
/// let source = CancellationSource::new();
/// assert!(!source.is_cancelled());
///
/// let observer = source.token();
/// source.cancel();
///
/// assert!(source.is_cancelled());
/// assert!(observer.is_cancelled());
/// ```
#[derive(Clone, Default)]
pub struct CancellationSource(CancellationToken);

impl CancellationSource {
    /// Creates a fresh, unraised source.
    #[must_use]
    pub fn new() -> Self {
        Self(CancellationToken::new())
    }

    /// Returns the observer half to insert into the run's context.
    ///
    /// Every token returned by the same source observes the same signal.
    #[must_use]
    pub fn token(&self) -> RunCancellation {
        RunCancellation(self.0.clone())
    }

    /// Raises the signal.
    ///
    /// Latched and idempotent: a second call changes nothing, and a
    /// [`RunCancellation`] handed out before *or* after this call observes the
    /// raised state.
    ///
    /// Synchronous: it returns once the signal is latched and the tasks waiting
    /// on it are woken. It does not await, but it is not an atomic store — it
    /// takes a short internal lock, so a call can briefly wait for another
    /// thread that is using the same signal. That makes it fine to call from
    /// ordinary async tasks and callbacks, and **not** safe to call from a raw
    /// signal handler, which can interrupt a thread that holds the lock.
    /// Forward the signal to a task instead:
    ///
    /// ```
    /// # use polaris_graph::cancel::CancellationSource;
    /// # async fn ctrl_c() {} // stands in for `tokio::signal::ctrl_c()`
    /// # async fn example() {
    /// let source = CancellationSource::new();
    ///
    /// let on_signal = source.clone();
    /// tokio::spawn(async move {
    ///     ctrl_c().await;
    ///     on_signal.cancel();
    /// });
    /// # }
    /// ```
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Returns whether the signal has been raised.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl fmt::Debug for CancellationSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationSource")
            .field("cancelled", &self.0.is_cancelled())
            .finish()
    }
}

/// The ability to notice that the run it belongs to was interrupted.
///
/// A per-run [`LocalResource`]. A system that wants to stop promptly reads it
/// with [`Res<RunCancellation>`](polaris_system::param::Res) and races its long
/// await against the signal; see [`run_until_cancelled`](Self::run_until_cancelled).
/// There is no `cancel` here by design — a system cannot end its own run through
/// it.
///
/// # Scope + why
///
/// `Local`. The signal is per-run: two concurrent turns of the same agent must
/// be interruptible independently, which a global could not express.
///
/// # Provided by
///
/// Consumer-supplied. No plugin registers a factory for it. Workspace session
/// drivers insert one on every turn and expose a host-facing interruption
/// method; see the [sessions reference documentation](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/sessions.md#interrupting-a-turn).
/// A caller driving [`GraphExecutor`](crate::GraphExecutor) directly inserts
/// [`CancellationSource::token`] — or [`never`](Self::never) when the run is not
/// meant to be interruptible.
///
/// # Access pattern
///
/// [`Res<RunCancellation>`](polaris_system::param::Res) only. The value is
/// immutable: raising is [`CancellationSource`]'s capability, not this one's.
/// Observation is latched, so once the signal is raised every later read sees
/// it and every future returned here resolves immediately.
///
/// # Alternatives
///
/// | Alternative | When |
/// |---|---|
/// | [`CancellationSource`] | You need to *raise* the signal, not observe it. |
/// | No token at all | The run is not interruptible; the executor's check is then a `None` test per node. |
///
/// # Scope crossing
///
/// Resolved through the parent chain like any local, so a system inside a
/// [`ContextPolicy::new()`](crate::ContextPolicy) isolated scope cannot read
/// it unless the policy declares `.share::<RunCancellation>()` — the same rule
/// every caller-provided per-turn local follows. The *between-node* stop needs
/// none of this: the executor holds the token from the root and checks it
/// inside every boundary, so an isolated interior still stops at its next node.
///
/// # Signature cost
///
/// A [`Res<RunCancellation>`](polaris_system::param::Res) read appears in the
/// graph's derived [`GraphSignature`](crate::GraphSignature) `requires`. That
/// is the honest interface of a cancellable graph, like any other system
/// parameter read of a caller-provided local resource.
///
/// # Example system
///
/// ```
/// use polaris_graph::cancel::RunCancellation;
/// use polaris_system::param::Res;
/// use polaris_system::system;
///
/// # async fn poll_once() -> Option<String> { None }
/// #[system]
/// async fn drain(cancel: Res<RunCancellation>) -> usize {
///     let mut seen = 0;
///     while !cancel.is_cancelled() {
///         match poll_once().await {
///             Some(_) => seen += 1,
///             None => break,
///         }
///     }
///     seen
/// }
/// ```
#[derive(Clone)]
pub struct RunCancellation(CancellationToken);

impl LocalResource for RunCancellation {}

impl RunCancellation {
    /// Returns a signal that never fires.
    ///
    /// For callers that run a graph whose systems read
    /// [`Res<RunCancellation>`](polaris_system::param::Res) but who do not intend
    /// to interrupt it. Without it, those systems would fail to resolve the
    /// resource.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::cancel::RunCancellation;
    ///
    /// let never = RunCancellation::never();
    /// assert!(!never.is_cancelled());
    /// ```
    #[must_use]
    pub fn never() -> Self {
        Self(CancellationToken::new())
    }

    /// Returns whether the run has been interrupted.
    ///
    /// A synchronous read that takes a short internal lock; it is not lock-free
    /// and does not await. Cheap enough for a check between units of work
    /// inside one system.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves when the run is interrupted; immediately if it already was.
    ///
    /// Cancel-safe: dropping the returned future loses nothing, because the
    /// signal is latched.
    ///
    /// # Examples
    ///
    /// ```
    /// # use polaris_graph::cancel::CancellationSource;
    /// # async fn example() {
    /// let source = CancellationSource::new();
    /// let cancel = source.token();
    /// source.cancel();
    /// cancel.cancelled().await; // returns immediately
    /// # }
    /// ```
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        self.0.cancelled()
    }

    /// Consumes the token and returns a `'static` future that resolves when
    /// the run is interrupted.
    ///
    /// The owned form adapters need when they cannot borrow the token — for
    /// example `polaris_shell`'s `ShellCancel::from_fn`, which takes a
    /// `Future<Output = ()> + Send + 'static` factory. Clone the token first
    /// if the caller also needs to keep observing it.
    ///
    /// # Examples
    ///
    /// ```
    /// # use polaris_graph::cancel::CancellationSource;
    /// # async fn example() {
    /// let source = CancellationSource::new();
    /// let cancel = source.token();
    /// let factory = move || cancel.clone().cancelled_owned();
    /// source.cancel();
    /// factory().await; // returns immediately
    /// # }
    /// ```
    pub fn cancelled_owned(self) -> impl Future<Output = ()> + Send + 'static {
        self.0.cancelled_owned()
    }

    /// Runs `fut` until it completes or the run is interrupted, whichever is
    /// first.
    ///
    /// `Some(output)` means `fut` finished. `None` means the run was
    /// interrupted and `fut` was **dropped at its own await point** — the same
    /// cost `tokio::select!` has, accepted here by the system author who chose
    /// this form. Anything `fut` had already committed stays; anything after
    /// its current await does not run.
    ///
    /// # Examples
    ///
    /// ```
    /// # use polaris_graph::cancel::CancellationSource;
    /// # async fn example() {
    /// let source = CancellationSource::new();
    /// let cancel = source.token();
    ///
    /// let done = cancel.run_until_cancelled(async { 7 }).await;
    /// assert_eq!(done, Some(7));
    ///
    /// source.cancel();
    /// let stopped = cancel.run_until_cancelled(std::future::pending::<u8>()).await;
    /// assert_eq!(stopped, None);
    /// # }
    /// ```
    pub async fn run_until_cancelled<F: Future>(&self, fut: F) -> Option<F::Output> {
        self.0.run_until_cancelled(fut).await
    }
}

impl fmt::Debug for RunCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunCancellation")
            .field("cancelled", &self.0.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_token_share_one_latch() {
        let source = CancellationSource::new();
        let first = source.token();
        assert!(!source.is_cancelled());
        assert!(!first.is_cancelled());

        source.cancel();

        // A token handed out before the raise observes it...
        assert!(first.is_cancelled());
        // ...and so does one handed out after.
        assert!(source.token().is_cancelled());
        assert!(source.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let source = CancellationSource::new();
        source.cancel();
        source.cancel();
        assert!(source.is_cancelled());
    }

    #[test]
    fn a_clone_of_the_source_raises_the_same_signal() {
        let source = CancellationSource::new();
        let token = source.token();
        source.clone().cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn never_does_not_fire_and_is_independent() {
        let never = RunCancellation::never();
        assert!(!never.is_cancelled());
        // Two `never`s are distinct latches, not a shared global.
        let other = RunCancellation::never();
        assert!(!other.is_cancelled());
    }

    #[test]
    fn debug_renders_the_latch_state_without_naming_the_underlying_token() {
        let source = CancellationSource::new();
        let token = source.token();
        assert_eq!(
            format!("{source:?}"),
            "CancellationSource { cancelled: false }"
        );
        assert_eq!(format!("{token:?}"), "RunCancellation { cancelled: false }");

        source.cancel();
        assert_eq!(
            format!("{source:?}"),
            "CancellationSource { cancelled: true }"
        );
        assert_eq!(format!("{token:?}"), "RunCancellation { cancelled: true }");
    }

    #[tokio::test]
    async fn cancelled_resolves_after_a_raise_from_another_task() {
        let source = CancellationSource::new();
        let token = source.token();

        let raiser = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            source.cancel();
        });

        token.cancelled().await;
        assert!(token.is_cancelled());
        raiser.await.expect("raiser task panicked");
    }

    #[tokio::test]
    async fn cancelled_owned_resolves_immediately_when_already_raised() {
        let source = CancellationSource::new();
        let token = source.token();
        source.cancel();

        // The shell adapter's pre-spawn check depends on this: an owned
        // future built from an already-raised token must not park.
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            token.cancelled_owned(),
        )
        .await
        .expect("an already-raised token must resolve its owned future immediately");
    }

    #[tokio::test]
    async fn run_until_cancelled_reports_which_side_won() {
        let source = CancellationSource::new();
        let token = source.token();

        assert_eq!(token.run_until_cancelled(async { 7 }).await, Some(7));

        source.cancel();
        assert_eq!(
            token
                .run_until_cancelled(std::future::pending::<u8>())
                .await,
            None
        );
    }

    #[tokio::test]
    async fn dropping_a_cancelled_future_loses_nothing() {
        let source = CancellationSource::new();
        let token = source.token();

        // Build and drop a waiter before the raise; the latch is unaffected.
        drop(token.cancelled());
        assert!(!token.is_cancelled());

        source.cancel();
        // A fresh waiter still resolves.
        token.cancelled().await;
    }
}
