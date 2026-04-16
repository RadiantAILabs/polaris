//! Per-invocation context for tool execution.
//!
//! [`ToolContext`] carries per-invocation state from the calling system into
//! tool functions — anything the tool needs that shouldn't be part of its
//! LLM-facing argument schema. It is a lightweight typed map — no locks,
//! no hierarchy, no guards — designed to bridge the gap between `Res<T>`
//! in systems and `#[context]` parameters in tools.
//!
//! Typical contents are whatever is scoped to the current invocation: a
//! session identifier, a working directory, a locale, a user-supplied
//! budget, a dry-run flag, an opaque backend handle. The type is domain-
//! neutral — it imposes no framing (HTTP, CLI, batch, or otherwise).
//!
//! # Usage
//!
//! Build a [`ToolContext`] in a system, then pass it to
//! [`ToolRegistry::execute_with`](crate::ToolRegistry::execute_with):
//!
//! ```
//! use polaris_tools::ToolContext;
//!
//! #[derive(Clone)]
//! struct SessionId(String);
//!
//! let ctx = ToolContext::new()
//!     .with(SessionId("s-42".into()));
//!
//! assert!(ctx.contains::<SessionId>());
//! assert_eq!(ctx.get::<SessionId>().unwrap().0, "s-42");
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// Per-invocation context for tool execution.
///
/// A lightweight typed map that carries per-invocation state from systems
/// into tool functions. Values are inserted by the system orchestrating
/// tool execution and extracted by tools via `#[context]` parameters or
/// manual [`get`](Self::get) calls.
///
/// Values are stored behind `Arc`, so [`ToolContext`] is cheaply [`Clone`]
/// regardless of whether individual value types implement `Clone` —
/// cloning bumps the refcount of each contained value.
///
/// Not intended as a credential carrier: the context provides no scrubbing,
/// no guard types, and no constant-time access patterns. Pass secret material
/// through a dedicated credential-handling mechanism instead.
///
/// # Examples
///
/// ```
/// use polaris_tools::ToolContext;
/// use std::path::PathBuf;
///
/// struct WorkingDir(PathBuf);
/// struct DryRun(bool);
///
/// let ctx = ToolContext::new()
///     .with(WorkingDir("/tmp/work".into()))
///     .with(DryRun(true));
///
/// assert!(ctx.contains::<WorkingDir>());
/// assert!(ctx.contains::<DryRun>());
/// assert!(ctx.get::<DryRun>().unwrap().0);
/// ```
#[derive(Default, Clone)]
pub struct ToolContext {
    // TODO(pre-1.0): consider a `#[cfg(feature = "serde")]` path for snapshotting
    // context into traces / distributed propagation. Adding it post-1.0 would be
    // a breaking change to any manual `Tool` impls that match on the field layout.
    resources: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("resource_count", &self.resources.len())
            .finish()
    }
}

impl ToolContext {
    /// Creates an empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a value and returns `self` for builder-style chaining.
    ///
    /// If a value of the same type already exists, it is replaced.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_tools::ToolContext;
    ///
    /// struct Locale(String);
    ///
    /// let ctx = ToolContext::new()
    ///     .with(Locale("en-US".into()));
    /// ```
    #[must_use]
    pub fn with<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.insert(value);
        self
    }

    /// Inserts a value into the context.
    ///
    /// If a value of the same type already exists, it is replaced.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        self.resources.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Returns a reference to a value by type, or `None` if not present.
    #[must_use]
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.resources
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }

    /// Returns `true` if the context contains a value of type `T`.
    #[must_use]
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.resources.contains_key(&TypeId::of::<T>())
    }

    /// Returns `true` if the context contains no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// Returns the number of values in the context.
    #[must_use]
    pub fn len(&self) -> usize {
        self.resources.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct SessionId(String);

    #[derive(Debug, Clone, PartialEq)]
    struct Locale(String);

    #[test]
    fn new_context_is_empty() {
        let ctx = ToolContext::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn insert_and_get() {
        let mut ctx = ToolContext::new();
        ctx.insert(SessionId("abc".into()));

        assert_eq!(ctx.get::<SessionId>().unwrap().0, "abc");
        assert!(ctx.contains::<SessionId>());
        assert!(!ctx.contains::<Locale>());
    }

    #[test]
    fn builder_style_with() {
        let ctx = ToolContext::new()
            .with(SessionId("s1".into()))
            .with(Locale("en-US".into()));

        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx.get::<SessionId>().unwrap().0, "s1");
        assert_eq!(ctx.get::<Locale>().unwrap().0, "en-US");
    }

    #[test]
    fn insert_replaces_existing() {
        let ctx = ToolContext::new()
            .with(SessionId("old".into()))
            .with(SessionId("new".into()));

        assert_eq!(ctx.get::<SessionId>().unwrap().0, "new");
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn get_missing_returns_none() {
        let ctx = ToolContext::new();
        assert!(ctx.get::<SessionId>().is_none());
    }

    #[test]
    fn debug_shows_count() {
        let ctx = ToolContext::new().with(SessionId("x".into()));
        let debug = format!("{ctx:?}");
        assert!(debug.contains("resource_count: 1"));
    }

    #[test]
    fn clone_shares_underlying_values() {
        let original = ToolContext::new().with(SessionId("shared".into()));
        let cloned = original.clone();

        assert_eq!(original.get::<SessionId>().unwrap().0, "shared");
        assert_eq!(cloned.get::<SessionId>().unwrap().0, "shared");

        let orig_ptr: *const SessionId = original.get::<SessionId>().unwrap();
        let clone_ptr: *const SessionId = cloned.get::<SessionId>().unwrap();
        assert_eq!(orig_ptr, clone_ptr);
    }

    #[test]
    fn clone_insert_is_independent() {
        let original = ToolContext::new().with(SessionId("a".into()));
        let mut cloned = original.clone();
        cloned.insert(Locale("en-US".into()));

        assert!(!original.contains::<Locale>());
        assert!(cloned.contains::<Locale>());
    }

    #[test]
    fn context_allows_non_clone_values() {
        struct NotClone(String);

        let ctx = ToolContext::new().with(NotClone("opaque".into()));
        assert!(ctx.contains::<NotClone>());

        let cloned = ctx.clone();
        assert_eq!(cloned.get::<NotClone>().unwrap().0, "opaque");
    }
}
