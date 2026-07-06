//! Selectors for [`Dynamic`](crate::node::Node::Dynamic) nodes.
//!
//! A selector reads the execution [`SystemContext`] at runtime and returns the
//! key of the candidate subgraph to run. It mirrors the
//! [`Discriminator`](crate::predicate::Discriminator) pattern used by switch
//! nodes, with two differences: it reads the *full* context (resources **and**
//! outputs, not just one output), and it returns an owned
//! [`Arc<str>`](std::sync::Arc) key so candidate sets can be extended at runtime.
//!
//! Most callers pass a closure to [`Graph::add_dynamic`](crate::Graph::add_dynamic),
//! which wraps it in a [`Selector`]. Implement [`ErasedSelector`] directly only
//! when a closure cannot express the choice (e.g. it needs to return an error),
//! and wire it with [`Graph::add_dynamic_boxed`](crate::Graph::add_dynamic_boxed) /
//! [`Graph::add_dynamic_registry_boxed`](crate::Graph::add_dynamic_registry_boxed),
//! which accept a pre-boxed [`BoxedSelector`].
//!
//! # The selector is a trust boundary
//!
//! A selector turns context state into a control-flow decision, and it reads the
//! *full* context — including outputs that may be derived from model or other
//! untrusted input. The candidate set is **open**, not closed: a selector must
//! not assume the key it returns exists. An unknown key is routed to the node's
//! configured `default`, or fails with
//! [`DynamicCandidateNotFound`](crate::executor::ExecutionError::DynamicCandidateNotFound)
//! — it never panics. Soundness does not rest on the selector: whatever candidate
//! it picks was already signature-checked against the slot contract, so a
//! mis-routing can only run a *shape-compatible* graph, never an unverified one.

use crate::predicate::PredicateError;
use polaris_system::param::SystemContext;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

/// Object-safe trait for type-erased selectors stored in dynamic nodes.
///
/// Returns the [`Arc<str>`] key identifying which candidate subgraph the
/// [`Dynamic`](crate::node::Node::Dynamic) node should run. If the key is not
/// present in the candidate set, the node falls back to its configured default
/// (or errors with
/// [`DynamicCandidateNotFound`](crate::executor::ExecutionError::DynamicCandidateNotFound)).
pub trait ErasedSelector: Send + Sync {
    /// Chooses a candidate key from the current context.
    ///
    /// # Errors
    ///
    /// Returns an error if the selector cannot read what it needs from the
    /// context (closure-based selectors never error; they return a key and
    /// rely on the node's default for unknown keys).
    fn select(&self, ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError>;

    /// Returns a human-readable name of the selector's input, for debugging.
    fn input_type_name(&self) -> &'static str;
}

impl fmt::Debug for dyn ErasedSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErasedSelector")
            .field("input_type", &self.input_type_name())
            .finish()
    }
}

/// Type alias for boxed selectors handed to the builder.
pub type BoxedSelector = Box<dyn ErasedSelector>;

/// A typed selector wrapping a closure over the full execution context.
///
/// The closure receives `&SystemContext` and returns the candidate key — any
/// `impl Into<Arc<str>>`, so `&'static str`, `String`, and `Arc<str>` all work
/// without ceremony at the call site. It is infallible: a closure that cannot
/// find what it needs should return a fallback key and let the node's `default`
/// handle unknown keys.
///
/// # Example
///
/// ```
/// use polaris_graph::selector::Selector;
///
/// struct Route { tool: bool }
///
/// // Route on a previous system's output.
/// let selector = Selector::new(|ctx| {
///     match ctx.get_output::<Route>() {
///         Ok(route) if route.tool => "tool",
///         _ => "respond",
///     }
/// });
/// ```
pub struct Selector<F, K = Arc<str>> {
    func: F,
    // `fn() -> K` rather than `K`: the selector produces keys, it never holds
    // one, so `K` must not affect `Send`/`Sync`.
    _key: PhantomData<fn() -> K>,
}

impl<F, K> Selector<F, K>
where
    F: Fn(&SystemContext<'_>) -> K + Send + Sync + 'static,
    K: Into<Arc<str>>,
{
    /// Creates a new selector from a closure over the context.
    #[must_use]
    pub fn new(func: F) -> Self {
        Self {
            func,
            _key: PhantomData,
        }
    }
}

impl<F, K> ErasedSelector for Selector<F, K>
where
    F: Fn(&SystemContext<'_>) -> K + Send + Sync + 'static,
    K: Into<Arc<str>>,
{
    fn select(&self, ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
        Ok((self.func)(ctx).into())
    }

    fn input_type_name(&self) -> &'static str {
        "SystemContext"
    }
}

impl<F, K> fmt::Debug for Selector<F, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selector")
            .field("input_type", &"SystemContext")
            .finish()
    }
}
