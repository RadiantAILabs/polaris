#![cfg_attr(docsrs_dep, feature(doc_cfg))]

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
//!   ├── WsRouter API (WebSocket route registration, `ws` feature)
//!   ├── AppConfig (host, port, CORS)
//!   ├── ServerHandle (shutdown signal, API)
//!   ├── AuthProvider trait (pluggable authentication)
//!   ├── Tower middleware (CORS, tracing, request ID, auth)
//!   └── RequestContextPlugin (trace/correlation/request IDs)
//! ```
//!
//! # Request Context
//!
//! [`RequestContext`] carries `trace_id`, `correlation_id`, and `request_id`
//! for observability and propagation. Two entry points:
//!
//! - **Custom axum handlers** — [`RequestContext`] implements
//!   [`FromRequestParts`](axum::extract::FromRequestParts) with
//!   `Rejection = Infallible`, so handlers can accept it as an argument.
//! - **Session graphs** — handlers insert [`HttpHeaders`] into the setup
//!   closure, and [`RequestContextPlugin`]'s `OnGraphStart` hook turns them
//!   into a [`RequestContext`] that systems read via `Res<RequestContext>`.
//!
//! The pure core is [`RequestContext::from_headers`], lenient by design:
//! missing headers become `None`, never a rejection.
//!
//! # Feature Flags
//!
//! - `ws` — enables [`WsRouter`] for plugin-contributed WebSocket routes
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
//!
//! # See Also
//!
//! For the full framework guide, deferred router construction
//! (`add_routes_with`), and HTTP integration patterns, see the
//! [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

pub mod auth;
pub mod config;
mod middleware;
pub mod plugin;
pub mod public_route;
pub mod request_context;
pub mod router;
#[cfg(feature = "ws")]
#[cfg_attr(docsrs_dep, doc(cfg(feature = "ws")))]
pub mod ws;

pub use auth::AuthProvider;
pub use config::AppConfig;
pub use plugin::{AppPlugin, ServerHandle};
pub use public_route::{PublicPath, PublicPrefix, PublicRouteError};
pub use request_context::{HttpHeaders, RequestContext, RequestContextPlugin};
pub use router::HttpRouter;
#[cfg(feature = "ws")]
#[cfg_attr(docsrs_dep, doc(cfg(feature = "ws")))]
pub use ws::WsRouter;
