//! Node types for graphs.
//!
//! Nodes are the vertices in a graph, representing units of computation
//! or control flow decisions.

use crate::graph::Graph;
use crate::predicate::BoxedPredicate;
use core::any::Any;
use hashbrown::{HashMap, HashSet};
use polaris_system::plugin::{IntoScheduleIds, ScheduleId};
use polaris_system::resource::LocalResource;
use polaris_system::system::{BoxedSystem, ErasedSystem, IntoSystem};
use std::any::TypeId;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Unique identifier for a node in the graph.
///
/// Node IDs are generated using nanoid, providing globally unique identifiers
/// that don't require coordination between graph instances. This enables
/// merging graphs without ID collision handling.
///
/// Internally uses `Arc<str>` for cheap cloning (reference count bump only).
///
/// # Examples
///
/// ```
/// use polaris_graph::NodeId;
///
/// // Auto-generated unique ID
/// let id = NodeId::new();
/// assert!(!id.as_str().is_empty());
///
/// // From a known string (useful in tests)
/// let id = NodeId::from_string("my_node");
/// assert_eq!(id.as_str(), "my_node");
///
/// // IDs are always unique
/// assert_ne!(NodeId::new(), NodeId::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(Arc<str>);

impl LocalResource for NodeId {}

impl NodeId {
    /// Creates a new node ID with a unique nanoid.
    #[must_use]
    pub fn new() -> Self {
        Self(nanoid::nanoid!(8).into())
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

impl IntoIterator for NodeId {
    type Item = NodeId;
    type IntoIter = std::iter::Once<NodeId>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self)
    }
}

/// A node in the graph.
///
/// Each node represents either a computation unit (system) or a control flow
/// construct (decision, loop, parallel execution).
///
/// # Examples
///
/// Nodes are created through the [`Graph`] builder API rather than directly:
///
/// ```
/// use polaris_graph::Graph;
///
/// async fn reason() -> i32 { 1 }
/// async fn act() -> i32 { 2 }
///
/// let mut graph = Graph::new();
/// graph.add_system(reason).add_system(act);
///
/// // Access nodes after construction
/// for node in graph.nodes() {
///     let _id = node.id();
///     let _name = node.name();
/// }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum Node {
    /// Executes a system function.
    System(SystemNode),
    /// Routes flow based on predicate (binary branch).
    Decision(DecisionNode),
    /// Routes flow based on discriminator (multi-way branch).
    Switch(SwitchNode),
    /// Executes multiple paths of subgraphs concurrently.
    /// The parallel node is both the entry and exit point — after all branches
    /// complete, execution continues from the parallel node's outgoing edge.
    Parallel(ParallelNode),
    /// Repeats subgraph until termination condition.
    Loop(LoopNode),
    /// Executes an embedded graph with a configurable context boundary.
    Scope(ScopeNode),
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
            Node::Scope(n) => n.id.clone(),
        }
    }

    /// Returns the node's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Node::System(n) => n.name(),
            Node::Decision(n) => n.name,
            Node::Switch(n) => n.name,
            Node::Parallel(n) => n.name,
            Node::Loop(n) => n.name,
            Node::Scope(n) => n.name,
        }
    }
}

/// Retry policy for system nodes that may fail transiently.
///
/// When a system fails and has a retry policy, the executor retries
/// according to the policy before routing to error/timeout handlers.
///
/// # Examples
///
/// ```
/// use polaris_graph::RetryPolicy;
/// use std::time::Duration;
///
/// // Fixed delay: retry up to 3 times with 100ms between attempts
/// let fixed = RetryPolicy::fixed(3, Duration::from_millis(100));
/// assert_eq!(fixed.max_retries(), 3);
/// assert_eq!(fixed.delay_for_attempt(0), Duration::from_millis(100));
/// assert_eq!(fixed.delay_for_attempt(2), Duration::from_millis(100));
///
/// // Exponential backoff: 100ms, 200ms, 400ms, ... capped at 1s
/// let expo = RetryPolicy::exponential(5, Duration::from_millis(100))
///     .with_max_delay(Duration::from_secs(1));
/// assert_eq!(expo.delay_for_attempt(0), Duration::from_millis(100));
/// assert_eq!(expo.delay_for_attempt(3), Duration::from_millis(800));
/// assert_eq!(expo.delay_for_attempt(4), Duration::from_secs(1)); // capped
/// ```
#[derive(Debug, Clone)]
pub enum RetryPolicy {
    /// Fixed delay between retries.
    Fixed {
        /// Maximum number of retry attempts (not counting the initial attempt).
        max_retries: usize,
        /// Delay between attempts.
        delay: Duration,
    },
    /// Exponential backoff between retries.
    Exponential {
        /// Maximum number of retry attempts (not counting the initial attempt).
        max_retries: usize,
        /// Delay before the first retry.
        initial_delay: Duration,
        /// Maximum delay between retries (caps the exponential growth).
        max_delay: Option<Duration>,
    },
}

impl RetryPolicy {
    /// Creates a fixed-delay retry policy.
    #[must_use]
    pub fn fixed(max_retries: usize, delay: Duration) -> Self {
        RetryPolicy::Fixed { max_retries, delay }
    }

    /// Creates an exponential backoff retry policy.
    #[must_use]
    pub fn exponential(max_retries: usize, initial_delay: Duration) -> Self {
        RetryPolicy::Exponential {
            max_retries,
            initial_delay,
            max_delay: None,
        }
    }

    /// Sets the maximum delay (for exponential backoff).
    ///
    /// Has no effect on [`Fixed`](RetryPolicy::Fixed) policies.
    #[must_use]
    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        if let RetryPolicy::Exponential {
            max_delay: ref mut md,
            ..
        } = self
        {
            *md = Some(max_delay);
        }
        self
    }

    /// Returns the maximum number of retry attempts.
    #[must_use]
    pub fn max_retries(&self) -> usize {
        match self {
            RetryPolicy::Fixed { max_retries, .. }
            | RetryPolicy::Exponential { max_retries, .. } => *max_retries,
        }
    }

    /// Returns the delay for the given attempt number (0-indexed).
    ///
    /// Attempt 0 is the delay before the first retry (after the initial attempt fails).
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: usize) -> Duration {
        match self {
            RetryPolicy::Fixed { delay, .. } => *delay,
            RetryPolicy::Exponential {
                initial_delay,
                max_delay,
                ..
            } => {
                // 2^attempt, saturating on overflow (attempt >= 32)
                let multiplier = 1u32.checked_shl(attempt as u32);
                let delay = if let Some(m) = multiplier {
                    initial_delay.saturating_mul(m)
                } else {
                    max_delay.unwrap_or(Duration::MAX)
                };
                if let Some(cap) = max_delay {
                    delay.min(*cap)
                } else {
                    delay
                }
            }
        }
    }
}

/// A node that executes a system function.
///
/// This is the most common node type, wrapping an async system function
/// that performs computation (LLM calls, tool invocations, etc.).
///
/// # Examples
///
/// System nodes are typically created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// async fn call_llm() -> String { String::new() }
/// async fn parse_response() -> i32 { 42 }
///
/// let mut graph = Graph::new();
/// graph
///     .add_system(call_llm)
///     .add_system(parse_response);
/// ```
///
/// For low-level construction:
///
/// ```
/// use polaris_graph::node::SystemNode;
/// use polaris_system::system::IntoSystem;
///
/// async fn my_system() -> i32 { 42 }
///
/// let node = SystemNode::new(my_system.into_system());
/// assert!(node.name().contains("my_system"));
/// ```
pub struct SystemNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// The boxed system to execute.
    pub system: BoxedSystem,
    /// Optional timeout for this system's execution.
    /// If set and exceeded, the executor will follow any timeout edge if present.
    pub timeout: Option<Duration>,
    /// Optional retry policy for transient failures.
    pub retry_policy: Option<RetryPolicy>,
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
            retry_policy: None,
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
            retry_policy: None,
            schedules: Vec::new(),
        }
    }

    /// Sets the timeout for this system node.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
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
///
/// # Examples
///
/// Decision nodes are created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// #[derive(PartialEq)]
/// enum Action { UseTool, Respond }
/// struct ReasoningResult { action: Action }
///
/// async fn use_tool() -> i32 { 1 }
/// async fn respond() -> i32 { 2 }
///
/// let mut graph = Graph::new();
/// graph.add_conditional_branch::<ReasoningResult, _, _, _>(
///     "needs_tool",
///     |result| result.action == Action::UseTool,
///     |g| { g.add_system(use_tool); },
///     |g| { g.add_system(respond); },
/// );
/// ```
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
///
/// # Examples
///
/// Switch nodes are created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// struct RouterOutput { action: &'static str }
///
/// async fn use_tool() -> i32 { 1 }
/// async fn respond() -> i32 { 2 }
/// async fn handle_unknown() -> i32 { 3 }
///
/// let mut graph = Graph::new();
/// graph.add_switch::<RouterOutput, _, _, _>(
///     "route_action",
///     |output| output.action,
///     vec![
///         ("tool", Box::new(|g: &mut Graph| { g.add_system(use_tool); })
///             as Box<dyn FnOnce(&mut Graph)>),
///         ("respond", Box::new(|g: &mut Graph| { g.add_system(respond); })
///             as Box<dyn FnOnce(&mut Graph)>),
///     ],
///     Some(Box::new(|g: &mut Graph| { g.add_system(handle_unknown); })),
/// );
/// ```
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
/// simultaneously. After all branches complete, outputs are merged
/// and execution continues from the parallel node's outgoing edge.
///
/// # Examples
///
/// Parallel nodes are created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// async fn fetch_user() -> String { String::new() }
/// async fn fetch_orders() -> Vec<i32> { vec![] }
/// async fn fetch_preferences() -> bool { true }
///
/// let mut graph = Graph::new();
/// graph.add_parallel("gather_data", [
///     |g: &mut Graph| { g.add_system(fetch_user); },
///     |g: &mut Graph| { g.add_system(fetch_orders); },
///     |g: &mut Graph| { g.add_system(fetch_preferences); },
/// ]);
/// ```
#[derive(Debug)]
pub struct ParallelNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// Node IDs for each parallel branch entry point.
    pub branches: Vec<NodeId>,
}

impl ParallelNode {
    /// Creates a new parallel node.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            id: NodeId::new(),
            name,
            branches: Vec::new(),
        }
    }
}

/// A node that repeats a subgraph until a termination condition.
///
/// Loop nodes implement iterative execution patterns, repeating the
/// loop body until a termination predicate returns true or max iterations
/// is reached.
///
/// # Examples
///
/// Loop nodes are created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::Graph;
///
/// struct LoopState { done: bool }
///
/// async fn iterate() -> LoopState { LoopState { done: false } }
///
/// // With a termination predicate
/// let mut graph = Graph::new();
/// graph.add_loop::<LoopState, _, _>(
///     "work_loop",
///     |state| state.done,
///     |g| { g.add_system(iterate); },
/// );
/// ```
///
/// With a fixed iteration count:
///
/// ```
/// use polaris_graph::Graph;
///
/// async fn attempt() -> i32 { 1 }
///
/// let mut graph = Graph::new();
/// graph.add_loop_n("retry", 5, |g| {
///     g.add_system(attempt);
/// });
/// ```
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

// ─────────────────────────────────────────────────────────────────────────────
// Scope Node
// ─────────────────────────────────────────────────────────────────────────────

/// Type-erased clone function used by the `forward` crossing.
///
/// Returns `None` on downcast failure (should never happen in practice —
/// `TypeId` is verified before invocation).
pub(crate) type CloneFn = fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>;

/// Per-resource crossing strategy used when entering a scope boundary.
///
/// Each variant corresponds to a positive builder verb on [`ContextPolicy`]
/// (`share`, `forward`, `fork`, `forward_fresh`). The negative `exclude` verb
/// is tracked separately on [`ContextPolicy::excludes`].
#[derive(Clone)]
pub(crate) enum CrossingAction {
    /// Reachable via the parent chain — no copy. Translates to an entry in
    /// the child's [`ParentFilter`](polaris_system::param::ParentFilter).
    Share,
    /// Cloned from the parent's local scope at scope entry.
    Forward(CloneFn),
    /// Forked from the parent's local scope via [`ForkStrategy::fork`].
    ///
    /// [`ForkStrategy::fork`]: polaris_system::resource::ForkStrategy::fork
    Fork(CloneFn),
    /// Re-instantiated at scope entry from the resource's registered factory.
    ForwardFresh,
}

impl PartialEq for CrossingAction {
    /// Variants compare by tag only — `Forward` and `Fork` deliberately
    /// ignore their inner [`CloneFn`] pointers because Rust does not
    /// guarantee fn-pointer addresses are unique across codegen units.
    /// Two crossings with the same verb on the same `T` are stored once
    /// per [`TypeId`], so equality only needs to distinguish verb shape.
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Share, Self::Share)
                | (Self::Forward(_), Self::Forward(_))
                | (Self::Fork(_), Self::Fork(_))
                | (Self::ForwardFresh, Self::ForwardFresh)
        )
    }
}

impl Eq for CrossingAction {}

impl fmt::Debug for CrossingAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Share => f.write_str("Share"),
            Self::Forward(_) => f.write_str("Forward"),
            Self::Fork(_) => f.write_str("Fork"),
            Self::ForwardFresh => f.write_str("ForwardFresh"),
        }
    }
}

/// Per-resource decision recorded on a [`ContextPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceCrossing {
    pub(crate) type_id: TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) action: CrossingAction,
}

/// High-level summary of a [`ContextPolicy`]'s scope-boundary mode.
///
/// `ContextMode` collapses the verb composition on a [`ContextPolicy`] into
/// a coarse classification suitable for hooks and middleware that don't need
/// the per-resource detail. Each policy maps to exactly one mode:
///
/// | Mode | Trigger |
/// |---|---|
/// | [`Shared`](Self::Shared) | [`ContextPolicy::shared`] |
/// | [`Inherit`](Self::Inherit) | [`share_rest`](ContextPolicy::share_rest) on a [`ContextPolicy::new`] policy |
/// | [`Isolated`](Self::Isolated) | a [`ContextPolicy::new`] policy without `share_rest` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContextMode {
    /// No boundary — the scope reuses the parent context
    /// ([`ContextPolicy::shared`]).
    Shared,
    /// Selective sharing — `share_rest()` lets the child see the parent
    /// chain by default, with optional per-type `exclude::<T>()` overrides.
    Inherit,
    /// Pure isolation or per-resource crossings only — the child sees
    /// globals plus explicitly forwarded/forked/fresh locals, and any
    /// types listed via `share::<T>()`.
    Isolated,
}

impl fmt::Display for ContextMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextMode::Shared => f.write_str("shared"),
            ContextMode::Inherit => f.write_str("inherit"),
            ContextMode::Isolated => f.write_str("isolated"),
        }
    }
}

/// Per-resource policy controlling which resources cross a scope boundary,
/// and how each one crosses.
///
/// Construct with [`ContextPolicy::new`] (empty: nothing crosses unless added)
/// or [`ContextPolicy::shared`] (no boundary at all, equivalent to running the
/// inner graph inline). Compose by chaining per-resource verbs:
///
/// | Verb | Mechanism | Requires of `T` | Use when |
/// |---|---|---|---|
/// | [`share`](Self::share) | Child reads via parent chain | nothing | Read-only access; large or expensive-to-clone |
/// | [`forward`](Self::forward) | `Clone::clone` into child's local | `T: Clone` | Small mutable resource; child needs its own copy |
/// | [`fork`](Self::fork) | [`ForkStrategy::fork`] into child's local | `T: ForkStrategy` | Stateful resource with non-`Clone` semantics |
/// | [`forward_fresh`](Self::forward_fresh) | Re-invoke `T`'s registered factory | factory registered via `Server::register_local` | Resource that should start clean (counters, scratchpads) |
/// | [`exclude`](Self::exclude) | Suppress any earlier verb / [`share_rest`](Self::share_rest) | nothing | Opt one resource out of the catch-all |
/// | [`share_rest`](Self::share_rest) | Apply `share` to every resource not otherwise mentioned | nothing | Common "mostly inherit, with a few overrides" case |
///
/// Verbs are applied in declaration order; later verbs override earlier ones
/// for the same `T`. [`share_rest`](Self::share_rest) only applies to types
/// not otherwise named.
///
/// # Panics
///
/// Calling any verb on a policy constructed via [`shared()`](Self::shared)
/// panics. `shared()` denotes "no boundary at all" — there is no child context
/// to apply per-resource decisions to, so attempting to compose verbs onto it
/// is a programmer error.
///
/// # Examples
///
/// ```
/// use polaris_graph::ContextPolicy;
/// use polaris_system::resource::LocalResource;
///
/// #[derive(Clone, Default)]
/// struct Config;
/// impl LocalResource for Config {}
///
/// // No boundary at all — same context.
/// let _ = ContextPolicy::shared();
///
/// // Strict isolation — only Config crosses, by clone.
/// let _ = ContextPolicy::new().forward::<Config>();
///
/// // Mostly inherit, override one resource.
/// let _ = ContextPolicy::new()
///     .forward::<Config>()
///     .share_rest();
/// ```
///
/// [`ForkStrategy::fork`]: polaris_system::resource::ForkStrategy::fork
///
/// Note: `ContextPolicy` derives `PartialEq`/`Eq` (sufficient for tests and
/// equality checks). It does **not** derive `Hash` because the underlying
/// `HashMap`/`HashSet` storage and the cached
/// [`ParentFilter`](polaris_system::param::ParentFilter) do not
/// implement `Hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPolicy {
    /// High-level scope-boundary mode (kept in sync with `share_rest` and the
    /// constructor used).
    pub(crate) mode: ContextMode,
    /// Positive per-resource crossings (`share`, `forward`, `fork`, `forward_fresh`).
    /// Disjoint from [`Self::excludes`].
    pub(crate) crossings: HashMap<TypeId, ResourceCrossing>,
    /// Resources opted out of any catch-all (e.g. `share_rest`).
    /// Disjoint from [`Self::crossings`].
    pub(crate) excludes: HashSet<TypeId>,
    /// When `true`, resources not in `crossings`/`excludes` are reachable via
    /// the parent chain.
    pub(crate) share_rest: bool,
    /// Cached [`ParentFilter`](polaris_system::param::ParentFilter) — rebuilt
    /// eagerly whenever a verb mutates the policy. Avoids per-call allocation
    /// in the executor and validation hot paths.
    pub(crate) cached_parent_filter: polaris_system::param::ParentFilter,
}

impl ContextPolicy {
    /// Empty per-resource policy: no resources cross unless explicitly added.
    ///
    /// The child context is created fresh and only sees globals plus whatever
    /// is added via [`share`](Self::share), [`forward`](Self::forward),
    /// [`fork`](Self::fork), [`forward_fresh`](Self::forward_fresh), or
    /// [`share_rest`](Self::share_rest). This is the right default for
    /// sandbox-style scopes.
    #[must_use]
    #[expect(
        clippy::new_without_default,
        reason = "ContextPolicy intentionally omits Default — `new()` and `shared()` make the boundary choice explicit"
    )]
    pub fn new() -> Self {
        Self {
            mode: ContextMode::Isolated,
            crossings: HashMap::new(),
            excludes: HashSet::new(),
            share_rest: false,
            // Pure-isolation default: parent chain hides every local; globals
            // still flow. `ParentFilter::default()` is `AllowAllExcept(empty)`
            // — the wrong starting point — so we set `AllowOnly(empty)`
            // explicitly here.
            cached_parent_filter: polaris_system::param::ParentFilter::allow_only([]),
        }
    }

    /// No-boundary policy — the scope reuses the parent context.
    ///
    /// Reads, writes, and outputs all flow through the parent. Per-resource
    /// verbs are rejected (see [`ContextPolicy`] panics).
    #[must_use]
    pub fn shared() -> Self {
        Self {
            mode: ContextMode::Shared,
            crossings: HashMap::new(),
            excludes: HashSet::new(),
            share_rest: false,
            // Unused for `shared` policies (no child context is built), but
            // keep a sensible value: an empty `AllowOnly` mirrors `new()`.
            cached_parent_filter: polaris_system::param::ParentFilter::allow_only([]),
        }
    }

    /// Make `T` reachable in the child via the parent chain — zero copy.
    ///
    /// The child does not own a copy; reads of `T` walk up to the parent.
    /// Use this for read-only access to large or expensive-to-clone resources
    /// (e.g. tool registries, system prompts).
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    #[must_use]
    pub fn share<T: LocalResource>(mut self) -> Self {
        self.assert_not_shared::<T>("share");
        self.set_crossing::<T>(CrossingAction::Share);
        self.refresh_parent_filter();
        self
    }

    /// Clone `T` from the parent's local scope into the child at scope entry.
    ///
    /// The clone is one-way — mutations in the child do not propagate back.
    /// If the scope sits inside a loop, each iteration clones the resource.
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    #[must_use]
    pub fn forward<T: LocalResource + Clone>(mut self) -> Self {
        self.assert_not_shared::<T>("forward");
        self.set_crossing::<T>(CrossingAction::Forward(|any| {
            Some(Box::new(any.downcast_ref::<T>()?.clone()))
        }));
        self.refresh_parent_filter();
        self
    }

    /// Fork `T` from the parent into the child via [`ForkStrategy::fork`].
    ///
    /// Use this when domain semantics differ from `Clone` — e.g. fresh-empty
    /// stores, `Arc`-shared atomics, or child trace spans.
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    ///
    /// [`ForkStrategy::fork`]: polaris_system::resource::ForkStrategy::fork
    #[must_use]
    pub fn fork<T>(mut self) -> Self
    where
        T: polaris_system::resource::ForkStrategy,
    {
        self.assert_not_shared::<T>("fork");
        self.set_crossing::<T>(CrossingAction::Fork(|any| {
            Some(Box::new(any.downcast_ref::<T>()?.fork()))
        }));
        self.refresh_parent_filter();
        self
    }

    /// Instantiate a fresh `T` in the child via the resource's registered factory.
    ///
    /// `T` must have been registered with the server via `register_local(...)`
    /// — the factory is captured on the resource entry and re-invoked at scope
    /// entry. If no factory is registered for `T` anywhere in the parent
    /// hierarchy, scope execution fails with a clear error.
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    #[must_use]
    pub fn forward_fresh<T: LocalResource>(mut self) -> Self {
        self.assert_not_shared::<T>("forward_fresh");
        self.set_crossing::<T>(CrossingAction::ForwardFresh);
        self.refresh_parent_filter();
        self
    }

    /// Suppress any earlier verb and any catch-all [`share_rest`](Self::share_rest)
    /// for this resource.
    ///
    /// Combine with [`share_rest`](Self::share_rest) to opt one resource out of
    /// the catch-all.
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    #[must_use]
    pub fn exclude<T: LocalResource>(mut self) -> Self {
        self.assert_not_shared::<T>("exclude");
        self.crossings.remove(&TypeId::of::<T>());
        self.excludes.insert(TypeId::of::<T>());
        self.refresh_parent_filter();
        self
    }

    /// Apply [`share`](Self::share) to every resource not otherwise named.
    ///
    /// Conventionally the last call in a chain. Combines naturally with
    /// [`exclude`](Self::exclude) for a "mostly inherit, with a few overrides"
    /// pattern.
    ///
    /// # Panics
    ///
    /// Panics if called on [`ContextPolicy::shared`] — see type-level docs.
    #[must_use]
    pub fn share_rest(mut self) -> Self {
        assert!(
            !matches!(self.mode, ContextMode::Shared),
            "share_rest() is not valid on ContextPolicy::shared() — \
             use ContextPolicy::new() to compose per-resource verbs",
        );
        self.share_rest = true;
        self.mode = ContextMode::Inherit;
        self.refresh_parent_filter();
        self
    }

    fn assert_not_shared<T: ?Sized>(&self, verb: &'static str) {
        assert!(
            !matches!(self.mode, ContextMode::Shared),
            "{verb}::<{}>() is not valid on ContextPolicy::shared() — \
             use ContextPolicy::new() to compose per-resource verbs",
            core::any::type_name::<T>(),
        );
    }

    /// Records a positive crossing for `T`, removing any prior `exclude`.
    fn set_crossing<T: 'static>(&mut self, action: CrossingAction) {
        let type_id = TypeId::of::<T>();
        self.excludes.remove(&type_id);
        self.crossings.insert(
            type_id,
            ResourceCrossing {
                type_id,
                type_name: core::any::type_name::<T>(),
                action,
            },
        );
    }

    /// Rebuilds the cached [`ParentFilter`] from the current verb composition.
    ///
    /// Called from the tail of every mutating verb method. Cheap (one
    /// allocation per mutation) and avoids repeated rebuilding at execution
    /// time.
    fn refresh_parent_filter(&mut self) {
        use polaris_system::param::ParentFilter;
        self.cached_parent_filter = if self.share_rest {
            ParentFilter::allow_all_except(self.excludes.iter().copied())
        } else {
            ParentFilter::allow_only(
                self.crossings
                    .values()
                    .filter(|c| matches!(c.action, CrossingAction::Share))
                    .map(|c| c.type_id),
            )
        };
    }

    /// Returns the high-level [`ContextMode`] for this policy.
    ///
    /// `Shared` for [`ContextPolicy::shared`], `Inherit` when
    /// [`share_rest`](Self::share_rest) is set, otherwise `Isolated`.
    #[must_use]
    pub fn mode(&self) -> ContextMode {
        self.mode
    }

    /// Returns whether this policy is the no-boundary form
    /// ([`ContextPolicy::shared`]).
    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(self.mode, ContextMode::Shared)
    }

    /// Returns the per-resource crossing decisions on this policy.
    pub(crate) fn crossings(&self) -> impl Iterator<Item = &ResourceCrossing> {
        self.crossings.values()
    }

    /// Looks up the crossing action for a specific type.
    #[cfg(test)]
    pub(crate) fn crossing_for(&self, type_id: TypeId) -> Option<&ResourceCrossing> {
        self.crossings.get(&type_id)
    }

    /// Returns the cached [`ParentFilter`](polaris_system::param::ParentFilter)
    /// for this policy.
    ///
    /// The filter governs which resource types are reachable through the
    /// parent chain in the child context. For pure-isolation policies (no
    /// `share` / `share_rest`) the filter allows only the explicitly shared
    /// types — globals still flow through.
    pub(crate) fn parent_filter(&self) -> &polaris_system::param::ParentFilter {
        &self.cached_parent_filter
    }
}

/// A node that executes an embedded graph with a configurable context boundary.
///
/// The embedded graph is a self-contained directed graph that is executed as a
/// single unit within the parent graph. The [`ContextPolicy`] controls how the
/// parent's [`SystemContext`](polaris_system::param::SystemContext) is shared
/// with the embedded graph.
///
/// From the parent graph's perspective, the scope node is a single opaque node —
/// execution enters the scope, runs the embedded graph to completion, and exits
/// from the scope's outgoing edge.
///
/// Unlike decision/loop/parallel nodes, the embedded graph's nodes are NOT merged
/// into the parent. The `ScopeNode` holds the [`Graph`] as a field.
///
/// # Examples
///
/// Scope nodes are created through the [`Graph`] builder API:
///
/// ```
/// use polaris_graph::{Graph, ContextPolicy};
///
/// async fn gather_info() -> String { String::new() }
/// async fn summarize() -> String { String::new() }
///
/// // Build an inner graph for the sub-agent
/// let mut research = Graph::new();
/// research.add_system(gather_info).add_system(summarize);
///
/// // Embed it as a scope that chain-reads parent resources.
/// let mut graph = Graph::new();
/// graph.add_scope("research", research, ContextPolicy::new().share_rest());
/// ```
#[derive(Debug)]
pub struct ScopeNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The embedded graph to execute.
    pub(crate) graph: Graph,
    /// Context sharing policy.
    pub(crate) context_policy: ContextPolicy,
}

impl ScopeNode {
    /// Creates a new scope node.
    #[must_use]
    pub fn new(name: &'static str, graph: Graph, context_policy: ContextPolicy) -> Self {
        Self {
            id: NodeId::new(),
            name,
            graph,
            context_policy,
        }
    }

    /// Returns a reference to the embedded graph.
    #[must_use]
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Returns a reference to the context policy.
    #[must_use]
    pub fn context_policy(&self) -> &ContextPolicy {
        &self.context_policy
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

    #[test]
    fn retry_policy_fixed_delay() {
        let policy = RetryPolicy::fixed(3, Duration::from_millis(100));
        assert_eq!(policy.max_retries(), 3);
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(100));
    }

    #[test]
    fn retry_policy_exponential_delay() {
        let policy = RetryPolicy::exponential(4, Duration::from_millis(100));
        assert_eq!(policy.max_retries(), 4);
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(400));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(800));
    }

    #[test]
    fn retry_policy_exponential_with_max_delay() {
        let policy = RetryPolicy::exponential(4, Duration::from_millis(100))
            .with_max_delay(Duration::from_millis(300));
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(200));
        // 400ms capped to 300ms
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(300));
        // 800ms capped to 300ms
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(300));
    }

    #[test]
    fn retry_policy_with_max_delay_no_effect_on_fixed() {
        let policy = RetryPolicy::fixed(2, Duration::from_millis(100))
            .with_max_delay(Duration::from_millis(50));
        // with_max_delay has no effect on Fixed
        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
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

    // ─────────────────────────────────────────────────────────────────────────
    // ContextPolicy tests
    // ─────────────────────────────────────────────────────────────────────────

    use polaris_system::param::ParentFilter;
    use polaris_system::resource::{ForkStrategy, LocalResource};

    #[derive(Clone, Default)]
    struct TestRes;
    impl LocalResource for TestRes {}

    #[derive(Default)]
    struct ForkRes;
    impl LocalResource for ForkRes {}
    impl ForkStrategy for ForkRes {
        fn fork(&self) -> Self {
            ForkRes
        }
    }

    #[test]
    fn context_policy_shared_no_boundary() {
        let policy = ContextPolicy::shared();
        assert!(policy.is_shared());
        assert_eq!(policy.mode(), ContextMode::Shared);
        assert!(policy.crossings().next().is_none());
    }

    #[test]
    fn context_policy_new_is_isolated_by_default() {
        let policy = ContextPolicy::new();
        assert!(!policy.is_shared());
        assert_eq!(policy.mode(), ContextMode::Isolated);
        assert_eq!(*policy.parent_filter(), ParentFilter::allow_only([]));
    }

    #[test]
    fn share_rest_yields_child_with_allow_all() {
        let policy = ContextPolicy::new().share_rest();
        assert_eq!(policy.mode(), ContextMode::Inherit);
        assert_eq!(*policy.parent_filter(), ParentFilter::allow_all_except([]));
    }

    #[test]
    fn share_specific_type_yields_child_with_allow_only() {
        let policy = ContextPolicy::new().share::<TestRes>();
        assert_eq!(
            *policy.parent_filter(),
            ParentFilter::allow_only([TypeId::of::<TestRes>()])
        );
    }

    #[test]
    fn share_then_share_rest_keeps_share_and_yields_allow_all() {
        // Under `share_rest`, an explicit `share::<T>()` is redundant — but
        // the policy must not lose the share or produce the wrong parent
        // filter. We assert both: the parent filter is `AllowAllExcept([])`
        // (i.e. unrestricted), and the share crossing is still recorded.
        let policy = ContextPolicy::new().share::<TestRes>().share_rest();
        assert_eq!(policy.mode(), ContextMode::Inherit);
        assert_eq!(*policy.parent_filter(), ParentFilter::allow_all_except([]));
        let crossing = policy
            .crossing_for(TypeId::of::<TestRes>())
            .expect("share crossing should be retained");
        assert!(matches!(crossing.action, CrossingAction::Share));
    }

    #[test]
    fn forward_records_clone_strategy() {
        let policy = ContextPolicy::new().forward::<TestRes>();
        let crossing = policy.crossing_for(TypeId::of::<TestRes>()).unwrap();
        assert!(matches!(crossing.action, CrossingAction::Forward(_)));
    }

    #[test]
    fn fork_records_fork_strategy() {
        let policy = ContextPolicy::new().fork::<ForkRes>();
        let crossing = policy.crossing_for(TypeId::of::<ForkRes>()).unwrap();
        assert!(matches!(crossing.action, CrossingAction::Fork(_)));
    }

    #[test]
    fn forward_fresh_records_factory_strategy() {
        let policy = ContextPolicy::new().forward_fresh::<TestRes>();
        let crossing = policy.crossing_for(TypeId::of::<TestRes>()).unwrap();
        assert!(matches!(crossing.action, CrossingAction::ForwardFresh));
    }

    #[test]
    fn exclude_in_share_rest_filters_parent() {
        let policy = ContextPolicy::new().share_rest().exclude::<TestRes>();
        let filter = policy.parent_filter();
        assert!(!filter.allows(TypeId::of::<TestRes>()));
        // share_rest still allows other types through.
        struct Other;
        assert!(filter.allows(TypeId::of::<Other>()));
    }

    #[test]
    fn exclude_clears_prior_positive_verb() {
        // .share().exclude() must remove the share — last verb wins, and
        // exclude must not leave a stale entry in `crossings`.
        let policy = ContextPolicy::new().share::<TestRes>().exclude::<TestRes>();
        assert!(policy.crossing_for(TypeId::of::<TestRes>()).is_none());
        // `exclude` alone (without share_rest) doesn't change parent_filter
        // — allow_only stays empty — but we do want excludes tracked for
        // share_rest interactions.
        let policy_with_rest = ContextPolicy::new()
            .share::<TestRes>()
            .exclude::<TestRes>()
            .share_rest();
        let filter = policy_with_rest.parent_filter();
        assert!(!filter.allows(TypeId::of::<TestRes>()));
    }

    #[test]
    fn positive_verb_clears_prior_exclude() {
        // .exclude().share() must clear the exclude — last verb wins.
        let policy = ContextPolicy::new().exclude::<TestRes>().share::<TestRes>();
        let crossing = policy.crossing_for(TypeId::of::<TestRes>()).unwrap();
        assert!(matches!(crossing.action, CrossingAction::Share));
        assert_eq!(
            *policy.parent_filter(),
            ParentFilter::allow_only([TypeId::of::<TestRes>()])
        );
    }

    #[test]
    fn later_verb_overrides_earlier_for_same_type() {
        let policy = ContextPolicy::new().forward::<TestRes>().share::<TestRes>();
        let crossing = policy.crossing_for(TypeId::of::<TestRes>()).unwrap();
        assert!(matches!(crossing.action, CrossingAction::Share));
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_share_panics() {
        let _ = ContextPolicy::shared().share::<TestRes>();
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_forward_panics() {
        let _ = ContextPolicy::shared().forward::<TestRes>();
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_forward_fresh_panics() {
        let _ = ContextPolicy::shared().forward_fresh::<TestRes>();
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_fork_panics() {
        let _ = ContextPolicy::shared().fork::<ForkRes>();
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_exclude_panics() {
        let _ = ContextPolicy::shared().exclude::<TestRes>();
    }

    #[test]
    #[should_panic(expected = "is not valid on ContextPolicy::shared()")]
    fn shared_plus_share_rest_panics() {
        let _ = ContextPolicy::shared().share_rest();
    }

    #[test]
    fn scope_node_accessors() {
        let inner = Graph::new();
        let scope = ScopeNode {
            id: NodeId::new(),
            name: "test_scope",
            graph: inner,
            context_policy: ContextPolicy::shared(),
        };
        let node = Node::Scope(scope);
        assert_eq!(node.name(), "test_scope");
        assert!(!node.id().as_str().is_empty());
    }
}
