//! [`AppPlugin`] — HTTP server lifecycle management.
//!
//! Registers [`AppConfig`] and [`HttpRouter`], then starts an axum server
//! in `ready()` with all routes merged and middleware applied.

use crate::config::AppConfig;
use crate::middleware;
use crate::router::HttpRouter;
use polaris_system::api::API;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use tokio::sync::watch;

/// Runtime handle for the HTTP server.
///
/// Registered as an [`API`] during `ready()`. Other plugins can access it
/// via `server.api::<ServerHandle>()` to trigger a graceful shutdown.
pub struct ServerHandle {
    /// Shutdown signal sender.
    shutdown_tx: parking_lot::Mutex<Option<watch::Sender<bool>>>,
    /// Server task join handle.
    handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerHandle")
            .field("running", &self.handle.lock().is_some())
            .finish()
    }
}

impl API for ServerHandle {}

impl ServerHandle {
    /// Sends the shutdown signal to the HTTP server.
    ///
    /// Returns `true` if the signal was sent, `false` if the server was
    /// already shut down or never started.
    pub fn shutdown(&self) -> bool {
        if let Some(tx) = self.shutdown_tx.lock().take() {
            let _ = tx.send(true);
            true
        } else {
            false
        }
    }
}

/// Shared HTTP server runtime for Polaris.
///
/// Provides an axum-based HTTP server that other plugins extend with routes.
/// Plugins register route fragments via [`HttpRouter`] during `build()`.
/// The server merges all routes, applies Tower middleware, and starts
/// listening in `ready()`. Graceful shutdown occurs in `cleanup()`.
///
/// # Lifecycle
///
/// - **`build()`** — inserts [`AppConfig`] as a global resource, registers
///   [`HttpRouter`] as a build-time API.
/// - **`ready()`** — merges all registered routes, applies middleware
///   (CORS, tracing, request ID, optional auth), spawns the axum server,
///   and registers [`ServerHandle`] as a build-time API.
/// - **`cleanup()`** — sends shutdown signal via [`ServerHandle`] and awaits
///   graceful drain (5-second timeout).
///
/// # Example
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_app::{AppPlugin, AppConfig};
///
/// let mut server = Server::new();
/// server.add_plugins(
///     AppPlugin::new(AppConfig::new().with_port(8080))
/// );
/// ```
pub struct AppPlugin {
    config: AppConfig,
    listener: parking_lot::Mutex<Option<tokio::net::TcpListener>>,
}

impl std::fmt::Debug for AppPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppPlugin")
            .field("config", &self.config)
            .field("has_listener", &self.listener.lock().is_some())
            .finish()
    }
}

impl AppPlugin {
    /// Creates a new plugin with the given configuration.
    #[must_use]
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            listener: parking_lot::Mutex::new(None),
        }
    }

    /// Provides a pre-bound [`TcpListener`](tokio::net::TcpListener) for the
    /// server to use instead of binding from [`AppConfig`].
    ///
    /// This is useful in tests to avoid TOCTOU races when discovering an
    /// available port: bind to port `0`, read the assigned port, and pass
    /// the listener here so the port stays reserved.
    #[must_use]
    pub fn with_listener(self, listener: tokio::net::TcpListener) -> Self {
        *self.listener.lock() = Some(listener);
        self
    }
}

impl Plugin for AppPlugin {
    const ID: &'static str = "polaris::app";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_global(self.config.clone());
        server.insert_api(HttpRouter::new());
    }

    async fn ready(&self, server: &mut Server) {
        let router_api = server
            .api::<HttpRouter>()
            .expect("HttpRouter API must exist (registered in build)");

        // Collect route fragments and auth provider registered by plugins
        let fragments = router_api.take_routes();
        let auth = router_api.take_auth();

        let mut app = axum::Router::new();
        for fragment in fragments {
            app = app.merge(fragment);
        }

        // Apply middleware stack (including auth if registered).
        // Scope the config borrow so it's dropped before insert_api.
        let (app, addr) = {
            let config = server
                .get_global::<AppConfig>()
                .expect("AppConfig must exist (registered in build)");
            let app = middleware::apply_middleware(app, &config, auth);
            let addr = config.addr();
            (app, addr)
        };

        // Shutdown channel
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Use a pre-bound listener if one was provided, otherwise bind now.
        let injected_listener = self.listener.lock().take();

        // Spawn server on background task
        let addr_for_log = addr;
        let handle = tokio::spawn(async move {
            let listener = if let Some(listener) = injected_listener {
                tracing::info!(
                    addr = %listener.local_addr().expect("listener must have local addr"),
                    "HTTP server listening (pre-bound)"
                );
                listener
            } else {
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        tracing::info!(addr = %addr, "HTTP server listening");
                        listener
                    }
                    Err(bind_err) => {
                        tracing::error!(
                            addr = %addr,
                            error = %bind_err,
                            "failed to bind HTTP server — no routes will be served. \
                             Check that the port is not already in use."
                        );
                        return;
                    }
                }
            };

            let shutdown_signal = create_shutdown_signal(shutdown_rx);

            if let Err(serve_err) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal)
                .await
            {
                tracing::error!(
                    addr = %addr_for_log,
                    error = %serve_err,
                    "HTTP server error"
                );
            }
        });

        // Register runtime state as an API (plugin-only access)
        server.insert_api(ServerHandle {
            shutdown_tx: parking_lot::Mutex::new(Some(shutdown_tx)),
            handle: parking_lot::Mutex::new(Some(handle)),
        });
    }

    async fn cleanup(&self, server: &mut Server) {
        let Some(server_handle) = server.api::<ServerHandle>() else {
            return;
        };

        // Signal graceful shutdown
        server_handle.shutdown();

        // Take the join handle out of the lock before awaiting
        let handle = server_handle.handle.lock().take();

        if let Some(handle) = handle {
            let timeout = std::time::Duration::from_secs(5);
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => tracing::info!("HTTP server shut down gracefully"),
                Ok(Err(join_err)) => {
                    tracing::warn!(error = %join_err, "HTTP server task panicked");
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = timeout.as_secs(),
                        "HTTP server shutdown timed out"
                    );
                }
            }
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        Vec::new()
    }
}

/// Creates a future that resolves when shutdown is signalled.
async fn create_shutdown_signal(mut rx: watch::Receiver<bool>) {
    // Wait until the sender sends `true`
    let _ = rx.wait_for(|&val| val).await;
    tracing::debug!("HTTP server shutdown signal received");
}
