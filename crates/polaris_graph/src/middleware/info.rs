//! Middleware info structs passed to middleware handlers.
//!
//! Each target has a corresponding info type that carries metadata about the
//! execution unit being wrapped. These are passed as the first parameter to
//! middleware handlers registered via [`MiddlewareAPI::register_system`](super::MiddlewareAPI::register_system).

use crate::NodeId;
use crate::node::ContextMode;

/// Metadata passed to [`GraphExecution`](super::GraphExecution) middleware.
#[derive(Debug, Clone)]
pub struct GraphInfo {
    /// Number of nodes in the graph.
    pub node_count: usize,
}

/// Metadata passed to [`System`](super::System) middleware.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    /// The node ID of the executing system.
    pub node_id: NodeId,
    /// The system's name.
    pub node_name: &'static str,
}

/// Metadata passed to [`Loop`](super::Loop) middleware.
#[derive(Debug, Clone)]
pub struct LoopInfo {
    /// The node ID of the loop node.
    pub node_id: NodeId,
    /// The loop's name.
    pub node_name: &'static str,
    /// The maximum iterations allowed.
    pub max_iterations: usize,
}

/// Metadata passed to [`Parallel`](super::Parallel) middleware.
#[derive(Debug, Clone)]
pub struct ParallelInfo {
    /// The node ID of the parallel node.
    pub node_id: NodeId,
    /// The parallel node's name.
    pub node_name: &'static str,
    /// Number of branches.
    pub branch_count: usize,
}

/// Metadata passed to [`Decision`](super::Decision) middleware.
#[derive(Debug, Clone)]
pub struct DecisionInfo {
    /// The node ID of the decision node.
    pub node_id: NodeId,
    /// The decision node's name.
    pub node_name: &'static str,
}

/// Metadata passed to [`Switch`](super::Switch) middleware.
#[derive(Debug, Clone)]
pub struct SwitchInfo {
    /// The node ID of the switch node.
    pub node_id: NodeId,
    /// The switch node's name.
    pub node_name: &'static str,
    /// Number of cases in the switch.
    pub case_count: usize,
    /// Whether a default case exists.
    pub has_default: bool,
}

/// Metadata passed to [`LoopIteration`](super::LoopIteration) middleware.
#[derive(Debug, Clone)]
pub struct LoopIterationInfo {
    /// The node ID of the loop node.
    pub node_id: NodeId,
    /// The loop's name.
    pub node_name: &'static str,
    /// Current iteration (0-indexed).
    pub iteration: usize,
    /// Maximum iterations allowed.
    pub max_iterations: usize,
}

/// Metadata passed to [`ParallelBranch`](super::ParallelBranch) middleware.
#[derive(Debug, Clone)]
pub struct ParallelBranchInfo {
    /// The node ID of the parallel node.
    pub node_id: NodeId,
    /// The parallel node's name.
    pub node_name: &'static str,
    /// Index of this branch (0-indexed).
    pub branch_index: usize,
    /// Total number of branches.
    pub branch_count: usize,
}

/// Metadata passed to [`Scope`](super::Scope) middleware.
#[derive(Debug, Clone)]
pub struct ScopeInfo {
    /// The node ID of the scope node.
    pub node_id: NodeId,
    /// The scope node's name.
    pub node_name: &'static str,
    /// The context mode for the scope.
    pub context_mode: ContextMode,
    /// Number of nodes in the embedded graph.
    pub inner_node_count: usize,
}
