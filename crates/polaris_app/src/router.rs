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
use parking_lot::{Mutex, RwLock};
use polaris_system::api::API;
use polaris_system::plugin::{Contract, Version};
use polaris_system::server::Server;
use std::sync::Arc;

/// Deferred router builder: runs during [`AppPlugin`](crate::AppPlugin)'s
/// `ready()` phase against a fully-initialized [`Server`].
pub(crate) type RouteBuilder = Box<dyn FnOnce(&Server) -> axum::Router + Send>;

/// Build-time API for registering HTTP routes.
///
/// Plugins call [`add_routes`](HttpRouter::add_routes) during their `build()`
/// phase to contribute stateless route fragments, or
/// [`add_routes_with`](HttpRouter::add_routes_with) to defer construction
/// until every plugin has registered its APIs.
/// [`AppPlugin`](crate::AppPlugin) merges all fragments in `ready()`.
///
/// Uses interior mutability (`RwLock`) so `server.api::<HttpRouter>()` returns
/// `&HttpRouter` while still allowing registration.
pub struct HttpRouter {
    routes: RwLock<Vec<axum::Router>>,
    // `Mutex` (not `RwLock`) because `dyn FnOnce` is `Send` but not `Sync`.
    builders: Mutex<Vec<RouteBuilder>>,
    auth: RwLock<Option<Arc<dyn AuthProvider>>>,
}

impl std::fmt::Debug for HttpRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRouter")
            .field("route_count", &self.routes.read().len())
            .field("builder_count", &self.builders.lock().len())
            .field("auth", &self.auth.read().as_ref().map(|a| format!("{a:?}")))
            .finish()
    }
}

impl API for HttpRouter {}

/// The contract version at which [`HttpRouter`] is exposed as a capability. Plugins that
/// mount routes (e.g. `HttpPlugin`) declare a requirement against this version; bump it
/// when the route-registration surface changes incompatibly.
impl Contract for HttpRouter {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl HttpRouter {
    /// Creates a new empty router registry.
    pub(crate) fn new() -> Self {
        Self {
            routes: RwLock::new(Vec::new()),
            builders: Mutex::new(Vec::new()),
            auth: RwLock::new(None),
        }
    }

    /// Registers a stateless axum [`Router`](axum::Router) fragment.
    ///
    /// Call this during your plugin's `build()` phase. All fragments are
    /// merged into a single router when [`AppPlugin`](crate::AppPlugin)
    /// enters `ready()`.
    ///
    /// Use [`add_routes_with`](Self::add_routes_with) when the router's
    /// state depends on APIs that other plugins register in `build()`.
    pub fn add_routes(&self, router: axum::Router) {
        self.routes.write().push(router);
    }

    /// Registers a deferred router builder that runs during
    /// [`AppPlugin`](crate::AppPlugin)'s `ready()` phase.
    ///
    /// The closure receives a fully-initialized [`Server`] — every plugin's
    /// `build()` has completed, so APIs registered by other plugins are
    /// available via `server.api::<T>()`. Use this when your router needs
    /// typed `.with_state(T)` injection for state that only materializes
    /// after `build()`.
    ///
    /// # Note
    ///
    /// Builders are drained once during [`AppPlugin`](crate::AppPlugin)'s
    /// `ready()`. Calling `add_routes` or `add_routes_with` *from inside*
    /// a builder closure has no effect — the added fragment is never
    /// merged. Register everything before returning the
    /// [`Router`](axum::Router).
    pub fn add_routes_with<F>(&self, build: F)
    where
        F: FnOnce(&Server) -> axum::Router + Send + 'static,
    {
        self.builders.lock().push(Box::new(build));
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

    /// Takes all deferred router builders, leaving the registry empty.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_builders(&self) -> Vec<RouteBuilder> {
        std::mem::take(&mut *self.builders.lock())
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

    #[test]
    fn register_and_take_builders() {
        let api = HttpRouter::new();

        api.add_routes_with(|_| axum::Router::new().route("/a", get(|| async { "a" })));
        api.add_routes_with(|_| axum::Router::new().route("/b", get(|| async { "b" })));

        let builders = api.take_builders();
        assert_eq!(builders.len(), 2);

        // After take, registry is empty
        let builders = api.take_builders();
        assert!(builders.is_empty());
    }
}
