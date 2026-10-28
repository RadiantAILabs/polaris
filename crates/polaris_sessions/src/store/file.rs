//! File-based session store.
//!
//! Each session is stored as a JSON file at `<base_dir>/<session_id>.json`.
//! Requires the `file-store` feature.

use super::bounded::{self, Overflow, ParseFailure};
use super::{SessionData, SessionId, SessionStore};
use crate::error::SessionError;
use polaris_system::system::BoxFuture;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// Default maximum bytes read from one session file: 64 MiB.
const DEFAULT_MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum persisted checkpoint or tombstone vector length.
const DEFAULT_MAX_CHECKPOINTS: usize = 1024;

/// Input bounds applied when [`FileStore`] loads persisted JSON.
///
/// The defaults admit the standard 64-checkpoint retention policy with ample
/// room for custom policies, while preventing an untrusted file from driving
/// unbounded reads or checkpoint-vector allocation. Hosts with deliberately
/// larger stores can supply explicit non-zero limits. The default file bound
/// is 64 MiB and the default vector bound is 1,024 entries each.
///
/// The same bounds apply when saving: a session that would write a file the
/// store then refuses to load is rejected at [`save`](SessionStore::save), so
/// a stored session can always be resumed. Under
/// [`CheckpointRetention::unbounded`](crate::checkpoint::CheckpointRetention::unbounded)
/// a long-lived session can reach the bounds, so raise them to match.
///
/// # Examples
///
/// Allow files up to 256 MiB holding up to 4,096 checkpoints:
///
/// ```no_run
/// use polaris_sessions::{FileStore, FileStoreLimits};
/// use std::num::NonZeroUsize;
///
/// let limits = FileStoreLimits::new(
///     NonZeroUsize::new(256 * 1024 * 1024).expect("non-zero"),
///     NonZeroUsize::new(4096).expect("non-zero"),
/// );
/// let store = FileStore::new("/var/lib/my-agent/sessions").with_load_limits(limits);
/// assert_eq!(store.load_limits(), limits);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStoreLimits {
    /// Maximum bytes read from one session file.
    pub max_file_bytes: NonZeroUsize,
    /// Maximum entries accepted in either the retained-checkpoint vector or
    /// its tombstone vector.
    pub max_checkpoints: NonZeroUsize,
}

impl FileStoreLimits {
    /// Creates explicit non-zero load limits.
    #[must_use]
    pub const fn new(max_file_bytes: NonZeroUsize, max_checkpoints: NonZeroUsize) -> Self {
        Self {
            max_file_bytes,
            max_checkpoints,
        }
    }
}

impl Default for FileStoreLimits {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_MAX_FILE_BYTES).expect("default file limit is non-zero"),
            NonZeroUsize::new(DEFAULT_MAX_CHECKPOINTS)
                .expect("default checkpoint limit is non-zero"),
        )
    }
}

/// A [`SessionStore`] that persists each session as a JSON file on disk.
///
/// File layout: `<base_dir>/<session_id>.json`.
#[derive(Debug)]
pub struct FileStore {
    base_dir: PathBuf,
    limits: FileStoreLimits,
}

impl FileStore {
    /// Creates a new file store rooted at `base_dir`.
    ///
    /// The directory is created lazily on the first write.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            limits: FileStoreLimits::default(),
        }
    }

    /// Overrides the bounds a session file must stay within.
    ///
    /// Loading refuses a file past either bound, and saving refuses a session
    /// that would write one. See [`FileStoreLimits`].
    ///
    /// # Examples
    ///
    /// Refuse sessions past 1 MiB or 32 checkpoints, at save and at load:
    ///
    /// ```no_run
    /// use polaris_sessions::{FileStore, FileStoreLimits};
    /// use std::num::NonZeroUsize;
    ///
    /// let store = FileStore::new("./sessions").with_load_limits(FileStoreLimits::new(
    ///     NonZeroUsize::new(1024 * 1024).expect("non-zero"),
    ///     NonZeroUsize::new(32).expect("non-zero"),
    /// ));
    /// ```
    #[must_use]
    pub fn with_load_limits(mut self, limits: FileStoreLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Returns the configured input bounds.
    #[must_use]
    pub fn load_limits(&self) -> FileStoreLimits {
        self.limits
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

/// Reads at most `max_bytes + 1` bytes so oversize detection itself remains
/// bounded.
///
/// Blocking: call from Tokio's blocking pool, never from an async worker.
fn read_limited(path: &Path, max_bytes: usize) -> Result<Vec<u8>, FileStoreError> {
    let file = std::fs::File::open(path).map_err(|source| FileStoreError::Read {
        path: path.to_owned(),
        source,
    })?;
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut reader = file.take(read_limit);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|source| FileStoreError::Read {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > max_bytes {
        return Err(FileStoreError::FileTooLarge {
            path: path.to_owned(),
            max_bytes,
        });
    }
    Ok(bytes)
}

/// Reads, parses and validates one session file; `Ok(None)` if it is absent.
///
/// Blocking: call from Tokio's blocking pool, never from an async worker.
/// Parsing a file near the byte bound takes long enough to stall one.
fn load_blocking(
    path: &Path,
    limits: FileStoreLimits,
) -> Result<Option<SessionData>, FileStoreError> {
    let bytes = match read_limited(path, limits.max_file_bytes.get()) {
        Ok(bytes) => bytes,
        Err(FileStoreError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    // serde_json's default recursion limit remains enabled, bounding nested
    // arrays/objects to 128 levels.
    let max_count = limits.max_checkpoints.get();
    bounded::parse_session(&bytes, max_count)
        .map(Some)
        .map_err(|failure| match failure {
            ParseFailure::Malformed(source) => FileStoreError::Deserialize {
                path: path.to_owned(),
                source,
            },
            ParseFailure::TooMany(Overflow::Checkpoints) => FileStoreError::TooManyCheckpoints {
                path: path.to_owned(),
                max_count,
            },
            ParseFailure::TooMany(Overflow::Tombstones) => FileStoreError::TooManyTombstones {
                path: path.to_owned(),
                max_count,
            },
        })
}

impl SessionStore for FileStore {
    fn save(&self, id: &SessionId, data: &SessionData) -> BoxFuture<'_, Result<(), SessionError>> {
        let path = match self.path_for(id) {
            Ok(path) => path,
            Err(source) => return Box::pin(async move { Err(source.into()) }),
        };
        let data = data.clone();
        let limits = self.limits;
        Box::pin(async move {
            // Refuse what `load` would refuse, before touching the disk, so a
            // stored session can always be resumed.
            let max_count = limits.max_checkpoints.get();
            if data.checkpoints.len() > max_count {
                return Err(FileStoreError::SaveTooManyCheckpoints {
                    path,
                    count: data.checkpoints.len(),
                    max_count,
                }
                .into());
            }
            if data.checkpoint_history.tombstones.len() > max_count {
                return Err(FileStoreError::SaveTooManyTombstones {
                    path,
                    count: data.checkpoint_history.tombstones.len(),
                    max_count,
                }
                .into());
            }

            let json =
                serde_json::to_vec_pretty(&data).map_err(|source| FileStoreError::Serialize {
                    path: path.clone(),
                    source,
                })?;
            let max_bytes = limits.max_file_bytes.get();
            if json.len() > max_bytes {
                return Err(FileStoreError::SaveTooLarge {
                    path,
                    bytes: json.len(),
                    max_bytes,
                }
                .into());
            }

            tokio::fs::create_dir_all(&self.base_dir)
                .await
                .map_err(|source| FileStoreError::CreateDir {
                    path: self.base_dir.clone(),
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
        let limits = self.limits;
        Box::pin(async move {
            let load_path = path.clone();
            match tokio::task::spawn_blocking(move || load_blocking(&load_path, limits)).await {
                Ok(loaded) => Ok(loaded?),
                Err(source) => Err(FileStoreError::ReadTask { path, source }.into()),
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

    /// A session file exceeds the configured read bound.
    #[error("session file '{}' exceeds the {max_bytes}-byte load limit", path.display())]
    FileTooLarge {
        /// The rejected file path.
        path: PathBuf,
        /// Maximum configured file size.
        max_bytes: usize,
    },

    /// A blocking file-read task could not be joined.
    #[error("failed to join read task for '{}': {source}", path.display())]
    ReadTask {
        /// The source file path.
        path: PathBuf,
        /// The blocking task failure.
        source: tokio::task::JoinError,
    },

    /// A session file contains more retained checkpoints than the limit.
    ///
    /// Loading stops at the first entry past the limit, so the total is not
    /// known.
    #[error("session file '{}' contains more than the configured limit of {max_count} checkpoints", path.display())]
    TooManyCheckpoints {
        /// The rejected file path.
        path: PathBuf,
        /// Maximum configured vector length.
        max_count: usize,
    },

    /// A session file contains more checkpoint tombstones than the limit.
    ///
    /// Loading stops at the first entry past the limit, so the total is not
    /// known.
    #[error("session file '{}' contains more than the configured limit of {max_count} checkpoint tombstones", path.display())]
    TooManyTombstones {
        /// The rejected file path.
        path: PathBuf,
        /// Maximum configured vector length.
        max_count: usize,
    },

    /// A session was not saved because the file would exceed the load limit.
    ///
    /// Nothing was written; the previously saved file is unchanged. Raise
    /// [`FileStoreLimits::max_file_bytes`] with
    /// [`FileStore::with_load_limits`], or bound checkpoint retention.
    #[error("not saving '{}': the session serializes to {bytes} bytes, over the {max_bytes}-byte load limit, so it could not be resumed", path.display())]
    SaveTooLarge {
        /// The file that would have been written.
        path: PathBuf,
        /// Size of the serialized session.
        bytes: usize,
        /// Maximum configured file size.
        max_bytes: usize,
    },

    /// A session was not saved because it holds more checkpoints than the
    /// load limit allows.
    ///
    /// Nothing was written; the previously saved file is unchanged. Raise
    /// [`FileStoreLimits::max_checkpoints`] with
    /// [`FileStore::with_load_limits`], or bound checkpoint retention.
    #[error("not saving '{}': the session holds {count} checkpoints, over the load limit of {max_count}, so it could not be resumed", path.display())]
    SaveTooManyCheckpoints {
        /// The file that would have been written.
        path: PathBuf,
        /// Checkpoints the session holds.
        count: usize,
        /// Maximum configured vector length.
        max_count: usize,
    },

    /// A session was not saved because it holds more checkpoint tombstones
    /// than the load limit allows.
    ///
    /// Nothing was written; the previously saved file is unchanged. Raise
    /// [`FileStoreLimits::max_checkpoints`] with
    /// [`FileStore::with_load_limits`].
    #[error("not saving '{}': the session holds {count} checkpoint tombstones, over the load limit of {max_count}, so it could not be resumed", path.display())]
    SaveTooManyTombstones {
        /// The file that would have been written.
        path: PathBuf,
        /// Tombstones the session holds.
        count: usize,
        /// Maximum configured vector length.
        max_count: usize,
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
    use crate::checkpoint::{
        CheckpointId, CheckpointMeta, CheckpointOrigin, CheckpointTombstone, RevisionId,
        StoredCheckpoint,
    };
    use crate::store::ResourceEntry;

    fn limits(max_file_bytes: usize, max_checkpoints: usize) -> FileStoreLimits {
        FileStoreLimits::new(
            NonZeroUsize::new(max_file_bytes).unwrap(),
            NonZeroUsize::new(max_checkpoints).unwrap(),
        )
    }

    fn stored_checkpoint(raw: u64) -> StoredCheckpoint {
        StoredCheckpoint {
            meta: CheckpointMeta {
                id: CheckpointId::from_raw(raw),
                resume_turn: raw as u32,
                turn: raw as u32,
                parent: None,
                revision: RevisionId::default(),
                origin: CheckpointOrigin::Manual,
                taken_at: "2026-01-01T00:00:00Z".to_owned(),
                bytes: 0,
            },
            resources: vec![ResourceEntry {
                plugin_id: "test".to_owned(),
                storage_key: "Counter".to_owned(),
                version: "1.0.0".to_owned(),
                data: serde_json::json!(raw),
            }],
        }
    }

    #[tokio::test]
    async fn round_trip_with_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());

        let id = SessionId::new();
        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);

        store.save(&id, &data).await.unwrap();

        let loaded = store.load(&id).await.unwrap().expect("should exist");
        assert_eq!(loaded.agent_type, "TestAgent");

        store.delete(&id).await.unwrap();
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_session_file_is_refused_with_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(64, 8));
        let id = SessionId::from_string("oversized");
        std::fs::write(store.path_for(&id).unwrap(), vec![b'x'; 65]).unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::FileTooLarge { max_bytes: 64, .. })
            ),
            "expected FileTooLarge, got {source:?}"
        );
    }

    #[tokio::test]
    async fn checkpoint_vector_length_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(4096, 1));
        let id = SessionId::from_string("too-many-checkpoints");
        let mut data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);
        data.checkpoints = vec![stored_checkpoint(1), stored_checkpoint(2)];
        std::fs::write(
            store.path_for(&id).unwrap(),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::TooManyCheckpoints { max_count: 1, .. })
            ),
            "expected TooManyCheckpoints, got {source:?}"
        );
    }

    #[tokio::test]
    async fn checkpoint_tombstone_vector_length_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(4096, 1));
        let id = SessionId::from_string("too-many-tombstones");
        let mut data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);
        data.checkpoint_history.tombstones = vec![
            CheckpointTombstone {
                id: CheckpointId::from_raw(1),
                evicted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            CheckpointTombstone {
                id: CheckpointId::from_raw(2),
                evicted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
        ];
        std::fs::write(
            store.path_for(&id).unwrap(),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::TooManyTombstones { max_count: 1, .. })
            ),
            "expected TooManyTombstones, got {source:?}"
        );
    }

    #[tokio::test]
    async fn checkpoint_entries_past_the_limit_are_never_deserialized() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 1));
        let id = SessionId::from_string("unread-checkpoints");
        let valid = serde_json::to_string(&stored_checkpoint(1)).unwrap();
        // The second entry is not a checkpoint at all. A parser that builds
        // every entry fails on it; one that stops at the limit never looks.
        let json = format!(
            r#"{{"agent_type":"TestAgent","turn_number":0,"created_at":"2026-01-01T00:00:00Z","resources":[],"checkpoints":[{valid},{{"meta":"garbage"}},{{"meta":"garbage"}}]}}"#
        );
        std::fs::write(store.path_for(&id).unwrap(), json).unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::TooManyCheckpoints { max_count: 1, .. })
            ),
            "expected TooManyCheckpoints, got {source:?}"
        );
    }

    #[tokio::test]
    async fn tombstone_entries_past_the_limit_are_never_deserialized() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 1));
        let id = SessionId::from_string("unread-tombstones");
        let json = r#"{"agent_type":"TestAgent","turn_number":0,"created_at":"2026-01-01T00:00:00Z","resources":[],"checkpoint_history":{"tombstones":[{"id":1,"evicted_at":"2026-01-01T00:00:00Z"},{"id":"garbage"},{"id":"garbage"}]}}"#;
        std::fs::write(store.path_for(&id).unwrap(), json).unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::TooManyTombstones { max_count: 1, .. })
            ),
            "expected TooManyTombstones, got {source:?}"
        );
    }

    #[tokio::test]
    async fn parsing_a_large_file_does_not_block_the_async_worker() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let id = SessionId::from_string("large");
        let resources: Vec<ResourceEntry> = (0..120_000)
            .map(|index| ResourceEntry {
                plugin_id: "test".to_owned(),
                storage_key: format!("Key{index}"),
                version: "1.0.0".to_owned(),
                data: serde_json::json!({ "index": index, "label": "padding padding padding" }),
            })
            .collect();
        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", resources);
        let bytes = serde_json::to_vec(&data).unwrap();
        std::fs::write(store.path_for(&id).unwrap(), &bytes).unwrap();

        // How long parsing this document takes when done inline.
        let started = std::time::Instant::now();
        let parsed: SessionData = serde_json::from_slice(&bytes).unwrap();
        let inline_parse = started.elapsed();
        assert_eq!(parsed.resources.len(), 120_000);

        // The longest single poll of the load future is how long the async
        // worker was held. Parsing on the worker holds it for the whole parse.
        let mut load = Box::pin(store.load(&id));
        let mut longest_poll = std::time::Duration::ZERO;
        let loaded = std::future::poll_fn(|cx| {
            let started = std::time::Instant::now();
            let poll = load.as_mut().poll(cx);
            longest_poll = longest_poll.max(started.elapsed());
            poll
        })
        .await
        .unwrap()
        .expect("should exist");

        assert_eq!(loaded.resources.len(), 120_000);
        assert!(
            longest_poll < inline_parse / 4,
            "a poll held the async worker for {longest_poll:?}; parsing takes {inline_parse:?}"
        );
    }

    fn file_store_error(err: SessionError) -> FileStoreError {
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        *source
            .downcast::<FileStoreError>()
            .expect("expected a file store error")
    }

    fn tombstone(raw: u64) -> CheckpointTombstone {
        CheckpointTombstone {
            id: CheckpointId::from_raw(raw),
            evicted_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn file_at_the_byte_limit_loads_and_one_byte_over_is_refused_at_save() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::from_string("byte-boundary");
        let data = SessionData::new("TestAgent", 3, "2026-01-01T00:00:00Z", vec![]);
        let default_store = FileStore::new(dir.path());
        default_store.save(&id, &data).await.unwrap();
        let path = default_store.path_for(&id).unwrap();
        let size = std::fs::metadata(&path).unwrap().len() as usize;
        let before = std::fs::read(&path).unwrap();

        let exact = FileStore::new(dir.path()).with_load_limits(limits(size, 8));
        exact.save(&id, &data).await.unwrap();
        assert_eq!(exact.load(&id).await.unwrap().unwrap().turn_number, 3);

        let tight = FileStore::new(dir.path()).with_load_limits(limits(size - 1, 8));
        let err = file_store_error(tight.save(&id, &data).await.unwrap_err());
        assert!(
            matches!(
                err,
                FileStoreError::SaveTooLarge { bytes, max_bytes, .. }
                    if bytes == size && max_bytes == size - 1
            ),
            "expected SaveTooLarge, got {err:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused save must leave the stored session untouched"
        );
        let err = file_store_error(tight.load(&id).await.unwrap_err());
        assert!(
            matches!(err, FileStoreError::FileTooLarge { max_bytes, .. } if max_bytes == size - 1),
            "expected FileTooLarge, got {err:?}"
        );
    }

    #[tokio::test]
    async fn checkpoints_at_the_count_limit_load_and_one_over_is_refused_at_save() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::from_string("count-boundary");
        let mut data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);
        data.checkpoints = vec![stored_checkpoint(1), stored_checkpoint(2)];

        let exact = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 2));
        exact.save(&id, &data).await.unwrap();
        assert_eq!(exact.load(&id).await.unwrap().unwrap().checkpoints.len(), 2);

        let tight = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 1));
        let other = SessionId::from_string("count-boundary-refused");
        let err = file_store_error(tight.save(&other, &data).await.unwrap_err());
        assert!(
            matches!(
                err,
                FileStoreError::SaveTooManyCheckpoints {
                    count: 2,
                    max_count: 1,
                    ..
                }
            ),
            "expected SaveTooManyCheckpoints, got {err:?}"
        );
        assert!(
            !tight.path_for(&other).unwrap().exists(),
            "a refused save must not create the file"
        );
    }

    #[tokio::test]
    async fn tombstones_at_the_count_limit_load_and_one_over_is_refused_at_save() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId::from_string("tombstone-boundary");
        let mut data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);
        data.checkpoint_history.tombstones = vec![tombstone(1), tombstone(2)];

        let exact = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 2));
        exact.save(&id, &data).await.unwrap();
        let loaded = exact.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.checkpoint_history.tombstones.len(), 2);

        let tight = FileStore::new(dir.path()).with_load_limits(limits(1 << 20, 1));
        let other = SessionId::from_string("tombstone-boundary-refused");
        let err = file_store_error(tight.save(&other, &data).await.unwrap_err());
        assert!(
            matches!(
                err,
                FileStoreError::SaveTooManyTombstones {
                    count: 2,
                    max_count: 1,
                    ..
                }
            ),
            "expected SaveTooManyTombstones, got {err:?}"
        );
        assert!(!tight.path_for(&other).unwrap().exists());
    }

    #[tokio::test]
    async fn session_written_before_checkpoints_existed_loads_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let id = SessionId::from_string("legacy");
        let json = r#"{
            "agent_type": "TestAgent",
            "turn_number": 4,
            "created_at": "2025-06-01T00:00:00Z",
            "resources": [
                {"plugin_id": "test", "storage_key": "Counter", "version": "1.0.0", "data": 4}
            ]
        }"#;
        std::fs::write(store.path_for(&id).unwrap(), json).unwrap();

        let loaded = store.load(&id).await.unwrap().expect("should exist");

        assert_eq!(loaded.turn_number, 4);
        assert_eq!(loaded.resources.len(), 1);
        assert!(loaded.checkpoints.is_empty());
        assert_eq!(loaded.next_checkpoint_id(), None);
        assert_eq!(loaded.revision, RevisionId::default());
        assert!(loaded.checkpoint_history.is_empty());
    }

    #[tokio::test]
    async fn every_session_field_survives_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let id = SessionId::from_string("full");
        let mut data = SessionData::new(
            "TestAgent",
            5,
            "2026-01-01T00:00:00Z",
            vec![ResourceEntry {
                plugin_id: "test".to_owned(),
                storage_key: "Counter".to_owned(),
                version: "1.0.0".to_owned(),
                data: serde_json::json!({ "count": 5 }),
            }],
        );
        data.checkpoints = vec![stored_checkpoint(1), stored_checkpoint(2)];
        data.next_checkpoint = 7;
        data.revision = RevisionId::default().next();
        data.checkpoint_history.current = Some(CheckpointId::from_raw(2));
        data.checkpoint_history.tombstones = vec![tombstone(3)];
        data.checkpoint_history.floor = Some(tombstone(2));

        store.save(&id, &data).await.unwrap();
        let loaded = store.load(&id).await.unwrap().expect("should exist");

        let expected = serde_json::to_value(&data).unwrap();
        for key in [
            "agent_type",
            "turn_number",
            "created_at",
            "resources",
            "checkpoints",
            "next_checkpoint",
            "revision",
            "checkpoint_history",
        ] {
            assert!(expected.get(key).is_some(), "fixture is missing {key}");
        }
        for key in ["current", "tombstones", "floor"] {
            assert!(
                expected["checkpoint_history"].get(key).is_some(),
                "fixture is missing checkpoint_history.{key}"
            );
        }
        assert_eq!(serde_json::to_value(&loaded).unwrap(), expected);
    }

    #[tokio::test]
    async fn deeply_nested_json_is_refused_by_the_deserializer_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path()).with_load_limits(limits(4096, 8));
        let id = SessionId::from_string("too-deep");
        let nested = format!("{}null{}", "[".repeat(140), "]".repeat(140));
        let json = format!(
            r#"{{"agent_type":"TestAgent","turn_number":0,"created_at":"2026-01-01T00:00:00Z","resources":[{{"plugin_id":"test","storage_key":"Deep","version":"1.0.0","data":{nested}}}]}}"#
        );
        std::fs::write(store.path_for(&id).unwrap(), json).unwrap();

        let err = store.load(&id).await.unwrap_err();
        let SessionError::Store(source) = err else {
            panic!("expected a store error");
        };
        assert!(
            matches!(
                source.downcast_ref::<FileStoreError>(),
                Some(FileStoreError::Deserialize { source, .. })
                    if source.to_string().contains("recursion limit exceeded")
            ),
            "expected the serde_json depth bound, got {source:?}"
        );
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
        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);

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

        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);

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

        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", vec![]);

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
