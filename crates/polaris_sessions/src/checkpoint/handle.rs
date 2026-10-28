//! The checkpoint surface of a [`SessionsAPI`], and the pin that holds one
//! point against eviction.

use super::store::{CheckpointSet, resource_refs};
use super::{
    CheckpointId, CheckpointMeta, CheckpointOrigin, CheckpointRetention, CheckpointStatus,
    CheckpointUsage, ResourceRef, RestoreFailureCause, RestoreOutcome, RevisionId,
};
use crate::api::SessionsAPI;
use crate::error::SessionError;
use crate::store::{ResourceEntry, SessionId};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// The checkpoint surface of a [`SessionsAPI`].
///
/// Obtained from [`SessionsAPI::checkpoints`]. Cloning is one `Arc` clone —
/// the same cost as cloning the `SessionsAPI` itself — so a handle is cheap to
/// pass around and does not need to be cached.
///
/// This is where every checkpoint operation beyond the legacy
/// [`checkpoint`](SessionsAPI::checkpoint) /
/// [`list_checkpoints`](SessionsAPI::list_checkpoints) /
/// [`rollback`](SessionsAPI::rollback) trio lives, so that adding a capability
/// to checkpoints does not widen `SessionsAPI` itself.
///
/// # Examples
///
/// ```no_run
/// # use polaris_sessions::{SessionsAPI, SessionId};
/// # use polaris_sessions::checkpoint::CheckpointOrigin;
/// # async fn example(sessions: &SessionsAPI, id: &SessionId) -> Result<(), Box<dyn std::error::Error>> {
/// let checkpoints = sessions.checkpoints();
/// let point = checkpoints.capture(id, CheckpointOrigin::Manual).await?;
///
/// // Every retained point, with metadata rather than a bare turn label.
/// let all = checkpoints.list(id)?;
/// assert!(all.iter().any(|meta| meta.id == point.id));
///
/// // What this session is spending against the configured bounds.
/// let usage = checkpoints.usage(id)?;
/// assert!(usage.count > 0);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SessionCheckpoints {
    sessions: SessionsAPI,
}

impl std::fmt::Debug for SessionCheckpoints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCheckpoints").finish_non_exhaustive()
    }
}

impl SessionCheckpoints {
    /// Creates a handle. Called by [`SessionsAPI::checkpoints`].
    pub(crate) fn new(sessions: SessionsAPI) -> Self {
        Self { sessions }
    }

    // ── Boundaries ────────────────────────────────────────────────────────

    /// Captures the session's current resource state as a new checkpoint.
    ///
    /// The captured point's [`resume_turn`](CheckpointMeta::resume_turn) is
    /// the turn the session would execute next, so restoring to it never
    /// re-runs a turn number that has already been used.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, checkpoint::CheckpointOrigin};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) -> Result<(), Box<dyn std::error::Error>> {
    /// let point = sessions.checkpoints().capture(id, CheckpointOrigin::Manual).await?;
    /// // Save the stable identity for a later rewind.
    /// let checkpoint_id = point.id;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`SessionError::SessionNotFound`] if the session does not exist;
    /// - [`SessionError::ReadOnly`] if the session was preserved by
    ///   [`run_oneshot_preserved`](SessionsAPI::run_oneshot_preserved). Such a
    ///   session can never be restored, so spending a bounded budget on it is
    ///   refused rather than silently accepted. The legacy
    ///   [`checkpoint`](SessionsAPI::checkpoint) keeps its original permissive
    ///   behaviour;
    /// - [`SessionError::CheckpointCapacity`] if the retained set is at a
    ///   bound and nothing is evictable;
    /// - [`SessionError::RestoreFailed`] if a failed restore left the context
    ///   incoherent; restore a retained point before capturing again;
    /// - [`SessionError::Persistence`] if a resource fails to serialize.
    pub async fn capture(
        &self,
        id: &SessionId,
        origin: CheckpointOrigin,
    ) -> Result<CheckpointMeta, SessionError> {
        let state = self.sessions.session_state(id)?;
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
        let ctx = state.ctx.lock().await;
        self.sessions.capture_locked(&state, id, &ctx, origin, None)
    }

    /// Every retained checkpoint for the session, oldest first, with full
    /// metadata.
    ///
    /// The turn-label view stays available as
    /// [`SessionsAPI::list_checkpoints`].
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn list(&self, id: &SessionId) -> Result<Vec<CheckpointMeta>, SessionError> {
        self.with_set(id, CheckpointSet::list)
    }

    /// The checkpoint the session is currently positioned at, if any.
    ///
    /// `None` for a session that has never captured one — for instance a
    /// session created while auto-checkpoint was disabled.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn current(&self, id: &SessionId) -> Result<Option<CheckpointMeta>, SessionError> {
        self.with_set(id, CheckpointSet::current)
    }

    /// The session's current history revision.
    ///
    /// Increments on every accepted [`restore`](Self::restore), so a consumer
    /// can tell two rewinds apart without comparing turn numbers.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn revision(&self, id: &SessionId) -> Result<RevisionId, SessionError> {
        self.with_set(id, CheckpointSet::revision)
    }

    /// Restores the session to `target`, on a new revision.
    ///
    /// Captures a [`CheckpointOrigin::PreRestore`] checkpoint of the state
    /// being left **before** touching the context; if that capture fails,
    /// the context is untouched. When recovering from a failed restore, reuses
    /// its coherent original instead of capturing the mixed context. Unlike
    /// [`rollback`](SessionsAPI::rollback) this does not discard by turn
    /// label: retained points on the abandoned chain stay listed and report
    /// [`CheckpointStatus::OffHistory`], and growth is bounded by
    /// [`CheckpointRetention`] instead.
    ///
    /// Restores what matches and **reports** the rest: entries in the
    /// checkpoint with no registered serializer arrive in
    /// [`RestoreOutcome::unmatched`], and resources that were live but absent
    /// from the checkpoint arrive in [`RestoreOutcome::carried_over`]. Use
    /// [`restore_exact`](Self::restore_exact) to refuse a partial restore
    /// instead.
    ///
    /// Like `rollback`, this does **not** re-run
    /// [`Agent::setup`](polaris_agent::Agent::setup) — doing so would replace
    /// live policy with defaults. Call
    /// [`setup_session`](SessionsAPI::setup_session) afterwards if the
    /// reported `carried_over` resources need rebuilding.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, checkpoint::CheckpointId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId, target: CheckpointId) -> Result<(), Box<dyn std::error::Error>> {
    /// let checkpoints = sessions.checkpoints();
    /// let outcome = checkpoints.restore(id, target).await?;
    /// // Keep the state we left available across subsequent captures.
    /// let original_pin = checkpoints.pin(id, outcome.original)?;
    /// // Inspect outcome.unmatched and outcome.carried_over before continuing.
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`SessionError::SessionNotFound`] if the session does not exist;
    /// - [`SessionError::ReadOnly`] if the session is read-only;
    /// - [`SessionError::CheckpointNotFound`] if `target` is not retained —
    ///   [`status`](Self::status) distinguishes evicted from never-existed;
    /// - [`SessionError::CheckpointCapacity`] if the pre-restore capture does
    ///   not fit, in which case the context is untouched;
    /// - [`SessionError::RestoreFailed`] if a resource fails to load partway
    ///   through, which leaves the context in an unknown state and makes the
    ///   session refuse further turns until a restore succeeds.
    pub async fn restore(
        &self,
        id: &SessionId,
        target: CheckpointId,
    ) -> Result<RestoreOutcome, SessionError> {
        self.restore_inner(id, target, false).await
    }

    /// Restores the session to `target`, refusing a partial restore.
    ///
    /// Checks every serialized entry in `target` against the registered
    /// serializers **before** writing anything. If any entry has no
    /// serializer, returns [`SessionError::RestoreIncomplete`] with the
    /// context byte-identical to before the call — no pre-restore checkpoint
    /// is taken and no resource is loaded.
    ///
    /// Reach for this when a partial restore is not an acceptable outcome, for
    /// instance when a resource's `schema_version` may have moved since the
    /// checkpoint was written.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, SessionError, checkpoint::CheckpointId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId, target: CheckpointId) -> Result<(), SessionError> {
    /// match sessions.checkpoints().restore_exact(id, target).await {
    ///     Ok(outcome) => assert!(outcome.unmatched.is_empty()),
    ///     Err(SessionError::RestoreIncomplete { unmatched, .. }) => {
    ///         // Register compatible serializers before retrying.
    ///         assert!(!unmatched.is_empty());
    ///     }
    ///     Err(error) => return Err(error),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`restore`](Self::restore), plus
    /// [`SessionError::RestoreIncomplete`] when the checkpoint contains an
    /// entry no registered serializer matches.
    pub async fn restore_exact(
        &self,
        id: &SessionId,
        target: CheckpointId,
    ) -> Result<RestoreOutcome, SessionError> {
        self.restore_inner(id, target, true).await
    }

    // ── Retention ─────────────────────────────────────────────────────────

    /// The configured retention policy.
    ///
    /// Read this rather than restating a bound: the policy is whatever the
    /// host chose with
    /// [`SessionsPlugin::with_checkpoint_retention`](crate::SessionsPlugin::with_checkpoint_retention),
    /// and a consumer that hardcodes a number will disagree with the framework
    /// the moment it changes.
    #[must_use]
    pub fn retention(&self) -> CheckpointRetention {
        self.sessions.checkpoint_retention()
    }

    /// What this session is currently using against the policy.
    ///
    /// `count` and `bytes` are measured on the retained set, never estimated,
    /// so they can never under-report a budget the caller is about to spend.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn usage(&self, id: &SessionId) -> Result<CheckpointUsage, SessionError> {
        let retention = self.retention();
        self.with_set(id, |set| set.usage(retention))
    }

    /// Whether one checkpoint is still available, and if not, why not.
    ///
    /// The difference between [`CheckpointStatus::Evicted`] and
    /// [`CheckpointStatus::Unknown`] is the whole point: a host that offers
    /// "rewind to here" can disable the option *and say why* instead of
    /// guessing.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn status(
        &self,
        id: &SessionId,
        checkpoint: CheckpointId,
    ) -> Result<CheckpointStatus, SessionError> {
        self.with_set(id, |set| set.status(checkpoint))
    }

    /// Holds `checkpoint` against eviction until the returned pin drops.
    ///
    /// Reach for this when holding a point across a user decision —
    /// preview-then-confirm — where a bound that reclaimed the point between
    /// the offer and the confirmation would be a race rather than a budget.
    ///
    /// The session's current checkpoint and both endpoints of a restore that
    /// has not completed are held automatically. A pre-restore capture may
    /// replace the previous current point unless it has an explicit pin.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist, or [`SessionError::CheckpointNotFound`] if the checkpoint is not
    /// retained.
    pub fn pin(
        &self,
        id: &SessionId,
        checkpoint: CheckpointId,
    ) -> Result<CheckpointPin, SessionError> {
        let state = self.sessions.session_state(id)?;
        let count = state.checkpoints.lock().pin(checkpoint).ok_or_else(|| {
            SessionError::CheckpointNotFound {
                session: id.clone(),
                checkpoint,
            }
        })?;
        Ok(CheckpointPin {
            count,
            session: id.clone(),
            checkpoint,
        })
    }

    // ── Internals ─────────────────────────────────────────────────────────

    /// Runs `read` against the session's checkpoint set.
    fn with_set<R>(
        &self,
        id: &SessionId,
        read: impl FnOnce(&CheckpointSet) -> R,
    ) -> Result<R, SessionError> {
        let state = self.sessions.session_state(id)?;
        let set = state.checkpoints.lock();
        Ok(read(&set))
    }

    async fn restore_inner(
        &self,
        id: &SessionId,
        target: CheckpointId,
        exact: bool,
    ) -> Result<RestoreOutcome, SessionError> {
        let state = self.sessions.session_state(id)?;
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
        let mut ctx = state.ctx.lock().await;

        // Resolve the target before anything is written, and copy its payload
        // out so the set lock is not held across the resource loads.
        let (target_meta, payload) = {
            let set = state.checkpoints.lock();
            let Some(meta) = set.meta(target) else {
                return Err(SessionError::CheckpointNotFound {
                    session: id.clone(),
                    checkpoint: target,
                });
            };
            let payload = set.payload(target).unwrap_or_default();
            (meta, payload)
        };

        // Protect the target before admitting the pre-restore snapshot. The
        // guard releases on serialization, capacity and exact-match refusals.
        let _target_pin = self.pin(id, target)?;
        let serializers = self.sessions.serializers();

        // `restore_exact` refuses before the context is touched at all, so a
        // refused restore cannot half-apply.
        if exact {
            let unmatched = crate::api::unmatched_refs(&serializers, &payload);
            if !unmatched.is_empty() {
                return Err(SessionError::RestoreIncomplete {
                    session: id.clone(),
                    target,
                    unmatched,
                });
            }
        }

        // A capture failure leaves the live context untouched.
        // A failed restore left a mixed context. Reuse its coherent recovery
        // point instead of taking a misleading snapshot or spending a slot.
        let recovery = {
            let set = state.checkpoints.lock();
            set.recovery_original()
                .and_then(|original| set.meta(original))
        };
        let original = match recovery {
            Some(original) => original,
            None => self.sessions.capture_locked(
                &state,
                id,
                &ctx,
                CheckpointOrigin::PreRestore,
                None,
            )?,
        };

        let carried_over = {
            let mut set = state.checkpoints.lock();
            set.begin_restore(original.id, target);
            carried_over(&set, original.id, &payload)
        };

        match crate::api::deserialize_entries_into_context(&serializers, &payload, &mut ctx) {
            Ok(report) => {
                let mut set = state.checkpoints.lock();
                set.accept_restore(target);
                state
                    .turn_number
                    .store(target_meta.resume_turn, Ordering::Release);
                Ok(RestoreOutcome {
                    target,
                    original: original.id,
                    revision: set.revision(),
                    resume_turn: target_meta.resume_turn,
                    restored: report.restored,
                    unmatched: report.unmatched,
                    carried_over,
                })
            }
            Err(err) => {
                state
                    .checkpoints
                    .lock()
                    .mark_incoherent(target, original.id, err.to_string());
                Err(SessionError::RestoreFailed {
                    session: id.clone(),
                    target,
                    original: original.id,
                    source: RestoreFailureCause::ResourceLoad(Box::new(err)),
                })
            }
        }
    }
}

/// Resources that were live before the restore and have no entry in the
/// target checkpoint, matched on `(plugin_id, storage_key)` so a schema bump
/// does not register as a disappearance.
fn carried_over(
    set: &CheckpointSet,
    original: CheckpointId,
    payload: &[ResourceEntry],
) -> Vec<ResourceRef> {
    let target_refs = resource_refs(payload);
    set.resource_refs_of(original)
        .into_iter()
        .filter(|live| {
            !target_refs.iter().any(|entry| {
                entry.plugin_id == live.plugin_id && entry.storage_key == live.storage_key
            })
        })
        .collect()
}

/// Holds one checkpoint against eviction for as long as it lives.
///
/// Obtained from [`SessionCheckpoints::pin`]. On drop the checkpoint becomes
/// evictable again, so a preview that is abandoned releases its hold without a
/// cleanup call. Drop uses an atomic decrement without waiting for checkpoint
/// encoding, eviction, or the session registry. Pinning does not prevent deleting
/// the session, and does not keep a session alive.
///
/// # Example
///
/// ```no_run
/// # use polaris_sessions::{SessionsAPI, SessionId};
/// # use polaris_sessions::checkpoint::CheckpointOrigin;
/// # async fn example(sessions: &SessionsAPI, id: &SessionId) -> Result<(), Box<dyn std::error::Error>> {
/// let checkpoints = sessions.checkpoints();
/// let point = checkpoints.capture(id, CheckpointOrigin::Manual).await?;
///
/// // Offer the point to the user; it cannot be reclaimed while pinned.
/// let pin = checkpoints.pin(id, point.id)?;
/// // ... user decides ...
/// drop(pin);
/// # Ok(())
/// # }
/// ```
pub struct CheckpointPin {
    count: Arc<AtomicUsize>,
    session: SessionId,
    checkpoint: CheckpointId,
}

impl std::fmt::Debug for CheckpointPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointPin")
            .field("session", &self.session)
            .field("checkpoint", &self.checkpoint)
            .finish_non_exhaustive()
    }
}

impl CheckpointPin {
    /// The pinned checkpoint.
    #[must_use]
    pub fn checkpoint(&self) -> CheckpointId {
        self.checkpoint
    }

    /// The session the pinned checkpoint belongs to.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

impl Drop for CheckpointPin {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionsPlugin;
    use crate::store::{AgentTypeId, memory::InMemoryStore};
    use polaris_agent::Agent;
    use polaris_core_plugins::PersistencePlugin;
    use polaris_graph::graph::Graph;
    use polaris_system::{server::Server, system};
    use std::sync::Arc;
    use std::time::Duration;

    #[system]
    async fn noop() {}

    struct EmptyAgent;
    impl Agent for EmptyAgent {
        fn build(&self, graph: &mut Graph) {
            graph.add_system(noop);
        }
        fn name(&self) -> &'static str {
            "EmptyAgent"
        }
    }

    #[tokio::test]
    async fn dropping_pin_does_not_wait_for_checkpoint_payload_lock() {
        let mut server = Server::new();
        server
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
        server.finish().await.unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.register_agent(EmptyAgent).unwrap();
        let id = SessionId::new();
        sessions
            .create_session(
                server.create_context(),
                &id,
                &AgentTypeId::from_name("EmptyAgent"),
            )
            .unwrap();
        let checkpoints = sessions.checkpoints();
        let target = checkpoints.current(&id).unwrap().unwrap();
        let pin = checkpoints.pin(&id, target.id).unwrap();
        let state = sessions.session_state(&id).unwrap();
        let held = state.checkpoints.lock();
        let (sent, received) = std::sync::mpsc::channel();
        let dropping = std::thread::spawn(move || {
            drop(pin);
            sent.send(()).unwrap();
        });
        let completed = received.recv_timeout(Duration::from_secs(1));
        drop(held);
        dropping.join().unwrap();
        assert!(
            completed.is_ok(),
            "pin drop must finish while the payload lock is held"
        );
    }
}
