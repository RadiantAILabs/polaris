#![cfg_attr(docsrs_dep, feature(doc_cfg))]

//! Cross-plugin contribution registry for Polaris dashboards and
//! observability UIs.
//!
//! Any plugin can declare nav items, sections, and panels for a dashboard
//! by contributing to a single [`DashboardRegistry`] during its `build()`
//! phase. [`DashboardPlugin`] freezes the registry in `ready()`, broadcasts
//! a [`RegistryEvent::Ready`] snapshot, and serves the snapshot at
//! `GET /v1/dashboard/manifest`.
//!
//! # Hard Invariant
//!
//! This crate is **registry-only**. No HTML, no JS bundles, no asset
//! pipeline ever lives here. The actual UI is owned by external consumers
//! (the `polaris-dashboard` Svelte app, future terminal TUIs, custom admin
//! UIs) that read the manifest and render it however they like.
//!
//! # Seed `kind` Vocabulary
//!
//! [`Panel::kind`] is a free-form `String` so plugins can introduce new
//! panel types without touching this crate. The Svelte reference dashboard
//! ships a component per *seed kind* below; plugin-declared kinds fall
//! through to a schema-driven generic renderer.
//!
//! | `kind`           | Intended use                                       |
//! |------------------|----------------------------------------------------|
//! | `list`           | Tabular listing of records (sessions, tools, …)    |
//! | `detail`         | Single-record detail view                          |
//! | `kv`             | Key/value inspector for resource snapshots         |
//! | `log`            | Append-only log stream (e.g. spans, events)        |
//! | `timeseries`     | Numeric time-series for live metrics               |
//! | `polaris-graph`  | Graph topology visualization for an agent          |
//! | `otel-trace`     | OpenTelemetry-formatted trace tree                 |
//! | `external`       | Iframe pointing at `metadata.url` (escape hatch)   |
//!
//! Plugins that need a different shape pick a fresh string and ship type
//! definitions for the panel's `metadata` payload via the workspace's
//! `typegen` pipeline.
//!
//! # Quick Start
//!
//! ```no_run
//! use polaris_app::{AppConfig, AppPlugin};
//! use polaris_dashboard::{
//!     DashboardPlugin, DashboardRegistry, NavItem, Panel, Section, Transport,
//! };
//! use polaris_system::plugin::{Plugin, PluginId, Version};
//! use polaris_system::server::Server;
//!
//! struct SessionsContribution;
//!
//! impl Plugin for SessionsContribution {
//!     const ID: &'static str = "myapp::sessions_dashboard";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn build(&self, server: &mut Server) {
//!         server
//!             .api::<DashboardRegistry>()
//!             .expect("DashboardPlugin must be added first")
//!             .add_nav_item(NavItem::new("sessions", "Sessions"))
//!             .add_section(Section::new("sessions-overview", "sessions", "Overview"))
//!             .add_panel(
//!                 Panel::new(
//!                     "sessions-list",
//!                     "Active sessions",
//!                     "list",
//!                     "/v1/sessions",
//!                     Transport::Rest,
//!                 )
//!                 .with_section("sessions-overview"),
//!             );
//!     }
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<DashboardPlugin>()]
//!     }
//! }
//!
//! # async fn run() {
//! let mut server = Server::new();
//! server
//!     .add_plugins(AppPlugin::new(AppConfig::new()))
//!     .add_plugins(DashboardPlugin)
//!     .add_plugins(SessionsContribution);
//! server.run().await;
//! # }
//! ```

mod events;
mod plugin;
mod registry;

pub use events::RegistryEvent;
pub use plugin::DashboardPlugin;
pub use registry::{DashboardRegistry, Manifest, NavItem, Panel, Section, Transport};
