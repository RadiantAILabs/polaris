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
/// Routes that expose private metadata or administrative controls should use
/// [`add_protected_routes`](Self::add_protected_routes) or
/// [`add_protected_routes_with`](Self::add_protected_routes_with); those
/// fragments are mounted only behind [`AuthProvider`] and ignore public-path
/// allowlist exemptions.
/// [`AppPlugin`](crate::AppPlugin) merges all fragments in `ready()`.
///
/// Uses interior mutability (`RwLock`) so `server.api::<HttpRouter>()` returns
/// `&HttpRouter` while still allowing registration.
///
/// # Provided by
///
/// [`AppPlugin`](crate::AppPlugin), via `insert_api` during its `build()`
/// phase.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`add_routes`](Self::add_routes) | Registers a stateless public or normally-authenticated router fragment. |
/// | [`add_routes_with`](Self::add_routes_with) | Defers route construction until every plugin has completed `build()`. |
/// | [`add_protected_routes`](Self::add_protected_routes) | Registers routes that always require authentication and ignore public allowlists. |
/// | [`add_protected_routes_with`](Self::add_protected_routes_with) | Deferred counterpart for protected routes. |
/// | [`set_auth`](Self::set_auth) | Sets the single authentication provider used by the app. |
///
/// # Lifecycle
///
/// Route and auth registration belongs in consumer plugins' `build()` phase.
/// [`AppPlugin`](crate::AppPlugin) drains the registry once during `ready()`,
/// runs deferred builders against the fully built [`Server`], applies
/// middleware, and starts serving. Registrations made after that drain are not
/// served.
///
/// # Composition
///
/// **Open extension for routes, single-replace for auth.** Any plugin may add
/// route fragments through `&self`; one call to [`set_auth`](Self::set_auth)
/// replaces any previously configured provider and logs a warning.
///
/// # Example consumers
///
/// - The sessions HTTP plugin registers its REST surface, including protected
///   agent metadata.
/// - Application plugins register their own axum route fragments during
///   `build()`.
///
/// # Example
///
/// ```no_run
/// use axum::{Router, routing::get};
/// use polaris_app::{AppConfig, AppPlugin, HttpRouter};
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server.add_plugins(AppPlugin::new(AppConfig::new().with_port(8080)));
///
/// struct HealthPlugin;
/// impl Plugin for HealthPlugin {
///     const ID: &'static str = "myapp::health";
///     const VERSION: Version = Version::new(0, 1, 0);
///
///     fn build(&self, server: &mut Server) {
///         server
///             .api::<HttpRouter>()
///             .expect("AppPlugin provides HttpRouter")
///             .add_routes(Router::new().route("/healthz", get(|| async { "ok" })));
///     }
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<AppPlugin>()]
///     }
/// }
/// server.add_plugins(HealthPlugin);
/// ```
pub struct HttpRouter {
    routes: RwLock<Vec<axum::Router>>,
    protected_routes: RwLock<Vec<axum::Router>>,
    // `Mutex` (not `RwLock`) because `dyn FnOnce` is `Send` but not `Sync`.
    builders: Mutex<Vec<RouteBuilder>>,
    protected_builders: Mutex<Vec<RouteBuilder>>,
    auth: RwLock<Option<Arc<dyn AuthProvider>>>,
}

impl std::fmt::Debug for HttpRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRouter")
            .field("route_count", &self.routes.read().len())
            .field("protected_route_count", &self.protected_routes.read().len())
            .field("builder_count", &self.builders.lock().len())
            .field(
                "protected_builder_count",
                &self.protected_builders.lock().len(),
            )
            .field("auth", &self.auth.read().as_ref().map(|a| format!("{a:?}")))
            .finish()
    }
}

impl API for HttpRouter {}

/// The contract version at which [`HttpRouter`] is exposed as a capability. Plugins that
/// mount routes (e.g. `HttpPlugin`) declare a requirement against this version; bump it
/// when the route-registration surface changes incompatibly.
impl Contract for HttpRouter {
    /// 0.1.1 adds protected route fragments
    /// (`add_protected_routes`, `add_protected_routes_with`).
    const CONTRACT_VERSION: Version = Version::new(0, 1, 1);
}

impl HttpRouter {
    /// Creates a new empty router registry.
    pub(crate) fn new() -> Self {
        Self {
            routes: RwLock::new(Vec::new()),
            protected_routes: RwLock::new(Vec::new()),
            builders: Mutex::new(Vec::new()),
            protected_builders: Mutex::new(Vec::new()),
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

    /// Registers a stateless route fragment that must never bypass
    /// [`AuthProvider`] checks.
    ///
    /// Protected fragments receive the complete middleware stack with a
    /// required authentication check. Unlike ordinary routes, they do not
    /// consult [`AppConfig`](crate::AppConfig)'s public-path allowlist, so a
    /// broad `with_public_prefix` cannot accidentally expose them. If no
    /// [`AuthProvider`] is registered, [`AppPlugin`](crate::AppPlugin) does
    /// not mount protected fragments.
    ///
    /// Use this for routes that expose private implementation details,
    /// privileged control surfaces, or tenant-specific data.
    pub fn add_protected_routes(&self, router: axum::Router) {
        self.protected_routes.write().push(router);
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

    /// Registers a deferred protected route builder.
    ///
    /// This is the protected-route counterpart to
    /// [`add_routes_with`](Self::add_routes_with): the closure runs during
    /// [`AppPlugin`](crate::AppPlugin)'s `ready()` phase against a
    /// fully-initialized [`Server`], and the returned routes are mounted only
    /// behind an [`AuthProvider`] check that ignores public-path exemptions.
    pub fn add_protected_routes_with<F>(&self, build: F)
    where
        F: FnOnce(&Server) -> axum::Router + Send + 'static,
    {
        self.protected_builders.lock().push(Box::new(build));
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

    /// Takes all registered protected route fragments.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_protected_routes(&self) -> Vec<axum::Router> {
        std::mem::take(&mut *self.protected_routes.write())
    }

    /// Takes all deferred router builders, leaving the registry empty.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_builders(&self) -> Vec<RouteBuilder> {
        std::mem::take(&mut *self.builders.lock())
    }

    /// Takes all deferred protected router builders.
    ///
    /// Called by [`AppPlugin`](crate::AppPlugin) during `ready()`.
    pub(crate) fn take_protected_builders(&self) -> Vec<RouteBuilder> {
        std::mem::take(&mut *self.protected_builders.lock())
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
    fn register_and_take_protected_routes() {
        let api = HttpRouter::new();

        api.add_protected_routes(
            axum::Router::new().route("/private", get(|| async { "private" })),
        );

        let routes = api.take_protected_routes();
        assert_eq!(routes.len(), 1);

        let routes = api.take_protected_routes();
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

    #[test]
    fn register_and_take_protected_builders() {
        let api = HttpRouter::new();

        api.add_protected_routes_with(|_| {
            axum::Router::new().route("/private", get(|| async { "private" }))
        });

        let builders = api.take_protected_builders();
        assert_eq!(builders.len(), 1);

        let builders = api.take_protected_builders();
        assert!(builders.is_empty());
    }
}
