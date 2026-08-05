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
//! | `GET`    | `/v1/sessions/agent-types`           | List registered agent type names |
//! | `GET`    | `/v1/sessions/agent-types/details`   | Auth-protected agent type signatures and contracts |
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
    AgentSignature, AgentTypeSummary, CheckpointResponse, CreateSessionRequest,
    CreateSessionResponse, ListAgentTypesResponse, ListCheckpointsResponse, ListSessionsResponse,
    ListStoredSessionsResponse, ProcessTurnRequest, ProcessTurnResponse, RollbackRequest,
    StreamTurnDone, TurnExecutionMetadata,
};
use polaris_app::HttpRouter;
use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris_system::server::Server;

/// Build the canonical sessions HTTP router.
///
/// Returns the public session route table [`HttpPlugin`] registers (see the
/// [module-level endpoint table](self)), with the given [`SessionsAPI`] as
/// handler state. Reach for this instead of [`HttpPlugin`] when you own the
/// mounting — embedding the endpoints in a larger axum `Router`, serving
/// them under a custom prefix, or exposing them from a listener the shared
/// [`HttpRouter`] does not manage. When the routes should simply appear on
/// the shared app listener, register [`HttpPlugin`] and let it do this
/// wiring.
///
/// The graph-signature and capability-contract metadata route is not part of
/// this standalone table. [`HttpPlugin`] registers that route through
/// [`HttpRouter::add_protected_routes_with`], so it is mounted only when an
/// [`AuthProvider`](polaris_app::AuthProvider) is configured.
///
/// The returned router is relative to its mount point. Mounting it at `/`
/// serves `/v1/sessions/*`; nesting it under `/api` serves
/// `/api/v1/sessions/*`.
///
/// # Security
///
/// The returned router carries **no authentication** and none of
/// [`AppPlugin`](polaris_app::AppPlugin)'s middleware. An
/// [`AuthProvider`](polaris_app::AuthProvider), CORS, and tracing apply
/// only to routes registered through [`HttpRouter`] and served by
/// [`AppPlugin`](polaris_app::AppPlugin) — mounting this router on your own
/// server exposes every endpoint in this public table, including turn
/// execution and session deletion, unauthenticated unless you layer
/// protection yourself. Private graph-signature metadata is intentionally not
/// included here.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use axum::Router;
/// use polaris_core_plugins::PersistencePlugin;
/// use polaris_sessions::{SessionsAPI, SessionsPlugin, http};
/// use polaris_sessions::store::memory::InMemoryStore;
/// use polaris_system::server::Server;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
/// server.finish().await?;
///
/// let sessions = server
///     .api::<SessionsAPI>()
///     .expect("SessionsPlugin inserted SessionsAPI")
///     .clone();
/// let app: Router = Router::new().nest("/api", http::routes(sessions));
/// # Ok(())
/// # }
/// ```
pub fn routes(sessions: SessionsAPI) -> Router {
    Router::new()
        .route(
            "/v1/sessions",
            post(handlers::create_session).get(handlers::list_sessions),
        )
        // Static paths before wildcards to avoid `{id}` capturing `stored` or
        // `agent-types`.
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
}

fn protected_routes(sessions: SessionsAPI) -> Router {
    Router::new()
        .route(
            "/v1/sessions/agent-types/details",
            get(handlers::list_agent_type_details),
        )
        .with_state(sessions)
}

/// Plugin that exposes session management over HTTP.
///
/// Registers REST endpoints against the [`HttpRouter`] for creating,
/// listing, inspecting, deleting sessions, processing agent turns (with
/// optional SSE streaming), managing checkpoints, and persisting or
/// resuming sessions. Reach for this when session management should be served
/// by the shared [`AppPlugin`](polaris_app::AppPlugin) listener. Routes are
/// composed inside `add_routes_with` closures so the [`SessionsAPI`] handle
/// is resolved during the app's `ready()` phase rather than at `build()`
/// time.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only mounts HTTP routes against [`HttpRouter`]. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | State for the routes is the [`SessionsAPI`] handle obtained from [`SessionsPlugin`](crate::SessionsPlugin). |
///
/// # Routes Provided
///
/// All paths are rooted at `/v1/sessions`; mounting through
/// [`AppPlugin`](polaris_app::AppPlugin) applies its CORS, tracing, request
/// ID, and auth middleware. Every handler reads [`SessionsAPI`] from
/// [`axum::extract::State`]; routes that process or resume turns also read
/// path/body extractors noted by the path and method.
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `POST` | `/v1/sessions` | Create a new session from a JSON [`CreateSessionRequest`]. |
/// | `GET` | `/v1/sessions` | List live sessions. |
/// | `GET` | `/v1/sessions/stored` | List sessions persisted in the backing store. |
/// | `GET` | `/v1/sessions/agent-types` | List registered agent type names; omits graph signatures and capability contracts. |
/// | `GET` | `/v1/sessions/agent-types/details` | Protected route with rendered graph signatures and satisfied capability contracts; mounted only when an [`AuthProvider`](polaris_app::AuthProvider) is configured. |
/// | `GET` | `/v1/sessions/{id}` | Get live session metadata. |
/// | `DELETE` | `/v1/sessions/{id}` | Delete a live session. |
/// | `POST` | `/v1/sessions/{id}/turns` | Process one turn from a JSON [`ProcessTurnRequest`]. |
/// | `POST` | `/v1/sessions/{id}/turns/stream` | Process one turn and stream `IOMessage` events as SSE. |
/// | `GET` | `/v1/sessions/{id}/turns` | List recorded turn summaries; `?include=messages` embeds messages. |
/// | `GET` | `/v1/sessions/{id}/turns/{n}` | Fetch a single recorded turn. |
/// | `GET` | `/v1/sessions/{id}/uptime` | Bucketed session lifecycle series; reads `bucket`, `since`, and `until` query parameters. |
/// | `POST` | `/v1/sessions/{id}/checkpoints` | Create a checkpoint. |
/// | `GET` | `/v1/sessions/{id}/checkpoints` | List checkpoints. |
/// | `POST` | `/v1/sessions/{id}/rollback` | Roll back to a checkpoint from a JSON [`RollbackRequest`]. |
/// | `POST` | `/v1/sessions/{id}/save` | Persist a live session to the backing store. |
/// | `POST` | `/v1/sessions/{id}/resume` | Resume a persisted session into memory. |
///
/// # Lifecycle
///
/// - Feature gated by `sessions-http`.
/// - `build()` contributes public routes with
///   [`HttpRouter::add_routes_with`] and the private agent metadata route
///   with [`HttpRouter::add_protected_routes_with`].
/// - The protected route is skipped unless an
///   [`AuthProvider`](polaris_app::AuthProvider) is configured on
///   [`HttpRouter`].
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
/// # Extends
///
/// - [`HttpRouter`] (from [`AppPlugin`](polaris_app::AppPlugin)) —
///   registers the public session REST endpoints via
///   [`HttpRouter::add_routes_with`] and the private agent metadata endpoint
///   via [`HttpRouter::add_protected_routes_with`], so the [`SessionsAPI`]
///   handler state is resolved during the app's `ready()` phase. This
///   plugin provides no resources or APIs of its own — it composes session
///   management onto the shared HTTP server.
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
        let router = server
            .api::<HttpRouter>()
            .expect("HttpRouter capability must be provided before HttpPlugin");
        router.add_routes_with(|server| {
            let sessions = server
                .api::<SessionsAPI>()
                .expect("SessionsAPI capability must be provided before HttpPlugin")
                .clone();
            routes(sessions)
        });
        router.add_protected_routes_with(|server| {
            let sessions = server
                .api::<SessionsAPI>()
                .expect("SessionsAPI capability must be provided before HttpPlugin")
                .clone();
            protected_routes(sessions)
        });
    }
}
