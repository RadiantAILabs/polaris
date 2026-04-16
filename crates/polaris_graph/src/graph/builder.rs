//! Builder API for constructing graphs.

use super::{Graph, MergeError};
use crate::edge::{Edge, ErrorEdge, TimeoutEdge};
use crate::executor::CaughtError;
use crate::node::{
    ContextPolicy, DecisionNode, IntoSystemNode, LoopNode, Node, NodeId, ParallelNode, RetryPolicy,
    ScopeNode, SwitchNode, SystemNode,
};
use crate::predicate::Predicate;
use hashbrown::HashSet;
use polaris_system::param::{ERROR_CONTEXT, SystemAccess, SystemContext};
use polaris_system::resource::Output;
use polaris_system::system::{BoxFuture, BoxedSystem, System, SystemError};
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
    /// Custom [`Schedule`] types can be attached to a system by passing a
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

    /// Adds a boxed system node directly.
    ///
    /// This is useful for adding custom `System` implementations
    /// that don't go through `IntoSystem`.
    ///
    /// # Returns
    ///
    /// The node ID of the newly added system node.
    pub fn add_boxed_system(&mut self, system: BoxedSystem) -> NodeId {
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

    /// Adds a boxed system node and returns a builder for configuring
    /// error/timeout handlers.
    ///
    /// This is useful for adding custom `System` implementations
    /// that don't go through `IntoSystem`,
    ///
    /// This is the builder equivalent of [`add_boxed_system`](Self::add_boxed_system),
    /// useful for custom `System` implementations that don't go through `IntoSystem`.
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
    pub fn system_boxed(&mut self, system: BoxedSystem) -> SystemNodeBuilder<'_> {
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

        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
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
        name: &'static str,
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
        let boxed_predicate = Box::new(Predicate::<T, P>::new(predicate));
        let mut decision = DecisionNode::with_predicate(name, boxed_predicate);
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

        if let Some(entry) = true_graph.entry {
            decision.true_branch = Some(entry);
        }

        // Build false branch
        let mut false_graph = Graph::new();
        false_path(&mut false_graph);

        if let Some(entry) = false_graph.entry {
            decision.false_branch = Some(entry);
        }

        // Add decision node
        self.nodes.push(Node::Decision(decision));

        // Merge branch graphs into main graph
        self.nodes.extend(true_graph.nodes);
        self.edges.extend(true_graph.edges);
        self.nodes.extend(false_graph.nodes);
        self.edges.extend(false_graph.edges);

        // Decision node becomes the last node (branches may rejoin later)
        self.last_node = Some(decision_id);

        self
    }

    /// Adds a parallel execution node.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the parallel node.
    /// * `branches` - Builder functions for each parallel branch.
    pub fn add_parallel<I, F>(&mut self, name: &'static str, branches: I) -> &mut Self
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

            if let Some(entry) = branch_graph.entry {
                parallel.branches.push(entry);
            }

            // Merge branch graph
            self.nodes.extend(branch_graph.nodes);
            self.edges.extend(branch_graph.edges);
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
    /// # async fn reason() -> LoopState { LoopState { is_done: false, iterations: 0 } }
    /// # async fn act() -> i32 { 1 }
    /// # async fn observe() -> i32 { 2 }
    /// # let mut graph = Graph::new();
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
    pub fn add_loop<T, P, F>(&mut self, name: &'static str, termination: P, body: F) -> &mut Self
    where
        T: Output,
        P: Fn(&T) -> bool + Send + Sync + 'static,
        F: FnOnce(&mut Graph),
    {
        let boxed_termination = Box::new(Predicate::<T, P>::new(termination));
        let mut loop_node = LoopNode::with_termination(name, boxed_termination);
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

        if let Some(entry) = body_graph.entry {
            loop_node.body_entry = Some(entry);
        }

        // Merge body graph
        self.nodes.extend(body_graph.nodes);
        self.edges.extend(body_graph.edges);

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
    pub fn add_loop_n<F>(&mut self, name: &'static str, max_iterations: usize, body: F) -> &mut Self
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

        if let Some(entry) = body_graph.entry {
            loop_node.body_entry = Some(entry);
        }

        // Merge body graph
        self.nodes.extend(body_graph.nodes);
        self.edges.extend(body_graph.edges);

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
    /// graph.add_scope("research", research, ContextPolicy::inherit());
    /// ```
    pub fn add_scope(
        &mut self,
        name: &'static str,
        graph: Graph,
        policy: ContextPolicy,
    ) -> &mut Self {
        let scope = ScopeNode::new(name, graph, policy);
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

        let Some(handler_entry) = handler_graph.entry else {
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

        // Wire error edges.
        for source in targets {
            let error_edge = Edge::Error(ErrorEdge::new(source, handler_entry.clone()));
            self.edges.push(error_edge);
        }

        // Merge handler subgraph into main graph.
        self.nodes.extend(handler_graph.nodes);
        self.edges.extend(handler_graph.edges);

        self
    }

    /// Adds an error handler for one or more specific nodes.
    ///
    /// When any of the specified source nodes fail, execution will continue at
    /// the error handler node built by `handler` instead of propagating the
    /// error. The handler subgraph is shared — only one copy of the handler
    /// nodes is added to the graph, with an error edge from each source.
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
        if let Some(handler_entry) = handler_graph.entry {
            // Add error edge from each source to handler
            for source in source_nodes {
                let error_edge = Edge::Error(ErrorEdge::new(source, handler_entry.clone()));
                self.edges.push(error_edge);
            }

            // Merge handler graph into main graph
            self.nodes.extend(handler_graph.nodes);
            self.edges.extend(handler_graph.edges);
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
        if let Some(handler_entry) = handler_graph.entry {
            // Add timeout edge from each source to handler
            for source in source_nodes {
                let timeout_edge = Edge::Timeout(TimeoutEdge::new(source, handler_entry.clone()));
                self.edges.push(timeout_edge);
            }

            // Merge handler graph into main graph
            self.nodes.extend(handler_graph.nodes);
            self.edges.extend(handler_graph.edges);
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
        name: &'static str,
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
        use crate::predicate::Discriminator;

        // Create the discriminator
        let boxed_discriminator: crate::predicate::BoxedDiscriminator =
            Box::new(Discriminator::<T, D>::new(discriminator));

        // Create switch node
        let mut switch_node = SwitchNode::with_discriminator(name, boxed_discriminator);
        let switch_id = switch_node.id.clone();

        // Build each case subgraph
        for (key, handler) in cases {
            let mut case_graph = Graph::new();
            handler(&mut case_graph);

            if let Some(case_entry) = case_graph.entry {
                switch_node.cases.push((key, case_entry));

                // Merge case graph into main graph
                self.nodes.extend(case_graph.nodes);
                self.edges.extend(case_graph.edges);
            }
        }

        // Build default case if provided
        if let Some(default_handler) = default {
            let mut default_graph = Graph::new();
            default_handler(&mut default_graph);

            if let Some(default_entry) = default_graph.entry {
                switch_node.default = Some(default_entry);

                // Merge default graph into main graph
                self.nodes.extend(default_graph.nodes);
                self.edges.extend(default_graph.edges);
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

/// Builder for configuring a system node's error handling, timeout, and retry.
///
/// Created by [`Graph::system`]. The node is already added to the graph;
/// this builder attaches decorations (error handlers, timeout, retry policy).
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
