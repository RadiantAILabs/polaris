//! Shared HTTP server runtime for Polaris products.
//!
//! `polaris_app` provides an axum-based HTTP server that integrates with the
//! Polaris plugin lifecycle. Plugins register route fragments during `build()`
//! via the [`HttpRouter`] API, and [`AppPlugin`] merges and serves them with
//! Tower middleware (CORS, tracing, request ID).
//!
//! This crate is the shared infrastructure for `polaris-http`, `polaris-mcp`
//! (SSE transport), and any future product that needs an HTTP interface.
//!
//! # Architecture
//!
//! ```text
//! AppPlugin (axum server lifecycle)
//!   ├── HttpRouter API (route and auth registration)
//!   ├── AppConfig (host, port, CORS)
//!   ├── ServerHandle (shutdown signal, API)
//!   ├── AuthProvider trait (pluggable authentication)
//!   ├── Tower middleware (CORS, tracing, request ID, auth)
//!   └── HttpIOProvider (channel-based IO bridging)
//! ```
//!
//! # Quick Start
//!
//! ```no_run
//! use polaris_system::server::Server;
//! use polaris_app::{AppPlugin, AppConfig, HttpRouter};
//! use axum::{Router, routing::get};
//!
//! let mut server = Server::new();
//! server.add_plugins(
//!     AppPlugin::new(AppConfig::new().with_port(8080))
//! );
//! // Other plugins register routes via HttpRouter in their build()
//! ```
//!
//! # Plugin Route Registration
//!
//! Any plugin that depends on [`AppPlugin`] can register routes:
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
//!         let router = Router::new().route("/healthz", get(|| async { "ok" }));
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

pub mod auth;
pub mod config;
pub mod io;
mod middleware;
pub mod plugin;
pub mod router;

pub use auth::AuthProvider;
pub use config::AppConfig;
pub use io::HttpIOProvider;
pub use plugin::{AppPlugin, ServerHandle};
pub use router::HttpRouter;
