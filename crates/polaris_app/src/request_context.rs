//! Request context plugin for propagating trace and correlation IDs.
//!
//! Provides [`RequestContextPlugin`] which registers [`RequestContext`] as a
//! per-context local resource. HTTP handlers populate it from request headers;
//! systems read it via `Res<RequestContext>`.
//!
//! # Example
//!
//! ```
//! use polaris_system::server::Server;
//! use polaris_system::param::Res;
//! use polaris_system::system;
//! use polaris_app::{RequestContextPlugin, RequestContext};
//!
//! #[system]
//! async fn traced_system(req_ctx: Res<RequestContext>) {
//!     tracing::info!(trace_id = %req_ctx.trace_id, "processing request");
//! }
//!
//! let mut server = Server::new();
//! server.add_plugins(RequestContextPlugin);
//! # tokio_test::block_on(async {
//! server.finish().await;
//! # });
//!
//! // Inject per-request values in a setup closure:
//! let mut ctx = server.create_context();
//! ctx.insert(RequestContext {
//!     trace_id: "abc-123".into(),
//!     correlation_id: Some("corr-456".into()),
//!     ..Default::default()
//! });
//! ```

use hashbrown::HashMap;
use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;

/// Per-request context carrying trace and correlation identifiers.
///
/// Injected by HTTP handlers (or test harnesses) via `ctx.insert(RequestContext { .. })`
/// in a setup closure. Systems read it as `Res<RequestContext>`.
///
/// The default factory generates a random trace ID so that contexts created
/// without explicit injection still have a unique identifier for tracing.
/// When the caller provides a trace ID (e.g. via an `x-trace-id` header),
/// the HTTP handler should construct a `RequestContext` with that value
/// instead of relying on the default.
///
/// # Header Conventions
///
/// When used with HTTP handlers, extract these headers:
///
/// | Header | Field | Fallback |
/// |--------|-------|----------|
/// | `x-trace-id` | [`trace_id`](Self::trace_id) | Generated value (timestamp + thread ID, not cryptographically random) |
/// | `x-correlation-id` | [`correlation_id`](Self::correlation_id) | `None` |
///
/// Any additional headers the application needs to propagate can be stored
/// in [`extras`](Self::extras).
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// Unique identifier for this request's trace.
    ///
    /// Prefer the caller-provided value from the `x-trace-id` header.
    /// Falls back to a generated value when no header is present.
    pub trace_id: String,
    /// Optional correlation ID linking related requests.
    ///
    /// Typically extracted from the `x-correlation-id` header.
    pub correlation_id: Option<String>,
    /// Additional propagated headers or metadata.
    ///
    /// Use this for application-specific headers that don't have dedicated
    /// fields (e.g. `x-forwarded-for`, `baggage`, custom routing headers).
    pub extras: HashMap<String, String>,
}

impl LocalResource for RequestContext {}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            trace_id: generate_trace_id(),
            correlation_id: None,
            extras: HashMap::new(),
        }
    }
}

fn generate_trace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let thread_id = std::thread::current().id();
    format!("{nanos:x}-{thread_id:?}")
}

/// Plugin that provides per-request trace context.
///
/// Registers [`RequestContext`] as a local resource with a default factory
/// that generates a unique trace ID. HTTP handlers should override the
/// default by calling `ctx.insert(RequestContext { .. })` in the setup
/// closure passed to `process_turn_with`, populating fields from the
/// incoming request headers.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`RequestContext`] | Local | Trace ID, correlation ID, and extra propagated headers |
///
/// # Dependencies
///
/// None.
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_app::{RequestContextPlugin, RequestContext};
///
/// let mut server = Server::new();
/// server.add_plugins(RequestContextPlugin);
/// # tokio_test::block_on(async {
/// server.finish().await;
/// # });
///
/// // Default context has an auto-generated trace ID
/// let ctx = server.create_context();
/// let req = ctx.get_resource::<RequestContext>().unwrap();
/// assert!(!req.trace_id.is_empty());
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestContextPlugin;

impl Plugin for RequestContextPlugin {
    const ID: &'static str = "polaris::request_context";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.register_local(RequestContext::default);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_request_context_has_trace_id() {
        let ctx = RequestContext::default();
        assert!(!ctx.trace_id.is_empty());
        assert!(ctx.correlation_id.is_none());
        assert!(ctx.extras.is_empty());
    }

    #[test]
    fn default_trace_ids_are_unique() {
        let a = RequestContext::default();
        let b = RequestContext::default();
        assert_ne!(a.trace_id, b.trace_id);
    }

    #[tokio::test]
    async fn plugin_registers_local_resource() {
        let mut server = Server::new();
        server.add_plugins(RequestContextPlugin);
        server.finish().await;

        let ctx = server.create_context();
        assert!(ctx.contains_resource::<RequestContext>());
        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert!(!req.trace_id.is_empty());
    }

    #[tokio::test]
    async fn injected_context_overrides_default() {
        let mut server = Server::new();
        server.add_plugins(RequestContextPlugin);
        server.finish().await;

        let mut ctx = server.create_context();
        ctx.insert(RequestContext {
            trace_id: "custom-123".into(),
            correlation_id: Some("corr-456".into()),
            extras: HashMap::new(),
        });

        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert_eq!(req.trace_id, "custom-123");
        assert_eq!(req.correlation_id.as_deref(), Some("corr-456"));
    }

    #[tokio::test]
    async fn extras_propagate_additional_headers() {
        let mut server = Server::new();
        server.add_plugins(RequestContextPlugin);
        server.finish().await;

        let mut extras = HashMap::new();
        extras.insert("x-forwarded-for".into(), "10.0.0.1".into());
        extras.insert("baggage".into(), "env=prod".into());

        let mut ctx = server.create_context();
        ctx.insert(RequestContext {
            extras,
            ..Default::default()
        });

        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert_eq!(req.extras.get("x-forwarded-for").unwrap(), "10.0.0.1");
        assert_eq!(req.extras.get("baggage").unwrap(), "env=prod");
    }
}
