//! Graph structure and builder API.
//!
//! The `Graph` is the core data structure representing an agent's behavior
//! as a directed graph of systems and control flow constructs.

mod builder;
mod validation;

pub use builder::SystemNodeBuilder;
pub use validation::{MergeError, ValidationError, ValidationResult, ValidationWarning};

use crate::edge::{Edge, EdgeId, SequentialEdge};
use crate::node::{Node, NodeId};
use hashbrown::HashSet;
use std::any::TypeId;

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
    // Internal helpers
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds a sequential edge between two nodes.
    pub(crate) fn add_sequential_edge(&mut self, from: NodeId, to: NodeId) {
        let edge = Edge::Sequential(SequentialEdge::new(from, to));
        self.edges.push(edge);
    }

    /// Returns `Ok(())` if every node is reachable from `entry`, or a
    /// [`MergeError::DisconnectedNodes`] listing the orphans.
    pub(crate) fn check_connectivity(&self, entry: &NodeId) -> Result<(), MergeError> {
        let reachable: HashSet<NodeId> =
            self.reachable_nodes(entry).iter().map(|n| n.id()).collect();
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

    /// Returns all nodes reachable from `entry` within a subgraph.
    ///
    /// Performs a DFS following sequential edges and control-flow internal
    /// links (decision branches, switch cases, loop bodies, nested parallel
    /// branches). Uses a visited set to handle cycles.
    ///
    /// Subgraph boundaries are naturally respected: branch subgraphs built
    /// by the builder are self-contained (their terminal nodes have no
    /// outgoing sequential edges to the parent graph).
    pub(crate) fn reachable_nodes(&self, entry: &NodeId) -> Vec<&Node> {
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
                Node::System(_) => {}
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
