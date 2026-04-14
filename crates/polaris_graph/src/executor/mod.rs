//! Graph execution engine.
//!
//! The [`GraphExecutor`] traverses and executes graphs, handling all node types
//! including sequential execution, conditional branching, loops, and parallel execution.
//!
//! # Example
//!
//! ```
//! # async fn example_fn() -> Result<(), Box<dyn std::error::Error>> {
//! use polaris_graph::{Graph, GraphExecutor};
//! use polaris_system::param::SystemContext;
//!
//! async fn reason() -> i32 { 1 }
//! async fn act() -> i32 { 2 }
//!
//! let mut graph = Graph::new();
//! graph.add_system(reason).add_system(act);
//!
//! let mut ctx = SystemContext::new();
//! let executor = GraphExecutor::new();
//! let result = executor.execute(&graph, &mut ctx, None, None).await?;
//!
//! # Ok(())
//! # }
//! ```

mod error;
mod run;

pub use error::{CaughtError, ErrorKind, ExecutionError, ResourceValidationError};
pub use run::DEFAULT_SWITCH_CASE;

use crate::edge::Edge;
use crate::graph::Graph;
use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{OnGraphComplete, OnGraphFailure, OnGraphStart, OnSystemStart};
use crate::middleware::{self, MiddlewareAPI};
use crate::node::{ContextMode, Node, NodeId};
use hashbrown::HashSet;
use polaris_system::param::{AccessMode, SystemContext};
use polaris_system::plugin::{Schedule, ScheduleId};
use std::any::{Any, TypeId};
use std::time::Duration;

/// Result of executing a graph.
///
/// Contains execution statistics and optionally the typed output from
/// the last system that produced a value. Use [`output`](Self::output)
/// to downcast the output to a concrete type.
///
/// # Examples
///
/// ```no_run
/// # async fn example_fn() -> Result<(), Box<dyn std::error::Error>> {
/// use polaris_graph::{Graph, GraphExecutor};
/// use polaris_system::param::SystemContext;
///
/// async fn step() -> i32 { 1 }
///
/// let mut graph = Graph::new();
/// graph.add_system(step);
///
/// let mut ctx = SystemContext::new();
/// let executor = GraphExecutor::new();
/// let result = executor.execute(&graph, &mut ctx, None, None).await?;
///
/// assert!(result.nodes_executed > 0);
/// assert!(!result.duration.is_zero());
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct ExecutionResult {
    /// Number of nodes executed during traversal.
    pub nodes_executed: usize,
    /// Total execution duration.
    pub duration: Duration,
    /// The output value from the last system that produced one.
    ///
    /// This is the same value stored via `ctx.insert_output_boxed()` during
    /// execution. Use [`output`](Self::output) to downcast to a concrete type.
    final_output: Option<Box<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for ExecutionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionResult")
            .field("nodes_executed", &self.nodes_executed)
            .field("duration", &self.duration)
            .field(
                "final_output",
                if self.final_output.is_some() {
                    &"Some(<output>)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}

impl ExecutionResult {
    /// Attempts to extract the typed output from graph execution.
    ///
    /// Returns `Some(&T)` if the last system produced a value of type `T`,
    /// or `None` if no output was produced or the type doesn't match.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use polaris_graph::{Graph, GraphExecutor};
    /// use polaris_system::param::SystemContext;
    ///
    /// async fn compute() -> i32 { 42 }
    ///
    /// # async fn example() {
    /// let mut graph = Graph::new();
    /// graph.add_system(compute);
    ///
    /// let mut ctx = SystemContext::new();
    /// let executor = GraphExecutor::new();
    /// let result = executor.execute(&graph, &mut ctx, None, None).await.unwrap();
    ///
    /// assert_eq!(result.output::<i32>(), Some(&42));
    /// # }
    /// ```
    #[must_use]
    pub fn output<T: 'static>(&self) -> Option<&T> {
        self.final_output.as_ref()?.downcast_ref::<T>()
    }

    /// Returns `true` if the result contains an output value.
    #[must_use]
    pub fn has_output(&self) -> bool {
        self.final_output.is_some()
    }
}

/// Graph execution engine.
///
/// `GraphExecutor` traverses a graph starting from its entry point,
/// executing systems and following control flow edges.
///
/// # Examples
///
/// ```
/// use polaris_graph::GraphExecutor;
///
/// // Default executor with 1000-iteration safety limit
/// let executor = GraphExecutor::new();
///
/// // Customize limits
/// let executor = GraphExecutor::new()
///     .with_default_max_iterations(500)
///     .with_max_recursion_depth(32);
///
/// // No iteration limit (use with caution)
/// let unlimited = GraphExecutor::without_iteration_limit();
/// ```
#[derive(Debug, Clone)]
pub struct GraphExecutor {
    /// Maximum iterations for loops without explicit limits (safety default).
    pub(crate) default_max_iterations: Option<usize>,
    /// Maximum recursion depth for nested control flow (safety default).
    pub(crate) max_recursion_depth: usize,
    /// Maximum total execution duration for the graph.
    pub(crate) max_duration: Option<Duration>,
}

impl Default for GraphExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphExecutor {
    /// Default maximum recursion depth for nested control flow.
    const DEFAULT_MAX_RECURSION_DEPTH: usize = 64;

    /// Creates a new graph executor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            default_max_iterations: Some(1000),
            max_recursion_depth: Self::DEFAULT_MAX_RECURSION_DEPTH,
            max_duration: None,
        }
    }

    /// Creates a new executor with no default iteration limit.
    ///
    /// # Warning
    ///
    /// This can lead to infinite loops if graphs contain loops
    /// without termination predicates or explicit `max_iterations`.
    #[must_use]
    pub fn without_iteration_limit() -> Self {
        Self {
            default_max_iterations: None,
            max_recursion_depth: Self::DEFAULT_MAX_RECURSION_DEPTH,
            max_duration: None,
        }
    }

    /// Sets the default maximum iterations for loops without explicit limits.
    #[must_use]
    pub fn with_default_max_iterations(mut self, max: usize) -> Self {
        self.default_max_iterations = Some(max);
        self
    }

    /// Sets the maximum recursion depth for nested control flow.
    #[must_use]
    pub fn with_max_recursion_depth(mut self, max: usize) -> Self {
        self.max_recursion_depth = max;
        self
    }

    /// Sets the maximum total execution duration for the graph.
    ///
    /// When set, the executor wraps graph execution in a timeout.
    /// If exceeded, returns [`ExecutionError::GraphTimeout`] after
    /// invoking `OnGraphFailure` hooks.
    ///
    /// # Cancel safety
    ///
    /// When the timeout fires, the in-flight future is dropped. Systems
    /// that hold mutable state across `.await` points may leave partial
    /// writes. Design systems to be cancel-safe or use error edges to
    /// handle cleanup when timeout is enabled.
    #[must_use]
    pub fn with_max_duration(mut self, duration: Duration) -> Self {
        self.max_duration = Some(duration);
        self
    }

    /// Validates that all resources required by systems in the graph
    /// are available in the context.
    ///
    /// This method performs eager validation before execution, catching
    /// missing resources early rather than failing during execution.
    ///
    /// # What is Validated
    ///
    /// - **Resources** (`Res<T>`, `ResMut<T>`): Checked against the context's
    ///   resources (local scope, parent chain, and globals).
    /// - **Hook-provided resources**: Resources provided by hooks on `OnGraphStart`
    ///   and `OnSystemStart` are considered available.
    /// - **Outputs** (`Out<T>`): Validated along sequential (linear) edges.
    ///   Each system's declared output dependencies are checked against the set
    ///   of outputs produced by preceding systems in the linear chain. Non-system
    ///   nodes (Decision, Switch, Loop, Parallel) contribute all output types
    ///   reachable from their subgraphs. Hook-provided types are also considered
    ///   available. Scope nodes are skipped (outputs flow differently across
    ///   scope boundaries). Conditional, switch, and parallel branches are not
    ///   individually validated because their execution is dynamic.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if all resources are available, or a vector of
    /// validation errors describing missing resources.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use polaris_graph::{Graph, GraphExecutor};
    /// use polaris_system::param::SystemContext;
    ///
    /// async fn my_system() -> i32 { 42 }
    ///
    /// let mut graph = Graph::new();
    /// graph.add_system(my_system);
    ///
    /// let ctx = SystemContext::new();
    /// let executor = GraphExecutor::new();
    ///
    /// // Validate before executing to catch missing resources early
    /// if let Err(errors) = executor.validate_resources(&graph, &ctx, None) {
    ///     for err in &errors {
    ///         tracing::error!("{err}");
    ///     }
    /// }
    /// ```
    pub fn validate_resources(
        &self,
        graph: &Graph,
        ctx: &SystemContext<'_>,
        hooks: Option<&HooksAPI>,
    ) -> Result<(), Vec<ResourceValidationError>> {
        let mut errors = Vec::new();

        let hook_provided: HashSet<TypeId> = hooks
            .map(|h| {
                let mut resources = HashSet::new();
                resources.extend(h.provided_resources_for(OnGraphStart::schedule_id()));
                resources.extend(h.provided_resources_for(OnSystemStart::schedule_id()));
                resources
            })
            .unwrap_or_default();

        self.validate_graph_resources(graph, ctx, &hook_provided, &mut errors, 0);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Recursively validates resource availability for all systems in a graph,
    /// including systems inside scope nodes.
    ///
    /// For scope nodes, the validation builds a synthetic child context that
    /// matches the runtime context the scoped graph will receive:
    /// - **Shared**: same context as parent (no boundary)
    /// - **Inherit**: child context with parent chain + forwarded resources
    /// - **Isolated**: fresh context with only globals + forwarded resources
    ///
    /// The `depth` parameter mirrors the execution depth limit to prevent
    /// unbounded recursion during validation of deeply nested scope graphs.
    fn validate_graph_resources(
        &self,
        graph: &Graph,
        ctx: &SystemContext<'_>,
        hook_provided: &HashSet<TypeId>,
        errors: &mut Vec<ResourceValidationError>,
        depth: usize,
    ) {
        if depth > self.max_recursion_depth {
            return;
        }

        for node in graph.nodes() {
            match node {
                Node::System(sys) => {
                    let access = sys.system.access();
                    self.validate_system_access(
                        &sys.id,
                        sys.system.name(),
                        &access,
                        ctx,
                        hook_provided,
                        errors,
                    );
                }
                Node::Scope(scope) => {
                    match scope.context_policy.mode {
                        ContextMode::Shared => {
                            // No boundary — same context
                            self.validate_graph_resources(
                                &scope.graph,
                                ctx,
                                hook_provided,
                                errors,
                                depth + 1,
                            );
                        }
                        ContextMode::Inherit => {
                            // Child context: reads walk parent chain, writes local only.
                            // Forwarded resources are cloned into the child.
                            let mut child = ctx.child();
                            for fwd in &scope.context_policy.forward_resources {
                                // Placeholder value: validation only checks TypeId
                                // presence via `contains_resource_by_type_id`, never
                                // downcasts the value. The actual clone happens at
                                // execution time in `forward_resources()`.
                                child.insert_boxed(fwd.type_id, Box::new(()));
                            }
                            self.validate_graph_resources(
                                &scope.graph,
                                &child,
                                hook_provided,
                                errors,
                                depth + 1,
                            );
                        }
                        ContextMode::Isolated => {
                            // Fresh context: no parent chain.
                            // Only forwarded resources + globals are available.
                            let mut child = match ctx.globals_arc() {
                                Some(globals) => SystemContext::with_globals(globals),
                                None => SystemContext::new(),
                            };
                            for fwd in &scope.context_policy.forward_resources {
                                // Placeholder value: see Inherit comment above.
                                child.insert_boxed(fwd.type_id, Box::new(()));
                            }
                            self.validate_graph_resources(
                                &scope.graph,
                                &child,
                                hook_provided,
                                errors,
                                depth + 1,
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        self.validate_output_reachability(graph, hook_provided, errors);
    }

    /// Validates that `Out<T>` parameters declared by systems have a matching
    /// output produced by a predecessor system along the linear (sequential) chain.
    ///
    /// Walks from the graph's entry node following only sequential edges, building
    /// a set of output `TypeId`s produced so far. For each system node, every
    /// declared output dependency (`access.outputs`) must appear in the produced
    /// set or the hook-provided set. Non-system nodes (Decision, Switch, Loop,
    /// Parallel) contribute all output types reachable from their subgraphs, since
    /// at least one execution path through those nodes might produce them. Scope
    /// nodes are skipped because outputs flow differently across scope boundaries.
    fn validate_output_reachability(
        &self,
        graph: &Graph,
        hook_provided: &HashSet<TypeId>,
        errors: &mut Vec<ResourceValidationError>,
    ) {
        let chain = self.build_linear_chain(graph);

        let mut produced_outputs: HashSet<TypeId> = HashSet::new();

        for node_id in &chain {
            let Some(node) = graph.get_node(node_id.clone()) else {
                continue;
            };

            match node {
                Node::System(sys) => {
                    let access = sys.system.access();
                    for out_access in &access.outputs {
                        if !produced_outputs.contains(&out_access.type_id)
                            && !hook_provided.contains(&out_access.type_id)
                        {
                            errors.push(ResourceValidationError::MissingOutput {
                                node: sys.id.clone(),
                                system_name: sys.system.name(),
                                output_type: out_access.type_name,
                                type_id: out_access.type_id,
                            });
                        }
                    }
                    produced_outputs.insert(sys.system.output_type_id());
                }
                Node::Decision(dec) => {
                    for branch in [&dec.true_branch, &dec.false_branch].into_iter().flatten() {
                        for (type_id, _) in graph.collect_branch_output_types(branch) {
                            produced_outputs.insert(type_id);
                        }
                    }
                }
                Node::Switch(sw) => {
                    for (_, target) in &sw.cases {
                        for (type_id, _) in graph.collect_branch_output_types(target) {
                            produced_outputs.insert(type_id);
                        }
                    }
                    if let Some(default) = &sw.default {
                        for (type_id, _) in graph.collect_branch_output_types(default) {
                            produced_outputs.insert(type_id);
                        }
                    }
                }
                Node::Loop(lp) => {
                    if let Some(body) = &lp.body_entry {
                        for (type_id, _) in graph.collect_branch_output_types(body) {
                            produced_outputs.insert(type_id);
                        }
                    }
                }
                Node::Parallel(par) => {
                    for branch in &par.branches {
                        for (type_id, _) in graph.collect_branch_output_types(branch) {
                            produced_outputs.insert(type_id);
                        }
                    }
                }
                Node::Scope(_) => {}
            }
        }
    }

    /// Builds the linear chain of node IDs by following sequential edges from
    /// the graph's entry point.
    fn build_linear_chain(&self, graph: &Graph) -> Vec<NodeId> {
        let mut chain = Vec::new();
        let Some(entry) = graph.entry() else {
            return chain;
        };

        let seq_edges: hashbrown::HashMap<NodeId, NodeId> = graph
            .edges()
            .iter()
            .filter_map(|edge| {
                if let Edge::Sequential(seq) = edge {
                    Some((seq.from.clone(), seq.to.clone()))
                } else {
                    None
                }
            })
            .collect();

        let mut current = Some(entry);
        let mut visited = HashSet::new();
        while let Some(node_id) = current {
            if !visited.insert(node_id.clone()) {
                break;
            }
            chain.push(node_id.clone());
            current = seq_edges.get(&node_id).cloned();
        }

        chain
    }

    /// Validates a single system's access requirements against the context.
    fn validate_system_access(
        &self,
        node_id: &NodeId,
        system_name: &'static str,
        access: &polaris_system::param::SystemAccess,
        ctx: &SystemContext<'_>,
        hook_provided: &HashSet<TypeId>,
        errors: &mut Vec<ResourceValidationError>,
    ) {
        for res_access in &access.resources {
            if hook_provided.contains(&res_access.type_id) {
                continue;
            }

            let exists = match res_access.mode {
                AccessMode::Read => ctx.contains_resource_by_type_id(res_access.type_id),
                AccessMode::Write => ctx.contains_local_resource_by_type_id(res_access.type_id),
            };

            if !exists {
                errors.push(ResourceValidationError::MissingResource {
                    node: node_id.clone(),
                    system_name,
                    resource_type: res_access.type_name,
                    type_id: res_access.type_id,
                    access_mode: res_access.mode,
                });
            }
        }
    }

    /// Executes a graph starting from its entry point.
    ///
    /// System outputs are stored in the context after each system executes,
    /// making them available to subsequent systems via `Out<T>` parameters
    /// and predicates.
    ///
    /// # Hooks
    ///
    /// If `hooks` is provided, lifecycle hooks are invoked at key execution points:
    /// - `OnGraphStart` / `OnGraphComplete` / `OnGraphFailure` - Graph-level events
    /// - `OnSystemStart` / `OnSystemComplete` / `OnSystemError` - System events
    /// - `OnDecisionStart` / `OnDecisionComplete` - Decision node events
    /// - `OnSwitchStart` / `OnSwitchComplete` - Switch node events
    /// - `OnLoopStart` / `OnLoopEnd` - Loop iteration events
    /// - `OnParallelStart` / `OnParallelComplete` - Parallel execution events
    ///
    /// For more, see the [`hooks` module](crate::hooks).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The graph has no entry point
    /// - A referenced node is not found
    /// - A system execution fails
    /// - A predicate evaluation fails
    /// - A loop exceeds its maximum iterations
    pub async fn execute(
        &self,
        graph: &Graph,
        ctx: &mut SystemContext<'_>,
        hooks: Option<&HooksAPI>,
        middleware: Option<&MiddlewareAPI>,
    ) -> Result<ExecutionResult, ExecutionError> {
        let default_mw = MiddlewareAPI::default();
        let mw = middleware.unwrap_or(&default_mw);

        let entry = graph.entry().ok_or(ExecutionError::EmptyGraph)?;
        let node_count = graph.node_count();

        let middleware_info = middleware::info::GraphInfo { node_count };
        mw.inner
            .graph_execution
            .execute(middleware_info, ctx, |ctx| {
                let entry = entry.clone();
                Box::pin(self.execute_graph_body(graph, ctx, entry, node_count, hooks, mw))
            })
            .await
    }

    /// Executes the graph body: invokes lifecycle hooks and runs from the entry point.
    async fn execute_graph_body(
        &self,
        graph: &Graph,
        ctx: &mut SystemContext<'_>,
        entry: NodeId,
        node_count: usize,
        hooks: Option<&HooksAPI>,
        middleware: &MiddlewareAPI,
    ) -> Result<ExecutionResult, ExecutionError> {
        let start = std::time::Instant::now();

        let node_map: Vec<_> = graph
            .nodes()
            .iter()
            .map(|node| (node.id(), node.name()))
            .collect();

        Self::invoke_hook::<OnGraphStart>(
            hooks,
            ctx,
            &GraphEvent::GraphStart {
                node_count,
                node_map,
            },
        );

        let result = if let Some(max) = self.max_duration {
            match tokio::time::timeout(
                max,
                self.execute_from(graph, ctx, entry, 0, hooks, middleware),
            )
            .await
            {
                Ok(inner) => inner,
                Err(_timeout) => {
                    let elapsed = start.elapsed();
                    Err(ExecutionError::GraphTimeout { elapsed, max })
                }
            }
        } else {
            self.execute_from(graph, ctx, entry, 0, hooks, middleware)
                .await
        };

        let duration = start.elapsed();
        match result {
            Ok(nodes_executed) => {
                let final_output = ctx.outputs_mut().take_last();

                Self::invoke_hook::<OnGraphComplete>(
                    hooks,
                    ctx,
                    &GraphEvent::GraphComplete {
                        nodes_executed,
                        duration,
                    },
                );
                Ok(ExecutionResult {
                    nodes_executed,
                    duration,
                    final_output,
                })
            }
            Err(err) => {
                Self::invoke_hook::<OnGraphFailure>(
                    hooks,
                    ctx,
                    &GraphEvent::GraphFailure { error: err.clone() },
                );
                Err(err)
            }
        }
    }

    /// Helper to invoke a hook if the [`HooksAPI`] is present.
    ///
    /// Hooks receive mutable access to the context, enabling both observability
    /// and resource injection.
    pub(crate) fn invoke_hook<S: Schedule>(
        hooks: Option<&HooksAPI>,
        ctx: &mut SystemContext<'_>,
        event: &GraphEvent,
    ) {
        if let Some(api) = hooks {
            api.invoke(S::schedule_id(), ctx, event);
        }
    }

    /// Invokes a graph event on each custom schedule attached to a system node.
    pub(crate) fn invoke_custom_schedules(
        hooks: Option<&HooksAPI>,
        ctx: &mut SystemContext<'_>,
        schedules: &[ScheduleId],
        event: &GraphEvent,
    ) {
        if let Some(api) = hooks {
            for schedule in schedules {
                api.invoke(*schedule, ctx, event);
            }
        }
    }
}

/// Unit tests for [`GraphExecutor`] configuration and error types.
/// Execution tests are in `tests/integration.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_creation() {
        let executor = GraphExecutor::new();
        assert_eq!(executor.default_max_iterations, Some(1000));
        assert_eq!(executor.max_recursion_depth, 64);
        assert_eq!(executor.max_duration, None);
    }

    #[test]
    fn executor_without_limit() {
        let executor = GraphExecutor::without_iteration_limit();
        assert_eq!(executor.default_max_iterations, None);
        assert_eq!(executor.max_duration, None);
    }

    #[test]
    fn executor_with_custom_limit() {
        let executor = GraphExecutor::new().with_default_max_iterations(500);
        assert_eq!(executor.default_max_iterations, Some(500));
    }

    #[test]
    fn executor_with_custom_recursion_depth() {
        let executor = GraphExecutor::new().with_max_recursion_depth(128);
        assert_eq!(executor.max_recursion_depth, 128);
    }

    #[test]
    fn executor_with_max_duration() {
        let executor = GraphExecutor::new().with_max_duration(Duration::from_secs(30));
        assert_eq!(executor.max_duration, Some(Duration::from_secs(30)));
    }
}
