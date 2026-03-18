//! Tracing plugin and subscriber infrastructure.
//!
//! [`TracingPlugin`] registers a `tracing` subscriber. By default no output
//! layers are included — call [`TracingPlugin::with_fmt`] to add console
//! output, or use [`DefaultPlugins`](crate::DefaultPlugins) which enables
//! fmt automatically.
//!
//! Other plugins can register additional layers (e.g., `OTel` export) via
//! [`TracingLayersApi`] during their `build()` phase. The subscriber is
//! installed once in [`TracingPlugin::ready()`] with all accumulated layers.

mod fmt_layer;
pub use fmt_layer::FmtConfig;

use crate::ServerInfoPlugin;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// ─────────────────────────────────────────────────────────────────────────────
// TracingLayersApi
// ─────────────────────────────────────────────────────────────────────────────

/// Build-time resource that accumulates tracing layers.
///
/// Created by [`TracingPlugin`] during `build()`. Other plugins that depend
/// on `TracingPlugin` can push additional layers during their own `build()`
/// phase. The subscriber is installed with all accumulated layers when
/// `TracingPlugin::ready()` runs.
///
/// # Example
///
/// Access this resource from another plugin's `build()` method to register
/// custom tracing layers:
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_core_plugins::{ServerInfoPlugin, TracingLayersApi, TracingPlugin};
/// use tracing_subscriber::fmt;
///
/// struct MyPlugin;
///
/// impl Plugin for MyPlugin {
///     const ID: &'static str = "my_plugin";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<TracingPlugin>()]
///     }
///
///     fn build(&self, server: &mut Server) {
///         let mut api = server
///             .get_resource_mut::<TracingLayersApi>()
///             .expect("TracingPlugin must be added first");
///         api.push(fmt::layer().compact());
///     }
/// }
///
/// Server::new()
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(MyPlugin)
///     .run_once();
/// ```
pub struct TracingLayersApi {
    layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>>,
}

impl TracingLayersApi {
    fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Pushes a tracing layer into the shared subscriber.
    pub fn push<L>(&mut self, layer: L)
    where
        L: Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
    {
        self.layers.push(Box::new(layer));
    }

    fn install(self) -> Result<(), tracing_subscriber::util::TryInitError> {
        tracing_subscriber::registry().with(self.layers).try_init()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TracingFormat
// ─────────────────────────────────────────────────────────────────────────────

/// Tracing output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TracingFormat {
    /// Human-readable colored output (default).
    #[default]
    Pretty,
    /// Compact single-line output.
    Compact,
    /// JSON structured output for log aggregation.
    Json,
}

// ─────────────────────────────────────────────────────────────────────────────
// TracingConfig Resource
// ─────────────────────────────────────────────────────────────────────────────

/// Tracing configuration resource.
///
/// Global resource exposing the tracing configuration. Systems can read
/// this to adapt their logging behavior based on the configured log level.
///
/// # Example
///
/// ```
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_core_plugins::TracingConfig;
/// use tracing::Level;
///
/// #[system]
/// async fn adaptive_logging(config: Res<TracingConfig>) {
///     tracing::info!("Processing request");
///
///     if config.level <= Level::DEBUG {
///         tracing::debug!("Debug mode active");
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TracingConfig {
    /// The configured log level.
    pub level: Level,
}

impl GlobalResource for TracingConfig {}

// ─────────────────────────────────────────────────────────────────────────────
// TracingPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Tracing subscriber infrastructure.
///
/// Registers a [`TracingLayersApi`] during `build()` so other plugins can
/// push additional layers (e.g., `OTel` export). The subscriber is installed
/// once in [`TracingPlugin::ready()`] with all accumulated layers.
///
/// No output layers are included by default. Call [`with_fmt`](Self::with_fmt)
/// to add console output, or use [`DefaultPlugins`](crate::DefaultPlugins)
/// which enables it automatically.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`TracingConfig`] | Global | Tracing configuration (read-only) |
/// | [`TracingLayersApi`] | Build-time | Layer registration for other plugins |
///
/// # Dependencies
///
/// - [`ServerInfoPlugin`]
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{ServerInfoPlugin, TracingPlugin, FmtConfig, TracingFormat};
/// use tracing::Level;
///
/// Server::new()
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(
///         TracingPlugin::default()
///             .with_level(Level::DEBUG)
///             .with_fmt(FmtConfig::default().format(TracingFormat::Json))
///     )
///     .run();
/// ```
#[derive(Clone)]
pub struct TracingPlugin {
    /// Maximum log level.
    level: Level,
    /// Fmt console output configuration. `None` disables fmt output.
    fmt: Option<FmtConfig>,
}

impl Default for TracingPlugin {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            fmt: None,
        }
    }
}

impl TracingPlugin {
    /// Creates a new `TracingPlugin` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum log level.
    #[must_use]
    pub fn with_level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Enables fmt console output with the given configuration.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_core_plugins::{TracingPlugin, FmtConfig, TracingFormat};
    ///
    /// TracingPlugin::default()
    ///     .with_fmt(
    ///         FmtConfig::default()
    ///             .format(TracingFormat::Json)
    ///             .env_filter("polaris=debug,hyper=warn")
    ///     );
    /// ```
    #[must_use]
    pub fn with_fmt(mut self, config: FmtConfig) -> Self {
        self.fmt = Some(config);
        self
    }
}

impl Plugin for TracingPlugin {
    const ID: &'static str = "polaris::tracing";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_global(TracingConfig { level: self.level });

        server.insert_resource(TracingLayersApi::new());

        if let Some(fmt) = &self.fmt {
            let mut api = server
                .get_resource_mut::<TracingLayersApi>()
                .expect("TracingLayersApi must exist after insert");

            fmt_layer::push_layer(&mut api, fmt, self.level);
        }
    }

    fn ready(&self, server: &mut Server) {
        if let Some(api) = server.remove_resource::<TracingLayersApi>() {
            api.install()
                .expect("a global tracing subscriber is already set");
        }

        tracing::info!(
            level = %self.level,
            fmt = self.fmt.is_some(),
            "TracingPlugin initialized"
        );
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ServerInfoPlugin>()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::server::Server;

    #[test]
    fn build_registers_config_and_layers_api() {
        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);
        server.add_plugins(TracingPlugin::default());
        server.finish();

        let ctx = server.create_context();
        assert!(
            ctx.contains_resource::<TracingConfig>(),
            "should register TracingConfig global"
        );
    }

    #[test]
    fn build_creates_layers_api() {
        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);

        let plugin = TracingPlugin::default();
        plugin.build(&mut server);

        assert!(
            server.contains_resource::<TracingLayersApi>(),
            "should create TracingLayersApi for layer accumulation"
        );
    }
}
