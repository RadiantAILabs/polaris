//! Builder API for constructing graphs.

use super::{Graph, GraphSignature, MergeError};
use crate::edge::{Edge, ErrorEdge, TimeoutEdge};
use crate::executor::CaughtError;
use crate::node::{
    CandidateSource, ContextPolicy, DecisionNode, DynamicNode, DynamicSlot, IntoSystemNode,
    LoopNode, Node, NodeId, NodeName, ParallelNode, RetryPolicy, ScopeNode, SwitchNode, SystemNode,
};
use crate::predicate::{BoxedDiscriminator, BoxedPredicate, Discriminator, Predicate};
use crate::registry::SubgraphRegistry;
use crate::selector::{BoxedSelector, Selector};
use hashbrown::HashSet;
use polaris_system::param::{ERROR_CONTEXT, SystemAccess, SystemContext};
use polaris_system::plugin::{IntoScheduleIds, ScheduleId};
use polaris_system::resource::Output;
use polaris_system::system::{BoxFuture, IntoBoxedSystem, System, SystemError};
use std::any::TypeId;
use std::sync::Arc;
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────────
// Closure-based error handler (internal implementation detail)
// ─────────────────────────────────────────────────────────────────────────────

/// Wraps a closure as a [`System`] for use as an error handler.
///
/// The closure receives the [`CaughtError`] stored by the executor when a
/// system fails and returns a value of type `T`, which becomes the system
/// output available to subsequent nodes via `Out<T>`.
struct ClosureErrorHandler<T, F> {
    handler: F,
    _marker: std::marker::PhantomData<T>,
}

impl<T, F> System for ClosureErrorHandler<T, F>
where
    T: Output,
    F: Fn(&CaughtError) -> T + Send + Sync + 'static,
{
    type Output = T;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let caught = ctx
                .get_output::<CaughtError>()
                .map_err(SystemError::ParamError)?;
            Ok((self.handler)(&caught))
        })
    }

    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.require_context(ERROR_CONTEXT);
        access
    }
}

impl Graph {
    // ─────────────────────────────────────────────────────────────────────────
    // Graph Configuration
    // ─────────────────────────────────────────────────────────────────────────

    /// Sets the maximum total execution duration for this graph.
    ///
    /// When set, the executor wraps this graph's execution in a timeout.
    /// If exceeded, returns [`ExecutionError::GraphTimeout`](crate::executor::ExecutionError::GraphTimeout)
    /// after invoking `OnGraphFailure` hooks. For scope-embedded graphs,
    /// `OnGraphFailure` fires on the **parent** graph (not a scope-specific
    /// event) and `OnScopeComplete` is skipped.
    ///
    /// This takes precedence over the executor's own
    /// [`with_max_duration`](crate::executor::GraphExecutor::with_max_duration).
    /// If the graph does not set a timeout, the executor's value is used as a
    /// fallback. Note that `GraphExecutor::with_max_duration` uses a consuming
    /// builder (`self -> Self`) while this method uses `&mut self -> &mut Self`
    /// to match the `Graph` builder convention.
    ///
    /// # Cancel safety
    ///
    /// Graph-level timeout is a hard abort. When it fires mid-system, tokio
    /// drops the currently-running future at its next `.await` point:
    ///
    /// - Rust `Drop` impls run, but `async` code after the cancellation
    ///   point does not execute.
    /// - Writes already made to `SystemContext` persist; later writes are
    ///   lost.
    /// - External side effects may have committed remotely but gone
    ///   unacknowledged.
    /// - The cancelled system's `on_error` / `on_timeout` edges are **not**
    ///   invoked — graph-level timeout bypasses node dispatch.
    /// - For scope graphs using [`ContextPolicy::shared()`](crate::node::ContextPolicy::shared),
    ///   the parent context may retain partial writes from systems that ran
    ///   before the timeout fired.
    ///
    /// This is distinct from **per-node** timeout, which integrates with
    /// retry policies and timeout handler edges for structured recovery.
    /// Per-node timeouts are set via [`SystemNodeBuilder::with_timeout`]
    /// (fluent builder) or [`Graph::set_timeout`] (by node ID).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use std::time::Duration;
    /// # async fn step_a() {}
    /// # async fn step_b() {}
    /// let mut graph = Graph::new();
    /// graph
    ///     .with_max_duration(Duration::from_secs(30))
    ///     .add_system(step_a)
    ///     .add_system(step_b);
    /// ```
    pub fn with_max_duration(&mut self, duration: Duration) -> &mut Self {
        self.max_duration = Some(duration);
        self
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Builder API
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a system node to the graph.
    ///
    /// The system is connected sequentially to the previous node (if any).
    /// If this is the first node, it becomes the entry point.
    ///
    /// # Custom schedules
    ///
    /// Custom [`Schedule`](polaris_system::plugin::Schedule) types can be attached to a system by passing a
    /// `(custom_schedules, system)` tuple. System lifecycle events are then
    /// re-emitted on those schedules, allowing hooks to subscribe to events
    /// for this system only.
    ///
    /// # Type Parameters
    ///
    /// * `S` - Any type implementing [`IntoSystemNode`] (a system or schedule+system tuple)
    /// * `M` - Marker type for trait dispatch
    ///
    /// # Returns
    ///
    /// The node ID of the newly added system node.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// async fn my_system() {}
    ///
    /// let mut graph = Graph::new();
    /// let id = graph.add_system_node(my_system);
    /// ```
    pub fn add_system_node<S, M>(&mut self, system: S) -> NodeId
    where
        S: IntoSystemNode<M>,
    {
        let (boxed_system, schedules) = system.into_system_node();
        let node = Node::System(SystemNode::new_boxed(boxed_system).with_schedules(schedules));
        let id = node.id();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(id.clone());
        }

        self.nodes.push(node);
        self.last_node = Some(id.clone());
        id
    }

    /// Adds a system node and returns self for chaining.
    ///
    /// This is the preferred builder method for fluent API usage.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// async fn step_a() {}
    /// async fn step_b() {}
    ///
    /// let mut graph = Graph::new();
    /// graph
    ///     .add_system(step_a)
    ///     .add_system(step_b);
    /// ```
    pub fn add_system<S, M>(&mut self, system: S) -> &mut Self
    where
        S: IntoSystemNode<M>,
    {
        self.add_system_node(system);
        self
    }

    /// Adds a system node and returns a builder for configuring error/timeout handlers.
    ///
    /// The node is added immediately (connected to the previous node).
    /// The builder allows attaching handlers before moving on.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use core::time::Duration;
    /// # async fn risky_operation() {}
    /// # async fn fallback() {}
    /// # async fn timeout_handler() {}
    /// # async fn next_step() {}
    /// # let mut graph = Graph::new();
    /// graph.system(risky_operation)
    ///     .on_error(|h: &mut Graph| { h.add_system(fallback); })
    ///     .with_timeout(Duration::from_secs(30))
    ///     .on_timeout(|h: &mut Graph| { h.add_system(timeout_handler); })
    ///     .done()
    ///     .add_system(next_step);
    /// ```
    pub fn system<S, M>(&mut self, system: S) -> SystemNodeBuilder<'_>
    where
        S: IntoSystemNode<M>,
    {
        let node_id = self.add_system_node(system);
        SystemNodeBuilder {
            graph: self,
            node_id,
        }
    }

    /// Adds a boxed or typed-erased system node directly.
    ///
    /// Accepts [`BoxedSystem`](polaris_system::system::BoxedSystem), boxed
    /// custom [`System`] implementations, and
    /// [`TypedSystem<O>`](polaris_system::system::TypedSystem) handles produced
    /// by [`erase`](polaris_system::system::erase).
    ///
    /// Prefer this over [`system_boxed`](Self::system_boxed) when a composition
    /// helper needs the `NodeId` to wire edges itself — for example reading
    /// [`TypedSystem::is_fallible`](polaris_system::system::TypedSystem::is_fallible)
    /// and attaching an error handler for the returned node.
    ///
    /// # Returns
    ///
    /// The node ID of the newly added system node.
    pub fn add_boxed_system(&mut self, system: impl IntoBoxedSystem) -> NodeId {
        let node = Node::System(SystemNode::new_boxed(system));
        let id = node.id();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(id.clone());
        }

        self.nodes.push(node);
        self.last_node = Some(id.clone());
        id
    }

    /// Adds a boxed or typed-erased system node and returns its builder.
    ///
    /// This is the builder equivalent of
    /// [`add_boxed_system`](Self::add_boxed_system), useful for boxed custom
    /// [`System`] implementations and typed-erased composition stages. The returned
    /// builder can attach schedules, error and timeout handlers, execution
    /// timeouts, and retry policies.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_system::system::{BoxedSystem, IntoSystem};
    /// # async fn my_custom_system() {}
    /// # async fn fallback() {}
    /// # async fn next_step() {}
    /// # let mut graph = Graph::new();
    /// let boxed: BoxedSystem = Box::new(my_custom_system.into_system());
    /// graph.system_boxed(boxed)
    ///     .on_error(|g: &mut Graph| { g.add_system(fallback); })
    ///     .done()
    ///     .add_system(next_step);
    /// ```
    ///
    /// A composition helper receives its stage as a
    /// [`TypedSystem<O>`](polaris_system::system::TypedSystem) — naming the
    /// output it requires without a system or marker type parameter — and
    /// attaches its own hook schedule here, since a handle cannot use the
    /// `(schedule, system)` tuple form:
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_system::plugin::Schedule;
    /// # use polaris_system::system::{TypedSystem, erase};
    /// # async fn predict() -> i32 { 7 }
    /// struct OnStage;
    /// impl Schedule for OnStage {}
    ///
    /// fn add_stage(graph: &mut Graph, stage: TypedSystem<i32>) {
    ///     graph.system_boxed(stage)
    ///         .with_schedules(OnStage)
    ///         .done();
    /// }
    ///
    /// let mut graph = Graph::new();
    /// add_stage(&mut graph, erase(predict));
    /// ```
    pub fn system_boxed(&mut self, system: impl IntoBoxedSystem) -> SystemNodeBuilder<'_> {
        let node_id = self.add_boxed_system(system);
        SystemNodeBuilder {
            graph: self,
            node_id,
        }
    }

    /// Passes the graph through a builder function for fluent composition.
    ///
    /// The closure receives `&mut self` directly — not a fresh subgraph.
    /// All modifications apply to the current graph and update `last_node`
    /// tracking, so the next chained call continues from wherever the
    /// closure left off.
    ///
    /// Use `pipe` to compose reusable graph fragments inline. For subgraph
    /// construction (branches, loops, error handlers), use the dedicated
    /// builder methods instead.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn fetch_data() { }
    /// # async fn log_metrics() { }
    /// # async fn check_health() { }
    /// # async fn respond() { }
    /// # let mut graph = Graph::new();
    /// fn monitoring(g: &mut Graph) {
    ///     g.add_system(log_metrics)
    ///      .add_system(check_health);
    /// }
    ///
    /// graph
    ///     .add_system(fetch_data)
    ///     .pipe(monitoring)
    ///     .add_system(respond);
    /// ```
    pub fn pipe<F>(&mut self, f: F) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        f(self);
        self
    }

    /// Binds `reader`'s `Out<T>` (or `Option<Out<T>>`) read of `T` to
    /// `producer`'s entries, instead of the newest write of `T` from any
    /// producer.
    ///
    /// The primitive behind [`SystemNodeBuilder::read_from`], which is the
    /// more ergonomic call for the common case of wiring a system's read to a
    /// producer that already exists. Call this directly instead when the
    /// producer's id is not yet available at the point the reader is added —
    /// notably a loop's backward wire, where the producer
    /// is added *after* the reader in the same body, since the reader reads
    /// its *previous* iteration's value:
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_system::param::Out;
    /// # #[derive(Clone, Copy)] struct Counter(u32);
    /// # async fn seed() -> Counter { Counter(0) }
    /// # #[polaris_system::system]
    /// # async fn remember(previous: Option<Out<Counter>>) -> i32 { previous.map_or(-1, |c| c.0 as i32) }
    /// # #[polaris_system::system]
    /// # async fn advance(counter: Out<Counter>) -> Counter { Counter(counter.0 + 1) }
    /// let mut graph = Graph::new();
    /// graph.add_system(seed);
    /// graph.add_loop_n("loop", 3, |g| {
    ///     let remember_id = g.system(remember).id();
    ///     let advance_id = g.system(advance).id();
    ///     g.read_from::<Counter>(remember_id, advance_id);
    /// });
    /// assert!(graph.validate().is_ok());
    /// ```
    ///
    /// A second `read_from::<T>` call for the same `reader` and `T` replaces
    /// the earlier binding.
    pub fn read_from<T: Output>(&mut self, reader: NodeId, producer: NodeId) -> &mut Self {
        self.output_wiring.bind(
            reader,
            TypeId::of::<T>(),
            producer,
            std::any::type_name::<T>(),
        );
        self
    }

    /// Sequentially appends another graph, connecting `self`'s last node
    /// to `other`'s entry with a sequential edge.
    ///
    /// Both graphs are checked for well-formedness before merging:
    /// - Each non-empty graph must have an entry point and an exit point
    ///   (last node).
    /// - All nodes in each graph must be reachable from its entry.
    ///
    /// After a successful append, `self.last_node` is updated to
    /// `other`'s last node so further chaining continues from the end
    /// of the appended graph.
    ///
    /// `max_duration` from `other` is **not** preserved — the receiving
    /// graph's timeout policy takes precedence. If `other` has a
    /// `max_duration` set, a warning is logged.
    ///
    /// Appending an empty graph is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`MergeError`] if either graph fails pre-merge checks.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn fetch_data() { }
    /// # async fn log_metrics() { }
    /// # async fn check_health() { }
    /// # async fn respond() { }
    /// # fn main() -> Result<(), polaris_graph::MergeError> {
    /// # let mut graph = Graph::new();
    /// let mut monitoring = Graph::new();
    /// monitoring
    ///     .add_system(log_metrics)
    ///     .add_system(check_health);
    ///
    /// graph
    ///     .add_system(fetch_data)
    ///     .append(monitoring)?
    ///     .add_system(respond);
    /// # Ok(())
    /// # }
    /// ```
    pub fn append(&mut self, other: Graph) -> Result<&mut Self, MergeError> {
        if other.is_empty() {
            return Ok(self);
        }

        let other_entry = other.entry.clone().ok_or(MergeError::NoEntry)?;
        let other_exit = other.last_node.clone().ok_or(MergeError::NoExit)?;

        // Connectivity: all nodes in `other` reachable from its entry.
        other.check_connectivity(&other_entry)?;

        if self.is_empty() {
            // Self is empty — adopt other wholesale.
            self.entry = Some(other_entry);
        } else {
            let self_entry = self.entry.clone().ok_or(MergeError::NoEntry)?;
            let self_exit = self.last_node.clone().ok_or(MergeError::NoExit)?;

            // Connectivity: all nodes in `self` reachable from its entry.
            self.check_connectivity(&self_entry)?;

            self.add_sequential_edge(self_exit, other_entry);
        }

        if other.max_duration.is_some() && self.max_duration.is_none() {
            tracing::warn!(
                "appended graph has max_duration set but the receiving graph does not — \
                 the appended graph's timeout will be discarded"
            );
        }

        self.absorb(other);
        self.last_node = Some(other_exit);

        Ok(self)
    }

    /// Adds a decision node for binary branching with a typed predicate.
    ///
    /// The predicate evaluates the output of a previous system and determines
    /// which branch to take. If the predicate returns `true`, the `true_path`
    /// is executed; otherwise, the `false_path` is executed.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The output type to evaluate (must be an `Output` from a previous system)
    /// * `P` - The predicate closure type
    /// * `F1` - Builder function type for the true branch
    /// * `F2` - Builder function type for the false branch
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the decision node
    /// * `predicate` - Closure that receives `&T` and returns `bool`
    /// * `true_path` - Builder function for the true branch
    /// * `false_path` - Builder function for the false branch
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # #[derive(PartialEq)] enum Action { UseTool }
    /// # struct ReasoningResult { action: Action }
    /// # async fn use_tool() -> i32 { 1 }
    /// # async fn respond() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// graph.add_conditional_branch::<ReasoningResult, _, _, _>(
    ///     "needs_tool",
    ///     |result| result.action == Action::UseTool,
    ///     |g| { g.add_system(use_tool); },
    ///     |g| { g.add_system(respond); },
    /// );
    /// ```
    pub fn add_conditional_branch<T, P, F1, F2>(
        &mut self,
        name: impl Into<NodeName>,
        predicate: P,
        true_path: F1,
        false_path: F2,
    ) -> &mut Self
    where
        T: Output,
        P: Fn(&T) -> bool + Send + Sync + 'static,
        F1: FnOnce(&mut Graph),
        F2: FnOnce(&mut Graph),
    {
        let boxed_predicate: BoxedPredicate = Predicate::<T, P>::new(predicate).into();
        self.add_conditional_branch_boxed(name, boxed_predicate, true_path, false_path)
    }

    /// Adds a decision node for binary branching with an already-boxed predicate.
    ///
    /// The type-erased counterpart of
    /// [`add_conditional_branch`](Self::add_conditional_branch) — build the
    /// [`BoxedPredicate`] with [`BoxedPredicate::new`] when the input type is
    /// only known at runtime (for example, assembling a graph from a JSON
    /// plan over a registry of pre-registered predicates), without naming the
    /// input type at the call site.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::predicate::BoxedPredicate;
    /// # struct ReasoningResult { needs_tool: bool }
    /// # async fn use_tool() -> i32 { 1 }
    /// # async fn respond() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// let predicate = BoxedPredicate::new::<ReasoningResult>(|result| result.needs_tool);
    /// graph.add_conditional_branch_boxed(
    ///     "needs_tool",
    ///     predicate,
    ///     |g| { g.add_system(use_tool); },
    ///     |g| { g.add_system(respond); },
    /// );
    /// ```
    pub fn add_conditional_branch_boxed<F1, F2>(
        &mut self,
        name: impl Into<NodeName>,
        predicate: BoxedPredicate,
        true_path: F1,
        false_path: F2,
    ) -> &mut Self
    where
        F1: FnOnce(&mut Graph),
        F2: FnOnce(&mut Graph),
    {
        let mut decision = DecisionNode::with_predicate(name, predicate);
        let decision_id = decision.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, decision_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(decision_id.clone());
        }

        // Build true branch
        let mut true_graph = Graph::new();
        true_path(&mut true_graph);

        if let Some(entry) = true_graph.entry.take() {
            decision.true_branch = Some(entry);
        }

        // Build false branch
        let mut false_graph = Graph::new();
        false_path(&mut false_graph);

        if let Some(entry) = false_graph.entry.take() {
            decision.false_branch = Some(entry);
        }

        // Add decision node
        self.nodes.push(Node::Decision(decision));

        // Merge branch graphs into main graph
        self.absorb(true_graph);
        self.absorb(false_graph);

        // Decision node becomes the last node (branches may rejoin later)
        self.last_node = Some(decision_id);

        self
    }

    /// Adds a parallel execution node.
    ///
    /// Every branch runs and merges its outputs back in branch order, so what
    /// the branches jointly guarantee is credited to the chain after this node
    /// — a fan-out may seed a downstream `Out<T>` read or loop without the
    /// parent supplying it. A branch cannot be satisfied by a sibling, and
    /// production behind a handler edge inside a branch is not credited. See
    /// [Graph — Parallel Execution](../../docs/reference/graph.md#parallel-execution).
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the parallel node.
    /// * `branches` - Builder functions for each parallel branch.
    pub fn add_parallel<I, F>(&mut self, name: impl Into<NodeName>, branches: I) -> &mut Self
    where
        I: IntoIterator<Item = F>,
        F: FnOnce(&mut Graph),
    {
        let mut parallel = ParallelNode::new(name);
        let parallel_id = parallel.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, parallel_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(parallel_id.clone());
        }

        // Build each branch
        for branch_fn in branches {
            let mut branch_graph = Graph::new();
            branch_fn(&mut branch_graph);

            if let Some(entry) = branch_graph.entry.take() {
                parallel.branches.push(entry);
            }

            // Merge branch graph
            self.absorb(branch_graph);
        }

        // Add parallel node (it serves as both entry and exit)
        self.nodes.push(Node::Parallel(parallel));
        self.last_node = Some(parallel_id);

        self
    }

    /// Adds a loop node with a typed termination predicate.
    ///
    /// The loop body executes repeatedly until the termination predicate
    /// returns `true`. The predicate evaluates the output of a system
    /// within the loop body.
    ///
    /// The predicate is evaluated **before each iteration, including the
    /// first** — before the body has ever run. Its input must therefore
    /// exist when the loop is reached: produce it earlier in the graph (any
    /// node whose outputs land before the loop counts — most simply a
    /// preceding init system). Outputs are the work products of systems, so
    /// production is the normative source; a caller driving the graph
    /// directly may instead pre-seed the context via
    /// [`SystemContext::insert_output`](polaris_system::param::SystemContext::insert_output)
    /// before executing. If no earlier node can produce it and the context
    /// lacks it, the executor fails at run start with
    /// [`ExecutionError::LoopPredicateInputMissingOnEntry`](crate::executor::ExecutionError::LoopPredicateInputMissingOnEntry).
    /// The derived [`Graph::signature`] lists the input in `requires_outputs`
    /// unless production is *guaranteed* earlier on the chain — by a preceding
    /// system, a parallel node (every branch runs and merges back), or a scope
    /// (inner outputs merge back on exit). A decision or switch case, a loop
    /// body, and a dynamic candidate guarantee nothing, so a seed produced
    /// only there is still advertised.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The output type to evaluate for termination
    /// * `P` - The termination predicate closure type
    /// * `F` - Builder function type for the loop body
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the loop node
    /// * `termination` - Closure that receives `&T` and returns `true` to exit
    /// * `body` - Builder function for the loop body
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # struct LoopState { is_done: bool, iterations: usize }
    /// # async fn init_state() -> LoopState { LoopState { is_done: false, iterations: 0 } }
    /// # async fn reason() -> LoopState { LoopState { is_done: false, iterations: 0 } }
    /// # async fn act() -> i32 { 1 }
    /// # async fn observe() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// // The first termination check reads `LoopState` before the body runs.
    /// graph.add_system(init_state);
    /// graph.add_loop::<LoopState, _, _>(
    ///     "react_loop",
    ///     |state| state.is_done || state.iterations >= 10,
    ///     |g| {
    ///         g.add_system(reason)
    ///          .add_system(act)
    ///          .add_system(observe);
    ///     },
    /// );
    /// ```
    pub fn add_loop<T, P, F>(
        &mut self,
        name: impl Into<NodeName>,
        termination: P,
        body: F,
    ) -> &mut Self
    where
        T: Output,
        P: Fn(&T) -> bool + Send + Sync + 'static,
        F: FnOnce(&mut Graph),
    {
        let boxed_termination: BoxedPredicate = Predicate::<T, P>::new(termination).into();
        self.add_loop_boxed(name, boxed_termination, body)
    }

    /// Adds a loop node with an already-boxed termination predicate.
    ///
    /// The type-erased counterpart of [`add_loop`](Self::add_loop) — build
    /// the [`BoxedPredicate`] with [`BoxedPredicate::new`] when the input
    /// type is only known at runtime, without naming it at the call site.
    /// The loop-entry check (the predicate's input type must be guaranteed
    /// before the loop is reached, or the executor fails at run start with
    /// [`ExecutionError::LoopPredicateInputMissingOnEntry`](crate::executor::ExecutionError::LoopPredicateInputMissingOnEntry))
    /// still runs, driven by
    /// [`ErasedPredicate::input_type_id`](crate::predicate::ErasedPredicate::input_type_id)
    /// on the boxed predicate exactly as it does for the typed [`add_loop`](Self::add_loop).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::predicate::BoxedPredicate;
    /// # struct LoopState { is_done: bool }
    /// # async fn init_state() -> LoopState { LoopState { is_done: false } }
    /// # async fn act() -> i32 { 1 }
    /// # let mut graph = Graph::new();
    /// let termination = BoxedPredicate::new::<LoopState>(|state| state.is_done);
    /// graph.add_system(init_state);
    /// graph.add_loop_boxed("react_loop", termination, |g| {
    ///     g.add_system(act);
    /// });
    /// ```
    pub fn add_loop_boxed<F>(
        &mut self,
        name: impl Into<NodeName>,
        termination: BoxedPredicate,
        body: F,
    ) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        let mut loop_node = LoopNode::with_termination(name, termination);
        let loop_id = loop_node.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, loop_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(loop_id.clone());
        }

        // Build loop body
        let mut body_graph = Graph::new();
        body(&mut body_graph);

        if let Some(entry) = body_graph.entry.take() {
            loop_node.body_entry = Some(entry);
        }

        // Merge body graph
        self.absorb(body_graph);

        // Add loop node
        self.nodes.push(Node::Loop(loop_node));

        // Loop node becomes the last node
        self.last_node = Some(loop_id);

        self
    }

    /// Adds a loop node with a maximum iteration count.
    ///
    /// The loop body executes up to `max_iterations` times. Use this
    /// when you want a simple bounded loop without a predicate.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the loop node
    /// * `max_iterations` - Maximum number of iterations
    /// * `body` - Builder function for the loop body
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn attempt_operation() -> i32 { 1 }
    /// # let mut graph = Graph::new();
    /// graph.add_loop_n("retry_loop", 3, |g| {
    ///     g.add_system(attempt_operation);
    /// });
    /// ```
    pub fn add_loop_n<F>(
        &mut self,
        name: impl Into<NodeName>,
        max_iterations: usize,
        body: F,
    ) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        let mut loop_node = LoopNode::with_max_iterations(name, max_iterations);
        let loop_id = loop_node.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, loop_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(loop_id.clone());
        }

        // Build loop body
        let mut body_graph = Graph::new();
        body(&mut body_graph);

        if let Some(entry) = body_graph.entry.take() {
            loop_node.body_entry = Some(entry);
        }

        // Merge body graph
        self.absorb(body_graph);

        // Add loop node
        self.nodes.push(Node::Loop(loop_node));

        // Loop node becomes the last node
        self.last_node = Some(loop_id);

        self
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Scope API
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a scope node containing an embedded graph.
    ///
    /// The scope node executes the embedded graph as a single opaque unit.
    /// The [`ContextPolicy`] controls context sharing between parent and child.
    ///
    /// Unlike decision/loop/parallel nodes, the embedded graph's nodes are NOT
    /// merged into the parent graph. The scope node holds the graph as a field.
    ///
    /// "Opaque" governs node merging and what the interior *needs*, not what it
    /// deposits: inner outputs merge back on exit whatever the policy, so the
    /// interior's guaranteed production is credited to the enclosing chain and
    /// a scope may seed a downstream `Out<T>` read or loop. Production behind a
    /// handler edge inside the scope is not credited. See
    /// [Graph — Scope](../../docs/reference/graph.md#scope).
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the scope node
    /// * `graph` - The embedded graph to execute
    /// * `policy` - Context sharing policy
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::node::ContextPolicy;
    /// # async fn gather_info() -> String { String::new() }
    /// # async fn summarize() -> String { String::new() }
    /// // Build an inner graph for the sub-agent
    /// let mut research = Graph::new();
    /// research.add_system(gather_info).add_system(summarize);
    ///
    /// // Embed it as a scope with inherited context
    /// let mut graph = Graph::new();
    /// graph.add_scope("research", research, ContextPolicy::new().share_rest());
    /// ```
    pub fn add_scope(
        &mut self,
        name: impl Into<NodeName>,
        graph: Graph,
        policy: ContextPolicy,
    ) -> &mut Self {
        self.push_scope(ScopeNode::new(name, graph, policy))
    }

    /// Adds a scope node that carries a declared [`GraphSignature`] contract.
    ///
    /// The contract is the interface the scope presents to this graph, and it
    /// replaces the opaque treatment a plain [`add_scope`](Self::add_scope)
    /// gets in [`signature`](Graph::signature) derivation: the contract's
    /// `requires` and `requires_outputs` are classified against what is
    /// guaranteed upstream and surfaced in this graph's signature otherwise,
    /// and its `produces` are credited as guaranteed production for downstream
    /// reads. Because the boundary's IO is fully declared, a graph embedding a
    /// contract-declared scope is admissible as a
    /// [`Dynamic`](Node::Dynamic) candidate, where a plain scope is not.
    ///
    /// It is also a **sealed** boundary at runtime, not just at validate time:
    /// on a non-[`shared()`](ContextPolicy::shared) `policy`, only the output
    /// types named in `produces` merge back when the scope completes — every
    /// other output the interior wrote, declared or not, is dropped with the
    /// child context, and a same-typed parent output survives untouched. A
    /// plain [`add_scope`](Self::add_scope) keeps merging every interior
    /// output unfiltered, contract or no contract. See
    /// [`ScopeNode`]'s *Declared contracts* docs for the `shared()` exception,
    /// where there is no child context to filter.
    ///
    /// The declaration cannot drift from the interior:
    /// [`validate`](Graph::validate) checks the embedded graph's `requires`
    /// and `requires_outputs` against the contract with the same exact-match
    /// relation [`SubgraphRegistry`] admission uses, and rejects a contract
    /// requirement the `policy` blocks from crossing the boundary at runtime.
    /// `produces` is checked as a **subset** instead: every declared produce
    /// must be production the interior *guarantees* (crediting an
    /// unguaranteed one downstream would be unsound), but the interior may
    /// guarantee more than the contract declares — the extra stays private,
    /// filtered out by the runtime boundary above rather than reported as
    /// drift. The checked interface includes eager
    /// [`forward`](ContextPolicy::forward)/[`fork`](ContextPolicy::fork)
    /// inputs even if the interior never accesses them: declare a read for a
    /// crossing-only type, retaining existing interior read/write requirements.
    /// [`forward_fresh`](ContextPolicy::forward_fresh) is rejected for declared
    /// scopes because a resource requirement cannot express factory availability.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::{ContextPolicy, Graph, GraphSignature};
    ///
    /// async fn gather_info() -> String { String::new() }
    /// async fn log_progress() -> u32 { 0 } // an internal detail, kept private below
    ///
    /// let mut research = Graph::new();
    /// research.add_system(gather_info).add_system(log_progress);
    ///
    /// // The scope advertises its interface instead of erasing it — and the
    /// // contract need not name everything the interior guarantees: `u32`
    /// // (produced by `log_progress`) is guaranteed but left undeclared, so
    /// // it validates as a subset and stays private to the scope at runtime.
    /// let mut graph = Graph::new();
    /// graph.add_scope_with_contract(
    ///     "research",
    ///     research,
    ///     ContextPolicy::new(),
    ///     GraphSignature::new().produce::<String>(),
    /// );
    /// assert!(graph.validate().is_ok());
    /// assert_eq!(graph.signature(), GraphSignature::new().produce::<String>());
    /// ```
    pub fn add_scope_with_contract(
        &mut self,
        name: impl Into<NodeName>,
        graph: Graph,
        policy: ContextPolicy,
        contract: GraphSignature,
    ) -> &mut Self {
        self.push_scope(ScopeNode::new(name, graph, policy).with_contract(contract))
    }

    /// Shared wiring for the `add_scope*` builders: connects the node to the
    /// chain and makes it the entry when it is the first node.
    fn push_scope(&mut self, scope: ScopeNode) -> &mut Self {
        let scope_id = scope.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, scope_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(scope_id.clone());
        }

        self.nodes.push(Node::Scope(scope));
        self.last_node = Some(scope_id);

        self
    }

    /// Adds a [`Dynamic`](Node::Dynamic) node with a fixed, inline set of
    /// candidate subgraphs.
    ///
    /// At execution time, `selector` reads the context and returns a candidate
    /// key; the matching candidate runs through the slot's
    /// [`policy`](DynamicSlot) and its outputs merge back into the parent. If the
    /// key is absent, the slot's [`default_key`](DynamicSlot::default_key) is
    /// tried; if that is also absent (or unset), execution fails with
    /// [`DynamicCandidateNotFound`](crate::executor::ExecutionError::DynamicCandidateNotFound).
    ///
    /// Each candidate's [signature](Graph::signature) is checked against the
    /// slot's contract at [`validate`](Graph::validate) time, so a parent graph
    /// that validates can never select an incompatible candidate.
    ///
    /// Unlike [`add_scope`](Self::add_scope) / [`add_switch`](Self::add_switch),
    /// which take a builder closure over a single inner graph, `candidates` is a
    /// set of independent, already-built `(key, Graph)` pairs: each candidate has
    /// its own entry point and is signature-checked on its own, so there is no
    /// shared graph for a closure to populate.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature};
    ///
    /// async fn use_tool() -> i32 { 1 }
    /// async fn respond() -> i32 { 2 }
    ///
    /// let mut tool = Graph::new();
    /// tool.add_system(use_tool);
    /// let mut reply = Graph::new();
    /// reply.add_system(respond);
    ///
    /// let slot = DynamicSlot::new(
    ///     GraphSignature::new().produce::<i32>(),
    ///     ContextPolicy::shared(),
    /// )
    /// .with_default_key("respond");
    ///
    /// let mut graph = Graph::new();
    /// graph.add_dynamic(
    ///     "route",
    ///     |_ctx| "respond",
    ///     [("tool", tool), ("respond", reply)],
    ///     slot,
    /// );
    /// assert!(graph.validate().is_ok());
    /// ```
    pub fn add_dynamic<S, R, I, K>(
        &mut self,
        name: impl Into<NodeName>,
        selector: S,
        candidates: I,
        slot: DynamicSlot,
    ) -> &mut Self
    where
        S: Fn(&SystemContext<'_>) -> R + Send + Sync + 'static,
        R: Into<Arc<str>> + 'static,
        I: IntoIterator<Item = (K, Graph)>,
        K: Into<Arc<str>>,
    {
        let inline: Vec<(Arc<str>, Arc<Graph>)> = candidates
            .into_iter()
            .map(|(key, graph)| (key.into(), Arc::new(graph)))
            .collect();
        self.push_dynamic(
            name,
            Box::new(Selector::new(selector)),
            CandidateSource::Inline(inline),
            slot,
        )
    }

    /// Adds a [`Dynamic`](Node::Dynamic) node whose candidates live in a
    /// per-session [`SubgraphRegistry`] local
    /// resource.
    ///
    /// Unlike [`add_dynamic`](Self::add_dynamic), the candidate set is open: it is
    /// seeded and mutated at runtime via `ResMut<SubgraphRegistry>` (each
    /// candidate is signature-checked against the registry contract on insertion).
    /// Insert a [`SubgraphRegistry::new(contract)`](crate::registry::SubgraphRegistry::new)
    /// whose contract matches the slot's contract into the session context before
    /// running.
    ///
    /// Because admission happens at the registry, the `slot`'s
    /// [`MatchingMode`](crate::graph::MatchingMode) is **not** consulted on
    /// this path — set the relation on the registry with
    /// [`SubgraphRegistry::with_matching`](crate::registry::SubgraphRegistry::with_matching),
    /// and mirror it on the slot only to document intent. `validate()` still
    /// checks this node's *contract* against its surroundings, and the
    /// executor still refuses a registry whose contract is not this node's.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature, SubgraphRegistry};
    /// use polaris_system::param::SystemContext;
    ///
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     async fn plan() -> i32 { 1 }
    ///
    ///     let contract = GraphSignature::new().produce::<i32>();
    ///
    ///     // The "planner" slot resolves its candidate from the registry at run time.
    ///     let mut graph = Graph::new();
    ///     graph.add_dynamic_registry(
    ///         "planner",
    ///         |_ctx| "current",
    ///         DynamicSlot::new(contract.clone(), ContextPolicy::shared()),
    ///     );
    ///     assert!(graph.validate().is_ok());
    ///
    ///     // Seed a registry whose contract matches the slot's, then insert it
    ///     // into the session context before execution.
    ///     let mut registry = SubgraphRegistry::new(contract);
    ///     let mut candidate = Graph::new();
    ///     candidate.add_system(plan);
    ///     registry.register("current", candidate)?; // contract-checked on insert
    ///     let _ctx = SystemContext::new().with(registry);
    ///     Ok(())
    /// }
    /// ```
    pub fn add_dynamic_registry<S, R>(
        &mut self,
        name: impl Into<NodeName>,
        selector: S,
        slot: DynamicSlot,
    ) -> &mut Self
    where
        S: Fn(&SystemContext<'_>) -> R + Send + Sync + 'static,
        R: Into<Arc<str>> + 'static,
    {
        let source = CandidateSource::Registry {
            resource_type: TypeId::of::<SubgraphRegistry>(),
        };
        self.push_dynamic(name, Box::new(Selector::new(selector)), source, slot)
    }

    /// Adds a [`Dynamic`](Node::Dynamic) node with an inline candidate set and a
    /// pre-boxed selector.
    ///
    /// The [`BoxedSelector`] variant of [`add_dynamic`](Self::add_dynamic):
    /// reach for it when the selection logic cannot be an infallible closure —
    /// typically a hand-implemented [`ErasedSelector`](crate::selector::ErasedSelector)
    /// whose `select` returns an error instead of falling back to a key.
    /// Everything else (candidate checking, defaults, boundary policy) behaves
    /// exactly like `add_dynamic`.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::predicate::PredicateError;
    /// use polaris_graph::selector::{BoxedSelector, ErasedSelector};
    /// use polaris_graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature};
    /// use polaris_system::param::SystemContext;
    /// use std::sync::Arc;
    ///
    /// struct Route { tool: bool }
    /// async fn use_tool() -> i32 { 1 }
    /// async fn respond() -> i32 { 2 }
    ///
    /// // A fallible selector: unlike a closure, it *errors* when its input is
    /// // missing rather than routing to a fallback key.
    /// struct RouteSelector;
    /// impl ErasedSelector for RouteSelector {
    ///     fn select(&self, ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
    ///         let route = ctx
    ///             .get_output::<Route>()
    ///             .map_err(|_| PredicateError::OutputNotFound { type_name: "Route" })?;
    ///         Ok(if route.tool { Arc::from("tool") } else { Arc::from("respond") })
    ///     }
    ///     fn input_type_name(&self) -> &'static str {
    ///         "Route"
    ///     }
    /// }
    ///
    /// let mut tool = Graph::new();
    /// tool.add_system(use_tool);
    /// let mut reply = Graph::new();
    /// reply.add_system(respond);
    ///
    /// let selector: BoxedSelector = Box::new(RouteSelector);
    /// let mut graph = Graph::new();
    /// graph.add_dynamic_boxed(
    ///     "route",
    ///     selector,
    ///     [("tool", tool), ("respond", reply)],
    ///     DynamicSlot::new(
    ///         GraphSignature::new().require_output::<Route>().produce::<i32>(),
    ///         ContextPolicy::shared(),
    ///     ),
    /// );
    /// ```
    pub fn add_dynamic_boxed<I, K>(
        &mut self,
        name: impl Into<NodeName>,
        selector: BoxedSelector,
        candidates: I,
        slot: DynamicSlot,
    ) -> &mut Self
    where
        I: IntoIterator<Item = (K, Graph)>,
        K: Into<Arc<str>>,
    {
        let inline: Vec<(Arc<str>, Arc<Graph>)> = candidates
            .into_iter()
            .map(|(key, graph)| (key.into(), Arc::new(graph)))
            .collect();
        self.push_dynamic(name, selector, CandidateSource::Inline(inline), slot)
    }

    /// Adds a [`Dynamic`](Node::Dynamic) node backed by a per-session
    /// [`SubgraphRegistry`] with a pre-boxed selector.
    ///
    /// The [`BoxedSelector`] variant of
    /// [`add_dynamic_registry`](Self::add_dynamic_registry) — see
    /// [`add_dynamic_boxed`](Self::add_dynamic_boxed) for when to prefer a
    /// hand-implemented selector over a closure.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::predicate::PredicateError;
    /// use polaris_graph::selector::{BoxedSelector, ErasedSelector};
    /// use polaris_graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature, SubgraphRegistry};
    /// use polaris_system::param::SystemContext;
    /// use std::sync::Arc;
    ///
    /// // A hand-implemented selector: reach for the `_boxed` form when selection
    /// // must be fallible rather than an infallible closure.
    /// struct PickCurrent;
    /// impl ErasedSelector for PickCurrent {
    ///     fn select(&self, _ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
    ///         Ok(Arc::from("current"))
    ///     }
    ///     fn input_type_name(&self) -> &'static str {
    ///         "()"
    ///     }
    /// }
    ///
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     async fn plan() -> i32 { 1 }
    ///
    ///     let contract = GraphSignature::new().produce::<i32>();
    ///
    ///     let selector: BoxedSelector = Box::new(PickCurrent);
    ///     let mut graph = Graph::new();
    ///     graph.add_dynamic_registry_boxed(
    ///         "planner",
    ///         selector,
    ///         DynamicSlot::new(contract.clone(), ContextPolicy::shared()),
    ///     );
    ///     assert!(graph.validate().is_ok());
    ///
    ///     // Seed a registry whose contract matches the slot's, then insert it
    ///     // into the session context before execution.
    ///     let mut registry = SubgraphRegistry::new(contract);
    ///     let mut candidate = Graph::new();
    ///     candidate.add_system(plan);
    ///     registry.register("current", candidate)?; // contract-checked on insert
    ///     let _ctx = SystemContext::new().with(registry);
    ///     Ok(())
    /// }
    /// ```
    pub fn add_dynamic_registry_boxed(
        &mut self,
        name: impl Into<NodeName>,
        selector: BoxedSelector,
        slot: DynamicSlot,
    ) -> &mut Self {
        let source = CandidateSource::Registry {
            resource_type: TypeId::of::<SubgraphRegistry>(),
        };
        self.push_dynamic(name, selector, source, slot)
    }

    /// Shared construction for the `add_dynamic*` builders: builds the node and
    /// wires it into the graph like `add_scope`.
    fn push_dynamic(
        &mut self,
        name: impl Into<NodeName>,
        selector: BoxedSelector,
        source: CandidateSource,
        slot: DynamicSlot,
    ) -> &mut Self {
        let dynamic = DynamicNode::new(name, selector, source, slot);
        let dynamic_id = dynamic.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, dynamic_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(dynamic_id.clone());
        }

        self.nodes.push(Node::Dynamic(dynamic));
        self.last_node = Some(dynamic_id);

        self
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Error and Timeout Handlers
    // ─────────────────────────────────────────────────────────────────────────

    /// Attaches an error handler to all fallible system nodes that don't
    /// already have an error edge.
    ///
    /// The `handler` closure builds a subgraph once. An [`ErrorEdge`] is then
    /// added from every fallible system node (where
    /// [`ErasedSystem::is_fallible`](polaris_system::system::ErasedSystem::is_fallible)
    /// returns `true`) that does not already have an error edge, pointing to
    /// the handler subgraph's entry.
    ///
    /// # Ordering
    ///
    /// Only nodes without an existing error edge are wired, so this must run
    /// *after* any node-specific handler
    /// ([`add_error_handler_for`](Self::add_error_handler_for),
    /// [`add_error_handler_fn_for`](Self::add_error_handler_fn_for)) meant to
    /// take priority over it — reversing the order gives that node two error
    /// routes, which [`Graph::validate`](Self::validate) rejects as
    /// [`ValidationError::AmbiguousRoute`](crate::graph::ValidationError::AmbiguousRoute)
    /// (`kind: `[`RouteKind::Error`](crate::graph::RouteKind::Error)).
    ///
    /// # Arguments
    ///
    /// * `handler` - Builder function that creates the shared error handling subgraph
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn risky_operation() -> Result<i32, String> { Ok(1) }
    /// # async fn fallback_operation() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// graph.add_system(risky_operation)
    ///     .add_error_handler(|g| {
    ///         g.add_system(fallback_operation);
    ///     });
    /// ```
    pub fn add_error_handler<F>(&mut self, handler: F) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        // Build handler subgraph once.
        let mut handler_graph = Graph::new();
        handler(&mut handler_graph);

        let Some(handler_entry) = handler_graph.entry.take() else {
            // Empty handler — nothing to wire.
            return self;
        };

        // Collect node IDs that already have an error edge sourced from them.
        let nodes_with_error_edge: HashSet<NodeId> = self
            .edges
            .iter()
            .filter_map(|edge| {
                if let Edge::Error(_) = edge {
                    Some(edge.from())
                } else {
                    None
                }
            })
            .collect();

        // Collect IDs of fallible system nodes that need wiring.
        let targets: Vec<NodeId> = self
            .nodes
            .iter()
            .filter_map(|node| {
                if let Node::System(sys) = node
                    && sys.system.is_fallible()
                    && !nodes_with_error_edge.contains(&sys.id)
                {
                    Some(sys.id.clone())
                } else {
                    None
                }
            })
            .collect();

        if targets.is_empty() {
            // Every fallible node already has a handler — the same "nothing
            // to wire" case as an empty handler closure above. Dropping the
            // built-but-unattached subgraph here matters: merging it in
            // regardless would leave its nodes with no route to them at all,
            // which `Graph::validate` now reports as
            // `ValidationError::DisconnectedNodes`.
            return self;
        }

        // Wire error edges.
        for source in targets {
            let error_edge = Edge::Error(ErrorEdge::new(source, handler_entry.clone()));
            self.edges.push(error_edge);
        }

        // Merge handler subgraph into main graph.
        self.absorb(handler_graph);

        self
    }

    /// Adds an error handler for one or more specific nodes.
    ///
    /// When any of the specified source nodes fail, execution will continue at
    /// the error handler node built by `handler` instead of propagating the
    /// error. The handler subgraph is shared — only one copy of the handler
    /// nodes is added to the graph, with an error edge from each source.
    ///
    /// # Ordering
    ///
    /// Wires an error edge from every source unconditionally — there is no
    /// check for an existing one. Calling this a second time for a source
    /// that already has an error edge (from this method, from
    /// [`add_error_handler_fn_for`](Self::add_error_handler_fn_for), or from
    /// a blanket [`add_error_handler`](Self::add_error_handler) called
    /// first) gives that node two error routes:
    /// [`Graph::validate`](Self::validate) rejects it as
    /// [`ValidationError::AmbiguousRoute`](crate::graph::ValidationError::AmbiguousRoute)
    /// (`kind: `[`RouteKind::Error`](crate::graph::RouteKind::Error)). There
    /// is no override — call this at most once per source node, and before
    /// any blanket handler meant to skip it.
    ///
    /// # Arguments
    ///
    /// * `source_nodes` - The node ID(s) to attach the error handler to
    /// * `handler` - Builder function that creates the error handling subgraph
    ///
    /// # Examples
    ///
    /// Single source:
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn risky_operation() -> Result<i32, String> { Ok(1) }
    /// # async fn fallback_operation() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// let risky_id = graph.add_system_node(risky_operation);
    /// graph.add_error_handler_for(risky_id, |g: &mut Graph| {
    ///     g.add_system(fallback_operation);
    /// });
    /// ```
    ///
    /// Multiple sources sharing one handler:
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn step_a() -> Result<i32, String> { Ok(1) }
    /// # async fn step_b() -> Result<i32, String> { Ok(2) }
    /// # async fn shared_fallback() -> i32 { 0 }
    /// # let mut graph = Graph::new();
    /// let a = graph.add_system_node(step_a);
    /// let b = graph.add_system_node(step_b);
    /// graph.add_error_handler_for([a, b], |g: &mut Graph| {
    ///     g.add_system(shared_fallback);
    /// });
    /// ```
    pub fn add_error_handler_for<I, F>(&mut self, source_nodes: I, handler: F) -> &mut Self
    where
        I: IntoIterator<Item = NodeId>,
        F: FnOnce(&mut Graph),
    {
        // Build handler subgraph
        let mut handler_graph = Graph::new();
        handler(&mut handler_graph);

        // Get handler entry point
        if let Some(handler_entry) = handler_graph.entry.take() {
            // Add error edge from each source to handler
            let mut wired_any = false;
            for source in source_nodes {
                let error_edge = Edge::Error(ErrorEdge::new(source, handler_entry.clone()));
                self.edges.push(error_edge);
                wired_any = true;
            }

            // Merge handler graph into main graph — but only if a source was
            // actually wired to it. An empty `source_nodes` would otherwise
            // leave the handler subgraph's nodes with no route to them at
            // all, which `Graph::validate` now reports as
            // `ValidationError::DisconnectedNodes`.
            if wired_any {
                self.absorb(handler_graph);
            }
        }

        self
    }

    /// Attaches a closure-based error handler to all fallible system nodes
    /// that don't already have an error edge.
    ///
    /// The closure receives a [`&CaughtError`](CaughtError) and returns
    /// a value of type `T`. The returned value is stored as the system output,
    /// making it available to subsequent systems via `Out<T>`.
    ///
    /// This is a convenience over [`add_error_handler`](Self::add_error_handler)
    /// for trivial error mapping that doesn't need a full system definition.
    ///
    /// # Ordering
    ///
    /// Delegates to [`add_error_handler`](Self::add_error_handler), so the
    /// same ordering rule applies: it must run *after* any node-specific
    /// handler meant to take priority over it, or that node ends up with two
    /// error routes and [`Graph::validate`](Self::validate) rejects the
    /// graph.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::CaughtError;
    /// # #[derive(Debug)]
    /// # struct ErrorResponse { code: u16, message: String }
    /// # async fn risky_operation() -> Result<i32, String> { Ok(1) }
    /// # let mut graph = Graph::new();
    /// graph.add_system(risky_operation)
    ///     .add_error_handler_fn(|error: &CaughtError| -> ErrorResponse {
    ///         ErrorResponse {
    ///             code: 500,
    ///             message: error.message.to_string(),
    ///         }
    ///     });
    /// ```
    pub fn add_error_handler_fn<T, F>(&mut self, handler: F) -> &mut Self
    where
        T: Output,
        F: Fn(&CaughtError) -> T + Send + Sync + 'static,
    {
        let system = ClosureErrorHandler {
            handler,
            _marker: std::marker::PhantomData,
        };
        self.add_error_handler(|g| {
            g.add_boxed_system(Box::new(system));
        })
    }

    /// Attaches a closure-based error handler to specific source nodes.
    ///
    /// Like [`add_error_handler_fn`](Self::add_error_handler_fn) but only
    /// wires error edges from the specified source nodes.
    ///
    /// # Ordering
    ///
    /// Delegates to [`add_error_handler_for`](Self::add_error_handler_for),
    /// so the same ordering rule applies: it wires unconditionally, so
    /// calling it a second time for a source that already has an error edge
    /// gives that node two error routes and
    /// [`Graph::validate`](Self::validate) rejects the graph. There is no
    /// override — call this at most once per source node.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::CaughtError;
    /// # async fn risky_a() -> Result<i32, String> { Ok(1) }
    /// # async fn risky_b() -> Result<i32, String> { Ok(2) }
    /// # let mut graph = Graph::new();
    /// let a = graph.add_system_node(risky_a);
    /// let b = graph.add_system_node(risky_b);
    /// graph.add_error_handler_fn_for([a, b], |error: &CaughtError| -> String {
    ///     format!("handled: {}", error.message)
    /// });
    /// ```
    pub fn add_error_handler_fn_for<T, F, I>(&mut self, source_nodes: I, handler: F) -> &mut Self
    where
        T: Output,
        F: Fn(&CaughtError) -> T + Send + Sync + 'static,
        I: IntoIterator<Item = NodeId>,
    {
        let system = ClosureErrorHandler {
            handler,
            _marker: std::marker::PhantomData,
        };
        self.add_error_handler_for(source_nodes, |g| {
            g.add_boxed_system(Box::new(system));
        })
    }

    /// Adds a timeout handler for a specific node.
    ///
    /// When the system at `source_node` times out, execution will continue at the
    /// timeout handler node built by `handler` instead of returning an error.
    ///
    /// Note: You must also set a timeout on the source node for this handler to
    /// be triggered. Use [`set_timeout`](Self::set_timeout) after adding the system.
    ///
    /// # Ordering
    ///
    /// Wires a timeout edge from every source unconditionally — there is no
    /// blanket variant and no check for an existing one. Calling this a
    /// second time for the same source node gives it two timeout routes:
    /// [`Graph::validate`](Self::validate) rejects it as
    /// [`ValidationError::AmbiguousRoute`](crate::graph::ValidationError::AmbiguousRoute)
    /// (`kind: `[`RouteKind::Timeout`](crate::graph::RouteKind::Timeout)).
    /// There is no override — call this at most once per source node.
    ///
    /// # Arguments
    ///
    /// * `source_node` - The node ID to attach the timeout handler to
    /// * `handler` - Builder function that creates the timeout handling subgraph
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use core::time::Duration;
    /// # async fn slow_operation() -> i32 { 1 }
    /// # async fn fallback_operation() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// let slow_id = graph.add_system_node(slow_operation);
    /// graph.set_timeout(slow_id.clone(), Duration::from_secs(5));
    /// graph.add_timeout_handler(slow_id, |g| {
    ///     g.add_system(fallback_operation);
    /// });
    /// ```
    pub fn add_timeout_handler<I, F>(&mut self, source_nodes: I, handler: F) -> &mut Self
    where
        I: IntoIterator<Item = NodeId>,
        F: FnOnce(&mut Graph),
    {
        // Build handler subgraph
        let mut handler_graph = Graph::new();
        handler(&mut handler_graph);

        // Get handler entry point
        if let Some(handler_entry) = handler_graph.entry.take() {
            // Add timeout edge from each source to handler
            let mut wired_any = false;
            for source in source_nodes {
                let timeout_edge = Edge::Timeout(TimeoutEdge::new(source, handler_entry.clone()));
                self.edges.push(timeout_edge);
                wired_any = true;
            }

            // Merge handler graph into main graph — but only if a source was
            // actually wired to it. An empty `source_nodes` would otherwise
            // leave the handler subgraph's nodes with no route to them at
            // all, which `Graph::validate` now reports as
            // `ValidationError::DisconnectedNodes`.
            if wired_any {
                self.absorb(handler_graph);
            }
        }

        self
    }

    /// Sets a timeout on a system node.
    ///
    /// If the system's execution exceeds the timeout, the executor will either
    /// follow a timeout edge (if one exists) or return a `Timeout` error.
    ///
    /// # Panics
    ///
    /// Panics if the node is not a system node or doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use core::time::Duration;
    /// # async fn slow_operation() -> i32 { 1 }
    /// # let mut graph = Graph::new();
    /// let id = graph.add_system_node(slow_operation);
    /// graph.set_timeout(id, Duration::from_secs(5));
    /// ```
    pub fn set_timeout(&mut self, node_id: NodeId, timeout: Duration) -> &mut Self {
        for node in &mut self.nodes {
            if let Node::System(sys) = node
                && sys.id == node_id
            {
                sys.timeout = Some(timeout);
                return self;
            }
        }
        panic!("set_timeout: node {node_id} not found or is not a system node");
    }

    /// Sets a retry policy on a system node.
    ///
    /// When the system fails, the executor will retry according to the policy
    /// before routing to error/timeout handlers.
    ///
    /// # Panics
    ///
    /// Panics if the node is not a system node or doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::RetryPolicy;
    /// # use core::time::Duration;
    /// # async fn call_llm() -> Result<String, String> { Ok("response".to_string()) }
    /// # let mut graph = Graph::new();
    /// let id = graph.add_system_node(call_llm);
    /// graph.set_retry_policy(id, RetryPolicy::exponential(3, Duration::from_millis(100)));
    /// ```
    pub fn set_retry_policy(&mut self, node_id: NodeId, policy: RetryPolicy) -> &mut Self {
        for node in &mut self.nodes {
            if let Node::System(sys) = node
                && sys.id == node_id
            {
                sys.retry_policy = Some(policy);
                return self;
            }
        }
        panic!("set_retry_policy: node {node_id} not found or is not a system node");
    }

    /// Sets how many of a loop's most recent completed iterations remain
    /// visible to its later iterations.
    ///
    /// The default is 1: while an iteration runs, the previous iteration's
    /// outputs stay readable. A larger value keeps more history available to
    /// `Out<T>`/`OutAll<T>` reads in subsequent iterations; 0 keeps none, so
    /// each iteration sees only what it wrote itself plus what existed before
    /// the loop. Whatever the value, only the last iteration's outputs cross
    /// the loop boundary when the loop ends. See
    /// [`set_loop_retention_for_type`](Self::set_loop_retention_for_type) to
    /// change it for one output type only; 0 means the same there.
    ///
    /// Configuration lives in a side table keyed by `node_id` rather than as a
    /// field on the loop node itself, so `add_loop*`'s existing `&mut Graph`
    /// chaining return is unchanged — call this after adding the loop, using
    /// [`last_node`](Graph::last_node) to get its id.
    ///
    /// # Panics
    ///
    /// Panics if the node is not a loop node or doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn step() -> i32 { 1 }
    /// # let mut graph = Graph::new();
    /// graph.add_loop_n("loop", 5, |g| { g.add_system(step); });
    /// let loop_id = graph.last_node().expect("the loop was just added");
    /// graph.set_loop_retention(loop_id, 3);
    /// ```
    pub fn set_loop_retention(&mut self, node_id: NodeId, iterations: usize) -> &mut Self {
        self.assert_is_loop_node("set_loop_retention", &node_id);
        self.loop_retention.set_default(node_id, iterations);
        self
    }

    /// Sets a loop's explicit maximum iteration count.
    ///
    /// This limit applies in addition to a loop's termination predicate and
    /// overrides the executor's default iteration limit. A predicate loop
    /// whose predicate remains false when it reaches this limit returns
    /// [`ExecutionError::MaxIterationsExceeded`](crate::executor::ExecutionError::MaxIterationsExceeded).
    ///
    /// A limit of zero has the same meaning as it does for
    /// [`add_loop_n`](Self::add_loop_n): the body does not run. A predicate
    /// that is already true still ends the loop normally; otherwise the loop
    /// returns `MaxIterationsExceeded { max: 0 }` before starting its body.
    ///
    /// # Panics
    ///
    /// Panics if the node is not a loop node or doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn initial_state() -> bool { false }
    /// # async fn step() -> bool { false }
    /// # let mut graph = Graph::new();
    /// graph.add_system(initial_state).add_loop::<bool, _, _>(
    ///     "loop",
    ///     |done| *done,
    ///     |body| { body.add_system(step); },
    /// );
    /// let loop_id = graph.last_node().expect("the loop was just added");
    /// graph.set_loop_max_iterations(loop_id, 5);
    /// ```
    pub fn set_loop_max_iterations(&mut self, node_id: NodeId, max_iterations: usize) -> &mut Self {
        self.assert_is_loop_node("set_loop_max_iterations", &node_id);
        for node in &mut self.nodes {
            if let Node::Loop(loop_node) = node
                && loop_node.id == node_id
            {
                loop_node.max_iterations = Some(max_iterations);
                return self;
            }
        }
        unreachable!("loop node was checked before setting its iteration limit");
    }

    /// Sets a per-type override for how many of a loop's most recent
    /// completed iterations of `T` a loop keeps visible to its later
    /// iterations. It wins over [`set_loop_retention`](Self::set_loop_retention)
    /// for `T` without changing the retention of any other output type.
    ///
    /// The value means the same as it does for `set_loop_retention`: 0 keeps
    /// no earlier iteration's `T` readable, and the last iteration's `T`
    /// still crosses the loop boundary when the loop ends.
    ///
    /// # Panics
    ///
    /// Panics if the node is not a loop node or doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn draft() -> String { String::new() }
    /// # async fn score() -> u32 { 1 }
    /// # let mut graph = Graph::new();
    /// graph.add_loop_n("refine", 5, |g| {
    ///     g.add_system(draft).add_system(score);
    /// });
    /// let loop_id = graph.last_node().expect("the loop was just added");
    /// // Keep three earlier scores readable, but only the current draft.
    /// graph
    ///     .set_loop_retention(loop_id.clone(), 3)
    ///     .set_loop_retention_for_type::<String>(loop_id, 0);
    /// ```
    pub fn set_loop_retention_for_type<T: Output>(
        &mut self,
        node_id: NodeId,
        iterations: usize,
    ) -> &mut Self {
        self.assert_is_loop_node("set_loop_retention_for_type", &node_id);
        self.loop_retention
            .set_for_type(node_id, TypeId::of::<T>(), iterations);
        self
    }

    /// Panics with a message naming `caller` if `node_id` is not a loop node
    /// in this graph — the shared precondition check for the loop retention
    /// setters, mirroring [`set_timeout`](Self::set_timeout)'s and
    /// [`set_retry_policy`](Self::set_retry_policy)'s own panics for the
    /// wrong node kind.
    fn assert_is_loop_node(&self, caller: &str, node_id: &NodeId) {
        let is_loop = self
            .nodes
            .iter()
            .any(|node| matches!(node, Node::Loop(loop_node) if loop_node.id == *node_id));
        assert!(
            is_loop,
            "{caller}: node {node_id} not found or is not a loop node"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Switch API
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a switch node for multi-way branching based on a discriminator.
    ///
    /// Switch nodes generalize decision nodes to handle multiple cases,
    /// similar to a match/switch statement. The discriminator evaluates
    /// the previous system's output and returns a case key.
    ///
    /// # Type Parameters
    ///
    /// - `T`: The output type from the previous system
    /// - `D`: The discriminator closure type
    /// - `C`: The case builder type (iterable of key-handler pairs)
    /// - `F`: The default case builder type (optional)
    ///
    /// The discriminator must return `&'static str` and case keys must be
    /// `&'static str`, matching every existing call site's turbofish arity
    /// unchanged. For a discriminator or case keys known only at runtime
    /// (e.g. `String`s), build a [`BoxedDiscriminator`] and use
    /// [`add_switch_boxed`](Self::add_switch_boxed) instead — adding type
    /// parameters here to generalize over the return/key type would change
    /// this method's generic arity and break every existing explicit
    /// turbofish call site, since Rust does not allow a turbofish to supply
    /// a strict prefix of a function's type parameters.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for debugging
    /// * `discriminator` - Closure that returns a case key from `&T`
    /// * `cases` - Vec of (key, handler) pairs for each case branch
    /// * `default` - Optional default handler if no case matches
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # struct RouterOutput { action: &'static str }
    /// # async fn use_tool() -> i32 { 1 }
    /// # async fn respond() -> i32 { 2 }
    /// # async fn handle_unknown() -> i32 { 3 }
    /// # let mut graph = Graph::new();
    /// graph.add_switch::<RouterOutput, _, _, _>(
    ///     "route_action",
    ///     |output| output.action,
    ///     vec![
    ///         ("tool", Box::new(|g: &mut Graph| { g.add_system(use_tool); }) as Box<dyn FnOnce(&mut Graph)>),
    ///         ("respond", Box::new(|g: &mut Graph| { g.add_system(respond); }) as Box<dyn FnOnce(&mut Graph)>),
    ///     ],
    ///     Some(Box::new(|g: &mut Graph| { g.add_system(handle_unknown); })),
    /// );
    /// ```
    pub fn add_switch<T, D, C, F>(
        &mut self,
        name: impl Into<NodeName>,
        discriminator: D,
        cases: C,
        default: Option<F>,
    ) -> &mut Self
    where
        T: Output,
        D: Fn(&T) -> &'static str + Send + Sync + 'static,
        C: IntoIterator<Item = (&'static str, F)>,
        F: FnOnce(&mut Graph),
    {
        let boxed_discriminator: BoxedDiscriminator =
            Discriminator::<T, D>::new(discriminator).into();
        self.add_switch_boxed(name, boxed_discriminator, cases, default)
    }

    /// Adds a switch node for multi-way branching with an already-boxed discriminator.
    ///
    /// The type-erased counterpart of [`add_switch`](Self::add_switch) —
    /// build the [`BoxedDiscriminator`] with [`BoxedDiscriminator::new`] when
    /// the input type is only known at runtime, without naming it at the
    /// call site. Case keys accept any `impl Into<NodeName>`, so they can
    /// come from runtime `String`s as well as `&'static str` literals.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::predicate::BoxedDiscriminator;
    /// # struct RouterOutput { action: &'static str }
    /// # async fn use_tool() -> i32 { 1 }
    /// # async fn respond() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// let discriminator = BoxedDiscriminator::new::<RouterOutput, _>(|out| out.action);
    /// graph.add_switch_boxed(
    ///     "route_action",
    ///     discriminator,
    ///     vec![
    ///         ("tool", Box::new(|g: &mut Graph| { g.add_system(use_tool); }) as Box<dyn FnOnce(&mut Graph)>),
    ///         ("respond", Box::new(|g: &mut Graph| { g.add_system(respond); }) as Box<dyn FnOnce(&mut Graph)>),
    ///     ],
    ///     None,
    /// );
    /// ```
    pub fn add_switch_boxed<C, F, K>(
        &mut self,
        name: impl Into<NodeName>,
        discriminator: BoxedDiscriminator,
        cases: C,
        default: Option<F>,
    ) -> &mut Self
    where
        C: IntoIterator<Item = (K, F)>,
        K: Into<NodeName>,
        F: FnOnce(&mut Graph),
    {
        // Create switch node
        let mut switch_node = SwitchNode::with_discriminator(name, discriminator);
        let switch_id = switch_node.id.clone();

        // Build each case subgraph
        for (key, handler) in cases {
            let mut case_graph = Graph::new();
            handler(&mut case_graph);

            if let Some(case_entry) = case_graph.entry.take() {
                switch_node.cases.push((key.into(), case_entry));

                // Merge case graph into main graph
                self.absorb(case_graph);
            }
        }

        // Build default case if provided
        if let Some(default_handler) = default {
            let mut default_graph = Graph::new();
            default_handler(&mut default_graph);

            if let Some(default_entry) = default_graph.entry.take() {
                switch_node.default = Some(default_entry);

                // Merge default graph into main graph
                self.absorb(default_graph);
            }
        }

        // Link previous node to switch
        if let Some(last) = self.last_node.clone() {
            self.add_sequential_edge(last, switch_id.clone());
        }

        // Set entry point if this is the first node
        if self.entry.is_none() {
            self.entry = Some(switch_id.clone());
        }

        // Add switch node and update last_node
        self.nodes.push(Node::Switch(switch_node));
        self.last_node = Some(switch_id);

        self
    }
}

/// Builder for configuring a system node's schedules, error handling, timeout,
/// and retry policy.
///
/// Created by [`Graph::system`] or [`Graph::system_boxed`]. The node is already
/// added to the graph; this builder attaches decorations (schedules, error
/// handlers, timeout, retry policy).
///
/// Call [`.done()`](Self::done) to return to `&mut Graph` for continued
/// fluent chaining, or let the builder drop to release the borrow.
pub struct SystemNodeBuilder<'a> {
    graph: &'a mut Graph,
    node_id: NodeId,
}

impl<'a> SystemNodeBuilder<'a> {
    /// Returns the node ID of this system node.
    #[must_use]
    pub fn id(&self) -> NodeId {
        self.node_id.clone()
    }

    /// Attaches custom schedules to this system node.
    ///
    /// System lifecycle events are re-emitted on these schedules, allowing
    /// hooks to subscribe to events for this system only. A tuple attaches
    /// multiple schedules.
    ///
    /// Schedules **accumulate**: markers already attached — by an earlier call
    /// or by the `(schedule, system)` tuple form passed to
    /// [`Graph::system`](Graph::system) — are preserved, and a marker already
    /// present is not added twice. This means a composition helper can stamp
    /// its own role marker onto a caller-supplied stage without discarding the
    /// caller's observability markers. Attachment order is preserved, with
    /// newly added markers appended after the existing ones.
    ///
    /// # Panics
    ///
    /// Panics if this builder's node is no longer a system node in the graph.
    /// Unreachable through the public API: the builder is only constructed
    /// immediately after its node is pushed, and it holds `&mut Graph` for its
    /// whole lifetime.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_system::plugin::Schedule;
    /// # async fn run_stage() {}
    /// struct OnStage;
    /// impl Schedule for OnStage {}
    ///
    /// let mut graph = Graph::new();
    /// graph.system(run_stage)
    ///     .with_schedules(OnStage)
    ///     .done();
    /// ```
    ///
    /// Accumulating onto a tuple-attached marker keeps both:
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::node::Node;
    /// # use polaris_system::plugin::{Schedule, ScheduleId};
    /// # async fn run_stage() {}
    /// struct OnAudit;
    /// impl Schedule for OnAudit {}
    /// struct OnStage;
    /// impl Schedule for OnStage {}
    ///
    /// let mut graph = Graph::new();
    /// graph.system((OnAudit, run_stage)).with_schedules(OnStage).done();
    ///
    /// let Node::System(node) = &graph.nodes()[0] else { unreachable!() };
    /// assert_eq!(
    ///     node.schedules,
    ///     [ScheduleId::of::<OnAudit>(), ScheduleId::of::<OnStage>()],
    /// );
    /// ```
    pub fn with_schedules(self, schedules: impl IntoScheduleIds) -> Self {
        fn schedule_ids_for<Sch: IntoScheduleIds>(_: &Sch) -> Vec<ScheduleId> {
            Sch::schedule_ids()
        }

        let added = schedule_ids_for(&schedules);
        let system = self
            .graph
            .nodes
            .iter_mut()
            .find_map(|node| match node {
                Node::System(system) if system.id == self.node_id => Some(system),
                _ => None,
            })
            .expect("SystemNodeBuilder always refers to an existing system node");
        system.push_schedules(added);
        self
    }

    /// Sets the node's runtime display name.
    ///
    /// Defaults to the system's static function name (`S::name()` /
    /// [`ErasedSystem::name`](polaris_system::system::ErasedSystem::name)). Setting a
    /// runtime name is useful when the same system is added to a graph more
    /// than once and the copies need to be told apart in hook events,
    /// middleware [`SystemInfo`](crate::middleware::info::SystemInfo), and
    /// devtools [`SystemInfo`](crate::dev::SystemInfo). Works for both the
    /// typed ([`Graph::system`](Graph::system)) and boxed / typed-erased
    /// ([`Graph::system_boxed`](Graph::system_boxed)) construction paths,
    /// since both return this builder.
    ///
    /// # Panics
    ///
    /// Panics if this builder's node is no longer a system node in the graph.
    /// Unreachable through the public API: the builder is only constructed
    /// immediately after its node is pushed, and it holds `&mut Graph` for its
    /// whole lifetime.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::node::Node;
    /// # async fn run_stage() {}
    /// let mut graph = Graph::new();
    /// graph.system(run_stage).named("stage_one").done();
    ///
    /// let Node::System(node) = &graph.nodes()[0] else { unreachable!() };
    /// assert_eq!(node.node_name, "stage_one");
    /// ```
    pub fn named(self, name: impl Into<NodeName>) -> Self {
        let name = name.into();
        let system = self
            .graph
            .nodes
            .iter_mut()
            .find_map(|node| match node {
                Node::System(system) if system.id == self.node_id => Some(system),
                _ => None,
            })
            .expect("SystemNodeBuilder always refers to an existing system node");
        system.node_name = name;
        self
    }

    /// Attaches an error handler to this system node.
    pub fn on_error<F>(self, handler: F) -> Self
    where
        F: FnOnce(&mut Graph),
    {
        self.graph
            .add_error_handler_for(self.node_id.clone(), handler);
        self
    }

    /// Sets a timeout on this system node.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.graph.set_timeout(self.node_id.clone(), timeout);
        self
    }

    /// Sets a retry policy on this system node.
    pub fn with_retry(self, policy: RetryPolicy) -> Self {
        self.graph.set_retry_policy(self.node_id.clone(), policy);
        self
    }

    /// Binds this system's `Out<T>` (or `Option<Out<T>>`) read of `T` to one
    /// producer node's entries, instead of the newest write of `T` from any
    /// producer.
    ///
    /// The system itself still declares a plain `Out<T>` — the *graph* picks
    /// its producer, not the system's own type, so the same system can be
    /// reused in a graph with a different producer wired in. At run time the
    /// executor installs this binding only for the duration of this node's
    /// own `run` call.
    ///
    /// A second `read_from::<T>` call on the same node replaces the earlier
    /// binding for `T` — the same "last call wins" rule as
    /// [`with_timeout`](Self::with_timeout).
    ///
    /// [`Graph::validate`] rejects the wire if `producer` is missing from
    /// this graph, is not a system node, has a different output type, or is
    /// not guaranteed to have completed before this node on every path —
    /// unless both nodes are in the same loop body, in which case the wire
    /// reads the producer's entry from an earlier iteration (seed the value
    /// before the loop, or read it as `Option<Out<T>>`, since the first
    /// iteration has none).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_system::param::Out;
    /// # struct Plan { steps: Vec<String> }
    /// # async fn plan() -> Plan { Plan { steps: vec![] } }
    /// # #[polaris_system::system]
    /// # async fn act(plan: Out<Plan>) -> i32 { plan.steps.len() as i32 }
    /// let mut graph = Graph::new();
    /// let planner = graph.system(plan).id();
    /// graph.system(act).read_from::<Plan>(planner);
    /// ```
    ///
    /// A loop's backward wire (reading an earlier iteration) needs the
    /// producer's id *before* the producer node has
    /// been added, since the producer runs structurally later in the body
    /// than the reader that reads its previous value. This builder method
    /// cannot express that ordering; use [`Graph::read_from`] directly once
    /// both nodes exist by id instead.
    pub fn read_from<T: Output>(self, producer: NodeId) -> Self {
        self.graph.read_from::<T>(self.node_id.clone(), producer);
        self
    }

    /// Attaches a closure-based error handler to this system node.
    ///
    /// The closure receives a [`&CaughtError`](CaughtError) and returns a value
    /// of type `T`, which is stored as the handler's output. This is a
    /// convenience over [`on_error`](Self::on_error) for trivial error mapping.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # use polaris_graph::CaughtError;
    /// # async fn risky_operation() -> Result<i32, String> { Ok(1) }
    /// # async fn next_step() {}
    /// # let mut graph = Graph::new();
    /// graph.system(risky_operation)
    ///     .on_error_fn(|error: &CaughtError| -> String {
    ///         format!("handled: {}", error.message)
    ///     })
    ///     .done()
    ///     .add_system(next_step);
    /// ```
    pub fn on_error_fn<T, F>(self, handler: F) -> Self
    where
        T: Output,
        F: Fn(&CaughtError) -> T + Send + Sync + 'static,
    {
        self.graph
            .add_error_handler_fn_for([self.node_id.clone()], handler);
        self
    }

    /// Attaches a timeout handler to this system node.
    pub fn on_timeout<F>(self, handler: F) -> Self
    where
        F: FnOnce(&mut Graph),
    {
        self.graph
            .add_timeout_handler(self.node_id.clone(), handler);
        self
    }

    /// Finishes configuration and returns the graph for continued chaining.
    pub fn done(self) -> &'a mut Graph {
        self.graph
    }
}
