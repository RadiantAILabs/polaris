//! Shared test utilities for `polaris_graph` integration tests.
//!
//! This module provides common helpers, systems, and resources used across
//! multiple test files. Import via `mod test_utils;` in test files.

#![allow(
    dead_code,
    missing_docs,
    reason = "shared test utilities — not all items used in every test binary"
)]

use polaris_graph::dev::{DevToolsPlugin, SystemInfo};
use polaris_graph::graph::Graph;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::node::NodeId;
use polaris_graph::{CaughtError, ErrorKind};
use polaris_system::param::{ParamError, SystemContext};
use polaris_system::plugin::Plugin;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════════════════
// TEST SERVER SETUP
// ═══════════════════════════════════════════════════════════════════════════════

/// Creates a test server with `DevToolsPlugin` enabled.
pub fn create_test_server() -> Server {
    let mut server = Server::new();
    DevToolsPlugin::default().build(&mut server);
    server
}

/// Returns the `HooksAPI` from a server, if available.
pub fn get_hooks(server: &Server) -> Option<&HooksAPI> {
    server.api::<HooksAPI>()
}

// ═══════════════════════════════════════════════════════════════════════════════
// GRAPH BUILDER HELPERS
// ═══════════════════════════════════════════════════════════════════════════════

/// Wraps a closure in a Box for use in parallel/switch branches.
///
/// # Example
///
/// ```
/// # use polaris_graph::Graph;
/// # use polaris_graph_test_utils::branch;
/// # async fn system_a() -> i32 { 1 }
/// # async fn system_b() -> i32 { 2 }
/// # let mut graph = Graph::new();
/// graph.add_parallel("par", [
///     branch(|g| g.add_system(system_a)),
///     branch(|g| g.add_system(system_b)),
/// ]);
/// ```
pub fn branch<F>(f: F) -> Box<dyn FnOnce(&mut Graph)>
where
    F: FnOnce(&mut Graph) + 'static,
{
    Box::new(f)
}

// ═══════════════════════════════════════════════════════════════════════════════
// COMMON RESOURCES
// ═══════════════════════════════════════════════════════════════════════════════

/// Tracks whether a handler was invoked during execution.
#[derive(Clone, Default)]
pub struct HandlerLog {
    invoked: Arc<Mutex<bool>>,
}

impl LocalResource for HandlerLog {}

impl HandlerLog {
    /// Returns whether the handler was invoked.
    pub fn was_invoked(&self) -> bool {
        *self.invoked.lock().unwrap()
    }

    /// Marks the handler as invoked.
    pub fn mark_invoked(&self) {
        *self.invoked.lock().unwrap() = true;
    }
}

/// Execution log that records which `NodeId`s were executed in order.
///
/// Uses `Res<SystemInfo>` injected by `DevToolsPlugin` to verify the *correct* nodes ran.
#[derive(Clone, Default)]
pub struct ExecutionLog {
    executed: Arc<Mutex<Vec<NodeId>>>,
}

impl LocalResource for ExecutionLog {}

impl ExecutionLog {
    /// Records that a node was executed.
    pub fn record(&self, node_id: &NodeId) {
        self.executed.lock().unwrap().push(node_id.clone());
    }

    /// Returns all executed node IDs in execution order.
    pub fn executed(&self) -> Vec<NodeId> {
        self.executed.lock().unwrap().clone()
    }

    /// Count occurrences of a specific node.
    pub fn count(&self, node_id: &NodeId) -> usize {
        self.executed
            .lock()
            .unwrap()
            .iter()
            .filter(|id| *id == node_id)
            .count()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// COMMON SYSTEMS
// ═══════════════════════════════════════════════════════════════════════════════

/// System that always succeeds immediately.
pub struct SuccessSystem;

impl System for SuccessSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "success_system"
    }
}

/// System that always fails with an error.
pub struct FailingSystem;

impl System for FailingSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Err(SystemError::ExecutionError("intentional failure".into())) })
    }

    fn name(&self) -> &'static str {
        "failing_system"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

/// System that sleeps for a specified duration.
pub struct SlowSystem {
    pub duration: std::time::Duration,
}

impl System for SlowSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let duration = self.duration;
        Box::pin(async move {
            tokio::time::sleep(duration).await;
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "slow_system"
    }
}

/// System that marks a handler was invoked via `HandlerLog`.
pub struct HandlerSystem;

impl System for HandlerSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            if let Ok(log) = ctx.get_resource::<HandlerLog>() {
                log.mark_invoked();
            }
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "handler_system"
    }
}

/// System that sets a boolean flag when executed.
pub struct FlagSystem {
    pub flag: Arc<Mutex<bool>>,
}

impl System for FlagSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let flag = Arc::clone(&self.flag);
        Box::pin(async move {
            *flag.lock().unwrap() = true;
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "flag_system"
    }
}

/// System that logs its own `NodeId` when executed.
///
/// Uses `Res<SystemInfo>` injected by `DevToolsPlugin` and `Res<ExecutionLog>`.
pub struct LoggingSystem;

impl System for LoggingSystem {
    type Output = NodeId;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let info = ctx
                .get_resource::<SystemInfo>()
                .expect("SystemInfo not injected by DevToolsPlugin");
            let node_id = info.node_id();
            let log = ctx
                .get_resource::<ExecutionLog>()
                .expect("ExecutionLog resource not found");
            log.record(&node_id);
            Ok(node_id)
        })
    }

    fn name(&self) -> &'static str {
        "logging_system"
    }

    fn access(&self) -> polaris_system::param::SystemAccess {
        polaris_system::param::SystemAccess::new()
            .with_read::<SystemInfo>()
            .with_read::<ExecutionLog>()
    }
}

/// Adds a logging system to the graph that records its node ID when executed.
///
/// Returns the node ID assigned to this system.
pub fn add_tracker(g: &mut Graph) -> NodeId {
    g.add_boxed_system(Box::new(LoggingSystem))
}

// ═══════════════════════════════════════════════════════════════════════════════
// OUTPUT/PREDICATE TEST SYSTEMS
// ═══════════════════════════════════════════════════════════════════════════════

/// Output from producer system for chaining tests.
#[derive(Debug, Clone)]
pub struct ProducerOutput {
    /// The produced value.
    pub value: i32,
}

/// System that produces an output value.
pub struct ProducerSystem {
    pub value: i32,
}

impl System for ProducerSystem {
    type Output = ProducerOutput;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let value = self.value;
        Box::pin(async move { Ok(ProducerOutput { value }) })
    }

    fn name(&self) -> &'static str {
        "producer_system"
    }
}

/// System that reads and stores the producer output value.
pub struct ConsumerSystem {
    pub received: Arc<Mutex<Option<i32>>>,
}

impl System for ConsumerSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let received = Arc::clone(&self.received);
        Box::pin(async move {
            let output = ctx
                .get_output::<ProducerOutput>()
                .expect("ProducerOutput should be available");
            *received.lock().unwrap() = Some(output.value);
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "consumer_system"
    }
}

/// Marker type for decision predicate output.
#[derive(Debug)]
pub struct DecisionOutput {
    pub take_true: bool,
}

/// System that outputs a decision marker.
pub struct DecisionSystem {
    pub take_true: bool,
}

impl System for DecisionSystem {
    type Output = DecisionOutput;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let take_true = self.take_true;
        Box::pin(async move { Ok(DecisionOutput { take_true }) })
    }

    fn name(&self) -> &'static str {
        "decision_system"
    }
}

/// Output for switch discriminator tests.
#[derive(Debug)]
pub struct SwitchOutput {
    /// The switch key to select the branch.
    pub key: &'static str,
}

/// System that outputs a switch key.
pub struct SwitchKeySystem {
    /// The switch key to output.
    pub key: &'static str,
}

impl System for SwitchKeySystem {
    type Output = SwitchOutput;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let key = self.key;
        Box::pin(async move { Ok(SwitchOutput { key }) })
    }

    fn name(&self) -> &'static str {
        "switch_key_system"
    }
}

/// Loop state for termination predicate tests.
#[derive(Debug)]
pub struct LoopState {
    /// Current iteration count.
    pub iteration: usize,
}

/// System that tracks loop iteration count.
pub struct LoopIterationSystem {
    /// Shared counter for iterations.
    pub counter: Arc<Mutex<usize>>,
}

impl System for LoopIterationSystem {
    type Output = LoopState;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let counter = Arc::clone(&self.counter);
        Box::pin(async move {
            let mut count = counter.lock().unwrap();
            *count += 1;
            Ok(LoopState { iteration: *count })
        })
    }

    fn name(&self) -> &'static str {
        "loop_iteration_system"
    }
}

/// System that produces initial loop state (iteration 0).
pub struct InitialStateSystem;

impl System for InitialStateSystem {
    type Output = LoopState;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(LoopState { iteration: 0 }) })
    }

    fn name(&self) -> &'static str {
        "initial_state_system"
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ERROR KIND CHECKING SYSTEMS
// ═══════════════════════════════════════════════════════════════════════════════

/// System that fails with a `ParamError` (parameter resolution failure).
pub struct ParamFailingSystem;

impl System for ParamFailingSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            Err(SystemError::ParamError(ParamError::ResourceNotFound(
                "MissingType",
            )))
        })
    }

    fn name(&self) -> &'static str {
        "param_failing_system"
    }
}

/// System that records the `ErrorKind` from `CaughtError` via `HandlerLog`.
pub struct KindCheckingHandler;

impl System for KindCheckingHandler {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            if let Ok(log) = ctx.get_resource::<HandlerLog>() {
                log.mark_invoked();
            }
            if let Ok(caught) = ctx.get_output::<CaughtError>()
                && let Ok(kind_log) = ctx.get_resource::<ErrorKindLog>()
            {
                kind_log.record(caught.kind);
            }
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "kind_checking_handler"
    }
}

/// Resource that records the `ErrorKind` observed by an error handler.
#[derive(Clone, Default)]
pub struct ErrorKindLog {
    kind: Arc<Mutex<Option<ErrorKind>>>,
}

impl LocalResource for ErrorKindLog {}

impl ErrorKindLog {
    /// Returns the recorded error kind, if any.
    pub fn kind(&self) -> Option<ErrorKind> {
        *self.kind.lock().unwrap()
    }

    /// Records an error kind.
    pub fn record(&self, kind: ErrorKind) {
        *self.kind.lock().unwrap() = Some(kind);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// RETRY TEST SYSTEMS
// ═══════════════════════════════════════════════════════════════════════════════

/// A system that fails the first `fail_count` times, then succeeds.
///
/// Tracks attempt count via a shared `AtomicU32` so tests can verify
/// how many times the system was invoked.
pub struct EventuallySucceedsSystem {
    /// Number of times to fail before succeeding.
    pub fail_count: u32,
    /// Shared counter tracking total invocations.
    pub attempts: Arc<AtomicU32>,
}

impl System for EventuallySucceedsSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.fail_count {
                Err(SystemError::ExecutionError(format!(
                    "transient failure (attempt {attempt})"
                )))
            } else {
                Ok(())
            }
        })
    }

    fn name(&self) -> &'static str {
        "eventually_succeeds_system"
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TRACKER NODE COLLECTION
// ═══════════════════════════════════════════════════════════════════════════════

/// Collects tracker `NodeId`s during graph building.
#[derive(Clone, Default)]
pub struct TrackerNodes(Arc<Mutex<Vec<NodeId>>>);

impl TrackerNodes {
    /// Adds a node ID to the collection.
    pub fn add(&self, id: NodeId) {
        self.0.lock().unwrap().push(id);
    }

    /// Consumes self and returns the collected node IDs.
    pub fn into_vec(self) -> Vec<NodeId> {
        Arc::try_unwrap(self.0)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_else(|arc| arc.lock().unwrap().clone())
    }
}
