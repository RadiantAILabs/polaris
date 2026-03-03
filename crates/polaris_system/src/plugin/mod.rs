//! Plugin system for extensible server functionality.
//!
//! Plugins are the fundamental unit of composition in Polaris. In Polaris, every piece of
//! functionality, from core infrastructure to agent-specific features, is
//! delivered through plugins.
//!
//! # Example
//!
//! ```
//! use polaris_system::plugin::{Plugin, PluginId, Version};
//! use polaris_system::server::Server;
//!
//! struct MyPlugin {
//!     config: String,
//! }
//!
//! # struct TracingPlugin;
//! # impl Plugin for TracingPlugin {
//! #     const ID: &'static str = "polaris::tracing";
//! #     const VERSION: Version = Version::new(0, 0, 1);
//! #     fn build(&self, _server: &mut Server) {}
//! # }
//!
//! # struct MyConfig {
//! #    value: String,
//! # }
//!
//! impl Plugin for MyPlugin {
//!     const ID: &'static str = "polaris::my_plugin";
//!     const VERSION: Version = Version::new(0, 0, 1);
//!
//!     fn build(&self, server: &mut Server) {
//!         server.insert_resource(MyConfig {
//!             value: self.config.clone(),
//!         });
//!     }
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<TracingPlugin>()]
//!     }
//! }
//!
//! Server::new()
//!     .add_plugins(TracingPlugin)
//!     .add_plugins(MyPlugin { config: "test".into() })
//!     .run();
//! ```

mod schedule;

use crate::server::Server;
pub use schedule::{IntoScheduleIds, Schedule, ScheduleId};

// ─────────────────────────────────────────────────────────────────────────────
// Version
// ─────────────────────────────────────────────────────────────────────────────

/// Semantic version identifier.
///
/// # Example
///
/// ```
/// use polaris_system::plugin::Version;
///
/// let v = Version::new(1, 2, 3);
/// assert_eq!(v.to_string(), "1.2.3");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Version {
    /// Breaking changes.
    pub major: u64,
    /// Backwards-compatible additions.
    pub minor: u64,
    /// Backwards-compatible bug fixes.
    pub patch: u64,
}

impl Version {
    /// Creates a new [Version].
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PluginId
// ─────────────────────────────────────────────────────────────────────────────

/// Stable and unique developer-provided identifier for a plugin.
///
/// Used for dependency resolution and duplicate detection.
///
/// # Example
///
/// ```
/// use polaris_system::plugin::PluginId;
///
/// let id = PluginId::new("polaris::server_info");
/// assert_eq!(id.to_string(), "polaris::server_info");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(&'static str);

impl PluginId {
    /// Creates a `PluginId` from a string identifier.
    #[must_use]
    pub fn new(id: &'static str) -> Self {
        Self(id)
    }

    /// Returns the [`PluginId`] for a plugin type using its [`Plugin::ID`] constant.
    ///
    /// This is often used for type-safe dependency declarations.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::plugin::{Plugin, PluginId, Version};
    /// # use polaris_system::server::Server;
    /// # struct TracingPlugin;
    /// # impl Plugin for TracingPlugin {
    /// #     const ID: &'static str = "tracing";
    /// #     const VERSION: Version = Version::new(0,0,1);
    /// #     fn build(&self, _: &mut Server) {}
    /// # }
    ///
    /// PluginId::of::<TracingPlugin>();
    /// ```
    #[must_use]
    pub fn of<P: Plugin>() -> Self {
        Self::new(P::ID)
    }
}

impl From<&'static str> for PluginId {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

impl core::fmt::Display for PluginId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Plugin Trait
// ─────────────────────────────────────────────────────────────────────────────

/// A collection of resources and sub-plugins that extend server functionality.
///
/// Plugins follow a strict lifecycle managed by the server:
///
/// 1. **Build Phase** - `build()` is called in dependency order
/// 2. **Ready Phase** - `ready()` is called in dependency order
/// 3. **Tick Phase** - `update()` is called when schedules are triggered by Layer 2
/// 4. **Cleanup Phase** - `cleanup()` is called in reverse dependency order
///
/// # Example
///
/// ```
/// # use polaris_system::plugin::{Plugin, PluginId, Version};
/// # use polaris_system::server::Server;
/// # use polaris_system::resource::{Resource, GlobalResource};
/// # use std::time::Duration;
/// # struct MetricsCollector;
/// # impl MetricsCollector {
/// #     fn new(_: Duration) -> Self { Self }
/// #     fn flush(&self) {}
/// # }
/// # struct GlobalConfig;
/// # impl GlobalResource for GlobalConfig {}
/// # struct TracingPlugin;
/// # impl Plugin for TracingPlugin {
/// #     const ID: &'static str = "tracing";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// pub struct MetricsPlugin {
///     pub collect_interval: Duration,
/// }
///
/// impl Plugin for MetricsPlugin {
///     const ID: &'static str = "polaris::metrics";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn build(&self, server: &mut Server) {
///         server.insert_resource(MetricsCollector::new(self.collect_interval));
///     }
///
///     fn ready(&self, server: &mut Server) {
///         // Validate required resources exist
///         let _config = server.get_resource::<GlobalConfig>()
///             .expect("MetricsPlugin requires GlobalConfig");
///     }
///
///     fn cleanup(&self, server: &mut Server) {
///         // Flush any buffered metrics
///         if let Some(collector) = server.get_resource::<MetricsCollector>() {
///             collector.flush();
///         }
///     }
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<TracingPlugin>()]
///     }
/// }
/// ```
pub trait Plugin: Send + Sync + 'static {
    /// Stable, unique identifier for this plugin type.
    const ID: &'static str;

    /// Semantic version of this plugin.
    const VERSION: Version;

    /// Configures the server. Called once when the plugin is added.
    ///
    /// Use this to:
    /// - Register resources with initial values
    /// - Add sub-plugins via `server.add_plugins()`
    ///
    /// # Note
    ///
    /// Keep `build()` lightweight. Heavy initialization should be deferred
    /// to `ready()` or done in systems.
    fn build(&self, server: &mut Server);

    /// Called after all plugins have been built and the server is ready to run.
    ///
    /// Use this for:
    /// - Validation that required resources exist
    /// - One-time initialization that depends on other plugins
    /// - Establishing connections (databases, APIs, etc.)
    fn ready(&self, _server: &mut Server) {}

    /// Called when a schedule this plugin registered for is triggered.
    ///
    /// The `schedule` parameter indicates which schedule triggered this update,
    /// allowing plugins to handle different schedules differently. Only called
    /// if the plugin declared interest via [`tick_schedules()`](Self::tick_schedules).
    ///
    /// Most logic should live in systems, not here. Reserve `update()` for
    /// plugin-level housekeeping (buffer flushing, health checks, etc.).
    fn update(&self, _server: &mut Server, _schedule: ScheduleId) {}

    /// Called when the server is shutting down.
    ///
    /// Use this for:
    /// - Graceful connection termination
    /// - Flushing buffers
    /// - Cleanup of external resources
    ///
    /// Called in **reverse** dependency order (dependents cleanup before dependencies).
    fn cleanup(&self, _server: &mut Server) {}

    /// Declares which schedules this plugin wants to receive updates on.
    ///
    /// Schedules are marker types defined by Layer 2 (e.g., `PostAgentRun`, `PreTurn`).
    /// Return an empty vec to receive no scheduled updates (default).
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::plugin::{Plugin, Schedule, ScheduleId, Version};
    /// # use polaris_system::server::Server;
    /// # pub struct PostAgentRun;
    /// # impl Schedule for PostAgentRun {}
    /// # pub struct PreTurn;
    /// # impl Schedule for PreTurn {}
    /// struct MetricsPlugin;
    ///
    /// impl Plugin for MetricsPlugin {
    ///     const ID: &'static str = "metrics";
    ///     const VERSION: Version = Version::new(0, 0, 1);
    ///
    ///     fn build(&self, _: &mut Server) {}
    ///
    ///     fn tick_schedules(&self) -> Vec<ScheduleId> {
    ///         vec![
    ///             ScheduleId::of::<PostAgentRun>(),
    ///             ScheduleId::of::<PreTurn>(),
    ///         ]
    ///     }
    ///
    ///     fn update(&self, _server: &mut Server, schedule: ScheduleId) {
    ///         // Called when PostAgentRun or PreTurn schedules trigger
    ///     }
    /// }
    /// ```
    fn tick_schedules(&self) -> Vec<ScheduleId> {
        Vec::new()
    }

    /// Declares plugins that must be added before this one.
    ///
    /// The server will panic if dependencies are not satisfied when `run()` is called.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::plugin::{Plugin, PluginId, Version};
    /// # use polaris_system::server::Server;
    /// struct TracingPlugin;
    ///
    /// impl Plugin for TracingPlugin {
    ///     const ID: &'static str = "tracing";
    ///     const VERSION: Version = Version::new(0, 0, 1);
    ///     fn build(&self, _: &mut Server) {}
    /// }
    ///
    /// struct IOPlugin;
    ///
    /// impl Plugin for IOPlugin {
    ///     const ID: &'static str = "io";
    ///     const VERSION: Version = Version::new(0, 0, 1);
    ///     fn build(&self, _: &mut Server) {}
    ///
    ///     fn dependencies(&self) -> Vec<PluginId> {
    ///         vec![PluginId::of::<TracingPlugin>()]
    ///     }
    /// }
    /// ```
    fn dependencies(&self) -> Vec<PluginId> {
        Vec::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DynPlugin Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Dyn-compatible counterpart of [`Plugin`].
///
/// The [`Plugin`] trait has associated constants (`ID`, `VERSION`) which make it
/// non-dyn-compatible. This trait mirrors the same methods but uses regular
/// methods instead, making it safe to use as `Box<dyn DynPlugin>`.
///
/// A blanket impl implements [`DynPlugin`] for every [`Plugin`], so plugin authors
/// only ever implement [`Plugin`].
pub(crate) trait DynPlugin: Send + Sync + 'static {
    /// Returns this plugin's [`PluginId`].
    fn id(&self) -> PluginId;
    /// Returns this plugin's [`Version`].
    #[expect(dead_code, reason = "reserved for future use by server introspection")]
    fn version(&self) -> Version;
    /// See [`Plugin::build`].
    fn build(&self, server: &mut Server);
    /// See [`Plugin::ready`].
    fn ready(&self, server: &mut Server);
    /// See [`Plugin::update`].
    fn update(&self, server: &mut Server, schedule: ScheduleId);
    /// See [`Plugin::cleanup`].
    fn cleanup(&self, server: &mut Server);
    /// See [`Plugin::tick_schedules`].
    fn tick_schedules(&self) -> Vec<ScheduleId>;
    /// See [`Plugin::dependencies`].
    fn dependencies(&self) -> Vec<PluginId>;
}

impl<T: Plugin> DynPlugin for T {
    fn id(&self) -> PluginId {
        PluginId::new(T::ID)
    }
    fn version(&self) -> Version {
        T::VERSION
    }
    fn build(&self, server: &mut Server) {
        Plugin::build(self, server);
    }
    fn ready(&self, server: &mut Server) {
        Plugin::ready(self, server);
    }
    fn update(&self, server: &mut Server, schedule: ScheduleId) {
        Plugin::update(self, server, schedule);
    }
    fn cleanup(&self, server: &mut Server) {
        Plugin::cleanup(self, server);
    }
    fn tick_schedules(&self) -> Vec<ScheduleId> {
        Plugin::tick_schedules(self)
    }
    fn dependencies(&self) -> Vec<PluginId> {
        Plugin::dependencies(self)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Plugins Trait (for add_plugins polymorphism)
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for types that can be added to a server as plugins.
///
/// This trait enables `server.add_plugins()` to accept both:
/// - Single plugins implementing [`Plugin`]
/// - Plugin groups via [`PluginGroupBuilder`]
///
/// Users typically don't implement this trait directly.
pub trait Plugins {
    /// Adds these plugins to the server.
    fn add_to_server(self, server: &mut Server);
}

/// Single plugins implement `Plugins` directly.
impl<P: Plugin> Plugins for P {
    fn add_to_server(self, server: &mut Server) {
        server.add_plugin_boxed(Box::new(self));
    }
}

/// `PluginGroupBuilder` implements `Plugins` to add all contained plugins.
impl Plugins for PluginGroupBuilder {
    fn add_to_server(self, server: &mut Server) {
        for boxed in self.plugins {
            server.add_plugin_boxed(boxed.plugin);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PluginGroup Trait
// ─────────────────────────────────────────────────────────────────────────────

/// A collection of plugins that can be added together.
///
/// Plugin groups provide a convenient way to bundle related plugins.
/// Users can customize the group before adding it to the server.
///
/// # Example
///
/// ```
/// # use polaris_system::plugin::{Plugin, PluginGroup, PluginGroupBuilder, Version};
/// # use polaris_system::server::Server;
/// # struct CorePlugin;
/// # impl Plugin for CorePlugin {
/// #     const ID: &'static str = "core";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// # struct TracingPlugin;
/// # impl TracingPlugin { fn default() -> Self { Self } }
/// # impl Plugin for TracingPlugin {
/// #     const ID: &'static str = "tracing";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// # struct IOPlugin;
/// # impl Plugin for IOPlugin {
/// #     const ID: &'static str = "io";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// pub struct DefaultPlugins;
///
/// impl PluginGroup for DefaultPlugins {
///     fn build(self) -> PluginGroupBuilder {
///         PluginGroupBuilder::new()
///             .add(CorePlugin)
///             .add(TracingPlugin::default())
///             .add(IOPlugin)
///     }
/// }
///
/// Server::new()
///     .add_plugins(
///         DefaultPlugins
///             .build()
///     )
///     .finish();
/// ```
pub trait PluginGroup {
    /// Returns the plugins in this group.
    fn build(self) -> PluginGroupBuilder;
}

// ─────────────────────────────────────────────────────────────────────────────
// BoxedPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// A boxed plugin with its captured [`PluginId`].
///
/// This struct preserves the plugin's type identity (via `PluginId`) after
/// boxing, enabling proper dependency resolution and duplicate detection.
pub(crate) struct BoxedPlugin {
    /// The plugin's unique identifier (captured before boxing).
    pub(crate) id: PluginId,
    /// The boxed plugin instance.
    pub(crate) plugin: Box<dyn DynPlugin>,
}

// ─────────────────────────────────────────────────────────────────────────────
// PluginGroupBuilder
// ─────────────────────────────────────────────────────────────────────────────

/// Builder for customizing plugin groups.
///
/// Allows adding, removing, and reordering plugins within a group.
///
/// # Example
///
/// ```
/// # use polaris_system::plugin::{Plugin, PluginGroup, PluginGroupBuilder, Version};
/// # use polaris_system::server::Server;
/// # struct TracingPlugin;
/// # impl Plugin for TracingPlugin {
/// #     const ID: &'static str = "tracing";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// # struct MetricsPlugin;
/// # impl Plugin for MetricsPlugin {
/// #     const ID: &'static str = "metrics";
/// #     const VERSION: Version = Version::new(0,0,1);
/// #     fn build(&self, _: &mut Server) {}
/// # }
/// # struct DefaultPlugins;
/// # impl PluginGroup for DefaultPlugins {
/// #     fn build(self) -> PluginGroupBuilder {
/// #         PluginGroupBuilder::new().add(TracingPlugin).add(MetricsPlugin)
/// #     }
/// # }
/// // Disable a plugin from a group
/// # let _ =
/// DefaultPlugins
///     .build()
///     .disable::<TracingPlugin>()
/// # ;
/// ```
#[derive(Default)]
pub struct PluginGroupBuilder {
    /// The plugins in this group, in order.
    pub(crate) plugins: Vec<BoxedPlugin>,
}

impl PluginGroupBuilder {
    /// Creates a new empty plugin group builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Adds a plugin to the group, or replaces it if one of the same type
    /// already exists (preserving its position).
    #[must_use]
    #[expect(
        clippy::should_implement_trait,
        reason = "This is a builder method, not std::ops::Add"
    )]
    pub fn add<P: Plugin>(mut self, plugin: P) -> Self {
        let id = PluginId::of::<P>();
        if let Some(pos) = self.plugins.iter().position(|p| p.id == id) {
            self.plugins[pos] = BoxedPlugin {
                id,
                plugin: Box::new(plugin),
            };
        } else {
            self.plugins.push(BoxedPlugin {
                id,
                plugin: Box::new(plugin),
            });
        }
        self
    }

    /// Adds a plugin before another plugin in the group.
    ///
    /// If `Target` is not found, the plugin is added at the beginning.
    ///
    /// # Type Parameters
    ///
    /// - `P`: The plugin to add
    /// - `Target`: The plugin to insert before
    #[must_use]
    pub fn add_before<P: Plugin, Target: Plugin>(mut self, plugin: P) -> Self {
        let target = PluginId::of::<Target>();
        let position = self
            .plugins
            .iter()
            .position(|p| p.id == target)
            .unwrap_or(0);
        let id = PluginId::of::<P>();
        self.plugins.insert(
            position,
            BoxedPlugin {
                id,
                plugin: Box::new(plugin),
            },
        );
        self
    }

    /// Adds a plugin after another plugin in the group.
    ///
    /// If `Target` is not found, the plugin is added at the end.
    ///
    /// # Type Parameters
    ///
    /// - `P`: The plugin to add
    /// - `Target`: The plugin to insert after
    #[must_use]
    pub fn add_after<P: Plugin, Target: Plugin>(mut self, plugin: P) -> Self {
        let target = PluginId::of::<Target>();
        let position = self
            .plugins
            .iter()
            .position(|p| p.id == target)
            .map(|i| i + 1)
            .unwrap_or(self.plugins.len());
        let id = PluginId::of::<P>();
        self.plugins.insert(
            position,
            BoxedPlugin {
                id,
                plugin: Box::new(plugin),
            },
        );
        self
    }

    /// Removes a plugin from the group by type.
    ///
    /// If the plugin is not found, this is a no-op.
    #[must_use]
    pub fn disable<P: Plugin>(mut self) -> Self {
        let target = PluginId::of::<P>();
        self.plugins.retain(|p| p.id != target);
        self
    }

    /// Returns the number of plugins in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Returns true if the group contains no plugins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test plugins
    struct PluginA;
    impl Plugin for PluginA {
        const ID: &'static str = "test::plugin_a";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
    }

    struct PluginB;
    impl Plugin for PluginB {
        const ID: &'static str = "test::plugin_b";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<PluginA>()]
        }
    }

    struct PluginC;
    impl Plugin for PluginC {
        const ID: &'static str = "test::plugin_c";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
    }

    #[test]
    fn plugin_id_equality() {
        let id1 = PluginId::of::<PluginA>();
        let id2 = PluginId::of::<PluginA>();
        let id3 = PluginId::of::<PluginB>();

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn plugin_id_display() {
        let id = PluginId::new("polaris::server_info");
        assert_eq!(id.to_string(), "polaris::server_info");
    }

    #[test]
    fn plugin_id_and_version() {
        assert_eq!(PluginId::of::<PluginA>(), PluginId::new("test::plugin_a"));
        assert_eq!(PluginA::VERSION, Version::new(0, 0, 1));
    }

    #[test]
    fn plugin_default_dependencies_empty() {
        assert!(Plugin::dependencies(&PluginA).is_empty());
    }

    #[test]
    fn plugin_default_tick_schedules_empty() {
        assert!(Plugin::tick_schedules(&PluginA).is_empty());
    }

    #[test]
    fn plugin_with_dependencies() {
        let deps = Plugin::dependencies(&PluginB);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], PluginId::of::<PluginA>());
    }

    #[test]
    fn plugin_group_builder_add() {
        let builder = PluginGroupBuilder::new().add(PluginA).add(PluginB);

        assert_eq!(builder.len(), 2);
    }

    #[test]
    fn plugin_group_builder_disable() {
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add(PluginB)
            .disable::<PluginA>();

        assert_eq!(builder.len(), 1);
        assert!(builder.plugins[0].id == PluginId::of::<PluginB>());
    }

    #[test]
    fn plugin_group_builder_add_before() {
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add(PluginB)
            .add_before::<_, PluginB>(PluginC);

        assert_eq!(builder.len(), 3);
        // Order: A, C, B
        assert!(builder.plugins[0].id == PluginId::of::<PluginA>());
        assert!(builder.plugins[1].id == PluginId::of::<PluginC>());
        assert!(builder.plugins[2].id == PluginId::of::<PluginB>());
    }

    #[test]
    fn plugin_group_builder_add_after() {
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add(PluginB)
            .add_after::<_, PluginA>(PluginC);

        assert_eq!(builder.len(), 3);
        // Order: A, C, B
        assert!(builder.plugins[0].id == PluginId::of::<PluginA>());
        assert!(builder.plugins[1].id == PluginId::of::<PluginC>());
        assert!(builder.plugins[2].id == PluginId::of::<PluginB>());
    }

    #[test]
    fn plugin_group_builder_add_before_not_found() {
        // When target not found, add at beginning
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add_before::<_, PluginB>(PluginC); // PluginB not in list

        assert_eq!(builder.len(), 2);
        // C added at beginning since B not found
        assert!(builder.plugins[0].id == PluginId::of::<PluginC>());
        assert!(builder.plugins[1].id == PluginId::of::<PluginA>());
    }

    #[test]
    fn plugin_group_builder_add_after_not_found() {
        // When target not found, add at end
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add_after::<_, PluginB>(PluginC); // PluginB not in list

        assert_eq!(builder.len(), 2);
        // C added at end since B not found
        assert!(builder.plugins[0].id == PluginId::of::<PluginA>());
        assert!(builder.plugins[1].id == PluginId::of::<PluginC>());
    }

    // Test PluginGroup trait
    struct TestPluginGroup;

    impl PluginGroup for TestPluginGroup {
        fn build(self) -> PluginGroupBuilder {
            PluginGroupBuilder::new().add(PluginA).add(PluginB)
        }
    }

    #[test]
    fn plugin_group_build() {
        let builder = TestPluginGroup.build();
        assert_eq!(builder.len(), 2);
    }

    #[test]
    fn plugin_group_empty() {
        let builder = PluginGroupBuilder::new();

        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
    }

    #[test]
    fn plugin_group_disable_all() {
        let builder = PluginGroupBuilder::new()
            .add(PluginA)
            .add(PluginB)
            .disable::<PluginA>()
            .disable::<PluginB>();

        assert!(builder.is_empty());
    }

    #[test]
    fn plugin_group_disable_nonexistent_is_noop() {
        let builder = PluginGroupBuilder::new().add(PluginA).disable::<PluginC>(); // PluginC not in list

        // Should still have PluginA
        assert_eq!(builder.len(), 1);
        assert!(builder.plugins[0].id == PluginId::of::<PluginA>());
    }

    #[test]
    fn plugin_group_builder_len_and_is_empty() {
        let empty = PluginGroupBuilder::new();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let with_one = PluginGroupBuilder::new().add(PluginA);
        assert!(!with_one.is_empty());
        assert_eq!(with_one.len(), 1);

        let with_two = PluginGroupBuilder::new().add(PluginA).add(PluginB);
        assert!(!with_two.is_empty());
        assert_eq!(with_two.len(), 2);
    }
}
