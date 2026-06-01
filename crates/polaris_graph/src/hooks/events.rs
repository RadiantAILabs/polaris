//! Unified event enum for graph execution hooks.
//!
//! All hooks receive `&GraphEvent` and can match on variants for typed access.
//!
//! # Run correlation
//!
//! Every variant carries a [`RunId`] (a fresh identifier minted by
//! [`GraphExecutor::execute`](crate::GraphExecutor::execute) per invocation)
//! and a [`RunLabels`] map (an opaque correlation bag populated by the caller).
//! Use the run id to group `GraphStart`/`SystemStart`/…/`GraphComplete` into a
//! single trace, and the labels to filter by application-level identifiers
//! such as `"session_id"` or `"agent_type"`.
//!
//! # Example
//!
//! ```
//! use polaris_graph::hooks::events::GraphEvent;
//!
//! fn handle_event(event: &GraphEvent) {
//!     let run = event.run_id();
//!     match event {
//!         GraphEvent::SystemStart { node_name, .. } => {
//!             println!("[{run}] system {node_name} starting");
//!         }
//!         GraphEvent::SystemComplete { duration, .. } => {
//!             println!("[{run}] completed in {:?}", duration);
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use crate::ExecutionError;
use crate::node::{ContextMode, NodeId};
use hashbrown::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Identifier minted per `GraphExecutor::execute*` call.
///
/// Each invocation of [`GraphExecutor::execute`](crate::GraphExecutor::execute)
/// or [`GraphExecutor::execute_with_labels`](crate::GraphExecutor::execute_with_labels)
/// mints a fresh `RunId`. Hook handlers use it to correlate all events from the
/// same run: `GraphStart`, every `SystemStart`/`SystemComplete` in between, and
/// the eventual `GraphComplete` or `GraphFailure` all carry the same id.
///
/// `RunId` is cheaply cloneable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(Arc<str>);

impl RunId {
    /// Generates a fresh run id.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::from(nanoid::nanoid!()))
    }

    /// Builds a `RunId` from an existing string.
    ///
    /// Intended for tests or for deserializing run identifiers minted
    /// elsewhere (e.g., when bridging events from another process).
    pub fn from_string(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for RunId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for RunId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self(Arc::from(raw.as_ref())))
    }
}

/// Opaque correlation labels attached to a graph execution.
///
/// `polaris_graph` (Layer 2) does not know about sessions, agents, or any
/// other higher-level concept. Layer 3 callers populate this map with
/// whatever identifiers their dashboards or log pipelines need to correlate.
/// Common keys (by convention, not enforcement):
///
/// - `"session_id"` — identifier of the owning session, when applicable.
/// - `"agent_type"` — name of the agent that constructed the graph.
///
/// The map is shared (`Arc`-backed) so each emitted [`GraphEvent`] carries
/// the labels cheaply by cloning the wrapper.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunLabels {
    inner: Arc<HashMap<String, String>>,
}

impl RunLabels {
    /// Returns an empty label set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Looks up a label by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(String::as_str)
    }

    /// Returns `true` when no labels are attached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of labels attached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Iterates over label `(key, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

impl<K, V, I> From<I> for RunLabels
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    fn from(iter: I) -> Self {
        let map: HashMap<String, String> = iter
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self {
            inner: Arc::new(map),
        }
    }
}

impl serde::Serialize for RunLabels {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (*self.inner).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for RunLabels {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = <HashMap<String, String> as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self {
            inner: Arc::new(map),
        })
    }
}

/// Unified event enum for all graph execution hooks.
///
/// All hooks receive `&GraphEvent` and can match on variants for typed access.
/// This design provides:
/// - Simple multi-schedule registration (all hooks receive the same type)
/// - Typed access via pattern matching
///
/// Every variant carries `run_id` (per-invocation) and `labels` (caller-supplied
/// correlation bag) for cross-cutting filtering and trace assembly. Most match
/// arms can ignore them with `..`; handlers that care about correlation read
/// them directly or via [`GraphEvent::run_id`] and [`GraphEvent::labels`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GraphEvent {
    // ─────────────────────────────────────────────────────────────────────────
    // Graph-Level Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event fired before graph execution begins.
    GraphStart {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// Number of nodes in the graph.
        node_count: usize,
        /// Node ID to name mapping for all nodes in the graph.
        node_map: Vec<(NodeId, &'static str)>,
    },

    /// Event fired after graph execution completes.
    GraphComplete {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// Number of nodes executed.
        nodes_executed: usize,
        /// Total execution duration.
        duration: Duration,
    },

    /// Event fired when graph execution fails with an error.
    GraphFailure {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// Error details, if available (e.g., stack trace).
        error: ExecutionError,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // System Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a system starts execution.
    SystemStart {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the executing system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
    },

    /// Event emitted after a system completes successfully.
    SystemComplete {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the completed system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
        /// How long the system took to execute.
        duration: Duration,
    },

    /// Event emitted when a system fails.
    SystemError {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the failed system.
        node_id: NodeId,
        /// The system's name.
        node_name: &'static str,
        /// The error message.
        error: Arc<str>,
    },

    // ─────────────────────────────────────────────────────────────────────────
    // Decision Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a decision node evaluates its predicate.
    DecisionStart {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the decision node.
        node_id: NodeId,
        /// The decision node's name.
        node_name: &'static str,
    },

    /// Event emitted after a decision branch is selected and executed.
    DecisionComplete {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
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
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
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
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
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
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the loop node.
        node_id: NodeId,
        /// The loop's name.
        node_name: &'static str,
        /// The maximum iterations allowed.
        max_iterations: usize,
    },

    /// Event emitted at the start of each loop iteration.
    LoopIteration {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the loop node.
        node_id: NodeId,
        /// The loop's name.
        node_name: &'static str,
        /// The current iteration number (0-indexed).
        iteration: usize,
    },

    /// Event emitted after a loop completes all iterations.
    LoopEnd {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
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
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the parallel node.
        node_id: NodeId,
        /// The parallel node's name.
        node_name: &'static str,
        /// The number of parallel branches.
        branch_count: usize,
    },

    /// Event emitted after all parallel branches complete.
    ParallelComplete {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
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

    // ─────────────────────────────────────────────────────────────────────────
    // Scope Events
    // ─────────────────────────────────────────────────────────────────────────
    /// Event emitted before a scope node begins execution.
    ScopeStart {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the scope node.
        node_id: NodeId,
        /// The scope node's name.
        node_name: &'static str,
        /// The context mode for the scope (e.g., "Shared", "Inherit", "Isolated").
        context_mode: ContextMode,
        /// Number of nodes in the embedded graph.
        inner_node_count: usize,
    },

    /// Event emitted after a scope node completes execution.
    ScopeComplete {
        /// Identifier of this execution run.
        run_id: RunId,
        /// Caller-supplied correlation labels.
        labels: RunLabels,
        /// The node ID of the scope node.
        node_id: NodeId,
        /// The scope node's name.
        node_name: &'static str,
        /// The context mode for the scope.
        context_mode: ContextMode,
        /// Total nodes executed inside the scope.
        nodes_executed: usize,
        /// Total duration for scope execution.
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
            GraphEvent::ScopeStart { .. } => "OnScopeStart",
            GraphEvent::ScopeComplete { .. } => "OnScopeComplete",
        }
    }

    /// Returns the run id carried by this event.
    ///
    /// All events emitted by a single `execute*` invocation share the same
    /// run id, allowing handlers to assemble a per-run trace.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        match self {
            GraphEvent::GraphStart { run_id, .. }
            | GraphEvent::GraphComplete { run_id, .. }
            | GraphEvent::GraphFailure { run_id, .. }
            | GraphEvent::SystemStart { run_id, .. }
            | GraphEvent::SystemComplete { run_id, .. }
            | GraphEvent::SystemError { run_id, .. }
            | GraphEvent::DecisionStart { run_id, .. }
            | GraphEvent::DecisionComplete { run_id, .. }
            | GraphEvent::SwitchStart { run_id, .. }
            | GraphEvent::SwitchComplete { run_id, .. }
            | GraphEvent::LoopStart { run_id, .. }
            | GraphEvent::LoopIteration { run_id, .. }
            | GraphEvent::LoopEnd { run_id, .. }
            | GraphEvent::ParallelStart { run_id, .. }
            | GraphEvent::ParallelComplete { run_id, .. }
            | GraphEvent::ScopeStart { run_id, .. }
            | GraphEvent::ScopeComplete { run_id, .. } => run_id,
        }
    }

    /// Returns the correlation labels carried by this event.
    #[must_use]
    pub fn labels(&self) -> &RunLabels {
        match self {
            GraphEvent::GraphStart { labels, .. }
            | GraphEvent::GraphComplete { labels, .. }
            | GraphEvent::GraphFailure { labels, .. }
            | GraphEvent::SystemStart { labels, .. }
            | GraphEvent::SystemComplete { labels, .. }
            | GraphEvent::SystemError { labels, .. }
            | GraphEvent::DecisionStart { labels, .. }
            | GraphEvent::DecisionComplete { labels, .. }
            | GraphEvent::SwitchStart { labels, .. }
            | GraphEvent::SwitchComplete { labels, .. }
            | GraphEvent::LoopStart { labels, .. }
            | GraphEvent::LoopIteration { labels, .. }
            | GraphEvent::LoopEnd { labels, .. }
            | GraphEvent::ParallelStart { labels, .. }
            | GraphEvent::ParallelComplete { labels, .. }
            | GraphEvent::ScopeStart { labels, .. }
            | GraphEvent::ScopeComplete { labels, .. } => labels,
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
            | GraphEvent::ParallelComplete { node_id, .. }
            | GraphEvent::ScopeStart { node_id, .. }
            | GraphEvent::ScopeComplete { node_id, .. } => Some(node_id.clone()),
        }
    }
}

impl std::fmt::Display for GraphEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let run = self.run_id();
        match self {
            GraphEvent::GraphStart {
                node_count,
                node_map,
                ..
            } => {
                write!(f, "[{run}] GraphStart(nodes: {node_count})")?;
                if !node_map.is_empty() {
                    writeln!(f, ", node_map:")?;
                    for (id, name) in node_map {
                        writeln!(f, "  {name:20} {id}")?;
                    }
                };
                Ok(())
            }
            GraphEvent::GraphComplete {
                nodes_executed,
                duration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] GraphComplete(executed: {nodes_executed}, duration: {duration:?})"
                )
            }
            GraphEvent::GraphFailure { error, .. } => {
                write!(f, "[{run}] GraphFailure(error: {error})")
            }
            GraphEvent::SystemStart {
                node_id, node_name, ..
            } => {
                write!(f, "[{run}] SystemStart({node_name} @ {node_id:?})")
            }
            GraphEvent::SystemComplete {
                node_id,
                node_name,
                duration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] SystemComplete({node_name} @ {node_id:?}, duration: {duration:?})"
                )
            }
            GraphEvent::SystemError {
                node_id,
                node_name,
                error,
                ..
            } => {
                write!(
                    f,
                    "[{run}] SystemError({node_name} @ {node_id:?}, error: {error})"
                )
            }
            GraphEvent::DecisionStart {
                node_id, node_name, ..
            } => {
                write!(f, "[{run}] DecisionStart({node_name} @ {node_id:?})")
            }
            GraphEvent::DecisionComplete {
                node_id,
                node_name,
                selected_branch,
                ..
            } => {
                write!(
                    f,
                    "[{run}] DecisionComplete({node_name} @ {node_id:?}, branch: {selected_branch})"
                )
            }
            GraphEvent::SwitchStart {
                node_id,
                node_name,
                case_count,
                has_default,
                ..
            } => {
                write!(
                    f,
                    "[{run}] SwitchStart({node_name} @ {node_id:?}, cases: {case_count}, default: {has_default})"
                )
            }
            GraphEvent::SwitchComplete {
                node_id,
                node_name,
                selected_case,
                used_default,
                ..
            } => {
                write!(
                    f,
                    "[{run}] SwitchComplete({node_name} @ {node_id:?}, case: {selected_case}, used_default: {used_default})"
                )
            }
            GraphEvent::LoopStart {
                node_id,
                node_name,
                max_iterations,
                ..
            } => {
                write!(
                    f,
                    "[{run}] LoopStart({node_name} @ {node_id:?}, max_iterations: {max_iterations})"
                )
            }
            GraphEvent::LoopIteration {
                node_id,
                node_name,
                iteration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] LoopIteration({node_name} @ {node_id:?}, iteration: {iteration})"
                )
            }
            GraphEvent::LoopEnd {
                node_id,
                node_name,
                iterations,
                nodes_executed,
                duration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] LoopEnd({node_name} @ {node_id:?}, iterations: {iterations}, executed: {nodes_executed}, duration: {duration:?})"
                )
            }
            GraphEvent::ParallelStart {
                node_id,
                node_name,
                branch_count,
                ..
            } => {
                write!(
                    f,
                    "[{run}] ParallelStart({node_name} @ {node_id:?}, branches: {branch_count})"
                )
            }
            GraphEvent::ParallelComplete {
                node_id,
                node_name,
                branch_count,
                total_nodes_executed,
                duration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] ParallelComplete({node_name} @ {node_id:?}, branches: {branch_count}, executed: {total_nodes_executed}, duration: {duration:?})"
                )
            }
            GraphEvent::ScopeStart {
                node_id,
                node_name,
                context_mode,
                inner_node_count,
                ..
            } => {
                write!(
                    f,
                    "[{run}] ScopeStart({node_name} @ {node_id:?}, mode: {context_mode}, inner_nodes: {inner_node_count})"
                )
            }
            GraphEvent::ScopeComplete {
                node_id,
                node_name,
                context_mode,
                nodes_executed,
                duration,
                ..
            } => {
                write!(
                    f,
                    "[{run}] ScopeComplete({node_name} @ {node_id:?}, mode: {context_mode}, executed: {nodes_executed}, duration: {duration:?})"
                )
            }
        }
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn run_id_round_trips_through_json() {
        let id = RunId::from_string("abc-123");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"abc-123\"");
        let restored: RunId = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, id);
    }

    #[test]
    fn run_labels_round_trip_through_json() {
        let labels = RunLabels::from([
            ("session_id", "s-1".to_owned()),
            ("agent_type", "react".to_owned()),
        ]);
        let json = serde_json::to_string(&labels).unwrap();
        let restored: RunLabels = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, labels);
        assert_eq!(restored.get("session_id"), Some("s-1"));
        assert_eq!(restored.get("agent_type"), Some("react"));
    }

    #[test]
    fn empty_run_labels_round_trip() {
        let labels = RunLabels::empty();
        let json = serde_json::to_string(&labels).unwrap();
        assert_eq!(json, "{}");
        let restored: RunLabels = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
    }
}
