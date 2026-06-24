//! OpenTelemetry plugin for exporting traces via OTLP.
//!
//! Provides [`OpenTelemetryPlugin`] which sets up the OpenTelemetry SDK and
//! pushes a `tracing-opentelemetry` layer into [`TracingPlugin`](crate::TracingPlugin)'s
//! shared subscriber so that `tracing` spans are exported as OpenTelemetry traces.
//!
//! The default service name is `"polaris"` and can be overridden with
//! [`OpenTelemetryPlugin::with_service_name`].
//!
//! # Example
//!
//! ```no_run
//! use polaris_system::server::Server;
//! use polaris_core_plugins::{ServerInfoPlugin, TracingPlugin, OpenTelemetryPlugin};
//!
//! let mut server = Server::new();
//! server.add_plugins(ServerInfoPlugin);
//! # server.add_plugins(polaris_models::ModelsPlugin);
//! # server.add_plugins(polaris_tools::ToolsPlugin);
//! server.add_plugins(TracingPlugin::default());
//! server.add_plugins(
//!     OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
//!         .with_service_name("my-agent")
//! );
//! # tokio_test::block_on(async {
//! server.finish().await.unwrap();
//! # });
//! ```

use crate::{SpanProcessorRegistry, TracingLayers, TracingPlugin};
use opentelemetry::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, Span, SpanData, SpanProcessor};
use parking_lot::Mutex;
use polaris_system::plugin::{
    Contract, DefaultDependencies, Plugin, PluginAccess, PluginId, Version, VersionReq,
};
use polaris_system::server::Server;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::Layer as _;

struct BoxedSpanProcessor(Box<dyn SpanProcessor + 'static>);

impl std::fmt::Debug for BoxedSpanProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxedSpanProcessor").finish()
    }
}

impl SpanProcessor for BoxedSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.0.on_start(span, cx);
    }
    fn on_end(&self, span: SpanData) {
        self.0.on_end(span);
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
}

/// OpenTelemetry tracing plugin.
///
/// Sets up OTLP trace export via `tracing-opentelemetry`. All `tracing`
/// spans in the application are exported as OpenTelemetry traces.
///
/// # Lifecycle
///
/// - **`build()`** — builds the OTLP exporter and tracer, and pushes the
///   `OTel` layer into [`TracingLayers`].
/// - **`cleanup()`** — shuts down the tracer provider, flushing pending spans.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`SpanProcessorRegistry`] | Build-time | An empty registry, plus a fan-out [`SpanProcessor`] installed on the tracer provider. Downstream plugins contribute processors to it during their own `build()`. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin contributes a layer to the shared tracing subscriber and installs no APIs. |
///
/// # Dependencies
///
/// - [`TracingPlugin`](crate::TracingPlugin) — owns the subscriber and the
///   [`TracingLayers`] this plugin pushes its layer into. Auto-registered as a
///   default dependency if the host hasn't already added one.
///
/// # Extends
///
/// - [`TracingLayers`] (from [`TracingPlugin`](crate::TracingPlugin)) —
///   pushes a `tracing-opentelemetry` layer, scoped by its own
///   [`EnvFilter`](tracing_subscriber::EnvFilter), so every `tracing`
///   span is exported as an OpenTelemetry trace through the shared
///   subscriber. Composes with any other layer contributor — neither
///   knows about the other.
///
/// # Panics
///
/// `build()` panics if the configured endpoint, protocol, or export headers
/// cannot be assembled into a valid OTLP exporter. This surfaces operator
/// misconfiguration at server startup rather than silently dropping traces.
///
/// # Stability
///
/// The `otel` feature surface re-exports types from the pre-1.0
/// [`opentelemetry`](https://docs.rs/opentelemetry) crate family
/// (`SpanProcessor`, `Protocol`, `Context`, `Resource`, …). Those crates make
/// breaking changes across minor versions, so this plugin's public API tracks
/// the `opentelemetry` version it is built against — pin it accordingly in
/// downstream `otel`-feature consumers.
///
/// # Example
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{ServerInfoPlugin, TracingPlugin, OpenTelemetryPlugin};
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// # server.add_plugins(polaris_models::ModelsPlugin);
/// # server.add_plugins(polaris_tools::ToolsPlugin);
/// server.add_plugins(TracingPlugin::default());
/// # let api_key = "secret";
/// server.add_plugins(
///     OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
///         .with_service_name("my-agent")
///         .with_env_filter("polaris=debug,hyper=warn")
///         .with_resource_attribute("deployment.environment.name", "production")
///         .with_export_header("x-api-key", api_key),
/// );
/// # tokio_test::block_on(async {
/// server.finish().await.unwrap();
/// # });
/// ```
pub struct OpenTelemetryPlugin {
    endpoint: Option<String>,
    service_name: String,
    env_filter: Option<String>,
    resource_attributes: Vec<(String, String)>,
    export_headers: Vec<(String, String)>,
    protocol: Option<Protocol>,
    extra_span_processors: Mutex<Vec<Box<dyn SpanProcessor + 'static>>>,
    provider: Mutex<Option<SdkTracerProvider>>,
}

impl OpenTelemetryPlugin {
    /// Creates a new plugin exporting to the given OTLP HTTP endpoint.
    ///
    /// To set up the tracer provider without exporting to a collector — for
    /// in-process consumers such as a local dashboard — use [`without_export`].
    ///
    /// # Arguments
    ///
    /// * `endpoint` - OTLP HTTP endpoint
    ///
    /// [`without_export`]: Self::without_export
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: Some(endpoint.into()),
            ..Self::without_export()
        }
    }

    /// Creates a plugin that sets up the tracer provider with no export.
    #[must_use]
    pub fn without_export() -> Self {
        Self {
            endpoint: None,
            service_name: "polaris".to_string(),
            env_filter: None,
            resource_attributes: Vec::new(),
            export_headers: Vec::new(),
            protocol: None,
            extra_span_processors: Mutex::new(Vec::new()),
            provider: Mutex::new(None),
        }
    }

    /// Sets the OTLP wire protocol used by the exporter.
    ///
    /// The default is the OTLP crate's default ([`Protocol::HttpBinary`]).
    #[must_use]
    pub fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    /// Attach an additional [`SpanProcessor`] to the `OTel` `SdkTracerProvider`
    /// built by this plugin.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use opentelemetry::Context;
    /// use opentelemetry_sdk::error::OTelSdkResult;
    /// use opentelemetry_sdk::trace::{Span, SpanData, SpanProcessor};
    /// use polaris_core_plugins::OpenTelemetryPlugin;
    ///
    /// #[derive(Debug)]
    /// struct NoopProcessor;
    /// impl SpanProcessor for NoopProcessor {
    ///     fn on_start(&self, _: &mut Span, _: &Context) {}
    ///     fn on_end(&self, _: SpanData) {}
    ///     fn force_flush(&self) -> OTelSdkResult { Ok(()) }
    ///     fn shutdown_with_timeout(&self, _: Duration) -> OTelSdkResult { Ok(()) }
    /// }
    ///
    /// OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
    ///     .with_span_processor(NoopProcessor);
    /// ```
    #[must_use]
    pub fn with_span_processor(self, processor: impl SpanProcessor + 'static) -> Self {
        self.extra_span_processors.lock().push(Box::new(processor));
        self
    }

    /// Sets the service name reported in traces.
    #[must_use]
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    /// Sets a custom environment filter for `OTel` span export.
    ///
    /// Format: `target=level,target=level,...`
    #[must_use]
    pub fn with_env_filter(mut self, filter: impl Into<String>) -> Self {
        self.env_filter = Some(filter.into());
        self
    }

    /// Adds an `OTel` resource attribute to the trace provider.
    ///
    /// Resource attributes identify the entity producing telemetry
    /// (e.g., deployment environment, service version, host).
    ///
    /// ```
    /// use polaris_core_plugins::OpenTelemetryPlugin;
    ///
    /// OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
    ///     .with_resource_attribute("deployment.environment.name", "production")
    ///     .with_resource_attribute("service.version", "1.2.0");
    /// ```
    #[must_use]
    pub fn with_resource_attribute(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.resource_attributes.push((key.into(), value.into()));
        self
    }

    /// Adds an HTTP header to OTLP export requests.
    ///
    /// ```
    /// use polaris_core_plugins::OpenTelemetryPlugin;
    ///
    /// # let api_key = "secret";
    /// OpenTelemetryPlugin::new("https://api.honeycomb.io/v1/traces")
    ///     .with_export_header("x-api-key", api_key);
    /// ```
    #[must_use]
    pub fn with_export_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.export_headers.push((key.into(), value.into()));
        self
    }
}

/// Sets up the tracer provider without an exporter, equivalent to
/// [`OpenTelemetryPlugin::without_export`].
impl Default for OpenTelemetryPlugin {
    fn default() -> Self {
        Self::without_export()
    }
}

impl Plugin for OpenTelemetryPlugin {
    const ID: &'static str = "polaris::otel";
    const VERSION: Version = Version::new(0, 0, 1);

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<TracingLayers>(VersionReq::caret(TracingLayers::CONTRACT_VERSION))
            .provides::<SpanProcessorRegistry>(SpanProcessorRegistry::CONTRACT_VERSION)
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<TracingPlugin>()]
    }

    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new().add::<TracingPlugin>()
    }

    fn build(&self, server: &mut Server) {
        // Install the W3C trace-context propagator so HTTP boundary extractors
        // (e.g. `polaris_app::middleware`) can parent spans on upstream traces.
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

        // Build the `OTel` resource
        let mut resource_builder =
            opentelemetry_sdk::Resource::builder().with_service_name(self.service_name.clone());

        for (key, value) in &self.resource_attributes {
            resource_builder = resource_builder
                .with_attribute(opentelemetry::KeyValue::new(key.clone(), value.clone()));
        }

        // Build the tracer provider.
        let mut provider_builder =
            SdkTracerProvider::builder().with_resource(resource_builder.build());

        if let Some(endpoint) = &self.endpoint {
            let mut exporter_builder = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(endpoint);

            if let Some(protocol) = self.protocol {
                exporter_builder = exporter_builder.with_protocol(protocol);
            }

            if !self.export_headers.is_empty() {
                let headers: std::collections::HashMap<String, String> =
                    self.export_headers.iter().cloned().collect();
                exporter_builder = exporter_builder.with_headers(headers);
            }

            let exporter = exporter_builder
                .build()
                .expect("failed to create OTLP span exporter");
            provider_builder = provider_builder.with_batch_exporter(exporter);
        }

        // Chain in any caller-attached processors.
        for processor in self.extra_span_processors.lock().drain(..) {
            provider_builder = provider_builder.with_span_processor(BoxedSpanProcessor(processor));
        }

        // Provide the registry and install one fan-out processor that forwards to
        // every processor contributed by extenders.
        server.insert_resource(SpanProcessorRegistry::new());
        let fanout = server
            .get_resource::<SpanProcessorRegistry>()
            .expect("SpanProcessorRegistry just inserted")
            .fanout();
        provider_builder = provider_builder.with_span_processor(fanout);

        let provider = provider_builder.build();

        let tracer = provider.tracer_with_scope(
            opentelemetry::InstrumentationScope::builder("polaris")
                .with_version(env!("CARGO_PKG_VERSION"))
                .build(),
        );

        // Store provider for shutdown in cleanup()
        *self.provider.lock() = Some(provider);

        // Build the env filter for the `OTel` layer
        let env_filter = match &self.env_filter {
            Some(filter) => EnvFilter::try_new(filter).unwrap_or_else(|parse_err| {
                tracing::warn!(
                    filter = %filter,
                    error = %parse_err,
                    "invalid OTel env filter, falling back to \"info\""
                );
                EnvFilter::new("info")
            }),
            None => EnvFilter::new("info"),
        };

        // Push the `OTel` layer (with its own filter) into the shared registry.
        let mut layers = server
            .get_resource_mut::<TracingLayers>()
            .expect("TracingPlugin must provide TracingLayers before OpenTelemetryPlugin");
        layers.push(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(env_filter),
        );
    }

    async fn ready(&self, _server: &mut Server) {
        tracing::info!(
            endpoint = self.endpoint.as_deref().unwrap_or("none (export disabled)"),
            service_name = %self.service_name,
            "OpenTelemetryPlugin initialized"
        );
    }

    async fn cleanup(&self, _server: &mut Server) {
        tracing::debug!("OpenTelemetryPlugin shutting down, flushing spans");
        if let Some(provider) = self.provider.lock().take()
            && let Err(otel_err) = provider.shutdown()
        {
            tracing::warn!(error = %otel_err, "OTel provider shutdown error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerInfoPlugin, TracingPlugin};
    use opentelemetry::trace::{Span as _, Tracer as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock [`SpanProcessor`] that counts `on_end` invocations, mirroring the
    /// `Counter` used by the `span_processor_registry` tests.
    #[derive(Debug, Clone)]
    struct Counter(Arc<AtomicUsize>);

    impl SpanProcessor for Counter {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, _span: SpanData) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            Ok(())
        }
    }

    /// Mock [`SpanProcessor`] that records whether `shutdown_with_timeout` ran,
    /// so cleanup's provider shutdown is observable through a contributed
    /// processor.
    #[derive(Debug, Clone)]
    struct ShutdownFlag(Arc<std::sync::atomic::AtomicBool>);

    impl SpanProcessor for ShutdownFlag {
        fn on_start(&self, _span: &mut Span, _cx: &Context) {}
        fn on_end(&self, _span: SpanData) {}
        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Drives a span through the provider the plugin built and stored, exercising
    /// the processors actually installed on it.
    fn emit_span(plugin: &OpenTelemetryPlugin) {
        let guard = plugin.provider.lock();
        let provider = guard.as_ref().expect("build() must store a provider");
        let tracer = provider.tracer("otel-plugin-test");
        tracer.start("op").end();
    }

    #[test]
    fn build_wires_registry_and_reaches_caller_processor() {
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        TracingPlugin::default().build(&mut server);

        let count = Arc::new(AtomicUsize::new(0));
        // No endpoint: keep the test offline. The caller-supplied processor is
        // attached directly to the provider, independent of any exporter.
        let plugin = OpenTelemetryPlugin::without_export()
            .with_service_name("test-agent")
            .with_env_filter("polaris=debug")
            .with_resource_attribute("deployment.environment.name", "test")
            .with_span_processor(Counter(count.clone()));
        plugin.build(&mut server);

        // (a) The build phase inserts the fan-out registry resource.
        assert!(
            server.contains_resource::<SpanProcessorRegistry>(),
            "build() must insert SpanProcessorRegistry as a resource"
        );

        // (b) A processor supplied via `with_span_processor` before build is
        // reachable on the installed provider: emitting a span fires its
        // `on_end`.
        emit_span(&plugin);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "caller-supplied processor must receive on_end through the built provider"
        );
    }

    #[test]
    fn build_with_tracing_plugin() {
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        TracingPlugin::default().build(&mut server);

        let plugin = OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
            .with_service_name("test-agent")
            .with_env_filter("polaris=debug")
            .with_resource_attribute("deployment.environment.name", "test")
            .with_export_header("x-api-key", "test-key");
        plugin.build(&mut server);

        assert!(
            server.contains_resource::<SpanProcessorRegistry>(),
            "build() must insert SpanProcessorRegistry as a resource"
        );
        assert!(
            server.contains_resource::<TracingLayers>(),
            "TracingLayers must remain present after the OTel layer is pushed"
        );
    }

    #[test]
    fn declares_tracing_plugin_as_default_dependency() {
        // `OpenTelemetryPlugin` extends `TracingLayers`, owned by `TracingPlugin`.
        // It must auto-register `TracingPlugin` so a host adding only this plugin
        // still resolves. We assert on the `default_dependencies()` declaration
        // the resolver consumes (`Server::auto_register_default_dependencies`),
        // rather than on a full `finish()`: `TracingPlugin::ready()` installs a
        // *process-global* tracing subscriber via `try_init`, which can succeed
        // only once per test binary. A second `finish()`-with-`TracingPlugin`
        // test would race the existing `build_registers_config_and_layers_api`
        // and panic. The declaration is the behavior the resolver acts on.
        let defaults = OpenTelemetryPlugin::without_export().default_dependencies();
        assert_eq!(
            defaults.len(),
            1,
            "exactly one default dependency (TracingPlugin) should be declared"
        );
        let rendered = format!("{defaults:?}");
        let tracing_id = PluginId::of::<TracingPlugin>().to_string();
        assert!(
            rendered.contains(&tracing_id),
            "default dependencies should include TracingPlugin ({tracing_id}), got: {rendered}"
        );
    }

    #[tokio::test]
    async fn cleanup_shuts_down_provider() {
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Drive build()/cleanup() directly rather than through `finish()`:
        // `finish()` runs `TracingPlugin::ready()`, which installs a global
        // tracing subscriber that can only be set once per process (see
        // `declares_tracing_plugin_as_default_dependency`). `cleanup()` itself
        // needs neither `ready()` nor the global subscriber.
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        // `OpenTelemetryPlugin::build()` requires `TracingLayers`; supply it via
        // TracingPlugin's `build()` (no `ready()`, so no global install).
        TracingPlugin::default().build(&mut server);

        let plugin = OpenTelemetryPlugin::without_export()
            .with_service_name("cleanup-test")
            .with_span_processor(ShutdownFlag(shutdown.clone()));
        plugin.build(&mut server);

        assert!(
            plugin.provider.lock().is_some(),
            "build() must store the provider so cleanup() can shut it down"
        );
        assert!(
            !shutdown.load(Ordering::SeqCst),
            "provider must not be shut down before cleanup()"
        );

        // `OpenTelemetryPlugin::cleanup()` shuts down the stored
        // `SdkTracerProvider`, which propagates `shutdown` to its processors.
        plugin.cleanup(&mut server).await;

        assert!(
            shutdown.load(Ordering::SeqCst),
            "cleanup() must shut down the provider, propagating shutdown to processors"
        );
        assert!(
            plugin.provider.lock().is_none(),
            "cleanup() must take the provider, releasing it after flush"
        );
    }

    #[test]
    fn without_export_installs_no_exporter() {
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        TracingPlugin::default().build(&mut server);

        let plugin = OpenTelemetryPlugin::without_export().with_service_name("no-export");
        assert!(
            plugin.endpoint.is_none(),
            "without_export() must leave the OTLP endpoint unset"
        );

        // Builds successfully without contacting any collector, and still wires
        // up the provider + registry.
        plugin.build(&mut server);
        assert!(
            server.contains_resource::<SpanProcessorRegistry>(),
            "without_export() build must still provide SpanProcessorRegistry"
        );
        assert!(
            plugin.provider.lock().is_some(),
            "without_export() must still build and store a tracer provider"
        );
    }

    #[test]
    fn with_protocol_is_accepted() {
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        TracingPlugin::default().build(&mut server);

        let plugin = OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
            .with_protocol(Protocol::HttpBinary);
        assert_eq!(
            plugin.protocol,
            Some(Protocol::HttpBinary),
            "with_protocol must record the selected protocol"
        );

        // Selecting a protocol must not break exporter construction.
        plugin.build(&mut server);
        assert!(
            server.contains_resource::<SpanProcessorRegistry>(),
            "build with an explicit protocol must still provide the registry"
        );
    }

    #[test]
    fn build_with_invalid_env_filter_falls_back_to_info() {
        let mut server = Server::new();
        ServerInfoPlugin.build(&mut server);
        TracingPlugin::default().build(&mut server);

        // "bogus" is not a valid level, so `EnvFilter::try_new` rejects this
        // directive. `build()` must not propagate the parse error — it falls
        // back to "info" and continues wiring the provider/registry.
        assert!(
            EnvFilter::try_new("polaris=bogus").is_err(),
            "test premise: the filter string must be rejected by EnvFilter"
        );

        let plugin = OpenTelemetryPlugin::without_export().with_env_filter("polaris=bogus");
        plugin.build(&mut server);

        // Reaching here means the `unwrap_or_else` fallback ran without
        // panicking and the rest of build() completed.
        assert!(
            server.contains_resource::<SpanProcessorRegistry>(),
            "build must complete (falling back to \"info\") despite an invalid env filter"
        );
        assert!(
            plugin.provider.lock().is_some(),
            "build must still store the tracer provider after the filter fallback"
        );
    }
}
