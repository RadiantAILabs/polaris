//! Graph-based execution primitives for Polaris (Layer 2).
//!
//! `polaris_graph` provides the core abstractions for defining behavior as
//! directed graphs of systems. This is the foundation for safe, composable,
//! inspectable agent behavior.
//!
//! # Core Concepts
//!
//! - [`Graph`] - Directed graph structure with builder API
//! - [`Node`](crate::node::Node) - Vertices representing computation or control flow
//! - [`Edge`](crate::edge::Edge) - Connections defining execution flow
//! - [`Predicate`](crate::predicate::Predicate) - Type-safe predicates for control flow decisions
//! - [`GraphExecutor`] - Runtime engine for graph traversal and execution
//!
//! # Example
//!
//! ```
//! # use polaris_graph::{Graph, GraphExecutor};
//! # use polaris_system::param::SystemContext;
//! # async fn example_fn() -> Result<(), Box<dyn std::error::Error>> {
//! # async fn reason() -> i32 { 1 }
//! # async fn decide() -> i32 { 2 }
//! # async fn respond() -> i32 { 3 }
//!
//! let mut graph = Graph::new();
//! graph
//!     .add_system(reason)
//!     .add_system(decide)
//!     .add_system(respond);
//!
//! let mut ctx = SystemContext::new();
//! let executor = GraphExecutor::new();
//! let result = executor.execute(&graph, &mut ctx, None, None).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Architecture
//!
//! This crate is Layer 2 of the Polaris architecture:
//!
//! - **Layer 1** (`polaris_system`): ECS-inspired primitives (System, Resource, Plugin)
//! - **Layer 2** (`polaris_graph`): Graph execution primitives (this crate)
//! - **Layer 2** (`polaris_agent`): Agent pattern definition (Agent trait)
//! - **Layer 3** (plugins): Concrete agent implementations
//!
//! For the full framework guide, architecture overview, and integration patterns,
//! see the [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

pub mod edge;

pub mod executor;

pub mod graph;

pub mod node;

pub mod predicate;

pub mod hooks;

pub mod dev;

pub mod middleware;

/// Re-export all common types for easy access.
pub mod prelude {
    pub use crate::edge::{
        ConditionalEdge, Edge, EdgeId, ErrorEdge, LoopBackEdge, ParallelEdge, SequentialEdge,
        TimeoutEdge,
    };
    pub use crate::executor::{
        CaughtError, ErrorKind, ExecutionError, ExecutionResult, GraphExecutor,
        ResourceValidationError,
    };
    pub use crate::graph::{
        Graph, MergeError, SystemNodeBuilder, ValidationError, ValidationResult, ValidationWarning,
    };
    pub use crate::middleware::{MiddlewareAPI, MiddlewareError};
    pub use crate::node::{
        ContextMode, ContextPolicy, DecisionNode, IntoSystemNode, LoopNode, Node, NodeId,
        NodeMarker, ParallelNode, ResourceForward, RetryPolicy, ScheduledNodeMarker, ScopeNode,
        SwitchNode, SystemNode,
    };
    pub use crate::predicate::{
        BoxedDiscriminator, BoxedPredicate, Discriminator, ErasedDiscriminator, ErasedPredicate,
        Predicate, PredicateError,
    };
}

// Re-export key types at crate root for convenience
pub use dev::{DevToolsPlugin, SystemInfo};
pub use executor::{
    CaughtError, ErrorKind, ExecutionError, ExecutionResult, GraphExecutor, ResourceValidationError,
};
pub use graph::{
    Graph, MergeError, SystemNodeBuilder, ValidationError, ValidationResult, ValidationWarning,
};
pub use hooks::{RunId, RunLabels};
pub use middleware::{MiddlewareAPI, MiddlewareError};
pub use node::{ContextMode, ContextPolicy, NodeId, ResourceForward, RetryPolicy, ScopeNode};
