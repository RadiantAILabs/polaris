//! Server runtime for plugin orchestration.
//!
//! The [`Server`] is the central runtime that manages plugins and resources.
//! It is purely a plugin orchestrator, all functionality is provided by plugins.
//!
//! ```
//! # use polaris_system::server::Server;
//! # use polaris_system::plugin::{Plugin, Version};
//! # struct DefaultPlugins;
//! # impl Plugin for DefaultPlugins { const ID: &'static str = "default"; const VERSION: Version = Version::new(0,0,1); fn build(&self, _: &mut Server) {} }
//!
//! # tokio_test::block_on(async {
//! Server::new()
//!     .add_plugins(DefaultPlugins)
//!     .run()
//!     .await;
//! # });
//! ```
//!
//! # Resource Scoping
//!
//! The server distinguishes between two resource scopes:
//!
//! - **Global resources** ([`GlobalResource`](crate::resource::GlobalResource)) —
//!   server-lifetime, read-only via [`Res<T>`](crate::param::Res)
//! - **Local resources** ([`LocalResource`](crate::resource::LocalResource)) —
//!   per-context, mutable via [`ResMut<T>`](crate::param::ResMut)
//!
//! ```
//! # use polaris_system::server::Server;
//! # use polaris_system::resource::{GlobalResource, LocalResource};
//! # #[derive(Default)] struct Config;
//! # impl GlobalResource for Config {}
//! # struct Memory;
//! # impl LocalResource for Memory {}
//! # impl Memory { fn new() -> Self { Self } }
//! # let mut server = Server::new();
//! // Global: shared across all contexts, read-only via Res<T>
//! server.insert_global(Config::default());
//!
//! // Local: fresh instance per context, mutable via ResMut<T>
//! server.register_local(Memory::new);
//!
//! // Each call to create_context() produces a SystemContext
//! // with its own local resources and access to globals
//! let ctx = server.create_context();
//! ```
//!
//! See [`SystemContext`](crate::param::SystemContext) for how systems resolve
//! parameters from contexts.
//!
//! # Lifecycle
//!
//! The server manages a strict plugin lifecycle:
//!
//! 1. **Dependency Resolution** - Validate and topologically sort plugins
//! 2. **Build Phase** - Call `plugin.build()` in dependency order
//! 3. **Ready Phase** - Call `plugin.ready()` in dependency order
//! 4. **Run Loop** - Execute systems and call `plugin.update()` (Layer 2)
//! 5. **Cleanup Phase** - Call `plugin.cleanup()` in reverse order

use crate::api::API;
use crate::param::SystemContext;
use crate::plugin::{DynPlugin, Plugin, PluginId, Plugins, Schedule, ScheduleId};
use crate::resource::{
    GlobalResource, LocalResource, Resource, ResourceRef, ResourceRefMut, Resources,
};
use hashbrown::{HashMap, HashSet};
use std::any::TypeId;
use std::sync::{Arc, OnceLock};

// ─────────────────────────────────────────────────────────────────────────────
// Server
// ─────────────────────────────────────────────────────────────────────────────

/// Type-erased resource for dynamic storage.
type BoxedResource = Box<dyn std::any::Any + Send + Sync>;

/// Factory function that creates a local resource instance.
type LocalFactory = Arc<dyn Fn() -> BoxedResource + Send + Sync>;

/// Type-erased API for dynamic storage.
type BoxedAPI = Box<dyn std::any::Any + Send + Sync>;

/// Represents the build state of the server.
///
/// The server progresses through these states linearly:
/// `NotStarted` → `Building` → `Built`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BuildState {
    /// Server has not started building yet (initial state).
    #[default]
    NotStarted,
    /// Server is currently in the build phase (`finish()` is executing).
    Building,
    /// Server has completed building (`finish()` has returned).
    Built,
}

/// The runtime that orchestrates plugins and manages resources.
///
/// See the [module-level documentation](crate::server) for resource scoping
/// and lifecycle details.
pub struct Server {
    /// Global resources (server-lifetime, read-only, shared across all contexts).
    ///
    /// Registered via [`insert_global()`](Self::insert_global).
    /// Accessed via `Res<T>` (not `ResMut<T>`).
    global: Arc<Resources>,

    /// Resources field for server-wide mutable storage.
    ///
    /// Resources inserted via [`insert_resource()`](Self::insert_resource) go here.
    /// We keep this separate from `global` for mutable access to resources not
    /// accessible to systems via `Res<T>` and `ResMut<T>`. This is useful
    /// for plugins that need mutable server-wide state.
    /// Note: This is safe because Plugins' `update()` calls are not run concurrently.
    resources: Resources,

    /// Factories for creating per-context local resources.
    ///
    /// Registered via [`register_local()`](Self::register_local).
    /// Each call to [`create_context()`](Self::create_context) invokes these factories
    /// to create fresh resource instances.
    local_factories: HashMap<TypeId, LocalFactory>,

    /// APIs for plugin orchestration (build-time capability registries).
    ///
    /// Registered via [`insert_api()`](Self::insert_api).
    /// Accessed via [`api()`](Self::api) by plugins during build/ready phases.
    /// APIs are NOT accessed by systems.
    apis: HashMap<TypeId, BoxedAPI>,

    /// Plugins pending build (not yet sorted).
    pending_plugins: Vec<PluginEntry>,

    /// Plugins that have been built, in sorted order.
    built_plugins: Vec<PluginEntry>,

    /// Set of plugin IDs that have been added (for duplicate detection).
    plugin_ids: HashSet<PluginId>,

    /// Maps schedule → plugin indices that registered for it.
    ///
    /// Indices are in dependency order (same as `built_plugins`).
    /// Built during `finish()` from plugin `tick_schedules()`.
    schedule_registry: HashMap<ScheduleId, Vec<usize>>,

    /// The current build state of the server.
    ///
    /// Progresses linearly: `NotStarted` → `Building` → `Built`.
    build_state: BuildState,

    /// Shared handle for deferred global binding in [`ContextFactory`].
    ///
    /// Filled at the end of [`finish()`](Self::finish) so that factories created
    /// during the `ready()` phase can resolve globals without bumping the
    /// `Arc` reference count on [`global`](Self::global) prematurely.
    deferred_globals: Arc<OnceLock<Arc<Resources>>>,
}

/// Internal entry for a registered plugin.
struct PluginEntry {
    /// The plugin's unique identifier.
    ///
    /// Used for dependency resolution and duplicate detection.
    id: PluginId,

    /// The plugin instance.
    plugin: Box<dyn DynPlugin>,
}

/// A default plugin awaiting auto-registration.
struct BoxedDefault {
    /// The plugin that offered this default — surfaced in the tracing log when
    /// the default is actually consumed.
    provider: PluginId,
    /// The default plugin instance, ready to insert if its `PluginId` is missing.
    plugin: Box<dyn DynPlugin>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// Creates a new empty server.
    ///
    /// The server starts with no plugins and no resources.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: Arc::new(Resources::new()),
            resources: Resources::new(),
            local_factories: HashMap::new(),
            apis: HashMap::new(),
            pending_plugins: Vec::new(),
            built_plugins: Vec::new(),
            plugin_ids: HashSet::new(),
            schedule_registry: HashMap::new(),
            build_state: BuildState::NotStarted,
            deferred_globals: Arc::new(OnceLock::new()),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Plugin Management
    // ─────────────────────────────────────────────────────────────────────────

    /// Adds one or more plugins to the server.
    ///
    /// Accepts either:
    /// - A single plugin implementing [`Plugin`]
    /// - A [`PluginGroupBuilder`](crate::plugin::PluginGroupBuilder) containing multiple plugins
    ///
    /// # Panics
    ///
    /// Panics if a unique plugin is added twice.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::plugin::{Plugin, Version};
    /// # struct TracingPlugin;
    /// # impl TracingPlugin { fn default() -> Self { Self } }
    /// # impl Plugin for TracingPlugin { const ID: &'static str = "tracing"; const VERSION: Version = Version::new(0,0,1); fn build(&self, _: &mut Server) {} }
    /// # struct DefaultPlugins;
    /// # impl Plugin for DefaultPlugins { const ID: &'static str = "default"; const VERSION: Version = Version::new(0,0,1); fn build(&self, _: &mut Server) {} }
    /// # struct MyPlugin;
    /// # impl Plugin for MyPlugin { const ID: &'static str = "my"; const VERSION: Version = Version::new(0,0,1); fn build(&self, _: &mut Server) {} }
    /// # let mut server = Server::new();
    /// server
    ///     .add_plugins(TracingPlugin::default())
    ///     .add_plugins(DefaultPlugins)
    ///     .add_plugins(MyPlugin);
    /// ```
    pub fn add_plugins<P: Plugins>(&mut self, plugins: P) -> &mut Self {
        plugins.add_to_server(self);
        self
    }

    /// Internal method to add a boxed plugin with its captured ID.
    ///
    /// Called by [`Plugins::add_to_server`] implementations.
    ///
    /// # Arguments
    ///
    /// * `plugin` - The boxed plugin instance
    pub(crate) fn add_plugin_boxed(&mut self, plugin: Box<dyn DynPlugin>) {
        let id = plugin.id();

        // Reject duplicate plugins
        if self.plugin_ids.contains(&id) {
            panic!("Plugin '{}' was already added.", id);
        }

        // Track this plugin ID
        self.plugin_ids.insert(id.clone());

        let entry = PluginEntry { id, plugin };

        // If we're in the build phase, the plugin is built immediately
        if self.build_state == BuildState::Building {
            // Build immediately and add to built list
            entry.plugin.build(self);
            self.built_plugins.push(entry);
        } else {
            // Queue for later
            self.pending_plugins.push(entry);
        }
    }

    /// Returns true if a plugin of the given type has been added.
    #[must_use]
    pub fn has_plugin<P: Plugin>(&self) -> bool {
        self.plugin_ids.contains(&PluginId::of::<P>())
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Resource Access
    // ─────────────────────────────────────────────────────────────────────────

    /// Inserts a resource into the server.
    ///
    /// If a resource of this type already exists, it is replaced and the
    /// old value is returned.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::resource::Resource;
    /// # struct MyConfig { value: i32 }
    /// # let mut server = Server::new();
    /// server.insert_resource(MyConfig { value: 42 });
    /// ```
    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> Option<R> {
        self.resources.insert(resource)
    }

    /// Returns true if a resource of type `R` exists.
    #[must_use]
    pub fn contains_resource<R: Resource>(&self) -> bool {
        self.resources.contains::<R>()
    }

    /// Gets an immutable reference to a resource.
    ///
    /// Returns `None` if the resource doesn't exist or is mutably borrowed.
    #[must_use]
    pub fn get_resource<R: Resource>(&self) -> Option<ResourceRef<R>> {
        self.resources.get::<R>().ok()
    }

    /// Gets a mutable reference to a resource.
    ///
    /// Returns `None` if the resource doesn't exist or is already borrowed.
    #[must_use]
    pub fn get_resource_mut<R: Resource>(&self) -> Option<ResourceRefMut<R>> {
        self.resources.get_mut::<R>().ok()
    }

    /// Removes a resource from the server and returns it.
    ///
    /// Returns `None` if the resource doesn't exist.
    pub fn remove_resource<R: Resource>(&mut self) -> Option<R> {
        self.resources.remove::<R>()
    }

    /// Returns a reference to the underlying resources container.
    #[must_use]
    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// Returns a mutable reference to the underlying resources container.
    #[must_use]
    pub fn resources_mut(&mut self) -> &mut Resources {
        &mut self.resources
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Scoped Resources (Global / Local)
    // ─────────────────────────────────────────────────────────────────────────

    /// Inserts a [`GlobalResource`] into the server.
    ///
    /// If a resource of this type already exists, it is replaced and the
    /// old value is returned.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::resource::GlobalResource;
    /// # use polaris_system::param::Res;
    /// # use polaris_system::system;
    /// struct Config { name: String }
    /// impl GlobalResource for Config {}
    ///
    /// # let mut server = Server::new();
    /// server.insert_global(Config { name: "my-agent".into() });
    ///
    /// // The global resource can later be used in a system.
    /// #[system]
    /// async fn my_system(config: Res<Config>) {
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if system contexts have already been created, since global
    /// resources are shared via `Arc` and system contexts hold references
    /// to the global container. This is a safety measure to prevent mutable
    /// access to globals after system contexts have been created.
    pub fn insert_global<R: GlobalResource>(&mut self, resource: R) -> Option<R> {
        Arc::get_mut(&mut self.global)
            .expect("cannot insert globals after system contexts have been created")
            .insert(resource)
    }

    /// Returns true if a global resource of type `R` exists.
    #[must_use]
    pub fn contains_global<R: GlobalResource>(&self) -> bool {
        self.global.contains::<R>()
    }

    /// Gets an immutable reference to a global resource.
    ///
    /// Returns `None` if the resource doesn't exist.
    #[must_use]
    pub fn get_global<R: GlobalResource>(&self) -> Option<ResourceRef<R>> {
        self.global.get::<R>().ok()
    }

    /// Registers a factory for creating per-context [`LocalResource`] instances.
    ///
    /// Each call to [`create_context()`](Self::create_context) invokes the
    /// factory to produce a fresh instance.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::resource::LocalResource;
    /// # use polaris_system::param::ResMut;
    /// # use polaris_system::system;
    /// struct Memory { messages: Vec<String> }
    /// impl LocalResource for Memory {}
    ///
    /// impl Memory {
    ///     fn new() -> Self {
    ///         Self { messages: Vec::new() }
    ///     }
    /// }
    ///
    /// # let mut server = Server::new();
    /// server.register_local(Memory::new);
    ///
    /// // The local resource can later be used in a system.
    /// #[system]
    /// async fn my_system(mut memory: ResMut<Memory>) {
    ///     memory.messages.push("Hello".into());
    /// }
    /// ```
    pub fn register_local<R: LocalResource>(
        &mut self,
        factory: impl Fn() -> R + Send + Sync + 'static,
    ) {
        self.local_factories
            .insert(TypeId::of::<R>(), Arc::new(move || Box::new(factory())));
    }

    /// Returns true if a local resource factory for type `R` is registered.
    #[must_use]
    pub fn has_local<R: LocalResource>(&self) -> bool {
        self.local_factories.contains_key(&TypeId::of::<R>())
    }

    /// Creates an execution context with global resources and fresh local resources.
    /// The returned `SystemContext<'static>` is decoupled from the server's lifetime,
    /// allowing it to be stored in systems or plugins.
    ///
    /// The returned context:
    /// - Has read-only access to all global resources via `Res<T>`
    /// - Has mutable access to fresh local resource instances via `ResMut<T>`
    /// - Can create child contexts via [`SystemContext::child()`]
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::resource::{GlobalResource, LocalResource};
    /// # #[derive(Default)] struct Config;
    /// # impl GlobalResource for Config {}
    /// # struct Memory;
    /// # impl LocalResource for Memory {}
    /// # impl Memory { fn new() -> Self { Self } }
    /// # let mut server = Server::new();
    /// // Register resources
    /// server.insert_global(Config::default());
    /// server.register_local(Memory::new);
    ///
    /// // Create execution context
    /// let ctx = server.create_context();
    ///
    /// // Resources can be accessed from the context
    /// let config = ctx.get_resource::<Config>().unwrap();  // From global
    /// let mut memory = ctx.get_resource_mut::<Memory>().unwrap();  // Fresh local instance
    /// ```
    #[must_use]
    pub fn create_context(&self) -> SystemContext<'static> {
        // Create context with access to server's global resources
        let mut ctx = SystemContext::with_globals(Arc::clone(&self.global));

        // Instantiate local resources from factories
        for (type_id, factory) in &self.local_factories {
            let boxed = factory();
            ctx.insert_boxed(*type_id, boxed);
        }

        ctx
    }

    /// Returns a reference to the global resources container.
    #[must_use]
    pub fn global_resources(&self) -> &Resources {
        &self.global
    }

    /// Returns a clonable factory that creates fresh [`SystemContext`] instances.
    ///
    /// The factory captures the current global resources and local resource
    /// factories, enabling context creation outside of direct `Server` access
    /// (e.g., from HTTP handlers).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::resource::LocalResource;
    /// # struct Memory;
    /// # impl LocalResource for Memory {}
    /// # impl Memory { fn new() -> Self { Self } }
    /// # let mut server = Server::new();
    /// # server.register_local(Memory::new);
    /// let factory = server.context_factory();
    /// let ctx = factory.create_context();
    /// ```
    #[must_use]
    pub fn context_factory(&self) -> ContextFactory {
        let globals = match self.build_state {
            // During the ready() phase, return a deferred handle so we don't
            // bump the Arc ref count on `self.global` (which would break
            // Arc::get_mut in downstream insert_global calls).
            BuildState::Building => {
                ContextFactoryGlobals::Deferred(Arc::clone(&self.deferred_globals))
            }
            // Before or after finish(), clone the Arc directly.
            _ => ContextFactoryGlobals::Direct(Arc::clone(&self.global)),
        };
        ContextFactory {
            globals,
            local_factories: self
                .local_factories
                .iter()
                .map(|(k, v)| (*k, Arc::clone(v)))
                .collect(),
        }
    }

    /// Returns whether the server has been built (i.e., `finish()` has been called).
    #[must_use]
    pub fn is_built(&self) -> bool {
        self.build_state == BuildState::Built
    }

    // ─────────────────────────────────────────────────────────────────────────
    // API Access
    // ─────────────────────────────────────────────────────────────────────────

    /// Inserts an API into the server.
    ///
    /// APIs are build-time capability registries that plugins use for orchestration.
    /// APIs are accessed by plugins during the build/ready phases.
    ///
    /// If an API of this type already exists, it is replaced and the old value
    /// is returned.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::api::API;
    /// pub struct AgentAPI;
    /// impl API for AgentAPI {}
    /// # impl AgentAPI { fn new() -> Self { AgentAPI } }
    ///
    /// // In a plugin's build():
    /// # fn build_example(server: &mut Server) {
    /// server.insert_api(AgentAPI::new());
    /// # }
    /// ```
    pub fn insert_api<A: API>(&mut self, api: A) -> Option<A> {
        let type_id = TypeId::of::<A>();
        let boxed: BoxedAPI = Box::new(api);
        self.apis
            .insert(type_id, boxed)
            .and_then(|old| old.downcast::<A>().ok())
            .map(|b| *b)
    }

    /// Gets a reference to an API.
    ///
    /// Returns `None` if the API doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::api::API;
    /// # struct AgentAPI;
    /// # impl API for AgentAPI {}
    /// # impl AgentAPI { fn register(&self, _: &str, _: MyAgent) {} }
    /// # struct MyAgent;
    /// # impl MyAgent { fn new() -> Self { Self } }
    /// // In a plugin's ready():
    /// # fn ready_example(server: &mut Server) {
    /// let api = server.api::<AgentAPI>()
    ///     .expect("AgentAPI required");
    /// api.register("my-agent", MyAgent::new());
    /// # }
    /// ```
    #[must_use]
    pub fn api<A: API>(&self) -> Option<&A> {
        self.apis
            .get(&TypeId::of::<A>())
            .and_then(|boxed| boxed.downcast_ref::<A>())
    }

    /// Returns true if an API of type `A` exists.
    #[must_use]
    pub fn contains_api<A: API>(&self) -> bool {
        self.apis.contains_key(&TypeId::of::<A>())
    }

    /// Like [`api`](Self::api), but logs a `tracing::warn!` when the API
    /// isn't registered. Returns an owned clone wrapped in `Option`.
    ///
    /// Use this in `ready()` to capture an API handle that the caller treats
    /// as **required** for correct behavior, but where falling back to a
    /// default is safe (e.g., capturing optional instrumentation middleware).
    /// The warning makes the silent-fallback case visible without changing
    /// the runtime semantics.
    ///
    /// `purpose` is a short free-text reason ("graph span instrumentation",
    /// "decision-outcome hooks", …) that appears in the warning so the log
    /// line points at the caller, not just the missing type.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::api::API;
    /// # #[derive(Clone)]
    /// # struct MiddlewareAPI;
    /// # impl API for MiddlewareAPI {}
    /// # fn ready_example(server: &Server) {
    /// let middleware = server.expect_api::<MiddlewareAPI>("graph span instrumentation");
    /// // `middleware` is `None` *and* a warning was logged if no plugin
    /// // inserted MiddlewareAPI before this point.
    /// # let _ = middleware;
    /// # }
    /// ```
    #[must_use]
    pub fn expect_api<A: API + Clone>(&self, purpose: &'static str) -> Option<A> {
        match self.api::<A>() {
            Some(api) => Some(api.clone()),
            None => {
                tracing::warn!(
                    api = std::any::type_name::<A>(),
                    purpose,
                    "expect_api: required API not registered — caller will fall back to default behavior"
                );
                None
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Tick Methods
    // ─────────────────────────────────────────────────────────────────────────

    /// Triggers a tick for the given schedule type.
    ///
    /// Only plugins that declared interest in this schedule via
    /// [`Plugin::tick_schedules()`] will have their [`Plugin::update()`] called.
    /// Plugins are ticked in dependency order (same as build/ready).
    ///
    /// Typically called by Layer 2 (`polaris_agent`) in response to agent
    /// execution events (e.g. after an agent run or between conversation turns).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::plugin::Schedule;
    /// // Layer 2 defines schedule marker types:
    /// pub struct PostAgentRun;
    /// impl Schedule for PostAgentRun {}
    ///
    /// // Layer 2 executor triggers the tick:
    /// # let mut server = Server::new();
    /// server.tick::<PostAgentRun>();
    /// ```
    pub fn tick<S: Schedule + 'static>(&mut self) {
        self.tick_schedule(S::schedule_id());
    }

    /// Triggers a tick for the given schedule ID.
    ///
    /// Plugins are ticked in dependency order.
    /// This is the non-generic version of [`tick()`](Self::tick).
    pub fn tick_schedule(&mut self, schedule: ScheduleId) {
        let Some(plugin_indices) = self.schedule_registry.get(&schedule) else {
            return;
        };

        // Clone indices to avoid borrow conflict with &mut self passed to update()
        let indices: Vec<usize> = plugin_indices.clone();

        for idx in indices {
            let plugin_ptr =
                std::ptr::from_ref::<Box<dyn DynPlugin>>(&self.built_plugins[idx].plugin);
            // SAFETY: built_plugins cannot be modified during this loop:
            // - It's a private field, inaccessible to plugin code
            // - add_plugins() during update goes to pending_plugins (build_state is Built)
            // - finish() during update panics (build_state is not NotStarted)
            // The pointer remains valid throughout the loop.
            unsafe {
                (*plugin_ptr).update(self, schedule);
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Lifecycle Methods
    // ─────────────────────────────────────────────────────────────────────────

    /// Builds all plugins and prepares the server for execution.
    ///
    /// This method:
    /// 1. Auto-registers missing dependencies that have declared defaults
    /// 2. Validates all plugin dependencies exist
    /// 3. Topologically sorts plugins by dependencies
    /// 4. Calls `build()` on each plugin in order
    /// 5. Calls `ready()` on each plugin in order
    ///
    /// # Panics
    ///
    /// - If a plugin's dependency is not satisfied and no default is declared
    /// - If there is a circular dependency between plugins
    /// - If called more than once
    pub async fn finish(&mut self) {
        if self.build_state != BuildState::NotStarted {
            panic!("Server::finish() was already called. Cannot build twice.");
        }

        // Phase 0: Auto-register missing dependencies that declare a default.
        self.auto_register_default_dependencies();

        // Phase 1: Sort plugins by dependencies
        let sorted_plugins = self.sort_plugins_by_dependencies();

        // Phase 2: Build all plugins in sorted order
        self.build_state = BuildState::Building;
        for entry in sorted_plugins {
            entry.plugin.build(self);
            self.built_plugins.push(entry);
        }

        // Phase 3: Ready all plugins in sorted order
        // We need to iterate by index since ready() takes &mut Server
        for i in 0..self.built_plugins.len() {
            // SAFETY: We're using index-based access to avoid borrow conflicts
            // The plugin is borrowed immutably, and we pass &mut self to ready()
            let plugin_ptr =
                std::ptr::from_ref::<Box<dyn DynPlugin>>(&self.built_plugins[i].plugin);
            // SAFETY: We don't modify built_plugins during this loop, and the
            // pointer remains valid. The plugin's ready() may add resources but
            // shouldn't modify built_plugins.
            unsafe {
                (*plugin_ptr).ready(self).await;
            }
        }

        // Phase 4: Build schedule registry from plugin tick_schedules()
        self.build_schedule_registry();

        // Phase 5: Bind deferred globals so any ContextFactory created during
        // ready() can now resolve.
        let _ = self.deferred_globals.set(Arc::clone(&self.global));

        self.build_state = BuildState::Built;
    }

    /// Builds the schedule registry from plugin `tick_schedules()` declarations.
    ///
    /// Called at the end of `finish()`. Maps each schedule to the indices of
    /// plugins that registered for it, preserving dependency order.
    fn build_schedule_registry(&mut self) {
        self.schedule_registry.clear();

        // Iterate in dependency order (built_plugins is already sorted)
        for (idx, entry) in self.built_plugins.iter().enumerate() {
            for schedule in entry.plugin.tick_schedules() {
                self.schedule_registry
                    .entry(schedule)
                    .or_default()
                    .push(idx);
            }
        }
    }

    /// Runs the server lifecycle.
    ///
    /// This is a convenience method that calls `finish()` and then returns.
    /// The full run loop with `update()` calls will be added in Layer 2.
    ///
    /// # Panics
    ///
    /// Same as [`finish()`](Self::finish).
    pub async fn run(&mut self) {
        self.finish().await;
        // Run loop will be added in Layer 2
    }

    /// Runs build and ready phases, then returns.
    ///
    /// This is an alias for `finish()`, intended for testing.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::server::Server;
    /// # use polaris_system::plugin::{Plugin, Version};
    /// # use polaris_system::resource::Resource;
    /// # struct MyResource;
    /// # struct MyPlugin;
    /// # impl Plugin for MyPlugin {
    /// #     const ID: &'static str = "my";
    /// #     const VERSION: Version = Version::new(0,0,1);
    /// #     fn build(&self, server: &mut Server) {
    /// #         server.insert_resource(MyResource);
    /// #     }
    /// # }
    /// # tokio_test::block_on(async {
    /// let mut server = Server::new();
    /// server.add_plugins(MyPlugin);
    /// server.run_once().await;
    ///
    /// assert!(server.contains_resource::<MyResource>());
    /// # });
    /// ```
    pub async fn run_once(&mut self) {
        self.finish().await;
    }

    /// Cleans up all plugins in reverse dependency order.
    ///
    /// Call this when shutting down the server to allow plugins to
    /// gracefully release resources.
    pub async fn cleanup(&mut self) {
        // Cleanup in reverse order (dependents before dependencies)
        for i in (0..self.built_plugins.len()).rev() {
            let plugin_ptr =
                std::ptr::from_ref::<Box<dyn DynPlugin>>(&self.built_plugins[i].plugin);
            // SAFETY: Same as ready() - we don't modify built_plugins during cleanup
            unsafe {
                (*plugin_ptr).cleanup(self).await;
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Internal: Dependency Resolution
    // ─────────────────────────────────────────────────────────────────────────

    /// Walks pending plugins' `default_dependencies()` and inserts any whose
    /// `PluginId` is neither pending nor already built.
    ///
    /// Iterates to a fixed point so that an auto-registered default that itself
    /// declares default dependencies also gets satisfied. If two plugins offer
    /// a default for the same `PluginId`, the first one encountered wins.
    fn auto_register_default_dependencies(&mut self) {
        // Pre-collect each pending plugin's default offerings into a single map
        // keyed by PluginId. Doing this once up front avoids re-invoking
        // `default_dependencies()` repeatedly during the fixed-point loop.
        let mut offered: HashMap<PluginId, BoxedDefault> = HashMap::new();
        for entry in &self.pending_plugins {
            for boxed in entry.plugin.default_dependencies().plugins {
                // First offer wins.
                offered.entry(boxed.id.clone()).or_insert(BoxedDefault {
                    provider: entry.id.clone(),
                    plugin: boxed.plugin,
                });
            }
        }

        // Each loop iteration scans every still-pending plugin for unsatisfied
        // dependencies and consumes a default if available. New auto-registered
        // plugins go to `pending_plugins`, so the next iteration sees their
        // dependencies and can resolve them too.
        loop {
            let mut to_add: Vec<(PluginId, PluginId, BoxedDefault)> = Vec::new();
            for entry in &self.pending_plugins {
                for dep_id in entry.plugin.dependencies() {
                    if self.plugin_ids.contains(&dep_id) {
                        continue;
                    }
                    // The dep is missing — register the default if we have one,
                    // taking it out of the `offered` map so we don't add twice.
                    if let Some(default) = offered.remove(&dep_id) {
                        to_add.push((dep_id, entry.id.clone(), default));
                    }
                }
            }
            if to_add.is_empty() {
                break;
            }
            for (dep_id, requirer, default) in to_add {
                tracing::info!(
                    plugin = %dep_id,
                    requirer = %requirer,
                    provider = %default.provider,
                    "auto-registering default plugin",
                );
                // Pull the new plugin's own defaults into the offered map so
                // they're visible to the next iteration's pass over pending.
                for boxed in default.plugin.default_dependencies().plugins {
                    offered.entry(boxed.id.clone()).or_insert(BoxedDefault {
                        provider: dep_id.clone(),
                        plugin: boxed.plugin,
                    });
                }
                self.plugin_ids.insert(dep_id.clone());
                self.pending_plugins.push(PluginEntry {
                    id: dep_id,
                    plugin: default.plugin,
                });
            }
        }
    }

    /// Sorts pending plugins by dependencies using topological sort.
    ///
    /// Returns the sorted list of plugins.
    ///
    /// # Panics
    ///
    /// - If one or more plugin dependencies are not satisfied. The panic message
    ///   lists every missing dependency together with the plugins that required it.
    /// - If there is a circular dependency
    fn sort_plugins_by_dependencies(&mut self) -> Vec<PluginEntry> {
        if self.pending_plugins.is_empty() {
            return Vec::new();
        }

        // Build a map of plugin id -> index for dependency lookup
        let mut id_to_index: HashMap<PluginId, usize> = HashMap::new();
        for (i, entry) in self.pending_plugins.iter().enumerate() {
            id_to_index.insert(entry.id.clone(), i);
        }

        // Build adjacency list and compute in-degrees
        let n = self.pending_plugins.len();
        let mut in_degree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

        // missing_id -> requirers (preserving insertion order for stable output)
        let mut missing: Vec<(PluginId, Vec<PluginId>)> = Vec::new();

        for (i, entry) in self.pending_plugins.iter().enumerate() {
            for dep_id in entry.plugin.dependencies() {
                // Find the dependency in pending plugins
                if let Some(&dep_idx) = id_to_index.get(&dep_id) {
                    // dep_idx must be built before i
                    dependents[dep_idx].push(i);
                    in_degree[i] += 1;
                } else if !self.built_plugins.iter().any(|p| p.id == dep_id) {
                    // Missing — record it; aggregate before panicking so the
                    // user sees every problem at once.
                    if let Some(slot) = missing.iter_mut().find(|(id, _)| *id == dep_id) {
                        slot.1.push(entry.id.clone());
                    } else {
                        missing.push((dep_id, vec![entry.id.clone()]));
                    }
                }
                // else: already built, no edge needed
            }
        }

        if !missing.is_empty() {
            panic!("{}", format_missing_dependencies(&missing));
        }

        // Kahn's algorithm for topological sort
        let mut queue: Vec<usize> = Vec::new();
        for (i, &deg) in in_degree.iter().enumerate() {
            if deg == 0 {
                queue.push(i);
            }
        }

        let mut sorted_indices: Vec<usize> = Vec::with_capacity(n);

        while let Some(idx) = queue.pop() {
            sorted_indices.push(idx);

            for &dependent_idx in &dependents[idx] {
                in_degree[dependent_idx] -= 1;
                if in_degree[dependent_idx] == 0 {
                    queue.push(dependent_idx);
                }
            }
        }

        // Check for cycle
        if sorted_indices.len() != n {
            // Find plugins involved in cycle
            let in_cycle: Vec<String> = in_degree
                .iter()
                .enumerate()
                .filter(|(_, deg)| **deg > 0)
                .map(|(i, _)| self.pending_plugins[i].id.to_string())
                .collect();

            panic!(
                "Circular dependency detected among plugins: {:?}\n\
                 Break the cycle by extracting shared functionality into a separate plugin.",
                in_cycle
            );
        }

        // Extract plugins in sorted order
        // We need to drain pending_plugins while preserving order
        let mut pending = std::mem::take(&mut self.pending_plugins);

        // Create a mapping from old index to new position
        let mut old_to_new: Vec<Option<usize>> = vec![None; n];
        for (new_pos, &old_idx) in sorted_indices.iter().enumerate() {
            old_to_new[old_idx] = Some(new_pos);
        }

        // Sort pending by the new order
        // We'll collect into a vec of Options, then unwrap
        let mut result: Vec<Option<PluginEntry>> = (0..n).map(|_| None).collect();
        for (old_idx, entry) in pending.drain(..).enumerate() {
            let new_pos = old_to_new[old_idx].expect("all indices should be mapped");
            result[new_pos] = Some(entry);
        }

        result.into_iter().flatten().collect()
    }
}

/// Formats the panic message for one or more missing plugin dependencies.
///
/// Lists every missing dependency together with the plugins that required it,
/// so the user can resolve all of them in one pass rather than rebuilding to
/// surface each error individually.
fn format_missing_dependencies(missing: &[(PluginId, Vec<PluginId>)]) -> String {
    use std::fmt::Write as _;

    let plural = if missing.len() == 1 { "" } else { "s" };
    let mut msg = format!(
        "{} plugin dependenc{} not satisfied:\n",
        missing.len(),
        if missing.len() == 1 { "y" } else { "ies" },
    );
    for (dep, requirers) in missing {
        let requirers_str = requirers
            .iter()
            .map(PluginId::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(msg, "  - '{dep}' required by: {requirers_str}");
    }
    msg.push_str(
        "\nFix: add the missing plugin{plural} before calling Server::finish() / Server::run(),\n\
         either directly via add_plugins(...) or via a plugin group such as DefaultPlugins.\n\
         Alternatively, declare a Default for the missing plugin and offer it from one of the\n\
         dependent plugins via Plugin::default_dependencies() for auto-registration.",
    );
    // Replace the literal `{plural}` placeholder (the str above is not a format
    // string). Done this way so the body reads naturally and we only pluralize
    // once.
    msg.replace("{plural}", plural)
}

// ─────────────────────────────────────────────────────────────────────────────
// ContextFactory
// ─────────────────────────────────────────────────────────────────────────────

/// Internal strategy for how a [`ContextFactory`] resolves global resources.
#[derive(Clone)]
enum ContextFactoryGlobals {
    /// Direct reference — used before `finish()` starts or after it completes.
    Direct(Arc<Resources>),
    /// Deferred binding — used during the `ready()` phase so that no extra
    /// `Arc` reference count is held on the server's global resource map
    /// (which would block [`Server::insert_global`] via `Arc::get_mut`).
    /// The [`OnceLock`] is filled at the end of [`Server::finish()`].
    Deferred(Arc<OnceLock<Arc<Resources>>>),
}

/// A clonable factory that creates fresh [`SystemContext`] instances.
///
/// Captures the server's global resources and local resource factories,
/// enabling context creation outside of direct [`Server`] access (e.g.,
/// from HTTP handlers running on background tasks).
///
/// Created via [`Server::context_factory()`]. Safe to call during the
/// plugin `ready()` phase — the factory uses deferred binding internally
/// so it does not block [`Server::insert_global()`] in downstream plugins.
///
/// # Example
///
/// ```
/// # use polaris_system::server::Server;
/// # use polaris_system::resource::LocalResource;
/// # struct Memory;
/// # impl LocalResource for Memory {}
/// # impl Memory { fn new() -> Self { Self } }
/// # let mut server = Server::new();
/// # server.register_local(Memory::new);
/// let factory = server.context_factory();
///
/// // Move factory to another thread, create contexts freely
/// let ctx = factory.create_context();
/// ```
#[derive(Clone)]
pub struct ContextFactory {
    globals: ContextFactoryGlobals,
    local_factories: Vec<(TypeId, Arc<dyn Fn() -> BoxedResource + Send + Sync>)>,
}

impl std::fmt::Debug for ContextFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextFactory")
            .field("local_factory_count", &self.local_factories.len())
            .finish_non_exhaustive()
    }
}

impl ContextFactory {
    /// Creates a new [`SystemContext`] with global resources and fresh local
    /// resource instances.
    ///
    /// Equivalent to [`Server::create_context()`] but does not require a
    /// `&Server` reference.
    ///
    /// # Panics
    ///
    /// Panics if the factory was created during the `ready()` phase and
    /// [`Server::finish()`] has not yet completed.
    #[must_use]
    pub fn create_context(&self) -> SystemContext<'static> {
        let globals = match &self.globals {
            ContextFactoryGlobals::Direct(arc) => Arc::clone(arc),
            ContextFactoryGlobals::Deferred(once) => Arc::clone(
                once.get()
                    .expect("cannot create context: Server::finish() has not completed"),
            ),
        };
        let mut ctx = SystemContext::with_globals(globals);
        for (type_id, factory) in &self.local_factories {
            ctx.insert_boxed(*type_id, factory());
        }
        ctx
    }
}
