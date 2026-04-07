//! Route registration API for plugins.
//!
//! [`HttpRouter`] is a build-time API that plugins use to register axum route
//! fragments. [`AppPlugin`](crate::AppPlugin) merges all registered fragments
//! in `ready()` before starting the HTTP server.
//!
//! # Example
//!
//! ```no_run
//! use polaris_system::plugin::{Plugin, PluginId, Version};
//! use polaris_system::server::Server;
//! use polaris_app::{AppPlugin, HttpRouter};
//! use axum::{Router, routing::get};
//!
//! struct HealthPlugin;
//!
//! impl Plugin for HealthPlugin {
//!     const ID: &'static str = "myapp::health";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn build(&self, server: &mut Server) {
//!         let router = Router::new()
//!             .route("/healthz", get(|| async { "ok" }));
//!         server.api::<HttpRouter>()
//!             .expect("AppPlugin must be added first")
//!             .add_routes(router);
//!     }
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<AppPlugin>()]
//!     }
//! }
//! ```

use crate::auth::AuthProvider;
use parking_lot::RwLock;
use polaris_system::api::API;
use std::sync::Arc;

/// Build-time API for registering HTTP routes.
///
/// Plugins call [`add_routes`](HttpRouter::add_routes) during their `build()`
/// phase to contribute route fragments. [`AppPlugin`](crate::AppPlugin) merges
/// all fragments in `ready()`.
///
/// Uses interior mutability (`RwLock`) so `server.api::<HttpRouter>()` returns
/// `&HttpRouter` while still allowing registration.
pub struct HttpRouter {
    routes: RwLock<Vec<axum::Router>>,
    auth: RwLock<Option<Arc<dyn AuthProvider>>>,
}

impl std::fmt::Debug for HttpRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRouter")
            .field("route_count", &self.routes.read().len())
            .field("auth", &self.auth.read().as_ref().map(|a| format!("{a:?}")))
            .finish()
    }
}

impl API for HttpRouter {}

impl HttpRouter {
    /// Creates a new empty router registry.
    pub(crate) fn new() -> Self {
        Self {
            routes: RwLock::new(Vec::new()),
            auth: RwLock::new(None),
        }
    }

    /// Registers an axum [`Router`](axum::Router) fragment.
    ///
    /// Call this during your plugin's `build()` phase. All fragments are
    /// merged into a single router when [`AppPlugin`](crate::AppPlugin)
    /// enters `ready()`.
    pub fn add_routes(&self, router: axum::Router) {
        self.routes.write().push(router);
    }

    /// Sets the authentication provider for all routes.
    ///
    /// Call this during your plugin's `build()` phase. Only one provider
    /// can be active — calling this again replaces the previous one.
    /// [`AppPlugin`](crate::AppPlugin) applies the provider as middleware
    /// in `ready()`.
    pub fn set_auth(&self, provider: impl AuthProvider) {
        let mut guard = self.auth.write();
        if guard.is_some() {
            tracing::warn!("overwriting previously registered AuthProvider");
        }
        *guard = Some(Arc::new(provider));
    }

    /// Takes all registered route fragments, leaving the registry empty.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_routes(&self) -> Vec<axum::Router> {
        std::mem::take(&mut *self.routes.write())
    }

    /// Takes the registered auth provider, if any.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_auth(&self) -> Option<Arc<dyn AuthProvider>> {
        self.auth.write().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    #[test]
    fn register_and_take_routes() {
        let api = HttpRouter::new();

        api.add_routes(axum::Router::new().route("/a", get(|| async { "a" })));
        api.add_routes(axum::Router::new().route("/b", get(|| async { "b" })));

        let routes = api.take_routes();
        assert_eq!(routes.len(), 2);

        // After take, registry is empty
        let routes = api.take_routes();
        assert!(routes.is_empty());
    }
}
