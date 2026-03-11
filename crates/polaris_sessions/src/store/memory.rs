//! In-memory session store.

use super::{SessionData, SessionId, SessionStore};
use crate::error::SessionError;
use hashbrown::HashMap;
use parking_lot::RwLock;
use polaris_system::system::BoxFuture;

/// An in-memory [`SessionStore`] backed by a `HashMap` behind a read-write lock.
///
/// Data is lost when the process exits. Useful for testing and
/// short-lived applications that do not need durable persistence.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    data: RwLock<HashMap<SessionId, SessionData>>,
}

impl InMemoryStore {
    /// Creates a new, empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for InMemoryStore {
    fn save(&self, id: &SessionId, data: &SessionData) -> BoxFuture<'_, Result<(), SessionError>> {
        let id = id.clone();
        let data = data.clone();
        Box::pin(async move {
            self.data.write().insert(id, data);
            Ok(())
        })
    }

    fn load(&self, id: &SessionId) -> BoxFuture<'_, Result<Option<SessionData>, SessionError>> {
        let id = id.clone();
        Box::pin(async move { Ok(self.data.read().get(&id).cloned()) })
    }

    fn delete(&self, id: &SessionId) -> BoxFuture<'_, Result<(), SessionError>> {
        let id = id.clone();
        Box::pin(async move {
            self.data.write().remove(&id);
            Ok(())
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<SessionId>, SessionError>> {
        Box::pin(async move { Ok(self.data.read().keys().cloned().collect()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let store = InMemoryStore::new();
        let id = SessionId::from_string("test-1");
        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        store.save(&id, &data).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("should exist");
        assert_eq!(loaded.agent_type, "TestAgent");
        assert!(loaded.resources.is_empty());
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let store = InMemoryStore::new();
        let id = SessionId::from_string("nonexistent");
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let store = InMemoryStore::new();
        let id = SessionId::from_string("test-del");
        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        store.save(&id, &data).await.unwrap();
        store.delete(&id).await.unwrap();

        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_all_ids() {
        let store = InMemoryStore::new();
        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        let id1 = SessionId::from_string("a");
        let id2 = SessionId::from_string("b");
        store.save(&id1, &data).await.unwrap();
        store.save(&id2, &data).await.unwrap();

        let mut ids = store.list().await.unwrap();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_str(), "a");
        assert_eq!(ids[1].as_str(), "b");
    }
}
