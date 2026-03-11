//! File-based session store.
//!
//! Each session is stored as a JSON file at `<base_dir>/<session_id>.json`.
//! Requires the `file-store` feature.

use super::{SessionData, SessionId, SessionStore};
use crate::error::SessionError;
use polaris_system::system::BoxFuture;
use std::path::PathBuf;

/// A [`SessionStore`] that persists each session as a JSON file on disk.
///
/// File layout: `<base_dir>/<session_id>.json`.
#[derive(Debug)]
pub struct FileStore {
    base_dir: PathBuf,
}

impl FileStore {
    /// Creates a new file store rooted at `base_dir`.
    ///
    /// The directory is created lazily on the first write.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Returns the file path for a given session ID.
    ///
    /// Rejects IDs that contain path separators or would cause path traversal
    /// by verifying the resolved path remains a direct child of `base_dir`.
    fn path_for(&self, id: &SessionId) -> Result<PathBuf, FileStoreError> {
        let raw = id.as_str();
        if raw.chars().any(std::path::is_separator) {
            return Err(FileStoreError::InvalidId { id: raw.to_owned() });
        }

        let file_name = format!("{raw}.json");
        let path = self.base_dir.join(&file_name);

        if path.parent() != Some(&self.base_dir) || path.file_name() != Some(file_name.as_ref()) {
            return Err(FileStoreError::InvalidId { id: raw.to_owned() });
        }

        Ok(path)
    }
}

impl SessionStore for FileStore {
    fn save(&self, id: &SessionId, data: &SessionData) -> BoxFuture<'_, Result<(), SessionError>> {
        let path = match self.path_for(id) {
            Ok(path) => path,
            Err(source) => return Box::pin(async move { Err(source.into()) }),
        };
        let data = data.clone();
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.base_dir)
                .await
                .map_err(|source| FileStoreError::CreateDir {
                    path: self.base_dir.clone(),
                    source,
                })?;

            let json =
                serde_json::to_vec_pretty(&data).map_err(|source| FileStoreError::Serialize {
                    path: path.clone(),
                    source,
                })?;

            // Write to a temporary file in the same directory, then atomically
            // rename. This ensures a crash mid-write never leaves a corrupt
            // session file.
            let tmp_path = path.with_extension("json.tmp");

            tokio::fs::write(&tmp_path, json)
                .await
                .map_err(|source| FileStoreError::Write {
                    path: tmp_path.clone(),
                    source,
                })?;

            tokio::fs::rename(&tmp_path, &path)
                .await
                .map_err(|source| FileStoreError::Write {
                    path: path.clone(),
                    source,
                })?;

            Ok(())
        })
    }

    fn load(&self, id: &SessionId) -> BoxFuture<'_, Result<Option<SessionData>, SessionError>> {
        let path = match self.path_for(id) {
            Ok(path) => path,
            Err(source) => return Box::pin(async move { Err(source.into()) }),
        };
        Box::pin(async move {
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let data: SessionData = serde_json::from_slice(&bytes).map_err(|source| {
                        FileStoreError::Deserialize {
                            path: path.clone(),
                            source,
                        }
                    })?;
                    Ok(Some(data))
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(FileStoreError::Read { path, source }.into()),
            }
        })
    }

    fn delete(&self, id: &SessionId) -> BoxFuture<'_, Result<(), SessionError>> {
        let path = match self.path_for(id) {
            Ok(path) => path,
            Err(source) => return Box::pin(async move { Err(source.into()) }),
        };
        Box::pin(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(FileStoreError::Delete { path, source }.into()),
            }
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<SessionId>, SessionError>> {
        Box::pin(async move {
            let mut ids = Vec::new();

            let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
                Err(source) => {
                    return Err(FileStoreError::List {
                        path: self.base_dir.clone(),
                        source,
                    }
                    .into());
                }
            };

            while let Some(entry) =
                entries
                    .next_entry()
                    .await
                    .map_err(|source| FileStoreError::List {
                        path: self.base_dir.clone(),
                        source,
                    })?
            {
                let path = entry.path();

                let is_file = entry
                    .file_type()
                    .await
                    .map(|ft| ft.is_file())
                    .unwrap_or(false);

                if is_file
                    && path.extension().is_some_and(|ext| ext == "json")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    ids.push(SessionId::from_string(stem));
                }
            }

            Ok(ids)
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FileStoreError
// ─────────────────────────────────────────────────────────────────────────────

/// Errors specific to the file-based session store.
#[derive(Debug, thiserror::Error)]
pub enum FileStoreError {
    /// Failed to create the base directory.
    #[error("failed to create directory '{}': {source}", path.display())]
    CreateDir {
        /// The directory path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to write a session file.
    #[error("failed to write '{}': {source}", path.display())]
    Write {
        /// The file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to read a session file.
    #[error("failed to read '{}': {source}", path.display())]
    Read {
        /// The file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to delete a session file.
    #[error("failed to delete '{}': {source}", path.display())]
    Delete {
        /// The file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to list entries in the base directory.
    #[error("failed to list '{}': {source}", path.display())]
    List {
        /// The directory path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to serialize session data to JSON.
    #[error("failed to serialize session for '{}': {source}", path.display())]
    Serialize {
        /// The target file path.
        path: PathBuf,
        /// The underlying serialization error.
        source: serde_json::Error,
    },

    /// Failed to deserialize session data from JSON.
    #[error("failed to deserialize '{}': {source}", path.display())]
    Deserialize {
        /// The source file path.
        path: PathBuf,
        /// The underlying deserialization error.
        source: serde_json::Error,
    },

    /// The session ID contains path separators or would escape the base directory.
    #[error("invalid session id '{id}': contains path separator or traversal")]
    InvalidId {
        /// The rejected session ID.
        id: String,
    },
}

impl From<FileStoreError> for SessionError {
    fn from(err: FileStoreError) -> Self {
        SessionError::Store(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_with_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());

        let id = SessionId::new();
        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        store.save(&id, &data).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("should exist");
        assert_eq!(loaded.agent_type, "TestAgent");

        store.delete(&id).await.unwrap();
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());

        assert!(
            store
                .load(&SessionId::from_string("nope"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        // Forward slash
        let id = SessionId::from_string("../escape");
        let err = store.save(&id, &data).await.unwrap_err();
        assert!(err.to_string().contains("invalid session id"));

        // Backslash (only a separator on Windows)
        #[cfg(windows)]
        {
            let id = SessionId::from_string("..\\escape");
            let err = store.save(&id, &data).await.unwrap_err();
            assert!(err.to_string().contains("invalid session id"));
        }

        // Nested path
        let id = SessionId::from_string("sub/dir");
        let err = store.load(&id).await.unwrap_err();
        assert!(err.to_string().contains("invalid session id"));
    }

    #[tokio::test]
    async fn list_returns_json_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());

        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        store
            .save(&SessionId::from_string("x"), &data)
            .await
            .unwrap();
        store
            .save(&SessionId::from_string("y"), &data)
            .await
            .unwrap();

        let mut ids = store.list().await.unwrap();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_str(), "x");
        assert_eq!(ids[1].as_str(), "y");
    }

    #[tokio::test]
    async fn list_ignores_non_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());

        let data = SessionData {
            agent_type: "TestAgent".into(),
            turn_number: 0,
            resources: vec![],
        };

        // One real session
        store
            .save(&SessionId::from_string("real"), &data)
            .await
            .unwrap();

        // Non-.json file
        std::fs::write(dir.path().join("notes.txt"), b"not a session").unwrap();
        // Stale temp file from interrupted write
        std::fs::write(dir.path().join("stale.json.tmp"), b"{}").unwrap();
        // Hidden file
        std::fs::write(dir.path().join(".DS_Store"), b"").unwrap();
        // Subdirectory named with .json extension
        std::fs::create_dir_all(dir.path().join("subdir.json")).unwrap();

        let ids = store.list().await.unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].as_str(), "real");
    }
}
