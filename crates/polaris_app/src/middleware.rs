//! Tower middleware stack for the HTTP server.

use crate::auth::AuthProvider;
use crate::config::AppConfig;
use axum::extract::Request;
use axum::middleware::Next;
use http::header::{CONTENT_TYPE, HeaderName};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::propagate_header::PropagateHeaderLayer;
use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

/// Header name for request IDs.
pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Applies the Tower middleware stack to an axum [`Router`](axum::Router).
///
/// The stack includes (in application order, outermost first):
/// 1. **Request ID** — injects a UUID `x-request-id` header on every request
/// 2. **Tracing** — logs request/response spans via `tracing`
/// 3. **Auth** — optional [`AuthProvider`] check (if registered)
/// 4. **CORS** — configurable allowed origins
/// 5. **Propagate header** — copies `x-request-id` from request to response
pub(crate) fn apply_middleware(
    router: axum::Router,
    config: &AppConfig,
    auth: Option<Arc<dyn AuthProvider>>,
) -> axum::Router {
    let cors = build_cors_layer(config);

    let router = router
        .layer(PropagateHeaderLayer::new(X_REQUEST_ID.clone()))
        .layer(cors);

    // Auth layer is applied between CORS and tracing so that:
    // - CORS preflight requests pass through (browsers need them)
    // - Rejected requests still appear in trace logs
    let router = if let Some(provider) = auth {
        router.layer(axum::middleware::from_fn(move |req, next| {
            auth_middleware(provider.clone(), req, next)
        }))
    } else {
        router
    };

    router
        .layer(http_trace_layer())
        .layer(SetRequestIdLayer::new(
            X_REQUEST_ID.clone(),
            MakeRequestUuid,
        ))
}

/// Axum middleware that delegates to an [`AuthProvider`].
async fn auth_middleware(
    provider: Arc<dyn AuthProvider>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    match provider.authenticate(&parts) {
        Ok(()) => {
            let req = Request::from_parts(parts, body);
            next.run(req).await
        }
        Err(rejection) => *rejection,
    }
}

/// Builds the HTTP tracing layer.
///
/// Emits spans under the `polaris::http` target so they are captured by
/// `polaris=debug` (or similar) env-filter directives.
///
/// When the `otel` feature is enabled, attribute names follow the
/// [OTel HTTP semantic conventions][semconv] and include `otel.name` /
/// `otel.kind` fields. Without the feature, Polaris-namespaced attributes
/// are used instead.
///
/// [semconv]: https://opentelemetry.io/docs/specs/semconv/http/http-spans/
///
/// # Span fields
///
/// | Field (otel) | Field (default) | Description |
/// |--------------|-----------------|-------------|
/// | `http.request.method` | `polaris.http.method` | HTTP method |
/// | `url.path` | `polaris.http.path` | Request URI path |
/// | `http.response.status_code` | `polaris.http.status_code` | Response status |
/// | `polaris.http.request_id` | `polaris.http.request_id` | `x-request-id` header |
fn http_trace_layer() -> TraceLayer<
    tower_http::classify::SharedClassifier<tower_http::classify::ServerErrorsAsFailures>,
    impl Fn(&Request) -> tracing::Span + Clone,
    impl Fn(&Request, &tracing::Span) + Clone,
    impl Fn(&http::Response<axum::body::Body>, Duration, &tracing::Span) + Clone,
> {
    TraceLayer::new_for_http()
        .make_span_with(|request: &Request| {
            let method = request.method().as_str();
            let path = request.uri().path();
            let request_id = request
                .headers()
                .get(&X_REQUEST_ID)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-");

            make_http_span(method, path, request_id)
        })
        .on_request(|_request: &Request, _span: &tracing::Span| {
            tracing::info!(target: "polaris::http", "started processing request");
        })
        .on_response(
            |response: &http::Response<axum::body::Body>,
             latency: Duration,
             span: &tracing::Span| {
                let status = response.status().as_u16();
                record_status(span, status);
                tracing::info!(
                    target: "polaris::http",
                    latency_ms = latency.as_millis() as u64,
                    "finished processing request"
                );
            },
        )
}

/// Creates the HTTP request span with OTel semantic convention attributes.
#[cfg(feature = "otel")]
fn make_http_span(method: &str, path: &str, request_id: &str) -> tracing::Span {
    tracing::info_span!(
        target: "polaris::http",
        "HTTP",
        otel.name = %format_args!("{method} {path}"),
        otel.kind = "Server",
        http.request.method = method,
        url.path = path,
        http.response.status_code = tracing::field::Empty,
        polaris.http.request_id = %request_id,
    )
}

/// Creates the HTTP request span with Polaris-namespaced attributes.
#[cfg(not(feature = "otel"))]
fn make_http_span(method: &str, path: &str, request_id: &str) -> tracing::Span {
    tracing::info_span!(
        target: "polaris::http",
        "polaris.http.request",
        polaris.http.method = method,
        polaris.http.path = path,
        polaris.http.status_code = tracing::field::Empty,
        polaris.http.request_id = %request_id,
    )
}

/// Records the response status code on the span.
#[cfg(feature = "otel")]
fn record_status(span: &tracing::Span, status: u16) {
    span.record("http.response.status_code", status);
}

/// Records the response status code on the span.
#[cfg(not(feature = "otel"))]
fn record_status(span: &tracing::Span, status: u16) {
    span.record("polaris.http.status_code", status);
}

/// Builds the CORS layer from config.
///
/// If no origins are configured, allows any origin.
/// Always allows `Content-Type` header and common HTTP methods.
fn build_cors_layer(config: &AppConfig) -> CorsLayer {
    let origins = config.cors_origins();

    let allow_origin = if origins.is_empty() {
        AllowOrigin::any()
    } else {
        let parsed: Vec<_> = origins
            .iter()
            .filter_map(|origin| match origin.parse() {
                Ok(val) => Some(val),
                Err(err) => {
                    tracing::warn!(
                        origin = %origin,
                        error = %err,
                        "ignoring invalid CORS origin"
                    );
                    None
                }
            })
            .collect();
        AllowOrigin::list(parsed)
    };

    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods(tower_http::cors::Any)
        .allow_headers([CONTENT_TYPE])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_allows_any_origin() {
        let config = AppConfig::default();
        let _cors = build_cors_layer(&config);
    }

    #[test]
    fn specific_origins_are_accepted() {
        let config = AppConfig::new()
            .with_cors_origin("http://localhost:3000")
            .with_cors_origin("https://example.com");
        let _cors = build_cors_layer(&config);
    }
}
