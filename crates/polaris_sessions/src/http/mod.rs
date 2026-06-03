//! HTTP REST endpoints for session management.
//!
//! Provides [`HttpPlugin`], which registers REST endpoints for
//! creating, listing, inspecting, deleting sessions, processing
//! agent turns, managing checkpoints, and persisting sessions.
//! Requires the `sessions-http` feature flag.
//!
//! # Endpoints
//!
//! | Method   | Path                                 | Description                  |
//! |----------|--------------------------------------|------------------------------|
//! | `POST`   | `/v1/sessions`                       | Create a new session         |
//! | `GET`    | `/v1/sessions`                       | List live sessions           |
//! | `GET`    | `/v1/sessions/stored`                | List persisted sessions      |
//! | `GET`    | `/v1/sessions/agent-types`           | List registered agent types  |
//! | `GET`    | `/v1/sessions/{id}`                  | Get session info             |
//! | `DELETE` | `/v1/sessions/{id}`                  | Delete a session             |
//! | `POST`   | `/v1/sessions/{id}/turns`            | Process a turn               |
//! | `POST`   | `/v1/sessions/{id}/turns/stream`     | Process a turn (SSE)         |
//! | `GET`    | `/v1/sessions/{id}/turns`            | List recorded turn summaries |
//! | `GET`    | `/v1/sessions/{id}/turns/{n}`        | Get a single turn detail     |
//! | `GET`    | `/v1/sessions/{id}/uptime`           | Bucketed lifecycle series    |
//! | `POST`   | `/v1/sessions/{id}/checkpoints`      | Create a checkpoint          |
//! | `GET`    | `/v1/sessions/{id}/checkpoints`      | List checkpoints             |
//! | `POST`   | `/v1/sessions/{id}/rollback`         | Rollback to a checkpoint     |
//! | `POST`   | `/v1/sessions/{id}/save`             | Persist session to store     |
//! | `POST`   | `/v1/sessions/{id}/resume`           | Resume from store            |
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! use polaris_sessions::{SessionsPlugin, http::HttpPlugin};
//! use polaris_sessions::store::memory::InMemoryStore;
//! use polaris_app::{AppPlugin, AppConfig};
//! use polaris_core_plugins::PersistencePlugin;
//! use polaris_system::server::Server;
//!
//! # async fn example() {
//! let mut server = Server::new();
//! server
//!     .add_plugins(PersistencePlugin)
//!     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
//!     .add_plugins(AppPlugin::new(AppConfig::new()))
//!     .add_plugins(HttpPlugin::new());
//! server.run().await.unwrap();
//! # }
//! ```

mod error;
mod handlers;
mod io;
pub mod models;

use crate::api::SessionsAPI;
use axum::Router;
use axum::routing::{get, post};
pub use io::HttpIOProvider;
pub use models::{
    CheckpointResponse, CreateSessionRequest, CreateSessionResponse, ListCheckpointsResponse,
    ListSessionsResponse, ListStoredSessionsResponse, ProcessTurnRequest, ProcessTurnResponse,
    RollbackRequest, StreamTurnDone, TurnExecutionMetadata,
};
use polaris_app::HttpRouter;
use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris_system::server::Server;

/// Plugin that exposes session management over HTTP.
///
/// Registers REST endpoints against the [`HttpRouter`] for creating,
/// listing, inspecting, deleting sessions, processing agent turns (with
/// optional SSE streaming), managing checkpoints, and persisting or
/// resuming sessions. Routes are composed inside an `add_routes_with`
/// closure so the [`SessionsAPI`] handle is resolved during the app's
/// `ready()` phase rather than at `build()` time.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only mounts HTTP routes against [`HttpRouter`]. |
///
/// # APIs Provided
///
/// None. State for the routes is the [`SessionsAPI`] handle obtained
/// from [`SessionsPlugin`](crate::SessionsPlugin).
///
/// # Dependencies
///
/// Expressed as capabilities (see [`Plugin::access`]):
///
/// - extends [`HttpRouter`] (from [`AppPlugin`](polaris_app::AppPlugin)) —
///   the router the routes are mounted on.
/// - requires [`SessionsAPI`] (from [`SessionsPlugin`](crate::SessionsPlugin))
///   — used as handler state.
///
/// See the [module-level documentation](self) for the endpoint table.
///
/// # Extends
///
/// - [`HttpRouter`] (from [`AppPlugin`](polaris_app::AppPlugin)) — registers the session REST
///   endpoints listed in the [module-level documentation](self) via an
///   `add_routes_with` closure, so the [`SessionsAPI`] handler state is
///   resolved during the app's `ready()` phase. This plugin provides no
///   resources or APIs of its own — it composes session management onto
///   the shared HTTP server.
///
/// # Example
///
/// ```no_run
/// # use std::sync::Arc;
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_core_plugins::PersistencePlugin;
/// use polaris_sessions::{SessionsPlugin, http::HttpPlugin};
/// use polaris_sessions::store::memory::InMemoryStore;
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(HttpPlugin::new());
/// server.run().await.unwrap();
/// # }
/// ```
#[derive(Debug, Default)]
pub struct HttpPlugin;

impl HttpPlugin {
    /// Creates a new `HttpPlugin`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for HttpPlugin {
    const ID: &'static str = "polaris::sessions::http";
    const VERSION: Version = Version::new(0, 0, 1);

    fn access(&self) -> PluginAccess {
        // Declares the capability relationships rather than naming `AppPlugin` /
        // `SessionsPlugin`: extends the `HttpRouter` it mounts routes on, and requires the
        // `SessionsAPI` used as handler state. Both are APIs (not resources), so they are
        // declared here and accessed imperatively in `build()` via `server.api::<_>()`
        // rather than through typed `Extends`/`Requires` build parameters. The resolver
        // orders both providers first and guarantees their presence.
        PluginAccess::new()
            .extends::<HttpRouter>(VersionReq::caret(HttpRouter::CONTRACT_VERSION))
            .requires::<SessionsAPI>(VersionReq::caret(SessionsAPI::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        server
            .api::<HttpRouter>()
            .expect("HttpRouter capability must be provided before HttpPlugin")
            .add_routes_with(|server| {
                let sessions = server
                    .api::<SessionsAPI>()
                    .expect("SessionsAPI capability must be provided before HttpPlugin")
                    .clone();
                Router::new()
                    .route(
                        "/v1/sessions",
                        post(handlers::create_session).get(handlers::list_sessions),
                    )
                    // Static paths before wildcards to avoid `{id}` capturing
                    // `stored` or `agent-types`.
                    .route("/v1/sessions/stored", get(handlers::list_stored_sessions))
                    .route("/v1/sessions/agent-types", get(handlers::list_agent_types))
                    .route(
                        "/v1/sessions/{id}",
                        get(handlers::get_session).delete(handlers::delete_session),
                    )
                    .route(
                        "/v1/sessions/{id}/turns",
                        post(handlers::process_turn).get(handlers::list_turns),
                    )
                    .route(
                        "/v1/sessions/{id}/turns/stream",
                        post(handlers::process_turn_stream),
                    )
                    .route("/v1/sessions/{id}/turns/{n}", get(handlers::get_turn))
                    .route("/v1/sessions/{id}/uptime", get(handlers::get_uptime))
                    .route(
                        "/v1/sessions/{id}/checkpoints",
                        post(handlers::create_checkpoint).get(handlers::list_checkpoints),
                    )
                    .route("/v1/sessions/{id}/rollback", post(handlers::rollback))
                    .route("/v1/sessions/{id}/save", post(handlers::save_session))
                    .route("/v1/sessions/{id}/resume", post(handlers::resume_session))
                    .with_state(sessions)
            });
    }
}
