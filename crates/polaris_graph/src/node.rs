//! Node types for graphs.
//!
//! Nodes are the vertices in a graph, representing units of computation
//! or control flow decisions.

use crate::predicate::BoxedPredicate;
use polaris_system::plugin::{IntoScheduleIds, ScheduleId};
use polaris_system::resource::LocalResource;
use polaris_system::system::{BoxedSystem, ErasedSystem, IntoSystem};
use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

/// Unique identifier for a node in the graph.
///
/// Node IDs are generated using nanoid, providing globally unique identifiers
/// that don't require coordination between graph instances. This enables
/// merging graphs without ID collision handling.
///
/// Internally uses `Arc<str>` for cheap cloning (reference count bump only).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(Arc<str>);

impl LocalResource for NodeId {}

impl NodeId {
    /// Creates a new node ID with a unique nanoid.
    #[must_use]
    pub fn new() -> Self {
        Self(nanoid::nanoid!().into())
    }

    /// Creates a node ID from a specific string value.
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

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node_{}", self.0)
    }
}

/// A node in the graph.
///
/// Each node represents either a computation unit (system) or a control flow
/// construct (decision, loop, parallel execution).
#[derive(Debug)]
pub enum Node {
    /// Executes a system function.
    System(SystemNode),
    /// Routes flow based on predicate (binary branch).
    Decision(DecisionNode),
    /// Routes flow based on discriminator (multi-way branch).
    Switch(SwitchNode),
    /// Executes multiple paths concurrently.
    Parallel(ParallelNode),
    /// Repeats subgraph until termination condition.
    Loop(LoopNode),
    /// Aggregates results from parallel paths.
    Join(JoinNode),
}

impl Node {
    /// Returns the node's ID.
    #[must_use]
    pub fn id(&self) -> NodeId {
        match self {
            Node::System(n) => n.id.clone(),
            Node::Decision(n) => n.id.clone(),
            Node::Switch(n) => n.id.clone(),
            Node::Parallel(n) => n.id.clone(),
            Node::Loop(n) => n.id.clone(),
            Node::Join(n) => n.id.clone(),
        }
    }

    /// Returns the node's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Node::System(n) => n.name(),
            Node::Decision(n) => n.name,
            Node::Switch(n) => n.name,
            Node::Parallel(n) => n.name,
            Node::Loop(n) => n.name,
            Node::Join(n) => n.name,
        }
    }
}

/// A node that executes a system function.
///
/// This is the most common node type, wrapping an async system function
/// that performs computation (LLM calls, tool invocations, etc.).
pub struct SystemNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// The boxed system to execute.
    pub system: BoxedSystem,
    /// Optional timeout for this system's execution.
    /// If set and exceeded, the executor will follow any timeout edge if present.
    pub timeout: Option<core::time::Duration>,
    /// Custom schedules attached to this system node.
    /// System lifecycle events are re-emitted on these schedules,
    /// allowing hooks to subscribe to events for this system only.
    pub schedules: Vec<ScheduleId>,
}

impl SystemNode {
    /// Creates a new system node from any type implementing [`ErasedSystem`].
    #[must_use]
    pub fn new<S: ErasedSystem>(system: S) -> Self {
        Self {
            id: NodeId::new(),
            system: Box::new(system),
            timeout: None,
            schedules: Vec::new(),
        }
    }

    /// Creates a new system node from an already-boxed system.
    #[must_use]
    pub fn new_boxed(system: BoxedSystem) -> Self {
        Self {
            id: NodeId::new(),
            system,
            timeout: None,
            schedules: Vec::new(),
        }
    }

    /// Sets the timeout for this system node.
    #[must_use]
    pub fn with_timeout(mut self, timeout: core::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the custom schedules for this system node.
    #[must_use]
    pub fn with_schedules(mut self, schedules: Vec<ScheduleId>) -> Self {
        self.schedules = schedules;
        self
    }

    /// Returns the system's name for debugging and tracing.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.system.name()
    }

    /// Returns the [`TypeId`] of this system's output type.
    #[must_use]
    pub fn output_type_id(&self) -> TypeId {
        self.system.output_type_id()
    }

    /// Returns the output type name for error messages.
    #[must_use]
    pub fn output_type_name(&self) -> &'static str {
        self.system.output_type_name()
    }
}

impl fmt::Debug for SystemNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemNode")
            .field("id", &self.id)
            .field("name", &self.name())
            .field("output_type", &self.output_type_name())
            .field("schedules", &self.schedules)
            .finish()
    }
}

/// A node that routes flow based on a boolean predicate.
///
/// Decision nodes implement binary branching: if the predicate returns true,
/// flow continues to the "true" branch; otherwise to the "false" branch.
pub struct DecisionNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The predicate that determines which branch to take.
    pub predicate: Option<BoxedPredicate>,
    /// Node ID for the true branch.
    pub true_branch: Option<NodeId>,
    /// Node ID for the false branch.
    pub false_branch: Option<NodeId>,
}

impl DecisionNode {
    /// Creates a new decision node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            predicate: None,
            true_branch: None,
            false_branch: None,
        }
    }

    /// Creates a new decision node with a predicate.
    #[must_use]
    pub fn with_predicate(name: &'static str, predicate: BoxedPredicate) -> Self {
        Self {
            id: NodeId::new(),
            name,
            predicate: Some(predicate),
            true_branch: None,
            false_branch: None,
        }
    }
}

impl fmt::Debug for DecisionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionNode")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("has_predicate", &self.predicate.is_some())
            .field("true_branch", &self.true_branch)
            .field("false_branch", &self.false_branch)
            .finish()
    }
}

/// A node that routes flow based on a discriminator value (multi-way branch).
///
/// Switch nodes generalize decision nodes to handle multiple cases,
/// similar to a match/switch statement.
pub struct SwitchNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The discriminator that determines which case to take.
    pub discriminator: Option<crate::predicate::BoxedDiscriminator>,
    /// Node IDs for each case, keyed by case name.
    pub cases: Vec<(&'static str, NodeId)>,
    /// Default case if no match.
    pub default: Option<NodeId>,
}

impl SwitchNode {
    /// Creates a new switch node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            discriminator: None,
            cases: Vec::new(),
            default: None,
        }
    }

    /// Creates a new switch node with a discriminator.
    #[must_use]
    pub fn with_discriminator(
        name: &'static str,
        discriminator: crate::predicate::BoxedDiscriminator,
    ) -> Self {
        Self {
            id: NodeId::new(),
            name,
            discriminator: Some(discriminator),
            cases: Vec::new(),
            default: None,
        }
    }
}

impl fmt::Debug for SwitchNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwitchNode")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("has_discriminator", &self.discriminator.is_some())
            .field("cases", &self.cases)
            .field("default", &self.default)
            .finish()
    }
}

/// A node that executes multiple paths concurrently.
///
/// Parallel nodes fork execution into multiple branches that run
/// simultaneously. Results are collected at a corresponding Join node.
#[derive(Debug)]
pub struct ParallelNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// Node IDs for each parallel branch.
    pub branches: Vec<NodeId>,
    /// Node ID of the join node that collects results.
    pub join: Option<NodeId>,
}

impl ParallelNode {
    /// Creates a new parallel node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            branches: Vec::new(),
            join: None,
        }
    }
}

/// A node that repeats a subgraph until a termination condition.
///
/// Loop nodes implement iterative execution patterns, repeating the
/// loop body until a termination predicate returns true or max iterations
/// is reached.
pub struct LoopNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The termination predicate (loop exits when this returns true).
    pub termination: Option<BoxedPredicate>,
    /// Maximum number of iterations (safety limit).
    pub max_iterations: Option<usize>,
    /// Entry point of the loop body.
    pub body_entry: Option<NodeId>,
}

impl LoopNode {
    /// Creates a new loop node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            termination: None,
            max_iterations: None,
            body_entry: None,
        }
    }

    /// Creates a new loop node with a termination predicate.
    #[must_use]
    pub fn with_termination(name: &'static str, termination: BoxedPredicate) -> Self {
        Self {
            id: NodeId::new(),
            name,
            termination: Some(termination),
            max_iterations: None,
            body_entry: None,
        }
    }

    /// Creates a new loop node with a maximum iteration count.
    #[must_use]
    pub fn with_max_iterations(name: &'static str, max_iterations: usize) -> Self {
        Self {
            id: NodeId::new(),
            name,
            termination: None,
            max_iterations: Some(max_iterations),
            body_entry: None,
        }
    }
}

impl fmt::Debug for LoopNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopNode")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("has_termination", &self.termination.is_some())
            .field("max_iterations", &self.max_iterations)
            .field("body_entry", &self.body_entry)
            .finish()
    }
}

/// A node that aggregates results from parallel paths.
///
/// Join nodes are the counterpart to Parallel nodes, collecting
/// results from all parallel branches before continuing execution.
#[derive(Debug)]
pub struct JoinNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// Node IDs of the parallel branches being joined.
    pub sources: Vec<NodeId>,
}

impl JoinNode {
    /// Creates a new join node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            sources: Vec::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IntoSystemNode
// ─────────────────────────────────────────────────────────────────────────────

/// Converts a type into the components needed for a [`SystemNode`].
///
/// Enables `add_system` to accept both bare systems and
/// `(custom_schedules, system)` tuples.
pub trait IntoSystemNode<Marker> {
    /// Converts into a boxed system and its custom schedules.
    fn into_system_node(self) -> (BoxedSystem, Vec<ScheduleId>);
}

/// Marker for bare system nodes.
pub struct NodeMarker<M>(PhantomData<M>);

/// Marker for system nodes with custom schedules attached.
pub struct ScheduledNodeMarker<M>(PhantomData<M>);

impl<S, M> IntoSystemNode<NodeMarker<M>> for S
where
    S: IntoSystem<M>,
    S::System: 'static,
{
    fn into_system_node(self) -> (BoxedSystem, Vec<ScheduleId>) {
        (Box::new(self.into_system()), Vec::new())
    }
}

impl<Sch, S, M> IntoSystemNode<ScheduledNodeMarker<M>> for (Sch, S)
where
    Sch: IntoScheduleIds,
    S: IntoSystem<M>,
    S::System: 'static,
{
    fn into_system_node(self) -> (BoxedSystem, Vec<ScheduleId>) {
        (Box::new(self.1.into_system()), Sch::schedule_ids())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::plugin::Schedule;
    use polaris_system::system::IntoSystem;

    // Test system functions
    async fn test_system() -> String {
        "hello".to_string()
    }

    async fn sys_fn() -> i32 {
        42
    }

    #[test]
    fn node_id_uniqueness() {
        // Generated IDs should be unique
        let id1 = NodeId::new();
        let id2 = NodeId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn system_node_creation() {
        let system = test_system.into_system();
        let node = SystemNode::new(system);
        // ID is auto-generated, just check it exists
        assert!(!node.id.as_str().is_empty());
        assert!(node.name().contains("test_system"));
    }

    #[test]
    fn node_enum_accessors() {
        let system = Node::System(SystemNode::new(sys_fn.into_system()));
        assert!(!system.id().as_str().is_empty());
        assert!(system.name().contains("sys_fn"));

        let decision = Node::Decision(DecisionNode::new("dec"));
        assert!(!decision.id().as_str().is_empty());
        assert_eq!(decision.name(), "dec");
    }

    #[test]
    fn system_node_preserves_type_info() {
        let system = sys_fn.into_system();
        let node = SystemNode::new(system);

        assert_eq!(node.output_type_id(), TypeId::of::<i32>());
        assert!(node.output_type_name().contains("i32"));
    }

    struct MarkerA;
    impl Schedule for MarkerA {}

    struct MarkerB;
    impl Schedule for MarkerB {}

    #[test]
    fn into_system_node_bare() {
        let (_, schedules) = sys_fn.into_system_node();
        assert!(schedules.is_empty());
    }

    #[test]
    fn into_system_node_single_schedule() {
        let (_, schedules) = (MarkerA, sys_fn).into_system_node();
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0], ScheduleId::of::<MarkerA>());
    }

    #[test]
    fn into_system_node_multi_schedules() {
        let (_, schedules) = ((MarkerA, MarkerB), sys_fn).into_system_node();
        assert_eq!(schedules.len(), 2);
        assert_eq!(schedules[0], ScheduleId::of::<MarkerA>());
        assert_eq!(schedules[1], ScheduleId::of::<MarkerB>());
    }

    #[test]
    fn system_node_with_schedules() {
        let node = SystemNode::new(sys_fn.into_system()).with_schedules(vec![
            ScheduleId::of::<MarkerA>(),
            ScheduleId::of::<MarkerB>(),
        ]);
        assert_eq!(node.schedules.len(), 2);
        assert_eq!(node.schedules[0], ScheduleId::of::<MarkerA>());
        assert_eq!(node.schedules[1], ScheduleId::of::<MarkerB>());
    }
}
