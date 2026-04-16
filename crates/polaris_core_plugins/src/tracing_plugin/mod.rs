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
#[cfg(feature = "models_tracing")]
mod genai_content;
#[cfg(feature = "graph_tracing")]
mod graph_middleware;
#[cfg(feature = "models_tracing")]
mod llm_decorator;
#[cfg(feature = "tools_tracing")]
mod tool_decorator;

use crate::ServerInfoPlugin;
#[cfg(feature = "models_tracing")]
use crate::tracing_plugin::llm_decorator::TracingLlmProvider;
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
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// # #[cfg(feature = "models_tracing")]
/// # server.add_plugins(polaris_models::ModelsPlugin);
/// # #[cfg(feature = "tools_tracing")]
/// # server.add_plugins(polaris_tools::ToolsPlugin);
/// server.add_plugins(TracingPlugin::new());
/// server.add_plugins(MyPlugin);
/// # tokio_test::block_on(async {
/// server.run_once().await;
/// # });
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
/// - [`ModelsPlugin`](polaris_models::ModelsPlugin) — when the `models_tracing` feature is enabled
/// - [`ToolsPlugin`](polaris_tools::ToolsPlugin) — when the `tools_tracing` feature is enabled
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{ServerInfoPlugin, TracingPlugin, FmtConfig, TracingFormat};
/// use tracing::Level;
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// # #[cfg(feature = "models_tracing")]
/// # server.add_plugins(polaris_models::ModelsPlugin);
/// # #[cfg(feature = "tools_tracing")]
/// # server.add_plugins(polaris_tools::ToolsPlugin);
/// server.add_plugins(
///     TracingPlugin::default()
///         .with_level(Level::DEBUG)
///         .with_fmt(FmtConfig::default().format(TracingFormat::Json))
/// );
/// # tokio_test::block_on(async {
/// server.run().await;
/// # });
/// ```
#[derive(Debug, Clone)]
pub struct TracingPlugin {
    /// Maximum log level.
    level: Level,
    /// Fmt console output configuration. `None` disables fmt output.
    fmt: Option<FmtConfig>,
    /// Whether to capture `GenAI` content attributes on instrumentation spans.
    capture_genai_content: bool,
}

impl Default for TracingPlugin {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            fmt: None,
            capture_genai_content: false,
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

    /// Enables recording of `GenAI` content attributes on instrumentation spans.
    ///
    /// When enabled, LLM `chat` spans include `gen_ai.input.messages`,
    /// `gen_ai.output.messages`, `gen_ai.system_instructions`, and
    /// `gen_ai.tool.definitions`. Tool `execute_tool` spans include
    /// `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result`.
    ///
    /// These attributes may be large and can contain sensitive data.
    /// Tool arguments and results are captured verbatim — a tool that
    /// returns or receives credentials (auth tokens, API keys), PII, or
    /// other secret material will have those values recorded on the span.
    /// Enable only when the configured trace backend is an appropriate
    /// destination for such data, or ensure tools scrub sensitive fields
    /// from their inputs and outputs before returning.
    ///
    /// Disabled by default.
    #[must_use]
    pub fn with_capture_genai_content(mut self) -> Self {
        self.capture_genai_content = true;
        self
    }
}

impl Plugin for TracingPlugin {
    const ID: &'static str = "polaris::tracing";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server.insert_global(TracingConfig { level: self.level });

        server.insert_resource(TracingLayersApi::new());

        if let Some(fmt) = &self.fmt {
            let mut api = server
                .get_resource_mut::<TracingLayersApi>()
                .expect("TracingLayersApi must exist after insert");

            fmt_layer::push_layer(&mut api, fmt, self.level);
        }

        #[cfg(feature = "graph_tracing")]
        self.register_instrumentation(server);
    }

    async fn ready(&self, server: &mut Server) {
        if let Some(api) = server.remove_resource::<TracingLayersApi>() {
            api.install()
                .expect("a global tracing subscriber is already set");
        }

        self.decorate_registries(server);

        tracing::info!(
            level = %self.level,
            fmt = self.fmt.is_some(),
            "TracingPlugin initialized"
        );
    }

    fn dependencies(&self) -> Vec<PluginId> {
        #[cfg_attr(
            not(any(feature = "models_tracing", feature = "tools_tracing")),
            expect(unused_mut, reason = "mutated only when tracing features are enabled")
        )]
        let mut deps = vec![PluginId::of::<ServerInfoPlugin>()];
        #[cfg(feature = "models_tracing")]
        deps.push(PluginId::of::<polaris_models::ModelsPlugin>());
        #[cfg(feature = "tools_tracing")]
        deps.push(PluginId::of::<polaris_tools::ToolsPlugin>());
        deps
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Instrumentation
// ─────────────────────────────────────────────────────────────────────────────

impl TracingPlugin {
    /// Registers graph middleware for span creation around node execution.
    #[cfg(feature = "graph_tracing")]
    fn register_instrumentation(&self, server: &mut Server) {
        if !server.contains_api::<polaris_graph::MiddlewareAPI>() {
            server.insert_api(polaris_graph::MiddlewareAPI::new());
        }

        let mw = server
            .api::<polaris_graph::MiddlewareAPI>()
            .expect("MiddlewareAPI should be present after initialization");
        graph_middleware::register(mw);
    }

    /// Wraps registered providers and tools with tracing decorators.
    fn decorate_registries(&self, _server: &mut Server) {
        #[cfg(feature = "models_tracing")]
        self.decorate_model_registry(_server);
        #[cfg(feature = "tools_tracing")]
        self.decorate_tool_registry(_server);
    }

    /// Rebuilds the [`ModelRegistry`] with tracing-decorated providers.
    #[cfg(feature = "models_tracing")]
    fn decorate_model_registry(&self, server: &mut Server) {
        let capture = self.capture_genai_content;
        let Some(old) = server.insert_global(polaris_models::ModelRegistry::default()) else {
            tracing::warn!("no ModelRegistry found — models_tracing decoration skipped");
            return;
        };

        let mut new = polaris_models::ModelRegistry::new();
        for name in old.llm_provider_names() {
            if let Some(provider) = old.llm_provider(&name) {
                new.register_llm_provider(TracingLlmProvider::new(provider, capture));
            }
        }

        server.insert_global(new);
    }

    /// Rebuilds the [`ToolRegistry`] with tracing-decorated tools.
    #[cfg(feature = "tools_tracing")]
    fn decorate_tool_registry(&self, server: &mut Server) {
        use tool_decorator::TracingTool;

        let capture = self.capture_genai_content;
        let Some(old) = server.insert_global(polaris_tools::ToolRegistry::default()) else {
            tracing::warn!("no ToolRegistry found — tools_tracing decoration skipped");
            return;
        };

        let mut new = polaris_tools::ToolRegistry::new();
        for name in old.names() {
            if let Some(tool) = old.to_arc(name) {
                new.register(TracingTool::new(tool, capture));
            }
        }

        for (name, permission) in old.permission_overrides() {
            // Tool is guaranteed to exist — we just re-registered all of them above.
            new.set_permission(name, *permission)
                .expect("decorated tool should exist in new registry");
        }

        server.insert_global(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::server::Server;

    #[tokio::test]
    async fn build_registers_config_and_layers_api() {
        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);
        #[cfg(feature = "models_tracing")]
        server.add_plugins(polaris_models::ModelsPlugin);
        #[cfg(feature = "tools_tracing")]
        server.add_plugins(polaris_tools::ToolsPlugin);
        server.add_plugins(TracingPlugin::default());
        server.finish().await;

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

    #[cfg(feature = "tools_tracing")]
    mod tools_tracing_tests {
        use super::*;
        use polaris_tools::ToolContext;
        use polaris_tools::permission::ToolPermission;
        use polaris_tools::tool::Tool;
        use polaris_tools::{ToolError, ToolRegistry, ToolsPlugin};
        use std::future::Future;
        use std::pin::Pin;

        struct StubTool(&'static str);

        impl Tool for StubTool {
            fn definition(&self) -> polaris_models::llm::ToolDefinition {
                polaris_models::llm::ToolDefinition {
                    name: self.0.into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }

            fn execute<'ctx>(
                &'ctx self,
                _args: serde_json::Value,
                _ctx: &'ctx ToolContext,
            ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'ctx>>
            {
                Box::pin(async { Ok(serde_json::json!("ok")) })
            }
        }

        #[tokio::test]
        async fn decoration_preserves_permission_overrides() {
            let mut server = Server::new();

            let tools = ToolsPlugin;
            tools.build(&mut server);

            {
                let mut registry = server
                    .get_resource_mut::<ToolRegistry>()
                    .expect("ToolRegistry should exist after build");
                registry.register(StubTool("safe_tool"));
                registry.register(StubTool("dangerous_tool"));
                registry
                    .set_permission("dangerous_tool", ToolPermission::Deny)
                    .unwrap();
            }

            tools.ready(&mut server).await;

            let tracing = TracingPlugin::default();
            tracing.decorate_tool_registry(&mut server);

            let registry = server
                .get_global::<ToolRegistry>()
                .expect("decorated ToolRegistry should exist");
            assert_eq!(
                registry.permission("dangerous_tool"),
                Some(ToolPermission::Deny),
                "permission override should survive decoration"
            );
            assert_eq!(
                registry.permission("safe_tool"),
                Some(ToolPermission::Allow),
                "default permission should be preserved"
            );
        }
    }
}
