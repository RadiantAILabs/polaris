//! Graph structure and builder API.
//!
//! The `Graph` is the core data structure representing an agent's behavior
//! as a directed graph of systems and control flow constructs.

use crate::edge::{Edge, EdgeId, ErrorEdge, SequentialEdge, TimeoutEdge};
use crate::node::IntoSystemNode;
use crate::node::{
    DecisionNode, JoinNode, LoopNode, Node, NodeId, ParallelNode, SwitchNode, SystemNode,
};
use crate::predicate::Predicate;
use core::any::TypeId;
use core::fmt;
use hashbrown::{HashMap, HashSet};
use polaris_system::resource::Output;

/// A directed graph of systems.
///
/// Graphs are the fundamental structure for composing safe agentic behavior.
/// Each graph contains:
/// - **Nodes**: Computation units (systems) and control flow constructs
/// - **Edges**: Connections defining execution flow between nodes
/// - **Entry**: The starting point for graph execution
///
/// # Example
///
/// ```
/// # use polaris_graph::Graph;
/// # async fn reason() -> i32 { 1 }
/// # async fn decide() -> i32 { 2 }
/// # async fn invoke_tool() -> i32 { 3 }
/// # async fn respond() -> i32 { 4 }
/// let mut graph = Graph::new();
/// graph
///     .add_system(reason)
///     .add_system(decide)
///     .add_conditional_branch::<i32, _, _, _>(
///         "use_tool",
///         |_| true,
///         |g| { g.add_system(invoke_tool); },
///         |g| { g.add_system(respond); },
///     );
/// ```
#[derive(Debug, Default)]
pub struct Graph {
    /// All nodes in the graph.
    nodes: Vec<Node>,
    /// All edges connecting nodes.
    edges: Vec<Edge>,
    /// Entry point for graph execution.
    entry: Option<NodeId>,
    /// The last node added (for chaining).
    last_node: Option<NodeId>,
}

impl Graph {
    /// Creates a new empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns all nodes in the graph.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Returns all edges in the graph.
    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Returns the entry point node ID, if set.
    #[must_use]
    pub fn entry(&self) -> Option<NodeId> {
        self.entry.clone()
    }

    /// Returns the number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns true if the graph has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Gets a node by ID.
    ///
    /// Note: Node IDs may not correspond to array indices due to ID offsets
    /// used when building subgraphs, so this performs a search by ID.
    #[must_use]
    pub fn get_node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id() == id)
    }

    /// Gets an edge by ID.
    ///
    /// Note: Edge IDs may not correspond to array indices due to ID offsets
    /// used when building subgraphs, so this performs a search by ID.
    #[must_use]
    pub fn get_edge(&self, id: EdgeId) -> Option<&Edge> {
        self.edges.iter().find(|edge| edge.id() == id)
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
    /// async fn my_system() -> i32 { 42 }
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
    /// async fn step_a() -> i32 { 1 }
    /// async fn step_b() -> i32 { 2 }
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

    /// Adds a boxed system node directly.
    ///
    /// This is useful for adding custom `System` implementations
    /// that don't go through `IntoSystem`.
    ///
    /// # Returns
    ///
    /// The node ID of the newly added system node.
    pub fn add_boxed_system(&mut self, system: polaris_system::system::BoxedSystem) -> NodeId {
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

        // Create join node
        let mut join = JoinNode::new("join");
        join.sources = parallel.branches.clone();
        let join_id = join.id.clone();
        parallel.join = Some(join_id.clone());

        // Add nodes
        self.nodes.push(Node::Parallel(parallel));
        self.nodes.push(Node::Join(join));

        // Join becomes the last node
        self.last_node = Some(join_id);

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

    /// Adds an error handler for a specific node.
    ///
    /// When the system at `source_node` fails, execution will continue at the
    /// error handler node built by `handler` instead of propagating the error.
    ///
    /// # Arguments
    ///
    /// * `source_node` - The node ID to attach the error handler to
    /// * `handler` - Builder function that creates the error handling subgraph
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn risky_operation() -> Result<i32, String> { Ok(1) }
    /// # async fn fallback_operation() -> i32 { 2 }
    /// # let mut graph = Graph::new();
    /// let risky_id = graph.add_system_node(risky_operation);
    /// graph.add_error_handler(risky_id, |g| {
    ///     g.add_system(fallback_operation);
    /// });
    /// ```
    pub fn add_error_handler<F>(&mut self, source_node: NodeId, handler: F) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        // Build handler subgraph
        let mut handler_graph = Graph::new();
        handler(&mut handler_graph);

        // Get handler entry point
        if let Some(handler_entry) = handler_graph.entry {
            // Add error edge from source to handler
            let error_edge = Edge::Error(ErrorEdge::new(source_node, handler_entry));
            self.edges.push(error_edge);

            // Merge handler graph into main graph
            self.nodes.extend(handler_graph.nodes);
            self.edges.extend(handler_graph.edges);
        }

        self
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
    pub fn add_timeout_handler<F>(&mut self, source_node: NodeId, handler: F) -> &mut Self
    where
        F: FnOnce(&mut Graph),
    {
        // Build handler subgraph
        let mut handler_graph = Graph::new();
        handler(&mut handler_graph);

        // Get handler entry point
        if let Some(handler_entry) = handler_graph.entry {
            // Add timeout edge from source to handler
            let timeout_edge = Edge::Timeout(TimeoutEdge::new(source_node, handler_entry));
            self.edges.push(timeout_edge);

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
    pub fn set_timeout(&mut self, node_id: NodeId, timeout: core::time::Duration) -> &mut Self {
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

    // ─────────────────────────────────────────────────────────────────────────
    // Internal helpers
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a sequential edge between two nodes.
    fn add_sequential_edge(&mut self, from: NodeId, to: NodeId) {
        let edge = Edge::Sequential(SequentialEdge::new(from, to));
        self.edges.push(edge);
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Graph Analysis
    // ─────────────────────────────────────────────────────────────────────────

    /// Returns all nodes reachable from `entry` within a subgraph.
    ///
    /// Performs a DFS following sequential edges and control-flow internal
    /// links (decision branches, switch cases, loop bodies, nested parallel
    /// branches + join nodes). Uses a visited set to handle cycles.
    ///
    /// Subgraph boundaries are naturally respected: branch subgraphs built
    /// by the builder are self-contained (their terminal nodes have no
    /// outgoing sequential edges to the parent graph).
    fn reachable_nodes(&self, entry: &NodeId) -> Vec<&Node> {
        let mut visited = HashSet::new();
        let mut result = Vec::new();
        let mut stack = vec![entry.clone()];

        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            let Some(node) = self.get_node(current.clone()) else {
                continue;
            };

            result.push(node);

            // Follow control-flow internal links into subgraphs.
            // For Parallel, we must also push the join node since the
            // executor transitions to the join via the node's `join` field
            // rather than a sequential edge from the parallel node.
            match node {
                Node::Decision(dec) => {
                    if let Some(t) = &dec.true_branch {
                        stack.push(t.clone());
                    }
                    if let Some(f) = &dec.false_branch {
                        stack.push(f.clone());
                    }
                }
                Node::Switch(sw) => {
                    for (_, target) in &sw.cases {
                        stack.push(target.clone());
                    }
                    if let Some(d) = &sw.default {
                        stack.push(d.clone());
                    }
                }
                Node::Loop(lp) => {
                    if let Some(body) = &lp.body_entry {
                        stack.push(body.clone());
                    }
                }
                Node::Parallel(par) => {
                    for branch in &par.branches {
                        stack.push(branch.clone());
                    }
                    if let Some(join) = &par.join {
                        stack.push(join.clone());
                    }
                }
                Node::System(_) | Node::Join(_) => {}
            }

            // Follow sequential edges from this node
            for edge in &self.edges {
                if let Edge::Sequential(seq) = edge
                    && seq.from == current
                {
                    stack.push(seq.to.clone());
                }
            }
        }

        result
    }

    /// Collects output types produced by all system nodes reachable from `entry`.
    ///
    /// Returns `(TypeId, type_name)` pairs for each system node in the subgraph.
    fn collect_branch_output_types(&self, entry: &NodeId) -> Vec<(TypeId, &'static str)> {
        self.reachable_nodes(entry)
            .into_iter()
            .filter_map(|node| match node {
                Node::System(sys) => Some((sys.output_type_id(), sys.output_type_name())),
                _ => None,
            })
            .collect()
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Validation API
    // ─────────────────────────────────────────────────────────────────────────

    /// Validates the graph structure for correctness.
    ///
    /// This method performs build-time validation to catch errors before execution:
    /// - Verifies the graph has an entry point
    /// - Checks all edges reference valid nodes
    /// - Ensures decision nodes have predicates and both branches
    /// - Ensures loop nodes have termination conditions
    /// - Ensures parallel nodes have branches and join targets
    /// - Ensures switch nodes have discriminators
    /// - Warns if parallel branches produce overlapping output types
    /// - Errors if a loop predicate reads an output type no body system produces
    ///
    /// # Returns
    ///
    /// Returns `Ok(warnings)` if the graph is structurally valid (warnings may be
    /// empty), or `Err(errors)` for structural issues that would cause runtime failures.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn my_system() -> i32 { 1 }
    /// let mut graph = Graph::new();
    /// graph.add_system(my_system);
    ///
    /// match graph.validate() {
    ///     Ok(warnings) => {
    ///         for _w in &warnings {
    ///             // handle validation warnings (e.g. overlapping parallel outputs)
    ///         }
    ///     }
    ///     Err(errors) => {
    ///         for _err in &errors {
    ///             // handle validation errors that would cause runtime failures
    ///         }
    ///     }
    /// }
    /// ```
    pub fn validate(&self) -> Result<Vec<ValidationWarning>, Vec<ValidationError>> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Check for entry point
        if self.entry.is_none() {
            errors.push(ValidationError::NoEntryPoint);
        }

        // Build a set of valid node IDs for quick lookup
        let valid_nodes: HashSet<NodeId> = self.nodes.iter().map(Node::id).collect();

        // Validate entry point exists
        if let Some(entry) = &self.entry
            && !valid_nodes.contains(entry)
        {
            errors.push(ValidationError::InvalidEntryPoint(entry.clone()));
        }

        // Validate edges reference valid nodes
        for edge in &self.edges {
            self.validate_edge(edge, &valid_nodes, &mut errors);
        }

        // Validate each node
        for node in &self.nodes {
            self.validate_node(node, &valid_nodes, &mut errors, &mut warnings);
        }

        if errors.is_empty() {
            Ok(warnings)
        } else {
            Err(errors)
        }
    }

    /// Validates a single edge.
    ///
    /// # Edge Type Validation
    ///
    /// ## Sequential Edge (A → B)
    /// - `from` must reference an existing node
    /// - `to` must reference an existing node
    ///
    /// ## Conditional Edge (A → B if true, A → C if false)
    /// - `from` must reference an existing node (typically a `DecisionNode`)
    /// - `true_target` must reference an existing node
    /// - `false_target` must reference an existing node
    ///
    /// ## Parallel Edge (A → [B, C, D])
    /// - `from` must reference an existing node (typically a `ParallelNode`)
    /// - All `targets` must reference existing nodes
    ///
    /// ## `LoopBack` Edge (end → start)
    /// - `from` must reference an existing node (end of loop body)
    /// - `to` must reference an existing node (loop entry point)
    ///
    /// ## Error Edge (A → handler on failure)
    /// - `from` must reference an existing node (the node that may fail)
    /// - `to` must reference an existing node (error handler)
    ///
    /// ## Timeout Edge (A → handler on timeout)
    /// - `from` must reference an existing node (the node with timeout)
    /// - `to` must reference an existing node (timeout handler)
    fn validate_edge(
        &self,
        edge: &Edge,
        valid_nodes: &HashSet<NodeId>,
        errors: &mut Vec<ValidationError>,
    ) {
        match edge {
            // Sequential: simple A → B connection
            // Both source and target must exist
            Edge::Sequential(seq) => {
                if !valid_nodes.contains(&seq.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: seq.id.clone(),
                        node: seq.from.clone(),
                    });
                }
                if !valid_nodes.contains(&seq.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: seq.id.clone(),
                        node: seq.to.clone(),
                    });
                }
            }
            // Conditional: binary branch with true/false targets
            // Source and both targets must exist
            Edge::Conditional(cond) => {
                if !valid_nodes.contains(&cond.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: cond.id.clone(),
                        node: cond.from.clone(),
                    });
                }
                if !valid_nodes.contains(&cond.true_target) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: cond.id.clone(),
                        node: cond.true_target.clone(),
                    });
                }
                if !valid_nodes.contains(&cond.false_target) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: cond.id.clone(),
                        node: cond.false_target.clone(),
                    });
                }
            }
            // Parallel: fork to multiple targets
            // Source and all targets must exist
            Edge::Parallel(par) => {
                if !valid_nodes.contains(&par.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: par.id.clone(),
                        node: par.from.clone(),
                    });
                }
                for target in &par.targets {
                    if !valid_nodes.contains(target) {
                        errors.push(ValidationError::InvalidEdgeTarget {
                            edge: par.id.clone(),
                            node: target.clone(),
                        });
                    }
                }
            }
            // LoopBack: return to earlier node for iteration
            // Both source (loop body end) and target (loop entry) must exist
            Edge::LoopBack(lb) => {
                if !valid_nodes.contains(&lb.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: lb.id.clone(),
                        node: lb.from.clone(),
                    });
                }
                if !valid_nodes.contains(&lb.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: lb.id.clone(),
                        node: lb.to.clone(),
                    });
                }
            }
            // Error: fallback path when a system fails
            // Both the failing node and error handler must exist
            Edge::Error(err) => {
                if !valid_nodes.contains(&err.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: err.id.clone(),
                        node: err.from.clone(),
                    });
                }
                if !valid_nodes.contains(&err.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: err.id.clone(),
                        node: err.to.clone(),
                    });
                }
            }
            // Timeout: fallback path when a system times out
            // Both the timed-out node and timeout handler must exist
            Edge::Timeout(timeout) => {
                if !valid_nodes.contains(&timeout.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: timeout.id.clone(),
                        node: timeout.from.clone(),
                    });
                }
                if !valid_nodes.contains(&timeout.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: timeout.id.clone(),
                        node: timeout.to.clone(),
                    });
                }
            }
        }
    }

    /// Validates a single node.
    ///
    /// # Node Type Validation
    ///
    /// ## `SystemNode`
    /// - Always valid if it exists (no additional constraints)
    ///
    /// ## `DecisionNode`
    /// - Must have a predicate function
    /// - Must have both `true_branch` and `false_branch` targets
    /// - Branch targets must reference existing nodes
    ///
    /// ## `SwitchNode`
    /// - Must have a discriminator function
    /// - Must have at least one case or a default
    /// - All case targets must reference existing nodes
    /// - Default target (if present) must reference an existing node
    ///
    /// ## `ParallelNode`
    /// - Must have at least one branch
    /// - All branch targets must reference existing nodes
    /// - Must have a join node
    /// - Join target must reference an existing node
    ///
    /// ## `LoopNode`
    /// - Must have either a termination predicate or `max_iterations`
    /// - Must have a body entry point
    /// - Body entry must reference an existing node
    ///
    /// ## `JoinNode`
    /// - Must have at least one source
    /// - All sources must reference existing nodes
    fn validate_node(
        &self,
        node: &Node,
        valid_nodes: &HashSet<NodeId>,
        errors: &mut Vec<ValidationError>,
        warnings: &mut Vec<ValidationWarning>,
    ) {
        match node {
            // System nodes are always valid - they just wrap a boxed system
            Node::System(_) => {}

            // Decision nodes need a predicate and both branch targets
            Node::Decision(dec) => {
                if dec.predicate.is_none() {
                    errors.push(ValidationError::MissingPredicate {
                        node: dec.id.clone(),
                        name: dec.name,
                    });
                }
                if dec.true_branch.is_none() {
                    errors.push(ValidationError::MissingBranch {
                        node: dec.id.clone(),
                        name: dec.name,
                        branch: "true",
                    });
                } else if let Some(target) = &dec.true_branch
                    && !valid_nodes.contains(target)
                {
                    errors.push(ValidationError::InvalidBranchTarget {
                        node: dec.id.clone(),
                        branch: "true",
                        target: target.clone(),
                    });
                }
                if dec.false_branch.is_none() {
                    errors.push(ValidationError::MissingBranch {
                        node: dec.id.clone(),
                        name: dec.name,
                        branch: "false",
                    });
                } else if let Some(target) = &dec.false_branch
                    && !valid_nodes.contains(target)
                {
                    errors.push(ValidationError::InvalidBranchTarget {
                        node: dec.id.clone(),
                        branch: "false",
                        target: target.clone(),
                    });
                }
            }

            // Switch nodes need a discriminator and at least one case or default
            Node::Switch(sw) => {
                if sw.discriminator.is_none() {
                    errors.push(ValidationError::MissingDiscriminator {
                        node: sw.id.clone(),
                        name: sw.name,
                    });
                }
                if sw.cases.is_empty() && sw.default.is_none() {
                    errors.push(ValidationError::EmptySwitch {
                        node: sw.id.clone(),
                        name: sw.name,
                    });
                }
                for (case_name, target) in &sw.cases {
                    if !valid_nodes.contains(target) {
                        errors.push(ValidationError::InvalidCaseTarget {
                            node: sw.id.clone(),
                            case: case_name,
                            target: target.clone(),
                        });
                    }
                }
                if let Some(default) = &sw.default
                    && !valid_nodes.contains(default)
                {
                    errors.push(ValidationError::InvalidDefaultTarget {
                        node: sw.id.clone(),
                        target: default.clone(),
                    });
                }
            }

            // Parallel nodes need branches and a join point
            Node::Parallel(par) => {
                if par.branches.is_empty() {
                    errors.push(ValidationError::EmptyParallel {
                        node: par.id.clone(),
                        name: par.name,
                    });
                }
                for branch in &par.branches {
                    if !valid_nodes.contains(branch) {
                        errors.push(ValidationError::InvalidBranchTarget {
                            node: par.id.clone(),
                            branch: "parallel",
                            target: branch.clone(),
                        });
                    }
                }
                if par.join.is_none() {
                    errors.push(ValidationError::MissingJoin {
                        node: par.id.clone(),
                        name: par.name,
                    });
                } else if let Some(join) = &par.join
                    && !valid_nodes.contains(join)
                {
                    errors.push(ValidationError::InvalidJoinTarget {
                        node: par.id.clone(),
                        target: join.clone(),
                    });
                }

                // Check for overlapping output types across parallel branches.
                // If 2+ branches produce the same type, the last branch in
                // declaration order silently wins at merge time.
                let mut type_counts: HashMap<TypeId, (usize, &'static str)> = HashMap::new();
                for branch in &par.branches {
                    let branch_types: HashSet<_> = self
                        .collect_branch_output_types(branch)
                        .into_iter()
                        .collect();
                    for (type_id, type_name) in branch_types {
                        type_counts
                            .entry(type_id)
                            .and_modify(|(count, _)| *count += 1)
                            .or_insert((1, type_name));
                    }
                }
                for (_, (count, type_name)) in type_counts {
                    if count > 1 {
                        warnings.push(ValidationWarning::ConflictingParallelOutputs {
                            node: par.id.clone(),
                            name: par.name,
                            output_type: type_name,
                        });
                    }
                }
            }

            // Loop nodes need a termination condition and a body
            Node::Loop(lp) => {
                // Must have either termination predicate or max_iterations to prevent infinite loops
                if lp.termination.is_none() && lp.max_iterations.is_none() {
                    errors.push(ValidationError::NoTerminationCondition {
                        node: lp.id.clone(),
                        name: lp.name,
                    });
                }
                if lp.body_entry.is_none() {
                    errors.push(ValidationError::EmptyLoopBody {
                        node: lp.id.clone(),
                        name: lp.name,
                    });
                } else if let Some(body) = &lp.body_entry
                    && !valid_nodes.contains(body)
                {
                    errors.push(ValidationError::InvalidLoopBody {
                        node: lp.id.clone(),
                        target: body.clone(),
                    });
                }

                // If a termination predicate exists and the body is valid,
                // check that the predicate's input type is actually produced
                // by a system in the loop body.
                if let Some(term) = &lp.termination
                    && let Some(body) = &lp.body_entry
                    && valid_nodes.contains(body)
                {
                    let predicate_input = term.input_type_id();
                    let body_output_types: HashSet<TypeId> = self
                        .collect_branch_output_types(body)
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    if !body_output_types.contains(&predicate_input) {
                        errors.push(ValidationError::LoopPredicateOutputNotProduced {
                            node: lp.id.clone(),
                            name: lp.name,
                            expected_output: term.input_type_name(),
                        });
                    }
                }
            }

            // Join nodes need sources to aggregate
            Node::Join(join) => {
                if join.sources.is_empty() {
                    errors.push(ValidationError::EmptyJoinSources {
                        node: join.id.clone(),
                        name: join.name,
                    });
                }
                for source in &join.sources {
                    if !valid_nodes.contains(source) {
                        errors.push(ValidationError::InvalidJoinSource {
                            node: join.id.clone(),
                            source: source.clone(),
                        });
                    }
                }
            }
        }
    }
}

/// Warnings produced during graph validation.
///
/// Warnings indicate potential issues that won't prevent execution but may
/// cause unexpected behavior at runtime.
#[derive(Debug, Clone)]
pub enum ValidationWarning {
    /// Two or more parallel branches produce the same output type.
    /// The last branch in declaration order will win at merge time.
    ConflictingParallelOutputs {
        /// The parallel node ID.
        node: NodeId,
        /// The parallel node name.
        name: &'static str,
        /// The conflicting output type name.
        output_type: &'static str,
    },
}

impl fmt::Display for ValidationWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationWarning::ConflictingParallelOutputs {
                name,
                node,
                output_type,
            } => {
                write!(
                    f,
                    "parallel node '{name}' ({node}) has multiple branches producing output type '{output_type}'; last branch wins"
                )
            }
        }
    }
}

impl core::error::Error for ValidationWarning {}

/// Errors that can occur during graph validation.
///
/// These errors are detected at build time (when calling [`Graph::validate`])
/// before the graph is executed, allowing early detection of structural issues.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// The graph has no entry point.
    NoEntryPoint,
    /// The entry point references an invalid node.
    InvalidEntryPoint(NodeId),
    /// An edge's source node doesn't exist.
    InvalidEdgeSource {
        /// The edge ID.
        edge: EdgeId,
        /// The invalid node ID.
        node: NodeId,
    },
    /// An edge's target node doesn't exist.
    InvalidEdgeTarget {
        /// The edge ID.
        edge: EdgeId,
        /// The invalid node ID.
        node: NodeId,
    },
    /// A decision node is missing its predicate.
    MissingPredicate {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A decision node is missing a branch target.
    MissingBranch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
        /// Which branch is missing ("true" or "false").
        branch: &'static str,
    },
    /// A branch target references an invalid node.
    InvalidBranchTarget {
        /// The node ID.
        node: NodeId,
        /// The branch name.
        branch: &'static str,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A switch node is missing its discriminator.
    MissingDiscriminator {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A switch node has no cases and no default.
    EmptySwitch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A switch case target references an invalid node.
    InvalidCaseTarget {
        /// The node ID.
        node: NodeId,
        /// The case name.
        case: &'static str,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A switch default target references an invalid node.
    InvalidDefaultTarget {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A parallel node has no branches.
    EmptyParallel {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A parallel node is missing its join node.
    MissingJoin {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A join target references an invalid node.
    InvalidJoinTarget {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A loop node has no termination condition.
    NoTerminationCondition {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A loop node has no body.
    EmptyLoopBody {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A loop body entry references an invalid node.
    InvalidLoopBody {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A join node has no sources.
    EmptyJoinSources {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A join source references an invalid node.
    InvalidJoinSource {
        /// The node ID.
        node: NodeId,
        /// The invalid source node ID.
        source: NodeId,
    },
    /// A loop's termination predicate reads an output type that no system in
    /// the loop body produces.
    LoopPredicateOutputNotProduced {
        /// The loop node ID.
        node: NodeId,
        /// The loop node name.
        name: &'static str,
        /// The output type the predicate expects.
        expected_output: &'static str,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::NoEntryPoint => write!(f, "graph has no entry point"),
            ValidationError::InvalidEntryPoint(id) => {
                write!(f, "entry point references invalid node: {id}")
            }
            ValidationError::InvalidEdgeSource { edge, node } => {
                write!(f, "edge {edge} has invalid source node: {node}")
            }
            ValidationError::InvalidEdgeTarget { edge, node } => {
                write!(f, "edge {edge} has invalid target node: {node}")
            }
            ValidationError::MissingPredicate { node, name } => {
                write!(f, "decision node '{name}' ({node}) is missing predicate")
            }
            ValidationError::MissingBranch { node, name, branch } => {
                write!(
                    f,
                    "decision node '{name}' ({node}) is missing {branch} branch"
                )
            }
            ValidationError::InvalidBranchTarget {
                node,
                branch,
                target,
            } => {
                write!(
                    f,
                    "node {node} has {branch} branch pointing to invalid node: {target}"
                )
            }
            ValidationError::MissingDiscriminator { node, name } => {
                write!(f, "switch node '{name}' ({node}) is missing discriminator")
            }
            ValidationError::EmptySwitch { node, name } => {
                write!(
                    f,
                    "switch node '{name}' ({node}) has no cases and no default"
                )
            }
            ValidationError::InvalidCaseTarget { node, case, target } => {
                write!(
                    f,
                    "switch node {node} has case '{case}' pointing to invalid node: {target}"
                )
            }
            ValidationError::InvalidDefaultTarget { node, target } => {
                write!(
                    f,
                    "switch node {node} has default pointing to invalid node: {target}"
                )
            }
            ValidationError::EmptyParallel { node, name } => {
                write!(f, "parallel node '{name}' ({node}) has no branches")
            }
            ValidationError::MissingJoin { node, name } => {
                write!(f, "parallel node '{name}' ({node}) is missing join node")
            }
            ValidationError::InvalidJoinTarget { node, target } => {
                write!(
                    f,
                    "parallel node {node} has join pointing to invalid node: {target}"
                )
            }
            ValidationError::NoTerminationCondition { node, name } => {
                write!(
                    f,
                    "loop node '{name}' ({node}) has no termination condition (predicate or max_iterations)"
                )
            }
            ValidationError::EmptyLoopBody { node, name } => {
                write!(f, "loop node '{name}' ({node}) has no body")
            }
            ValidationError::InvalidLoopBody { node, target } => {
                write!(
                    f,
                    "loop node {node} has body entry pointing to invalid node: {target}"
                )
            }
            ValidationError::EmptyJoinSources { node, name } => {
                write!(f, "join node '{name}' ({node}) has no sources")
            }
            ValidationError::InvalidJoinSource { node, source } => {
                write!(
                    f,
                    "join node {node} has source pointing to invalid node: {source}"
                )
            }
            ValidationError::LoopPredicateOutputNotProduced {
                node,
                name,
                expected_output,
            } => {
                write!(
                    f,
                    "loop node '{name}' ({node}) predicate expects output type '{expected_output}' not produced by any system in the loop body"
                )
            }
        }
    }
}

impl core::error::Error for ValidationError {}
