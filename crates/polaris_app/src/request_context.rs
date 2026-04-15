//! Request context plugin for propagating trace and correlation IDs.
//!
//! Provides [`RequestContextPlugin`] which registers [`RequestContext`] as a
//! per-context local resource. HTTP handlers insert the raw request headers
//! as [`HttpHeaders`] on the context; an `OnGraphStart` hook then parses them
//! into a [`RequestContext`] that systems read via `Res<RequestContext>`.
//!
//! # Flow
//!
//! ```text
//! tower middleware   ─▶ stamps x-request-id on the Request
//! axum handler       ─▶ ctx.insert(HttpHeaders(headers))
//! OnGraphStart hook  ─▶ RequestContext::from_headers(&headers) into ctx
//! systems            ─▶ Res<RequestContext>
//! ```
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
//! ```

use axum::extract::FromRequestParts;
use hashbrown::HashMap;
use http::HeaderMap;
use http::request::Parts;
use polaris_graph::hooks::api::BoxedHook;
use polaris_graph::hooks::schedule::OnGraphStart;
use polaris_graph::hooks::{GraphEvent, HooksAPI};
use polaris_system::param::SystemContext;
use polaris_system::plugin::{Plugin, ScheduleId, Version};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use std::any::TypeId;
use std::convert::Infallible;

/// Per-request context carrying trace and correlation identifiers.
///
/// Built automatically from [`HttpHeaders`] by an `OnGraphStart` hook when a
/// graph executes under an HTTP request. For non-HTTP paths, the default
/// factory produces a context with a generated `trace_id`.
///
/// # Header Conventions
///
/// | Header | Field | Fallback |
/// |--------|-------|----------|
/// | `x-trace-id` | [`trace_id`](Self::trace_id) | Generated value (timestamp + thread ID, not cryptographically random) |
/// | `x-correlation-id` | [`correlation_id`](Self::correlation_id) | `None` |
/// | `x-request-id` | [`request_id`](Self::request_id) | `None` (populated by `SetRequestIdLayer` middleware in `polaris_app`) |
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
    /// Framework-generated request ID from the `x-request-id` header.
    ///
    /// `polaris_app`'s `SetRequestIdLayer` middleware stamps this header on
    /// every HTTP request, so this field is `Some(_)` for any graph that ran
    /// under an HTTP request. `None` outside the HTTP path (tests, background
    /// jobs).
    pub request_id: Option<String>,
    /// Additional propagated headers or metadata.
    pub extras: HashMap<String, String>,
}

impl LocalResource for RequestContext {}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            trace_id: generate_trace_id(),
            correlation_id: None,
            request_id: None,
            extras: HashMap::new(),
        }
    }
}

impl RequestContext {
    /// Builds a `RequestContext` from an HTTP `HeaderMap`.
    ///
    /// Missing `x-trace-id` falls back to a generated value; missing
    /// `x-correlation-id` and `x-request-id` stay `None`. Lenient by design —
    /// policy about required headers lives at the app layer, not here.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let header_str = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        Self {
            trace_id: header_str("x-trace-id").unwrap_or_else(generate_trace_id),
            correlation_id: header_str("x-correlation-id"),
            request_id: header_str("x-request-id"),
            extras: HashMap::new(),
        }
    }
}

/// Extracts a [`RequestContext`] from request headers.
///
/// Infallible by design — missing headers become `None` and a missing
/// `x-trace-id` falls back to a generated value. Policy about which headers
/// must be present (e.g. requiring a correlation ID) belongs at the
/// application layer, not the framework.
impl<S: Send + Sync> FromRequestParts<S> for RequestContext {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Self::from_headers(&parts.headers))
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

/// Raw HTTP headers inserted by an axum handler for the current turn.
///
/// HTTP handlers insert `HttpHeaders(headers)` into the context inside the
/// setup closure passed to `process_turn_with`. The `RequestContextPlugin`'s
/// `OnGraphStart` hook reads this and produces a parsed [`RequestContext`].
///
/// Exposed as a resource so other plugins can also read the raw `HeaderMap`
/// (e.g. for headers that `RequestContext` doesn't surface as typed fields).
#[derive(Debug, Clone, Default)]
pub struct HttpHeaders(pub HeaderMap);

impl LocalResource for HttpHeaders {}

/// Plugin that provides per-request trace context.
///
/// Registers:
/// - [`RequestContext`] as a local resource with a default factory
///   (auto-generated `trace_id`, all other fields empty).
/// - An `OnGraphStart` hook that replaces the default with
///   `RequestContext::from_headers(&headers)` whenever [`HttpHeaders`] has
///   been inserted into the context.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`RequestContext`] | Local | Trace, correlation, and request IDs |
///
/// # Dependencies
///
/// Requires [`HooksAPI`] from `polaris_graph`. Inserts it if not already
/// present on the server.
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

        if !server.contains_api::<HooksAPI>() {
            server.insert_api(HooksAPI::new());
        }

        let hooks = server
            .api::<HooksAPI>()
            .expect("HooksAPI must be present after initialization");

        hooks
            .register_boxed(
                ScheduleId::of::<OnGraphStart>(),
                "request_context_from_headers",
                BoxedHook::new(
                    |ctx: &mut SystemContext<'_>, _event: &GraphEvent| {
                        let req_ctx = {
                            let Ok(headers) = ctx.get_resource::<HttpHeaders>() else {
                                return;
                            };
                            RequestContext::from_headers(&headers.0)
                        };
                        ctx.insert(req_ctx);
                    },
                    vec![TypeId::of::<RequestContext>()],
                ),
            )
            .expect("RequestContextPlugin hook registration must not fail");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn default_request_context_has_trace_id() {
        let ctx = RequestContext::default();
        assert!(!ctx.trace_id.is_empty());
        assert!(ctx.correlation_id.is_none());
        assert!(ctx.request_id.is_none());
        assert!(ctx.extras.is_empty());
    }

    #[test]
    fn default_trace_ids_are_unique() {
        let a = RequestContext::default();
        let b = RequestContext::default();
        assert_ne!(a.trace_id, b.trace_id);
    }

    #[test]
    fn from_headers_populates_all_known_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("x-trace-id", HeaderValue::from_static("trace-abc"));
        headers.insert("x-correlation-id", HeaderValue::from_static("corr-123"));
        headers.insert("x-request-id", HeaderValue::from_static("req-xyz"));

        let ctx = RequestContext::from_headers(&headers);
        assert_eq!(ctx.trace_id, "trace-abc");
        assert_eq!(ctx.correlation_id.as_deref(), Some("corr-123"));
        assert_eq!(ctx.request_id.as_deref(), Some("req-xyz"));
    }

    #[test]
    fn from_headers_falls_back_when_trace_id_missing() {
        let headers = HeaderMap::new();
        let ctx = RequestContext::from_headers(&headers);
        assert!(!ctx.trace_id.is_empty());
        assert!(ctx.correlation_id.is_none());
        assert!(ctx.request_id.is_none());
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
            ..Default::default()
        });

        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert_eq!(req.trace_id, "custom-123");
        assert_eq!(req.correlation_id.as_deref(), Some("corr-456"));
    }

    #[tokio::test]
    async fn on_graph_start_hook_builds_from_http_headers() {
        use polaris_graph::hooks::schedule::OnGraphStart;

        let mut server = Server::new();
        server.add_plugins(RequestContextPlugin);
        server.finish().await;

        let mut headers = HeaderMap::new();
        headers.insert("x-trace-id", HeaderValue::from_static("from-header"));
        headers.insert("x-request-id", HeaderValue::from_static("req-42"));

        let mut ctx = server.create_context();
        ctx.insert(HttpHeaders(headers));

        let hooks = server.api::<HooksAPI>().expect("HooksAPI present");
        hooks.invoke(
            ScheduleId::of::<OnGraphStart>(),
            &mut ctx,
            &GraphEvent::GraphStart {
                node_count: 0,
                node_map: Vec::new(),
            },
        );

        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert_eq!(req.trace_id, "from-header");
        assert_eq!(req.request_id.as_deref(), Some("req-42"));
    }

    #[tokio::test]
    async fn from_request_parts_reads_headers() {
        use axum::extract::FromRequestParts;
        use http::Request;

        let req = Request::builder()
            .header("x-trace-id", "trace-extract")
            .header("x-request-id", "req-extract")
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();

        let ctx = RequestContext::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(ctx.trace_id, "trace-extract");
        assert_eq!(ctx.request_id.as_deref(), Some("req-extract"));
    }

    #[tokio::test]
    async fn on_graph_start_hook_no_ops_without_http_headers() {
        use polaris_graph::hooks::schedule::OnGraphStart;

        let mut server = Server::new();
        server.add_plugins(RequestContextPlugin);
        server.finish().await;

        let mut ctx = server.create_context();
        let original_trace_id = ctx
            .get_resource::<RequestContext>()
            .unwrap()
            .trace_id
            .clone();

        let hooks = server.api::<HooksAPI>().expect("HooksAPI present");
        hooks.invoke(
            ScheduleId::of::<OnGraphStart>(),
            &mut ctx,
            &GraphEvent::GraphStart {
                node_count: 0,
                node_map: Vec::new(),
            },
        );

        let req = ctx.get_resource::<RequestContext>().unwrap();
        assert_eq!(req.trace_id, original_trace_id);
        assert!(req.request_id.is_none());
    }
}
