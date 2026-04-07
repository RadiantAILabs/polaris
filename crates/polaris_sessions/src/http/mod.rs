//! HTTP REST endpoints for session management.
//!
//! Provides [`HttpPlugin`], which registers REST endpoints for
//! creating, listing, inspecting, deleting sessions, processing
//! agent turns, managing checkpoints, and persisting sessions.
//! Requires the `http` feature flag.
//!
//! # Endpoints
//!
//! | Method   | Path                              | Description              |
//! |----------|-----------------------------------|--------------------------|
//! | `POST`   | `/v1/sessions`                    | Create a new session     |
//! | `GET`    | `/v1/sessions`                    | List live sessions       |
//! | `GET`    | `/v1/sessions/stored`             | List persisted sessions  |
//! | `GET`    | `/v1/sessions/{id}`               | Get session info         |
//! | `DELETE` | `/v1/sessions/{id}`               | Delete a session         |
//! | `POST`   | `/v1/sessions/{id}/turns`         | Process a turn           |
//! | `POST`   | `/v1/sessions/{id}/checkpoints`   | Create a checkpoint      |
//! | `GET`    | `/v1/sessions/{id}/checkpoints`   | List checkpoints         |
//! | `POST`   | `/v1/sessions/{id}/rollback`      | Rollback to a checkpoint |
//! | `POST`   | `/v1/sessions/{id}/save`          | Persist session to store |
//! | `POST`   | `/v1/sessions/{id}/resume`        | Resume from store        |
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
//! server.run().await;
//! # }
//! ```

mod error;
mod handlers;
pub mod models;

use crate::api::{SessionsAPI, SessionsPlugin};
use axum::Router;
use axum::routing::{get, post};
pub use models::{
    CheckpointResponse, CreateSessionRequest, CreateSessionResponse, ListCheckpointsResponse,
    ListSessionsResponse, ListStoredSessionsResponse, ProcessTurnRequest, ProcessTurnResponse,
    RollbackRequest, TurnExecutionMetadata,
};
use polaris_app::{AppPlugin, HttpRouter};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::sync::{Arc, OnceLock};

/// Shared state for session HTTP handlers.
///
/// Uses [`OnceLock`] for deferred initialization: routes are registered in
/// [`build()`](HttpPlugin::build) but the [`SessionsAPI`] reference is only
/// available in [`ready()`](HttpPlugin::ready).
pub(crate) type DeferredState = Arc<OnceLock<SessionsAPI>>;

/// Plugin that exposes session management over HTTP.
///
/// Registers REST endpoints for creating, listing, inspecting, and
/// deleting sessions. Requires [`AppPlugin`] and [`SessionsPlugin`].
///
/// See the [module-level documentation](self) for endpoint details.
#[derive(Debug)]
pub struct HttpPlugin {
    state: DeferredState,
}

impl HttpPlugin {
    /// Creates a new `HttpPlugin`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(OnceLock::new()),
        }
    }
}

impl Default for HttpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for HttpPlugin {
    const ID: &'static str = "polaris::sessions::http";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let state = Arc::clone(&self.state);
        let router = Router::new()
            .route(
                "/v1/sessions",
                post(handlers::create_session).get(handlers::list_sessions),
            )
            // Static path before wildcard to avoid `{id}` capturing "stored".
            .route("/v1/sessions/stored", get(handlers::list_stored_sessions))
            .route(
                "/v1/sessions/{id}",
                get(handlers::get_session).delete(handlers::delete_session),
            )
            .route("/v1/sessions/{id}/turns", post(handlers::process_turn))
            .route(
                "/v1/sessions/{id}/checkpoints",
                post(handlers::create_checkpoint).get(handlers::list_checkpoints),
            )
            .route("/v1/sessions/{id}/rollback", post(handlers::rollback))
            .route("/v1/sessions/{id}/save", post(handlers::save_session))
            .route("/v1/sessions/{id}/resume", post(handlers::resume_session))
            .with_state(state);

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added before HttpPlugin")
            .add_routes(router);
    }

    async fn ready(&self, server: &mut Server) {
        let sessions = server
            .api::<SessionsAPI>()
            .expect("SessionsPlugin must be added before HttpPlugin")
            .clone();

        if self.state.set(sessions).is_err() {
            panic!("HttpPlugin::ready() must only be called once");
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<AppPlugin>(),
            PluginId::of::<SessionsPlugin>(),
        ]
    }
}
