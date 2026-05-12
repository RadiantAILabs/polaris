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
//! /// Constant-time equality on two byte slices.
//! ///
//! /// Production implementations should reach for a vetted constant-time
//! /// comparator (`subtle::ConstantTimeEq`, `ring::constant_time`,
//! /// `openssl::memcmp::eq`) rather than `==`. A short-circuiting `==`
//! /// leaks the length of the matching prefix through timing, which is
//! /// enough for a remote attacker to recover a bearer token byte by byte.
//! fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
//!     if a.len() != b.len() {
//!         return false;
//!     }
//!     let mut diff = 0u8;
//!     for (x, y) in a.iter().zip(b.iter()) {
//!         diff |= x ^ y;
//!     }
//!     diff == 0
//! }
//!
//! #[derive(Debug)]
//! struct BearerAuth {
//!     expected_header: String,
//! }
//!
//! impl BearerAuth {
//!     fn new(token: &str) -> Self {
//!         Self { expected_header: format!("Bearer {token}") }
//!     }
//! }
//!
//! impl AuthProvider for BearerAuth {
//!     fn authenticate(&self, parts: &http::request::Parts) -> Result<(), AuthRejection> {
//!         let header = parts
//!             .headers
//!             .get(http::header::AUTHORIZATION)
//!             .and_then(|v| v.to_str().ok())
//!             .unwrap_or("");
//!
//!         if constant_time_eq(header.as_bytes(), self.expected_header.as_bytes()) {
//!             Ok(())
//!         } else {
//!             Err(Box::new(StatusCode::UNAUTHORIZED.into_response()))
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
///
/// # Public routes
///
/// For path-based exemptions (health checks, login pages, static assets),
/// prefer [`AppConfig::with_public_path`](crate::AppConfig::with_public_path)
/// and [`AppConfig::with_public_prefix`](crate::AppConfig::with_public_prefix)
/// over hand-rolling matching inside the trait. The middleware consults the
/// allowlist before invoking `authenticate`, keeping path-based routing
/// decisions out of credential-validation logic.
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
