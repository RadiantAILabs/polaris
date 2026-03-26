//! Middleware registration and execution API.
//!
//! The [`MiddlewareAPI`] provides a registry for middleware handlers. See the
//! [module-level docs](super) for an overview of targets, layer ordering, and
//! usage.

use super::info::{
    DecisionInfo, GraphInfo, LoopInfo, LoopIterationInfo, ParallelBranchInfo, ParallelInfo,
    SwitchInfo, SystemInfo,
};
use crate::executor::ExecutionError;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use std::fmt;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// MiddlewareError
// ─────────────────────────────────────────────────────────────────────────────

/// Error returned by middleware handlers.
#[derive(Debug, Clone)]
pub enum MiddlewareError {
    /// Error raised by a the current middleware layer.
    ///
    /// The framework converts this into
    /// [`ExecutionError::MiddlewareError`] with the registered middleware
    /// name attached automatically.
    Layer(String),
    /// An error from deeper in the middleware chain.
    ///
    /// Propagated unchanged, preserving the original [`ExecutionError`]
    /// variant for downstream control-flow routing.
    Inner(ExecutionError),
}

impl fmt::Display for MiddlewareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MiddlewareError::Layer(msg) => write!(f, "middleware error: {msg}"),
            MiddlewareError::Inner(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for MiddlewareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MiddlewareError::Layer(_) => None,
            MiddlewareError::Inner(err) => Some(err),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MiddlewareHandler
// ─────────────────────────────────────────────────────────────────────────────

/// Bound for middleware handler closures.
///
/// Blanket-implemented for any `Fn` matching the handler signature, so
/// closures passed to the `register_*` methods on [`MiddlewareAPI`] satisfy
/// this automatically. See the [module-level docs](super) for further details.
///
/// `I` is the info type for the middleware target (e.g. [`SystemInfo`], [`LoopInfo`]).
///
/// The handler is higher-ranked over two lifetimes:
/// - `'a` — how long the `&mut SystemContext` is borrowed and the returned
///   future lives.
/// - `'p` — how long the resources that `SystemContext` borrows from live
///   (e.g. the server's resource storage).
pub trait MiddlewareHandler<I>:
    for<'a, 'p> Fn(
        I,
        &'a mut SystemContext<'p>,
        Next<'a, I>,
    ) -> BoxFuture<'a, Result<(), MiddlewareError>>
    + Send
    + Sync
    + 'static
{
}

impl<I, F> MiddlewareHandler<I> for F where
    F: for<'a, 'p> Fn(
            I,
            &'a mut SystemContext<'p>,
            Next<'a, I>,
        ) -> BoxFuture<'a, Result<(), MiddlewareError>>
        + Send
        + Sync
        + 'static
{
}

// ─────────────────────────────────────────────────────────────────────────────
// BoxedMiddleware
// ─────────────────────────────────────────────────────────────────────────────

/// Type-erased middleware handler.
struct BoxedMiddleware<I> {
    /// Human-readable name for debugging and diagnostics.
    name: String,
    /// The middleware handler function.
    handler: Arc<dyn MiddlewareHandler<I>>,
}

// Manual Clone: `I` is only a type parameter of the handler function
// signature, not a stored value, there is no need for `I: Clone`.
impl<I> Clone for BoxedMiddleware<I> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            handler: Arc::clone(&self.handler),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TerminalFn
// ─────────────────────────────────────────────────────────────────────────────

/// Bound for the innermost function at the end of a middleware chain.
///
/// Uses `for<'p>` to accept any [`SystemContext`] parent lifetime, while
/// the reference lifetime and return future lifetime are tied to `'a`.
pub(crate) trait TerminalFn<'a, T>:
    for<'p> Fn(&'a mut SystemContext<'p>) -> BoxFuture<'a, Result<T, ExecutionError>> + Send + Sync + 'a
{
}

impl<'a, T, F> TerminalFn<'a, T> for F where
    F: for<'p> Fn(&'a mut SystemContext<'p>) -> BoxFuture<'a, Result<T, ExecutionError>>
        + Send
        + Sync
        + 'a
{
}

// ─────────────────────────────────────────────────────────────────────────────
// Next
// ─────────────────────────────────────────────────────────────────────────────

/// Represents the remaining middleware chain plus the terminal function.
///
/// Each middleware handler receives a `Next` and must call [`Next::run`] to
/// continue the chain. This enables wrapping behavior (before/after logic,
/// timing, span activation, etc.).
///
/// # Example
///
/// ```
/// # use polaris_graph::middleware::{MiddlewareAPI, info::SystemInfo};
/// # let mw = MiddlewareAPI::new();
/// mw.register_system("timer", |info: SystemInfo, ctx, next| {
///     Box::pin(async move {
///         let start = std::time::Instant::now();
///         let result = next.run(ctx).await;
///         tracing::info!("{}: {:?}", info.node_name, start.elapsed());
///         result
///     })
/// });
/// ```
pub struct Next<'a, I> {
    /// Snapshot of the middleware chain.
    chain: Arc<Vec<BoxedMiddleware<I>>>,
    /// Index of the next middleware to execute, counting down from `chain.len()`.
    /// When 0, the terminal is called.
    index: usize,
    /// The innermost function to call when no middleware remains.
    terminal: Box<dyn TerminalFn<'a, ()>>,
    /// Metadata for this middleware carried through the chain.
    info: I,
}

impl<'a, I: Clone + Send + Sync + 'static> Next<'a, I> {
    /// Continues the middleware chain, or calls the terminal if no middleware
    /// remains.
    ///
    /// See the [module-level docs](super) for a usage example.
    pub fn run(self, ctx: &'a mut SystemContext<'_>) -> BoxFuture<'a, Result<(), MiddlewareError>> {
        if self.index == 0 {
            let terminal = self.terminal;
            Box::pin(async move { (terminal)(ctx).await.map_err(MiddlewareError::Inner) })
        } else {
            // Walk backward: last registered (highest index) is outermost.
            let mw = &self.chain[self.index - 1];
            let handler = Arc::clone(&mw.handler);
            let name = mw.name.clone();
            let handler_info = self.info.clone();
            let next = Next {
                chain: self.chain,
                index: self.index - 1,
                terminal: self.terminal,
                info: self.info,
            };
            Box::pin(async move {
                match (*handler)(handler_info, ctx, next).await {
                    Ok(()) => Ok(()),
                    Err(MiddlewareError::Inner(err)) => Err(MiddlewareError::Inner(err)),
                    Err(MiddlewareError::Layer(message)) => {
                        Err(MiddlewareError::Inner(ExecutionError::MiddlewareError {
                            middleware: name,
                            message,
                        }))
                    }
                }
            })
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Chain
// ─────────────────────────────────────────────────────────────────────────────

/// Middleware chain for a single target.
///
/// Each chain has its own [`RwLock`], so registration on one target never
/// blocks reads or writes on another.
pub(crate) struct Chain<I> {
    middlewares: RwLock<Arc<Vec<BoxedMiddleware<I>>>>,
}

impl<I> Default for Chain<I> {
    fn default() -> Self {
        Self {
            middlewares: RwLock::new(Arc::new(Vec::new())),
        }
    }
}

impl<I: Clone + Send + Sync + 'static> Chain<I> {
    fn push(&self, name: impl Into<String>, handler: impl MiddlewareHandler<I>) {
        let mut guard = self.middlewares.write();
        Arc::make_mut(&mut guard).push(BoxedMiddleware {
            name: name.into(),
            handler: Arc::new(handler),
        });
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.middlewares.read().len()
    }

    /// Executes the middleware chain.
    ///
    /// The return type `T` of the terminal is captured internally. Middleware handlers
    /// only see `()`, preventing them tampering with the result.
    pub(crate) fn execute<'a, T: Send + 'a>(
        &'a self,
        info: I,
        ctx: &'a mut SystemContext<'_>,
        terminal: impl TerminalFn<'a, T>,
    ) -> BoxFuture<'a, Result<T, ExecutionError>> {
        let chain = Arc::clone(&self.middlewares.read());

        if chain.is_empty() {
            terminal(ctx)
        } else {
            let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
            let slot_write = Arc::clone(&slot);

            let wrapper =
                move |ctx: &'a mut SystemContext<'_>| -> BoxFuture<'a, Result<(), ExecutionError>> {
                    let fut = terminal(ctx);
                    let slot_write = Arc::clone(&slot_write);
                    Box::pin(async move {
                        let some_future = Some(fut.await?);
                        *slot_write.lock() = some_future;
                        Ok(())
                    })
                };

            let len = chain.len();
            let next = Next {
                chain,
                index: len,
                terminal: Box::new(wrapper),
                info,
            };

            Box::pin(async move {
                next.run(ctx).await.map_err(|mw_err| match mw_err {
                    MiddlewareError::Inner(err) => err,
                    // Layer variant should never match here as Next::run converts it
                    // to Inner before propagation.
                    MiddlewareError::Layer(message) => ExecutionError::InternalError(format!(
                        "unattributed middleware error: {message}"
                    )),
                })?;
                slot.lock().take().ok_or_else(|| {
                    ExecutionError::InternalError(
                        "middleware terminal did not produce a result".into(),
                    )
                })
            })
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MiddlewareAPI
// ─────────────────────────────────────────────────────────────────────────────

/// Registry for middleware chains.
///
/// See the [module-level docs](super) for an overview of targets and layer
/// ordering.
#[derive(Default)]
pub struct MiddlewareAPI {
    pub(crate) graph_execution: Chain<GraphInfo>,
    pub(crate) system: Chain<SystemInfo>,
    pub(crate) loop_node: Chain<LoopInfo>,
    pub(crate) parallel_node: Chain<ParallelInfo>,
    pub(crate) decision: Chain<DecisionInfo>,
    pub(crate) switch: Chain<SwitchInfo>,
    pub(crate) loop_iteration: Chain<LoopIterationInfo>,
    pub(crate) parallel_branch: Chain<ParallelBranchInfo>,
}

impl API for MiddlewareAPI {}

impl MiddlewareAPI {
    /// Creates a new empty middleware registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a middleware handler for the entire graph execution.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_graph_execution(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<GraphInfo>,
    ) -> &Self {
        self.graph_execution.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`System`](super::System) nodes.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::middleware::{MiddlewareAPI, info::SystemInfo};
    /// let mw = MiddlewareAPI::new();
    /// mw.register_system("timer", |info: SystemInfo, ctx, next| {
    ///     Box::pin(async move {
    ///         let start = std::time::Instant::now();
    ///         let result = next.run(ctx).await;
    ///         tracing::info!("{}: {:?}", info.node_name, start.elapsed());
    ///         result
    ///     })
    /// });
    /// ```
    pub fn register_system(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<SystemInfo>,
    ) -> &Self {
        self.system.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`Loop`](super::Loop) nodes.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_loop(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<LoopInfo>,
    ) -> &Self {
        self.loop_node.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`Parallel`](super::Parallel) nodes.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_parallel(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<ParallelInfo>,
    ) -> &Self {
        self.parallel_node.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`Decision`](super::Decision) nodes.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_decision(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<DecisionInfo>,
    ) -> &Self {
        self.decision.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`Switch`](super::Switch) nodes.
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_switch(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<SwitchInfo>,
    ) -> &Self {
        self.switch.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`LoopIteration`](super::LoopIteration).
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_loop_iteration(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<LoopIterationInfo>,
    ) -> &Self {
        self.loop_iteration.push(name, handler);
        self
    }

    /// Registers a middleware handler for [`ParallelBranch`](super::ParallelBranch).
    ///
    /// See the [module-level docs](super) for handler contract and examples.
    pub fn register_parallel_branch(
        &self,
        name: impl Into<String>,
        handler: impl MiddlewareHandler<ParallelBranchInfo>,
    ) -> &Self {
        self.parallel_branch.push(name, handler);
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeId;
    use crate::middleware::info::SystemInfo;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Creates a mock [`SystemInfo`].
    fn mock_system_info() -> SystemInfo {
        SystemInfo {
            node_name: "test",
            node_id: NodeId::new(),
        }
    }

    #[test]
    fn register_and_count() {
        let api = MiddlewareAPI::new();
        assert_eq!(api.system.len(), 0, "initial count should be zero");

        api.register_system("first", |_info, _ctx, next| {
            Box::pin(async move { next.run(_ctx).await })
        });

        assert_eq!(
            api.system.len(),
            1,
            "count should be 1 after first register"
        );

        api.register_system("second", |_info, _ctx, next| {
            Box::pin(async move { next.run(_ctx).await })
        });

        assert_eq!(
            api.system.len(),
            2,
            "count should be 2 after second register"
        );
    }

    #[tokio::test]
    async fn execute_no_middleware_calls_terminal() {
        let api = MiddlewareAPI::new();
        let called = Arc::new(AtomicUsize::new(0));
        let called_clone = Arc::clone(&called);

        let terminal =
            move |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
                called_clone.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, &terminal)
            .await;

        assert!(result.is_ok());
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execute_single_middleware() {
        let api = MiddlewareAPI::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_mw = Arc::clone(&order);
        let order_term = Arc::clone(&order);

        api.register_system("wrapper", move |_info, ctx, next| {
            let order = Arc::clone(&order_mw);
            Box::pin(async move {
                order.lock().push("before");
                let result = next.run(ctx).await;
                order.lock().push("after");
                result
            })
        });

        let terminal =
            move |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
                let order = Arc::clone(&order_term);
                Box::pin(async move {
                    order.lock().push("terminal");
                    Ok(())
                })
            };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, &terminal)
            .await;

        assert!(result.is_ok());
        let steps = order.lock();
        assert_eq!(*steps, vec!["before", "terminal", "after"]);
    }

    #[tokio::test]
    async fn execute_ordering() {
        let api = MiddlewareAPI::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Register A then B — B (last registered) should be outermost
        for label in ["A", "B"] {
            let order_clone = Arc::clone(&order);
            let label_owned = label.to_owned();
            api.register_system(format!("order_{label}"), move |_info, ctx, next| {
                let order = Arc::clone(&order_clone);
                let n = label_owned.clone();
                Box::pin(async move {
                    order.lock().push(format!("{n}:before"));
                    let result = next.run(ctx).await;
                    order.lock().push(format!("{n}:after"));
                    result
                })
            });
        }

        let order_term = Arc::clone(&order);
        let terminal =
            move |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
                let order = Arc::clone(&order_term);
                Box::pin(async move {
                    order.lock().push("terminal".to_owned());
                    Ok(())
                })
            };

        let mut ctx = SystemContext::new();
        api.system
            .execute(mock_system_info(), &mut ctx, &terminal)
            .await
            .unwrap();

        let steps = order.lock();
        assert_eq!(
            *steps,
            vec!["B:before", "A:before", "terminal", "A:after", "B:after"],
            "last registered should be outermost"
        );
    }

    #[tokio::test]
    async fn invoke_passes_typed_info() {
        let api = MiddlewareAPI::new();
        let captured_name = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&captured_name);

        api.register_system("capture_info", move |info, _ctx, next| {
            let captured = Arc::clone(&captured);
            Box::pin(async move {
                *captured.lock() = info.node_name.to_string();
                next.run(_ctx).await
            })
        });

        let terminal = |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
            Box::pin(async { Ok(()) })
        };

        let mut ctx = SystemContext::new();
        let info = SystemInfo {
            node_name: "my_system",
            node_id: NodeId::new(),
        };

        api.system.execute(info, &mut ctx, &terminal).await.unwrap();

        assert_eq!(
            *captured_name.lock(),
            "my_system",
            "handler should receive the info passed to invoke"
        );
    }

    #[test]
    fn register_chaining() {
        let api = MiddlewareAPI::new();

        api.register_system("first", |_info, _ctx, next| {
            Box::pin(async move { next.run(_ctx).await })
        })
        .register_system("second", |_info, _ctx, next| {
            Box::pin(async move { next.run(_ctx).await })
        });

        assert_eq!(api.system.len(), 2);
    }

    #[tokio::test]
    async fn skipping_next_run_returns_error() {
        let api = MiddlewareAPI::new();

        api.register_system("short_circuiting_middleware", move |_info, _ctx, _next| {
            Box::pin(async move { Ok(()) })
        });

        let terminal =
            move |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
                Box::pin(async move { Ok(()) })
            };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, &terminal)
            .await;

        assert!(
            matches!(
                result,
                Err(ExecutionError::InternalError(message))
                    if message == "middleware terminal did not produce a result"
            ),
            "middleware that skips next.run() should produce an internal terminal-missing error"
        );
    }

    #[tokio::test]
    async fn error_propagates_through_chain() {
        let api = MiddlewareAPI::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        let order_mw = Arc::clone(&order);
        api.register_system("error_observer", move |_info, ctx, next| {
            let order = Arc::clone(&order_mw);
            Box::pin(async move {
                order.lock().push("before");
                let result = next.run(ctx).await;
                order.lock().push("after");
                result
            })
        });

        let terminal = |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
            Box::pin(async { Err(ExecutionError::SystemError("test error".into())) })
        };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, &terminal)
            .await;

        assert!(
            result.is_err(),
            "terminal error should propagate through the chain"
        );
        let steps = order.lock();
        assert_eq!(
            *steps,
            vec!["before", "after"],
            "outer middleware should still execute its after-logic on error"
        );
    }

    #[tokio::test]
    async fn inner_error_preserves_variant() {
        let api = MiddlewareAPI::new();

        api.register_system("passthrough", |_info, ctx, next| {
            Box::pin(async move { next.run(ctx).await })
        });

        let terminal = |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
            Box::pin(async { Err(ExecutionError::SystemError("inner failure".into())) })
        };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, terminal)
            .await;

        assert!(
            matches!(
                result,
                Err(ExecutionError::SystemError(ref msg)) if msg == "inner failure"
            ),
            "inner error should retain its original variant and message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn layer_error_is_attributed() {
        let api = MiddlewareAPI::new();

        api.register_system("failing_middleware", |_info, _ctx, _next| {
            Box::pin(async move { Err(MiddlewareError::Layer("middleware error".into())) })
        });

        let terminal = |_ctx: &mut SystemContext<'_>| -> BoxFuture<'_, Result<(), ExecutionError>> {
            Box::pin(async { Ok(()) })
        };

        let mut ctx = SystemContext::new();
        let result = api
            .system
            .execute(mock_system_info(), &mut ctx, terminal)
            .await;

        assert!(
            matches!(
                result,
                Err(ExecutionError::MiddlewareError {
                    ref middleware,
                    ref message,
                }) if middleware == "failing_middleware"
                    && message == "middleware error"
            ),
            "Layer error should carry name and message, got: {result:?}"
        );
    }
}
