//! Tower middleware stack for the HTTP server.

use crate::auth::AuthProvider;
use crate::config::AppConfig;
use crate::public_route::{PublicPath, PublicPrefix};
use axum::extract::Request;
use axum::middleware::Next;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName};
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
/// 3. **Auth** — optional [`AuthProvider`] check (if registered). The
///    public-path allowlist on [`AppConfig`] is consulted first; matching
///    requests bypass the provider and are forwarded as-is.
/// 4. **CORS** — configurable allowed origins
/// 5. **Propagate header** — copies `x-request-id` from request to response
pub(crate) fn apply_middleware(
    router: axum::Router,
    config: &AppConfig,
    auth: Option<Arc<dyn AuthProvider>>,
) -> axum::Router {
    let cors = build_cors_layer(config, auth.is_some());

    let router = router
        .layer(PropagateHeaderLayer::new(X_REQUEST_ID.clone()))
        .layer(cors);

    // Auth layer is applied between CORS and tracing so that:
    // - CORS preflight requests pass through (browsers need them)
    // - Rejected requests still appear in trace logs
    let router = if let Some(provider) = auth {
        let allowlist = Arc::new(PublicAllowlist::from_config(config));
        router.layer(axum::middleware::from_fn(move |req, next| {
            auth_middleware(provider.clone(), allowlist.clone(), req, next)
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

/// Snapshot of the public-path allowlist used by [`auth_middleware`].
///
/// Built once at server start and shared (via `Arc`) into the middleware
/// closure so per-request matching is allocation-free. The validated
/// [`PublicPath`] / [`PublicPrefix`] newtypes guarantee that no entry is
/// empty or unanchored.
#[derive(Debug)]
struct PublicAllowlist {
    paths: Vec<PublicPath>,
    prefixes: Vec<PublicPrefix>,
}

impl PublicAllowlist {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            paths: config.public_paths().to_vec(),
            prefixes: config.public_prefixes().to_vec(),
        }
    }

    fn is_public(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p.as_str() == path)
            || self.prefixes.iter().any(|p| path.starts_with(p.as_str()))
    }
}

/// Axum middleware that delegates to an [`AuthProvider`], skipping any
/// request whose path matches the configured public-path allowlist.
async fn auth_middleware(
    provider: Arc<dyn AuthProvider>,
    allowlist: Arc<PublicAllowlist>,
    req: Request,
    next: Next,
) -> axum::response::Response {
    let (parts, body) = req.into_parts();
    if allowlist.is_public(parts.uri.path()) {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
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
            let headers = request.headers();
            let request_id = headers
                .get(&X_REQUEST_ID)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-");

            make_http_span(headers, method, path, request_id)
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

/// Creates the HTTP request span with `OTel` semantic convention attributes.
///
/// Extracts the W3C trace context (`traceparent` / `tracestate`) from request
/// headers via the globally installed propagator and sets it as the `OTel`
/// parent of the span. With no upstream context, the span starts a fresh
/// trace.
#[cfg(feature = "otel")]
fn make_http_span(
    headers: &http::HeaderMap,
    method: &str,
    path: &str,
    request_id: &str,
) -> tracing::Span {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let span = tracing::info_span!(
        target: "polaris::http",
        "HTTP",
        otel.name = %format_args!("{method} {path}"),
        otel.kind = "Server",
        http.request.method = method,
        url.path = path,
        http.response.status_code = tracing::field::Empty,
        polaris.http.request_id = %request_id,
    );

    let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(headers))
    });
    let _ = span.set_parent(parent_cx);
    span
}

/// Creates the HTTP request span with Polaris-namespaced attributes.
#[cfg(not(feature = "otel"))]
fn make_http_span(
    _headers: &http::HeaderMap,
    method: &str,
    path: &str,
    request_id: &str,
) -> tracing::Span {
    tracing::info_span!(
        target: "polaris::http",
        "polaris.http.request",
        polaris.http.method = method,
        polaris.http.path = path,
        polaris.http.status_code = tracing::field::Empty,
        polaris.http.request_id = %request_id,
    )
}

/// Adapter that lets `opentelemetry`'s text-map propagator read from an
/// [`http::HeaderMap`] without depending on the `opentelemetry-http` crate.
#[cfg(feature = "otel")]
struct HeaderExtractor<'a>(&'a http::HeaderMap);

#[cfg(feature = "otel")]
impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(HeaderName::as_str).collect()
    }
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
/// Resolution order:
/// 1. If explicit origins are configured, allow only those.
/// 2. If `with_allow_any_cors_origin()` was called, allow any origin.
/// 3. If an [`AuthProvider`] is registered, panic — wildcard CORS would
///    expose authenticated endpoints cross-origin without the operator
///    explicitly opting in.
/// 4. Otherwise (no origins, no auth) emit a warning and fall back to
///    `AllowOrigin::any()` so unauthenticated demo / dev paths keep working.
fn build_cors_layer(config: &AppConfig, has_auth: bool) -> CorsLayer {
    let origins = config.cors_origins();

    let allow_origin = if !origins.is_empty() {
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
    } else if config.allow_any_cors_origin() {
        AllowOrigin::any()
    } else if has_auth {
        panic!(
            "AppConfig has no CORS origins configured and an AuthProvider is registered — \
             refusing to start with `Access-Control-Allow-Origin: *` on authenticated routes. \
             Configure explicit origins via `with_cors_origin(..)` or call \
             `with_allow_any_cors_origin()` to opt in deliberately."
        );
    } else {
        tracing::warn!(
            "AppConfig has no CORS origins configured; defaulting to `Access-Control-Allow-Origin: *`. \
             Set explicit origins via `with_cors_origin(..)` for production, or call \
             `with_allow_any_cors_origin()` to silence this warning."
        );
        AllowOrigin::any()
    };

    // `Authorization` is allowed so a cross-origin SPA can forward a bearer
    // token to an `AuthProvider`-protected backend. The browser includes it in
    // the CORS preflight whenever the SPA sets it on a fetch — omitting it
    // here would cause the preflight to fail before the request lands.
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods(tower_http::cors::Any)
        .allow_headers([CONTENT_TYPE, AUTHORIZATION])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_without_auth_allows_any_origin_with_warning() {
        let config = AppConfig::default();
        let _cors = build_cors_layer(&config, false);
    }

    #[test]
    fn explicit_opt_in_allows_any_origin_even_with_auth() {
        let config = AppConfig::new().with_allow_any_cors_origin();
        let _cors = build_cors_layer(&config, true);
    }

    #[test]
    fn specific_origins_are_accepted() {
        let config = AppConfig::new()
            .with_cors_origin("http://localhost:3000")
            .with_cors_origin("https://example.com");
        let _cors = build_cors_layer(&config, false);
    }

    #[test]
    #[should_panic(expected = "AuthProvider is registered")]
    fn auth_without_explicit_origins_panics() {
        let config = AppConfig::default();
        let _ = build_cors_layer(&config, true);
    }

    #[test]
    fn auth_with_explicit_origins_is_accepted() {
        let config = AppConfig::new().with_cors_origin("https://example.com");
        let _cors = build_cors_layer(&config, true);
    }

    #[test]
    fn empty_allowlist_treats_no_path_as_public() {
        let allowlist = PublicAllowlist::from_config(&AppConfig::default());
        assert!(!allowlist.is_public("/healthz"));
        assert!(!allowlist.is_public("/"));
    }

    #[test]
    fn exact_path_matches_public_path() {
        let config = AppConfig::new().with_public_path("/healthz");
        let allowlist = PublicAllowlist::from_config(&config);
        assert!(allowlist.is_public("/healthz"));
        assert!(!allowlist.is_public("/healthz/extra"));
        assert!(!allowlist.is_public("/health"));
    }

    #[test]
    fn prefix_matches_public_prefix() {
        let config = AppConfig::new().with_public_prefix("/dashboard/");
        let allowlist = PublicAllowlist::from_config(&config);
        assert!(allowlist.is_public("/dashboard/"));
        assert!(allowlist.is_public("/dashboard/index.html"));
        assert!(allowlist.is_public("/dashboard/assets/app.js"));
        // Trailing-slash discipline: prefix without slash would match
        // "/dashboard-attack". Prefix with slash protects against that.
        assert!(!allowlist.is_public("/dashboard-attack"));
        assert!(!allowlist.is_public("/dashboar"));
    }

    #[test]
    fn either_match_makes_path_public() {
        let config = AppConfig::new()
            .with_public_path("/healthz")
            .with_public_prefix("/dashboard/");
        let allowlist = PublicAllowlist::from_config(&config);
        assert!(allowlist.is_public("/healthz"));
        assert!(allowlist.is_public("/dashboard/index.html"));
        assert!(!allowlist.is_public("/v1/sessions"));
    }

    #[test]
    fn empty_path_is_not_public() {
        // Belt-and-suspenders: in practice `parts.uri.path()` always yields
        // at least "/", but pin the assumption that an empty literal never
        // accidentally matches.
        let config = AppConfig::new()
            .with_public_path("/healthz")
            .with_public_prefix("/dashboard/");
        let allowlist = PublicAllowlist::from_config(&config);
        assert!(!allowlist.is_public(""));
    }

    #[test]
    #[should_panic(expected = "must end with '/'")]
    fn prefix_without_trailing_slash_is_rejected() {
        // `with_public_prefix("/dashboard")` (no trailing slash) used to be
        // accepted and would match `/dashboard-attack` — the trailing-slash
        // discipline is now enforced at config time. Operators wanting an
        // exact-match exemption should reach for `with_public_path` instead.
        let _ = AppConfig::new().with_public_prefix("/dashboard");
    }

    #[test]
    fn literal_question_mark_does_not_match_allowlist() {
        // `parts.uri.path()` strips the query string before reaching
        // `is_public`, so the matcher never sees `?…` in real requests. If a
        // caller ever passes a raw URI with `?` baked in, it must not be
        // treated as the canonical path. Pin both invariants:
        //   1. `is_public("/healthz?probe=1")` is false (literal mismatch on exact path)
        //   2. `is_public("/healthz")` is true (the post-strip form)
        let config = AppConfig::new().with_public_path("/healthz");
        let allowlist = PublicAllowlist::from_config(&config);
        assert!(allowlist.is_public("/healthz"));
        assert!(!allowlist.is_public("/healthz?probe=1"));
    }
}
