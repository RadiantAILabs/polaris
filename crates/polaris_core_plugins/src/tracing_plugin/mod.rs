//! Tracing plugin and subscriber infrastructure.
//!
//! [`TracingPlugin`] registers a `tracing` subscriber. By default no output
//! layers are included — call [`TracingPlugin::with_fmt`] to add console
//! output, or use [`DefaultPlugins`](crate::DefaultPlugins) which enables
//! fmt automatically.
//!
//! Other plugins can register additional layers (e.g., `OTel` export) via
//! [`TracingLayers`] during their `build()` phase. The subscriber is
//! installed once in [`TracingPlugin::ready()`] with all accumulated layers.

mod fmt_layer;
pub use fmt_layer::FmtConfig;
#[cfg(feature = "dashboard")]
mod buffer;
mod capture;
#[cfg(feature = "dashboard")]
mod dashboard;
mod instrument;
mod span_record;
mod span_store;
#[cfg(feature = "dashboard")]
mod usage;
#[cfg(feature = "dashboard")]
mod usage_pricing;
#[cfg(feature = "dashboard")]
pub use buffer::{RunSummary, SessionSummary, SpanBuffer, SpanEvent, SpanNode, SpanTree, TreeView};
// `RecordingLayer` / `SpanRecordSink` are the subscriber-side capture
// primitives. They are needed by `SpanStorePlugin` (durable history) and
// by the dashboard's in-process buffer; expose them unconditionally so
// either composition works without `dashboard`.
pub use capture::{RecordingLayer, SpanRecordSink};
#[cfg(feature = "dashboard")]
pub use dashboard::SpansResponse;
// `SpanRecord` / `SpanKind` are always exposed — they're the wire types
// shared by the `dashboard` buffer and the `file-store` backend, and
// gating them on either feature would force the umbrella to pull both
// when only one is wanted.
pub use span_record::{SpanKind, SpanRecord};
pub use span_store::{
    DynSpanStore, InMemorySpanStore, SpanStore, SpanStoreError, SpanStoreHandle, SpanStorePlugin,
};
#[cfg(feature = "file-store")]
pub use span_store::{FileSpanStore, FileSpanStoreError};
#[cfg(feature = "dashboard")]
pub use usage::{TokenUsageBreakdown, TokenUsageResponse, TokenUsageTotals};
#[cfg(feature = "dashboard")]
pub use usage_pricing::{ModelPricing, UsagePricing};

use crate::ServerInfoPlugin;
use crate::tracing_plugin::instrument::llm::TracingLlmProvider;
use polaris_system::plugin::{DefaultDependencies, Plugin, PluginId, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// ─────────────────────────────────────────────────────────────────────────────
// TracingLayers
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
/// use polaris_core_plugins::{ServerInfoPlugin, TracingLayers, TracingPlugin};
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
///             .get_resource_mut::<TracingLayers>()
///             .expect("TracingPlugin must be added first");
///         api.push(fmt::layer().compact());
///     }
/// }
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// # #[cfg(feature = "dashboard")]
/// # server.add_plugins(polaris_app::AppPlugin::new(polaris_app::AppConfig::new().with_host("127.0.0.1")));
/// # server.add_plugins(polaris_models::ModelsPlugin);
/// # server.add_plugins(polaris_tools::ToolsPlugin);
/// server.add_plugins(TracingPlugin::new());
/// server.add_plugins(MyPlugin);
/// # tokio_test::block_on(async {
/// server.run_once().await;
/// # });
/// ```
pub struct TracingLayers {
    layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>>,
}

impl TracingLayers {
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
/// Registers a [`TracingLayers`] during `build()` so other plugins can
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
/// | [`TracingLayers`] | Build-time | Layer registration for other plugins |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`SpanBuffer`] *(feature `dashboard`)* | In-process ring buffer of recent records. Backs the `/v1/tracing/*` and `/v1/sessions/{id}/runs[/...]` dashboard endpoints. |
/// | [`UsagePricing`] *(feature `dashboard`)* | Build-time per-`(provider, model)` rate table consulted by the usage endpoints. Empty by default; consumers populate it during `build()`. |
///
/// The layer registry itself is exposed as the mutable [`TracingLayers`]
/// **resource** (`get_resource_mut`), not via
/// [`Server::insert_api`](polaris_system::server::Server::insert_api) — see
/// the *Resources Provided* table.
///
/// # Dependencies
///
/// - [`ServerInfoPlugin`]
/// - [`ModelsPlugin`](polaris_models::ModelsPlugin) — for LLM provider instrumentation
/// - [`ToolsPlugin`](polaris_tools::ToolsPlugin) — for tool instrumentation
/// - [`AppPlugin`](polaris_app::AppPlugin) — when the `dashboard` feature is enabled
///
/// # Routes Provided
///
/// Mounted only when the `dashboard` feature is enabled, against the
/// [`HttpRouter`](polaris_app::HttpRouter) owned by `AppPlugin`. Every
/// handler is read-only (`GET`) and reads its parameters from axum
/// `Path` / `Query` extractors.
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `GET` | `/v1/tracing/spans` | Flat tail of recent span/event records. |
/// | `GET` | `/v1/tracing/runs` | Distinct runs observed in the buffer. |
/// | `GET` | `/v1/tracing/runs/{run_id}` | Hierarchical span tree for a run. |
/// | `GET` | `/v1/tracing/runs/{run_id}/spans/{span_id}` | One span's close record. |
/// | `GET` | `/v1/tracing/runs/{run_id}/usage` | Token-usage rollup for one run. |
/// | `GET` | `/v1/tracing/sessions` | Distinct sessions observed in the buffer. |
/// | `GET` | `/v1/tracing/usage` | Buffer-wide token-usage rollup (optional `?label=key:value`). |
/// | `GET` | `/v1/sessions/{session_id}/runs` | Runs filtered by the `session_id` label. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/tree` | Span tree, gated on session membership. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/spans/{span_id}` | One span, gated on session membership. |
/// | `GET` | `/v1/sessions/{session_id}/usage` | Token-usage rollup summed across the session's runs. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/usage` | Per-run token-usage rollup, gated on session membership. |
///
/// # Middleware Registered
///
/// Registered via [`MiddlewareAPI`](polaris_graph::MiddlewareAPI) under the
/// name `"tracing"`. Each handler wraps the node in a `tracing` span so
/// graph execution is observable end-to-end.
///
/// | Target | Behavior | Description |
/// |--------|----------|-------------|
/// | Graph execution | Wrap | Opens a `polaris.graph.execute` span around the whole run. |
/// | System | Wrap | Opens a `polaris.graph.execute_system` span per system node. |
/// | Loop / loop iteration | Wrap | Opens `polaris.graph.execute_loop` and `polaris.graph.loop_iteration` spans. |
/// | Parallel / parallel branch | Wrap | Opens `polaris.graph.execute_parallel` and `polaris.graph.parallel_branch` spans. |
/// | Decision | Wrap | Opens a `polaris.graph.execute_decision` span; the chosen branch is recorded by the hook below. |
/// | Switch | Wrap | Opens a `polaris.graph.execute_switch` span; the selected case is recorded by the hook below. |
///
/// # Hooks Registered
///
/// Registered via [`HooksAPI`](polaris_graph::hooks::HooksAPI) as observers
/// that record branch/case outcomes onto the active middleware span.
///
/// | Schedule | Description |
/// |----------|-------------|
/// | `OnDecisionComplete` | Records the selected branch on the enclosing `polaris.graph.execute_decision` span. |
/// | `OnSwitchComplete` | Records the selected case and `used_default` flag on the enclosing `polaris.graph.execute_switch` span. |
///
/// # Panics
///
/// `ready()` panics if a global `tracing` subscriber is already
/// installed by the time `TracingPlugin` reaches its `ready` phase —
/// for example, if the host binary called
/// `tracing_subscriber::fmt().init()` (or a test harness installed a
/// capture subscriber) before `Server::run`. There can only be one
/// global subscriber per process; route every layer through this
/// plugin (see [`TracingLayers`]) or omit the external installation.
///
/// # Lifecycle
///
/// - **`build()`** — inserts [`TracingConfig`] and the [`TracingLayers`]
///   resource, optionally pushes the fmt layer, and registers the graph
///   instrumentation middleware and hooks. With the `dashboard` feature
///   on, also installs the span buffer, usage-pricing API, recording
///   layer, and HTTP routes.
/// - **`ready()`** — installs the global `tracing` subscriber from all
///   accumulated layers, then decorates the [`ModelRegistry`](polaris_models::ModelRegistry)
///   and [`ToolRegistry`](polaris_tools::ToolRegistry) by rebuilding them
///   with tracing-instrumented providers and tools.
/// - The `dashboard` feature gates the buffer, the usage-pricing API, the
///   `AppPlugin` dependency, and every route above.
/// - Registers no tick schedules.
///
/// # Extends
///
/// - [`MiddlewareAPI`](polaris_graph::MiddlewareAPI) — registers
///   span-creating middleware on every graph execution target. Inserts
///   the API if no other plugin provided it.
/// - [`HooksAPI`](polaris_graph::hooks::HooksAPI) — registers the
///   decision/switch outcome observer hooks. Inserts the API if absent.
/// - [`ModelRegistry`](polaris_models::ModelRegistry) (from
///   [`ModelsPlugin`](polaris_models::ModelsPlugin)) — in `ready()`,
///   wraps each registered LLM provider in a tracing decorator.
/// - [`ToolRegistry`](polaris_tools::ToolRegistry) (from
///   [`ToolsPlugin`](polaris_tools::ToolsPlugin)) — in `ready()`, wraps
///   each registered tool in a tracing decorator.
/// - [`HttpRouter`](polaris_app::HttpRouter) (from
///   [`AppPlugin`](polaris_app::AppPlugin)) *(feature `dashboard`)* —
///   mounts the dashboard routes listed above.
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
/// # #[cfg(feature = "dashboard")]
/// # server.add_plugins(polaris_app::AppPlugin::new(polaris_app::AppConfig::new().with_host("127.0.0.1")));
/// # server.add_plugins(polaris_models::ModelsPlugin);
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

/// Builds a `TracingPlugin` with **no output layers attached**.
///
/// This mirrors `TracingPlugin::new()` / [`TracingPlugin::quiet`]: the
/// subscriber installs without an `fmt` layer, so the process emits no
/// console tracing output until another plugin (or
/// [`with_fmt`](TracingPlugin::with_fmt)) contributes one. If you want
/// readable output out of the box, use [`TracingPlugin::pretty`] or
/// add the plugin via [`DefaultPlugins`](crate::DefaultPlugins).
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
    ///
    /// Equivalent to [`TracingPlugin::quiet`]: no `fmt` layer is attached,
    /// so the process emits no console tracing output. Call
    /// [`with_fmt`](Self::with_fmt) to opt in, or use
    /// [`TracingPlugin::pretty`] for the human-readable default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a `TracingPlugin` with the human-readable `fmt` console
    /// layer attached at `INFO` level.
    ///
    /// Picks up [`FmtConfig::default()`] (pretty format,
    /// `RUST_LOG`-driven env filter). Reach for this when you want
    /// readable output without thinking about the layer wiring;
    /// [`DefaultPlugins`](crate::DefaultPlugins) installs this variant.
    #[must_use]
    pub fn pretty() -> Self {
        Self::default().with_fmt(FmtConfig::default())
    }

    /// Creates a `TracingPlugin` with no `fmt` layer.
    ///
    /// Identical to [`TracingPlugin::new`] / [`TracingPlugin::default`],
    /// but names the intent: the subscriber installs without console
    /// output. Use this when another plugin (e.g. `OpenTelemetryPlugin`,
    /// available under feature `otel`) is the only layer you want active.
    #[must_use]
    pub fn quiet() -> Self {
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

        server.insert_resource(TracingLayers::new());

        if let Some(fmt) = &self.fmt {
            let mut api = server
                .get_resource_mut::<TracingLayers>()
                .expect("TracingLayers must exist after insert");

            fmt_layer::push_layer(&mut api, fmt, self.level);
        }

        self.register_instrumentation(server);

        #[cfg(feature = "dashboard")]
        dashboard::install(server);
    }

    async fn ready(&self, server: &mut Server) {
        if let Some(api) = server.remove_resource::<TracingLayers>() {
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
            not(feature = "dashboard"),
            expect(unused_mut, reason = "mutated only when `dashboard` is enabled")
        )]
        let mut deps = vec![
            PluginId::of::<ServerInfoPlugin>(),
            PluginId::of::<polaris_models::ModelsPlugin>(),
            PluginId::of::<polaris_tools::ToolsPlugin>(),
        ];
        #[cfg(feature = "dashboard")]
        deps.push(PluginId::of::<polaris_app::AppPlugin>());
        deps
    }

    /// Auto-registers the zero-config dependencies so a user adding only
    /// `TracingPlugin` (or `DefaultPlugins`) doesn't have to wire each
    /// satellite plugin by hand.
    ///
    /// [`AppPlugin`](polaris_app::AppPlugin) is intentionally omitted —
    /// it requires explicit host/port configuration, so a default
    /// instance would mask configuration mistakes rather than help.
    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new()
            .add::<ServerInfoPlugin>()
            .add::<polaris_models::ModelsPlugin>()
            .add::<polaris_tools::ToolsPlugin>()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Instrumentation
// ─────────────────────────────────────────────────────────────────────────────

impl TracingPlugin {
    /// Registers graph middleware for span creation around node execution
    /// and hooks that record decision/switch outcomes onto those spans.
    fn register_instrumentation(&self, server: &mut Server) {
        if !server.contains_api::<polaris_graph::MiddlewareAPI>() {
            server.insert_api(polaris_graph::MiddlewareAPI::new());
        }

        let mw = server
            .api::<polaris_graph::MiddlewareAPI>()
            .expect("MiddlewareAPI should be present after initialization");
        instrument::graph::register(mw);

        if !server.contains_api::<polaris_graph::hooks::HooksAPI>() {
            server.insert_api(polaris_graph::hooks::HooksAPI::new());
        }

        let hooks = server
            .api::<polaris_graph::hooks::HooksAPI>()
            .expect("HooksAPI should be present after initialization");
        instrument::graph::register_outcome_hooks(hooks);
    }

    /// Wraps registered providers and tools with tracing decorators.
    fn decorate_registries(&self, server: &mut Server) {
        self.decorate_model_registry(server);
        self.decorate_tool_registry(server);
    }

    /// Rebuilds the [`ModelRegistry`] with tracing-decorated providers.
    fn decorate_model_registry(&self, server: &mut Server) {
        let capture = self.capture_genai_content;
        let Some(old) = server.insert_global(polaris_models::ModelRegistry::default()) else {
            tracing::warn!("no ModelRegistry found — LLM tracing decoration skipped");
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
    fn decorate_tool_registry(&self, server: &mut Server) {
        use instrument::tool::TracingTool;

        let capture = self.capture_genai_content;
        let Some(old) = server.insert_global(polaris_tools::ToolRegistry::default()) else {
            tracing::warn!("no ToolRegistry found — tool tracing decoration skipped");
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
        // `TracingPlugin` always depends on ModelsPlugin + ToolsPlugin (for
        // instrumentation decoration). `dashboard` additionally requires
        // AppPlugin for HTTP-router wiring.
        #[cfg(feature = "dashboard")]
        server.add_plugins(polaris_app::AppPlugin::new(
            polaris_app::AppConfig::new().with_host("127.0.0.1"),
        ));
        server.add_plugins(polaris_models::ModelsPlugin);
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
        // This test calls `plugin.build(...)` directly rather than through
        // `add_plugins`, so AppPlugin's own `build` must run first to make
        // the HttpRouter API available to the dashboard wiring inside
        // TracingPlugin::build.
        #[cfg(feature = "dashboard")]
        Plugin::build(
            &polaris_app::AppPlugin::new(polaris_app::AppConfig::new().with_host("127.0.0.1")),
            &mut server,
        );

        let plugin = TracingPlugin::default();
        plugin.build(&mut server);

        assert!(
            server.contains_resource::<TracingLayers>(),
            "should create TracingLayers for layer accumulation"
        );
    }

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

            // `dashboard` makes ToolsPlugin require AppPlugin. This test
            // calls `tools.build` directly, so AppPlugin's own `build` must
            // be invoked the same way to install the HttpRouter API.
            #[cfg(feature = "dashboard")]
            Plugin::build(
                &polaris_app::AppPlugin::new(polaris_app::AppConfig::new().with_host("127.0.0.1")),
                &mut server,
            );

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
