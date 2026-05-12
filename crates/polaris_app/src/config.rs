//! HTTP server configuration.

use crate::public_route::{PublicPath, PublicPrefix};
use polaris_system::resource::GlobalResource;
use std::net::SocketAddr;

/// Configuration for the HTTP server.
///
/// Registered as a [`GlobalResource`] by [`AppPlugin`](crate::AppPlugin).
/// Read-only after server startup.
///
/// # Example
///
/// ```no_run
/// use polaris_app::AppConfig;
///
/// let config = AppConfig::new()
///     .with_host("0.0.0.0")
///     .with_port(8080)
///     .with_cors_origin("http://localhost:3000")
///     .with_public_path("/healthz")
///     .with_public_prefix("/dashboard/");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    /// Host address to bind to.
    host: String,
    /// Port to listen on.
    port: u16,
    /// Allowed CORS origins. Empty means no explicit list.
    cors_origins: Vec<String>,
    /// Whether `Access-Control-Allow-Origin: *` is explicitly opted into.
    allow_any_cors_origin: bool,
    /// Exact request paths exempt from `AuthProvider`.
    public_paths: Vec<PublicPath>,
    /// Path prefixes exempt from `AuthProvider`.
    public_prefixes: Vec<PublicPrefix>,
}

impl GlobalResource for AppConfig {}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
            cors_origins: Vec::new(),
            allow_any_cors_origin: false,
            public_paths: Vec::new(),
            public_prefixes: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Creates a new config with default settings (127.0.0.1:3000).
    ///
    /// # Security
    ///
    /// By default, no CORS origins are configured. The middleware emits a
    /// warning at startup and falls back to `Access-Control-Allow-Origin: *`
    /// only when no [`AuthProvider`](crate::AuthProvider) is registered. If
    /// an auth provider *is* registered without an explicit origin list,
    /// startup panics rather than silently exposing authenticated endpoints
    /// cross-origin. To opt into wildcard CORS deliberately, call
    /// [`AppConfig::with_allow_any_cors_origin`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the host address.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Sets the port.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Adds a CORS allowed origin.
    #[must_use]
    pub fn with_cors_origin(mut self, origin: impl Into<String>) -> Self {
        self.cors_origins.push(origin.into());
        self
    }

    /// Explicitly opts into `Access-Control-Allow-Origin: *`.
    ///
    /// Use this only for genuinely public APIs. When combined with an
    /// [`AuthProvider`](crate::AuthProvider), this allows browsers on any
    /// origin to invoke authenticated endpoints — make sure that is the
    /// intended exposure before opting in.
    #[must_use]
    pub fn with_allow_any_cors_origin(mut self) -> Self {
        self.allow_any_cors_origin = true;
        self
    }

    /// Exempts an exact request path from [`AuthProvider`](crate::AuthProvider).
    ///
    /// Useful for endpoints that must be reachable before authentication can
    /// even be attempted: load-balancer health checks, login pages, OAuth
    /// callback URLs. Path comparison is exact — for hierarchies, see
    /// [`with_public_prefix`](Self::with_public_prefix).
    ///
    /// Path-based exemption belongs at the config layer rather than inside an
    /// `AuthProvider` impl, because every auth scheme has public routes and
    /// embedding routing decisions in the auth trait conflates two concerns.
    ///
    /// Accepts anything convertible into [`PublicPath`] — `&str`, `String`,
    /// or a pre-validated [`PublicPath`]. The conversion calls
    /// [`PublicPath::new`] internally; use that constructor directly if you
    /// prefer to handle invalid input as a [`Result`] rather than a panic.
    ///
    /// # Panics
    ///
    /// Panics if the input is empty or does not start with `/`. Validating
    /// eagerly surfaces misconfiguration (e.g. `with_public_path(env_var)`
    /// where the variable is unset) at startup rather than silently exposing
    /// authenticated endpoints.
    #[must_use]
    pub fn with_public_path(mut self, path: impl Into<String>) -> Self {
        let path = PublicPath::new(path).unwrap_or_else(|err| {
            panic!("AppConfig::with_public_path: {err}");
        });
        self.public_paths.push(path);
        self
    }

    /// Exempts a request-path prefix from [`AuthProvider`](crate::AuthProvider).
    ///
    /// Use for hierarchical exemptions like a static-asset mount
    /// (`/dashboard/`) where every path under it should bypass auth. The
    /// prefix matches as a string against [`http::Uri::path`]; a trailing
    /// slash is required so `"/dashboard/"` does not match
    /// `/dashboard-attack`. For an exact-match exemption (e.g. `/healthz`)
    /// use [`with_public_path`](Self::with_public_path) instead.
    ///
    /// Accepts anything convertible into [`PublicPrefix`] — `&str`,
    /// `String`, or a pre-validated [`PublicPrefix`]. The conversion calls
    /// [`PublicPrefix::new`] internally; use that constructor directly if
    /// you prefer to handle invalid input as a [`Result`] rather than a
    /// panic.
    ///
    /// # Panics
    ///
    /// Panics if the input is empty, does not start with `/`, or does not
    /// end with `/`. An empty prefix would make every request public
    /// (`str::starts_with("")` is always `true`), silently disabling the
    /// [`AuthProvider`](crate::AuthProvider); a prefix without a trailing
    /// slash matches sibling paths (`"/dashboard"` matches
    /// `/dashboard-attack`). Validating eagerly surfaces both
    /// misconfigurations at startup rather than at request time.
    #[must_use]
    pub fn with_public_prefix(mut self, prefix: impl Into<String>) -> Self {
        let prefix = PublicPrefix::new(prefix).unwrap_or_else(|err| {
            panic!("AppConfig::with_public_prefix: {err}");
        });
        self.public_prefixes.push(prefix);
        self
    }

    /// Returns the configured host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the configured port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns the configured CORS origins.
    #[must_use]
    pub fn cors_origins(&self) -> &[String] {
        &self.cors_origins
    }

    /// Returns whether wildcard CORS has been explicitly opted into.
    #[must_use]
    pub fn allow_any_cors_origin(&self) -> bool {
        self.allow_any_cors_origin
    }

    /// Returns the configured exact public paths (auth-exempt).
    #[must_use]
    pub fn public_paths(&self) -> &[PublicPath] {
        &self.public_paths
    }

    /// Returns the configured public path prefixes (auth-exempt).
    #[must_use]
    pub fn public_prefixes(&self) -> &[PublicPrefix] {
        &self.public_prefixes
    }

    /// Returns the parsed socket address.
    ///
    /// # Panics
    ///
    /// Panics if `host` is not a valid IP address. This validates eagerly so
    /// configuration errors surface at startup rather than at bind time.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_app::AppConfig;
    ///
    /// let config = AppConfig::new().with_host("0.0.0.0").with_port(8080);
    /// let addr = config.addr();
    /// assert_eq!(addr.port(), 8080);
    /// ```
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        let raw = format!("{}:{}", self.host, self.port);
        raw.parse()
            .expect("AppConfig host must be a valid IP address")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "AppConfig::with_public_prefix: must not be empty")]
    fn with_public_prefix_rejects_empty() {
        let _ = AppConfig::new().with_public_prefix("");
    }

    #[test]
    #[should_panic(expected = "AppConfig::with_public_prefix: must start with '/'")]
    fn with_public_prefix_rejects_missing_leading_slash() {
        let _ = AppConfig::new().with_public_prefix("dashboard");
    }

    #[test]
    #[should_panic(expected = "AppConfig::with_public_path: must not be empty")]
    fn with_public_path_rejects_empty() {
        let _ = AppConfig::new().with_public_path("");
    }

    #[test]
    #[should_panic(expected = "AppConfig::with_public_path: must start with '/'")]
    fn with_public_path_rejects_missing_leading_slash() {
        let _ = AppConfig::new().with_public_path("healthz");
    }

    #[test]
    #[should_panic(expected = "must end with '/'")]
    fn with_public_prefix_rejects_missing_trailing_slash() {
        let _ = AppConfig::new().with_public_prefix("/dashboard");
    }

    #[test]
    fn valid_public_path_and_prefix_are_accepted() {
        let config = AppConfig::new()
            .with_public_path("/healthz")
            .with_public_prefix("/dashboard/");
        assert_eq!(config.public_paths().len(), 1);
        assert_eq!(config.public_paths()[0].as_str(), "/healthz");
        assert_eq!(config.public_prefixes().len(), 1);
        assert_eq!(config.public_prefixes()[0].as_str(), "/dashboard/");
    }
}
