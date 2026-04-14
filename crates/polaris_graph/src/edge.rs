//! Edge types for graphs.
//!
//! Edges are the connections between nodes, defining control flow
//! through the graph.

use crate::node::NodeId;
use std::fmt;
use std::sync::Arc;

/// Unique identifier for an edge in the graph.
///
/// Edge IDs are generated using nanoid, providing globally unique identifiers
/// that don't require coordination between graph instances. This enables
/// merging graphs without ID collision handling.
///
/// Internally uses `Arc<str>` for cheap cloning (reference count bump only).
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::EdgeId;
///
/// // Auto-generated unique ID
/// let id = EdgeId::new();
/// assert!(!id.as_str().is_empty());
///
/// // From a known string (useful in tests)
/// let id = EdgeId::from_string("edge_1");
/// assert_eq!(id.as_str(), "edge_1");
///
/// // IDs are always unique
/// assert_ne!(EdgeId::new(), EdgeId::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeId(Arc<str>);

impl EdgeId {
    /// Creates a new edge ID with a unique nanoid.
    #[must_use]
    pub fn new() -> Self {
        Self(nanoid::nanoid!(8).into())
    }

    /// Creates an edge ID from a specific string value.
    ///
    /// This is primarily useful for testing or when restoring serialized graphs.
    #[must_use]
    pub fn from_string(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    /// Returns the ID as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for EdgeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "edge_{}", self.0)
    }
}

/// A connection between nodes defining control flow.
///
/// Edges determine how execution flows through the graph after
/// a node completes.
///
/// # Examples
///
/// Edges are created automatically by the [`Graph`](crate::graph::Graph) builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// async fn step_a() -> i32 { 1 }
/// async fn step_b() -> i32 { 2 }
///
/// let mut graph = Graph::new();
/// graph.add_system(step_a).add_system(step_b);
///
/// // The builder created a sequential edge between the two nodes
/// assert_eq!(graph.edges().len(), 1);
/// ```
#[derive(Debug)]
pub enum Edge {
    /// A -> B, output flows to input.
    Sequential(SequentialEdge),
    /// A -> B if predicate, else A -> C.
    Conditional(ConditionalEdge),
    /// A -> [B, C, D] concurrently.
    Parallel(ParallelEdge),
    /// Return to earlier node in graph.
    LoopBack(LoopBackEdge),
    /// Fallback path on failure.
    Error(ErrorEdge),
    /// Fallback path on timeout.
    Timeout(TimeoutEdge),
}

impl Edge {
    /// Returns the edge's ID.
    #[must_use]
    pub fn id(&self) -> EdgeId {
        match self {
            Edge::Sequential(edge) => edge.id.clone(),
            Edge::Conditional(edge) => edge.id.clone(),
            Edge::Parallel(edge) => edge.id.clone(),
            Edge::LoopBack(edge) => edge.id.clone(),
            Edge::Error(edge) => edge.id.clone(),
            Edge::Timeout(edge) => edge.id.clone(),
        }
    }

    /// Returns the source node ID.
    #[must_use]
    pub fn from(&self) -> NodeId {
        match self {
            Edge::Sequential(edge) => edge.from.clone(),
            Edge::Conditional(edge) => edge.from.clone(),
            Edge::Parallel(edge) => edge.from.clone(),
            Edge::LoopBack(edge) => edge.from.clone(),
            Edge::Error(edge) => edge.from.clone(),
            Edge::Timeout(edge) => edge.from.clone(),
        }
    }
}

/// A sequential edge: A -> B.
///
/// The simplest edge type, connecting one node to another
/// with output flowing directly to input.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::SequentialEdge;
/// use polaris_graph::NodeId;
///
/// let from = NodeId::from_string("step_1");
/// let to = NodeId::from_string("step_2");
/// let edge = SequentialEdge::new(from, to);
///
/// assert_eq!(edge.from.as_str(), "step_1");
/// assert_eq!(edge.to.as_str(), "step_2");
/// ```
#[derive(Debug)]
pub struct SequentialEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID.
    pub from: NodeId,
    /// Destination node ID.
    pub to: NodeId,
}

impl SequentialEdge {
    /// Creates a new sequential edge.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            to,
        }
    }
}

/// A conditional edge: A -> B if true, else A -> C.
///
/// Used with `DecisionNode` to implement binary branching.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::ConditionalEdge;
/// use polaris_graph::NodeId;
///
/// let decision = NodeId::from_string("check");
/// let true_target = NodeId::from_string("yes_branch");
/// let false_target = NodeId::from_string("no_branch");
///
/// let edge = ConditionalEdge::new(decision, true_target, false_target);
/// assert_eq!(edge.true_target.as_str(), "yes_branch");
/// assert_eq!(edge.false_target.as_str(), "no_branch");
/// ```
#[derive(Debug)]
pub struct ConditionalEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID (typically a `DecisionNode`).
    pub from: NodeId,
    /// Destination if condition is true.
    pub true_target: NodeId,
    /// Destination if condition is false.
    pub false_target: NodeId,
}

impl ConditionalEdge {
    /// Creates a new conditional edge.
    #[must_use]
    pub fn new(from: NodeId, true_target: NodeId, false_target: NodeId) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            true_target,
            false_target,
        }
    }
}

/// A parallel edge: A -> [B, C, D] concurrently.
///
/// Used with `ParallelNode` to fork execution into multiple paths.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::ParallelEdge;
/// use polaris_graph::NodeId;
///
/// let fork = NodeId::from_string("fork");
/// let targets = vec![
///     NodeId::from_string("branch_a"),
///     NodeId::from_string("branch_b"),
/// ];
///
/// let edge = ParallelEdge::new(fork, targets);
/// assert_eq!(edge.targets.len(), 2);
/// ```
#[derive(Debug)]
pub struct ParallelEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID (typically a `ParallelNode`).
    pub from: NodeId,
    /// Destination node IDs for each parallel branch.
    pub targets: Vec<NodeId>,
}

impl ParallelEdge {
    /// Creates a new parallel edge.
    #[must_use]
    pub fn new(from: NodeId, targets: Vec<NodeId>) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            targets,
        }
    }
}

/// A loop-back edge: return to earlier node.
///
/// Used with `LoopNode` to implement iteration.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::LoopBackEdge;
/// use polaris_graph::NodeId;
///
/// let body_end = NodeId::from_string("body_end");
/// let loop_entry = NodeId::from_string("loop_start");
///
/// let edge = LoopBackEdge::new(body_end, loop_entry);
/// assert_eq!(edge.from.as_str(), "body_end");
/// assert_eq!(edge.to.as_str(), "loop_start");
/// ```
#[derive(Debug)]
pub struct LoopBackEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID (end of loop body).
    pub from: NodeId,
    /// Target node ID (loop entry point).
    pub to: NodeId,
}

impl LoopBackEdge {
    /// Creates a new loop-back edge.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            to,
        }
    }
}

/// An error edge: fallback path on failure.
///
/// Provides an alternative execution path when a node fails.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::ErrorEdge;
/// use polaris_graph::NodeId;
///
/// let risky = NodeId::from_string("risky_call");
/// let handler = NodeId::from_string("error_handler");
///
/// let edge = ErrorEdge::new(risky, handler);
/// assert_eq!(edge.from.as_str(), "risky_call");
/// assert_eq!(edge.to.as_str(), "error_handler");
/// ```
#[derive(Debug)]
pub struct ErrorEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID (the node that may fail).
    pub from: NodeId,
    /// Target node ID (error handler).
    pub to: NodeId,
}

impl ErrorEdge {
    /// Creates a new error edge.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            to,
        }
    }
}

/// A timeout edge: fallback path on timeout.
///
/// Provides an alternative execution path when a node times out.
///
/// # Examples
///
/// ```
/// use polaris_graph::edge::TimeoutEdge;
/// use polaris_graph::NodeId;
///
/// let slow = NodeId::from_string("slow_operation");
/// let handler = NodeId::from_string("timeout_handler");
///
/// let edge = TimeoutEdge::new(slow, handler);
/// assert_eq!(edge.from.as_str(), "slow_operation");
/// assert_eq!(edge.to.as_str(), "timeout_handler");
/// ```
#[derive(Debug)]
pub struct TimeoutEdge {
    /// Unique identifier for this edge.
    pub id: EdgeId,
    /// Source node ID (the node that may timeout).
    pub from: NodeId,
    /// Target node ID (timeout handler).
    pub to: NodeId,
}

impl TimeoutEdge {
    /// Creates a new timeout edge.
    #[must_use]
    pub fn new(from: NodeId, to: NodeId) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_id_uniqueness() {
        // Generated IDs should be unique
        let id1 = EdgeId::new();
        let id2 = EdgeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn sequential_edge_creation() {
        let from = NodeId::from_string("n1");
        let to = NodeId::from_string("n2");
        let edge = SequentialEdge::new(from.clone(), to.clone());
        // ID is auto-generated
        assert!(!edge.id.as_str().is_empty());
        assert_eq!(edge.from.as_str(), "n1");
        assert_eq!(edge.to.as_str(), "n2");
    }

    #[test]
    fn edge_enum_accessors() {
        let from = NodeId::from_string("n1");
        let to = NodeId::from_string("n2");
        let seq = Edge::Sequential(SequentialEdge::new(from, to));
        assert!(!seq.id().as_str().is_empty());
        assert_eq!(seq.from().as_str(), "n1");
    }
}
