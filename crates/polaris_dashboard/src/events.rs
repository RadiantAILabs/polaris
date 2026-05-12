//! Broadcast events emitted by [`DashboardRegistry`](crate::DashboardRegistry).
//!
//! v0.1 defines exactly one variant — [`RegistryEvent::Ready`] — emitted
//! once when [`DashboardPlugin`](crate::DashboardPlugin) freezes the
//! registry in `ready()`. The enum is `#[non_exhaustive]` so future
//! variants (live mutation, removal, …) are non-breaking.

use crate::registry::Manifest;
use std::sync::Arc;

/// Event broadcast by [`DashboardRegistry`](crate::DashboardRegistry).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RegistryEvent {
    /// The registry has been frozen and the carried [`Manifest`] is the
    /// canonical snapshot that backs `GET /v1/dashboard/manifest`.
    Ready(Arc<Manifest>),
}
