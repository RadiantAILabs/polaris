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

mod error;
mod run;

pub use error::{CaughtError, ErrorKind, ExecutionError, ResourceValidationError};
pub use run::DEFAULT_SWITCH_CASE;

use crate::graph::Graph;
use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{OnGraphComplete, OnGraphFailure, OnGraphStart, OnSystemStart};
use crate::node::Node;
use hashbrown::HashSet;
use polaris_system::param::{AccessMode, SystemContext};
use polaris_system::plugin::{Schedule, ScheduleId};
use std::any::TypeId;
use std::time::Duration;

/// Result of executing a graph.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Number of nodes executed during traversal.
    pub nodes_executed: usize,
    /// Total execution duration.
    pub duration: Duration,
}

/// Graph execution engine.
///
/// `GraphExecutor` traverses a graph starting from its entry point,
/// executing systems and following control flow edges.
#[derive(Debug, Clone)]
pub struct GraphExecutor {
    /// Maximum iterations for loops without explicit limits (safety default).
    pub(crate) default_max_iterations: Option<usize>,
    /// Maximum recursion depth for nested control flow (safety default).
    pub(crate) max_recursion_depth: usize,
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
        node_id: &crate::node::NodeId,
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
    ) -> Result<ExecutionResult, ExecutionError> {
        let start = std::time::Instant::now();
        let entry = graph.entry().ok_or(ExecutionError::EmptyGraph)?;

        // Invoke OnGraphStart hook
        Self::invoke_hook::<OnGraphStart>(
            hooks,
            ctx,
            &GraphEvent::GraphStart {
                node_count: graph.node_count(),
                node_map: graph
                    .nodes()
                    .iter()
                    .map(|node| (node.id(), node.name()))
                    .collect(),
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
