//! HTTP server for the ReAct agent.
//!
//! Serves the same ReAct agent as the CLI example, but over HTTP using
//! `polaris_app`. Demonstrates the `AppPlugin` route registration pattern
//! with plugin-based route composition and session management via REST.
//!
//! # Usage
//!
//! ```bash
//! cargo run -p examples --bin http -- <working_dir> [--port 3000]
//! ```
//!
//! # Endpoints
//!
//! ```bash
//! # Health check
//! curl http://localhost:3000/healthz
//!
//! # Server and agent info
//! curl http://localhost:3000/v1/info
//!
//! # Session management
//! curl -X POST http://localhost:3000/v1/sessions \
//!   -H 'Content-Type: application/json' \
//!   -d '{"agent_type": "react"}'
//! curl http://localhost:3000/v1/sessions
//! curl http://localhost:3000/v1/sessions/{id}
//! curl -X DELETE http://localhost:3000/v1/sessions/{id}
//!
//! # Send a turn (using the pre-loaded "demo" session)
//! curl -X POST http://localhost:3000/v1/sessions/demo/turns \
//!   -H 'Content-Type: application/json' \
//!   -d '{"message": "What files are in the current directory?"}'
//! ```

use axum::routing::get;
use axum::{Json, Router};
use examples::plugins::{FileToolsConfig, FileToolsPlugin};
use examples::react_agent::{AgentConfig, ReActAgent, ReActPlugin};
use polaris::models::AnthropicPlugin;
use polaris::plugins::{PersistencePlugin, ServerInfoPlugin, TracingPlugin};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::{
    graph::DevToolsPlugin, models::ModelsPlugin, plugins::FmtConfig, system::server::Server,
    tools::ToolsPlugin,
};
use polaris_app::{AppConfig, AppPlugin, HttpRouter};
use polaris_sessions::http::HttpPlugin;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

/// Registered agent entry for the info endpoint.
#[derive(Clone, Serialize)]
struct AgentEntry {
    name: String,
    model_id: String,
}

/// Shared state for HTTP handlers.
#[derive(Clone)]
struct AppState {
    agents: Vec<AgentEntry>,
}

/// Plugin that registers example HTTP routes.
struct ExampleRoutesPlugin {
    state: AppState,
}

impl ExampleRoutesPlugin {
    fn new(agents: Vec<AgentEntry>) -> Self {
        Self {
            state: AppState { agents },
        }
    }
}

impl Plugin for ExampleRoutesPlugin {
    const ID: &'static str = "example::http_routes";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let router = Router::new()
            .route("/healthz", get(health))
            .route("/v1/info", get(info))
            .with_state(self.state.clone());

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added first")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct InfoResponse {
    name: &'static str,
    version: &'static str,
    agents: Vec<AgentEntry>,
}

async fn info(axum::extract::State(state): axum::extract::State<AppState>) -> Json<InfoResponse> {
    Json(InfoResponse {
        name: "polaris-http",
        version: env!("CARGO_PKG_VERSION"),
        agents: state.agents,
    })
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: http <working_dir> [--port <port>]");
        std::process::exit(1);
    }

    let working_dir = PathBuf::from(&args[1])
        .canonicalize()
        .unwrap_or_else(|err| {
            eprintln!("Error: {err}");
            std::process::exit(1);
        });

    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let model_id = "anthropic/claude-sonnet-4-6";

    // Build the agent registry for the info endpoint.
    // In a real app, this would be dynamic based on registered agents and their configs.
    let agents = vec![AgentEntry {
        name: ReActAgent::NAME.to_string(),
        model_id: model_id.to_string(),
    }];

    // Build server with AppPlugin for HTTP serving
    let mut server = Server::new();
    server
        .add_plugins(
            TracingPlugin::default()
                .with_fmt(FmtConfig::default().env_filter("polaris=debug,warn")),
        )
        .add_plugins(ServerInfoPlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
        .add_plugins(ToolsPlugin)
        .add_plugins(FileToolsPlugin::new(FileToolsConfig::new(&working_dir)))
        .add_plugins(PersistencePlugin)
        .add_plugins(ReActPlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(DevToolsPlugin::new().with_event_tracing())
        .add_plugins(AppPlugin::new(
            AppConfig::new().with_host("0.0.0.0").with_port(port),
        ))
        .add_plugins(HttpPlugin::new())
        .add_plugins(ExampleRoutesPlugin::new(agents));

    server.finish().await;

    // Register the agent so the HTTP session endpoints can create sessions for it.
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(ReActAgent).unwrap();

    // Create a demo session pre-loaded with agent config.
    // Additional sessions can be created dynamically via POST /v1/sessions.
    let session_id = polaris::sessions::SessionId::from_string("demo");
    let agent_type = AgentTypeId::from_name(ReActAgent::NAME);
    sessions
        .create_session_with(server.create_context(), &session_id, &agent_type, |ctx| {
            ctx.insert(AgentConfig::new(model_id));
        })
        .unwrap();

    tracing::info!(
        port = port,
        working_dir = %working_dir.display(),
        session = "demo",
        "polaris-http ready — try: curl http://localhost:{port}/healthz"
    );

    // Keep the server alive until Ctrl+C
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");

    tracing::info!("shutting down");
    server.cleanup().await;
}
