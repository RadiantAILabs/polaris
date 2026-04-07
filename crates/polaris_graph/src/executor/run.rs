//! Core graph execution engine — node dispatch and control flow.

use super::GraphExecutor;
use super::error::{CaughtError, ErrorKind, ExecutionError, SystemOutcome};
use crate::edge::Edge;
use crate::graph::Graph;
use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{
    OnDecisionComplete, OnDecisionStart, OnLoopEnd, OnLoopIteration, OnLoopStart,
    OnParallelComplete, OnParallelStart, OnSwitchComplete, OnSwitchStart, OnSystemComplete,
    OnSystemError, OnSystemStart,
};
use crate::middleware::{self, MiddlewareAPI};
use crate::node::{LoopNode, Node, NodeId, ParallelNode, SwitchNode, SystemNode};
use futures::future::BoxFuture;
use polaris_system::param::SystemContext;

/// Default case name for switch nodes when no match is found.
pub const DEFAULT_SWITCH_CASE: &str = "default";

impl GraphExecutor {
    /// Finds the next node connected by a sequential edge.
    pub(crate) fn find_next_sequential(
        &self,
        graph: &Graph,
        from: &NodeId,
    ) -> Result<NodeId, ExecutionError> {
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
    pub(crate) fn find_error_edge(&self, graph: &Graph, from: &NodeId) -> Option<NodeId> {
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
    pub(crate) fn find_timeout_edge(&self, graph: &Graph, from: &NodeId) -> Option<NodeId> {
        for edge in graph.edges() {
            if let Edge::Timeout(timeout_edge) = edge
                && timeout_edge.from == *from
            {
                return Some(timeout_edge.to.clone());
            }
        }
        None
    }

    /// Executes a system with optional retry policy and timeout per attempt.
    ///
    /// Each retry attempt gets a fresh timeout window. After all retries
    /// are exhausted, returns the final outcome (error or timeout).
    pub(crate) async fn run_with_retry(
        sys: &SystemNode,
        ctx: &mut SystemContext<'_>,
    ) -> SystemOutcome {
        let total_attempts = sys
            .retry_policy
            .as_ref()
            .map(|p| p.max_retries() + 1)
            .unwrap_or(1);

        let mut last_was_timeout = false;
        let mut last_err = None;

        for attempt in 0..total_attempts {
            if attempt > 0
                && let Some(policy) = &sys.retry_policy
            {
                let delay = policy.delay_for_attempt(attempt - 1);
                tokio::time::sleep(delay).await;
            }

            let result = if let Some(timeout_duration) = sys.timeout {
                match tokio::time::timeout(timeout_duration, sys.system.run_erased(ctx)).await {
                    Ok(inner) => inner,
                    Err(_elapsed) => {
                        last_was_timeout = true;
                        continue;
                    }
                }
            } else {
                sys.system.run_erased(ctx).await
            };

            match result {
                Ok(output) => return SystemOutcome::Ok(output),
                Err(sys_err) => {
                    last_was_timeout = false;
                    last_err = Some(sys_err);
                }
            }
        }

        if last_was_timeout {
            SystemOutcome::Timeout
        } else {
            SystemOutcome::Err(last_err.expect("at least one attempt was made"))
        }
    }

    /// Executes a loop node, returning the number of nodes executed in the loop body.
    ///
    /// Returns a boxed future to support recursion with nested control flow.
    pub(crate) fn execute_loop<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        loop_node: &'a LoopNode,
        max_iterations: usize,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            let mut iterations = 0;
            let mut nodes_executed = 0;

            // Invoke OnLoopStart hook
            Self::invoke_hook::<OnLoopStart>(
                hooks,
                ctx,
                &GraphEvent::LoopStart {
                    node_id: loop_node.id.clone(),
                    node_name: loop_node.name,
                    max_iterations,
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

                if let Some(body) = &loop_node.body_entry {
                    let iter_info = middleware::info::LoopIterationInfo {
                        node_id: loop_node.id.clone(),
                        node_name: loop_node.name,
                        iteration: iterations,
                        max_iterations,
                    };
                    let count = middleware
                        .inner
                        .loop_iteration
                        .execute(iter_info, ctx, |ctx| {
                            Self::invoke_hook::<OnLoopIteration>(
                                hooks,
                                ctx,
                                &GraphEvent::LoopIteration {
                                    node_id: loop_node.id.clone(),
                                    node_name: loop_node.name,
                                    iteration: iterations,
                                },
                            );
                            self.execute_from(graph, ctx, body.clone(), depth, hooks, middleware)
                        })
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
                    node_name: loop_node.name,
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
    pub(crate) fn execute_parallel<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        par: &'a ParallelNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
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

            let futures = par
                .branches
                .iter()
                .enumerate()
                .zip(child_contexts.iter_mut())
                .map(|((branch_index, branch), child_ctx)| {
                    let branch_info = middleware::info::ParallelBranchInfo {
                        node_name: par.name,
                        node_id: par.id.clone(),
                        branch_index,
                        branch_count: par.branches.len(),
                    };
                    middleware
                        .inner
                        .parallel_branch
                        .execute(branch_info, child_ctx, |ctx| {
                            self.execute_from(graph, ctx, branch.clone(), depth, hooks, middleware)
                        })
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

    /// Executes a switch node, returning the number of nodes executed in the selected branch.
    pub(crate) fn execute_switch<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        switch_node: &'a SwitchNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
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

            let nodes_executed = self
                .execute_from(graph, ctx, target, depth, hooks, middleware)
                .await?;

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

            Ok(nodes_executed)
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
    /// * `middleware` - Middleware chain for wrapping execution units
    ///
    /// # Returns
    ///
    /// The number of nodes executed, or an error if execution fails.
    pub(crate) fn execute_from<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        start: NodeId,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
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
                        let sys_info = middleware::info::SystemInfo {
                            node_id: current.clone(),
                            node_name: sys.name(),
                        };

                        let result = middleware
                            .inner
                            .system
                            .execute(sys_info, ctx, |ctx| {
                                Box::pin(async {
                                    // Invoke OnSystemStart hook
                                    let start_event = GraphEvent::SystemStart {
                                        node_id: current.clone(),
                                        node_name: sys.name(),
                                    };
                                    Self::invoke_hook::<OnSystemStart>(hooks, ctx, &start_event);
                                    Self::invoke_custom_schedules(
                                        hooks,
                                        ctx,
                                        &sys.schedules,
                                        &start_event,
                                    );

                                    let system_start = std::time::Instant::now();

                                    match Self::run_with_retry(sys, ctx).await {
                                        SystemOutcome::Ok(output) => {
                                            ctx.insert_output_boxed(sys.output_type_id(), output);

                                            // Invoke OnSystemComplete hook
                                            let complete_event = GraphEvent::SystemComplete {
                                                node_id: current.clone(),
                                                node_name: sys.name(),
                                                duration: system_start.elapsed(),
                                            };
                                            Self::invoke_hook::<OnSystemComplete>(
                                                hooks,
                                                ctx,
                                                &complete_event,
                                            );
                                            Self::invoke_custom_schedules(
                                                hooks,
                                                ctx,
                                                &sys.schedules,
                                                &complete_event,
                                            );

                                            Ok(())
                                        }
                                        SystemOutcome::Timeout => Err(ExecutionError::Timeout {
                                            node: current.clone(),
                                            timeout: sys
                                                .timeout
                                                .expect("timeout set when outcome is Timeout"),
                                        }),
                                        SystemOutcome::Err(err) => {
                                            let kind = match &err {
                                                polaris_system::system::SystemError::ParamError(
                                                    _,
                                                ) => ErrorKind::ParamResolution,
                                                polaris_system::system::SystemError::ExecutionError(
                                                    _,
                                                ) => ErrorKind::Execution,
                                            };
                                            let error_string = err.to_string();

                                            // Invoke OnSystemError hook
                                            let error_event = GraphEvent::SystemError {
                                                node_id: current.clone(),
                                                node_name: sys.name(),
                                                error: error_string.clone(),
                                            };
                                            Self::invoke_hook::<OnSystemError>(
                                                hooks,
                                                ctx,
                                                &error_event,
                                            );
                                            Self::invoke_custom_schedules(
                                                hooks,
                                                ctx,
                                                &sys.schedules,
                                                &error_event,
                                            );

                                            // Store error context for a potential error handler.
                                            // If no error edge exists, the error propagates and
                                            // this output is never consumed.
                                            ctx.outputs_mut().insert(CaughtError {
                                                message: error_string.clone(),
                                                system_name: sys.name(),
                                                node_id: current.clone(),
                                                duration: system_start.elapsed(),
                                                kind,
                                            });

                                            Err(ExecutionError::SystemError(error_string))
                                        }
                                    }
                                })
                            })
                            .await;

                        // Control flow routing stays outside middleware
                        match result {
                            Ok(()) => match self.find_next_sequential(graph, &current) {
                                Ok(next) => current = next,
                                Err(ExecutionError::NoNextNode(_)) => break,
                                Err(err) => return Err(err),
                            },
                            Err(ExecutionError::Timeout { .. }) => {
                                if let Some(handler) = self.find_timeout_edge(graph, &current) {
                                    current = handler;
                                } else {
                                    return Err(result.unwrap_err());
                                }
                            }
                            Err(ExecutionError::SystemError(_)) => {
                                if let Some(handler) = self.find_error_edge(graph, &current) {
                                    current = handler;
                                } else {
                                    return Err(result.unwrap_err());
                                }
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Decision(dec) => {
                        let decision_id = current.clone();
                        let dec_info = middleware::info::DecisionInfo {
                            node_id: decision_id.clone(),
                            node_name: dec.name,
                        };

                        let branch_count = middleware
                            .inner
                            .decision
                            .execute(dec_info, ctx, |ctx| {
                                Box::pin(async {
                                    // Invoke OnDecisionStart hook
                                    Self::invoke_hook::<OnDecisionStart>(
                                        hooks,
                                        ctx,
                                        &GraphEvent::DecisionStart {
                                            node_id: decision_id.clone(),
                                            node_name: dec.name,
                                        },
                                    );

                                    let predicate = dec.predicate.as_ref().ok_or_else(|| {
                                        ExecutionError::MissingPredicate(decision_id.clone())
                                    })?;

                                    let result = predicate
                                        .evaluate(ctx)
                                        .map_err(ExecutionError::PredicateError)?;

                                    let (branch_entry, selected_branch) = if result {
                                        (
                                            dec.true_branch.clone().ok_or_else(|| {
                                                ExecutionError::MissingBranch {
                                                    node: decision_id.clone(),
                                                    branch: "true",
                                                }
                                            })?,
                                            "true",
                                        )
                                    } else {
                                        (
                                            dec.false_branch.clone().ok_or_else(|| {
                                                ExecutionError::MissingBranch {
                                                    node: decision_id.clone(),
                                                    branch: "false",
                                                }
                                            })?,
                                            "false",
                                        )
                                    };

                                    // Execute branch as subgraph (with increased depth)
                                    let branch_count = self
                                        .execute_from(
                                            graph,
                                            ctx,
                                            branch_entry,
                                            depth + 1,
                                            hooks,
                                            middleware,
                                        )
                                        .await?;

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

                                    Ok(branch_count)
                                })
                            })
                            .await?;

                        nodes_executed += branch_count;

                        match self.find_next_sequential(graph, &decision_id) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Loop(loop_node) => {
                        let resolved_max = loop_node
                            .max_iterations
                            .or(self.default_max_iterations)
                            .ok_or_else(|| {
                                ExecutionError::NoTerminationCondition(current.clone())
                            })?;
                        let loop_info = middleware::info::LoopInfo {
                            node_id: current.clone(),
                            node_name: loop_node.name,
                            max_iterations: resolved_max,
                        };
                        let loop_count = middleware
                            .inner
                            .loop_node
                            .execute(loop_info, ctx, |ctx| {
                                self.execute_loop(
                                    graph,
                                    ctx,
                                    loop_node,
                                    resolved_max,
                                    depth + 1,
                                    hooks,
                                    middleware,
                                )
                            })
                            .await?;
                        nodes_executed += loop_count;

                        match self.find_next_sequential(graph, &current) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Parallel(par) => {
                        let par_info = middleware::info::ParallelInfo {
                            node_id: current.clone(),
                            node_name: par.name,
                            branch_count: par.branches.len(),
                        };
                        let parallel_count = middleware
                            .inner
                            .parallel_node
                            .execute(par_info, ctx, |ctx| {
                                self.execute_parallel(graph, ctx, par, depth + 1, hooks, middleware)
                            })
                            .await?;
                        nodes_executed += parallel_count;

                        match self.find_next_sequential(graph, &current) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    Node::Switch(switch_node) => {
                        let switch_info = middleware::info::SwitchInfo {
                            node_id: current.clone(),
                            node_name: switch_node.name,
                            case_count: switch_node.cases.len(),
                            has_default: switch_node.default.is_some(),
                        };
                        let switch_count = middleware
                            .inner
                            .switch
                            .execute(switch_info, ctx, |ctx| {
                                self.execute_switch(
                                    graph,
                                    ctx,
                                    switch_node,
                                    depth + 1,
                                    hooks,
                                    middleware,
                                )
                            })
                            .await?;
                        nodes_executed += switch_count;

                        match self.find_next_sequential(graph, &current) {
                            Ok(next) => current = next,
                            Err(ExecutionError::NoNextNode(_)) => break,
                            Err(err) => return Err(err),
                        }
                    }
                }
            }

            Ok(nodes_executed)
        })
    }
}
