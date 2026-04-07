//! Pluggable authentication for HTTP routes.
//!
//! Implement [`AuthProvider`] and register it via
//! [`HttpRouter::set_auth`](crate::HttpRouter::set_auth) to add authentication
//! middleware to all routes served by [`AppPlugin`](crate::AppPlugin).
//!
//! # Example
//!
//! ```no_run
//! use polaris_app::{AuthProvider, auth::AuthRejection};
//! use axum::response::IntoResponse;
//! use http::StatusCode;
//!
//! #[derive(Debug)]
//! struct BearerAuth {
//!     expected_token: String,
//! }
//!
//! impl AuthProvider for BearerAuth {
//!     fn authenticate(&self, parts: &http::request::Parts) -> Result<(), AuthRejection> {
//!         let header = parts
//!             .headers
//!             .get(http::header::AUTHORIZATION)
//!             .and_then(|v| v.to_str().ok());
//!
//!         match header {
//!             Some(val) if val == format!("Bearer {}", self.expected_token) => Ok(()),
//!             _ => Err(Box::new(StatusCode::UNAUTHORIZED.into_response())),
//!         }
//!     }
//! }
//! ```

/// The rejection type returned by [`AuthProvider::authenticate`].
///
/// Wraps an [`axum::response::Response`] in a `Box` to keep the `Result`
/// return value small on the stack.
pub type AuthRejection = Box<axum::response::Response>;

/// Trait for pluggable request authentication.
///
/// Plugins implement this trait and register it via
/// [`HttpRouter::set_auth`](crate::HttpRouter::set_auth) during `build()`.
/// [`AppPlugin`](crate::AppPlugin) applies the provider as middleware in
/// `ready()`, rejecting unauthenticated requests before they reach route
/// handlers.
///
/// Authentication is intentionally synchronous — most schemes (bearer tokens,
/// API keys, HMAC signatures) only need to inspect request headers. If your
/// scheme requires async I/O (e.g., remote token introspection), perform the
/// lookup in a Tower layer registered directly via
/// [`HttpRouter::add_routes`](crate::HttpRouter::add_routes) instead.
pub trait AuthProvider: Send + Sync + std::fmt::Debug + 'static {
    /// Validates request authentication.
    ///
    /// Returns `Ok(())` to allow the request through, or `Err(rejection)` to
    /// reject it. The rejection response is sent directly to the client — use
    /// an appropriate status code (e.g., `401 Unauthorized`, `403 Forbidden`).
    ///
    /// # Errors
    ///
    /// Returns an [`AuthRejection`] containing the HTTP response to send
    /// when authentication fails.
    fn authenticate(&self, parts: &http::request::Parts) -> Result<(), AuthRejection>;
}
