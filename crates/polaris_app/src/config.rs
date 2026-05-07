//! HTTP server configuration.

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
///     .with_cors_origin("http://localhost:3000");
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
}

impl GlobalResource for AppConfig {}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
            cors_origins: Vec::new(),
            allow_any_cors_origin: false,
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
