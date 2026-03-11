//! System output storage.
//!
//! This module provides the [`Outputs`] container for storing ephemeral
//! system return values that flow between systems during a single execution.
//!
//! Unlike [`Resources`](super::Resources) which store long-lived shared state,
//! outputs are cleared between agent runs and are immutable once stored.

use hashbrown::HashMap;
use parking_lot::{RwLock, RwLockReadGuard};
use std::any::{Any, TypeId, type_name};

/// Marker trait for types that can be stored as system outputs.
///
/// Any type that is `Send + Sync + 'static` automatically implements `Output`.
/// This is the same bound as [`Resource`](super::Resource), but semantically
/// outputs represent ephemeral data flowing between systems.
pub trait Output: Send + Sync + 'static {}

// Blanket implementation for all compatible types
impl<T: Send + Sync + 'static> Output for T {}

/// Unique identifier for an output type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(TypeId);

impl OutputId {
    /// Creates an `OutputId` for the given type.
    #[must_use]
    pub fn of<T: Output>() -> Self {
        Self(TypeId::of::<T>())
    }
}

/// Errors that can occur during output operations.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// The requested output type was not found.
    #[error("output not found: {0}")]
    NotFound(&'static str),

    /// The output is currently being written.
    #[error("output busy: {0}")]
    Busy(&'static str),
}

/// Internal storage for a single output with thread-safe read access.
struct OutputEntry {
    /// Type-erased output data protected by `RwLock`.
    data: RwLock<Box<dyn Any + Send + Sync>>,
}

impl OutputEntry {
    /// Creates a new output entry.
    fn new<T: Output>(value: T) -> Self {
        Self {
            data: RwLock::new(Box::new(value)),
        }
    }

    /// Attempts to acquire a read lock.
    fn try_read(&self) -> Option<RwLockReadGuard<Box<dyn Any + Send + Sync>>> {
        self.data.try_read()
    }
}

/// Container for storing system outputs during execution.
///
/// `Outputs` provides type-safe storage for system return values. Unlike
/// [`Resources`](super::Resources), outputs are:
///
/// - **Ephemeral**: Cleared between agent runs
/// - **Write-once**: Set by the executor when a system returns
/// - **Read-only**: Systems can only read outputs, not modify them
///
/// # Example
///
/// ```
/// use polaris_system::resource::Outputs;
///
/// struct ReasoningResult { action: String }
///
/// let mut outputs = Outputs::new();
///
/// // Executor stores system return value
/// outputs.insert(ReasoningResult { action: "search".into() });
///
/// // Next system reads the output
/// {
///     let result = outputs.get::<ReasoningResult>().unwrap();
///     assert_eq!(result.action, "search");
/// }
///
/// // Clear between runs
/// outputs.clear();
/// ```
#[derive(Default)]
pub struct Outputs {
    storage: HashMap<OutputId, OutputEntry>,
}

impl Outputs {
    /// Creates a new empty outputs container.
    #[must_use]
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
        }
    }

    /// Inserts a system output.
    ///
    /// Called by the executor after a system returns a value.
    /// If an output of this type already exists, it is replaced.
    pub fn insert<T: Output>(&mut self, value: T) -> Option<T> {
        let id = OutputId::of::<T>();
        let entry = OutputEntry::new(value);

        self.storage.insert(id, entry).and_then(|old| {
            old.data
                .into_inner()
                .downcast::<T>()
                .ok()
                .map(|boxed| *boxed)
        })
    }

    /// Inserts a type-erased system output.
    ///
    /// This is used by the executor to store outputs when the concrete type
    /// is not known at compile time. The `type_id` must match the correct type
    /// of the boxed value.
    ///
    /// If an output with this type ID already exists, it is replaced.
    pub fn insert_boxed(&mut self, type_id: TypeId, value: Box<dyn Any + Send + Sync>) {
        let id = OutputId(type_id);
        let entry = OutputEntry {
            data: RwLock::new(value),
        };
        self.storage.insert(id, entry);
    }

    /// Returns `true` if an output of type `T` exists.
    #[must_use]
    pub fn contains<T: Output>(&self) -> bool {
        self.storage.contains_key(&OutputId::of::<T>())
    }

    /// Returns `true` if an output with the given `TypeId` exists.
    ///
    /// This is useful for validation when the concrete type is not known
    /// at compile time (e.g., validating access declarations).
    #[must_use]
    pub fn contains_by_type_id(&self, type_id: TypeId) -> bool {
        self.storage.contains_key(&OutputId(type_id))
    }

    /// Gets an immutable reference to an output.
    ///
    /// # Errors
    ///
    /// - [`OutputError::NotFound`] if no system has produced this output type
    /// - [`OutputError::Busy`] if the output is currently being written
    pub fn get<T: Output>(&self) -> Result<OutputRef<T>, OutputError> {
        let id = OutputId::of::<T>();
        let type_name = type_name::<T>();

        let entry = self
            .storage
            .get(&id)
            .ok_or(OutputError::NotFound(type_name))?;

        let guard = entry.try_read().ok_or(OutputError::Busy(type_name))?;

        Ok(OutputRef {
            guard,
            _marker: std::marker::PhantomData,
        })
    }

    /// Clears all outputs.
    ///
    /// Called by the executor between agent runs to reset ephemeral state.
    pub fn clear(&mut self) {
        self.storage.clear();
    }

    /// Returns the number of outputs stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.len()
    }

    /// Returns `true` if no outputs are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    /// Merges all outputs from `other` into this container.
    ///
    /// Consumes `other`, moving all entries into `self`.
    /// If both containers have an output of the same type, the entry
    /// from `other` overwrites the one in `self`.
    ///
    /// This is used by the executor to propagate outputs from child
    /// contexts (parallel branches) back to the parent context.
    pub fn merge_from(&mut self, other: Outputs) {
        for (id, entry) in other.storage {
            self.storage.insert(id, entry);
        }
    }
}

/// RAII guard for immutable output access.
///
/// This guard is returned by [`Outputs::get`] and provides read-only
/// access to the underlying output. The lock is released when the
/// guard is dropped.
pub struct OutputRef<'a, T: Output> {
    guard: RwLockReadGuard<'a, Box<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<&'a T>,
}

impl<T: Output> std::ops::Deref for OutputRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .downcast_ref::<T>()
            .expect("output type mismatch (this is a bug)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct ReasoningResult {
        action: String,
    }

    #[derive(Debug, PartialEq)]
    struct ToolResult {
        value: i32,
    }

    #[test]
    fn insert_and_get() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "search".into(),
        });

        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "search");
    }

    #[test]
    fn insert_replaces_existing() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "first".into(),
        });

        let old = outputs.insert(ReasoningResult {
            action: "second".into(),
        });
        assert_eq!(
            old,
            Some(ReasoningResult {
                action: "first".into()
            })
        );

        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "second");
    }

    #[test]
    fn multiple_output_types() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "search".into(),
        });
        outputs.insert(ToolResult { value: 42 });

        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "search");
        assert_eq!(outputs.get::<ToolResult>().unwrap().value, 42);
    }

    #[test]
    fn not_found_error() {
        let outputs = Outputs::new();
        let result = outputs.get::<ReasoningResult>();
        assert!(matches!(result, Err(OutputError::NotFound(_))));
    }

    #[test]
    fn clear_removes_all() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });
        outputs.insert(ToolResult { value: 1 });

        assert_eq!(outputs.len(), 2);

        outputs.clear();

        assert!(outputs.is_empty());
        assert!(outputs.get::<ReasoningResult>().is_err());
        assert!(outputs.get::<ToolResult>().is_err());
    }

    #[test]
    fn contains_checks_presence() {
        let mut outputs = Outputs::new();

        assert!(!outputs.contains::<ReasoningResult>());
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });
        assert!(outputs.contains::<ReasoningResult>());
    }

    #[test]
    fn multiple_concurrent_reads() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        let read1 = outputs.get::<ReasoningResult>().unwrap();
        let read2 = outputs.get::<ReasoningResult>().unwrap();

        assert_eq!(read1.action, read2.action);
    }

    #[test]
    fn insert_boxed_type_erased() {
        let mut outputs = Outputs::new();

        // Insert via type-erased method
        let type_id = TypeId::of::<ReasoningResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ReasoningResult {
            action: "boxed".into(),
        });
        outputs.insert_boxed(type_id, boxed);

        // Should be retrievable via normal get
        assert!(outputs.contains::<ReasoningResult>());
        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "boxed");
    }

    #[test]
    fn contains_by_type_id() {
        let mut outputs = Outputs::new();

        let reasoning_id = TypeId::of::<ReasoningResult>();
        let tool_id = TypeId::of::<ToolResult>();

        assert!(!outputs.contains_by_type_id(reasoning_id));
        assert!(!outputs.contains_by_type_id(tool_id));

        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        assert!(outputs.contains_by_type_id(reasoning_id));
        assert!(!outputs.contains_by_type_id(tool_id));
    }

    #[test]
    fn output_ref_raii_releases_on_drop() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        // Take a read borrow
        {
            let _borrow = outputs.get::<ReasoningResult>().unwrap();
            // Multiple reads should still work (RwLock allows multiple readers)
            assert!(outputs.get::<ReasoningResult>().is_ok());
        }
        // After drop, reads should still succeed
        assert!(outputs.get::<ReasoningResult>().is_ok());
    }

    #[test]
    fn insert_boxed_replaces_existing() {
        let mut outputs = Outputs::new();

        // Insert first via normal method
        outputs.insert(ReasoningResult {
            action: "first".into(),
        });

        // Replace via type-erased method
        let type_id = TypeId::of::<ReasoningResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ReasoningResult {
            action: "second".into(),
        });
        outputs.insert_boxed(type_id, boxed);

        // Should have the new value
        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "second");

        // Should still be only one entry
        assert_eq!(outputs.len(), 1);
    }

    // ─────────────────────────────────────────────────────────────────────
    // merge_from tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn merge_from_overwrites_existing() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "original".into(),
        });

        let mut source = Outputs::new();
        source.insert(ReasoningResult {
            action: "overwritten".into(),
        });

        target.merge_from(source);

        assert_eq!(target.len(), 1);
        assert_eq!(
            target.get::<ReasoningResult>().unwrap().action,
            "overwritten"
        );
    }

    #[test]
    fn merge_from_combines_different_types() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "reasoning".into(),
        });

        let mut source = Outputs::new();
        source.insert(ToolResult { value: 99 });

        target.merge_from(source);

        assert_eq!(target.len(), 2);
        assert_eq!(target.get::<ReasoningResult>().unwrap().action, "reasoning");
        assert_eq!(target.get::<ToolResult>().unwrap().value, 99);
    }

    #[test]
    fn output_id_of_method() {
        let id = OutputId::of::<ReasoningResult>();
        let id2 = OutputId::of::<ReasoningResult>();
        let tool_id = OutputId::of::<ToolResult>();

        // Same type produces same id
        assert_eq!(id, id2);

        // Different types produce different ids
        assert_ne!(id, tool_id);
    }
}
