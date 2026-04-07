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
//! ```
//! use polaris_system::server::Server;
//! use polaris_core_plugins::{ServerInfoPlugin, TracingPlugin, OpenTelemetryPlugin};
//!
//! let mut server = Server::new();
//! server.add_plugins(ServerInfoPlugin);
//! # #[cfg(feature = "models_tracing")]
//! # server.add_plugins(polaris_models::ModelsPlugin);
//! # #[cfg(feature = "tools_tracing")]
//! # server.add_plugins(polaris_tools::ToolsPlugin);
//! server.add_plugins(TracingPlugin::default());
//! server.add_plugins(
//!     OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
//!         .with_service_name("my-agent")
//! );
//! # tokio_test::block_on(async {
//! server.finish().await;
//! # });
//! ```

use crate::TracingLayersApi;
use crate::TracingPlugin;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use parking_lot::Mutex;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::Layer as _;

/// OpenTelemetry tracing plugin.
///
/// Sets up OTLP trace export via `tracing-opentelemetry`. All `tracing`
/// spans in the application are exported as OpenTelemetry traces.
///
/// Depends on [`TracingPlugin`](crate::TracingPlugin) which owns the subscriber.
///
/// # Lifecycle
///
/// - **`build()`** — builds the OTLP exporter and tracer, and pushes the
///   `OTel` layer into [`TracingLayersApi`].
/// - **`cleanup()`** — shuts down the tracer provider, flushing pending spans.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::OpenTelemetryPlugin;
///
/// # let api_key = "secret";
/// OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
///     .with_service_name("my-agent")
///     .with_env_filter("polaris=debug,hyper=warn")
///     .with_resource_attribute("deployment.environment.name", "production")
///     .with_export_header("x-api-key", api_key);
/// ```
pub struct OpenTelemetryPlugin {
    endpoint: String,
    service_name: String,
    env_filter: Option<String>,
    resource_attributes: Vec<(String, String)>,
    export_headers: Vec<(String, String)>,
    provider: Mutex<Option<SdkTracerProvider>>,
}

impl OpenTelemetryPlugin {
    /// Creates a new plugin targeting the given OTLP HTTP endpoint.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - OTLP HTTP endpoint
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: "polaris".to_string(),
            env_filter: None,
            resource_attributes: Vec::new(),
            export_headers: Vec::new(),
            provider: Mutex::new(None),
        }
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

impl Plugin for OpenTelemetryPlugin {
    const ID: &'static str = "polaris::otel";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        // Build the OTLP HTTP exporter
        let mut exporter_builder = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&self.endpoint);

        if !self.export_headers.is_empty() {
            let headers: std::collections::HashMap<String, String> =
                self.export_headers.iter().cloned().collect();
            exporter_builder = exporter_builder.with_headers(headers);
        }

        let exporter = exporter_builder
            .build()
            .expect("failed to create OTLP span exporter");

        // Build the `OTel` resource
        let mut resource_builder =
            opentelemetry_sdk::Resource::builder().with_service_name(self.service_name.clone());

        for (key, value) in &self.resource_attributes {
            resource_builder = resource_builder
                .with_attribute(opentelemetry::KeyValue::new(key.clone(), value.clone()));
        }

        // Build the tracer provider
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource_builder.build())
            .build();

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

        // Push the `OTel` layer (with its own filter) into the shared API
        let mut api = server
            .get_resource_mut::<TracingLayersApi>()
            .expect("TracingPlugin must be added before OpenTelemetryPlugin");

        api.push(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(env_filter),
        );
    }

    async fn ready(&self, _server: &mut Server) {
        tracing::info!(
            endpoint = %self.endpoint,
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

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<TracingPlugin>()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerInfoPlugin;

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
    }
}
