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
