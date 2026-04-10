//! Resource storage and management.
//!
//! This module provides the [`Resource`] trait, marker traits for scoping,
//! and [`Resources`] container for type-safe storage of shared state.
//!
//! # Resource Scoping
//!
//! Resources can be either global (shared across all execution contexts) or
//! local (per-context, mutable). This is controlled by marker traits:
//!
//! - [`GlobalResource`] - Read-only, server lifetime, shared across all agents
//! - [`LocalResource`] - Mutable, per-context, isolated per agent
//!
//! The distinction enables compile-time safety: `ResMut<T>` only works with
//! `LocalResource`, preventing accidental mutation of global state.

use hashbrown::HashMap;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::any::{Any, TypeId, type_name};

/// A resource that can be stored in the registry and injected into systems.
///
/// Resources are shared state that systems can access. Any type that is
/// `Send + Sync + 'static` automatically implements `Resource`.
///
/// # Scoping
///
/// To use a type with `ResMut<T>`, it must also implement [`LocalResource`].
/// Types marked with [`GlobalResource`] are read-only and can only be accessed
/// via `Res<T>`.
///
/// # Example
///
/// ```
/// use polaris_system::resource::{GlobalResource, LocalResource};
///
/// // LocalResource - can be mutated via ResMut<Counter>
/// struct Counter { value: i32 }
/// impl LocalResource for Counter {}
///
/// // GlobalResource - read-only via Res<Config>
/// struct Config { name: String }
/// impl GlobalResource for Config {}
/// ```
pub trait Resource: Send + Sync + 'static {
    /// Returns the type name for debugging purposes.
    fn type_name(&self) -> &'static str {
        type_name::<Self>()
    }
}

// Blanket implementation for all compatible types
impl<T: Send + Sync + 'static> Resource for T {}

/// Marker trait for global, read-only resources.
///
/// Global resources are:
/// - Stored at the server level (server lifetime)
/// - Shared across all execution contexts (agents, sessions, turns)
/// - Read-only (accessible via `Res<T>`, not `ResMut<T>`)
///
/// Use this for configuration, tool registries, and other shared state
/// that should not be modified during agent execution.
///
/// # Example
///
/// ```
/// use polaris_system::resource::GlobalResource;
/// use polaris_system::param::Res;
/// use polaris_system::server::Server;
/// use polaris_system::system;
///
/// struct Config {
///     system_prompt: String,
///     max_tokens: usize,
/// }
///
/// impl GlobalResource for Config {}
///
/// // Register via Server::insert_global()
/// let mut server = Server::new();
/// server.insert_global(Config {
///     system_prompt: "You are an AI.".into(),
///     max_tokens: 2048,
/// });
///
/// // Access as read-only in systems via Res<T>
/// #[system]
/// async fn my_system(config: Res<Config>) {
///     // config.system_prompt, config.max_tokens available
/// }
/// ```
///
/// Attempting to mutate a `GlobalResource` will fail to compile:
///
/// ```compile_fail
/// # use polaris_system::resource::GlobalResource;
/// # use polaris_system::param::ResMut;
/// # struct Config { system_prompt: String, max_tokens: usize }
/// # impl GlobalResource for Config {}
/// fn bad_system(mut config: ResMut<Config>) {  // Compile error!
///     // GlobalResource cannot be mutated
/// }
/// ```
pub trait GlobalResource: Resource {}

/// Marker trait for local, per-context resources.
///
/// Local resources are:
/// - Created fresh for each execution context
/// - Isolated between agents (Agent A's memory ≠ Agent B's memory)
/// - Mutable (can be used with `ResMut<T>`)
///
/// Use this for agent state, memory, scratchpads, and other state
/// that should be isolated per agent execution.
///
/// # Example
///
/// ```
/// use polaris_system::resource::LocalResource;
/// use polaris_system::param::ResMut;
/// use polaris_system::server::Server;
/// use polaris_system::system;
///
/// struct Message { content: String }
///
/// #[derive(Default)]
/// struct Memory { messages: Vec<Message> }
///
/// impl LocalResource for Memory {}
///
/// // Register a factory via Server::register_local()
/// let mut server = Server::new();
/// server.register_local(Memory::default);
///
/// // Access as mutable in systems via ResMut<T>
/// #[system]
/// async fn my_system(mut memory: ResMut<Memory>) {
///     memory.messages.push(Message { content: "Hello".into() });
/// }
/// ```
pub trait LocalResource: Resource {}

/// Unique identifier for a resource type.
///
/// Used internally to key resources in the storage map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(TypeId);

impl ResourceId {
    /// Creates a `ResourceId` for the given type.
    #[must_use]
    pub fn of<T: Resource>() -> Self {
        Self(TypeId::of::<T>())
    }

    /// Returns the underlying `TypeId`.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        self.0
    }
}

/// Errors that can occur during resource operations.
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    /// The requested resource type was not found in the container.
    #[error("resource not found: {0}")]
    NotFound(&'static str),

    /// The resource is currently borrowed mutably and cannot be accessed.
    #[error("resource already borrowed mutably: {0}")]
    BorrowConflict(&'static str),
}

/// Internal storage for a single resource with thread-safe access.
struct ResourceEntry {
    /// Type-erased resource data protected by `RwLock`.
    data: RwLock<Box<dyn Any + Send + Sync>>,
    /// Optional clone function for type-erased cloning.
    /// Registered via [`Resources::register_clone_fn`] for types that implement `Clone`.
    clone_fn: Option<fn(&dyn Any) -> Box<dyn Any + Send + Sync>>,
}

impl ResourceEntry {
    /// Creates a new resource entry.
    fn new<T: Resource>(resource: T) -> Self {
        Self {
            data: RwLock::new(Box::new(resource)),
            clone_fn: None,
        }
    }

    /// Creates a new resource entry from a boxed type-erased resource.
    fn new_boxed(data: Box<dyn Any + Send + Sync>) -> Self {
        Self {
            data: RwLock::new(data),
            clone_fn: None,
        }
    }

    /// Attempts to acquire a read lock.
    fn try_read(&self) -> Option<RwLockReadGuard<Box<dyn Any + Send + Sync>>> {
        self.data.try_read()
    }

    /// Attempts to acquire a write lock.
    fn try_write(&self) -> Option<RwLockWriteGuard<Box<dyn Any + Send + Sync>>> {
        self.data.try_write()
    }

    /// Consumes the entry and returns the inner data.
    fn into_inner(self) -> Box<dyn Any + Send + Sync> {
        self.data.into_inner()
    }
}

/// Container for storing and managing resources.
///
/// `Resources` provides type-safe storage for arbitrary types that implement
/// [`Resource`]. Resources are accessed through RAII guards that manage
/// borrowing automatically.
///
/// # Thread Safety
///
/// `Resources` uses `RwLock` internally, allowing multiple concurrent readers
/// or a single writer for each resource type.
///
/// # Example
///
/// ```
/// use polaris_system::resource::Resources;
///
/// struct Counter { value: i32 }
///
/// let mut resources = Resources::new();
/// resources.insert(Counter { value: 0 });
///
/// // Read access
/// {
///     let counter = resources.get::<Counter>().unwrap();
///     println!("Count: {}", counter.value);
/// }
///
/// // Write access
/// {
///     let mut counter = resources.get_mut::<Counter>().unwrap();
///     counter.value += 1;
/// }
/// ```
#[derive(Default)]
pub struct Resources {
    storage: HashMap<ResourceId, ResourceEntry>,
}

impl Resources {
    /// Creates a new empty resource container.
    #[must_use]
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
        }
    }

    /// Inserts a resource into the container.
    ///
    /// If a resource of this type already exists, it is replaced and the
    /// old value is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::resource::Resources;
    ///
    /// struct Counter { value: i32 }
    ///
    /// let mut resources = Resources::new();
    ///
    /// let old = resources.insert(Counter { value: 1 });
    /// assert!(old.is_none()); // First insertion
    ///
    /// let old = resources.insert(Counter { value: 2 });
    /// assert_eq!(old.unwrap().value, 1); // Replaced
    /// ```
    pub fn insert<T: Resource>(&mut self, resource: T) -> Option<T> {
        let id = ResourceId::of::<T>();
        let entry = ResourceEntry::new(resource);

        self.storage
            .insert(id, entry)
            .and_then(|old| old.into_inner().downcast::<T>().ok().map(|boxed| *boxed))
    }

    /// Inserts a type-erased resource into the container.
    ///
    /// This is used internally by factories that create resources dynamically.
    /// The `type_id` must match the type of the boxed resource.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `type_id` corresponds to the type stored
    /// in `resource`. Mismatches will cause panics when the resource is
    /// accessed via `get::<T>()`.
    pub fn insert_boxed(&mut self, type_id: TypeId, resource: Box<dyn Any + Send + Sync>) {
        let id = ResourceId(type_id);
        let entry = ResourceEntry::new_boxed(resource);
        self.storage.insert(id, entry);
    }

    /// Returns `true` if a resource of type `T` exists.
    #[must_use]
    pub fn contains<T: Resource>(&self) -> bool {
        self.storage.contains_key(&ResourceId::of::<T>())
    }

    /// Returns `true` if a resource with the given `TypeId` exists.
    ///
    /// This is useful for validation when the concrete type is not known
    /// at compile time (e.g., validating access declarations).
    #[must_use]
    pub fn contains_by_type_id(&self, type_id: TypeId) -> bool {
        self.storage.contains_key(&ResourceId(type_id))
    }

    /// Gets an immutable reference to a resource.
    ///
    /// Returns an error if the resource doesn't exist or is currently
    /// borrowed mutably.
    ///
    /// # Errors
    ///
    /// - [`ResourceError::NotFound`] if the resource type is not registered
    /// - [`ResourceError::BorrowConflict`] if the resource is mutably borrowed
    pub fn get<T: Resource>(&self) -> Result<ResourceRef<T>, ResourceError> {
        let id = ResourceId::of::<T>();
        let type_name = type_name::<T>();

        let entry = self
            .storage
            .get(&id)
            .ok_or(ResourceError::NotFound(type_name))?;

        let guard = entry
            .try_read()
            .ok_or(ResourceError::BorrowConflict(type_name))?;

        Ok(ResourceRef {
            guard,
            _marker: std::marker::PhantomData,
        })
    }

    /// Gets a mutable reference to a resource.
    ///
    /// Returns an error if the resource doesn't exist or is currently
    /// borrowed (either mutably or immutably).
    ///
    /// # Errors
    ///
    /// - [`ResourceError::NotFound`] if the resource type is not registered
    /// - [`ResourceError::BorrowConflict`] if the resource is already borrowed
    pub fn get_mut<T: Resource>(&self) -> Result<ResourceRefMut<T>, ResourceError> {
        let id = ResourceId::of::<T>();
        let type_name = type_name::<T>();

        let entry = self
            .storage
            .get(&id)
            .ok_or(ResourceError::NotFound(type_name))?;

        let guard = entry
            .try_write()
            .ok_or(ResourceError::BorrowConflict(type_name))?;

        Ok(ResourceRefMut {
            guard,
            _marker: std::marker::PhantomData,
        })
    }

    /// Removes a resource from the container and returns it.
    ///
    /// Returns `None` if the resource doesn't exist.
    pub fn remove<T: Resource>(&mut self) -> Option<T> {
        let id = ResourceId::of::<T>();

        self.storage
            .remove(&id)
            .and_then(|entry| entry.into_inner().downcast::<T>().ok().map(|boxed| *boxed))
    }

    /// Removes all resources from the container.
    pub fn clear(&mut self) {
        self.storage.clear();
    }

    /// Returns the number of resources stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.len()
    }

    /// Returns `true` if no resources are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    /// Registers a clone function for a resource type that implements `Clone`.
    ///
    /// This enables [`clone_by_type_id`](Self::clone_by_type_id) for the given
    /// type. The resource must already exist in the container; if it does not,
    /// this method is a no-op.
    ///
    /// The bound here is `Resource + Clone` (any resource). [`SystemContext::register_clone_fn`]
    /// narrows this to `LocalResource + Clone` to match its local-only semantics.
    ///
    /// [`SystemContext::register_clone_fn`]: crate::param::SystemContext::register_clone_fn
    ///
    /// # Ordering constraint
    ///
    /// [`insert`](Self::insert) and [`insert_boxed`](Self::insert_boxed) replace
    /// the entire resource entry, discarding any previously registered clone
    /// function. Always call `register_clone_fn` **after** the final insertion
    /// of the resource.
    ///
    /// Returns `true` if the clone function was registered, or `false` if the
    /// resource does not exist in the container.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::resource::Resources;
    ///
    /// #[derive(Clone, Debug, PartialEq)]
    /// struct Counter { value: i32 }
    ///
    /// let mut resources = Resources::new();
    /// resources.insert(Counter { value: 42 });
    /// assert!(resources.register_clone_fn::<Counter>());
    ///
    /// let cloned = resources.clone_by_type_id(std::any::TypeId::of::<Counter>());
    /// assert!(cloned.is_some());
    /// ```
    pub fn register_clone_fn<T: Resource + Clone>(&mut self) -> bool {
        let id = ResourceId::of::<T>();
        if let Some(entry) = self.storage.get_mut(&id) {
            entry.clone_fn = Some(|any: &dyn Any| -> Box<dyn Any + Send + Sync> {
                let val = any
                    .downcast_ref::<T>()
                    .expect("type mismatch in clone_fn (this is a bug)");
                Box::new(val.clone())
            });
            true
        } else {
            false
        }
    }

    /// Clones a resource by its [`TypeId`], returning a new boxed value.
    ///
    /// Returns `None` in three cases:
    /// - The resource does not exist in the container.
    /// - The resource exists but no clone function was registered via
    ///   [`register_clone_fn`](Self::register_clone_fn).
    /// - The resource is currently borrowed mutably.
    ///
    /// # Panics
    ///
    /// Panics if an internal type mismatch is detected in the registered clone
    /// function. This indicates a bug — the clone function was registered for a
    /// different type than what is stored under the same [`TypeId`].
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::resource::Resources;
    /// use std::any::TypeId;
    ///
    /// #[derive(Clone, Debug, PartialEq)]
    /// struct Counter { value: i32 }
    ///
    /// let mut resources = Resources::new();
    /// resources.insert(Counter { value: 10 });
    /// resources.register_clone_fn::<Counter>();
    ///
    /// let cloned = resources.clone_by_type_id(TypeId::of::<Counter>()).unwrap();
    /// let counter = cloned.downcast_ref::<Counter>().unwrap();
    /// assert_eq!(counter.value, 10);
    /// ```
    #[must_use]
    pub fn clone_by_type_id(&self, type_id: TypeId) -> Option<Box<dyn Any + Send + Sync>> {
        let id = ResourceId(type_id);
        let entry = self.storage.get(&id)?;
        let clone_fn = entry.clone_fn?;
        let guard = entry.try_read()?;
        Some(clone_fn(&**guard))
    }

    /// Clones a resource by its [`TypeId`] using an externally-provided clone function.
    ///
    /// Unlike [`clone_by_type_id`](Self::clone_by_type_id), this does not require
    /// a clone function to be registered via [`register_clone_fn`](Self::register_clone_fn).
    /// The caller supplies the clone function directly, which is useful when the
    /// concrete type (and its `Clone` impl) was captured at an earlier compile-time
    /// boundary — e.g., in [`ContextPolicy::forward`](crate::ContextPolicy::forward).
    ///
    /// Returns `None` if the resource does not exist or is currently write-locked.
    #[must_use]
    pub fn clone_by_type_id_with(
        &self,
        type_id: TypeId,
        clone_fn: fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        let id = ResourceId(type_id);
        let entry = self.storage.get(&id)?;
        let guard = entry.try_read()?;
        clone_fn(&**guard)
    }
}

/// RAII guard for immutable resource access.
///
/// This guard is returned by [`Resources::get`] and provides read-only
/// access to the underlying resource. The lock is released when the
/// guard is dropped.
pub struct ResourceRef<'a, T: Resource> {
    guard: RwLockReadGuard<'a, Box<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<&'a T>,
}

impl<T: Resource> std::ops::Deref for ResourceRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The type is guaranteed to be T because we created
        // the guard with ResourceId::of::<T>()
        self.guard
            .downcast_ref::<T>()
            .expect("resource type mismatch (this is a bug)")
    }
}

/// RAII guard for mutable resource access.
///
/// This guard is returned by [`Resources::get_mut`] and provides read-write
/// access to the underlying resource. The lock is released when the
/// guard is dropped.
pub struct ResourceRefMut<'a, T: Resource> {
    guard: RwLockWriteGuard<'a, Box<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<&'a mut T>,
}

impl<T: Resource> std::ops::Deref for ResourceRefMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Same as ResourceRef
        self.guard
            .downcast_ref::<T>()
            .expect("resource type mismatch (this is a bug)")
    }
}

impl<T: Resource> std::ops::DerefMut for ResourceRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Same as ResourceRef
        self.guard
            .downcast_mut::<T>()
            .expect("resource type mismatch (this is a bug)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Counter {
        value: i32,
    }

    #[derive(Debug, PartialEq)]
    struct Name(String);

    #[test]
    fn insert_and_get() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        let counter = resources.get::<Counter>().unwrap();
        assert_eq!(counter.value, 42);
    }

    #[test]
    fn insert_replaces_existing() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 1 });

        let old = resources.insert(Counter { value: 2 });
        assert_eq!(old, Some(Counter { value: 1 }));

        let counter = resources.get::<Counter>().unwrap();
        assert_eq!(counter.value, 2);
    }

    #[test]
    fn get_mut_modifies() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 0 });

        {
            let mut counter = resources.get_mut::<Counter>().unwrap();
            counter.value += 10;
        }

        let counter = resources.get::<Counter>().unwrap();
        assert_eq!(counter.value, 10);
    }

    #[test]
    fn multiple_immutable_borrows() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        let borrow1 = resources.get::<Counter>().unwrap();
        let borrow2 = resources.get::<Counter>().unwrap();

        assert_eq!(borrow1.value, borrow2.value);
    }

    #[test]
    fn mutable_borrow_blocks_immutable() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        let _borrow_mut = resources.get_mut::<Counter>().unwrap();
        let result = resources.get::<Counter>();

        assert!(matches!(result, Err(ResourceError::BorrowConflict(_))));
    }

    #[test]
    fn immutable_borrow_blocks_mutable() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        let _borrow = resources.get::<Counter>().unwrap();
        let result = resources.get_mut::<Counter>();

        assert!(matches!(result, Err(ResourceError::BorrowConflict(_))));
    }

    #[test]
    fn remove_returns_resource() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        let removed = resources.remove::<Counter>();
        assert_eq!(removed, Some(Counter { value: 42 }));

        let result = resources.get::<Counter>();
        assert!(matches!(result, Err(ResourceError::NotFound(_))));
    }

    #[test]
    fn multiple_resource_types() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });
        resources.insert(Name("Alice".to_string()));

        assert_eq!(resources.get::<Counter>().unwrap().value, 42);
        assert_eq!(resources.get::<Name>().unwrap().0, "Alice");
    }

    #[test]
    fn contains_checks_presence() {
        let mut resources = Resources::new();

        assert!(!resources.contains::<Counter>());
        resources.insert(Counter { value: 1 });
        assert!(resources.contains::<Counter>());
    }

    #[test]
    fn len_and_is_empty() {
        let mut resources = Resources::new();

        assert!(resources.is_empty());
        assert_eq!(resources.len(), 0);

        resources.insert(Counter { value: 1 });
        assert!(!resources.is_empty());
        assert_eq!(resources.len(), 1);

        resources.insert(Name("Test".to_string()));
        assert_eq!(resources.len(), 2);

        resources.clear();
        assert!(resources.is_empty());
    }

    #[test]
    fn insert_boxed_type_erased() {
        let mut resources = Resources::new();

        // Insert via type-erased method
        let type_id = TypeId::of::<Counter>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(Counter { value: 99 });
        resources.insert_boxed(type_id, boxed);

        // Should be retrievable via normal get
        assert!(resources.contains::<Counter>());
        let counter = resources.get::<Counter>().unwrap();
        assert_eq!(counter.value, 99);
    }

    #[test]
    fn contains_by_type_id() {
        let mut resources = Resources::new();

        let counter_id = TypeId::of::<Counter>();
        let name_id = TypeId::of::<Name>();

        assert!(!resources.contains_by_type_id(counter_id));
        assert!(!resources.contains_by_type_id(name_id));

        resources.insert(Counter { value: 1 });

        assert!(resources.contains_by_type_id(counter_id));
        assert!(!resources.contains_by_type_id(name_id));
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut resources = Resources::new();

        // Removing non-existent resource returns None
        let result = resources.remove::<Counter>();
        assert!(result.is_none());

        // Insert and remove
        resources.insert(Counter { value: 42 });
        let result = resources.remove::<Counter>();
        assert_eq!(result, Some(Counter { value: 42 }));

        // Second remove returns None
        let result = resources.remove::<Counter>();
        assert!(result.is_none());
    }

    #[test]
    fn resource_ref_raii_guard_releases_on_drop() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        // Take an immutable borrow
        {
            let _borrow = resources.get::<Counter>().unwrap();
            // While borrowed immutably, mutable borrow should fail
            assert!(resources.get_mut::<Counter>().is_err());
        }
        // After drop, mutable borrow should succeed
        assert!(resources.get_mut::<Counter>().is_ok());
    }

    #[test]
    fn resource_ref_mut_raii_guard_releases_on_drop() {
        let mut resources = Resources::new();
        resources.insert(Counter { value: 42 });

        // Take a mutable borrow
        {
            let _borrow_mut = resources.get_mut::<Counter>().unwrap();
            // While borrowed mutably, any borrow should fail
            assert!(resources.get::<Counter>().is_err());
            assert!(resources.get_mut::<Counter>().is_err());
        }
        // After drop, borrows should succeed
        assert!(resources.get::<Counter>().is_ok());
        assert!(resources.get_mut::<Counter>().is_ok());
    }

    #[test]
    fn resource_id_type_id_method() {
        let id = ResourceId::of::<Counter>();
        assert_eq!(id.type_id(), TypeId::of::<Counter>());

        let name_id = ResourceId::of::<Name>();
        assert_eq!(name_id.type_id(), TypeId::of::<Name>());

        // Different types have different ids
        assert_ne!(id.type_id(), name_id.type_id());
    }

    // ─────────────────────────────────────────────────────────────────────
    // clone_by_type_id tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq)]
    struct Cloneable {
        value: i32,
    }

    #[test]
    fn clone_registered_resource() {
        let mut resources = Resources::new();
        resources.insert(Cloneable { value: 42 });
        resources.register_clone_fn::<Cloneable>();

        let cloned = resources
            .clone_by_type_id(TypeId::of::<Cloneable>())
            .expect("clone should succeed");

        let cloned_val = cloned
            .downcast_ref::<Cloneable>()
            .expect("should downcast to Cloneable");
        assert_eq!(cloned_val.value, 42);
    }

    #[test]
    fn clone_unregistered_resource() {
        let mut resources = Resources::new();
        resources.insert(Cloneable { value: 42 });
        // Deliberately NOT registering clone fn

        let result = resources.clone_by_type_id(TypeId::of::<Cloneable>());
        assert!(result.is_none());
    }

    #[test]
    fn clone_nonexistent_resource() {
        let resources = Resources::new();

        let result = resources.clone_by_type_id(TypeId::of::<Cloneable>());
        assert!(result.is_none());
    }

    #[test]
    fn clone_independence() {
        let mut resources = Resources::new();
        resources.insert(Cloneable { value: 10 });
        resources.register_clone_fn::<Cloneable>();

        // Clone while original is 10
        let cloned = resources
            .clone_by_type_id(TypeId::of::<Cloneable>())
            .expect("clone should succeed");

        // Mutate the original
        {
            let mut guard = resources.get_mut::<Cloneable>().unwrap();
            guard.value = 999;
        }

        // Clone should retain the original value
        let cloned_val = cloned
            .downcast_ref::<Cloneable>()
            .expect("should downcast to Cloneable");
        assert_eq!(cloned_val.value, 10);

        // Original should be mutated
        let original = resources.get::<Cloneable>().unwrap();
        assert_eq!(original.value, 999);
    }

    // ─────────────────────────────────────────────────────────────────────
    // clone_by_type_id_with tests
    // ─────────────────────────────────────────────────────────────────────

    fn cloneable_clone_fn(any: &dyn Any) -> Option<Box<dyn Any + Send + Sync>> {
        Some(Box::new(any.downcast_ref::<Cloneable>()?.clone()))
    }

    #[test]
    fn clone_with_external_fn() {
        let mut resources = Resources::new();
        resources.insert(Cloneable { value: 77 });

        let cloned = resources
            .clone_by_type_id_with(TypeId::of::<Cloneable>(), cloneable_clone_fn)
            .expect("clone_with should succeed");

        let val = cloned.downcast_ref::<Cloneable>().expect("should downcast");
        assert_eq!(val.value, 77);
    }

    #[test]
    fn clone_with_nonexistent_resource() {
        let resources = Resources::new();

        let result = resources.clone_by_type_id_with(TypeId::of::<Cloneable>(), cloneable_clone_fn);
        assert!(result.is_none(), "should return None for missing resource");
    }

    #[test]
    fn clone_with_write_locked_resource() {
        let mut resources = Resources::new();
        resources.insert(Cloneable { value: 1 });

        let _guard = resources.get_mut::<Cloneable>().unwrap();
        let result = resources.clone_by_type_id_with(TypeId::of::<Cloneable>(), cloneable_clone_fn);
        assert!(
            result.is_none(),
            "should return None when resource is write-locked"
        );
    }
}
