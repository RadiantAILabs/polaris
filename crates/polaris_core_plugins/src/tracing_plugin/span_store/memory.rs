//! In-memory [`SpanStore`] backend.

use super::{SpanRecord, SpanStore, SpanStoreError};
use parking_lot::RwLock;
use polaris_system::system::BoxFuture;
use std::collections::HashMap;

/// An in-memory [`SpanStore`] backed by a `HashMap` behind a read-write
/// lock.
///
/// Persists nothing across process restart — useful as the default when
/// `SpanStorePlugin` is wired in without a durable backend, in tests, and
/// for short-lived applications. For cross-restart durability, swap in
/// [`FileSpanStore`](super::FileSpanStore) or a custom backend.
#[derive(Debug, Default)]
pub struct InMemorySpanStore {
    sessions: RwLock<HashMap<String, Vec<SpanRecord>>>,
}

impl InMemorySpanStore {
    /// Creates a new, empty in-memory span store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SpanStore for InMemorySpanStore {
    fn append(
        &self,
        session_id: &str,
        record: &SpanRecord,
    ) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        let session_id = session_id.to_owned();
        let record = record.clone();
        Box::pin(async move {
            self.sessions
                .write()
                .entry(session_id)
                .or_default()
                .push(record);
            Ok(())
        })
    }

    fn load(&self, session_id: &str) -> BoxFuture<'_, Result<Vec<SpanRecord>, SpanStoreError>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            Ok(self
                .sessions
                .read()
                .get(&session_id)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, SpanStoreError>> {
        Box::pin(async move { Ok(self.sessions.read().keys().cloned().collect()) })
    }

    fn delete(&self, session_id: &str) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            self.sessions.write().remove(&session_id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_plugin::SpanKind;
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn make(session: &str, name: &str) -> SpanRecord {
        SpanRecord {
            ts: "2026-05-17T00:00:00.000Z".into(),
            started_at: None,
            duration_ms: None,
            level: "info".into(),
            target: "tests".into(),
            name: name.into(),
            kind: SpanKind::Event,
            span_id: None,
            parent_span_id: None,
            run_id: None,
            labels: {
                let mut labels = BTreeMap::new();
                labels.insert("session_id".into(), session.into());
                labels
            },
            fields: Map::new(),
            message: None,
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_order() {
        let store = InMemorySpanStore::new();
        store.append("s1", &make("s1", "first")).await.unwrap();
        store.append("s1", &make("s1", "second")).await.unwrap();

        let loaded = store.load("s1").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "first");
        assert_eq!(loaded[1].name, "second");
    }

    #[tokio::test]
    async fn load_unknown_session_returns_empty() {
        let store = InMemorySpanStore::new();
        assert!(store.load("nope").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_returns_keys() {
        let store = InMemorySpanStore::new();
        store.append("a", &make("a", "x")).await.unwrap();
        store.append("b", &make("b", "y")).await.unwrap();

        let mut ids = store.list_sessions().await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn delete_drops_history() {
        let store = InMemorySpanStore::new();
        store.append("s", &make("s", "x")).await.unwrap();
        store.delete("s").await.unwrap();
        assert!(store.load("s").await.unwrap().is_empty());
    }
}
