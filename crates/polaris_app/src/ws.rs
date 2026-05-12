//! WebSocket route registration API for plugins.
//!
//! [`WsRouter`] is a build-time API that plugins use to register axum route
//! fragments containing WebSocket upgrade handlers. [`AppPlugin`](crate::AppPlugin)
//! merges all registered fragments in `ready()` before starting the server.
//!
//! WebSocket routes go through the same middleware stack (CORS, tracing,
//! request ID, auth) as regular HTTP routes because they are merged into the
//! main axum router before middleware is applied. This means
//! [`AuthProvider`](crate::AuthProvider) validates WebSocket upgrade requests
//! just like REST requests.
//!
//! Authentication is handled by the existing
//! [`HttpRouter::set_auth`](crate::HttpRouter::set_auth) -- `WsRouter` only
//! provides route registration.
//!
//! # Example
//!
//! ```no_run
//! use polaris_system::plugin::{Plugin, PluginId, Version};
//! use polaris_system::server::Server;
//! use polaris_app::{AppPlugin, WsRouter};
//! use axum::{Router, routing::get, extract::ws::{WebSocketUpgrade, WebSocket}};
//! use axum::response::IntoResponse;
//!
//! struct EchoWsPlugin;
//!
//! async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
//!     ws.on_upgrade(handle_socket)
//! }
//!
//! async fn handle_socket(mut socket: WebSocket) {
//!     // echo logic
//! }
//!
//! impl Plugin for EchoWsPlugin {
//!     const ID: &'static str = "myapp::echo_ws";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn build(&self, server: &mut Server) {
//!         let router = Router::new()
//!             .route("/ws/echo", get(ws_handler));
//!         server.api::<WsRouter>()
//!             .expect("AppPlugin with `ws` feature must be added first")
//!             .add_routes(router);
//!     }
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<AppPlugin>()]
//!     }
//! }
//! ```

use parking_lot::RwLock;
use polaris_system::api::API;

/// Build-time API for registering WebSocket routes.
///
/// Plugins call [`add_routes`](WsRouter::add_routes) during their `build()`
/// phase to contribute route fragments that contain WebSocket upgrade handlers.
/// [`AppPlugin`](crate::AppPlugin) merges all fragments in `ready()` alongside
/// HTTP route fragments, before applying the middleware stack.
///
/// Uses interior mutability (`RwLock`) so `server.api::<WsRouter>()` returns
/// `&WsRouter` while still allowing registration.
///
/// Authentication for WebSocket upgrade requests is handled by the existing
/// [`HttpRouter::set_auth`](crate::HttpRouter::set_auth) mechanism -- there is
/// no separate auth on `WsRouter`.
pub struct WsRouter {
    routes: RwLock<Vec<axum::Router>>,
}

impl std::fmt::Debug for WsRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsRouter")
            .field("route_count", &self.routes.read().len())
            .finish()
    }
}

impl API for WsRouter {}

impl WsRouter {
    /// Creates a new empty WebSocket router registry.
    pub(crate) fn new() -> Self {
        Self {
            routes: RwLock::new(Vec::new()),
        }
    }

    /// Registers an axum [`Router`](axum::Router) fragment containing WebSocket
    /// upgrade handlers.
    ///
    /// Call this during your plugin's `build()` phase. All fragments are
    /// merged into the main router when [`AppPlugin`](crate::AppPlugin)
    /// enters `ready()`, before middleware is applied.
    pub fn add_routes(&self, router: axum::Router) {
        self.routes.write().push(router);
    }

    /// Takes all registered route fragments, leaving the registry empty.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_routes(&self) -> Vec<axum::Router> {
        std::mem::take(&mut *self.routes.write())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    #[test]
    fn register_and_take_routes() {
        let api = WsRouter::new();

        api.add_routes(axum::Router::new().route("/ws/a", get(|| async { "a" })));
        api.add_routes(axum::Router::new().route("/ws/b", get(|| async { "b" })));

        let routes = api.take_routes();
        assert_eq!(routes.len(), 2);

        // After take, registry is empty
        let routes = api.take_routes();
        assert!(routes.is_empty());
    }
}
