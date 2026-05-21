//! Persistence trait for span/run history.
//!
//! Process restart wipes the in-memory in-memory span buffer, but
//! `polaris_sessions::SessionStore` keeps session identity and resources
//! alive across reboots. That mismatch leaves the dashboard unable to
//! show run history for a resumed session — even though the session is
//! still listed. [`SpanStore`] closes that gap by giving span history
//! the same lifetime as the session it describes.
//!
//! Records are keyed by the `session_id` correlation label (propagated by
//! `polaris_sessions::SessionsAPI` via the `polaris.label.session_id`
//! tracing field). Records that lack a `session_id` are not persisted —
//! they cannot be queried per-session anyway, so storing them would only
//! waste disk.
//!
//! # Composition
//!
//! `SpanStore` lives in `polaris_core_plugins::tracing_plugin` to avoid a
//! back-edge from `polaris_core_plugins` to `polaris_sessions`. The wire
//! shape of [`SpanRecord`](super::SpanRecord) is reused as-is.
//!
//! Plugins that want persistent run history register a [`SpanStorePlugin`]
//! alongside `TracingPlugin`. The plugin installs its own tracing layer
//! through `TracingLayers`, coexists with `OpenTelemetryPlugin`, and
//! when the `dashboard` feature is on its `ready()` hydrates the in-memory
//! `SpanBuffer` from the configured store so resumed sessions render
//! immediately after boot.
//!
//! # Backends
//!
//! - [`InMemorySpanStore`] — the default; trivially serves tests and
//!   processes that want the store API surface without touching disk.
//! - [`FileSpanStore`] — feature-gated on `file-store`, persists each
//!   session's records as a JSON-lines file at
//!   `<base_dir>/<session_id>.jsonl`.
//!
//! Custom backends (Postgres, S3, Redis, ...) implement the [`SpanStore`]
//! trait directly. The trait is intentionally narrow.

#[cfg(feature = "file-store")]
mod file;
mod memory;
mod plugin;

#[cfg(feature = "file-store")]
pub use file::{FileSpanStore, FileSpanStoreError};
pub use memory::InMemorySpanStore;
pub use plugin::{SpanStoreHandle, SpanStorePlugin};

use super::SpanRecord;
use polaris_system::system::BoxFuture;
use std::sync::Arc;

/// Convenient alias for the trait-object form callers usually hold.
pub type DynSpanStore = Arc<dyn SpanStore>;

/// Trait for durable span/run history backends.
///
/// Mirrors `polaris_sessions::SessionStore` in shape — async methods
/// boxed via [`BoxFuture`] for dyn-compatibility, `Send + Sync + 'static`
/// so the store can live behind an `Arc` shared across threads.
///
/// Records are keyed by `session_id`. A session that has produced N runs
/// will have all N runs' span and event records returned by
/// [`SpanStore::load`], in the order they were appended.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use polaris_core_plugins::{InMemorySpanStore, SpanStore};
///
/// let store: Arc<dyn SpanStore> = Arc::new(InMemorySpanStore::new());
/// ```
pub trait SpanStore: Send + Sync + 'static {
    /// Append one record to the given session's history.
    ///
    /// Callers should only invoke this for records that carry a
    /// `session_id` label. Implementations may treat unrelated session ids
    /// as opaque strings — no schema validation is performed.
    fn append(
        &self,
        session_id: &str,
        record: &SpanRecord,
    ) -> BoxFuture<'_, Result<(), SpanStoreError>>;

    /// Load every record stored for `session_id`, in append order.
    fn load(&self, session_id: &str) -> BoxFuture<'_, Result<Vec<SpanRecord>, SpanStoreError>>;

    /// Lists every `session_id` that has at least one record stored.
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, SpanStoreError>>;

    /// Delete every record stored for `session_id`. Missing sessions are not
    /// an error.
    fn delete(&self, session_id: &str) -> BoxFuture<'_, Result<(), SpanStoreError>>;
}

/// Errors returned by [`SpanStore`] implementations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpanStoreError {
    /// Backend-specific I/O or serialization failure.
    #[error("span store backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// The session id was rejected by the backend (e.g. contains a path
    /// separator for the file backend).
    #[error("invalid session id '{id}'")]
    InvalidSessionId {
        /// The rejected id.
        id: String,
    },
}
