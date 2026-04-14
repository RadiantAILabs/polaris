//! Agent trait for defining reusable behavior patterns.
//!
//! The `Agent` trait provides a way to encapsulate agent behavior as a
//! reusable graph structure. Layer 3 implementations (`ReAct`, `ReWOO`, etc.)
//! implement this trait to define specific agent patterns.
//!
//! # Architecture
//!
//! This crate provides the pattern definition layer:
//!
//! - **`polaris_graph`**: Core graph primitives (Graph, Node, Edge, `GraphExecutor`)
//! - **`polaris_agent`**: Agent pattern definition (this crate)
//! - **Layer 3 plugins**: Concrete agent implementations (`ReAct`, `ReWOO`, etc.)
//!
//! # Example
//!
//! ```
//! use polaris_agent::Agent;
//! use polaris_graph::Graph;
//! use polaris_system::system;
//!
//! # async fn reason() {}
//! # async fn decide() {}
//! # async fn respond() {}
//!
//! struct SimpleAgent {
//!     max_iterations: usize,
//! }
//!
//! impl Agent for SimpleAgent {
//!     fn build(&self, graph: &mut Graph) {
//!         graph
//!             .add_system(reason)
//!             .add_system(decide)
//!             .add_system(respond);
//!     }
//!
//!     fn name(&self) -> &'static str {
//!         "SimpleAgent"
//!     }
//! }
//!
//! // Convert agent to graph
//! let agent = SimpleAgent { max_iterations: 10 };
//! let graph = agent.to_graph();
//! ```
//!
//! # See Also
//!
//! For the full framework guide, architecture overview, and integration patterns,
//! see the [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

use polaris_graph::graph::Graph;
use polaris_graph::node::ContextPolicy;
use polaris_system::param::{ParamError, SystemContext};

/// Error returned by [`Agent::setup`].
///
/// Wraps an arbitrary error source so agent implementations remain flexible
/// in what they report while the framework has a single, named error type
/// at the trait boundary.
///
/// When the inner error is a [`ParamError::ResourceNotFound`], the display
/// output includes a hint about how to provide the missing resource.
#[derive(Debug)]
pub struct SetupError(Box<dyn std::error::Error + Send + Sync>);

impl SetupError {
    /// Creates a new setup error from any error type.
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(source))
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = &*self.0;
        write!(f, "{inner}")?;
        if inner
            .downcast_ref::<ParamError>()
            .is_some_and(|pe| matches!(pe, ParamError::ResourceNotFound(_)))
        {
            write!(
                f,
                " — inject via ctx.insert() or register a plugin that provides it"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// Defines an agent's behavior as a graph of systems.
///
/// Implement this trait to create reusable agent patterns. Each agent
/// defines its behavior by building a graph of systems and control flow
/// constructs.
///
/// # Example
///
/// ```
/// use polaris_agent::Agent;
/// use polaris_graph::Graph;
/// use polaris_system::system;
///
/// # async fn reason() {}
/// # async fn decide() {}
/// # async fn respond() {}
///
/// struct SimpleAgent {
///     max_iterations: usize,
/// }
///
/// impl Agent for SimpleAgent {
///     fn build(&self, graph: &mut Graph) {
///         graph
///             .add_system(reason)
///             .add_system(decide)
///             .add_system(respond);
///     }
///
///     fn name(&self) -> &'static str {
///        "SimpleAgent"
///     }
/// }
/// ```
///
/// # Design Notes
///
/// - Agents are **builders**, not executors. They construct graphs that
///   will be executed by a separate executor component.
/// - Agents should be `Send + Sync` to allow concurrent graph building.
/// - The `build` method receives a mutable reference to allow agents to
///   conditionally construct different graph structures based on config.
pub trait Agent: Send + Sync + 'static {
    /// Builds the directed graph of systems that defines this agent's behavior.
    ///
    /// This method is called once when the agent is registered with the server.
    /// The graph structure becomes the source of truth for the agent's behavior.
    ///
    /// # Arguments
    ///
    /// * `graph` - The graph builder to construct the agent's behavior.
    fn build(&self, graph: &mut Graph);

    /// Returns a stable, user-defined name for this agent type.
    fn name(&self) -> &'static str;

    /// Initializes session resources before the first turn.
    ///
    /// Called automatically by the sessions layer during session creation and
    /// resume. Implementations can read configuration from `self` or the
    /// context and insert any resources the agent's systems need.
    ///
    /// The default implementation is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError`] if initialization fails.
    fn setup(&self, _ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        Ok(())
    }

    /// Builds and returns the agent's graph.
    ///
    /// Convenience method that creates a new [`Graph`] and calls [`build`](Self::build).
    /// Callable on trait objects (`dyn Agent`, `Arc<dyn Agent>`).
    fn to_graph(&self) -> Graph {
        let mut graph = Graph::new();
        self.build(&mut graph);
        graph
    }
}

/// Extension trait for adding agent sub-graphs as scope nodes.
///
/// # Example
///
/// ```
/// use polaris_agent::{Agent, AgentGraphExt};
/// use polaris_graph::Graph;
/// use polaris_graph::node::ContextPolicy;
///
/// # async fn step() {}
/// struct SubAgent;
/// impl Agent for SubAgent {
///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
///     fn name(&self) -> &'static str { "SubAgent" }
/// }
///
/// let mut graph = Graph::new();
/// graph.add_agent_scope(&SubAgent, ContextPolicy::inherit());
/// assert_eq!(graph.node_count(), 1);
/// ```
pub trait AgentGraphExt {
    /// Adds an agent's graph as a [`ScopeNode`](polaris_graph::node::ScopeNode).
    ///
    /// Builds the agent's graph via [`Agent::to_graph`] and wraps it in a
    /// scope node with the given [`ContextPolicy`]. The scope's name is
    /// taken from [`Agent::name`].
    fn add_agent_scope(&mut self, agent: &dyn Agent, policy: ContextPolicy) -> &mut Self;
}

impl AgentGraphExt for Graph {
    fn add_agent_scope(&mut self, agent: &dyn Agent, policy: ContextPolicy) -> &mut Self {
        let inner = agent.to_graph();
        self.add_scope(agent.name(), inner, policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_graph::node::ContextPolicy;

    // Test system functions
    async fn step_one() -> i32 {
        1
    }

    async fn step_two() -> i32 {
        2
    }

    async fn step_three() -> i32 {
        3
    }

    struct ThreeStepAgent;

    impl Agent for ThreeStepAgent {
        fn build(&self, graph: &mut Graph) {
            graph
                .add_system(step_one)
                .add_system(step_two)
                .add_system(step_three);
        }

        fn name(&self) -> &'static str {
            "ThreeStepAgent"
        }
    }

    #[test]
    fn agent_builds_graph() {
        let agent = ThreeStepAgent;
        let graph = agent.to_graph();

        assert_eq!(graph.node_count(), 3);
        assert!(graph.entry().is_some());
    }

    #[test]
    fn agent_name() {
        let agent = ThreeStepAgent;
        assert_eq!(agent.name(), "ThreeStepAgent");
    }

    // ─────────────────────────────────────────────────────────────────────
    // SetupError display tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn setup_error_resource_not_found_includes_hint() {
        let param_err = ParamError::ResourceNotFound("MyConfig");
        let setup_err = SetupError::new(param_err);
        let msg = setup_err.to_string();
        assert!(
            msg.contains("inject via ctx.insert()"),
            "expected hint in: {msg}"
        );
        assert!(msg.contains("MyConfig"));
    }

    #[test]
    fn setup_error_other_variant_no_hint() {
        let param_err = ParamError::BorrowConflict("Counter");
        let setup_err = SetupError::new(param_err);
        let msg = setup_err.to_string();
        assert!(
            !msg.contains("inject via ctx.insert()"),
            "unexpected hint in: {msg}"
        );
        assert!(msg.contains("borrow conflict"));
    }

    #[test]
    fn setup_error_non_param_error_no_hint() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let setup_err = SetupError::new(io_err);
        let msg = setup_err.to_string();
        assert!(
            !msg.contains("inject via ctx.insert()"),
            "unexpected hint in: {msg}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // AgentGraphExt tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn add_agent_scope_creates_scope_node() {
        let agent = ThreeStepAgent;
        let mut graph = Graph::new();
        graph.add_agent_scope(&agent, ContextPolicy::inherit());

        assert_eq!(graph.node_count(), 1);
        assert!(graph.entry().is_some());
        let node = &graph.nodes()[0];
        assert_eq!(node.name(), "ThreeStepAgent");
    }

    #[test]
    fn add_agent_scope_chains_after_system() {
        let agent = ThreeStepAgent;
        let mut graph = Graph::new();
        graph
            .add_system(step_one)
            .add_agent_scope(&agent, ContextPolicy::isolated());

        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
    }
}
