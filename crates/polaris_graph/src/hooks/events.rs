//! Unified event enum for graph execution hooks.
//!
//! All hooks receive `&GraphEvent` and can match on variants for typed access.
//!
//! # Example
//!
//! ```
//! use polaris_graph::hooks::events::GraphEvent;
//!
//! fn handle_event(event: &GraphEvent) {
//!     match event {
//!         GraphEvent::SystemStart { node_id, node_name } => {
//!             println!("System {} starting at {:?}", node_name, node_id);
//!         }
//!         GraphEvent::SystemComplete { duration, .. } => {
//!             println!("Completed in {:?}", duration);
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use crate::{ExecutionError, node::NodeId};
use std::time::Duration;

/// Unified event enum for all graph execution hooks.
///
/// All hooks receive `&GraphEvent` and can match on variants for typed access.
/// This design provides:
/// - Simple multi-schedule registration (all hooks receive the same type)
/// - Typed access via pattern matching
#[derive(Debug, Clone)]
pub enum GraphEvent {
    // ─────────────────────────────────────────────────────────────────────────
    // Graph-Level Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event fired before graph execution begins.
    GraphStart {
        /// Number of nodes in the graph.
        node_count: usize,
        /// Node ID to name mapping for all nodes in the graph.
        node_map: Vec<(NodeId, &'static str)>,
    },

    /// Event fired after graph execution completes.
    GraphComplete {
        /// Number of nodes executed.
        nodes_executed: usize,
        /// Total execution duration.
        duration: Duration,
    },

    /// Event fired when graph execution fails with an error.
    GraphFailure {
        /// Error details, if available (e.g., stack trace).
        error: ExecutionError,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // System Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a system starts execution.
    SystemStart {
        /// The node ID of the executing system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
    },

    /// Event emitted after a system completes successfully.
    SystemComplete {
        /// The node ID of the completed system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
        /// How long the system took to execute.
        duration: Duration,
    },

    /// Event emitted when a system fails.
    SystemError {
        /// The node ID of the failed system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
        /// The error message.
        error: String,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // Decision Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a decision node evaluates its predicate.
    DecisionStart {
        /// The node ID of the decision node.
        node_id: NodeId,
        /// The decision node's name.
        node_name: &'static str,
    },

    /// Event emitted after a decision branch is selected and executed.
    DecisionComplete {
        /// The node ID of the decision node.
        node_id: NodeId,
        /// The decision node's name.
        node_name: &'static str,
        /// The branch that was selected ("true" or "false").
        selected_branch: &'static str,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // Switch Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a switch node evaluates its discriminator.
    SwitchStart {
        /// The node ID of the switch node.
        node_id: NodeId,
        /// The switch node's name.
        node_name: &'static str,
        /// Number of cases in the switch.
        case_count: usize,
        /// Whether a default case exists.
        has_default: bool,
    },

    /// Event emitted after a switch case is selected and executed.
    SwitchComplete {
        /// The node ID of the switch node.
        node_id: NodeId,
        /// The switch node's name.
        node_name: &'static str,
        /// The case key that was selected.
        selected_case: &'static str,
        /// Whether the default case was used.
        used_default: bool,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // Loop Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a loop begins execution.
    LoopStart {
        /// The node ID of the loop node.
        node_id: NodeId,
        /// The loop's name.
        node_name: &'static str,
        /// The maximum iterations allowed.
        max_iterations: usize,
    },

    /// Event emitted at the start of each loop iteration.
    LoopIteration {
        /// The node ID of the loop node.
        node_id: NodeId,
        /// The loop's name.
        node_name: &'static str,
        /// The current iteration number (0-indexed).
        iteration: usize,
    },

    /// Event emitted after a loop completes all iterations.
    LoopEnd {
        /// The node ID of the loop node.
        node_id: NodeId,
        /// The loop's name.
        node_name: &'static str,
        /// The total number of iterations executed.
        iterations: usize,
        /// Total nodes executed across all iterations.
        nodes_executed: usize,
        /// Total duration for the loop.
        duration: Duration,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // Parallel Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before parallel branches start execution.
    ParallelStart {
        /// The node ID of the parallel node.
        node_id: NodeId,
        /// The parallel node's name.
        node_name: &'static str,
        /// The number of parallel branches.
        branch_count: usize,
    },

    /// Event emitted after all parallel branches complete.
    ParallelComplete {
        /// The node ID of the parallel node.
        node_id: NodeId,
        /// The parallel node's name.
        node_name: &'static str,
        /// The number of parallel branches.
        branch_count: usize,
        /// Total nodes executed across all branches.
        total_nodes_executed: usize,
        /// Total duration for parallel execution.
        duration: Duration,
    },
}

impl GraphEvent {
    /// Returns the schedule name for this event variant.
    ///
    /// This corresponds to the schedule marker type name (e.g., `OnSystemStart`).
    #[must_use]
    pub fn schedule_name(&self) -> &'static str {
        match self {
            GraphEvent::GraphStart { .. } => "OnGraphStart",
            GraphEvent::GraphComplete { .. } => "OnGraphComplete",
            GraphEvent::GraphFailure { .. } => "OnGraphFailure",
            GraphEvent::SystemStart { .. } => "OnSystemStart",
            GraphEvent::SystemComplete { .. } => "OnSystemComplete",
            GraphEvent::SystemError { .. } => "OnSystemError",
            GraphEvent::DecisionStart { .. } => "OnDecisionStart",
            GraphEvent::DecisionComplete { .. } => "OnDecisionComplete",
            GraphEvent::SwitchStart { .. } => "OnSwitchStart",
            GraphEvent::SwitchComplete { .. } => "OnSwitchComplete",
            GraphEvent::LoopStart { .. } => "OnLoopStart",
            GraphEvent::LoopIteration { .. } => "OnLoopIteration",
            GraphEvent::LoopEnd { .. } => "OnLoopEnd",
            GraphEvent::ParallelStart { .. } => "OnParallelStart",
            GraphEvent::ParallelComplete { .. } => "OnParallelComplete",
        }
    }

    /// Returns the node ID if this is a node-level event.
    ///
    /// Graph-level events (like `GraphStart`, `GraphComplete`) return `None`.
    /// Node-specific events return `Some(node_id)`.
    #[must_use]
    pub fn node_id(&self) -> Option<NodeId> {
        match self {
            GraphEvent::GraphStart { .. }
            | GraphEvent::GraphComplete { .. }
            | GraphEvent::GraphFailure { .. } => None,
            GraphEvent::SystemStart { node_id, .. }
            | GraphEvent::SystemComplete { node_id, .. }
            | GraphEvent::SystemError { node_id, .. }
            | GraphEvent::DecisionStart { node_id, .. }
            | GraphEvent::DecisionComplete { node_id, .. }
            | GraphEvent::SwitchStart { node_id, .. }
            | GraphEvent::SwitchComplete { node_id, .. }
            | GraphEvent::LoopStart { node_id, .. }
            | GraphEvent::LoopIteration { node_id, .. }
            | GraphEvent::LoopEnd { node_id, .. }
            | GraphEvent::ParallelStart { node_id, .. }
            | GraphEvent::ParallelComplete { node_id, .. } => Some(node_id.clone()),
        }
    }
}

impl std::fmt::Display for GraphEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphEvent::GraphStart {
                node_count,
                node_map,
            } => {
                write!(f, "GraphStart(nodes: {})", node_count)?;
                if !node_map.is_empty() {
                    writeln!(f, ", node_map:")?;
                    for (id, name) in node_map {
                        writeln!(f, "  {:20} {}", name, id)?;
                    }
                };
                Ok(())
            }
            GraphEvent::GraphComplete {
                nodes_executed,
                duration,
            } => {
                write!(
                    f,
                    "GraphComplete(executed: {}, duration: {:?})",
                    nodes_executed, duration
                )
            }
            GraphEvent::GraphFailure { error } => {
                write!(f, "GraphFailure(error: {})", error)
            }
            GraphEvent::SystemStart {
                node_id,
                node_name: system_name,
            } => {
                write!(f, "SystemStart({} @ {:?})", system_name, node_id)
            }
            GraphEvent::SystemComplete {
                node_id,
                node_name: system_name,
                duration,
            } => {
                write!(
                    f,
                    "SystemComplete({} @ {:?}, duration: {:?})",
                    system_name, node_id, duration
                )
            }
            GraphEvent::SystemError {
                node_id,
                node_name: system_name,
                error,
            } => {
                write!(
                    f,
                    "SystemError({} @ {:?}, error: {})",
                    system_name, node_id, error
                )
            }
            GraphEvent::DecisionStart { node_id, node_name } => {
                write!(f, "DecisionStart({} @ {:?})", node_name, node_id)
            }
            GraphEvent::DecisionComplete {
                node_id,
                node_name,
                selected_branch,
            } => {
                write!(
                    f,
                    "DecisionComplete({} @ {:?}, branch: {})",
                    node_name, node_id, selected_branch
                )
            }
            GraphEvent::SwitchStart {
                node_id,
                node_name,
                case_count,
                has_default,
            } => {
                write!(
                    f,
                    "SwitchStart({} @ {:?}, cases: {}, default: {})",
                    node_name, node_id, case_count, has_default
                )
            }
            GraphEvent::SwitchComplete {
                node_id,
                node_name,
                selected_case,
                used_default,
            } => {
                write!(
                    f,
                    "SwitchComplete({} @ {:?}, case: {}, used_default: {})",
                    node_name, node_id, selected_case, used_default
                )
            }
            GraphEvent::LoopStart {
                node_id,
                node_name: loop_name,
                max_iterations,
            } => {
                write!(
                    f,
                    "LoopStart({} @ {:?}, max_iterations: {})",
                    loop_name, node_id, max_iterations
                )
            }
            GraphEvent::LoopIteration {
                node_id,
                node_name: loop_name,
                iteration,
            } => {
                write!(
                    f,
                    "LoopIteration({} @ {:?}, iteration: {})",
                    loop_name, node_id, iteration
                )
            }
            GraphEvent::LoopEnd {
                node_id,
                node_name: loop_name,
                iterations,
                nodes_executed,
                duration,
            } => {
                write!(
                    f,
                    "LoopEnd({} @ {:?}, iterations: {}, executed: {}, duration: {:?})",
                    loop_name, node_id, iterations, nodes_executed, duration
                )
            }
            GraphEvent::ParallelStart {
                node_id,
                node_name,
                branch_count,
            } => {
                write!(
                    f,
                    "ParallelStart({} @ {:?}, branches: {})",
                    node_name, node_id, branch_count
                )
            }
            GraphEvent::ParallelComplete {
                node_id,
                node_name,
                branch_count,
                total_nodes_executed,
                duration,
            } => {
                write!(
                    f,
                    "ParallelComplete({} @ {:?}, branches: {}, executed: {}, duration: {:?})",
                    node_name, node_id, branch_count, total_nodes_executed, duration
                )
            }
        }
    }
}
