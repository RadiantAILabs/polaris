//! System parameter extraction and dependency injection.
//!
//! Systems declare their dependencies as typed function parameters. The
//! framework resolves each parameter from the [`SystemContext`] before the
//! system executes, so systems remain plain `async fn`s with no manual
//! resource lookup. Contexts are created by
//! [`Server::create_context()`](crate::server::Server::create_context); see
//! [`SystemContext`] for how they are structured and how resources are resolved.
//!
//! # Parameter Types
//!
//!
//! | Type | Access | Scope | Purpose |
//! |------|--------|-------|---------|
//! | [`Res<T>`] | Read-only | Walks context hierarchy | Read resources from any ancestor or global scope |
//! | [`ResMut<T>`] | Read-write | Current scope only | Mutate resources owned by this context |
//! | [`Out<T>`] | Read-only | Current context | Read the return value of a preceding system |
//! | [`ErrOut<T>`] | Read-only | Current context | Read error context from a failed system on an
//! error edge. The system must be reachable through an error edge from a fallible system |
//!
//! # Conflict Detection
//!
//! Each parameter declares an access pattern via [`SystemParam::access()`].
//! The scheduler uses these declarations to detect borrow conflicts between
//! systems *before* execution, following standard read-write lock semantics:
//!
//! - **Read + Read** — compatible (multiple [`Res<T>`] allowed)
//! - **Read + Write** — conflict ([`Res<T>`] and [`ResMut<T>`] to the same `T`)
//! - **Write + Write** — conflict (two [`ResMut<T>`] to the same `T`)
//!
//! # Example
//!
//! A system that reads shared configuration, mutates local state, and
//! consumes the output of a preceding system:
//!
//! ```
//! # use polaris_system::param::{Res, ResMut, Out};
//! # use polaris_system::resource::{GlobalResource, LocalResource};
//! # use polaris_system::system;
//! # struct Config { verbose: bool }
//! # impl GlobalResource for Config {}
//! # struct Memory { messages: Vec<String> }
//! # impl LocalResource for Memory {}
//! # struct PreviousResult { summary: String }
//! #[system]
//! async fn process(
//!     config: Res<Config>,
//!     mut memory: ResMut<Memory>,
//!     previous: Out<PreviousResult>,
//! ) -> String {
//!     memory.messages.push(previous.summary.clone());
//!     if config.verbose {
//!         format!("processed {} messages", memory.messages.len())
//!     } else {
//!         String::new()
//!     }
//! }
//! ```

mod access;

use crate::resource::{
    LocalResource, Output, OutputRef, Outputs, Resource, ResourceRef, ResourceRefMut, Resources,
};
pub use access::{Access, AccessMode, SystemAccess};
use std::any::{Any, TypeId, type_name};
use std::sync::Arc;
use variadics_please::all_tuples;

/// A parameter that can be injected into a system function.
///
/// Types implementing this trait can appear as parameters in system functions.
/// The framework calls [`fetch`](Self::fetch) to resolve each parameter from the
/// [`SystemContext`] before the system executes, and [`access`](Self::access) to
/// declare the borrow pattern for conflict detection.
///
/// See the [module documentation](self) for the built-in parameter types and
/// conflict detection rules.
pub trait SystemParam: Sized {
    /// The item type produced when fetching, parameterized by context lifetime.
    ///
    /// This GAT allows `IntoSystem` to use HRTB bounds like `for<'w> Fn(P::Item<'w>)`,
    /// enabling functions with `Res<T>` params to satisfy the trait bounds.
    type Item<'w>: SystemParam;

    /// Fetches this parameter from the system context.
    ///
    /// # Errors
    ///
    /// Returns [`ParamError`] if the parameter cannot be fetched
    /// (e.g., resource not found, borrow conflict).
    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError>;

    /// Declares the access pattern for this parameter.
    ///
    /// Used by the scheduler to detect conflicts between systems.
    /// The default implementation returns empty access (no conflicts).
    fn access() -> SystemAccess {
        SystemAccess::default()
    }
}

/// Errors that can occur when fetching system parameters.
#[derive(Debug, thiserror::Error)]
pub enum ParamError {
    /// The requested resource was not found.
    #[error("resource not found: {0}")]
    ResourceNotFound(&'static str),

    /// A borrow conflict occurred (e.g., trying to mutably borrow
    /// a resource that is already borrowed).
    #[error("borrow conflict: {0}")]
    BorrowConflict(&'static str),

    /// The requested output was not found (no system has produced it yet).
    #[error("output not found: {0}")]
    OutputNotFound(&'static str),

    /// Error context not found in outputs.
    ///
    /// A `ErrOut<T>` parameter requested error context, but no preceding
    /// system has failed with a matching error type. This typically means the
    /// system is not reachable through an error edge in the graph.
    #[error("error context not found: {0}")]
    ErrorNotFound(&'static str),
}

/// The execution context for a single scope in the resource hierarchy.
///
/// Each `SystemContext` holds:
///
/// - **Resources** — long-lived state. Resources may be
///   [`GlobalResource`](crate::resource::GlobalResource) (server-level,
///   read-only) or [`LocalResource`](crate::resource::LocalResource)
///   (per-context, mutable).
/// - **Outputs** — ephemeral return values produced by preceding systems in
///   the current execution, cleared between agent runs.
/// - **Parent chain** — an optional reference to a parent context, forming a
///   hierarchy that shares resources without sacrificing isolation.
///
/// Systems do not interact with `SystemContext` directly. The executor
/// creates it, and the [`SystemParam`] implementations resolve each
/// parameter from it automatically.
///
/// # Context Hierarchy
///
/// Contexts form a parent-child tree that isolates per-agent state while
/// sharing server-level configuration. By design, a child may read its parent's
/// resources but cannot mutate them:
///
/// ```text
/// Server (global: Config, ToolRegistry)
///    │
///    └── Agent Context (local: AgentMemory)
///           │
///           └── Session Context (local: ConversationHistory)
///                  │
///                  └── Turn Context (local: Scratchpad)
/// ```
///
/// # Resource Lookup Order
///
/// When a system requests [`Res<T>`], the context searches:
///
/// 1. Local resources owned by this context
/// 2. Parent contexts, walking up the chain (closest scope shadows)
/// 3. Server-level global resources
///
/// [`ResMut<T>`] skips the hierarchy and only accesses resources in the
/// current scope.
///
/// # Ownership
///
/// ```text
/// SystemContext<'parent>
/// ├── parent:    Option<&'parent SystemContext>   // read-only ancestor chain
/// ├── globals:   Option<Arc<Resources>>           // server-level globals
/// ├── resources: Resources                        // owned local state
/// └── outputs:   Outputs                          // owned ephemeral outputs
/// ```
///
/// The `globals` is cloned into child contexts, so every context
/// in a hierarchy can access server-level resources regardless of depth.
/// Root contexts (no parent) are `SystemContext<'static>` since the `Arc`
/// keeps globals alive independently of the server.
pub struct SystemContext<'parent> {
    /// Parent context for hierarchical resource lookup.
    /// Read access walks up this chain; write access is current-scope only.
    parent: Option<&'parent SystemContext<'parent>>,
    /// Handle to server's global resources.
    /// Checked after parent chain is exhausted. Cloned into child contexts.
    globals: Option<Arc<Resources>>,
    /// Resources owned by this scope.
    resources: Resources,
    /// Ephemeral system outputs for current execution (owned).
    outputs: Outputs,
}

impl Default for SystemContext<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'parent> SystemContext<'parent> {
    /// Creates a new root context with no parent or global resources.
    ///
    /// Resources and outputs are initialized empty. Use [`with_globals`](Self::with_globals)
    /// to create a context that can access server-level resources.
    #[must_use]
    pub fn new() -> Self {
        Self {
            parent: None,
            globals: None,
            resources: Resources::new(),
            outputs: Outputs::new(),
        }
    }

    /// Creates a new context with access to global resources.
    ///
    /// This is typically called by [`Server::create_context()`] to create
    /// execution contexts that can access server-level resources via `Res<T>`.
    #[must_use]
    pub fn with_globals(globals: Arc<Resources>) -> SystemContext<'static> {
        SystemContext {
            parent: None,
            globals: Some(globals),
            resources: Resources::new(),
            outputs: Outputs::new(),
        }
    }

    /// Builder pattern: inserts a resource and returns self.
    ///
    /// Useful for chaining insertions when creating a context.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::param::SystemContext;
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Clone)] struct Counter { value: i32 }
    /// # impl LocalResource for Counter {}
    /// # #[derive(Clone)] struct Config { name: String }
    /// # impl LocalResource for Config {}
    /// let ctx = SystemContext::new()
    ///     .with(Counter { value: 0 })
    ///     .with(Config { name: "test".into() });
    /// ```
    #[must_use]
    pub fn with<R: LocalResource>(mut self, resource: R) -> Self {
        self.insert(resource);
        self
    }

    /// Creates a child context with this context as its parent.
    ///
    /// The child can read resources from this context (and its ancestors)
    /// but has its own local resources for writes.
    #[must_use]
    pub fn child(&'parent self) -> SystemContext<'parent> {
        SystemContext {
            parent: Some(self),
            globals: self.globals.clone(),
            resources: Resources::new(),
            outputs: Outputs::new(),
        }
    }

    /// Inserts a local resource into this context's scope.
    ///
    /// This resource will shadow any resource of the same type in parent scopes
    /// for read access, and will be the target for mutable access.
    pub fn insert<R: LocalResource>(&mut self, resource: R) {
        self.resources.insert(resource);
    }

    /// Inserts any resource into this context's scope.
    ///
    /// This is primarily used for root contexts that hold global resources,
    /// or for testing. For normal usage, prefer [`insert`] which enforces
    /// the `LocalResource` bound.
    ///
    /// Note: Resources inserted this way can still only be mutated via
    /// `ResMut<T>` if they implement `LocalResource`.
    pub fn insert_resource<R: Resource>(&mut self, resource: R) {
        self.resources.insert(resource);
    }

    /// Inserts a type-erased resource into this context's scope.
    ///
    /// This is used internally by the server to instantiate local resources
    /// from factories. The `type_id` must match the correct type of the boxed
    /// resource.
    pub fn insert_boxed(&mut self, type_id: TypeId, resource: Box<dyn Any + Send + Sync>) {
        self.resources.insert_boxed(type_id, resource);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Resource methods (hierarchical lookup)
    // ─────────────────────────────────────────────────────────────────────

    /// Returns `true` if a resource of type `R` exists in this scope, any parent, or globals.
    #[must_use]
    pub fn contains_resource<R: Resource>(&self) -> bool {
        if self.resources.contains::<R>() {
            return true;
        }
        if let Some(parent) = self.parent {
            return parent.contains_resource::<R>();
        }
        if let Some(globals) = &self.globals {
            return globals.contains::<R>();
        }
        false
    }

    /// Returns `true` if a resource of type `R` exists in this scope only.
    #[must_use]
    pub fn contains_local_resource<R: Resource>(&self) -> bool {
        self.resources.contains::<R>()
    }

    /// Returns an immutable reference to a resource, searching the full
    /// [hierarchy](Self#resource-lookup-order).
    ///
    /// # Errors
    ///
    /// Returns an error if the resource is not found in any scope or is
    /// currently mutably borrowed.
    pub fn get_resource<R: Resource>(&self) -> Result<ResourceRef<R>, ParamError> {
        // Check local scope first
        match self.resources.get::<R>() {
            Ok(r) => return Ok(r),
            Err(crate::resource::ResourceError::BorrowConflict(name)) => {
                return Err(ParamError::BorrowConflict(name));
            }
            Err(crate::resource::ResourceError::NotFound(_)) => {
                // Not in local scope, try parent
            }
        }

        // Walk up to parent
        if let Some(parent) = self.parent {
            return parent.get_resource::<R>();
        }

        // Check global resources (server-level)
        if let Some(globals) = &self.globals {
            match globals.get::<R>() {
                Ok(r) => return Ok(r),
                Err(crate::resource::ResourceError::BorrowConflict(name)) => {
                    return Err(ParamError::BorrowConflict(name));
                }
                Err(crate::resource::ResourceError::NotFound(_)) => {
                    // Not in globals either
                }
            }
        }

        Err(ParamError::ResourceNotFound(type_name::<R>()))
    }

    /// Returns a mutable reference to a resource in the current scope only.
    ///
    /// Unlike [`get_resource`](Self::get_resource) this does not walk the parent chain. See
    /// [Resource Lookup Order](Self#resource-lookup-order) for details.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource is not found in this scope or is
    /// already borrowed.
    pub fn get_resource_mut<R: Resource>(&self) -> Result<ResourceRefMut<R>, ParamError> {
        self.resources.get_mut::<R>().map_err(|err| match err {
            crate::resource::ResourceError::NotFound(name) => ParamError::ResourceNotFound(name),
            crate::resource::ResourceError::BorrowConflict(name) => {
                ParamError::BorrowConflict(name)
            }
        })
    }

    /// Returns a reference to this scope's local resources.
    #[must_use]
    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// Returns a reference to the parent context, if any.
    #[must_use]
    pub fn parent(&self) -> Option<&SystemContext<'parent>> {
        self.parent
    }

    /// Returns a reference to the global resources, if any.
    #[must_use]
    pub fn globals(&self) -> Option<&Resources> {
        self.globals.as_deref()
    }

    /// Returns a clone of the global resources `Arc`, if any.
    ///
    /// This is useful for creating isolated child contexts that share
    /// global infrastructure resources without a parent chain.
    #[must_use]
    pub fn globals_arc(&self) -> Option<Arc<Resources>> {
        self.globals.clone()
    }

    /// Clones a local resource by its [`TypeId`], returning a new boxed value.
    ///
    /// Returns `None` if the resource does not exist in the local scope or
    /// has no clone function registered via [`register_clone_fn`](Self::register_clone_fn).
    /// Only examines the current context's local resources — does not walk the
    /// parent chain or check globals.
    ///
    /// The returned box can be inserted into another context via
    /// [`insert_boxed`](Self::insert_boxed).
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::SystemContext;
    /// use polaris_system::resource::LocalResource;
    /// use std::any::TypeId;
    ///
    /// #[derive(Clone)]
    /// struct Counter { value: i32 }
    /// impl LocalResource for Counter {}
    ///
    /// let mut ctx = SystemContext::new().with(Counter { value: 5 });
    /// ctx.register_clone_fn::<Counter>();
    ///
    /// let cloned = ctx.clone_local_resource(TypeId::of::<Counter>());
    /// assert!(cloned.is_some());
    /// ```
    #[must_use]
    pub fn clone_local_resource(&self, type_id: TypeId) -> Option<Box<dyn Any + Send + Sync>> {
        self.resources.clone_by_type_id(type_id)
    }

    /// Clones a local resource using an externally-provided clone function.
    ///
    /// Unlike [`clone_local_resource`](Self::clone_local_resource), this does not
    /// require [`register_clone_fn`](Self::register_clone_fn) to have been called.
    /// The clone function is supplied by the caller — typically captured at
    /// compile time in a [`ResourceForward`](crate::ResourceForward).
    ///
    /// Returns `None` if the resource does not exist in the local scope or is
    /// currently write-locked.
    #[must_use]
    pub fn clone_local_resource_with(
        &self,
        type_id: TypeId,
        clone_fn: fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        self.resources.clone_by_type_id_with(type_id, clone_fn)
    }

    /// Registers a clone function for a local resource type.
    ///
    /// This must be called before [`clone_local_resource`](Self::clone_local_resource)
    /// can clone resources of type `T`. The resource must already exist in
    /// local scope.
    ///
    /// Returns `true` if the clone function was registered, or `false` if the
    /// resource does not exist in local scope.
    ///
    /// # Ordering constraint
    ///
    /// [`insert`](Self::insert) and [`insert_boxed`](Self::insert_boxed) replace
    /// the entire resource entry, discarding any previously registered clone
    /// function. Always call `register_clone_fn` **after** the final insertion.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::SystemContext;
    /// use polaris_system::resource::LocalResource;
    ///
    /// #[derive(Clone)]
    /// struct Settings { retries: u32 }
    /// impl LocalResource for Settings {}
    ///
    /// let mut ctx = SystemContext::new().with(Settings { retries: 3 });
    /// assert!(ctx.register_clone_fn::<Settings>());
    /// ```
    pub fn register_clone_fn<T: LocalResource + Clone>(&mut self) -> bool {
        self.resources.register_clone_fn::<T>()
    }

    /// Returns `true` if a resource with the given `TypeId` exists in this scope,
    /// any parent, or globals.
    ///
    /// This is useful for validation when the concrete type is not known
    /// at compile time (e.g., validating system access declarations).
    #[must_use]
    pub fn contains_resource_by_type_id(&self, type_id: TypeId) -> bool {
        if self.resources.contains_by_type_id(type_id) {
            return true;
        }
        if let Some(parent) = self.parent {
            return parent.contains_resource_by_type_id(type_id);
        }
        if let Some(globals) = &self.globals {
            return globals.contains_by_type_id(type_id);
        }
        false
    }

    /// Returns `true` if a resource with the given `TypeId` exists in this scope only.
    ///
    /// This is useful for validating mutable access (`ResMut`) which only operates
    /// on the current scope.
    #[must_use]
    pub fn contains_local_resource_by_type_id(&self, type_id: TypeId) -> bool {
        self.resources.contains_by_type_id(type_id)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Output methods (owned, ephemeral system return values)
    // ─────────────────────────────────────────────────────────────────────

    /// Inserts a system output.
    ///
    /// Called by the executor after a system returns a value.
    /// If an output of this type already exists, it is replaced.
    pub fn insert_output<O: Output>(&mut self, output: O) {
        self.outputs.insert(output);
    }

    /// Inserts a type-erased system output.
    ///
    /// Called by the executor when the concrete output type is not known
    /// at compile time. The `type_id` must match the correct type of the value.
    pub fn insert_output_boxed(&mut self, type_id: TypeId, output: Box<dyn Any + Send + Sync>) {
        self.outputs.insert_boxed(type_id, output);
    }

    /// Returns `true` if an output of type `O` exists.
    #[must_use]
    pub fn contains_output<O: Output>(&self) -> bool {
        self.outputs.contains::<O>()
    }

    /// Returns `true` if an output with the given `TypeId` exists.
    ///
    /// This is useful for validation when the concrete type is not known
    /// at compile time (e.g., validating system access declarations).
    #[must_use]
    pub fn contains_output_by_type_id(&self, type_id: TypeId) -> bool {
        self.outputs.contains_by_type_id(type_id)
    }

    /// Gets an immutable reference to an output.
    ///
    /// # Errors
    ///
    /// Returns an error if the output doesn't exist.
    pub fn get_output<O: Output>(&self) -> Result<OutputRef<O>, ParamError> {
        self.outputs.get::<O>().map_err(|err| match err {
            crate::resource::OutputError::NotFound(name) => ParamError::OutputNotFound(name),
            crate::resource::OutputError::Busy(name) => ParamError::BorrowConflict(name),
        })
    }

    /// Clears all outputs.
    ///
    /// Called by the executor between agent runs to reset ephemeral state.
    pub fn clear_outputs(&mut self) {
        self.outputs.clear();
    }

    /// Returns a reference to the underlying outputs.
    #[must_use]
    pub fn outputs(&self) -> &Outputs {
        &self.outputs
    }

    /// Returns a mutable reference to the underlying outputs.
    #[must_use]
    pub fn outputs_mut(&mut self) -> &mut Outputs {
        &mut self.outputs
    }

    /// Takes ownership of this context's outputs, replacing them with an empty container.
    ///
    /// This is used to extract outputs from child contexts (e.g., after parallel
    /// branch execution) before dropping them, so outputs can be merged into the
    /// parent context without borrow conflicts.
    #[must_use]
    pub fn take_outputs(&mut self) -> Outputs {
        std::mem::take(&mut self.outputs)
    }
}

/// Shared, read-only access to a resource.
///
/// `Res<T>` resolves `T` by walking the full
/// [context hierarchy](SystemContext#resource-lookup-order), making it
/// suitable to access both [`GlobalResource`](crate::resource::GlobalResource) and
/// [`LocalResource`](crate::resource::LocalResource) types. Multiple systems
/// may hold `Res<T>` to the same resource simultaneously.
///
/// Implements [`Deref<Target = T>`](core::ops::Deref).
///
/// # Example
///
/// ```
/// # use polaris_system::param::Res;
/// # use polaris_system::resource::GlobalResource;
/// # use polaris_system::system;
/// struct AppConfig {
///     max_retries: usize,
/// }
/// impl GlobalResource for AppConfig {}
///
/// #[system]
/// async fn check_config(config: Res<AppConfig>) -> String {
///     format!("retries allowed: {}", config.max_retries)
/// }
/// ```
pub struct Res<'w, T: Resource> {
    inner: ResourceRef<'w, T>,
}

impl<'w, T: Resource> std::ops::Deref for Res<'w, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// SystemParam impl for Res with ANY lifetime 'a
// The GAT produces Res<'w, T> with the context's lifetime
impl<'a, T: Resource> SystemParam for Res<'a, T> {
    type Item<'w> = Res<'w, T>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        let inner = ctx.get_resource::<T>()?;
        Ok(Res { inner })
    }

    fn access() -> SystemAccess {
        SystemAccess::new().with_read::<T>()
    }
}

/// Mutable access to a local resource.
///
/// `ResMut<T>` provides read-write access to a
/// [`LocalResource`](crate::resource::LocalResource) in the current
/// [`SystemContext`] scope. Unlike [`Res<T>`], it does not walk the context
/// hierarchy — only resources owned by the current scope can be mutated.
///
/// The `T: LocalResource` bound is enforced at compile time.
/// [`GlobalResource`](crate::resource::GlobalResource) types cannot be used
/// with `ResMut<T>`, which guarantees that server-level shared state remains
/// read-only. See [`GlobalResource`](crate::resource::GlobalResource) for a
/// `compile_fail` example demonstrating this invariant.
///
/// Borrows are tracked at runtime via an internal `RwLock`. If the resource
/// is already borrowed (by [`Res<T>`] or another `ResMut<T>`), fetching
/// returns [`ParamError::BorrowConflict`].
///
/// Implements [`Deref<Target = T>`](core::ops::Deref) and
/// [`DerefMut`](core::ops::DerefMut).
///
/// # Example
///
/// ```
/// # use polaris_system::param::ResMut;
/// # use polaris_system::resource::LocalResource;
/// # use polaris_system::system;
/// struct Counter {
///     value: i32,
/// }
/// impl LocalResource for Counter {}
///
/// #[system]
/// async fn increment_counter(mut counter: ResMut<Counter>) {
///     counter.value += 1;
/// }
/// ```
pub struct ResMut<'w, T: LocalResource> {
    inner: ResourceRefMut<'w, T>,
}

impl<'w, T: LocalResource> std::ops::Deref for ResMut<'w, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'w, T: LocalResource> std::ops::DerefMut for ResMut<'w, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// SystemParam impl for ResMut with ANY lifetime 'a
// Requires T: LocalResource for compile-time safety
impl<'a, T: LocalResource> SystemParam for ResMut<'a, T> {
    type Item<'w> = ResMut<'w, T>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        let inner = ctx.get_resource_mut::<T>()?;
        Ok(ResMut { inner })
    }

    fn access() -> SystemAccess {
        SystemAccess::new().with_write::<T>()
    }
}

/// Read-only access to a preceding system's return value.
///
/// `Out<T>` reads ephemeral data from the current execution's output
/// container. Outputs are produced when a system returns a value and are
/// cleared between agent runs. Use [`Res<T>`] instead for long-lived state
/// that persists across executions.
///
/// Implements [`Deref<Target = T>`](core::ops::Deref).
///
/// # Example
///
/// ```
/// # use polaris_system::param::{Res, Out};
/// # use polaris_system::resource::GlobalResource;
/// # use polaris_system::system;
/// # struct LLM;
/// # impl GlobalResource for LLM {}
/// # struct ReasoningResult { action: String }
/// # struct Tools;
/// # impl GlobalResource for Tools {}
/// # impl Tools { fn execute(&self, _: &str) -> ToolResult { ToolResult } }
/// # struct ToolResult;
/// // System A produces a ReasoningResult
/// #[system]
/// async fn reason(llm: Res<LLM>) -> ReasoningResult {
///     ReasoningResult { action: "search".into() }
/// }
///
/// // System B consumes it via Out<T>
/// #[system]
/// async fn execute(reasoning: Out<ReasoningResult>, tools: Res<Tools>) -> ToolResult {
///     tools.execute(&reasoning.action)
/// }
/// ```
pub struct Out<'w, T: Output> {
    inner: OutputRef<'w, T>,
}

impl<'w, T: Output> std::ops::Deref for Out<'w, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// SystemParam impl for Out with ANY lifetime 'a
impl<'a, T: Output> SystemParam for Out<'a, T> {
    type Item<'w> = Out<'w, T>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        let inner = ctx.get_output::<T>()?;
        Ok(Out { inner })
    }

    fn access() -> SystemAccess {
        // Out<T> reads from outputs (previous system's return value)
        // We track this as output read access
        let mut access = SystemAccess::new();
        access.outputs.push(Access::read::<T>());
        access
    }
}

/// Context tag declared by [`ErrOut<T>`] to signal that the system expects
/// to run on a failure path.
pub const ERROR_CONTEXT: &str = "error";

/// Marker trait for output types that represent error context.
///
/// Types implementing `ErrorContext` can be used with [`ErrOut<T>`] to read
/// error information that is only available when execution routes through
/// a failure path. `ErrOut<T>` declares an `"error"` context requirement
/// so higher layers can validate the system is wired correctly.
///
/// `ErrorContext` is a subtrait of [`Output`], so any implementing type is
/// automatically storable in the [`Outputs`] container via the existing
/// blanket impl.
///
/// # Example
///
/// ```
/// use polaris_system::param::ErrorContext;
///
/// #[derive(Debug, Clone)]
/// struct FailureInfo { message: String }
///
/// impl ErrorContext for FailureInfo {}
/// ```
pub trait ErrorContext: Output {}

/// Read-only access to a contextual output that requires an error path.
///
/// `ErrOut<T>` reads from the same [`Outputs`] store as [`Out<T>`], but
/// additionally declares an `"error"` [context requirement](SystemAccess::require_context).
/// This allows higher layers to validate that the system is reachable
/// through the correct path before execution.
///
/// # When to Use
///
/// - **`Out<T>`**: Reading a previous system's return value (normal path)
/// - **`ErrOut<T>`**: Reading error context that is only available after
///   a system failure
///
/// # Example
///
/// ```
/// use polaris_system::param::{ErrOut, ErrorContext};
/// use polaris_system::system;
///
/// #[derive(Debug, Clone)]
/// struct FailureInfo { message: String }
/// impl ErrorContext for FailureInfo {}
///
/// #[system]
/// async fn handle_failure(info: ErrOut<FailureInfo>) {
///     eprint!("{}", info.message);
/// }
/// ```
pub struct ErrOut<'w, T: ErrorContext> {
    inner: OutputRef<'w, T>,
}

impl<'w, T: ErrorContext> std::ops::Deref for ErrOut<'w, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T: ErrorContext> SystemParam for ErrOut<'a, T> {
    type Item<'w> = ErrOut<'w, T>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        let inner = ctx.get_output::<T>().map_err(|err| match err {
            ParamError::OutputNotFound(_) => ParamError::ErrorNotFound(type_name::<T>()),
            other => other,
        })?;
        Ok(ErrOut { inner })
    }

    fn access() -> SystemAccess {
        let mut access = SystemAccess::new();
        access.outputs.push(Access::read::<T>());
        access.require_context(ERROR_CONTEXT);
        access
    }
}

/// Optional output access.
///
/// Returns `None` if the output doesn't exist instead of erroring.
impl<'a, T: Output> SystemParam for Option<Out<'a, T>> {
    type Item<'w> = Option<Out<'w, T>>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        match <Out<'a, T> as SystemParam>::fetch(ctx) {
            Ok(out) => Ok(Some(out)),
            Err(ParamError::OutputNotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn access() -> SystemAccess {
        <Out<'a, T> as SystemParam>::access()
    }
}

// Unit type implementation
impl SystemParam for () {
    type Item<'w> = ();

    fn fetch<'w>(_ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        Ok(())
    }
}

// Tuple implementations for multiple parameters
macro_rules! impl_system_param_tuple {
    ($($param:ident),*) => {
        impl<$($param: SystemParam),*> SystemParam for ($($param,)*) {
            type Item<'w> = ($($param::Item<'w>,)*);

            fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
                Ok(($($param::fetch(ctx)?,)*))
            }

            fn access() -> SystemAccess {
                let mut access = SystemAccess::new();
                $(access.merge(&$param::access());)*
                access
            }
        }
    };
}

// Generate impls for tuples of size 1 to 8
all_tuples!(impl_system_param_tuple, 1, 8, P);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::LocalResource;

    #[derive(Debug, PartialEq)]
    struct Counter {
        value: i32,
    }

    // Counter is a LocalResource - can be mutated via ResMut<Counter>
    impl LocalResource for Counter {}

    #[derive(Debug, PartialEq)]
    struct Config {
        name: String,
    }

    // Config is also LocalResource for these tests
    // (In real usage, Config would likely be GlobalResource)
    impl LocalResource for Config {}

    #[test]
    fn context_get_resource() {
        let ctx = SystemContext::new().with(Counter { value: 42 });
        let counter = ctx.get_resource::<Counter>().unwrap();
        assert_eq!(counter.value, 42);
    }

    #[test]
    fn res_fetch() {
        let ctx = SystemContext::new().with(Counter { value: 10 });
        let res = Res::<Counter>::fetch(&ctx).unwrap();
        assert_eq!(res.value, 10);
    }

    #[test]
    fn res_mut_fetch_and_modify() {
        let ctx = SystemContext::new().with(Counter { value: 0 });
        {
            let mut res = ResMut::<Counter>::fetch(&ctx).unwrap();
            res.value += 5;
        }

        let res = Res::<Counter>::fetch(&ctx).unwrap();
        assert_eq!(res.value, 5);
    }

    #[test]
    fn multiple_res_allowed() {
        let ctx = SystemContext::new().with(Counter { value: 42 });
        let res1 = Res::<Counter>::fetch(&ctx).unwrap();
        let res2 = Res::<Counter>::fetch(&ctx).unwrap();

        assert_eq!(res1.value, res2.value);
    }

    #[test]
    fn res_mut_blocks_res() {
        let ctx = SystemContext::new().with(Counter { value: 42 });
        let _res_mut = ResMut::<Counter>::fetch(&ctx).unwrap();
        let result = Res::<Counter>::fetch(&ctx);

        assert!(matches!(result, Err(ParamError::BorrowConflict(_))));
    }

    #[test]
    fn res_blocks_res_mut() {
        let ctx = SystemContext::new().with(Counter { value: 42 });
        let _res = Res::<Counter>::fetch(&ctx).unwrap();
        let result = ResMut::<Counter>::fetch(&ctx);

        assert!(matches!(result, Err(ParamError::BorrowConflict(_))));
    }

    #[test]
    fn missing_resource_error() {
        let ctx = SystemContext::new();

        let result = Res::<Counter>::fetch(&ctx);
        assert!(matches!(result, Err(ParamError::ResourceNotFound(_))));
    }

    #[test]
    fn tuple_param_fetch() {
        let ctx = SystemContext::new()
            .with(Counter { value: 1 })
            .with(Config {
                name: "test".into(),
            });
        let (counter, config) = <(Res<Counter>, Res<Config>)>::fetch(&ctx).unwrap();
        assert_eq!(counter.value, 1);
        assert_eq!(config.name, "test");
    }

    #[test]
    fn unit_param_fetch() {
        let ctx = SystemContext::new();
        let result = <()>::fetch(&ctx);
        assert!(result.is_ok());
    }

    // ─────────────────────────────────────────────────────────────────────
    // Hierarchical context tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn child_sees_parent_resources() {
        let parent = SystemContext::new().with(Counter { value: 42 });
        let child = parent.child();

        let counter = child.get_resource::<Counter>().unwrap();
        assert_eq!(counter.value, 42);
    }

    #[test]
    fn child_can_shadow_parent() {
        let parent = SystemContext::new().with(Counter { value: 1 });
        let child = parent.child().with(Counter { value: 2 });

        // Child sees its own value
        assert_eq!(child.get_resource::<Counter>().unwrap().value, 2);
        // Parent still has original
        assert_eq!(parent.get_resource::<Counter>().unwrap().value, 1);
    }

    #[test]
    fn mutation_only_in_current_scope() {
        let parent = SystemContext::new().with(Counter { value: 1 });
        let child = parent.child();

        // Can read from parent
        assert!(child.get_resource::<Counter>().is_ok());

        // Cannot mutate parent's resource (not in child's local scope)
        assert!(child.get_resource_mut::<Counter>().is_err());
    }

    #[test]
    fn child_can_mutate_own_resources() {
        let parent = SystemContext::new().with(Counter { value: 1 });
        let child = parent.child().with(Counter { value: 10 });

        // Child can mutate its own shadowed resource
        {
            let mut counter = child.get_resource_mut::<Counter>().unwrap();
            counter.value += 5;
        }

        assert_eq!(child.get_resource::<Counter>().unwrap().value, 15);
        // Parent unchanged
        assert_eq!(parent.get_resource::<Counter>().unwrap().value, 1);
    }

    #[test]
    fn deep_hierarchy() {
        let root = SystemContext::new().with(Counter { value: 1 });
        let level1 = root.child().with(Config {
            name: "level1".into(),
        });
        let level2 = level1.child();

        // level2 can see both Counter (from root) and Config (from level1)
        assert_eq!(level2.get_resource::<Counter>().unwrap().value, 1);
        assert_eq!(level2.get_resource::<Config>().unwrap().name, "level1");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Output tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct ReasoningResult {
        action: String,
    }

    #[test]
    fn context_insert_and_get_output() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "search".into(),
        });

        let output = ctx.get_output::<ReasoningResult>().unwrap();
        assert_eq!(output.action, "search");
    }

    #[test]
    fn out_fetch() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "calculate".into(),
        });

        let out = Out::<ReasoningResult>::fetch(&ctx).unwrap();
        assert_eq!(out.action, "calculate");
    }

    #[test]
    fn out_not_found_error() {
        let ctx = SystemContext::new();

        let result = Out::<ReasoningResult>::fetch(&ctx);
        assert!(matches!(result, Err(ParamError::OutputNotFound(_))));
    }

    #[test]
    fn optional_out_returns_none() {
        let ctx = SystemContext::new();

        let result = Option::<Out<ReasoningResult>>::fetch(&ctx).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn optional_out_returns_some() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "test".into(),
        });

        let result = Option::<Out<ReasoningResult>>::fetch(&ctx).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().action, "test");
    }

    #[test]
    fn clear_outputs() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "test".into(),
        });

        assert!(ctx.contains_output::<ReasoningResult>());

        ctx.clear_outputs();

        assert!(!ctx.contains_output::<ReasoningResult>());
        assert!(Out::<ReasoningResult>::fetch(&ctx).is_err());
    }

    #[test]
    fn outputs_and_resources_are_separate() {
        let mut ctx = SystemContext::new().with(Counter { value: 42 });
        ctx.insert_output(Counter { value: 100 });

        // They should be separate
        {
            let res = Res::<Counter>::fetch(&ctx).unwrap();
            let out = Out::<Counter>::fetch(&ctx).unwrap();

            assert_eq!(res.value, 42);
            assert_eq!(out.value, 100);
        } // Drop borrows before clearing

        // Clearing outputs doesn't affect resources
        ctx.clear_outputs();

        assert!(Res::<Counter>::fetch(&ctx).is_ok());
        assert!(Out::<Counter>::fetch(&ctx).is_err());
    }

    #[test]
    fn multiple_out_reads_allowed() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "test".into(),
        });

        let out1 = Out::<ReasoningResult>::fetch(&ctx).unwrap();
        let out2 = Out::<ReasoningResult>::fetch(&ctx).unwrap();

        assert_eq!(out1.action, out2.action);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Type-erased insert tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn context_insert_boxed_resource() {
        use std::any::{Any, TypeId};

        let mut ctx = SystemContext::new();

        let type_id = TypeId::of::<Counter>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(Counter { value: 77 });
        ctx.insert_boxed(type_id, boxed);

        // Should be retrievable via normal get
        let counter = ctx.get_resource::<Counter>().unwrap();
        assert_eq!(counter.value, 77);
    }

    #[test]
    fn context_insert_output_boxed() {
        use std::any::{Any, TypeId};

        let mut ctx = SystemContext::new();

        let type_id = TypeId::of::<ReasoningResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ReasoningResult {
            action: "boxed_action".into(),
        });
        ctx.insert_output_boxed(type_id, boxed);

        // Should be retrievable via normal get
        let result = ctx.get_output::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "boxed_action");
    }

    #[test]
    fn contains_resource_by_type_id() {
        use std::any::TypeId;

        let ctx = SystemContext::new().with(Counter { value: 1 });

        let counter_id = TypeId::of::<Counter>();
        let config_id = TypeId::of::<Config>();

        assert!(ctx.contains_resource_by_type_id(counter_id));
        assert!(!ctx.contains_resource_by_type_id(config_id));
    }

    #[test]
    fn contains_local_resource_by_type_id() {
        use std::any::TypeId;

        let parent = SystemContext::new().with(Counter { value: 1 });
        let child = parent.child().with(Config {
            name: "child".into(),
        });

        let counter_id = TypeId::of::<Counter>();
        let config_id = TypeId::of::<Config>();

        // Child can see Counter in hierarchy but not locally
        assert!(child.contains_resource_by_type_id(counter_id));
        assert!(!child.contains_local_resource_by_type_id(counter_id));

        // Child has Config locally
        assert!(child.contains_resource_by_type_id(config_id));
        assert!(child.contains_local_resource_by_type_id(config_id));
    }

    #[test]
    fn contains_output_by_type_id() {
        use std::any::TypeId;

        let mut ctx = SystemContext::new();
        ctx.insert_output(ReasoningResult {
            action: "test".into(),
        });

        let reasoning_id = TypeId::of::<ReasoningResult>();
        let counter_id = TypeId::of::<Counter>();

        assert!(ctx.contains_output_by_type_id(reasoning_id));
        assert!(!ctx.contains_output_by_type_id(counter_id));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Deep hierarchy tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct Level1Resource {
        name: String,
    }
    impl LocalResource for Level1Resource {}

    #[derive(Debug, PartialEq)]
    struct Level2Resource {
        value: i32,
    }
    impl LocalResource for Level2Resource {}

    #[derive(Debug, PartialEq)]
    struct Level3Resource {
        data: Vec<u8>,
    }
    impl LocalResource for Level3Resource {}

    #[test]
    fn three_level_hierarchy() {
        let root = SystemContext::new().with(Counter { value: 0 });
        let level1 = root.child().with(Level1Resource { name: "L1".into() });
        let level2 = level1.child().with(Level2Resource { value: 42 });
        let level3 = level2.child().with(Level3Resource {
            data: vec![1, 2, 3],
        });

        // Level 3 can see all resources up the chain
        assert_eq!(level3.get_resource::<Counter>().unwrap().value, 0);
        assert_eq!(level3.get_resource::<Level1Resource>().unwrap().name, "L1");
        assert_eq!(level3.get_resource::<Level2Resource>().unwrap().value, 42);
        assert_eq!(
            level3.get_resource::<Level3Resource>().unwrap().data,
            vec![1, 2, 3]
        );
    }

    #[test]
    fn four_level_hierarchy_shadowing() {
        let root = SystemContext::new().with(Counter { value: 1 });
        let level1 = root.child().with(Counter { value: 10 });
        let level2 = level1.child().with(Counter { value: 100 });
        let level3 = level2.child().with(Counter { value: 1000 });

        // Each level sees its own shadowed Counter
        assert_eq!(root.get_resource::<Counter>().unwrap().value, 1);
        assert_eq!(level1.get_resource::<Counter>().unwrap().value, 10);
        assert_eq!(level2.get_resource::<Counter>().unwrap().value, 100);
        assert_eq!(level3.get_resource::<Counter>().unwrap().value, 1000);

        // Each can only mutate its own
        {
            let mut counter = level3.get_resource_mut::<Counter>().unwrap();
            counter.value += 1;
        }
        assert_eq!(level3.get_resource::<Counter>().unwrap().value, 1001);
        // Others unchanged
        assert_eq!(level2.get_resource::<Counter>().unwrap().value, 100);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Tuple parameter tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn tuple_param_three_elements() {
        let mut ctx = SystemContext::new()
            .with(Counter { value: 1 })
            .with(Config {
                name: "test".into(),
            });
        ctx.insert_output(ReasoningResult {
            action: "go".into(),
        });

        let (counter, config, out) =
            <(Res<Counter>, Res<Config>, Out<ReasoningResult>)>::fetch(&ctx).unwrap();

        assert_eq!(counter.value, 1);
        assert_eq!(config.name, "test");
        assert_eq!(out.action, "go");
    }

    #[test]
    fn tuple_param_with_mutable() {
        let ctx = SystemContext::new()
            .with(Counter { value: 1 })
            .with(Config {
                name: "test".into(),
            });

        let (counter, mut config) = <(Res<Counter>, ResMut<Config>)>::fetch(&ctx).unwrap();

        assert_eq!(counter.value, 1);
        config.name = "modified".into();
        drop(config);
        drop(counter);

        let config = Res::<Config>::fetch(&ctx).unwrap();
        assert_eq!(config.name, "modified");
    }

    // ─────────────────────────────────────────────────────────────────────
    // SystemParam access declaration tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn res_declares_read_access() {
        let access = <Res<Counter>>::access();
        assert_eq!(access.resources.len(), 1);
        assert_eq!(access.resources[0].mode, AccessMode::Read);
        assert!(access.resources[0].type_name.contains("Counter"));
    }

    #[test]
    fn res_mut_declares_write_access() {
        let access = <ResMut<Counter>>::access();
        assert_eq!(access.resources.len(), 1);
        assert_eq!(access.resources[0].mode, AccessMode::Write);
        assert!(access.resources[0].type_name.contains("Counter"));
    }

    #[test]
    fn out_declares_output_access() {
        let access = <Out<ReasoningResult>>::access();
        assert_eq!(access.outputs.len(), 1);
        assert_eq!(access.outputs[0].mode, AccessMode::Read);
        assert!(access.outputs[0].type_name.contains("ReasoningResult"));
    }

    #[test]
    fn tuple_access_merges_all() {
        let access = <(Res<Counter>, ResMut<Config>, Out<ReasoningResult>)>::access();

        assert_eq!(access.resources.len(), 2);
        assert_eq!(access.outputs.len(), 1);
    }

    #[test]
    fn unit_declares_empty_access() {
        let access = <()>::access();
        assert!(access.is_empty());
    }

    // ─────────────────────────────────────────────────────────────────────
    // take_outputs + merge pattern tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn take_outputs_and_merge_into_parent() {
        let mut parent = SystemContext::new();
        parent.insert_output(ReasoningResult {
            action: "parent".into(),
        });

        // Simulate parallel branch: create child, produce output, extract, drop, merge
        let child_outputs = {
            let mut child = parent.child();
            child.insert_output(ReasoningResult {
                action: "child".into(),
            });
            child.take_outputs()
        };
        // child is dropped here, releasing borrow on parent

        parent.outputs_mut().merge_from(child_outputs);

        let output = parent.get_output::<ReasoningResult>().unwrap();
        assert_eq!(output.action, "child");
    }

    // ─────────────────────────────────────────────────────────────────────
    // ErrOut<T> tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct TestError {
        message: String,
    }

    impl ErrorContext for TestError {}

    #[test]
    fn err_out_fetch_returns_error_from_outputs() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(TestError {
            message: "boom".into(),
        });

        let err_out = ErrOut::<TestError>::fetch(&ctx).unwrap();
        assert_eq!(err_out.message, "boom");
    }

    #[test]
    fn err_out_fetch_returns_error_when_missing() {
        let ctx = SystemContext::new();
        let result = ErrOut::<TestError>::fetch(&ctx);
        assert!(matches!(result, Err(ParamError::ErrorNotFound(_))));
    }

    #[test]
    fn err_out_deref_to_inner_type() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(TestError {
            message: "test".into(),
        });

        let err_out = ErrOut::<TestError>::fetch(&ctx).unwrap();
        // Deref gives us &TestError
        let inner: &TestError = &err_out;
        assert_eq!(inner.message, "test");
    }

    #[test]
    fn err_out_declares_output_access_and_context_requirement() {
        let access = <ErrOut<TestError>>::access();
        assert_eq!(access.outputs.len(), 1);
        assert_eq!(access.outputs[0].mode, AccessMode::Read);
        assert!(access.outputs[0].type_name.contains("TestError"));
        assert_eq!(access.context_requirements, vec![ERROR_CONTEXT]);
    }

    // ─────────────────────────────────────────────────────────────────────
    // clone_local_resource tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq)]
    struct Cloneable {
        value: i32,
    }
    impl LocalResource for Cloneable {}

    #[test]
    fn clone_local_resource_roundtrip() {
        // Insert a resource and register its clone function
        let mut ctx = SystemContext::new().with(Cloneable { value: 77 });
        ctx.register_clone_fn::<Cloneable>();

        // Clone the resource by TypeId
        let type_id = TypeId::of::<Cloneable>();
        let cloned_box = ctx
            .clone_local_resource(type_id)
            .expect("clone should succeed");

        // Insert the cloned box into a second context
        let mut ctx2 = SystemContext::new();
        ctx2.insert_boxed(type_id, cloned_box);

        // Resolve via Res<T> and verify the value matches
        let res = Res::<Cloneable>::fetch(&ctx2).unwrap();
        assert_eq!(res.value, 77);
    }

    // ─────────────────────────────────────────────────────────────────────
    // clone_local_resource_with tests
    // ─────────────────────────────────────────────────────────────────────

    fn cloneable_clone_fn(any: &dyn Any) -> Option<Box<dyn Any + Send + Sync>> {
        Some(Box::new(any.downcast_ref::<Cloneable>()?.clone()))
    }

    #[test]
    fn clone_local_resource_with_external_fn() {
        let ctx = SystemContext::new().with(Cloneable { value: 42 });

        let cloned = ctx
            .clone_local_resource_with(TypeId::of::<Cloneable>(), cloneable_clone_fn)
            .expect("clone_with should succeed");

        let val = cloned.downcast_ref::<Cloneable>().expect("should downcast");
        assert_eq!(val.value, 42);
    }

    #[test]
    fn clone_local_resource_with_missing_returns_none() {
        let ctx = SystemContext::new();

        let result = ctx.clone_local_resource_with(TypeId::of::<Cloneable>(), cloneable_clone_fn);
        assert!(result.is_none());
    }

    #[test]
    fn clone_local_resource_with_does_not_walk_parent() {
        let parent = SystemContext::new().with(Cloneable { value: 99 });
        let child = parent.child();

        // clone_local_resource_with only checks local scope, not parent
        let result = child.clone_local_resource_with(TypeId::of::<Cloneable>(), cloneable_clone_fn);
        assert!(result.is_none(), "should not find resource in parent chain");
    }

    // ─────────────────────────────────────────────────────────────────────
    // globals_arc tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn globals_arc_none_without_globals() {
        let ctx = SystemContext::new();
        assert!(ctx.globals_arc().is_none());
    }

    #[test]
    fn globals_arc_some_with_globals() {
        let globals = Arc::new(Resources::new());
        let ctx = SystemContext::with_globals(globals.clone());

        let arc = ctx.globals_arc().expect("should return Some");
        assert!(Arc::ptr_eq(&arc, &globals));
    }

    #[test]
    fn globals_arc_inherited_by_child() {
        let globals = Arc::new(Resources::new());
        let parent = SystemContext::with_globals(globals.clone());
        let child = parent.child();

        let arc = child.globals_arc().expect("child should inherit globals");
        assert!(Arc::ptr_eq(&arc, &globals));
    }
}
