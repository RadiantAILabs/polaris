//! [`DashboardRegistry`] and the descriptor types it holds.
//!
//! The registry is the cross-plugin contribution surface: each plugin calls
//! [`add_nav_item`](DashboardRegistry::add_nav_item),
//! [`add_section`](DashboardRegistry::add_section), or
//! [`add_panel`](DashboardRegistry::add_panel) during its `build()` phase.
//! [`DashboardPlugin`](crate::DashboardPlugin) calls
//! [`freeze`](DashboardRegistry::freeze) once during `ready()` to capture an
//! immutable [`Manifest`] snapshot and broadcast it via
//! [`RegistryEvent::Ready`].

use crate::events::RegistryEvent;
use parking_lot::RwLock;
use polaris_system::api::API;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Capacity of the [`RegistryEvent`] broadcast channel.
///
/// The channel only carries `Ready(snapshot)` in v0.1, so the buffer is
/// intentionally small.
const EVENT_CHANNEL_CAPACITY: usize = 16;

// ─────────────────────────────────────────────────────────────────────────────
// Descriptor types
// ─────────────────────────────────────────────────────────────────────────────

/// A top-level navigation entry contributed by a plugin.
///
/// Frontends use [`NavItem::label`] for the link text and the entry's
/// [`Section`]s and [`Panel`]s to render its body.
///
/// `#[non_exhaustive]` so future fields can be added without breaking
/// downstream contributors. Construct via [`NavItem::new`] +
/// [`with_metadata`](Self::with_metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct NavItem {
    /// Stable identifier referenced by [`Section::nav_item_id`] and used
    /// for build-time suppression via
    /// [`DashboardRegistry::remove_nav_item`].
    pub id: String,
    /// Human-readable label rendered in the navigation.
    pub label: String,
    /// Plugin-defined extension. Free-form to allow new shapes without a
    /// core crate bump.
    #[serde(default, skip_serializing_if = "is_null")]
    #[cfg_attr(feature = "typegen", ts(type = "unknown"))]
    pub metadata: serde_json::Value,
}

impl NavItem {
    /// Creates a `NavItem` with empty metadata.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Attaches plugin-defined metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// A logical grouping of [`Panel`]s within a [`NavItem`].
///
/// Every panel is expected to belong to a section: [`Panel::section_id`]
/// is the only formal linkage from a panel back to its [`NavItem`] (via
/// `section.nav_item_id`), so consumers that render panels grouped by
/// section will drop section-less panels. A nav item with no logical
/// grouping should still register a single "Overview" section so its
/// panels render.
///
/// `#[non_exhaustive]` so future fields can be added without breaking
/// downstream contributors. Construct via [`Section::new`] +
/// [`with_metadata`](Self::with_metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct Section {
    /// Stable identifier referenced by [`Panel::section_id`] and used for
    /// build-time suppression via [`DashboardRegistry::remove_section`].
    pub id: String,
    /// [`NavItem::id`] this section belongs to.
    pub nav_item_id: String,
    /// Human-readable title rendered above the section's panels.
    pub title: String,
    /// Plugin-defined extension.
    #[serde(default, skip_serializing_if = "is_null")]
    #[cfg_attr(feature = "typegen", ts(type = "unknown"))]
    pub metadata: serde_json::Value,
}

impl Section {
    /// Creates a `Section` with empty metadata.
    pub fn new(
        id: impl Into<String>,
        nav_item_id: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            nav_item_id: nav_item_id.into(),
            title: title.into(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Attaches plugin-defined metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Wire transport for a [`Panel`]'s data feed.
///
/// `#[non_exhaustive]` so additional transports (e.g. gRPC) can be added
/// without breaking downstream `match` arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export, rename_all = "kebab-case"))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Transport {
    /// One-shot HTTP GET — re-fetched on demand.
    Rest,
    /// Server-Sent Events stream.
    Sse,
    /// Bidirectional WebSocket connection.
    WebSocket,
}

/// A single dashboard panel: a renderable view of plugin-owned data.
///
/// `kind` is a free-form string from the seed vocabulary documented in the
/// [crate root](crate) or a plugin-defined extension. `metadata` carries
/// kind-specific parameters and is duck-typed in v0.1.
///
/// `#[non_exhaustive]` so future fields can be added without breaking
/// downstream contributors. Construct via [`Panel::new`] +
/// [`with_section`](Self::with_section) / [`with_metadata`](Self::with_metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct Panel {
    /// Stable identifier used for build-time suppression via
    /// [`DashboardRegistry::remove_panel`].
    pub id: String,
    /// Optional [`Section::id`] this panel belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    /// Human-readable title rendered above the panel.
    pub title: String,
    /// Rendering hint (`list`, `detail`, `timeseries`, …). See the
    /// [crate-level seed vocabulary](crate).
    pub kind: String,
    /// Path or URL the consumer should hit to fetch panel data.
    pub endpoint: String,
    /// Wire transport for [`Self::endpoint`].
    pub transport: Transport,
    /// Plugin-defined extension carrying kind-specific parameters.
    #[serde(default, skip_serializing_if = "is_null")]
    #[cfg_attr(feature = "typegen", ts(type = "unknown"))]
    pub metadata: serde_json::Value,
}

impl Panel {
    /// Creates a `Panel` with no section, no metadata.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: impl Into<String>,
        endpoint: impl Into<String>,
        transport: Transport,
    ) -> Self {
        Self {
            id: id.into(),
            section_id: None,
            title: title.into(),
            kind: kind.into(),
            endpoint: endpoint.into(),
            transport,
            metadata: serde_json::Value::Null,
        }
    }

    /// Places this panel inside a [`Section`].
    #[must_use]
    pub fn with_section(mut self, section_id: impl Into<String>) -> Self {
        self.section_id = Some(section_id.into());
        self
    }

    /// Attaches plugin-defined metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Frozen snapshot of the registry produced by
/// [`DashboardRegistry::freeze`].
///
/// Serialized as the body of `GET /v1/dashboard/manifest` and broadcast via
/// [`RegistryEvent::Ready`].
///
/// `#[non_exhaustive]` so future top-level fields (e.g. `version`) can be
/// added without breaking downstream consumers that pattern-match the
/// struct.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct Manifest {
    /// Top-level navigation entries.
    pub nav_items: Vec<NavItem>,
    /// Sections grouping panels under nav items.
    pub sections: Vec<Section>,
    /// Panels — the renderable units.
    pub panels: Vec<Panel>,
}

// Helper for `skip_serializing_if`.
fn is_null(value: &serde_json::Value) -> bool {
    value.is_null()
}

// Serialize a manifest into JSON bytes for the wire path. Infallible because
// the descriptor types contain only owned `String`/`Vec`/`serde_json::Value`
// fields — none of which can fail to serialize.
fn serialize_manifest(manifest: &Manifest) -> bytes::Bytes {
    let bytes = serde_json::to_vec(manifest)
        .expect("Manifest serialization is infallible for our descriptor types");
    bytes::Bytes::from(bytes)
}

// ─────────────────────────────────────────────────────────────────────────────
// DashboardRegistry
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct RegistryState {
    nav_items: Vec<NavItem>,
    sections: Vec<Section>,
    panels: Vec<Panel>,
}

impl RegistryState {
    fn snapshot(&self) -> Manifest {
        Manifest {
            nav_items: self.nav_items.clone(),
            sections: self.sections.clone(),
            panels: self.panels.clone(),
        }
    }
}

/// Cross-plugin contribution registry.
///
/// Plugins contribute nav items, sections, and panels in their `build()`
/// phase using the chained-builder methods. Build-time suppression is
/// available via the `remove_*` methods (silent no-op when the id is
/// absent), so a downstream plugin can hide an upstream contribution
/// without forking it.
///
/// # Referential Integrity
///
/// The registry **does not validate cross-references**. `Section::nav_item_id`
/// and `Panel::section_id` are accepted as opaque strings; if a referenced
/// id is never contributed, the manifest will simply contain a dangling
/// reference. Frontends are expected to filter or surface orphans as they
/// see fit. Validation belongs to the consumer because plugins compose
/// independently — the registry has no global view of which contributors
/// will run.
///
/// # Cloning
///
/// `DashboardRegistry` is cheaply cloneable (`Arc`-backed). All clones
/// share the same underlying state and broadcast channel, which is what
/// allows [`DashboardPlugin`](crate::DashboardPlugin)'s deferred manifest
/// route builder to capture a clone and still observe the frozen snapshot.
///
/// # Lifecycle
///
/// 1. `DashboardPlugin::build()` registers an empty registry as an [`API`].
/// 2. Other plugins call `add_*` / `remove_*` in their own `build()` —
///    contributions accumulate until [`freeze`](Self::freeze) is called.
/// 3. `DashboardPlugin::ready()` calls [`freeze`](Self::freeze), which
///    captures an immutable [`Manifest`], pre-serializes its JSON bytes,
///    and broadcasts [`RegistryEvent::Ready`].
///
/// # Interior Mutability
///
/// All methods take `&self` and use a `RwLock` for the live state and a
/// `OnceLock` for the frozen snapshot. This is required because
/// `server.api::<DashboardRegistry>()` returns `&DashboardRegistry`.
#[derive(Clone)]
pub struct DashboardRegistry {
    state: Arc<RwLock<RegistryState>>,
    frozen: Arc<OnceLock<FrozenSnapshot>>,
    events: broadcast::Sender<RegistryEvent>,
}

/// Immutable post-freeze snapshot: the [`Manifest`] plus its pre-serialized
/// JSON wire bytes, so the manifest endpoint can serve a shared `Bytes`
/// without re-cloning or re-serializing per request.
#[derive(Clone)]
struct FrozenSnapshot {
    manifest: Arc<Manifest>,
    json: bytes::Bytes,
}

impl Default for DashboardRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DashboardRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.read();
        f.debug_struct("DashboardRegistry")
            .field("nav_items", &state.nav_items.len())
            .field("sections", &state.sections.len())
            .field("panels", &state.panels.len())
            .field("frozen", &self.frozen.get().is_some())
            .finish()
    }
}

impl API for DashboardRegistry {}

impl DashboardRegistry {
    /// Creates a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            state: Arc::new(RwLock::new(RegistryState::default())),
            frozen: Arc::new(OnceLock::new()),
            events,
        }
    }

    /// Returns the cached pre-serialized JSON bytes of the frozen
    /// [`Manifest`], if [`freeze`](Self::freeze) has been called.
    ///
    /// Used by the manifest endpoint to serve a shared [`bytes::Bytes`]
    /// without re-cloning or re-serializing per request. Pre-freeze callers
    /// fall back to serializing the live snapshot.
    #[must_use]
    pub fn manifest_bytes(&self) -> bytes::Bytes {
        match self.frozen.get() {
            Some(snapshot) => snapshot.json.clone(),
            None => serialize_manifest(&self.state.read().snapshot()),
        }
    }

    // ── Add ─────────────────────────────────────────────────────────────────

    /// Adds a [`NavItem`] to the registry. Chained-builder.
    pub fn add_nav_item(&self, item: NavItem) -> &Self {
        self.state.write().nav_items.push(item);
        self
    }

    /// Adds a [`Section`] to the registry. Chained-builder.
    pub fn add_section(&self, section: Section) -> &Self {
        self.state.write().sections.push(section);
        self
    }

    /// Adds a [`Panel`] to the registry. Chained-builder.
    pub fn add_panel(&self, panel: Panel) -> &Self {
        self.state.write().panels.push(panel);
        self
    }

    // ── Remove (build-time suppression) ─────────────────────────────────────

    /// Removes a [`NavItem`] by id. Silent no-op when the id is absent.
    ///
    /// Use this in a downstream plugin's `build()` to suppress an upstream
    /// contribution. Polaris already orders plugins by dependency, so a
    /// plugin that depends on the contributor will run after it and see
    /// the id present.
    pub fn remove_nav_item(&self, id: &str) -> &Self {
        self.state.write().nav_items.retain(|item| item.id != id);
        self
    }

    /// Removes a [`Section`] by id. Silent no-op when the id is absent.
    pub fn remove_section(&self, id: &str) -> &Self {
        self.state
            .write()
            .sections
            .retain(|section| section.id != id);
        self
    }

    /// Removes a [`Panel`] by id. Silent no-op when the id is absent.
    pub fn remove_panel(&self, id: &str) -> &Self {
        self.state.write().panels.retain(|panel| panel.id != id);
        self
    }

    // ── Snapshot ────────────────────────────────────────────────────────────

    /// Captures an immutable [`Manifest`] from the current state,
    /// pre-serializes its JSON wire bytes, and broadcasts
    /// [`RegistryEvent::Ready`].
    ///
    /// Called once by [`DashboardPlugin::ready`](crate::DashboardPlugin) —
    /// subsequent calls are no-ops to keep the snapshot stable.
    pub fn freeze(&self) -> Arc<Manifest> {
        if let Some(existing) = self.frozen.get() {
            return Arc::clone(&existing.manifest);
        }
        let manifest = Arc::new(self.state.read().snapshot());
        let json = serialize_manifest(&manifest);
        let snapshot = FrozenSnapshot {
            manifest: Arc::clone(&manifest),
            json,
        };
        // `set` returns Err if a concurrent caller raced ahead; in that case
        // the other snapshot is canonical and we discard ours.
        match self.frozen.set(snapshot) {
            Ok(()) => {
                let _ = self
                    .events
                    .send(RegistryEvent::Ready(Arc::clone(&manifest)));
                manifest
            }
            Err(_) => Arc::clone(
                &self
                    .frozen
                    .get()
                    .expect("frozen must be set after a failed set()")
                    .manifest,
            ),
        }
    }

    /// Returns the frozen [`Manifest`] if [`freeze`](Self::freeze) has been
    /// called, otherwise a fresh snapshot of the current state.
    ///
    /// Frontends should rely on the frozen snapshot via the manifest
    /// endpoint; this fallback only matters during the brief window
    /// between server startup and `DashboardPlugin::ready()` running.
    pub fn manifest(&self) -> Arc<Manifest> {
        match self.frozen.get() {
            Some(snapshot) => Arc::clone(&snapshot.manifest),
            None => Arc::new(self.state.read().snapshot()),
        }
    }

    /// Subscribes to [`RegistryEvent`]s.
    ///
    /// New subscribers receive events emitted *after* they subscribe. Call
    /// before `DashboardPlugin::ready()` to avoid missing
    /// [`RegistryEvent::Ready`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RegistryEvent> {
        self.events.subscribe()
    }

    /// Returns a clone of the broadcast sender for advanced use cases.
    #[must_use]
    pub fn events(&self) -> broadcast::Sender<RegistryEvent> {
        self.events.clone()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(id: &str) -> NavItem {
        NavItem::new(id, format!("label-{id}"))
    }

    fn section(id: &str, nav_id: &str) -> Section {
        Section::new(id, nav_id, format!("title-{id}"))
    }

    fn panel(id: &str) -> Panel {
        Panel::new(id, format!("title-{id}"), "list", "/x", Transport::Rest)
    }

    #[test]
    fn add_methods_are_chainable_and_accumulate() {
        let registry = DashboardRegistry::new();
        registry
            .add_nav_item(nav("a"))
            .add_nav_item(nav("b"))
            .add_section(section("s", "a"))
            .add_panel(panel("p1"))
            .add_panel(panel("p2"));

        let manifest = registry.manifest();
        assert_eq!(manifest.nav_items.len(), 2);
        assert_eq!(manifest.sections.len(), 1);
        assert_eq!(manifest.panels.len(), 2);
        assert_eq!(manifest.nav_items[0].id, "a");
        assert_eq!(manifest.nav_items[1].id, "b");
    }

    #[test]
    fn remove_drops_matching_id() {
        let registry = DashboardRegistry::new();
        registry
            .add_nav_item(nav("keep"))
            .add_nav_item(nav("drop"))
            .remove_nav_item("drop");

        let manifest = registry.manifest();
        assert_eq!(manifest.nav_items.len(), 1);
        assert_eq!(manifest.nav_items[0].id, "keep");
    }

    #[test]
    fn remove_unknown_id_is_silent_no_op() {
        let registry = DashboardRegistry::new();
        registry.add_panel(panel("present"));
        registry.remove_panel("absent");
        registry.remove_section("absent");
        registry.remove_nav_item("absent");

        let manifest = registry.manifest();
        assert_eq!(manifest.panels.len(), 1);
    }

    #[test]
    fn freeze_captures_state_and_subsequent_changes_are_invisible() {
        let registry = DashboardRegistry::new();
        registry.add_nav_item(nav("before"));
        let snapshot = registry.freeze();
        assert_eq!(snapshot.nav_items.len(), 1);

        // Mutating after freeze must not change the frozen snapshot.
        registry.add_nav_item(nav("after"));
        let again = registry.manifest();
        assert_eq!(again.nav_items.len(), 1);
        assert_eq!(again.nav_items[0].id, "before");
    }

    #[test]
    fn freeze_is_idempotent() {
        let registry = DashboardRegistry::new();
        registry.add_nav_item(nav("a"));
        let first = registry.freeze();
        registry.add_nav_item(nav("b"));
        let second = registry.freeze();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.nav_items.len(), 1);
    }

    #[tokio::test]
    async fn freeze_broadcasts_ready_event() {
        let registry = DashboardRegistry::new();
        let mut rx = registry.subscribe();
        registry.add_panel(panel("p"));
        registry.freeze();

        let event = rx
            .recv()
            .await
            .expect("Ready event must be broadcast on freeze");
        let RegistryEvent::Ready(manifest) = event;
        assert_eq!(manifest.panels.len(), 1);
        assert_eq!(manifest.panels[0].id, "p");
    }

    #[test]
    fn manifest_falls_back_to_live_snapshot_before_freeze() {
        let registry = DashboardRegistry::new();
        registry.add_panel(panel("live"));
        let manifest = registry.manifest();
        assert_eq!(manifest.panels.len(), 1);
        assert_eq!(manifest.panels[0].id, "live");
    }

    #[test]
    fn descriptors_serialize_with_optional_metadata_omitted() {
        let nav_item = NavItem::new("a", "A");
        let json = serde_json::to_value(&nav_item).unwrap();
        // Null metadata should be skipped to keep the wire format compact.
        assert!(json.get("metadata").is_none());
        assert_eq!(json["id"], "a");
        assert_eq!(json["label"], "A");
    }

    #[test]
    fn panel_with_metadata_round_trips() {
        let panel = Panel::new("p", "Title", "kv", "/v1/p", Transport::Sse)
            .with_section("sec")
            .with_metadata(serde_json::json!({ "schema": { "kind": "string" } }));
        let json = serde_json::to_value(&panel).unwrap();
        let back: Panel = serde_json::from_value(json).unwrap();
        assert_eq!(back, panel);
    }

    #[test]
    fn transport_websocket_serializes_as_kebab_case() {
        // Pin the kebab-case wire format so renames don't drift past us.
        let panel = Panel::new("p", "T", "list", "/x", Transport::WebSocket);
        let json = serde_json::to_value(&panel).unwrap();
        assert_eq!(json["transport"], "web-socket");

        let back: Panel = serde_json::from_value(json).unwrap();
        assert_eq!(back.transport, Transport::WebSocket);
    }

    #[test]
    fn manifest_bytes_serves_pre_serialized_snapshot_after_freeze() {
        let registry = DashboardRegistry::new();
        registry.add_panel(panel("p"));
        registry.freeze();

        let bytes_first = registry.manifest_bytes();
        let bytes_second = registry.manifest_bytes();

        // Post-freeze the cached `Bytes` must be ref-counted, not re-serialized.
        // `Bytes::clone` is `Arc`-shallow, so the underlying buffer is shared.
        assert_eq!(bytes_first.as_ptr(), bytes_second.as_ptr());

        // Wire format round-trips back into a Manifest.
        let parsed: Manifest = serde_json::from_slice(&bytes_first).unwrap();
        assert_eq!(parsed.panels.len(), 1);
        assert_eq!(parsed.panels[0].id, "p");
    }

    #[test]
    fn manifest_bytes_falls_back_to_live_snapshot_before_freeze() {
        let registry = DashboardRegistry::new();
        registry.add_panel(panel("live"));
        let bytes = registry.manifest_bytes();
        let parsed: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.panels.len(), 1);
        assert_eq!(parsed.panels[0].id, "live");
    }

    #[test]
    fn empty_registry_serializes_to_empty_collections() {
        // Realistic startup state: plugin enabled, no contributors yet.
        let registry = DashboardRegistry::new();
        let manifest = registry.manifest();
        assert!(manifest.nav_items.is_empty());
        assert!(manifest.sections.is_empty());
        assert!(manifest.panels.is_empty());

        let bytes = registry.manifest_bytes();
        let parsed: Manifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, Manifest::default());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_freeze_is_safe_and_consistent() {
        // Exercise the `OnceLock::set` race fallback in `freeze()` —
        // `registry.rs:freeze()` handles the case where two callers race
        // and only one wins the `set`. Spawn a fan-out of freeze tasks and
        // verify they all observe the same canonical snapshot.
        let registry = DashboardRegistry::new();
        registry.add_panel(panel("racer"));

        let mut handles = Vec::with_capacity(16);
        for _ in 0..16 {
            let reg = registry.clone();
            handles.push(tokio::spawn(async move { reg.freeze() }));
        }
        let mut snapshots = Vec::with_capacity(handles.len());
        for handle in handles {
            snapshots.push(handle.await.expect("freeze task must not panic"));
        }
        let canonical = &snapshots[0];
        for snapshot in &snapshots[1..] {
            assert!(
                Arc::ptr_eq(canonical, snapshot),
                "all racing freeze() callers must observe the same Arc<Manifest>"
            );
        }
        assert_eq!(canonical.panels.len(), 1);
    }
}
