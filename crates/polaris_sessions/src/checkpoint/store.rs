//! Persisted checkpoint shapes, and the live set that enforces retention.
//!
//! The persisted shapes ([`StoredCheckpoint`], [`CheckpointTombstone`],
//! [`CheckpointHistory`]) ride [`SessionData`](crate::SessionData) through the
//! configured [`SessionStore`](crate::SessionStore), which is why checkpoint
//! durability needs no new store method and no second store. `CheckpointSet`
//! is their in-memory form: it lives in the session's existing checkpoint
//! mutex and is the single place retention is enforced.

use super::{
    CheckpointId, CheckpointMeta, CheckpointOrigin, CheckpointRetention, CheckpointStatus,
    CheckpointUsage, ResourceRef, RestoreFailureCause, RevisionId,
};
use crate::store::{ResourceEntry, SessionId, TurnNumber};
use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// The first [`CheckpointId`] a session allocates.
///
/// `0` is reserved as "written by a version that did not retain checkpoint
/// identity", so a loaded `0` normalizes to this.
const FIRST_CHECKPOINT: u64 = 1;

// ─────────────────────────────────────────────────────────────────────────────
// Persisted shapes
// ─────────────────────────────────────────────────────────────────────────────

/// One retained checkpoint, as persisted.
///
/// Holds the serialized resources directly rather than a nested
/// [`SessionData`](crate::SessionData): recursion is then impossible, and the
/// stored form is *smaller* than the pre-durability in-memory snapshot, which
/// repeated the session-constant `agent_type` and `created_at` in every entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StoredCheckpoint {
    /// Everything known about the checkpoint except its payload.
    pub meta: CheckpointMeta,
    /// The serialized resources captured at this point.
    pub resources: Vec<ResourceEntry>,
}

/// A record that a checkpoint existed and was removed.
///
/// Two fields by design: a removed point must stay distinguishable from one
/// that never existed, and that costs an id and a timestamp — not a payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckpointTombstone {
    /// The removed checkpoint.
    pub id: CheckpointId,
    /// ISO 8601 UTC removal time.
    pub evicted_at: String,
}

/// Checkpoint bookkeeping that outlives the retained payloads.
///
/// Persisted alongside [`StoredCheckpoint`]s so that position, removal history
/// and the compaction floor all survive a restart — without which
/// [`CheckpointStatus::Evicted`] would degrade to
/// [`CheckpointStatus::Unknown`] on resume and the current point would be
/// unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckpointHistory {
    /// The checkpoint the session is positioned at, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<CheckpointId>,
    /// Individually recorded removals, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstones: Vec<CheckpointTombstone>,
    /// Compaction floor: every id at or below it answers
    /// [`CheckpointStatus::Evicted`], so old removals need not accumulate as
    /// individual tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<CheckpointTombstone>,
}

impl CheckpointHistory {
    /// Whether this carries no bookkeeping at all, so it can be omitted from
    /// the serialized form.
    pub(crate) fn is_empty(&self) -> bool {
        self.current.is_none() && self.tombstones.is_empty() && self.floor.is_none()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Refusals
// ─────────────────────────────────────────────────────────────────────────────

/// Why a capture could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityRefusal {
    /// Serialized bytes the refused checkpoint needed, or the smallest known
    /// size when budgeted measurement stopped early.
    pub(crate) needed_bytes: usize,
    /// Retained checkpoints that could not be evicted.
    pub(crate) pinned: usize,
}

/// A restore that left the context in an unknown state.
#[derive(Debug, Clone)]
pub(crate) struct Incoherence {
    pub(crate) target: CheckpointId,
    pub(crate) original: CheckpointId,
    pub(crate) reason: String,
}

/// The two endpoints of a restore that has not completed.
#[derive(Debug, Clone, Copy)]
struct InFlightRestore {
    original: CheckpointId,
    target: CheckpointId,
}

// ─────────────────────────────────────────────────────────────────────────────
// CheckpointSet
// ─────────────────────────────────────────────────────────────────────────────

/// One session's live checkpoint set.
///
/// Held in the session's existing `parking_lot::Mutex`, so every operation
/// here is synchronous and the session gains no second lock.
#[derive(Debug, Default)]
pub(crate) struct CheckpointSet {
    /// Retained checkpoints, oldest first. Ordered by allocation, which is
    /// also id order.
    retained: Vec<StoredCheckpoint>,
    /// Next id to allocate. `0` means nothing has been allocated yet.
    next_id: u64,
    /// The revision new captures are recorded on.
    revision: RevisionId,
    /// The checkpoint the session is positioned at.
    current: Option<CheckpointId>,
    /// Individually recorded removals, newest last.
    tombstones: Vec<CheckpointTombstone>,
    /// Compaction floor for old removals.
    floor: Option<CheckpointTombstone>,
    /// Running total of retained payload bytes.
    bytes: usize,
    /// Explicit pin counts, so nested pins release independently.
    pins: HashMap<CheckpointId, Arc<AtomicUsize>>,
    /// The endpoints of a restore that has not completed.
    in_flight: Option<InFlightRestore>,
    /// Set when a restore failed partway and the context is unknown.
    incoherent: Option<Incoherence>,
}

impl CheckpointSet {
    /// Rebuilds a set from persisted session data under the default policy.
    ///
    /// Production resume goes through
    /// [`from_stored_with_retention`](Self::from_stored_with_retention) with the
    /// host's configured policy; this shorthand is only used by the tests below.
    #[cfg(test)]
    pub(crate) fn from_stored(
        retained: Vec<StoredCheckpoint>,
        next_checkpoint: u64,
        revision: RevisionId,
        history: CheckpointHistory,
    ) -> Self {
        Self::from_stored_with_retention(
            retained,
            next_checkpoint,
            revision,
            history,
            CheckpointRetention::default(),
        )
    }

    /// Rebuilds and normalizes a set under the configured retention policy.
    ///
    /// Persisted byte metadata is never trusted: every payload is measured
    /// again before the running total is built. Existing points are then
    /// pruned to the supplied bounds, using the normal off-history-first
    /// order and keeping the current point until no other candidate remains.
    pub(crate) fn from_stored_with_retention(
        mut retained: Vec<StoredCheckpoint>,
        next_checkpoint: u64,
        revision: RevisionId,
        history: CheckpointHistory,
        retention: CheckpointRetention,
    ) -> Self {
        for checkpoint in &mut retained {
            checkpoint.meta.bytes = exact_payload_bytes(&checkpoint.resources);
        }
        let bytes = retained
            .iter()
            .map(|checkpoint| checkpoint.meta.bytes)
            .fold(0usize, usize::saturating_add);
        let highest = retained
            .iter()
            .map(|cp| cp.meta.id.as_u64())
            .max()
            .unwrap_or(0);
        // A store written before checkpoints were persisted carries `0`.
        // Never hand back an id that a retained checkpoint or a tombstone
        // already used: identity must stay unique across restarts.
        let tombstone_high = history
            .tombstones
            .iter()
            .chain(history.floor.iter())
            .map(|stone| stone.id.as_u64())
            .max()
            .unwrap_or(0);
        let next_id = next_checkpoint
            .max(highest.saturating_add(1))
            .max(tombstone_high.saturating_add(1))
            .max(FIRST_CHECKPOINT);

        let mut set = Self {
            retained,
            next_id,
            revision,
            current: history.current,
            tombstones: history.tombstones,
            floor: history.floor,
            bytes,
            pins: HashMap::new(),
            in_flight: None,
            incoherent: None,
        };
        if set.current.is_some_and(|id| set.find(id).is_none()) {
            set.current = None;
        }
        set.enforce_retention_on_load(retention);
        set
    }

    /// The retained checkpoints, for persistence.
    pub(crate) fn stored(&self) -> &[StoredCheckpoint] {
        &self.retained
    }

    /// The next id to allocate, for persistence.
    pub(crate) fn next_checkpoint(&self) -> u64 {
        self.next_id.max(FIRST_CHECKPOINT)
    }

    /// The bookkeeping that outlives the payloads, for persistence.
    pub(crate) fn history(&self) -> CheckpointHistory {
        CheckpointHistory {
            current: self.current,
            tombstones: self.tombstones.clone(),
            floor: self.floor.clone(),
        }
    }

    /// The revision new captures are recorded on.
    pub(crate) fn revision(&self) -> RevisionId {
        self.revision
    }

    /// Metadata for every retained checkpoint, oldest first.
    pub(crate) fn list(&self) -> Vec<CheckpointMeta> {
        self.retained.iter().map(|cp| cp.meta.clone()).collect()
    }

    /// The display labels of every retained checkpoint, oldest first — the
    /// legacy `list_checkpoints` view.
    pub(crate) fn turn_labels(&self) -> Vec<TurnNumber> {
        self.retained.iter().map(|cp| cp.meta.turn).collect()
    }

    /// Metadata for the checkpoint the session is positioned at.
    pub(crate) fn current(&self) -> Option<CheckpointMeta> {
        let id = self.current?;
        self.find(id).map(|cp| cp.meta.clone())
    }

    /// The payload of one retained checkpoint.
    pub(crate) fn payload(&self, id: CheckpointId) -> Option<Vec<ResourceEntry>> {
        self.find(id).map(|cp| cp.resources.clone())
    }

    /// Metadata for one retained checkpoint.
    pub(crate) fn meta(&self, id: CheckpointId) -> Option<CheckpointMeta> {
        self.find(id).map(|cp| cp.meta.clone())
    }

    /// Describes the resources one retained checkpoint holds, without cloning
    /// their payloads.
    pub(crate) fn resource_refs_of(&self, id: CheckpointId) -> Vec<ResourceRef> {
        self.find(id)
            .map(|cp| resource_refs(&cp.resources))
            .unwrap_or_default()
    }

    fn find(&self, id: CheckpointId) -> Option<&StoredCheckpoint> {
        self.retained.iter().find(|cp| cp.meta.id == id)
    }

    /// Current usage against `retention`.
    ///
    /// `count` and `bytes` are measured on the retained set rather than
    /// estimated, so neither can under-report.
    pub(crate) fn usage(&self, retention: CheckpointRetention) -> CheckpointUsage {
        CheckpointUsage {
            count: self.retained.len(),
            bytes: self.bytes,
            max_count: retention.max_count,
            max_bytes: retention.max_bytes,
            pinned: self
                .retained
                .iter()
                .filter(|cp| self.is_pinned(cp.meta.id))
                .count(),
        }
    }

    /// Whether `id` is retained, removed, off-history, or unknown.
    pub(crate) fn status(&self, id: CheckpointId) -> CheckpointStatus {
        if let Some(cp) = self.find(id) {
            return if self.is_on_current_history(id) {
                CheckpointStatus::Retained(cp.meta.clone())
            } else {
                CheckpointStatus::OffHistory(cp.meta.clone())
            };
        }
        if let Some(stone) = self.tombstones.iter().find(|stone| stone.id == id) {
            return CheckpointStatus::Evicted {
                id,
                evicted_at: stone.evicted_at.clone(),
            };
        }
        if let Some(floor) = &self.floor
            && id <= floor.id
        {
            return CheckpointStatus::Evicted {
                id,
                evicted_at: floor.evicted_at.clone(),
            };
        }
        CheckpointStatus::Unknown(id)
    }

    /// Whether `id` is the current checkpoint or one of its ancestors.
    ///
    /// Walks the recorded `parent` chain. No turn number is compared anywhere,
    /// which is what makes an abandoned branch inexpressible as "on history"
    /// rather than merely unlikely to be selected.
    pub(crate) fn is_on_current_history(&self, id: CheckpointId) -> bool {
        let mut cursor = self.current;
        // The chain is at most `retained.len()` long; the bound also stops a
        // cycle that a corrupted store could otherwise turn into a hang.
        for _ in 0..=self.retained.len() {
            match cursor {
                Some(current) if current == id => return true,
                Some(current) => cursor = self.find(current).and_then(|cp| cp.meta.parent),
                None => return false,
            }
        }
        false
    }

    /// Whether `id` may not be evicted.
    fn is_pinned(&self, id: CheckpointId) -> bool {
        if self.is_explicitly_pinned(id) {
            return true;
        }
        if self.current == Some(id) {
            return true;
        }
        self.in_flight
            .is_some_and(|restore| restore.original == id || restore.target == id)
    }

    /// Whether a live explicit pin protects this point.
    fn is_explicitly_pinned(&self, id: CheckpointId) -> bool {
        self.pins
            .get(&id)
            .is_some_and(|count| count.load(Ordering::Acquire) > 0)
    }

    /// Holds `id` against eviction, returning its independently releasable count.
    pub(crate) fn pin(&mut self, id: CheckpointId) -> Option<Arc<AtomicUsize>> {
        self.find(id)?;
        let count = self.pins.entry(id).or_default();
        count.fetch_add(1, Ordering::AcqRel);
        Some(Arc::clone(count))
    }

    /// Admits a new checkpoint, reporting a refusal as a [`SessionError`].
    ///
    /// The one enforcement point for [`CheckpointRetention`]: every capture
    /// path — manual, automatic, and the pre-restore one — arrives here.
    pub(crate) fn admit_checkpoint(
        &mut self,
        session: &SessionId,
        resources: Vec<ResourceEntry>,
        origin: CheckpointOrigin,
        resume_turn: TurnNumber,
        label: Option<TurnNumber>,
        taken_at: String,
        retention: CheckpointRetention,
    ) -> Result<CheckpointMeta, crate::error::SessionError> {
        self.admit(resources, origin, resume_turn, label, taken_at, retention)
            .map_err(|refusal| capacity_error(session, refusal, retention))
    }

    /// Admits a new checkpoint, evicting or refusing as `retention` requires.
    ///
    /// `label` overrides the display turn label; `None` uses `resume_turn`,
    /// which is correct for every capture except the post-turn automatic one,
    /// whose label is the turn that just ran.
    fn admit(
        &mut self,
        resources: Vec<ResourceEntry>,
        origin: CheckpointOrigin,
        resume_turn: TurnNumber,
        label: Option<TurnNumber>,
        taken_at: String,
        retention: CheckpointRetention,
    ) -> Result<CheckpointMeta, CapacityRefusal> {
        let bytes = match payload_bytes(&resources, retention.max_bytes) {
            PayloadBytes::Exact(bytes) => bytes,
            PayloadBytes::ExceedsLimit { minimum, .. } => {
                return Err(CapacityRefusal {
                    needed_bytes: minimum,
                    pinned: self.pinned_count(),
                });
            }
        };
        self.make_room(
            bytes,
            retention,
            &taken_at,
            origin == CheckpointOrigin::PreRestore,
        )?;

        let id = CheckpointId::from_raw(self.next_checkpoint());
        self.next_id = id.as_u64().saturating_add(1);

        let meta = CheckpointMeta {
            id,
            resume_turn,
            turn: label.unwrap_or(resume_turn),
            parent: self.current,
            revision: self.revision,
            origin,
            taken_at,
            bytes,
        };

        self.bytes = self.bytes.saturating_add(bytes);
        self.retained.push(StoredCheckpoint {
            meta: meta.clone(),
            resources,
        });
        self.current = Some(id);
        self.compact_tombstones(retention);
        Ok(meta)
    }

    /// Evicts until a checkpoint of `bytes` fits, or refuses.
    ///
    /// Eviction order: the oldest unpinned checkpoint that is **off** the
    /// current history path first, so an abandoned branch is reclaimed before
    /// live history; then the oldest unpinned checkpoint on it. The current
    /// checkpoint, the endpoints of an incomplete restore and anything held by
    /// a [`CheckpointPin`](super::CheckpointPin) are never evicted, except that
    /// a pre-restore snapshot may replace the previous current point when it
    /// has no explicit pin and is not an incomplete restore endpoint.
    ///
    /// Restricting eviction to off-history points alone would make the bound
    /// unreachable: in a session that never restores, every checkpoint is an
    /// ancestor of the newest one, so nothing would ever be evictable and the
    /// bound would refuse captures instead of bounding them.
    fn make_room(
        &mut self,
        bytes: usize,
        retention: CheckpointRetention,
        now: &str,
        replace_current: bool,
    ) -> Result<(), CapacityRefusal> {
        while self.exceeds(bytes, retention) {
            // A pre-restore snapshot already serialized the live context. It
            // can replace its previous current point when every other point
            // is protected, while explicit pins still take precedence.
            let candidate = self.eviction_candidate().or_else(|| {
                replace_current
                    .then(|| {
                        self.retained.iter().position(|checkpoint| {
                            let id = checkpoint.meta.id;
                            self.current == Some(id)
                                && !self.is_explicitly_pinned(id)
                                && self.in_flight.is_none()
                        })
                    })
                    .flatten()
            });
            let Some(index) = candidate else {
                return Err(CapacityRefusal {
                    needed_bytes: bytes,
                    pinned: self.pinned_count(),
                });
            };
            self.evict(index, now, replace_current);
        }
        Ok(())
    }

    /// Applies retention to a set reconstructed from persistence.
    ///
    /// No runtime pin survives a restart. If a single current checkpoint is
    /// itself larger than the byte policy, it is removed as the last resort:
    /// retaining it would make the configured bound false immediately after
    /// load. The session's separately persisted live resources remain intact.
    fn enforce_retention_on_load(&mut self, retention: CheckpointRetention) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        while self.exceeds_existing(retention) {
            let Some(index) = self.load_eviction_candidate() else {
                break;
            };
            self.evict(index, &now, true);
        }
        self.compact_tombstones(retention);
    }

    /// Whether the already-loaded set exceeds either configured bound.
    fn exceeds_existing(&self, retention: CheckpointRetention) -> bool {
        retention
            .max_count
            .is_some_and(|max| self.retained.len() > max)
            || retention.max_bytes.is_some_and(|max| self.bytes > max)
    }

    /// Whether admitting one more checkpoint of `bytes` would breach a bound.
    fn exceeds(&self, bytes: usize, retention: CheckpointRetention) -> bool {
        let over_count = retention
            .max_count
            .is_some_and(|max| self.retained.len().saturating_add(1) > max);
        let over_bytes = retention
            .max_bytes
            .is_some_and(|max| self.bytes.saturating_add(bytes) > max);
        over_count || over_bytes
    }

    /// The index of the checkpoint to evict next, if any.
    fn eviction_candidate(&self) -> Option<usize> {
        let history = self.current_history_ids();
        let mut oldest_unpinned = None;
        for (index, checkpoint) in self.retained.iter().enumerate() {
            if self.is_pinned(checkpoint.meta.id) {
                continue;
            }
            oldest_unpinned.get_or_insert(index);
            if !history.contains(&checkpoint.meta.id) {
                return Some(index);
            }
        }
        oldest_unpinned
    }

    /// The load-time candidate, when there are no runtime pins to preserve.
    fn load_eviction_candidate(&self) -> Option<usize> {
        let history = self.current_history_ids();
        self.retained
            .iter()
            .position(|checkpoint| !history.contains(&checkpoint.meta.id))
            .or_else(|| {
                self.retained
                    .iter()
                    .position(|checkpoint| Some(checkpoint.meta.id) != self.current)
            })
            .or_else(|| (!self.retained.is_empty()).then_some(0))
    }

    /// Collects the current ancestry in linear expected time.
    fn current_history_ids(&self) -> HashSet<CheckpointId> {
        let parents: HashMap<CheckpointId, Option<CheckpointId>> = self
            .retained
            .iter()
            .map(|checkpoint| (checkpoint.meta.id, checkpoint.meta.parent))
            .collect();
        let mut history = HashSet::with_capacity(self.retained.len());
        let mut cursor = self.current;
        for _ in 0..=self.retained.len() {
            let Some(id) = cursor else {
                break;
            };
            if !history.insert(id) {
                break;
            }
            cursor = parents.get(&id).copied().flatten();
        }
        history
    }

    /// Removes one checkpoint and records its durable tombstone.
    fn evict(&mut self, index: usize, now: &str, allow_current: bool) {
        let evicted = self.retained.remove(index);
        if allow_current && self.current == Some(evicted.meta.id) {
            self.current = evicted.meta.parent;
        }
        self.bytes = self.bytes.saturating_sub(evicted.meta.bytes);
        self.pins.remove(&evicted.meta.id);
        self.reparent_children(evicted.meta.id, evicted.meta.parent);
        self.tombstones.push(CheckpointTombstone {
            id: evicted.meta.id,
            evicted_at: now.to_owned(),
        });
    }

    /// Number of retained checkpoints protected from ordinary eviction.
    fn pinned_count(&self) -> usize {
        self.retained
            .iter()
            .filter(|checkpoint| self.is_pinned(checkpoint.meta.id))
            .count()
    }

    /// Re-links the children of a removed checkpoint onto its own parent, so
    /// the retained set stays a tree.
    ///
    /// Without this, removing a point in the middle of a chain would cut every
    /// older point off from the current one, and they would all report
    /// [`CheckpointStatus::OffHistory`] — claiming a branch switch where there
    /// was only an eviction. `parent` therefore means "nearest *retained*
    /// ancestor", which is the only parentage a caller can act on.
    fn reparent_children(&mut self, removed: CheckpointId, grandparent: Option<CheckpointId>) {
        for checkpoint in &mut self.retained {
            if checkpoint.meta.parent == Some(removed) {
                checkpoint.meta.parent = grandparent;
            }
        }
    }

    /// Collapses removals that no query can distinguish into the floor.
    ///
    /// Two rules, both bounded: a tombstone older than every retained
    /// checkpoint can only ever answer `Evicted`, so it folds into the floor;
    /// and the list itself is capped by the count bound, so removals cannot
    /// accumulate without limit even in a session that never retains much.
    fn compact_tombstones(&mut self, retention: CheckpointRetention) {
        if let Some(oldest) = self.retained.first().map(|cp| cp.meta.id) {
            let below: Vec<CheckpointTombstone> = self
                .tombstones
                .iter()
                .filter(|stone| stone.id < oldest)
                .cloned()
                .collect();
            if !below.is_empty() {
                self.tombstones.retain(|stone| stone.id >= oldest);
                self.raise_floor(below);
            }
        }

        if let Some(max) = retention.max_count
            && self.tombstones.len() > max
        {
            let excess = self.tombstones.len() - max;
            let dropped: Vec<CheckpointTombstone> = self.tombstones.drain(..excess).collect();
            self.raise_floor(dropped);
        }
    }

    /// Raises the floor to the highest id among `folded`.
    fn raise_floor(&mut self, folded: Vec<CheckpointTombstone>) {
        let highest = folded.into_iter().max_by_key(|stone| stone.id);
        let Some(highest) = highest else {
            return;
        };
        let raise = match &self.floor {
            Some(floor) => highest.id > floor.id,
            None => true,
        };
        if raise {
            self.floor = Some(highest);
        }
    }

    /// Moves the session's position to `id` on a fresh revision, as an
    /// accepted restore does.
    pub(crate) fn accept_restore(&mut self, id: CheckpointId) {
        self.current = Some(id);
        self.revision = self.revision.next();
        self.in_flight = None;
        self.incoherent = None;
    }

    /// Pins both endpoints of a restore for as long as it is incomplete.
    pub(crate) fn begin_restore(&mut self, original: CheckpointId, target: CheckpointId) {
        self.in_flight = Some(InFlightRestore { original, target });
    }

    /// Marks the context unknown after a restore failed partway through.
    pub(crate) fn mark_incoherent(
        &mut self,
        target: CheckpointId,
        original: CheckpointId,
        reason: String,
    ) {
        self.incoherent = Some(Incoherence {
            target,
            original,
            reason,
        });
    }

    /// The coherent recovery point held after a failed restore.
    pub(crate) fn recovery_original(&self) -> Option<CheckpointId> {
        self.incoherent.as_ref().map(|state| state.original)
    }

    /// The error a turn must be refused with while the context is unknown.
    ///
    /// A restore that failed partway leaves resources half-applied. Running
    /// another turn on that context would produce output nobody can reason
    /// about, so the session refuses until a restore succeeds or the caller
    /// resumes from the store. The framework holds this so every consumer does
    /// not have to.
    pub(crate) fn coherence_refusal(
        &self,
        session: &SessionId,
    ) -> Option<crate::error::SessionError> {
        self.incoherent
            .as_ref()
            .map(|state| crate::error::SessionError::RestoreFailed {
                session: session.clone(),
                target: state.target,
                original: state.original,
                source: RestoreFailureCause::Incoherent {
                    reason: state.reason.clone(),
                },
            })
    }

    // ── Legacy `checkpoint` / `list_checkpoints` / `rollback` support ──────

    /// The first retained checkpoint carrying the display label `turn`.
    ///
    /// First match, not last: the legacy `rollback` resolves that way and its
    /// behaviour is deliberately unchanged.
    pub(crate) fn find_by_label(&self, turn: TurnNumber) -> Option<CheckpointId> {
        self.retained
            .iter()
            .find(|cp| cp.meta.turn == turn)
            .map(|cp| cp.meta.id)
    }

    /// Discards every retained checkpoint whose display label is above
    /// `turn`, as the legacy `rollback` does, and positions the session at
    /// `target`.
    ///
    /// Discarded points are tombstoned so the new surface reports them as
    /// removed rather than as never having existed. The revision is left
    /// alone: a legacy rollback discards rather than branching, so there is no
    /// abandoned branch to disambiguate.
    pub(crate) fn discard_above_label(
        &mut self,
        turn: TurnNumber,
        target: CheckpointId,
        now: &str,
        retention: CheckpointRetention,
    ) {
        let mut discarded = Vec::new();
        let mut relink = Vec::new();
        self.retained.retain(|cp| {
            if cp.meta.turn <= turn {
                return true;
            }
            relink.push((cp.meta.id, cp.meta.parent));
            discarded.push(CheckpointTombstone {
                id: cp.meta.id,
                evicted_at: now.to_owned(),
            });
            false
        });
        for (removed, grandparent) in relink {
            self.reparent_children(removed, grandparent);
        }
        for stone in &discarded {
            self.pins.remove(&stone.id);
        }
        self.tombstones.extend(discarded);
        self.bytes = self.retained.iter().map(|cp| cp.meta.bytes).sum();
        self.current = Some(target);
        self.compact_tombstones(retention);
    }
}

/// Result of measuring one checkpoint payload without materializing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadBytes {
    /// The exact encoded byte count.
    Exact(usize),
    /// Encoding stopped as soon as the configured byte limit was crossed.
    ExceedsLimit {
        /// Smallest byte count known from the early refusal.
        minimum: usize,
        /// Bytes accepted by the sink before it refused the next write.
        written: usize,
    },
}

/// A counting JSON sink with an optional hard limit.
///
/// It never retains the bytes. With a limit, it refuses the first write that
/// would cross it, so an oversized payload is not fully encoded just to learn
/// that it cannot fit.
struct CountingWriter {
    written: usize,
    limit: Option<usize>,
    exceeded: bool,
}

impl CountingWriter {
    fn new(limit: Option<usize>) -> Self {
        Self {
            written: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self.written.checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("checkpoint byte count overflowed"));
        };
        if self.limit.is_some_and(|limit| next > limit) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "checkpoint payload exceeds retention limit",
            ));
        }
        self.written = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Measures the serialized size on the same compact JSON encoding used for
/// checkpoint payloads inside [`SessionData`](crate::SessionData).
fn payload_bytes(resources: &[ResourceEntry], limit: Option<usize>) -> PayloadBytes {
    let mut counter = CountingWriter::new(limit);
    match serde_json::to_writer(&mut counter, resources) {
        Ok(()) => PayloadBytes::Exact(counter.written),
        Err(_) if counter.exceeded => PayloadBytes::ExceedsLimit {
            minimum: limit.map_or(usize::MAX, |max| max.saturating_add(1)),
            written: counter.written,
        },
        // `ResourceEntry` contains only strings and `serde_json::Value`, so
        // the writer is the only practical failure source. Conservatively
        // over-report any future serializer failure rather than weakening a
        // bound.
        Err(_) => PayloadBytes::Exact(usize::MAX),
    }
}

/// Exact payload size for trusted, already-deserialized values.
fn exact_payload_bytes(resources: &[ResourceEntry]) -> usize {
    match payload_bytes(resources, None) {
        PayloadBytes::Exact(bytes) => bytes,
        PayloadBytes::ExceedsLimit { .. } => usize::MAX,
    }
}

/// Describes the resources in a checkpoint payload.
pub(crate) fn resource_refs(resources: &[ResourceEntry]) -> Vec<ResourceRef> {
    resources
        .iter()
        .map(|entry| ResourceRef {
            plugin_id: entry.plugin_id.clone(),
            storage_key: entry.storage_key.clone(),
            version: entry.version.clone(),
        })
        .collect()
}

/// Formats a capacity refusal for the session error.
fn capacity_error(
    session: &SessionId,
    refusal: CapacityRefusal,
    retention: CheckpointRetention,
) -> crate::error::SessionError {
    crate::error::SessionError::CheckpointCapacity {
        session: session.clone(),
        needed_bytes: refusal.needed_bytes,
        retention,
        pinned: refusal.pinned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(size: usize) -> ResourceEntry {
        ResourceEntry {
            plugin_id: "test".to_owned(),
            storage_key: "Big".to_owned(),
            version: "1.0.0".to_owned(),
            data: serde_json::json!("x".repeat(size)),
        }
    }

    fn admit(set: &mut CheckpointSet, turn: TurnNumber, retention: CheckpointRetention) {
        set.admit(
            vec![entry(4)],
            CheckpointOrigin::Manual,
            turn,
            None,
            "2026-01-01T00:00:00Z".to_owned(),
            retention,
        )
        .expect("admit should fit");
    }

    #[test]
    fn linear_history_still_evicts_under_the_count_bound() {
        // Every checkpoint in a session that never restores is an ancestor of
        // the newest one. Eviction must still find a candidate, or the bound
        // would refuse captures instead of bounding them.
        let retention = CheckpointRetention::new(Some(2), None);
        let mut set = CheckpointSet::default();
        for turn in 0..5 {
            admit(&mut set, turn, retention);
        }
        assert_eq!(set.usage(retention).count, 2);
        assert_eq!(set.turn_labels(), vec![3, 4]);
    }

    #[test]
    fn identity_never_reuses_an_id_after_a_reload() {
        let retention = CheckpointRetention::default();
        let mut set = CheckpointSet::default();
        admit(&mut set, 0, retention);
        admit(&mut set, 1, retention);
        let history = set.history();
        let reloaded = CheckpointSet::from_stored(
            Vec::new(),
            0,
            set.revision(),
            CheckpointHistory {
                current: history.current,
                tombstones: vec![CheckpointTombstone {
                    id: CheckpointId::from_raw(7),
                    evicted_at: "2026-01-01T00:00:00Z".to_owned(),
                }],
                floor: None,
            },
        );
        // `next_checkpoint = 0` means "written before identity was retained",
        // but a tombstone proves id 7 was used, so allocation resumes above it.
        assert_eq!(reloaded.next_checkpoint(), 8);
    }

    #[test]
    fn a_payload_larger_than_the_byte_bound_is_refused_not_dropped() {
        let retention = CheckpointRetention::new(None, Some(16));
        let mut set = CheckpointSet::default();
        let refusal = set
            .admit(
                vec![entry(4096)],
                CheckpointOrigin::Manual,
                0,
                None,
                "2026-01-01T00:00:00Z".to_owned(),
                retention,
            )
            .expect_err("an oversized payload must be refused");
        assert_eq!(refusal.needed_bytes, 17);
        assert_eq!(set.usage(retention).count, 0);
    }

    #[test]
    fn a_zero_count_policy_explicitly_retains_no_checkpoints() {
        let retention = CheckpointRetention::new(Some(0), None);
        let mut set = CheckpointSet::default();
        let refusal = set
            .admit(
                vec![entry(1)],
                CheckpointOrigin::Manual,
                0,
                None,
                "2026-01-01T00:00:00Z".to_owned(),
                retention,
            )
            .expect_err("a zero count policy must refuse every capture");
        assert_eq!(refusal.pinned, 0);
        assert_eq!(set.usage(retention).count, 0);
    }

    #[test]
    fn payload_measurement_matches_the_persisted_json_encoding() {
        let resources = vec![entry(37), entry(113)];
        assert_eq!(
            payload_bytes(&resources, None),
            PayloadBytes::Exact(serde_json::to_vec(&resources).unwrap().len())
        );
    }

    #[test]
    fn payload_measurement_aborts_at_the_byte_budget() {
        let resources = vec![entry(1024 * 1024)];
        let measured = payload_bytes(&resources, Some(64));
        let PayloadBytes::ExceedsLimit { minimum, written } = measured else {
            panic!("an over-budget payload must abort measurement, got {measured:?}");
        };
        assert_eq!(minimum, 65);
        assert!(
            written <= 64,
            "the sink accepted {written} bytes despite a 64-byte budget"
        );
    }

    #[test]
    fn loading_recomputes_forged_payload_sizes() {
        let resources = vec![entry(256)];
        let actual = serde_json::to_vec(&resources).unwrap().len();
        let stored = StoredCheckpoint {
            meta: CheckpointMeta {
                id: CheckpointId::from_raw(1),
                resume_turn: 0,
                turn: 0,
                parent: None,
                revision: RevisionId::default(),
                origin: CheckpointOrigin::Manual,
                taken_at: "2026-01-01T00:00:00Z".to_owned(),
                bytes: 1,
            },
            resources,
        };

        let set = CheckpointSet::from_stored(
            vec![stored],
            2,
            RevisionId::default(),
            CheckpointHistory {
                current: Some(CheckpointId::from_raw(1)),
                ..CheckpointHistory::default()
            },
        );

        assert_eq!(set.list()[0].bytes, actual);
        assert_eq!(set.usage(CheckpointRetention::default()).bytes, actual);
    }

    #[test]
    fn loading_enforces_the_default_retention_count() {
        let retained = (1..=65)
            .map(|raw| StoredCheckpoint {
                meta: CheckpointMeta {
                    id: CheckpointId::from_raw(raw),
                    resume_turn: raw as TurnNumber,
                    turn: raw as TurnNumber,
                    parent: (raw > 1).then(|| CheckpointId::from_raw(raw - 1)),
                    revision: RevisionId::default(),
                    origin: CheckpointOrigin::Manual,
                    taken_at: "2026-01-01T00:00:00Z".to_owned(),
                    bytes: 1,
                },
                resources: vec![entry(1)],
            })
            .collect();
        let first = CheckpointId::from_raw(1);
        let set = CheckpointSet::from_stored(
            retained,
            66,
            RevisionId::default(),
            CheckpointHistory {
                current: Some(CheckpointId::from_raw(65)),
                ..CheckpointHistory::default()
            },
        );

        assert_eq!(set.list().len(), 64);
        assert!(matches!(
            set.status(first),
            CheckpointStatus::Evicted { .. }
        ));
    }

    #[test]
    fn loading_enforces_a_configured_retention_count() {
        let retained = (1..=3)
            .map(|raw| StoredCheckpoint {
                meta: CheckpointMeta {
                    id: CheckpointId::from_raw(raw),
                    resume_turn: raw as TurnNumber,
                    turn: raw as TurnNumber,
                    parent: (raw > 1).then(|| CheckpointId::from_raw(raw - 1)),
                    revision: RevisionId::default(),
                    origin: CheckpointOrigin::Manual,
                    taken_at: "2026-01-01T00:00:00Z".to_owned(),
                    bytes: 0,
                },
                resources: vec![entry(1)],
            })
            .collect();
        let set = CheckpointSet::from_stored_with_retention(
            retained,
            4,
            RevisionId::default(),
            CheckpointHistory {
                current: Some(CheckpointId::from_raw(3)),
                ..CheckpointHistory::default()
            },
            CheckpointRetention::new(Some(2), None),
        );

        assert_eq!(set.list().len(), 2);
        assert!(matches!(
            set.status(CheckpointId::from_raw(1)),
            CheckpointStatus::Evicted { .. }
        ));
    }

    #[test]
    fn loading_enforces_a_configured_retention_byte_budget() {
        let resources = vec![entry(64)];
        let one_payload = serde_json::to_vec(&resources).unwrap().len();
        let retained = (1..=2)
            .map(|raw| StoredCheckpoint {
                meta: CheckpointMeta {
                    id: CheckpointId::from_raw(raw),
                    resume_turn: raw as TurnNumber,
                    turn: raw as TurnNumber,
                    parent: (raw > 1).then(|| CheckpointId::from_raw(raw - 1)),
                    revision: RevisionId::default(),
                    origin: CheckpointOrigin::Manual,
                    taken_at: "2026-01-01T00:00:00Z".to_owned(),
                    bytes: 1,
                },
                resources: resources.clone(),
            })
            .collect();
        let set = CheckpointSet::from_stored_with_retention(
            retained,
            3,
            RevisionId::default(),
            CheckpointHistory {
                current: Some(CheckpointId::from_raw(2)),
                ..CheckpointHistory::default()
            },
            CheckpointRetention::new(None, Some(one_payload)),
        );

        assert_eq!(set.list().len(), 1);
        assert_eq!(
            set.usage(CheckpointRetention::unbounded()).bytes,
            one_payload
        );
        assert!(matches!(
            set.status(CheckpointId::from_raw(1)),
            CheckpointStatus::Evicted { .. }
        ));
    }
    #[test]
    fn eviction_reparents_children_to_nearest_retained_ancestor() {
        let policy = CheckpointRetention::new(Some(3), None);
        let mut set = CheckpointSet::default();
        for turn in 0..3 {
            admit(&mut set, turn, policy);
        }
        let oldest = set.list()[0].id;
        let middle = set.list()[1].id;
        let newest = set.list()[2].id;
        let pin = set.pin(oldest).unwrap();
        admit(&mut set, 3, policy);
        assert!(matches!(
            set.status(middle),
            CheckpointStatus::Evicted { .. }
        ));
        assert_eq!(set.meta(newest).unwrap().parent, Some(oldest));
        assert!(matches!(set.status(oldest), CheckpointStatus::Retained(_)));
        pin.fetch_sub(1, Ordering::AcqRel);
    }

    #[test]
    fn corrupt_parent_cycles_terminate_history_and_eviction_walks() {
        let mut set = CheckpointSet::default();
        for turn in 0..3 {
            admit(&mut set, turn, CheckpointRetention::unbounded());
        }
        let first = set.list()[0].id;
        let second = set.list()[1].id;
        let third = set.list()[2].id;
        set.retained[0].meta.parent = Some(second);
        set.current = Some(second);
        assert!(set.is_on_current_history(first));
        assert!(!set.is_on_current_history(third));
        assert_eq!(set.current_history_ids().len(), 2);
        set.enforce_retention_on_load(CheckpointRetention::new(Some(2), None));
        assert!(matches!(
            set.status(third),
            CheckpointStatus::Evicted { .. }
        ));
        assert_eq!(set.current().unwrap().id, second);
    }

    #[test]
    fn loading_evicts_current_as_last_resort_to_meet_either_bound() {
        for policy in [
            CheckpointRetention::new(Some(0), None),
            CheckpointRetention::new(None, Some(0)),
        ] {
            let mut set = CheckpointSet::default();
            admit(&mut set, 0, CheckpointRetention::unbounded());
            let point = set.current().unwrap().id;
            let loaded = CheckpointSet::from_stored_with_retention(
                set.stored().to_vec(),
                set.next_checkpoint(),
                set.revision(),
                set.history(),
                policy,
            );
            assert!(loaded.current().is_none());
            assert!(loaded.list().is_empty());
            assert_eq!(loaded.usage(policy).bytes, 0);
            assert!(matches!(
                loaded.status(point),
                CheckpointStatus::Evicted { .. }
            ));
        }
    }

    #[test]
    fn nested_pins_release_independently_before_eviction() {
        let policy = CheckpointRetention::new(Some(2), None);
        let mut set = CheckpointSet::default();
        admit(&mut set, 0, policy);
        let oldest = set.current().unwrap().id;
        let outer = set.pin(oldest).unwrap();
        let inner = set.pin(oldest).unwrap();
        admit(&mut set, 1, policy);
        inner.fetch_sub(1, Ordering::AcqRel);
        assert!(set.is_pinned(oldest));
        assert!(
            set.admit(
                vec![entry(4)],
                CheckpointOrigin::Manual,
                2,
                None,
                String::new(),
                policy
            )
            .is_err()
        );
        outer.fetch_sub(1, Ordering::AcqRel);
        admit(&mut set, 2, policy);
        assert!(matches!(
            set.status(oldest),
            CheckpointStatus::Evicted { .. }
        ));
    }
}
