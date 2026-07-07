//! Graph structure and builder API.
//!
//! The `Graph` is the core data structure representing an agent's behavior
//! as a directed graph of systems and control flow constructs.

mod builder;
mod signature;
mod validation;

use crate::edge::{Edge, EdgeId, SequentialEdge};
use crate::node::{Node, NodeId, SystemNode, remap_node_id};
pub use builder::SystemNodeBuilder;
use hashbrown::{HashMap, HashSet};
pub use signature::{GraphSignature, SignatureDiff};
use std::any::TypeId;
use std::time::Duration;
pub use validation::{MergeError, ValidationError, ValidationResult, ValidationWarning};

/// A directed graph of systems.
///
/// Graphs are the fundamental structure for composing safe agentic behavior.
/// Each graph contains:
/// - **Nodes**: Computation units (systems) and control flow constructs
/// - **Edges**: Connections defining execution flow between nodes
/// - **Entry**: The starting point for graph execution
///
/// By default, a graph has no execution time limit (`max_duration` is `None`).
/// Use [`with_max_duration`](Graph::with_max_duration) to set one.
///
/// # Example
///
/// ```
/// # use polaris_graph::Graph;
/// # async fn reason() { }
/// # async fn decide() { }
/// # async fn invoke_tool() { }
/// # async fn respond() { }
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
    pub(crate) nodes: Vec<Node>,
    /// All edges connecting nodes.
    pub(crate) edges: Vec<Edge>,
    /// Entry point for graph execution.
    pub(crate) entry: Option<NodeId>,
    /// The last node added (for chaining).
    pub(crate) last_node: Option<NodeId>,
    /// Maximum total execution duration for this graph.
    ///
    /// When set, the executor wraps this graph's execution in a timeout.
    /// If exceeded, returns [`ExecutionError::GraphTimeout`](crate::executor::ExecutionError::GraphTimeout).
    /// This takes precedence over the executor's own `max_duration`.
    pub(crate) max_duration: Option<Duration>,
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

    /// Returns the last node added to the graph, if any.
    #[must_use]
    pub fn last_node(&self) -> Option<NodeId> {
        self.last_node.clone()
    }

    /// Returns the maximum execution duration for this graph, if set.
    #[must_use]
    pub fn max_duration(&self) -> Option<Duration> {
        self.max_duration
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

    /// Finds the first node whose [`name`](Node::name) equals `name`.
    ///
    /// Names are *not* unique. The branch subgraphs the builder produces
    /// (decision branches, switch cases, loop bodies, parallel branches) live
    /// in the same flat node list as the top level, so two nodes can share a
    /// name. This returns the first match in insertion order; use
    /// [`find_nodes_by_name`](Self::find_nodes_by_name) to retrieve every match.
    ///
    /// The search covers this graph's own nodes only — it does not descend into
    /// a [`Scope`](crate::node::ScopeNode) node's embedded graph, mirroring
    /// [`nodes`](Self::nodes) and [`get_node`](Self::get_node).
    ///
    /// A system node's name is its system's function name (a node added with
    /// `add_system(reason)` is named `"reason"`); a control-flow node's name is
    /// the label passed to the builder.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::Graph;
    ///
    /// async fn reason() -> i32 { 1 }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut graph = Graph::new();
    /// graph.add_system(reason);
    ///
    /// let node = graph
    ///     .find_node_by_name("reason")
    ///     .ok_or_else(|| std::io::Error::other("missing reason node"))?;
    /// assert_eq!(node.name(), "reason");
    /// assert!(graph.find_node_by_name("missing").is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn find_node_by_name(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.name() == name)
    }

    /// Returns an iterator over every node whose [`name`](Node::name) equals
    /// `name`, in insertion order.
    ///
    /// Useful when a name is ambiguous across the builder's branch subgraphs —
    /// for example two parallel branches that each contain a `respond` system.
    /// The iterator is lazy and borrows the graph: `collect` it when you want an
    /// owned list, or drive it directly to count or take matches without
    /// allocating. It yields nothing when no node matches.
    ///
    /// Like [`find_node_by_name`](Self::find_node_by_name), this searches this
    /// graph's own nodes only and does not descend into
    /// [`Scope`](crate::node::ScopeNode) embedded graphs.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::Graph;
    ///
    /// async fn respond() -> i32 { 1 }
    ///
    /// let mut graph = Graph::new();
    /// graph.add_conditional_branch::<i32, _, _, _>(
    ///     "branch",
    ///     |value| *value > 0,
    ///     |g| { g.add_system(respond); },
    ///     |g| { g.add_system(respond); },
    /// );
    ///
    /// // Both branches contribute a `respond` node.
    /// assert_eq!(graph.find_nodes_by_name("respond").count(), 2);
    /// assert_eq!(graph.find_nodes_by_name("missing").count(), 0);
    /// ```
    pub fn find_nodes_by_name(&self, name: &str) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(move |node| node.name() == name)
    }

    /// Finds the first *system* node whose name equals `name`, returning its
    /// [`NodeId`] alongside the [`SystemNode`].
    ///
    /// Unlike [`find_node_by_name`](Self::find_node_by_name), this skips
    /// control-flow nodes (decisions, switches, loops, parallels, scopes) and
    /// matches only [`System`](Node::System) nodes — the ones carrying
    /// executable behavior you can inspect or, paired with
    /// [`duplicate`](Self::duplicate), transform.
    ///
    /// The [`NodeId`] is surfaced up front — rather than left for the caller to
    /// read off the returned node — because this lookup exists for rewiring: the
    /// id is the handle for retargeting edges to or from the node, and resolves
    /// through [`get_node`](Self::get_node). That is the asymmetry with
    /// [`find_node_by_name`](Self::find_node_by_name), whose callers typically
    /// inspect a node in place and so receive only `&Node`.
    ///
    /// Returns `None` when no system node has that name — including when a
    /// control-flow node carries the name but no system does.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::Graph;
    ///
    /// async fn reason() -> i32 { 1 }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut graph = Graph::new();
    /// graph.add_system(reason);
    ///
    /// let (id, system) = graph
    ///     .find_system_by_name("reason")
    ///     .ok_or_else(|| std::io::Error::other("missing reason system"))?;
    /// assert_eq!(system.name(), "reason");
    /// assert!(graph.get_node(id).is_some());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn find_system_by_name(&self, name: &str) -> Option<(NodeId, &SystemNode)> {
        self.nodes.iter().find_map(|node| match node {
            Node::System(system) if system.name() == name => Some((system.id.clone(), system)),
            _ => None,
        })
    }

    /// Duplicates this graph: returns a structurally independent copy with
    /// **fresh node and edge IDs**, ready to mutate without touching the
    /// original.
    ///
    /// Reach for this to explore variants of a base graph — duplicate it,
    /// rewire or extend the copy, and run both — instead of rebuilding from
    /// scratch.
    ///
    /// Every internal reference — branch targets, switch cases, loop bodies,
    /// parallel branches, and the [`entry`](Self::entry) / `last_node` markers —
    /// is remapped to the new IDs, so the clone is wired up exactly like the
    /// original but shares no identity with it. Adding, removing, or rewiring
    /// nodes in the clone does not affect the original (and vice versa). A
    /// [`Scope`](crate::node::ScopeNode) node's embedded graph is itself
    /// duplicated, recursively.
    ///
    /// This is named `duplicate` rather than [`Clone`] on purpose: node and
    /// edge IDs are *semantic identity*, and they intentionally change in the
    /// copy. The system, predicate, and discriminator *behavior* is immutable
    /// and is shared cheaply (via [`Arc`](std::sync::Arc)) rather than
    /// duplicated — the clone executes identically to the original.
    ///
    /// A graph that passes [`validate`](Self::validate) clones to a graph that
    /// also passes it.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::Graph;
    ///
    /// async fn step_a() -> i32 { 1 }
    /// async fn step_b() -> i32 { 2 }
    ///
    /// let mut original = Graph::new();
    /// original.add_system(step_a).add_system(step_b);
    ///
    /// let clone = original.duplicate();
    ///
    /// // Same shape, but fresh IDs and an independent topology.
    /// assert_eq!(clone.node_count(), original.node_count());
    /// assert_eq!(clone.edge_count(), original.edge_count());
    /// assert_ne!(clone.entry(), original.entry());
    /// assert!(clone.validate().is_ok());
    /// ```
    #[must_use]
    pub fn duplicate(&self) -> Graph {
        // One fresh NodeId per existing node, preserving node order. Built
        // up-front so every reference (own id, branch targets, edges,
        // entry/last_node) resolves against the same old → new mapping.
        let node_map: HashMap<NodeId, NodeId> = self
            .nodes
            .iter()
            .map(|node| (node.id(), NodeId::new()))
            .collect();

        Graph {
            nodes: self
                .nodes
                .iter()
                .map(|node| node.remap(&node_map))
                .collect(),
            edges: self
                .edges
                .iter()
                .map(|edge| edge.remap(&node_map))
                .collect(),
            entry: self.entry.as_ref().map(|id| remap_node_id(&node_map, id)),
            last_node: self
                .last_node
                .as_ref()
                .map(|id| remap_node_id(&node_map, id)),
            max_duration: self.max_duration,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Internal helpers
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a sequential edge between two nodes.
    pub(crate) fn add_sequential_edge(&mut self, from: NodeId, to: NodeId) {
        let edge = Edge::Sequential(SequentialEdge::new(from, to));
        self.edges.push(edge);
    }

    /// Returns `Ok(())` if every node is reachable from `entry`, or a
    /// [`MergeError::DisconnectedNodes`] listing the orphans.
    ///
    /// Handler subgraphs count as reachable: they hang off their source nodes
    /// by error/timeout edges alone, and the executor does run them, so a graph
    /// with error handlers is fully connected.
    pub(crate) fn check_connectivity(&self, entry: &NodeId) -> Result<(), MergeError> {
        let reachable: HashSet<NodeId> = self
            .reachable_nodes_with_handlers(entry)
            .iter()
            .map(|n| n.id())
            .collect();
        if reachable.len() == self.node_count() {
            return Ok(());
        }
        let orphans: Vec<NodeId> = self
            .nodes
            .iter()
            .map(Node::id)
            .filter(|id| !reachable.contains(id))
            .collect();
        Err(MergeError::DisconnectedNodes {
            orphan_count: orphans.len(),
            orphans,
        })
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Graph Analysis
    // ─────────────────────────────────────────────────────────────────────────

    /// Returns all nodes reachable from `entry` within a subgraph, following
    /// sequential edges and control-flow internal links only.
    ///
    /// Handler subgraphs (error/timeout edge targets) are **not** traversed —
    /// use [`reachable_nodes_with_handlers`](Self::reachable_nodes_with_handlers)
    /// when they must count, e.g. for interface derivation. This variant feeds
    /// the happy-path analyses such as
    /// [`collect_branch_output_types`](Self::collect_branch_output_types),
    /// where crediting an output that only materializes on a failure path would
    /// be wrong.
    pub(crate) fn reachable_nodes(&self, entry: &NodeId) -> Vec<&Node> {
        self.reachable_nodes_inner(entry, false)
    }

    /// Returns all nodes reachable from `entry`, additionally following
    /// error/timeout edges into handler subgraphs.
    ///
    /// Handler systems do run (on the failure path) and their outputs merge
    /// back like any other, so every analysis that answers "what *may* this
    /// subgraph touch" — signature derivation, nested-boundary detection,
    /// connectivity — must see them.
    pub(crate) fn reachable_nodes_with_handlers(&self, entry: &NodeId) -> Vec<&Node> {
        self.reachable_nodes_inner(entry, true)
    }

    /// DFS over sequential edges and control-flow internal links (decision
    /// branches, switch cases, loop bodies, nested parallel branches), with a
    /// visited set to handle cycles. With `follow_handlers`, error/timeout edge
    /// targets are traversed too.
    ///
    /// Subgraph boundaries are naturally respected: branch subgraphs built
    /// by the builder are self-contained (their terminal nodes have no
    /// outgoing sequential edges to the parent graph).
    fn reachable_nodes_inner(&self, entry: &NodeId, follow_handlers: bool) -> Vec<&Node> {
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
            // Note: This relies on the builder API to ensure that all nodes within
            // a branch subgraph are only reachable through the branch entry node.
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
                }
                // Scope and Dynamic nodes hold opaque embedded graph(s) — do not
                // recurse into them. The node itself is reachable; its inner
                // nodes live in a separate Graph (a scope's embedded graph, or a
                // dynamic node's candidates).
                Node::System(_) | Node::Scope(_) | Node::Dynamic(_) => {}
            }

            // Follow sequential edges from this node — and, when asked, the
            // error/timeout edges into handler subgraphs.
            for edge in &self.edges {
                match edge {
                    Edge::Sequential(seq) if seq.from == current => {
                        stack.push(seq.to.clone());
                    }
                    Edge::Error(err) if follow_handlers && err.from == current => {
                        stack.push(err.to.clone());
                    }
                    Edge::Timeout(t) if follow_handlers && t.from == current => {
                        stack.push(t.to.clone());
                    }
                    _ => {}
                }
            }
        }

        result
    }

    /// Returns the entry node IDs of handler subgraphs attached to `node_id` —
    /// the targets of its error/timeout edges.
    pub(crate) fn handler_entries(&self, node_id: &NodeId) -> Vec<NodeId> {
        self.edges
            .iter()
            .filter_map(|edge| match edge {
                Edge::Error(err) if err.from == *node_id => Some(err.to.clone()),
                Edge::Timeout(t) if t.from == *node_id => Some(t.to.clone()),
                _ => None,
            })
            .collect()
    }

    /// Returns `true` if any node reachable from the entry — including handler
    /// subgraphs — is a [`Scope`](Node::Scope) or [`Dynamic`](Node::Dynamic)
    /// boundary.
    ///
    /// Flat [signature](Graph::signature) derivation treats those boundaries as
    /// opaque, so a candidate containing one presents a signature that hides the
    /// IO crossing it — which is why such candidates are rejected as dynamic-node
    /// candidates until recursive derivation lands. Handler subgraphs are
    /// searched too: a boundary smuggled behind an error edge would otherwise
    /// evade the rejection and re-enter selection at runtime.
    pub(crate) fn contains_nested_boundary(&self) -> bool {
        let Some(entry) = self.entry() else {
            return false;
        };
        self.reachable_nodes_with_handlers(&entry)
            .iter()
            .any(|node| matches!(node, Node::Scope(_) | Node::Dynamic(_)))
    }

    /// Collects output types produced by all system nodes reachable from `entry`.
    ///
    /// Returns `(TypeId, type_name)` pairs for each system node in the subgraph.
    pub(crate) fn collect_branch_output_types(
        &self,
        entry: &NodeId,
    ) -> Vec<(TypeId, &'static str)> {
        self.reachable_nodes(entry)
            .into_iter()
            .filter_map(|node| match node {
                Node::System(sys) => Some((sys.output_type_id(), sys.output_type_name())),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use crate::node::{ContextPolicy, Node, ScopeNode};

    async fn out_i32() -> i32 {
        1
    }
    async fn out_string() -> String {
        "x".to_string()
    }
    async fn out_bool() -> bool {
        true
    }

    fn node_id_set(graph: &Graph) -> HashSet<NodeId> {
        graph.nodes().iter().map(Node::id).collect()
    }

    fn edge_id_set(graph: &Graph) -> HashSet<EdgeId> {
        graph.edges().iter().map(Edge::id).collect()
    }

    /// Returns the first `ScopeNode` found in a graph's top-level nodes.
    fn first_scope(graph: &Graph) -> &ScopeNode {
        graph
            .nodes()
            .iter()
            .find_map(|node| match node {
                Node::Scope(scope) => Some(scope),
                _ => None,
            })
            .expect("expected a scope node")
    }

    /// A representative graph touching every node variant — a system, a decision
    /// (conditional edge), a switch, a parallel fan-out (parallel edge), a loop
    /// (loop-back edge), and a scope. Error and timeout edges are covered
    /// separately by [`duplicate_remaps_error_and_timeout_edges`].
    fn graph_with_each_node_type() -> Graph {
        let mut inner = Graph::new();
        inner.add_system(out_i32).add_system(out_string);

        let mut graph = Graph::new();
        graph
            .add_system(out_i32)
            .add_conditional_branch::<i32, _, _, _>(
                "dec",
                |value| *value > 0,
                |branch| {
                    branch.add_system(out_string);
                },
                |branch| {
                    branch.add_system(out_bool);
                },
            )
            .add_switch::<i32, _, _, _>(
                "sw",
                |_| "a",
                vec![(
                    "a",
                    Box::new(|branch: &mut Graph| {
                        branch.add_system(out_string);
                    }) as Box<dyn FnOnce(&mut Graph)>,
                )],
                None::<Box<dyn FnOnce(&mut Graph)>>,
            )
            .add_parallel(
                "par",
                [
                    |branch: &mut Graph| {
                        branch.add_system(out_string);
                    },
                    |branch: &mut Graph| {
                        branch.add_system(out_bool);
                    },
                ],
            )
            .add_loop::<i32, _, _>(
                "loop",
                |value| *value > 5,
                |branch| {
                    branch.add_system(out_i32);
                },
            )
            .add_scope("scope", inner, ContextPolicy::shared());
        graph
    }

    #[test]
    fn duplicate_mints_fresh_node_and_edge_ids() {
        let original = graph_with_each_node_type();
        let clone = original.duplicate();

        assert_eq!(clone.node_count(), original.node_count());
        assert_eq!(clone.edge_count(), original.edge_count());
        assert!(
            node_id_set(&original).is_disjoint(&node_id_set(&clone)),
            "no cloned node id may collide with an original node id"
        );
        assert!(
            edge_id_set(&original).is_disjoint(&edge_id_set(&clone)),
            "no cloned edge id may collide with an original edge id"
        );
    }

    #[test]
    fn duplicate_remaps_decision_references() {
        let mut original = Graph::new();
        original.add_conditional_branch::<i32, _, _, _>(
            "dec",
            |value| *value > 0,
            |branch| {
                branch.add_system(out_string);
            },
            |branch| {
                branch.add_system(out_bool);
            },
        );
        let clone = original.duplicate();

        let decision = clone
            .nodes()
            .iter()
            .find_map(|node| match node {
                Node::Decision(dec) => Some(dec),
                _ => None,
            })
            .expect("decision node");
        let true_branch = decision.true_branch.clone().expect("true branch");
        let false_branch = decision.false_branch.clone().expect("false branch");

        // Branch targets point to fresh ids absent from the original…
        assert!(original.get_node(true_branch.clone()).is_none());
        assert!(original.get_node(false_branch.clone()).is_none());
        // …and resolve to the clone of the *correct* original target: the true
        // branch to `out_string`, the false branch to `out_bool`. A true/false
        // swap in the remap would fail these output-type checks.
        let true_node = clone
            .get_node(true_branch)
            .expect("true branch resolves in clone");
        assert!(
            matches!(true_node, Node::System(sys) if sys.output_type_id() == TypeId::of::<String>()),
            "true branch must remap to the clone of out_string",
        );
        let false_node = clone
            .get_node(false_branch)
            .expect("false branch resolves in clone");
        assert!(
            matches!(false_node, Node::System(sys) if sys.output_type_id() == TypeId::of::<bool>()),
            "false branch must remap to the clone of out_bool",
        );
    }

    #[test]
    fn duplicate_remaps_switch_references() {
        let mut original = Graph::new();
        original.add_switch::<i32, _, _, _>(
            "sw",
            |_| "a",
            vec![(
                "a",
                Box::new(|branch: &mut Graph| {
                    branch.add_system(out_string);
                }) as Box<dyn FnOnce(&mut Graph)>,
            )],
            Some(Box::new(|branch: &mut Graph| {
                branch.add_system(out_bool);
            }) as Box<dyn FnOnce(&mut Graph)>),
        );
        let clone = original.duplicate();

        let switch = clone
            .nodes()
            .iter()
            .find_map(|node| match node {
                Node::Switch(sw) => Some(sw),
                _ => None,
            })
            .expect("switch node");
        for (case_name, target) in &switch.cases {
            assert!(original.get_node(target.clone()).is_none());
            let node = clone
                .get_node(target.clone())
                .expect("case target resolves in clone");
            // The only case, "a", routes to `out_string`; the remap must keep it
            // pointing at the clone of that exact target.
            assert_eq!(*case_name, "a");
            assert!(
                matches!(node, Node::System(sys) if sys.output_type_id() == TypeId::of::<String>()),
                "case \"a\" must remap to the clone of out_string",
            );
        }
        let default = switch.default.clone().expect("default case");
        assert!(original.get_node(default.clone()).is_none());
        let default_node = clone
            .get_node(default)
            .expect("default case resolves in clone");
        assert!(
            matches!(default_node, Node::System(sys) if sys.output_type_id() == TypeId::of::<bool>()),
            "default case must remap to the clone of out_bool",
        );
    }

    #[test]
    fn duplicate_remaps_parallel_and_loop_references() {
        let mut original = Graph::new();
        original
            .add_parallel(
                "par",
                [
                    |branch: &mut Graph| {
                        branch.add_system(out_string);
                    },
                    |branch: &mut Graph| {
                        branch.add_system(out_bool);
                    },
                ],
            )
            .add_loop::<i32, _, _>(
                "loop",
                |value| *value > 5,
                |branch| {
                    branch.add_system(out_i32);
                },
            );
        let clone = original.duplicate();

        let parallel = clone
            .nodes()
            .iter()
            .find_map(|node| match node {
                Node::Parallel(par) => Some(par),
                _ => None,
            })
            .expect("parallel node");
        assert!(!parallel.branches.is_empty());
        let branch_types: HashSet<TypeId> = parallel
            .branches
            .iter()
            .map(|branch| {
                assert!(original.get_node(branch.clone()).is_none());
                let node = clone
                    .get_node(branch.clone())
                    .expect("parallel branch resolves in clone");
                match node {
                    Node::System(sys) => sys.output_type_id(),
                    other => panic!("parallel branch should be a system, got {other:?}"),
                }
            })
            .collect();
        // The two branches run `out_string` and `out_bool`; the remap must keep
        // both targets distinct rather than collapsing or swapping them.
        assert!(branch_types.contains(&TypeId::of::<String>()));
        assert!(branch_types.contains(&TypeId::of::<bool>()));

        let loop_node = clone
            .nodes()
            .iter()
            .find_map(|node| match node {
                Node::Loop(lp) => Some(lp),
                _ => None,
            })
            .expect("loop node");
        let body = loop_node.body_entry.clone().expect("loop body entry");
        assert!(original.get_node(body.clone()).is_none());
        let body_node = clone.get_node(body).expect("loop body resolves in clone");
        assert!(
            matches!(body_node, Node::System(sys) if sys.output_type_id() == TypeId::of::<i32>()),
            "loop body must remap to the clone of out_i32",
        );
    }

    #[test]
    fn duplicate_remaps_error_and_timeout_edges() {
        // The entry system carries both an error handler and a timeout handler,
        // so the clone must remap Error and Timeout edges in addition to the
        // structural edge variants the other tests cover.
        let mut original = Graph::new();
        let entry = original.add_system_node(out_i32);
        original.set_timeout(entry.clone(), Duration::from_millis(50));
        original.add_error_handler_for([entry.clone()], |branch| {
            branch.add_system(out_string);
        });
        original.add_timeout_handler([entry], |branch| {
            branch.add_system(out_bool);
        });

        let clone = original.duplicate();

        // Fresh edge ids — the error and timeout edges are remapped too.
        assert!(
            edge_id_set(&original).is_disjoint(&edge_id_set(&clone)),
            "cloned edge ids (error/timeout included) must not collide with the original",
        );

        let mut saw_error = false;
        let mut saw_timeout = false;
        for edge in clone.edges() {
            let (from, to) = match edge {
                Edge::Error(err) => {
                    saw_error = true;
                    (err.from.clone(), err.to.clone())
                }
                Edge::Timeout(timeout) => {
                    saw_timeout = true;
                    (timeout.from.clone(), timeout.to.clone())
                }
                _ => continue,
            };
            // Both endpoints were rewired to the clone's fresh node ids.
            assert!(clone.get_node(from.clone()).is_some());
            assert!(clone.get_node(to.clone()).is_some());
            assert!(original.get_node(from).is_none());
            assert!(original.get_node(to).is_none());
        }
        assert!(saw_error, "clone should contain a remapped error edge");
        assert!(saw_timeout, "clone should contain a remapped timeout edge");

        // A graph with error/timeout edges clones to one that still validates.
        assert!(clone.validate().is_ok());
    }

    #[test]
    fn duplicate_remaps_entry_and_last_node() {
        let mut original = Graph::new();
        original.add_system(out_i32).add_system(out_string);
        let clone = original.duplicate();

        let entry = clone.entry().expect("entry");
        assert!(clone.get_node(entry.clone()).is_some());
        assert!(original.get_node(entry).is_none());
        assert_ne!(clone.entry(), original.entry());

        let last = clone.last_node().expect("last node");
        assert!(clone.get_node(last).is_some());
        assert_ne!(clone.last_node(), original.last_node());
    }

    #[test]
    fn duplicate_is_independent_of_original() {
        let original = graph_with_each_node_type();
        let original_nodes = original.node_count();
        let original_edges = original.edge_count();

        let mut clone = original.duplicate();
        clone.add_system(out_i32);

        // Growing the clone leaves the original untouched.
        assert_eq!(original.node_count(), original_nodes);
        assert_eq!(original.edge_count(), original_edges);
        assert_eq!(clone.node_count(), original_nodes + 1);
    }

    #[test]
    fn duplicate_validates_with_each_node_type() {
        let clone = graph_with_each_node_type().duplicate();
        let result = clone.validate();
        assert!(
            result.is_ok(),
            "cloned graph should validate, got: {:?}",
            result.errors()
        );
    }

    #[test]
    fn duplicate_validates_nested_scopes() {
        let mut innermost = Graph::new();
        innermost.add_system(out_i32);
        let mut middle = Graph::new();
        middle.add_scope("inner", innermost, ContextPolicy::shared());
        let mut original = Graph::new();
        original.add_scope("outer", middle, ContextPolicy::shared());

        let clone = original.duplicate();
        assert!(clone.validate().is_ok());
    }

    #[test]
    fn duplicate_recurses_into_scope_with_fresh_inner_ids() {
        let mut inner = Graph::new();
        inner.add_system(out_i32).add_system(out_string);
        let mut original = Graph::new();
        original.add_scope("scope", inner, ContextPolicy::shared());

        let clone = original.duplicate();

        let original_inner = node_id_set(first_scope(&original).graph());
        let clone_inner = node_id_set(first_scope(&clone).graph());
        assert_eq!(original_inner.len(), clone_inner.len());
        assert!(
            original_inner.is_disjoint(&clone_inner),
            "the scope's embedded graph must be duplicated with fresh ids"
        );
    }

    #[test]
    fn duplicate_of_empty_graph_is_empty() {
        let clone = Graph::new().duplicate();
        assert!(clone.is_empty());
        assert_eq!(clone.entry(), None);
        assert_eq!(clone.last_node(), None);
    }

    #[test]
    fn duplicate_preserves_max_duration() {
        let mut original = Graph::new();
        original.add_system(out_i32);
        original.with_max_duration(Duration::from_secs(7));
        let clone = original.duplicate();
        assert_eq!(clone.max_duration(), Some(Duration::from_secs(7)));
    }
}

#[cfg(test)]
mod find_by_name_tests {
    use super::*;
    use crate::node::{ContextPolicy, Node};

    async fn reason() -> i32 {
        1
    }
    async fn respond() -> String {
        "x".to_string()
    }
    async fn reflect() -> bool {
        true
    }

    #[test]
    fn find_node_by_name_exact_match() {
        let mut graph = Graph::new();
        graph.add_system(reason);

        let node = graph.find_node_by_name("reason").expect("reason node");
        assert_eq!(node.name(), "reason");
    }

    #[test]
    fn find_node_by_name_no_match_is_none() {
        let mut graph = Graph::new();
        graph.add_system(reason);

        assert!(graph.find_node_by_name("missing").is_none());
    }

    #[test]
    fn find_nodes_by_name_no_match_is_empty() {
        let mut graph = Graph::new();
        graph.add_system(reason);

        assert_eq!(graph.find_nodes_by_name("missing").count(), 0);
    }

    #[test]
    fn find_nodes_by_name_multiple_across_subgraphs() {
        // Both branches of the decision add a `respond` system, so the name is
        // shared across two builder-generated subgraphs flattened into the same
        // node list.
        let mut graph = Graph::new();
        graph.add_conditional_branch::<i32, _, _, _>(
            "branch",
            |value| *value > 0,
            |sub| {
                sub.add_system(respond);
            },
            |sub| {
                sub.add_system(respond);
            },
        );

        let matches: Vec<&Node> = graph.find_nodes_by_name("respond").collect();
        assert_eq!(matches.len(), 2, "both branches contribute a respond node");
        assert!(matches.iter().all(|node| node.name() == "respond"));
        assert!(
            matches.iter().all(|node| matches!(node, Node::System(_))),
            "every match is a system node",
        );
    }

    #[test]
    fn find_node_by_name_returns_first_in_insertion_order() {
        // Two same-named systems in sequence; the lookup returns the earliest.
        let mut graph = Graph::new();
        let first = graph.add_system_node(respond);
        graph.add_system(respond);

        assert_eq!(graph.find_nodes_by_name("respond").count(), 2);
        let found = graph.find_node_by_name("respond").expect("respond node");
        assert_eq!(found.id(), first, "first match wins on insertion order");
    }

    #[test]
    fn find_system_by_name_returns_id_and_node() {
        let mut graph = Graph::new();
        let id = graph.add_system_node(reason);

        let (found_id, system) = graph.find_system_by_name("reason").expect("reason system");
        assert_eq!(found_id, id);
        assert_eq!(system.name(), "reason");
        // The returned id resolves to the same system node via get_node.
        let resolved = graph.get_node(found_id).expect("id resolves");
        assert!(matches!(resolved, Node::System(sys) if sys.name() == "reason"));
    }

    #[test]
    fn find_system_by_name_no_match_is_none() {
        let mut graph = Graph::new();
        graph.add_system(reason);

        assert!(graph.find_system_by_name("missing").is_none());
    }

    #[test]
    fn find_system_by_name_skips_control_flow_nodes() {
        // A loop and a decision carry builder labels but are not system nodes.
        let mut graph = Graph::new();
        graph
            .add_loop::<i32, _, _>(
                "tick",
                |value| *value > 5,
                |sub| {
                    sub.add_system(reason);
                },
            )
            .add_conditional_branch::<i32, _, _, _>(
                "decide",
                |value| *value > 0,
                |sub| {
                    sub.add_system(respond);
                },
                |sub| {
                    sub.add_system(reflect);
                },
            );

        // The control-flow labels resolve as nodes…
        assert!(graph.find_node_by_name("tick").is_some());
        assert!(graph.find_node_by_name("decide").is_some());
        // …but never as system nodes.
        assert!(graph.find_system_by_name("tick").is_none());
        assert!(graph.find_system_by_name("decide").is_none());
        // A real system inside a branch is still reachable by name.
        assert!(graph.find_system_by_name("reason").is_some());
    }

    #[test]
    fn find_nodes_by_name_yields_matches_in_insertion_order() {
        // Two same-named systems with distinguishable ids: the iterator must
        // yield them in the order they were inserted, not merely return the
        // right count.
        let mut graph = Graph::new();
        let first = graph.add_system_node(respond);
        let second = graph.add_system_node(respond);

        let ids: Vec<NodeId> = graph.find_nodes_by_name("respond").map(Node::id).collect();
        assert_eq!(
            ids,
            vec![first, second],
            "matches are yielded in insertion order",
        );
    }

    #[test]
    fn lookups_on_empty_graph_find_nothing() {
        let graph = Graph::new();

        assert!(graph.find_node_by_name("anything").is_none());
        assert_eq!(graph.find_nodes_by_name("anything").count(), 0);
        assert!(graph.find_system_by_name("anything").is_none());
    }

    #[test]
    fn lookups_do_not_descend_into_scope_embedded_graphs() {
        // A `reason` system lives inside a scope's embedded graph, and the
        // scope node itself is named `outer`.
        let mut inner = Graph::new();
        inner.add_system(reason);

        let mut graph = Graph::new();
        graph.add_scope("outer", inner, ContextPolicy::shared());

        // The scope node is a top-level node: found by name, but not a system.
        assert!(graph.find_node_by_name("outer").is_some());
        assert!(graph.find_system_by_name("outer").is_none());

        // The embedded `reason` system is surfaced by no lookup — the search
        // must not descend into the scope's embedded graph.
        assert!(
            graph.find_node_by_name("reason").is_none(),
            "lookup must not descend into a scope's embedded graph",
        );
        assert_eq!(graph.find_nodes_by_name("reason").count(), 0);
        assert!(graph.find_system_by_name("reason").is_none());
    }
}
