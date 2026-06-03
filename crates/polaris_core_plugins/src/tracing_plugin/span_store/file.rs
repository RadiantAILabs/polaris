//! File-backed [`SpanStore`].
//!
//! Each session's history is stored as a JSON-lines file at
//! `<base_dir>/<session_id>.jsonl`. Append is the dominant write pattern, so
//! JSONL is a natural fit — one `serde_json::to_writer` + newline per
//! record, no read-modify-write, and corrupt tails (partial last line) are
//! recoverable by skipping while loading.

use super::{SpanRecord, SpanStore, SpanStoreError};
use polaris_system::system::BoxFuture;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

/// Maximum size of a single session file accepted by [`FileSpanStore::load`].
///
/// The plugin appends one JSON line per record and is the only writer, so a
/// file larger than this is corrupt or hostile. Capping the read keeps a bad
/// file on disk from turning `load` into an unbounded allocation.
const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// A [`SpanStore`] that persists each session's records as a JSON-lines
/// file on disk.
///
/// File layout: `<base_dir>/<session_id>.jsonl`. Records are appended one
/// per line in serialization order; load reads them back in append order
/// and silently skips any malformed trailing line (the only way a partial
/// write can land is when the process is killed mid-`write`).
#[derive(Debug)]
pub struct FileSpanStore {
    base_dir: PathBuf,
}

impl FileSpanStore {
    /// Creates a new file-backed span store rooted at `base_dir`.
    ///
    /// The directory is created lazily on the first write.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Returns the file path for a given session id.
    ///
    /// Rejects ids that contain path separators or would cause path
    /// traversal — same defence-in-depth check as
    /// [`FileStore`](polaris_sessions::FileStore).
    fn path_for(&self, session_id: &str) -> Result<PathBuf, SpanStoreError> {
        if session_id.chars().any(std::path::is_separator) {
            return Err(SpanStoreError::InvalidSessionId {
                id: session_id.to_owned(),
            });
        }

        let file_name = format!("{session_id}.jsonl");
        let path = self.base_dir.join(&file_name);

        if path.parent() != Some(&self.base_dir) || path.file_name() != Some(file_name.as_ref()) {
            return Err(SpanStoreError::InvalidSessionId {
                id: session_id.to_owned(),
            });
        }

        Ok(path)
    }
}

impl SpanStore for FileSpanStore {
    fn append(
        &self,
        session_id: &str,
        record: &SpanRecord,
    ) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        let path = match self.path_for(session_id) {
            Ok(path) => path,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let mut line = match serde_json::to_vec(record) {
            Ok(bytes) => bytes,
            Err(err) => {
                return Box::pin(async move {
                    Err(FileSpanStoreError::Serialize { path, source: err }.into())
                });
            }
        };
        line.push(b'\n');

        Box::pin(async move {
            tokio::fs::create_dir_all(&self.base_dir)
                .await
                .map_err(|source| FileSpanStoreError::CreateDir {
                    path: self.base_dir.clone(),
                    source,
                })?;

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|source| FileSpanStoreError::Open {
                    path: path.clone(),
                    source,
                })?;

            file.write_all(&line)
                .await
                .map_err(|source| FileSpanStoreError::Write {
                    path: path.clone(),
                    source,
                })?;
            // `tokio::fs::File` buffers writes through an internal worker
            // thread; the bytes are only guaranteed to be visible to other
            // readers after `flush` returns. Without this, a sibling call
            // — including `std::fs::read` on the same path immediately
            // afterwards — can see a zero-byte file under parallel test
            // pressure.
            file.flush()
                .await
                .map_err(|source| FileSpanStoreError::Write {
                    path: path.clone(),
                    source,
                })?;
            // `flush` only hands the bytes to the OS; they linger in the
            // page cache until the kernel decides to write them back. The
            // plugin promises a record survives a restart, so force the
            // data to stable storage before reporting success. Prefer
            // `append_batch` on the hot path — it pays this barrier once
            // per batch instead of once per record.
            file.sync_data()
                .await
                .map_err(|source| FileSpanStoreError::Write {
                    path: path.clone(),
                    source,
                })?;
            Ok(())
        })
    }

    fn append_batch<'a>(
        &'a self,
        records: &'a [(String, SpanRecord)],
    ) -> BoxFuture<'a, Result<(), SpanStoreError>> {
        Box::pin(async move {
            if records.is_empty() {
                return Ok(());
            }

            // Group by session, preserving each session's append order, so
            // we open + `fsync` each session file once for the whole batch
            // instead of once per record. A record that fails to serialize
            // is skipped with a warning rather than aborting the batch.
            let mut grouped: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
            for (session_id, record) in records {
                let buf = grouped.entry(session_id.as_str()).or_default();
                let mark = buf.len();
                match serde_json::to_writer(&mut *buf, record) {
                    Ok(()) => buf.push(b'\n'),
                    Err(source) => {
                        buf.truncate(mark);
                        tracing::warn!(
                            session_id = %session_id,
                            error = %source,
                            "FileSpanStore: skipping unserializable record in batch",
                        );
                    }
                }
            }

            tokio::fs::create_dir_all(&self.base_dir)
                .await
                .map_err(|source| FileSpanStoreError::CreateDir {
                    path: self.base_dir.clone(),
                    source,
                })?;

            for (session_id, bytes) in grouped {
                if bytes.is_empty() {
                    continue;
                }
                // A session id that fails the path-traversal check is
                // skipped, not fatal — one hostile id must not drop the
                // rest of the batch.
                let path = match self.path_for(session_id) {
                    Ok(path) => path,
                    Err(err) => {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %err,
                            "FileSpanStore: skipping invalid session id in batch",
                        );
                        continue;
                    }
                };

                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                    .map_err(|source| FileSpanStoreError::Open {
                        path: path.clone(),
                        source,
                    })?;
                file.write_all(&bytes)
                    .await
                    .map_err(|source| FileSpanStoreError::Write {
                        path: path.clone(),
                        source,
                    })?;
                file.flush()
                    .await
                    .map_err(|source| FileSpanStoreError::Write {
                        path: path.clone(),
                        source,
                    })?;
                // One durability barrier per session file for the batch.
                file.sync_data()
                    .await
                    .map_err(|source| FileSpanStoreError::Write {
                        path: path.clone(),
                        source,
                    })?;
            }
            Ok(())
        })
    }

    fn load(&self, session_id: &str) -> BoxFuture<'_, Result<Vec<SpanRecord>, SpanStoreError>> {
        let path = match self.path_for(session_id) {
            Ok(path) => path,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            // Bound the load allocation before reading the file whole: a
            // session file over the cap is corrupt or hostile (the plugin
            // is the only writer), so fail loudly instead of reading it in.
            match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.len() > MAX_SESSION_FILE_BYTES => {
                    return Err(FileSpanStoreError::TooLarge {
                        path,
                        size: meta.len(),
                        limit: MAX_SESSION_FILE_BYTES,
                    }
                    .into());
                }
                Ok(_) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(vec![]);
                }
                Err(source) => return Err(FileSpanStoreError::Read { path, source }.into()),
            }

            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(source) => {
                    return Err(FileSpanStoreError::Read { path, source }.into());
                }
            };

            let mut records = Vec::new();
            for line in bytes.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<SpanRecord>(line) {
                    Ok(record) => records.push(record),
                    Err(source) => {
                        // A trailing partial write is the only realistic
                        // path to a malformed line. Surface it as a warn and
                        // keep going so one truncated record doesn't blank
                        // the session's whole history.
                        tracing::warn!(
                            path = %path.display(),
                            error = %source,
                            "skipping malformed span record on load",
                        );
                    }
                }
            }
            Ok(records)
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, SpanStoreError>> {
        Box::pin(async move {
            let mut ids = Vec::new();

            let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
                Err(source) => {
                    return Err(FileSpanStoreError::List {
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
                    .map_err(|source| FileSpanStoreError::List {
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
                    && path.extension().is_some_and(|ext| ext == "jsonl")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    ids.push(stem.to_owned());
                }
            }
            Ok(ids)
        })
    }

    fn delete(&self, session_id: &str) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        let path = match self.path_for(session_id) {
            Ok(path) => path,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        Box::pin(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(FileSpanStoreError::Delete { path, source }.into()),
            }
        })
    }
}

/// Errors specific to the file-based span store.
#[derive(Debug, thiserror::Error)]
pub enum FileSpanStoreError {
    /// Failed to create the base directory.
    #[error("failed to create directory '{}': {source}", path.display())]
    CreateDir {
        /// The directory path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Failed to open a session file for append.
    #[error("failed to open '{}': {source}", path.display())]
    Open {
        /// The file path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Failed to write a record.
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
    /// Failed to serialize a record.
    #[error("failed to serialize span record for '{}': {source}", path.display())]
    Serialize {
        /// The target file path.
        path: PathBuf,
        /// The serialization error.
        source: serde_json::Error,
    },
    /// A session file exceeded the maximum size [`FileSpanStore::load`] reads.
    #[error(
        "session file '{}' is {size} bytes, over the {limit}-byte load cap",
        path.display()
    )]
    TooLarge {
        /// The session file path.
        path: PathBuf,
        /// Actual file size in bytes.
        size: u64,
        /// Maximum accepted size in bytes.
        limit: u64,
    },
}

impl From<FileSpanStoreError> for SpanStoreError {
    fn from(err: FileSpanStoreError) -> Self {
        SpanStoreError::Backend(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_plugin::SpanKind;
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn make(session: &str, name: &str) -> SpanRecord {
        let mut labels = BTreeMap::new();
        labels.insert("session_id".into(), session.into());
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
            labels,
            fields: Map::new(),
            message: None,
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_append_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());

        store.append("s1", &make("s1", "a")).await.unwrap();
        store.append("s1", &make("s1", "b")).await.unwrap();
        store.append("s1", &make("s1", "c")).await.unwrap();

        let loaded = store.load("s1").await.unwrap();
        assert_eq!(
            loaded.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[tokio::test]
    async fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        assert!(store.load("missing").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_returns_jsonl_stems() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        store.append("alpha", &make("alpha", "x")).await.unwrap();
        store.append("beta", &make("beta", "y")).await.unwrap();
        // Stray non-jsonl file should be ignored.
        std::fs::write(dir.path().join("note.txt"), b"x").unwrap();

        let mut ids = store.list_sessions().await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        let err = store
            .append("../escape", &make("../escape", "x"))
            .await
            .unwrap_err();
        assert!(matches!(err, SpanStoreError::InvalidSessionId { .. }));
    }

    #[tokio::test]
    async fn load_skips_malformed_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        store.append("s", &make("s", "good")).await.unwrap();

        let path = dir.path().join("s.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{not valid json}\n");
        std::fs::write(&path, &bytes).unwrap();

        let loaded = store.load("s").await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "good");
    }

    #[tokio::test]
    async fn delete_removes_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        store.append("s", &make("s", "x")).await.unwrap();
        store.delete("s").await.unwrap();
        assert!(store.load("s").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn load_rejects_oversized_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        // A sparse file one byte over the cap — `load` must reject it on
        // the metadata check rather than read it into memory.
        let path = dir.path().join("huge.jsonl");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_SESSION_FILE_BYTES + 1).unwrap();
        drop(file);

        let err = store.load("huge").await.unwrap_err();
        assert!(
            matches!(err, SpanStoreError::Backend(_)),
            "oversized file should surface as a backend error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn append_batch_persists_each_session_in_append_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());

        // Interleave two sessions in one batch. Each session's file must
        // preserve the batch's relative order, while each file is opened and
        // `fsync`'d once for the whole batch (the per-record cost amortized).
        let batch = vec![
            ("s1".to_string(), make("s1", "a")),
            ("s2".to_string(), make("s2", "x")),
            ("s1".to_string(), make("s1", "b")),
            ("s2".to_string(), make("s2", "y")),
            ("s1".to_string(), make("s1", "c")),
        ];
        store.append_batch(&batch).await.unwrap();

        let s1: Vec<_> = store
            .load("s1")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        let s2: Vec<_> = store
            .load("s2")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(s1, ["a", "b", "c"]);
        assert_eq!(s2, ["x", "y"]);
    }

    #[tokio::test]
    async fn append_batch_appends_onto_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());

        // A prior single append, then a batch for the same session: the
        // batch must append, not truncate.
        store.append("s", &make("s", "first")).await.unwrap();
        store
            .append_batch(&[
                ("s".to_string(), make("s", "second")),
                ("s".to_string(), make("s", "third")),
            ])
            .await
            .unwrap();

        let names: Vec<_> = store
            .load("s")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn append_batch_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());
        store.append_batch(&[]).await.unwrap();
        assert!(store.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_batch_skips_invalid_session_id_without_dropping_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSpanStore::new(dir.path());

        // A hostile id containing a path separator must be skipped with a
        // warning — not abort the batch — so the valid session still
        // persists. (An unserializable record is skipped the same way, but
        // that branch is unreachable for a real `SpanRecord`: every field
        // serializes and `serde_json::Value` cannot hold a NaN/Infinity.)
        let batch = vec![
            ("../escape".to_string(), make("../escape", "evil")),
            ("ok".to_string(), make("ok", "good")),
        ];
        store.append_batch(&batch).await.unwrap();

        let ok: Vec<_> = store
            .load("ok")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            ok,
            ["good"],
            "valid session must survive a hostile sibling in the batch"
        );
        // The escaped id wrote nothing inside (or outside) base_dir.
        assert_eq!(store.list_sessions().await.unwrap(), vec!["ok".to_string()]);
    }
}
