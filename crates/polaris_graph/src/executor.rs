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
//! let result = executor.execute(&graph, &mut ctx, None).await?;
//!
//! # Ok(())
//! # }
//! ```

use crate::edge::Edge;
use crate::graph::Graph;
use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{
    OnDecisionComplete, OnDecisionStart, OnGraphComplete, OnGraphFailure, OnGraphStart, OnLoopEnd,
    OnLoopIteration, OnLoopStart, OnParallelComplete, OnParallelStart, OnSwitchComplete,
    OnSwitchStart, OnSystemComplete, OnSystemError, OnSystemStart,
};
use crate::node::{LoopNode, Node, NodeId, ParallelNode, SwitchNode};
use crate::predicate::PredicateError;
use core::any::TypeId;
use core::fmt;
use core::time::Duration;
use hashbrown::HashSet;
use polaris_system::param::{AccessMode, SystemAccess, SystemContext};
use polaris_system::plugin::{Schedule, ScheduleId};

/// Default case name for switch nodes when no match is found.
pub const DEFAULT_SWITCH_CASE: &str = "default";

/// Result of executing a graph.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Number of nodes executed during traversal.
    pub nodes_executed: usize,
    /// Total execution duration.
    pub duration: Duration,
}

/// Errors that can occur during graph execution.
#[derive(Debug, Clone)]
pub enum ExecutionError {
    /// The graph has no entry point.
    EmptyGraph,
    /// A referenced node was not found in the graph.
    NodeNotFound(NodeId),
    /// No sequential edge found from the given node.
    NoNextNode(NodeId),
    /// A decision or loop node is missing its predicate.
    MissingPredicate(NodeId),
    /// A decision node is missing a branch target.
    MissingBranch {
        /// The node ID of the decision node.
        node: NodeId,
        /// Which branch is missing ("true" or "false").
        branch: &'static str,
    },
    /// A parallel node is missing its join target.
    MissingJoin(NodeId),
    /// A system execution error occurred.
    SystemError(String),
    /// A predicate evaluation error occurred.
    PredicateError(PredicateError),
    /// Maximum iterations exceeded in a loop.
    MaxIterationsExceeded {
        /// The loop node that exceeded iterations.
        node: NodeId,
        /// The maximum allowed iterations.
        max: usize,
    },
    /// A loop node has no termination condition (neither predicate nor `max_iterations`).
    NoTerminationCondition(NodeId),
    /// A system execution timed out.
    Timeout {
        /// The node that timed out.
        node: NodeId,
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },
    /// Feature not yet implemented.
    Unimplemented(&'static str),
    /// Maximum recursion depth exceeded in nested control flow.
    RecursionLimitExceeded {
        /// The current depth when the limit was hit.
        depth: usize,
        /// The maximum allowed depth.
        max: usize,
    },
    /// A switch node is missing its discriminator.
    MissingDiscriminator(NodeId),
    /// No matching case found in switch node and no default provided.
    NoMatchingCase {
        /// The switch node ID.
        node: NodeId,
        /// The discriminator value that didn't match any case.
        key: &'static str,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::EmptyGraph => write!(f, "graph has no entry point"),
            ExecutionError::NodeNotFound(id) => write!(f, "node not found: {id}"),
            ExecutionError::NoNextNode(id) => write!(f, "no sequential edge from node: {id}"),
            ExecutionError::MissingPredicate(id) => {
                write!(f, "missing predicate on node: {id}")
            }
            ExecutionError::MissingBranch { node, branch } => {
                write!(f, "missing {branch} branch on decision node: {node}")
            }
            ExecutionError::MissingJoin(id) => write!(f, "missing join on parallel node: {id}"),
            ExecutionError::SystemError(msg) => write!(f, "system error: {msg}"),
            ExecutionError::PredicateError(err) => write!(f, "predicate error: {err}"),
            ExecutionError::MaxIterationsExceeded { node, max } => {
                write!(f, "max iterations ({max}) exceeded on loop node: {node}")
            }
            ExecutionError::NoTerminationCondition(id) => {
                write!(f, "loop node has no termination condition: {id}")
            }
            ExecutionError::Timeout { node, timeout } => {
                write!(f, "system timed out after {:?} on node: {node}", timeout)
            }
            ExecutionError::Unimplemented(feature) => {
                write!(f, "feature not implemented: {feature}")
            }
            ExecutionError::RecursionLimitExceeded { depth, max } => {
                write!(
                    f,
                    "recursion limit exceeded: depth {depth} exceeds max {max}"
                )
            }
            ExecutionError::MissingDiscriminator(id) => {
                write!(f, "missing discriminator on switch node: {id}")
            }
            ExecutionError::NoMatchingCase { node, key } => {
                write!(f, "no matching case for key '{key}' on switch node: {node}")
            }
        }
    }
}

impl core::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            ExecutionError::PredicateError(err) => Some(err),
            _ => None,
        }
    }
}

/// Errors that can occur during resource validation.
///
/// These errors are detected before graph execution starts, allowing
/// early detection of missing resources that would cause runtime failures.
#[derive(Debug, Clone)]
pub enum ResourceValidationError {
    /// A required resource is missing from the context.
    MissingResource {
        /// The node ID of the system requiring the resource.
        node: NodeId,
        /// The name of the system.
        system_name: &'static str,
        /// The type name of the missing resource.
        resource_type: &'static str,
        /// The type ID of the missing resource.
        type_id: TypeId,
        /// The access mode (read or write).
        access_mode: AccessMode,
    },
    /// A required output from a previous system is missing.
    MissingOutput {
        /// The node ID of the system requiring the output.
        node: NodeId,
        /// The name of the system.
        system_name: &'static str,
        /// The type name of the missing output.
        output_type: &'static str,
        /// The type ID of the missing output.
        type_id: TypeId,
    },
}

impl fmt::Display for ResourceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceValidationError::MissingResource {
                node,
                system_name,
                resource_type,
                access_mode,
                ..
            } => {
                let mode_str = match access_mode {
                    AccessMode::Read => "read",
                    AccessMode::Write => "write",
                };
                write!(
                    f,
                    "system '{system_name}' ({node}) requires {mode_str} access to missing resource: {resource_type}"
                )
            }
            ResourceValidationError::MissingOutput {
                node,
                system_name,
                output_type,
                ..
            } => {
                write!(
                    f,
                    "system '{system_name}' ({node}) requires missing output: {output_type}"
                )
            }
        }
    }
}

impl core::error::Error for ResourceValidationError {}

/// Graph execution engine.
///
/// `GraphExecutor` traverses a graph starting from its entry point,
/// executing systems and following control flow edges.
#[derive(Debug)]
pub struct GraphExecutor {
    /// Maximum iterations for loops without explicit limits (safety default).
    default_max_iterations: Option<usize>,
    /// Maximum recursion depth for nested control flow (safety default).
    max_recursion_depth: usize,
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
    /// - **Outputs** (`Out<T>`): Currently not validated, as outputs are
    ///   produced dynamically during execution.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if all resources are available, or a vector of
    /// validation errors describing missing resources.
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

        for node in graph.nodes() {
            if let Node::System(sys) = node {
                let access = sys.system.access();
                self.validate_system_access(
                    &sys.id,
                    sys.system.name(),
                    &access,
                    ctx,
                    &hook_provided,
                    &mut errors,
                );
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validates a single system's access requirements against the context.
    fn validate_system_access(
        &self,
        node_id: &NodeId,
        system_name: &'static str,
        access: &SystemAccess,
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
    ) -> Result<ExecutionResult, ExecutionError> {
        let start = std::time::Instant::now();
        let entry = graph.entry().ok_or(ExecutionError::EmptyGraph)?;

        // Invoke OnGraphStart hook
        Self::invoke_hook::<OnGraphStart>(
            hooks,
            ctx,
            &GraphEvent::GraphStart {
                node_count: graph.node_count(),
            },
        );

        // Execute the graph
        let result = self.execute_from(graph, ctx, entry, 0, hooks).await;

        // Invoke OnGraphComplete hook
        let duration = start.elapsed();
        match result {
            Ok(nodes_executed) => {
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
    fn invoke_hook<S: Schedule>(
        hooks: Option<&HooksAPI>,
        ctx: &mut SystemContext<'_>,
        event: &GraphEvent,
    ) {
        if let Some(api) = hooks {
            api.invoke(S::schedule_id(), ctx, event);
        }
    }

    /// Invokes a graph event on each custom schedule attached to a system node.
    fn invoke_custom_schedules(
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

    /// Finds the next node connected by a sequential edge.
    fn find_next_sequential(&self, graph: &Graph, from: &NodeId) -> Result<NodeId, ExecutionError> {
        for edge in graph.edges() {
            if let Edge::Sequential(seq) = edge
                && seq.from == *from
            {
                return Ok(seq.to.clone());
            }
        }
        Err(ExecutionError::NoNextNode(from.clone()))
    }

    /// Finds an error handler edge from the given node.
    ///
    /// Returns the target node ID if an error edge exists from `from`.
    fn find_error_edge(&self, graph: &Graph, from: &NodeId) -> Option<NodeId> {
        for edge in graph.edges() {
            if let Edge::Error(err_edge) = edge
                && err_edge.from == *from
            {
                return Some(err_edge.to.clone());
            }
        }
        None
    }

    /// Finds a timeout handler edge from the given node.
    ///
    /// Returns the target node ID if a timeout edge exists from `from`.
    fn find_timeout_edge(&self, graph: &Graph, from: &NodeId) -> Option<NodeId> {
        for edge in graph.edges() {
            if let Edge::Timeout(timeout_edge) = edge
                && timeout_edge.from == *from
            {
                return Some(timeout_edge.to.clone());
            }
        }
        None
    }

    /// Executes a loop node, returning the number of nodes executed in the loop body.
    ///
    /// Returns a boxed future to support recursion with nested control flow.
    fn execute_loop<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        loop_node: &'a LoopNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
    ) -> futures::future::BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            let max_iterations = loop_node
                .max_iterations
                .or(self.default_max_iterations)
                .ok_or_else(|| ExecutionError::NoTerminationCondition(loop_node.id.clone()))?;

            let mut iterations = 0;
            let mut nodes_executed = 0;

            // Invoke OnLoopStart hook
            Self::invoke_hook::<OnLoopStart>(
                hooks,
                ctx,
                &GraphEvent::LoopStart {
                    node_id: loop_node.id.clone(),
                    loop_name: loop_node.name,
                    max_iterations: Some(max_iterations),
                },
            );

            let loop_start = std::time::Instant::now();

            loop {
                // Check termination predicate first
                if let Some(term) = &loop_node.termination
                    && term.evaluate(ctx).map_err(ExecutionError::PredicateError)?
                {
                    break;
                }

                if iterations >= max_iterations {
                    if loop_node.termination.is_some() {
                        return Err(ExecutionError::MaxIterationsExceeded {
                            node: loop_node.id.clone(),
                            max: max_iterations,
                        });
                    }
                    break;
                }

                // Invoke OnLoopIteration hook
                Self::invoke_hook::<OnLoopIteration>(
                    hooks,
                    ctx,
                    &GraphEvent::LoopIteration {
                        node_id: loop_node.id.clone(),
                        loop_name: loop_node.name,
                        iteration: iterations,
                    },
                );

                if let Some(body) = &loop_node.body_entry {
                    let count = self
                        .execute_from(graph, ctx, body.clone(), depth, hooks)
                        .await?;
                    nodes_executed += count;
                }

                iterations += 1;
            }

            // Invoke OnLoopEnd hook
            Self::invoke_hook::<OnLoopEnd>(
                hooks,
                ctx,
                &GraphEvent::LoopEnd {
                    node_id: loop_node.id.clone(),
                    loop_name: loop_node.name,
                    iterations,
                    nodes_executed,
                    duration: loop_start.elapsed(),
                },
            );

            Ok(nodes_executed)
        })
    }

    /// Executes parallel branches concurrently, returning the total nodes executed.
    ///
    /// Each branch runs in its own child context, providing isolation between
    /// parallel execution paths.
    fn execute_parallel<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        par: &'a ParallelNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
    ) -> futures::future::BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            use futures::future::try_join_all;

            let branch_count = par.branches.len();

            // Invoke OnParallelStart hook
            Self::invoke_hook::<OnParallelStart>(
                hooks,
                ctx,
                &GraphEvent::ParallelStart {
                    node_id: par.id.clone(),
                    node_name: par.name,
                    branch_count,
                },
            );

            let start = std::time::Instant::now();

            let mut child_contexts: Vec<SystemContext<'_>> =
                par.branches.iter().map(|_| ctx.child()).collect();

            let futures =
                par.branches
                    .iter()
                    .zip(child_contexts.iter_mut())
                    .map(|(branch, child_ctx)| {
                        self.execute_from(graph, child_ctx, branch.clone(), depth, hooks)
                    });

            let results = try_join_all(futures).await?;
            let total_nodes = results.iter().sum();

            // Merge outputs from child contexts back to parent (branch-order deterministic).
            // Extract outputs first, then drop children to release borrow on ctx.
            let child_outputs: Vec<_> = child_contexts
                .iter_mut()
                .map(SystemContext::take_outputs)
                .collect();
            drop(child_contexts);
            for outputs in child_outputs {
                ctx.outputs_mut().merge_from(outputs);
            }

            // Invoke OnParallelComplete hook
            Self::invoke_hook::<OnParallelComplete>(
                hooks,
                ctx,
                &GraphEvent::ParallelComplete {
                    node_id: par.id.clone(),
                    node_name: par.name,
                    branch_count,
                    total_nodes_executed: total_nodes,
                    duration: start.elapsed(),
                },
            );

            Ok(total_nodes)
        })
    }

    /// Executes a switch node, returning the nodes executed and optional next node.
    ///
    /// Returns a tuple of `(nodes_executed, next_node)` where `next_node` is the
    /// sequential continuation after the switch, if any.
    fn execute_switch<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        switch_node: &'a SwitchNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
    ) -> futures::future::BoxFuture<'a, Result<(usize, Option<NodeId>), ExecutionError>> {
        Box::pin(async move {
            // Invoke OnSwitchStart hook
            Self::invoke_hook::<OnSwitchStart>(
                hooks,
                ctx,
                &GraphEvent::SwitchStart {
                    node_id: switch_node.id.clone(),
                    node_name: switch_node.name,
                    case_count: switch_node.cases.len(),
                    has_default: switch_node.default.is_some(),
                },
            );

            let discriminator = switch_node
                .discriminator
                .as_ref()
                .ok_or_else(|| ExecutionError::MissingDiscriminator(switch_node.id.clone()))?;

            let key = discriminator
                .discriminate(ctx)
                .map_err(ExecutionError::PredicateError)?;

            let (target, used_default) = switch_node
                .cases
                .iter()
                .find(|(case_key, _)| *case_key == key)
                .map(|(_, node_id)| (node_id.clone(), false))
                .or_else(|| switch_node.default.as_ref().map(|d| (d.clone(), true)))
                .ok_or_else(|| ExecutionError::NoMatchingCase {
                    node: switch_node.id.clone(),
                    key,
                })?;

            let nodes_executed = self.execute_from(graph, ctx, target, depth, hooks).await?;

            // Invoke OnSwitchComplete hook
            Self::invoke_hook::<OnSwitchComplete>(
                hooks,
                ctx,
                &GraphEvent::SwitchComplete {
                    node_id: switch_node.id.clone(),
                    node_name: switch_node.name,
                    selected_case: if used_default {
                        DEFAULT_SWITCH_CASE
                    } else {
                        key
                    },
                    used_default,
                },
            );

            let next = self.find_next_sequential(graph, &switch_node.id).ok();
            Ok((nodes_executed, next))
        })
    }

    /// Core graph execution engine starting from a given node.
    ///
    /// This is the unified execution function used by both `execute()` (public API)
    /// and internal recursive calls for control flow constructs (decision branches,
    /// loop bodies, parallel branches, switch cases).
    ///
    /// Traverses the graph from `start`, executing nodes and following edges until
    /// a terminal point (no outgoing sequential edge) is reached.
    ///
    /// # Arguments
    ///
    /// * `graph` - The graph to execute
    /// * `ctx` - The system context for resource access and output storage
    /// * `start` - The node ID to begin execution from
    /// * `depth` - Current recursion depth for nested control flow (safety limit)
    /// * `hooks` - Optional hooks API for lifecycle callbacks
    ///
    /// # Returns
    ///
    /// The number of nodes executed, or an error if execution fails.
    fn execute_from<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        start: NodeId,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
    ) -> futures::future::BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            if depth >= self.max_recursion_depth {
                return Err(ExecutionError::RecursionLimitExceeded {
                    depth,
                    max: self.max_recursion_depth,
                });
            }

            let mut current = start;
            let mut nodes_executed = 0;

            loop {
                let node = graph
                    .get_node(current.clone())
                    .ok_or_else(|| ExecutionError::NodeNotFound(current.clone()))?;

                nodes_executed += 1;

                match node {
                    Node::System(sys) => {
                        // Invoke OnSystemStart hook
                        let start_event = GraphEvent::SystemStart {
                            node_id: current.clone(),
                            system_name: sys.name(),
                        };
                        Self::invoke_hook::<OnSystemStart>(hooks, ctx, &start_event);
                        Self::invoke_custom_schedules(hooks, ctx, &sys.schedules, &start_event);

                        let system_start = std::time::Instant::now();

                        let result = if let Some(timeout_duration) = sys.timeout {
                            match tokio::time::timeout(timeout_duration, sys.system.run_erased(ctx))
                                .await
                            {
                                Ok(inner_result) => inner_result,
                                Err(_elapsed) => {
                                    if let Some(handler) = self.find_timeout_edge(graph, &current) {
                                        current = handler;
                                        continue;
                                    }
                                    return Err(ExecutionError::Timeout {
                                        node: current,
                                        timeout: timeout_duration,
                                    });
                                }
                            }
                        } else {
                            sys.system.run_erased(ctx).await
                        };

                        match result {
                            Ok(output) => {
                                ctx.insert_output_boxed(sys.output_type_id(), output);

                                // Invoke OnSystemComplete hook
                                let complete_event = GraphEvent::SystemComplete {
                                    node_id: current.clone(),
                                    system_name: sys.name(),
                                    duration: system_start.elapsed(),
                                };
                                Self::invoke_hook::<OnSystemComplete>(hooks, ctx, &complete_event);
                                Self::invoke_custom_schedules(
                                    hooks,
                                    ctx,
                                    &sys.schedules,
                                    &complete_event,
                                );

                                match self.find_next_sequential(graph, &current) {
                                    Ok(next) => current = next,
                                    Err(ExecutionError::NoNextNode(_)) => break,
                                    Err(err) => return Err(err),
                                }
                            }
                            Err(err) => {
                                let error_string = err.to_string();

                                // Invoke OnSystemError hook
                                let error_event = GraphEvent::SystemError {
                                    node_id: current.clone(),
                                    system_name: sys.name(),
                                    error: error_string.clone(),
                                };
                                Self::invoke_hook::<OnSystemError>(hooks, ctx, &error_event);
                                Self::invoke_custom_schedules(
                                    hooks,
                                    ctx,
                                    &sys.schedules,
                                    &error_event,
                                );

                                if let Some(handler) = self.find_error_edge(graph, &current) {
                                    current = handler;
                                } else {
                                    return Err(ExecutionError::SystemError(error_string));
                                }
                            }
                        }
                    }
                    Node::Decision(dec) => {
                        let decision_id = current.clone();

                        // Invoke OnDecisionStart hook
                        Self::invoke_hook::<OnDecisionStart>(
                            hooks,
                            ctx,
                            &GraphEvent::DecisionStart {
                                node_id: current.clone(),
                                node_name: dec.name,
                            },
                        );

                        let predicate = dec
                            .predicate
                            .as_ref()
                            .ok_or_else(|| ExecutionError::MissingPredicate(current.clone()))?;

                        let result = predicate
                            .evaluate(ctx)
                            .map_err(ExecutionError::PredicateError)?;

                        let (branch_entry, selected_branch) = if result {
                            (
                                dec.true_branch.clone().ok_or_else(|| {
                                    ExecutionError::MissingBranch {
                                        node: current.clone(),
                                        branch: "true",
                                    }
                                })?,
                                "true",
                            )
                        } else {
                            (
                                dec.false_branch.clone().ok_or_else(|| {
                                    ExecutionError::MissingBranch {
                                        node: current.clone(),
                                        branch: "false",
                                    }
                                })?,
                                "false",
                            )
                        };

                        // Execute branch as subgraph (with increased depth)
                        let branch_count = self
                            .execute_from(graph, ctx, branch_entry, depth + 1, hooks)
                            .await?;
                        nodes_executed += branch_count;

                        // Invoke OnDecisionComplete hook
                        Self::invoke_hook::<OnDecisionComplete>(
                            hooks,
                            ctx,
                            &GraphEvent::DecisionComplete {
                                node_id: decision_id.clone(),
                                node_name: dec.name,
                                selected_branch,
                            },
                        );

                        match self.find_next_sequential(graph, &decision_id) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Join(_) => match self.find_next_sequential(graph, &current) {
                        Ok(next) => current = next,
                        Err(ExecutionError::NoNextNode(_)) => break,
                        Err(err) => return Err(err),
                    },
                    Node::Loop(loop_node) => {
                        let loop_count = self
                            .execute_loop(graph, ctx, loop_node, depth + 1, hooks)
                            .await?;
                        nodes_executed += loop_count;

                        match self.find_next_sequential(graph, &current) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Parallel(par) => {
                        let parallel_count = self
                            .execute_parallel(graph, ctx, par, depth + 1, hooks)
                            .await?;
                        nodes_executed += parallel_count;

                        if let Some(join) = &par.join {
                            current = join.clone();
                        } else {
                            match self.find_next_sequential(graph, &current) {
                                Ok(next) => current = next,
                                Err(ExecutionError::NoNextNode(_)) => break,
                                Err(err) => return Err(err),
                            }
                        }
                    }
                    Node::Switch(switch_node) => {
                        let (switch_count, next) = self
                            .execute_switch(graph, ctx, switch_node, depth + 1, hooks)
                            .await?;
                        nodes_executed += switch_count;
                        match next {
                            Some(n) => current = n,
                            None => break,
                        }
                    }
                }
            }

            Ok(nodes_executed)
        })
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
    }

    #[test]
    fn executor_without_limit() {
        let executor = GraphExecutor::without_iteration_limit();
        assert_eq!(executor.default_max_iterations, None);
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
}
