//! Core graph execution engine — node dispatch and control flow.

use super::GraphExecutor;
use super::RunContext;
use super::error::{CaughtError, ErrorKind, ExecutionError, SystemOutcome};
use crate::cancel::RunCancellation;
use crate::edge::Edge;
use crate::graph::Graph;
use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{
    OnDecisionComplete, OnDecisionStart, OnDynamicComplete, OnDynamicStart, OnLoopEnd,
    OnLoopIteration, OnLoopStart, OnParallelComplete, OnParallelStart, OnScopeComplete,
    OnScopeStart, OnSwitchComplete, OnSwitchStart, OnSystemComplete, OnSystemError, OnSystemStart,
};
use crate::middleware::{self, MiddlewareAPI};
use crate::node::{
    CandidateSource, ContextPolicy, CrossingAction, DecisionNode, DynamicNode, LoopNode, Node,
    NodeId, NodeName, ParallelNode, ScopeNode, SwitchNode, SystemNode,
};
use crate::registry::SubgraphRegistry;
use futures::future::BoxFuture;
use polaris_system::param::{ParamError, ReadFromBindings, SystemContext};
use polaris_system::resource::ProducerKey;
use std::any::TypeId;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Default case name for switch nodes when no match is found.
pub const DEFAULT_SWITCH_CASE: &str = "default";

/// How far one parallel branch got.
///
/// Written by the branch body itself, so it survives a failure the branch's own
/// middleware raises after the body returned — the node count and the fact that
/// the branch declined a node would otherwise be lost with the error.
#[derive(Default)]
struct BranchProgress {
    /// Nodes the branch started, counted as `ExecutionResult::nodes_executed`
    /// counts them.
    nodes_executed: AtomicUsize,
    /// Whether the branch declined to start a node because the signal was
    /// raised.
    stopped_early: AtomicBool,
}

impl BranchProgress {
    /// Records a branch that ran its whole chain.
    fn finished(&self, nodes_executed: usize) {
        self.nodes_executed.store(nodes_executed, Ordering::Relaxed);
    }

    /// Records a branch that declined a node after `nodes_executed` started.
    fn stopped_early(&self, nodes_executed: usize) {
        self.nodes_executed.store(nodes_executed, Ordering::Relaxed);
        self.stopped_early.store(true, Ordering::Relaxed);
    }

    fn nodes_executed(&self) -> usize {
        self.nodes_executed.load(Ordering::Relaxed)
    }

    fn is_stopped_early(&self) -> bool {
        self.stopped_early.load(Ordering::Relaxed)
    }
}

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
    ///
    /// A raised `cancel` stops the sequence: the attempt that is already
    /// running is never dropped, but the *next* one does not start and the
    /// inter-attempt sleep does not delay the stop. The last attempt's outcome
    /// is returned and the executor's boundary check reports it as an
    /// interrupt.
    pub(crate) async fn run_with_retry(
        sys: &SystemNode,
        ctx: &mut SystemContext<'_>,
        cancel: Option<&RunCancellation>,
    ) -> SystemOutcome {
        let total_attempts = sys
            .retry_policy
            .as_ref()
            .map(|p| p.max_retries() + 1)
            .unwrap_or(1);

        let mut last_was_timeout = false;
        let mut last_err = None;

        for attempt in 0..total_attempts {
            if attempt > 0 {
                if cancel.is_some_and(RunCancellation::is_cancelled) {
                    break;
                }
                if let Some(policy) = &sys.retry_policy {
                    let delay = policy.delay_for_attempt(attempt - 1);
                    match cancel {
                        // Racing the latched signal, not polling it: an
                        // interrupt during a 30 s backoff stops now.
                        Some(token) => {
                            if token
                                .run_until_cancelled(tokio::time::sleep(delay))
                                .await
                                .is_none()
                            {
                                break;
                            }
                        }
                        None => tokio::time::sleep(delay).await,
                    }
                }
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
    ///
    /// The body runs directly against `ctx` — no per-iteration child context.
    /// A nested scope inside the body may `fork`/`forward` a resource from
    /// whatever *this loop's own* enclosing context is, and that crossing is
    /// resolved from the immediate parent only
    /// (`SystemContext::clone_local_resource_with` does not walk the chain);
    /// a synthetic child would become that parent instead, breaking every
    /// such crossing inside a loop. Isolation instead
    /// lives one level down, in `Outputs`' own layer stack (opened through an
    /// `OutputLayerScope`), which resources never see at all — see
    /// `output_layers` and `OutputLayerScope`'s own docs for the full design.
    ///
    /// Every iteration owns one output layer. Completed layers remain
    /// readable to later iterations; retention evicts entries by type, then
    /// loop exit promotes only the newest retained layer into the enclosing
    /// one. An error, timeout, cancellation, or max-iteration failure — or
    /// dropping this future part-way — drops the whole iteration region
    /// without changing the enclosing layer.
    pub(crate) fn execute_loop<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        loop_node: &'a LoopNode,
        max_iterations: usize,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            let mut iterations = 0;
            let mut nodes_executed = 0;

            // Invoke OnLoopStart hook
            Self::invoke_hook::<OnLoopStart>(
                hooks,
                ctx,
                &GraphEvent::LoopStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: loop_node.id.clone(),
                    node_name: loop_node.name.clone(),
                    max_iterations,
                },
            );

            let loop_start = std::time::Instant::now();

            let (default_retention, per_type_retention) =
                graph.loop_output_retention(&loop_node.id);

            // The loop's iteration layers live in one scope. Every early
            // return below — an error, a cancellation, max iterations, or
            // this future being dropped mid-iteration — drops the scope,
            // which discards the whole region and leaves the enclosing
            // layers exactly as they were. Only a normal exit promotes.
            // Nested loops open their own scope above this one's layers.
            let mut scope = ctx.output_layer_scope();

            loop {
                // Check termination predicate first. Reads see retained
                // layers, including values from earlier loop iterations.
                if let Some(term) = &loop_node.termination
                    && term
                        .evaluate(&scope)
                        .map_err(ExecutionError::PredicateError)?
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
                    // Decline the iteration before its middleware chain and
                    // `OnLoopIteration` observe it: an iteration that never
                    // starts must not be reported as one that did. The
                    // termination predicate above still runs first, so a loop
                    // that was already done ends normally.
                    if run_ctx.is_cancelled() {
                        return Err(run_ctx.interrupted(body, nodes_executed));
                    }

                    // Make room for this iteration before it writes.
                    // Retention counts *completed* iterations: with N = 1 the
                    // previous iteration stays readable while this one runs,
                    // so a body system can read what a later system wrote in
                    // the previous iteration (and a backward `read_from` wire
                    // finds its entry).
                    scope.retain_layers(default_retention, per_type_retention.iter().copied());
                    scope.push_layer();

                    // Recorded on every output write in this iteration's body
                    // as `ProducerInfo` provenance (never as replace
                    // identity), so a reader of `OutAll<T>` in a later stage
                    // can tell iterations apart.
                    let iter_run_ctx = RunContext {
                        loop_iteration: iterations as u64,
                        output_path: run_ctx
                            .output_path
                            .push_loop_iteration(&loop_node.id, iterations as u64),
                        ..run_ctx.clone()
                    };
                    let run_ctx = &iter_run_ctx;

                    let iter_info = middleware::info::LoopIterationInfo {
                        node_id: loop_node.id.clone(),
                        node_name: loop_node.name.clone(),
                        iteration: iterations,
                        max_iterations,
                    };
                    let result = middleware
                        .inner
                        .loop_iteration
                        .execute(iter_info, &mut scope, |ctx| {
                            Self::invoke_hook::<OnLoopIteration>(
                                hooks,
                                ctx,
                                &GraphEvent::LoopIteration {
                                    run_id: run_ctx.run_id.clone(),
                                    labels: run_ctx.labels.clone(),
                                    node_id: loop_node.id.clone(),
                                    node_name: loop_node.name.clone(),
                                    iteration: iterations,
                                },
                            );
                            self.execute_from(
                                graph,
                                ctx,
                                body.clone(),
                                depth,
                                hooks,
                                middleware,
                                run_ctx,
                            )
                        })
                        .await;

                    match result {
                        Ok(count) => {
                            nodes_executed += count;
                        }
                        Err(err) => {
                            // No completed iteration crosses a failed loop
                            // boundary: returning drops `scope`, which
                            // discards every iteration layer, including the
                            // in-progress one.
                            return Err(RunContext::accumulate(nodes_executed, err));
                        }
                    }
                }

                iterations += 1;
            }

            // A zero-iteration loop has an empty region, so this is a no-op.
            // Otherwise older retained iteration layers are discarded and
            // only the newest layer crosses the loop boundary.
            scope.promote_newest(None);

            // Invoke OnLoopEnd hook
            Self::invoke_hook::<OnLoopEnd>(
                hooks,
                ctx,
                &GraphEvent::LoopEnd {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: loop_node.id.clone(),
                    node_name: loop_node.name.clone(),
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
    ///
    /// A failing branch aborts its siblings, except while the run's
    /// cancellation signal is raised: then no branch is dropped, failures are
    /// held until every branch has finished, and the node reports the stop (or
    /// a structural fault, which takes priority) afterwards. When every branch
    /// ran its whole chain the node completes normally and the next boundary
    /// decides whether the run continues.
    pub(crate) fn execute_parallel<'a>(
        &'a self,
        graph: &'a Graph,
        ctx: &'a mut SystemContext<'_>,
        par: &'a ParallelNode,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            use futures::future::try_join_all;

            let branch_count = par.branches.len();

            // Invoke OnParallelStart hook
            Self::invoke_hook::<OnParallelStart>(
                hooks,
                ctx,
                &GraphEvent::ParallelStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: par.id.clone(),
                    node_name: par.name.clone(),
                    branch_count,
                },
            );

            let start = std::time::Instant::now();

            let mut child_contexts: Vec<SystemContext<'_>> =
                par.branches.iter().map(|_| ctx.child()).collect();
            let progress: Vec<BranchProgress> = par
                .branches
                .iter()
                .map(|_| BranchProgress::default())
                .collect();

            let futures = par
                .branches
                .iter()
                .enumerate()
                .zip(child_contexts.iter_mut())
                .zip(progress.iter())
                .map(|(((branch_index, branch), child_ctx), progress)| {
                    let branch_info = middleware::info::ParallelBranchInfo {
                        node_name: par.name.clone(),
                        node_id: par.id.clone(),
                        branch_index,
                        branch_count: par.branches.len(),
                    };
                    // Diagnostic only (see `output_layers`): each branch's
                    // writes carry its own path segment so two writes from
                    // the same node in different branches are distinguishable
                    // in traces. Owned and moved into the branch's own
                    // future, never borrowed from `run_ctx`, since branches
                    // run concurrently and each needs its own copy.
                    let branch_run_ctx = RunContext {
                        output_path: run_ctx
                            .output_path
                            .push_parallel_branch(&par.id, branch_index),
                        ..run_ctx.clone()
                    };
                    let wrapped_branch = middleware.inner.parallel_branch.execute(
                        branch_info,
                        child_ctx,
                        move |ctx| {
                            let branch = branch.clone();
                            // `TerminalFn` requires `Fn`, not `FnOnce` — clone
                            // the branch's owned path per call rather than
                            // moving the outer capture (which only actually
                            // runs once in practice).
                            let branch_run_ctx = branch_run_ctx.clone();
                            Box::pin(async move {
                                let result = self
                                    .execute_from(
                                        graph,
                                        ctx,
                                        branch,
                                        depth,
                                        hooks,
                                        middleware,
                                        &branch_run_ctx,
                                    )
                                    .await;
                                match result {
                                    Ok(nodes_executed) => {
                                        progress.finished(nodes_executed);
                                        Ok(nodes_executed)
                                    }
                                    // An interrupted branch is returned as an
                                    // ordinary completed branch carrying its
                                    // node count, so a consumer's branch
                                    // middleware does not observe it as a
                                    // failed one. The parallel node reports
                                    // the stop itself after the join.
                                    Err(ExecutionError::Interrupted { nodes_executed, .. }) => {
                                        progress.stopped_early(nodes_executed);
                                        Ok(nodes_executed)
                                    }
                                    other => other,
                                }
                            })
                        },
                    );
                    async move {
                        match wrapped_branch.await {
                            Ok(_) => Ok(None),
                            // While the signal is raised, a failure must not
                            // make `try_join_all` drop a sibling that is still
                            // running. The failure is set aside, the join
                            // waits for every branch, and the verdict is
                            // reached below.
                            Err(err) if run_ctx.is_cancelled() => Ok(Some(err)),
                            Err(err) => Err(err),
                        }
                    }
                });

            let set_aside = try_join_all(futures).await?;
            // What each branch body reported, whether or not its own
            // middleware then failed.
            let total_nodes: usize = progress.iter().map(BranchProgress::nodes_executed).sum();

            // A structural fault is reported as itself, in branch order.
            // Anything an interrupt supersedes is folded into the stop below.
            let mut superseded = false;
            for err in set_aside.into_iter().flatten() {
                if !RunContext::yields_to_interrupt(&err) {
                    return Err(err);
                }
                superseded = true;
            }

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

            // Every branch has finished before we get here, so the stop is
            // reported only now — and only after the merge above. The whole
            // point of never dropping a sibling is that it is allowed to
            // finish; discarding its outputs after paying its latency would
            // make that guarantee worthless. `OnParallelComplete` does not
            // fire, consistent with every other propagating error.
            //
            // The parallel node stops only if a branch declined a node or
            // failed in a way the interrupt supersedes. When every branch ran
            // its whole chain, the node completed: a raise that landed during
            // it is observed at the next boundary, and a run that ends here
            // completes.
            if superseded || progress.iter().any(BranchProgress::is_stopped_early) {
                return Err(run_ctx.interrupted(&par.id, total_nodes));
            }

            // Invoke OnParallelComplete hook
            Self::invoke_hook::<OnParallelComplete>(
                hooks,
                ctx,
                &GraphEvent::ParallelComplete {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: par.id.clone(),
                    node_name: par.name.clone(),
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
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            // Invoke OnSwitchStart hook
            Self::invoke_hook::<OnSwitchStart>(
                hooks,
                ctx,
                &GraphEvent::SwitchStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: switch_node.id.clone(),
                    node_name: switch_node.name.clone(),
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
                    key: key.clone(),
                })?;

            let nodes_executed = self
                .execute_from(graph, ctx, target, depth, hooks, middleware, run_ctx)
                .await?;

            // Invoke OnSwitchComplete hook
            Self::invoke_hook::<OnSwitchComplete>(
                hooks,
                ctx,
                &GraphEvent::SwitchComplete {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: switch_node.id.clone(),
                    node_name: switch_node.name.clone(),
                    selected_case: if used_default {
                        DEFAULT_SWITCH_CASE.into()
                    } else {
                        key
                    },
                    used_default,
                },
            );

            Ok(nodes_executed)
        })
    }

    /// Returns the next node via sequential edge, or `None` at a terminal point.
    fn advance(&self, graph: &Graph, from: &NodeId) -> Result<Option<NodeId>, ExecutionError> {
        match self.find_next_sequential(graph, from) {
            Ok(next) => Ok(Some(next)),
            Err(ExecutionError::NoNextNode(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Routes a system execution result to the next node, following error/timeout
    /// edges when present.
    fn route_system_result(
        &self,
        graph: &Graph,
        current: &NodeId,
        result: Result<(), ExecutionError>,
    ) -> Result<Option<NodeId>, ExecutionError> {
        match result {
            Ok(()) => self.advance(graph, current),
            Err(err @ ExecutionError::Timeout { .. }) => {
                self.find_timeout_edge(graph, current).map(Some).ok_or(err)
            }
            Err(err @ ExecutionError::SystemError(_)) => {
                self.find_error_edge(graph, current).map(Some).ok_or(err)
            }
            Err(err) => Err(err),
        }
    }

    /// Builds the `read_from` producer bindings `reader` installs for the
    /// duration of its own `run` call — `None` when it has none, so the
    /// common case (no wiring on this node) pays only an `Option` check.
    ///
    /// Only the producer's node id matters for the lookup ([`ProducerKey`]
    /// equality compares the id alone), so the search key here carries no
    /// provenance — that is only ever attached to the key a producer
    /// *writes* under, in [`run_system_body`](Self::run_system_body)'s own
    /// two write sites.
    fn read_from_bindings_for(graph: &Graph, reader: &NodeId) -> Option<Arc<ReadFromBindings>> {
        let pairs: Vec<(TypeId, ProducerKey)> = graph
            .output_wiring
            .for_reader(reader)
            .map(|(type_id, binding)| (type_id, ProducerKey::new(binding.producer.as_str())))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(Arc::new(ReadFromBindings::from_pairs(pairs)))
        }
    }

    /// Middleware callback body for system nodes: hooks, retry, output/error handling.
    fn run_system_body<'a>(
        &'a self,
        graph: &'a Graph,
        sys: &'a SystemNode,
        current: &'a NodeId,
        hooks: Option<&'a HooksAPI>,
        ctx: &'a mut SystemContext<'_>,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<(), ExecutionError>> {
        Box::pin(async move {
            let start_event = GraphEvent::SystemStart {
                run_id: run_ctx.run_id.clone(),
                labels: run_ctx.labels.clone(),
                node_id: current.clone(),
                node_name: sys.node_name.clone(),
            };
            Self::invoke_hook::<OnSystemStart>(hooks, ctx, &start_event);
            Self::invoke_custom_schedules(hooks, ctx, &sys.schedules, &start_event);

            let system_start = std::time::Instant::now();

            // Installed only for the extent of this node's own run (all
            // retry attempts included), never copying a value or mutating
            // the output store. The scope restores the previous bindings
            // when it drops — after the run, or when this future is dropped
            // mid-run — see `SystemContext::read_from_scope`.
            let outcome = {
                let mut read_from =
                    ctx.read_from_scope(Self::read_from_bindings_for(graph, current));
                Self::run_with_retry(sys, &mut read_from, run_ctx.cancellation.as_ref()).await
            };

            match outcome {
                SystemOutcome::Ok(output) => {
                    let producer = run_ctx.producer_key(current, &sys.node_name);
                    ctx.insert_output_boxed_from(producer, sys.output_type_id(), output);

                    let complete_event = GraphEvent::SystemComplete {
                        run_id: run_ctx.run_id.clone(),
                        labels: run_ctx.labels.clone(),
                        node_id: current.clone(),
                        node_name: sys.node_name.clone(),
                        duration: system_start.elapsed(),
                    };
                    Self::invoke_hook::<OnSystemComplete>(hooks, ctx, &complete_event);
                    Self::invoke_custom_schedules(hooks, ctx, &sys.schedules, &complete_event);

                    Ok(())
                }
                SystemOutcome::Timeout => Err(ExecutionError::Timeout {
                    node: current.clone(),
                    timeout: sys.timeout.expect("timeout set when outcome is Timeout"),
                }),
                SystemOutcome::Err(err) => {
                    let kind = match &err {
                        polaris_system::system::SystemError::ParamError(_) => {
                            ErrorKind::ParamResolution
                        }
                        polaris_system::system::SystemError::ExecutionError(_) => {
                            ErrorKind::Execution
                        }
                    };
                    let error_msg: Arc<str> = Arc::from(err.to_string());

                    let error_event = GraphEvent::SystemError {
                        run_id: run_ctx.run_id.clone(),
                        labels: run_ctx.labels.clone(),
                        node_id: current.clone(),
                        node_name: sys.node_name.clone(),
                        error: Arc::clone(&error_msg),
                    };
                    Self::invoke_hook::<OnSystemError>(hooks, ctx, &error_event);
                    Self::invoke_custom_schedules(hooks, ctx, &sys.schedules, &error_event);

                    // Store error context for a potential error handler.
                    // If no error edge exists, the error propagates and
                    // this output is never consumed. Attributed to the
                    // failing node's own producer key, so a node that keeps
                    // failing across loop iterations keeps one `CaughtError`
                    // entry rather than accumulating one per iteration.
                    let producer = run_ctx.producer_key(current, &sys.node_name);
                    ctx.insert_output_from(
                        producer,
                        CaughtError {
                            message: Arc::clone(&error_msg),
                            system_name: sys.name(),
                            node_id: current.clone(),
                            duration: system_start.elapsed(),
                            kind,
                        },
                    );

                    Err(ExecutionError::SystemError(error_msg))
                }
            }
        })
    }

    /// Middleware callback body for decision nodes: predicate, branch selection, recursion.
    fn run_decision_body<'a>(
        &'a self,
        dec: &'a DecisionNode,
        decision_id: &'a NodeId,
        graph: &'a Graph,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        ctx: &'a mut SystemContext<'_>,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            Self::invoke_hook::<OnDecisionStart>(
                hooks,
                ctx,
                &GraphEvent::DecisionStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: decision_id.clone(),
                    node_name: dec.name.clone(),
                },
            );

            let predicate = dec
                .predicate
                .as_ref()
                .ok_or_else(|| ExecutionError::MissingPredicate(decision_id.clone()))?;

            let result = predicate
                .evaluate(ctx)
                .map_err(ExecutionError::PredicateError)?;

            let (branch_entry, selected_branch) = if result {
                (
                    dec.true_branch
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingBranch {
                            node: decision_id.clone(),
                            branch: "true",
                        })?,
                    "true",
                )
            } else {
                (
                    dec.false_branch
                        .clone()
                        .ok_or_else(|| ExecutionError::MissingBranch {
                            node: decision_id.clone(),
                            branch: "false",
                        })?,
                    "false",
                )
            };

            let branch_count = self
                .execute_from(
                    graph,
                    ctx,
                    branch_entry,
                    depth + 1,
                    hooks,
                    middleware,
                    run_ctx,
                )
                .await?;

            Self::invoke_hook::<OnDecisionComplete>(
                hooks,
                ctx,
                &GraphEvent::DecisionComplete {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: decision_id.clone(),
                    node_name: dec.name.clone(),
                    selected_branch,
                },
            );

            Ok(branch_count)
        })
    }

    /// Runs `graph` through a context boundary governed by `policy`, returning
    /// the number of nodes executed.
    ///
    /// This is the shared core of [`Scope`](Node::Scope) and
    /// [`Dynamic`](Node::Dynamic) execution: a shared policy reuses the parent
    /// context directly, otherwise a filtered child context is created,
    /// per-policy resource crossings are populated, the embedded graph runs, and
    /// its outputs merge back into the parent on exit. The embedded graph's own
    /// [`max_duration`](Graph::max_duration) wraps execution in a timeout.
    ///
    /// `produce_filter` narrows that merge-back to a declared set of output
    /// `TypeId`s — the mechanism behind a [`ScopeNode`] declared contract (see
    /// its *Declared contracts* docs): pass `Some` from a scope carrying one,
    /// `None` from a plain scope or any dynamic node (dynamic contracts are
    /// may-merge, not must, so nothing is filtered there). It only has an
    /// effect on the non-shared path below — `shared()` has no child context
    /// and thus no merge step, so a contract paired with `shared()` cannot
    /// hide anything the interior writes; see [`ScopeNode`]'s docs for why
    /// that is sound rather than a gap.
    ///
    /// `name` labels the boundary in resource-crossing errors, and
    /// `boundary_kind` (`"scope"` or `"dynamic"`) labels it in the
    /// diagnostic-only output write path (see `output_layers`). Hooks and
    /// node-specific tracing live in the per-node callers (`run_scope_body` /
    /// `run_dynamic_body`), so both node types share provably identical boundary
    /// semantics.
    fn execute_embedded<'a>(
        &'a self,
        graph: &'a Graph,
        policy: &'a ContextPolicy,
        produce_filter: Option<&'a HashSet<TypeId>>,
        name: &'a NodeName,
        boundary_kind: &'static str,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        ctx: &'a mut SystemContext<'_>,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let entry = graph.entry().ok_or(ExecutionError::EmptyGraph)?;
            // A Dynamic slot is always an output boundary, even when its
            // resource policy is `shared()`: shared resources and reads stay
            // available while candidate writes accumulate in a temporary
            // layer, then only the slot contract's outputs are promoted. A
            // shared Scope remains the one exception: it deliberately writes
            // directly into its parent.
            //
            // The scope is opened for every boundary but holds a layer only
            // for that one case. Dropping it — on an error, or when this
            // future is dropped mid-run — discards that layer; for every other
            // boundary it holds nothing, so dropping or promoting it changes
            // nothing.
            let shared_output_boundary = policy.is_shared() && boundary_kind == "dynamic";
            let mut scope = ctx.output_layer_scope();
            if shared_output_boundary {
                scope.push_layer();
            }

            let execute_body = async {
                let ctx = &mut *scope;
                if policy.is_shared() {
                    // Same context, so parent-produced or caller-seeded
                    // outputs satisfy an inner loop's first termination check.
                    // `shared()` has no layer of its own (see `output_layers`
                    // module docs), so the write path is unchanged here too.
                    Self::check_loop_entry_inputs(graph, ctx)?;
                    return self
                        .execute_from(graph, ctx, entry, depth + 1, hooks, middleware, run_ctx)
                        .await;
                }

                // Every non-shared boundary keeps a parent reference; the
                // parent_filter expresses what's reachable through it. Pure
                // isolation (no `share` / `share_rest`) is just an empty
                // AllowOnly filter — globals still flow through, parent locals
                // are blocked. Retaining the parent ref is what lets
                // ParamError::ResourceOutOfScope distinguish a missing resource
                // from one the policy is hiding.
                let mut child = ctx.child_filtered(policy.parent_filter_arc());
                Self::populate_child_locals(policy, name, ctx, &mut child)?;
                // Outputs never cross a non-shared boundary, so an inner
                // loop's first termination check can only be satisfied by
                // production earlier on the inner chain — the child context
                // starts with no outputs, and this check reflects that.
                Self::check_loop_entry_inputs(graph, &child)?;
                let boundary_run_ctx = RunContext {
                    output_path: run_ctx.output_path.push_boundary(boundary_kind, name),
                    ..run_ctx.clone()
                };
                let inner_count = self
                    .execute_from(
                        graph,
                        &mut child,
                        entry,
                        depth + 1,
                        hooks,
                        middleware,
                        &boundary_run_ctx,
                    )
                    .await?;
                let child_outputs = child.take_outputs();
                drop(child);
                match produce_filter {
                    Some(allowed) => ctx.outputs_mut().merge_filtered(child_outputs, allowed),
                    None => ctx.outputs_mut().merge_from(child_outputs),
                }
                Ok(inner_count)
            };

            let result = if let Some(max) = graph.max_duration {
                match tokio::time::timeout(max, execute_body).await {
                    Ok(inner) => inner,
                    Err(_timeout) => {
                        let elapsed = start.elapsed();
                        Err(ExecutionError::GraphTimeout { elapsed, max })
                    }
                }
            } else {
                execute_body.await
            };

            if result.is_ok() {
                scope.promote_newest(produce_filter);
            }
            result
        })
    }

    /// Middleware callback body for scope nodes: hooks + the shared embedded
    /// boundary.
    ///
    /// Accepts the parent's current `depth` so that the inner graph execution
    /// increments it, preventing nested scopes from bypassing the recursion limit.
    fn run_scope_body<'a>(
        &'a self,
        scope: &'a ScopeNode,
        scope_id: &'a NodeId,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        ctx: &'a mut SystemContext<'_>,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            let mode = scope.context_policy.mode();
            Self::invoke_hook::<OnScopeStart>(
                hooks,
                ctx,
                &GraphEvent::ScopeStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: scope_id.clone(),
                    node_name: scope.name.clone(),
                    mode,
                    inner_node_count: scope.graph.node_count(),
                },
            );

            let start = std::time::Instant::now();

            // `None` for a plain scope keeps today's unfiltered merge; `Some`
            // for a declared scope drops every interior output the contract
            // does not name, sealing the boundary the contract promises.
            let allowed: Option<HashSet<TypeId>> = scope.contract().map(|contract| {
                contract
                    .produces()
                    .iter()
                    .map(|access| access.type_id)
                    .collect()
            });

            let count = self
                .execute_embedded(
                    &scope.graph,
                    &scope.context_policy,
                    allowed.as_ref(),
                    &scope.name,
                    "scope",
                    depth,
                    hooks,
                    middleware,
                    ctx,
                    run_ctx,
                )
                .await?;

            Self::invoke_hook::<OnScopeComplete>(
                hooks,
                ctx,
                &GraphEvent::ScopeComplete {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: scope_id.clone(),
                    node_name: scope.name.clone(),
                    mode,
                    nodes_executed: count,
                    duration: start.elapsed(),
                },
            );

            Ok(count)
        })
    }

    /// Middleware callback body for dynamic nodes: select a candidate, then run
    /// it through the shared embedded boundary with hooks.
    fn run_dynamic_body<'a>(
        &'a self,
        node: &'a DynamicNode,
        node_id: &'a NodeId,
        depth: usize,
        hooks: Option<&'a HooksAPI>,
        middleware: &'a MiddlewareAPI,
        ctx: &'a mut SystemContext<'_>,
        run_ctx: &'a RunContext,
    ) -> BoxFuture<'a, Result<usize, ExecutionError>> {
        Box::pin(async move {
            Self::invoke_hook::<OnDynamicStart>(
                hooks,
                ctx,
                &GraphEvent::DynamicStart {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: node_id.clone(),
                    node_name: node.name.clone(),
                    mode: node.context_policy.mode(),
                },
            );

            // The reported duration covers the whole dynamic step — selection,
            // lookup, and candidate execution — matching the event's "total
            // duration" contract.
            let start = std::time::Instant::now();

            // Select a candidate key, then resolve it to a graph (falling back to
            // the configured default key). Both lookups borrow `ctx` immutably and
            // return an owned `Arc<Graph>`, so the borrow is released before the
            // candidate runs against `&mut ctx`.
            let key = node
                .selector
                .select(ctx)
                .map_err(ExecutionError::PredicateError)?;
            let (selected, candidate) = match Self::lookup_candidate(node, node_id, ctx, &key)? {
                Some(candidate) => (key, candidate),
                None => {
                    let default_key = node.default.clone().ok_or_else(|| {
                        ExecutionError::DynamicCandidateNotFound {
                            node: node_id.clone(),
                            name: node.name.clone(),
                            key: key.clone(),
                        }
                    })?;
                    let candidate = Self::lookup_candidate(node, node_id, ctx, &default_key)?
                        .ok_or_else(|| ExecutionError::DynamicCandidateNotFound {
                            node: node_id.clone(),
                            name: node.name.clone(),
                            key: default_key.clone(),
                        })?;
                    (default_key, candidate)
                }
            };

            // Promote only the slot contract's `produces` — the same rule a
            // declared scope contract enforces, and a deliberate behaviour
            // change from earlier (unfiltered) merge-back: a `Substitutable`
            // candidate's extra outputs (beyond the contract) no longer
            // reach the parent at all, so they can no longer shadow a
            // same-typed value the parent chain produces. See
            // `docs/reference/graph.md`'s "Extra outputs" section and
            // `ValidationWarning::DynamicCandidateShadowsOutput`.
            let allowed: HashSet<TypeId> = node
                .contract
                .produces()
                .iter()
                .map(|access| access.type_id)
                .collect();

            let count = self
                .execute_embedded(
                    &candidate,
                    &node.context_policy,
                    Some(&allowed),
                    &node.name,
                    "dynamic",
                    depth,
                    hooks,
                    middleware,
                    ctx,
                    run_ctx,
                )
                .await?;

            Self::invoke_hook::<OnDynamicComplete>(
                hooks,
                ctx,
                &GraphEvent::DynamicComplete {
                    run_id: run_ctx.run_id.clone(),
                    labels: run_ctx.labels.clone(),
                    node_id: node_id.clone(),
                    node_name: node.name.clone(),
                    mode: node.context_policy.mode(),
                    selected,
                    nodes_executed: count,
                    duration: start.elapsed(),
                },
            );

            Ok(count)
        })
    }

    /// Resolves a candidate key to its subgraph, cloning the `Arc<Graph>` out so
    /// the caller can drop the context borrow before running it.
    ///
    /// `Ok(None)` means exclusively that the key is absent from an otherwise
    /// resolvable registry (the caller falls back to the node's default). Every
    /// other failure is a hard, typed `Err` that must not fall back, because the
    /// default key resolves through the same registry and cannot recover:
    ///
    /// - the registry is absent from context
    ///   ([`DynamicRegistryMissing`](ExecutionError::DynamicRegistryMissing)),
    ///   write-locked ([`DynamicRegistryBusy`](ExecutionError::DynamicRegistryBusy)),
    ///   or hidden by an enclosing scope policy
    ///   ([`DynamicRegistryOutOfScope`](ExecutionError::DynamicRegistryOutOfScope));
    /// - the registry's contract does not fill this node's slot
    ///   ([`DynamicContractMismatch`](ExecutionError::DynamicContractMismatch)).
    fn lookup_candidate(
        node: &DynamicNode,
        node_id: &NodeId,
        ctx: &SystemContext<'_>,
        key: &str,
    ) -> Result<Option<Arc<Graph>>, ExecutionError> {
        match &node.source {
            CandidateSource::Inline(candidates) => Ok(candidates
                .iter()
                .find(|(candidate_key, _)| &**candidate_key == key)
                .map(|(_, graph)| Arc::clone(graph))),
            // v1 reads the concrete `SubgraphRegistry`; `resource_type` reserves
            // the slot for future wrapper resource types.
            CandidateSource::Registry { .. } => {
                // A failed registry resolution is a distinct, unrecoverable
                // cause — not a key miss. Preserve it as a typed error rather
                // than laundering it into a default-key fallback that resolves
                // through the same registry.
                let registry = match ctx.get_resource::<SubgraphRegistry>() {
                    Ok(registry) => registry,
                    Err(ParamError::BorrowConflict(_)) => {
                        return Err(ExecutionError::DynamicRegistryBusy {
                            node: node_id.clone(),
                            name: node.name.clone(),
                        });
                    }
                    Err(ParamError::ResourceOutOfScope(_)) => {
                        return Err(ExecutionError::DynamicRegistryOutOfScope {
                            node: node_id.clone(),
                            name: node.name.clone(),
                        });
                    }
                    // `ResourceNotFound` — and any other variant, since the
                    // output/error-context errors never arise from a resource
                    // lookup — means the registry is not available. Surface that
                    // rather than silently falling back to the default key, which
                    // resolves through the same missing registry.
                    Err(_) => {
                        return Err(ExecutionError::DynamicRegistryMissing {
                            node: node_id.clone(),
                            name: node.name.clone(),
                        });
                    }
                };
                // Soundness: each candidate was checked against the registry's
                // own contract at `register()` time, so it is only sound to run
                // here if that contract fills this node's slot. Refuse rather
                // than execute a candidate the parent graph never validated
                // against. This compares two *contracts* for identity, so it
                // stays exact-set (`compatible_with`) whatever `MatchingMode` mode
                // either side admits its candidates under: a registry whose
                // contract differs is a different interface, not a laxer one.
                // The common, compatible case runs on every execution, so use
                // the allocation-free `compatible_with` check and only build
                // the `SignatureDiff` on the (error) mismatch path.
                if !registry.contract().compatible_with(&node.contract) {
                    return Err(ExecutionError::DynamicContractMismatch {
                        node: node_id.clone(),
                        name: node.name.clone(),
                        diff: Box::new(registry.contract().diff(&node.contract)),
                    });
                }
                Ok(registry.get(key))
            }
        }
    }

    /// Populates a child context with the resource values produced by each
    /// per-resource crossing on the policy.
    ///
    /// `Share` produces no child-local entry — it is reflected in the child's
    /// parent filter. Excludes are tracked separately on the policy and don't
    /// appear here.
    ///
    /// `Forward`/`Fork` return [`ExecutionError::ScopeMissingResource`] when
    /// the parent has no local resource of the requested type, or
    /// [`ExecutionError::ScopeResourceBusy`] when the resource is present but
    /// currently held mutably (write-locked) and so cannot be copied. This
    /// mirrors the `ForwardFresh` behavior for missing factories. Validation
    /// ([`GraphExecutor::validate_resources`]) catches the static cases —
    /// this is the runtime safety net.
    ///
    /// [`GraphExecutor::validate_resources`]: super::GraphExecutor::validate_resources
    fn populate_child_locals(
        policy: &ContextPolicy,
        name: &NodeName,
        parent: &SystemContext<'_>,
        child: &mut SystemContext<'_>,
    ) -> Result<(), ExecutionError> {
        for crossing in policy.crossings() {
            match &crossing.action {
                CrossingAction::Share => {}
                CrossingAction::Forward(clone_fn) | CrossingAction::Fork(clone_fn) => {
                    let action = if matches!(crossing.action, CrossingAction::Fork(_)) {
                        "fork"
                    } else {
                        "forward"
                    };
                    let value = match parent.clone_local_resource_with(crossing.type_id, *clone_fn)
                    {
                        Some(value) => value,
                        // The clone failed. Distinguish a genuinely absent
                        // resource from one that is present but write-locked
                        // so the error names the real cause.
                        None if parent.contains_local_resource_by_type_id(crossing.type_id) => {
                            return Err(ExecutionError::ScopeResourceBusy {
                                scope: name.clone(),
                                resource: crossing.type_name,
                                action,
                            });
                        }
                        None => {
                            return Err(ExecutionError::ScopeMissingResource {
                                scope: name.clone(),
                                resource: crossing.type_name,
                                action,
                            });
                        }
                    };
                    child.insert_boxed(crossing.type_id, value);
                }
                CrossingAction::ForwardFresh => {
                    let factory =
                        parent
                            .factory_fn_by_type_id(crossing.type_id)
                            .ok_or_else(|| ExecutionError::ScopeMissingFactory {
                                scope: name.clone(),
                                resource: crossing.type_name,
                            })?;
                    child.insert_boxed_with_factory(crossing.type_id, factory.produce(), factory);
                }
            }
        }
        Ok(())
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
        run_ctx: &'a RunContext,
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

                // Check point 1: decline to start this node. Placed after the
                // lookup so a structural fault is still reported as itself,
                // and before the count so a node that never ran is excluded
                // from it.
                if run_ctx.is_cancelled() {
                    return Err(run_ctx.interrupted(&current, nodes_executed));
                }

                nodes_executed += 1;

                let (extra, next) = match node {
                    Node::System(sys) => {
                        let sys_info = middleware::info::SystemInfo {
                            node_id: current.clone(),
                            node_name: sys.node_name.clone(),
                        };
                        let result = middleware
                            .inner
                            .system
                            .execute(sys_info, ctx, |ctx| {
                                self.run_system_body(graph, sys, &current, hooks, ctx, run_ctx)
                            })
                            .await;
                        // Check point 2. `route_system_result` still *selects*
                        // the error or timeout handler; the loop then declines
                        // to start it at check point 1 on the next pass. When
                        // there is no such edge the failure propagates here and
                        // an interrupt supersedes it, so a system that returned
                        // `Err` because it noticed the signal ends the run as
                        // interrupted rather than as a system failure.
                        let routed = self
                            .route_system_result(graph, &current, result)
                            .map_err(|err| run_ctx.supersede(&current, nodes_executed, err))?;
                        (0, routed)
                    }
                    Node::Decision(dec) => {
                        let decision_id = current.clone();
                        let dec_info = middleware::info::DecisionInfo {
                            node_id: decision_id.clone(),
                            node_name: dec.name.clone(),
                        };

                        let branch_count = middleware
                            .inner
                            .decision
                            .execute(dec_info, ctx, |ctx| {
                                self.run_decision_body(
                                    dec,
                                    &decision_id,
                                    graph,
                                    depth,
                                    hooks,
                                    middleware,
                                    ctx,
                                    run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&decision_id, nodes_executed, err))?;
                        (branch_count, self.advance(graph, &decision_id)?)
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
                            node_name: loop_node.name.clone(),
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
                                    run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&current, nodes_executed, err))?;
                        (loop_count, self.advance(graph, &current)?)
                    }
                    Node::Parallel(par) => {
                        let par_info = middleware::info::ParallelInfo {
                            node_id: current.clone(),
                            node_name: par.name.clone(),
                            branch_count: par.branches.len(),
                        };
                        let parallel_count = middleware
                            .inner
                            .parallel_node
                            .execute(par_info, ctx, |ctx| {
                                self.execute_parallel(
                                    graph,
                                    ctx,
                                    par,
                                    depth + 1,
                                    hooks,
                                    middleware,
                                    run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&current, nodes_executed, err))?;
                        (parallel_count, self.advance(graph, &current)?)
                    }
                    Node::Switch(switch_node) => {
                        let switch_info = middleware::info::SwitchInfo {
                            node_id: current.clone(),
                            node_name: switch_node.name.clone(),
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
                                    run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&current, nodes_executed, err))?;
                        (switch_count, self.advance(graph, &current)?)
                    }
                    Node::Scope(scope) => {
                        let scope_id = current.clone();
                        let scope_info = middleware::info::ScopeInfo {
                            node_id: scope_id.clone(),
                            node_name: scope.name.clone(),
                            mode: scope.context_policy.mode(),
                            inner_node_count: scope.graph.node_count(),
                        };
                        let node_count = middleware
                            .inner
                            .scope
                            .execute(scope_info, ctx, |ctx| {
                                self.run_scope_body(
                                    scope, &scope_id, depth, hooks, middleware, ctx, run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&scope_id, nodes_executed, err))?;
                        (node_count, self.advance(graph, &current)?)
                    }
                    Node::Dynamic(dynamic) => {
                        let dynamic_id = current.clone();
                        let candidate_count = match &dynamic.source {
                            CandidateSource::Inline(candidates) => Some(candidates.len()),
                            CandidateSource::Registry { .. } => None,
                        };
                        let dynamic_info = middleware::info::DynamicInfo {
                            node_id: dynamic_id.clone(),
                            node_name: dynamic.name.clone(),
                            mode: dynamic.context_policy.mode(),
                            candidate_count,
                        };
                        let node_count = middleware
                            .inner
                            .dynamic
                            .execute(dynamic_info, ctx, |ctx| {
                                self.run_dynamic_body(
                                    dynamic,
                                    &dynamic_id,
                                    depth,
                                    hooks,
                                    middleware,
                                    ctx,
                                    run_ctx,
                                )
                            })
                            .await
                            .map_err(|err| run_ctx.supersede(&dynamic_id, nodes_executed, err))?;
                        (node_count, self.advance(graph, &current)?)
                    }
                };

                nodes_executed += extra;

                match next {
                    Some(node_id) => current = node_id,
                    None => break,
                }
            }

            Ok(nodes_executed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::{ErrorEdge, TimeoutEdge};
    use crate::graph::GraphSignature;
    use crate::hooks::events::{RunId, RunLabels};
    use crate::node::DynamicSlot;
    use crate::predicate::PredicateError;
    use crate::selector::{BoxedSelector, ErasedSelector, Selector};
    use std::any::TypeId;

    /// An empty-contract, registry-backed dynamic node driven by `selector`.
    /// The write-locked branch below cannot arise through the single-threaded
    /// executor, so the node is assembled by hand and `lookup_candidate` is
    /// exercised directly.
    fn registry_node(selector: BoxedSelector) -> DynamicNode {
        DynamicNode::new(
            "route",
            selector,
            CandidateSource::Registry {
                resource_type: TypeId::of::<SubgraphRegistry>(),
            },
            DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
        )
    }

    /// The `ValidationError::AmbiguousRoute` diagnostic (`kind:
    /// RouteKind::Sequential`) names the target of the *first* outgoing
    /// sequential edge as the successor execution follows, and every later one
    /// as skipped. That attribution holds only while traversal is first-match,
    /// so the premise is pinned here rather than left implicit: were this to
    /// become last-match — the natural change to make traversal agree with the
    /// last-edge-wins maps interface derivation builds — the diagnostic would
    /// confidently name the wrong successor with nothing else in the suite
    /// failing.
    #[test]
    fn find_next_sequential_follows_the_first_matching_edge() {
        async fn start() -> i32 {
            1
        }
        async fn earlier() -> String {
            "earlier".to_string()
        }
        async fn later() -> bool {
            true
        }

        let mut graph = Graph::new();
        let from = graph.add_system_node(start);
        let first_target = graph.add_system_node(earlier);
        let second_target = graph.add_system_node(later);
        // `second_target` was chained after `first_target`; give `from` a
        // second outgoing sequential edge so edge order is what decides.
        graph.add_sequential_edge(from.clone(), second_target.clone());

        let next = GraphExecutor::new()
            .find_next_sequential(&graph, &from)
            .expect("a node with an outgoing sequential edge has a next node");
        assert_eq!(
            next, first_target,
            "traversal is first-match: the earlier edge wins and the later edge is never \
             followed (got {next:?}, expected {first_target:?})",
        );
    }

    /// Same premise as [`find_next_sequential_follows_the_first_matching_edge`],
    /// for `RouteKind::Error`: `AmbiguousRoute`'s `followed`/`skipped`
    /// attribution for a node with two error edges is only correct while
    /// `find_error_edge` is first-match. Unlike the sequential shape, two
    /// error edges on one node *are* reachable through the public builder
    /// (`add_error_handler_for` called twice, or after a blanket
    /// `add_error_handler`), so a silent flip to last-match here would
    /// mislabel a real user's diagnostic, not just an internal invariant.
    #[test]
    fn find_error_edge_follows_the_first_matching_edge() {
        async fn start() -> i32 {
            1
        }
        async fn earlier() -> String {
            "earlier".to_string()
        }
        async fn later() -> bool {
            true
        }

        let mut graph = Graph::new();
        let from = graph.add_system_node(start);
        let first_target = graph.add_system_node(earlier);
        let second_target = graph.add_system_node(later);
        graph.edges.push(Edge::Error(ErrorEdge::new(
            from.clone(),
            first_target.clone(),
        )));
        graph
            .edges
            .push(Edge::Error(ErrorEdge::new(from.clone(), second_target)));

        let next = GraphExecutor::new()
            .find_error_edge(&graph, &from)
            .expect("a node with an outgoing error edge has a handler target");
        assert_eq!(
            next, first_target,
            "traversal is first-match: the earlier error edge wins (got {next:?}, expected \
             {first_target:?})",
        );
    }

    /// Same premise as [`find_next_sequential_follows_the_first_matching_edge`],
    /// for `RouteKind::Timeout`: see
    /// [`find_error_edge_follows_the_first_matching_edge`] — two timeout edges
    /// on one node are equally reachable through `add_timeout_handler` called
    /// twice.
    #[test]
    fn find_timeout_edge_follows_the_first_matching_edge() {
        async fn start() -> i32 {
            1
        }
        async fn earlier() -> String {
            "earlier".to_string()
        }
        async fn later() -> bool {
            true
        }

        let mut graph = Graph::new();
        let from = graph.add_system_node(start);
        let first_target = graph.add_system_node(earlier);
        let second_target = graph.add_system_node(later);
        graph.edges.push(Edge::Timeout(TimeoutEdge::new(
            from.clone(),
            first_target.clone(),
        )));
        graph
            .edges
            .push(Edge::Timeout(TimeoutEdge::new(from.clone(), second_target)));

        let next = GraphExecutor::new()
            .find_timeout_edge(&graph, &from)
            .expect("a node with an outgoing timeout edge has a handler target");
        assert_eq!(
            next, first_target,
            "traversal is first-match: the earlier timeout edge wins (got {next:?}, expected \
             {first_target:?})",
        );
    }

    #[test]
    fn lookup_candidate_reports_busy_when_registry_is_write_locked() {
        // A `SubgraphRegistry` held mutably (an in-flight `ResMut`) when
        // selection reads it resolves to `DynamicRegistryBusy`, not a misleading
        // key-miss. This write-locked-during-selection state cannot arise through
        // the single-threaded executor, so it is exercised directly against
        // `lookup_candidate`.
        let node = registry_node(Box::new(Selector::new(|_ctx| Arc::<str>::from("slot"))));
        let ctx = SystemContext::new().with(SubgraphRegistry::new(GraphSignature::new()));

        // Hold a mutable borrow of the registry across the lookup.
        let guard = ctx.get_resource_mut::<SubgraphRegistry>().unwrap();
        let err = GraphExecutor::lookup_candidate(&node, &node.id, &ctx, "slot")
            .expect_err("a write-locked registry must not resolve");
        assert!(
            matches!(&err, ExecutionError::DynamicRegistryBusy { name, .. } if *name == "route"),
            "expected DynamicRegistryBusy, got {err:?}"
        );
        drop(guard);
    }

    #[test]
    fn populate_child_locals_busy_and_missing_factory_name_the_dynamic_boundary() {
        // `populate_child_locals` labels boundary-crossing failures with the
        // node's name — a scope *or* a dynamic node. The forward/missing arm is
        // pinned through the public executor in
        // `tests/dynamic_node.rs::missing_forward_crossing_names_the_dynamic_boundary`;
        // the write-locked (`ScopeResourceBusy`) and missing-factory
        // (`ScopeMissingFactory`) arms cannot arise through the single-threaded
        // executor / a factory-less test context on that path, so they are
        // exercised directly with a dynamic node's name as the label.
        #[derive(Clone)]
        struct Crossed;
        impl polaris_system::resource::LocalResource for Crossed {}

        // Busy: the parent holds `Crossed` mutably while the crossing copies it.
        let forward_policy = ContextPolicy::new().forward::<Crossed>();
        let parent = SystemContext::new().with(Crossed);
        let guard = parent.get_resource_mut::<Crossed>().unwrap();
        let mut child = parent.child_filtered(forward_policy.parent_filter_arc());
        let route_name: NodeName = "route".into();
        let err =
            GraphExecutor::populate_child_locals(&forward_policy, &route_name, &parent, &mut child)
                .expect_err("a write-locked resource cannot be forwarded");
        assert!(
            matches!(
                &err,
                ExecutionError::ScopeResourceBusy {
                    scope,
                    action: "forward",
                    ..
                } if *scope == "route"
            ),
            "expected ScopeResourceBusy naming the dynamic node, got {err:?}"
        );
        drop(guard);

        // Missing factory: `forward_fresh` with no factory registered anywhere.
        let fresh_policy = ContextPolicy::new().forward_fresh::<Crossed>();
        let parent = SystemContext::new();
        let mut child = parent.child_filtered(fresh_policy.parent_filter_arc());
        let err =
            GraphExecutor::populate_child_locals(&fresh_policy, &route_name, &parent, &mut child)
                .expect_err("forward_fresh without a factory cannot populate the child");
        assert!(
            matches!(
                &err,
                ExecutionError::ScopeMissingFactory { scope, .. } if *scope == "route"
            ),
            "expected ScopeMissingFactory naming the dynamic node, got {err:?}"
        );
    }

    #[tokio::test]
    async fn dynamic_body_maps_selector_error_to_predicate_error() {
        // A fallible `ErasedSelector` returning `Err` surfaces as
        // `ExecutionError::PredicateError`. This pins the mapping at the unit
        // level; the public path through `add_dynamic_boxed` is covered by
        // `tests/dynamic_node.rs::boxed_selector_error_propagates_as_predicate_error`.
        struct FailingSelector;
        impl ErasedSelector for FailingSelector {
            fn select(&self, _ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
                Err(PredicateError::ContextError("selector failed".into()))
            }
            fn input_type_name(&self) -> &'static str {
                "FailingSelector"
            }
        }

        let node = registry_node(Box::new(FailingSelector));
        let mut ctx = SystemContext::new();
        let run_ctx = RunContext {
            run_id: RunId::new(),
            labels: RunLabels::empty(),
            cancellation: None,
            started_at: std::time::Instant::now(),
            loop_iteration: 0,
            output_path: crate::output_layers::WritePath::root(),
        };
        let middleware = MiddlewareAPI::new();
        let executor = GraphExecutor::new();

        let err = executor
            .run_dynamic_body(&node, &node.id, 0, None, &middleware, &mut ctx, &run_ctx)
            .await
            .expect_err("a selector error must abort the dynamic node");
        assert!(
            matches!(
                &err,
                ExecutionError::PredicateError(PredicateError::ContextError(msg))
                    if msg == "selector failed"
            ),
            "expected PredicateError carrying the selector's message, got {err:?}"
        );
    }

    /// A raised token must not turn "your graph names a node that does not
    /// exist" into "interrupted". The dangling edge needs `pub(crate)` edge
    /// construction, so this lives here rather than in
    /// `tests/cancellation.rs`, which covers the publicly reachable structural
    /// faults.
    #[tokio::test]
    async fn a_raised_token_does_not_mask_a_node_not_found() {
        /// Raises the run's signal, so the next node is reached with the token
        /// already up.
        struct Raise(crate::cancel::CancellationSource);

        impl polaris_system::system::System for Raise {
            type Output = ();

            fn run<'a>(
                &'a self,
                _ctx: &'a SystemContext<'_>,
            ) -> polaris_system::system::BoxFuture<
                'a,
                Result<Self::Output, polaris_system::system::SystemError>,
            > {
                Box::pin(async move {
                    self.0.cancel();
                    Ok(())
                })
            }

            fn name(&self) -> &'static str {
                "raise"
            }
        }

        let source = crate::cancel::CancellationSource::new();

        let mut graph = Graph::new();
        let entry = graph.add_boxed_system(Box::new(Raise(source.clone())));
        let missing = NodeId::from_string("no_such_node");
        graph.add_sequential_edge(entry.clone(), missing.clone());

        let mut ctx = SystemContext::new();
        let run_ctx = RunContext {
            run_id: RunId::new(),
            labels: RunLabels::empty(),
            cancellation: Some(source.token()),
            started_at: std::time::Instant::now(),
            loop_iteration: 0,
            output_path: crate::output_layers::WritePath::root(),
        };
        let middleware = MiddlewareAPI::new();
        let executor = GraphExecutor::new();

        let err = executor
            .execute_from(&graph, &mut ctx, entry, 0, None, &middleware, &run_ctx)
            .await
            .expect_err("the dangling edge is a structural fault");

        assert!(source.is_cancelled(), "the token really is raised");
        assert!(
            matches!(&err, ExecutionError::NodeNotFound(id) if *id == missing),
            "a structural fault must be reported as itself, not as an interrupt; got {err:?}"
        );
    }
}
