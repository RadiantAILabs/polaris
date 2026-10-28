//! Retention: the bound on one session's checkpoint set, and what it refuses.
//!
//! The bound exists because the checkpoint vector grew without limit on both
//! push paths, auto-checkpoint is on by default, and nothing ever read the
//! result — so a long-running consumer leaked one serialized snapshot per turn.
//! These tests prove the bound binds, that it never drops a point silently, and
//! that a point it did remove stays distinguishable from one that never existed.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, Storable};
use polaris_graph::graph::Graph;
use polaris_sessions::checkpoint::{
    CheckpointId, CheckpointOrigin, CheckpointRetention, CheckpointStatus,
};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId, SessionStore};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::ResMut;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, Storable)]
#[storable(key = "Counter")]
struct Counter {
    value: u32,
    /// Padding, so one checkpoint payload is big enough to test a byte bound
    /// without capturing thousands of points.
    padding: String,
}
impl LocalResource for Counter {}

/// The one-shot paths extract a typed output, so the agent emits one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CounterOutput {
    value: u32,
}

#[system]
async fn increment(mut counter: ResMut<Counter>) -> CounterOutput {
    counter.value += 1;
    CounterOutput {
        value: counter.value,
    }
}

struct CounterAgent;

impl Agent for CounterAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(increment);
    }

    fn name(&self) -> &'static str {
        "CounterAgent"
    }
}

/// A server whose checkpoint policy is `retention`. `auto` mirrors the host's
/// auto-checkpoint choice, so a test can either exercise the default-on path
/// or take every capture by hand.
async fn server_with(
    store: Arc<dyn SessionStore>,
    retention: CheckpointRetention,
    auto: bool,
) -> Server {
    let mut server = Server::new();
    let mut plugin = SessionsPlugin::new(store).with_checkpoint_retention(retention);
    if !auto {
        plugin = plugin.without_auto_checkpoint();
    }
    server.add_plugins(PersistencePlugin).add_plugins(plugin);
    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    persistence.register::<Counter>("test");
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CounterAgent).unwrap();
    server
}

fn create_session(server: &Server, id: &SessionId, padding: usize) {
    server
        .api::<SessionsAPI>()
        .unwrap()
        .create_session_with(
            server.create_context(),
            id,
            &AgentTypeId::from_name("CounterAgent"),
            |ctx| {
                ctx.insert(Counter {
                    value: 0,
                    padding: "x".repeat(padding),
                });
            },
        )
        .unwrap();
}

async fn capture(sessions: &SessionsAPI, id: &SessionId) -> CheckpointId {
    sessions
        .checkpoints()
        .capture(id, CheckpointOrigin::Manual)
        .await
        .expect("capture should fit")
        .id
}

// ─────────────────────────────────────────────────────────────────────────────
// Bounds
// ─────────────────────────────────────────────────────────────────────────────

/// The count bound binds on the default-on automatic path — the one that
/// leaked. A session that never restores has every point on its current
/// history, so a bound that refused to evict those would never bind at all.
#[tokio::test]
async fn the_count_bound_binds_on_the_default_auto_path() {
    let retention = CheckpointRetention::new(Some(4), None);
    let server = server_with(Arc::new(InMemoryStore::new()), retention, true).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    for _ in 0..10 {
        sessions
            .process_turn(&id)
            .await
            .expect("a bound must never fail a user's turn");
    }

    let usage = sessions.checkpoints().usage(&id).unwrap();
    assert_eq!(usage.count, 4, "eleven captures, four retained");
    assert_eq!(usage.max_count, Some(4));

    let points = sessions.checkpoints().list(&id).unwrap();
    assert_eq!(
        points
            .iter()
            .map(|meta| meta.resume_turn)
            .collect::<Vec<_>>(),
        vec![7, 8, 9, 10],
        "the newest points survive; the oldest are reclaimed"
    );
    assert_eq!(
        sessions.checkpoints().current(&id).unwrap().unwrap().id,
        points[3].id
    );
}

/// The byte bound binds on the measured payload, not an estimate of it.
#[tokio::test]
async fn the_byte_bound_binds_and_usage_never_under_reports() {
    let retention = CheckpointRetention::new(None, Some(4096));
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 512);

    for _ in 0..12 {
        capture(sessions, &id).await;
    }

    let usage = sessions.checkpoints().usage(&id).unwrap();
    assert!(usage.count < 12, "the byte bound reclaimed points");
    assert!(usage.count > 0, "and did not reclaim all of them");
    assert!(
        usage.bytes <= 4096,
        "{} bytes retained against a 4096-byte bound",
        usage.bytes
    );

    let measured: usize = sessions
        .checkpoints()
        .list(&id)
        .unwrap()
        .iter()
        .map(|meta| meta.bytes)
        .sum();
    assert_eq!(
        usage.bytes, measured,
        "usage is the sum of the measured payloads, so it cannot under-report"
    );
    assert!(measured > 512, "the padding really is in the payload");
}

/// Exhausting capacity with nothing evictable is refused, never absorbed by
/// dropping a point the caller might still need.
#[tokio::test]
async fn capacity_exhaustion_is_refused_rather_than_silently_dropped() {
    // One slot: the first capture becomes the session's current point, which is
    // never evictable, so the second has nowhere to go.
    let retention = CheckpointRetention::new(Some(1), None);
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    let first = capture(sessions, &id).await;
    let err = sessions
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .expect_err("an explicit capture must fail loudly");
    match err {
        SessionError::CheckpointCapacity {
            session,
            needed_bytes,
            retention: policy,
            pinned,
        } => {
            assert_eq!(session, id);
            assert!(needed_bytes > 0);
            assert_eq!(policy.max_count, Some(1));
            assert_eq!(pinned, 1, "the current point is the one that cannot go");
        }
        other => panic!("expected CheckpointCapacity, got {other:?}"),
    }

    let usage = sessions.checkpoints().usage(&id).unwrap();
    assert_eq!(usage.count, 1, "and nothing was dropped to make room");
    assert_eq!(
        sessions.checkpoints().current(&id).unwrap().unwrap().id,
        first
    );
}

/// An automatic checkpoint that cannot fit warns rather than failing the turn,
/// so a bound never costs a user their work.
#[tokio::test]
async fn an_automatic_checkpoint_that_cannot_fit_does_not_fail_the_turn() {
    let retention = CheckpointRetention::new(Some(1), None);
    let server = server_with(Arc::new(InMemoryStore::new()), retention, true).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    for _ in 0..3 {
        sessions
            .process_turn(&id)
            .await
            .expect("a refused automatic checkpoint must not fail the turn");
    }
    assert_eq!(sessions.checkpoints().usage(&id).unwrap().count, 1);
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 3);
}

// ─────────────────────────────────────────────────────────────────────────────
// Eviction order and pinning
// ─────────────────────────────────────────────────────────────────────────────

/// Abandoned branches are reclaimed before live history, and the oldest
/// candidate goes first within each group.
#[tokio::test]
async fn eviction_takes_the_oldest_off_history_point_before_live_history() {
    let retention = CheckpointRetention::new(Some(4), None);
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    let oldest = capture(sessions, &id).await;
    let abandoned = capture(sessions, &id).await;

    // Rewind to `oldest`: `abandoned` and the pre-restore point are now off the
    // current history path, while `oldest` stays on it without being current.
    let outcome = checkpoints.restore(&id, oldest).await.unwrap();
    let newest = capture(sessions, &id).await;
    assert!(matches!(
        checkpoints.status(&id, oldest).unwrap(),
        CheckpointStatus::Retained(_)
    ));
    assert!(matches!(
        checkpoints.status(&id, abandoned).unwrap(),
        CheckpointStatus::OffHistory(_)
    ));

    // One more than the bound allows: the oldest *off-history* point goes,
    // even though `oldest` itself is older.
    capture(sessions, &id).await;
    assert!(
        matches!(
            checkpoints.status(&id, abandoned).unwrap(),
            CheckpointStatus::Evicted { .. }
        ),
        "the abandoned branch is reclaimed first"
    );
    assert!(matches!(
        checkpoints.status(&id, oldest).unwrap(),
        CheckpointStatus::Retained(_)
    ));
    assert!(matches!(
        checkpoints.status(&id, newest).unwrap(),
        CheckpointStatus::Retained(_)
    ));
    assert!(
        matches!(
            checkpoints.status(&id, outcome.original).unwrap(),
            CheckpointStatus::OffHistory(_)
        ),
        "the completed restore's pre-restore point is off-history and evictable"
    );
}

/// A pin holds a point through evictions, and releases it on drop.
#[tokio::test]
async fn a_pin_blocks_eviction_and_releases_on_drop() {
    let retention = CheckpointRetention::new(Some(3), None);
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    let held = capture(sessions, &id).await;
    let pin = checkpoints.pin(&id, held).unwrap();
    assert_eq!(pin.checkpoint(), held);
    assert_eq!(pin.session(), &id);

    let next = capture(sessions, &id).await;
    for _ in 0..4 {
        capture(sessions, &id).await;
    }

    assert!(
        matches!(
            checkpoints.status(&id, held).unwrap(),
            CheckpointStatus::Retained(_)
        ),
        "a pinned point survives every eviction"
    );
    assert!(
        matches!(
            checkpoints.status(&id, next).unwrap(),
            CheckpointStatus::Evicted { .. }
        ),
        "while an unpinned younger point is reclaimed"
    );
    assert_eq!(sessions.checkpoints().usage(&id).unwrap().pinned, 2);

    drop(pin);
    assert_eq!(
        sessions.checkpoints().usage(&id).unwrap().pinned,
        1,
        "only the current point stays held automatically"
    );
    for _ in 0..3 {
        capture(sessions, &id).await;
    }
    assert!(
        matches!(
            checkpoints.status(&id, held).unwrap(),
            CheckpointStatus::Evicted { .. }
        ),
        "and the released point becomes evictable again"
    );
}

/// A read-only session can never be restored, so spending a bounded budget on
/// it is refused — while the legacy `checkpoint` keeps its permissive
/// behaviour, which is what makes it unchanged.
#[tokio::test]
async fn capture_refuses_a_read_only_session_but_the_legacy_checkpoint_does_not() {
    let server = server_with(
        Arc::new(InMemoryStore::new()),
        CheckpointRetention::default(),
        false,
    )
    .await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    // A live session, to borrow a checkpoint id from.
    let live = SessionId::new();
    create_session(&server, &live, 0);
    let borrowed = capture(sessions, &live).await;

    let (preserved, output) = sessions
        .run_oneshot_preserved::<CounterOutput>(&AgentTypeId::from_name("CounterAgent"), |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .expect("the one-shot turn runs and the session is preserved");
    assert_eq!(output.value, 1);

    let err = sessions
        .checkpoints()
        .capture(&preserved, CheckpointOrigin::Manual)
        .await
        .expect_err("capture must refuse a session it could never restore");
    assert!(matches!(err, SessionError::ReadOnly(_)), "got {err:?}");

    let err = sessions
        .checkpoints()
        .restore(&preserved, borrowed)
        .await
        .expect_err("and restore must refuse it too, as rollback always has");
    assert!(matches!(err, SessionError::ReadOnly(_)), "got {err:?}");

    sessions
        .checkpoint(&preserved)
        .await
        .expect("the legacy checkpoint accepted a preserved session before, and still does");
    assert!(
        !sessions.list_checkpoints(&preserved).unwrap().is_empty(),
        "and it really retained one"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Durability of the bound's bookkeeping
// ─────────────────────────────────────────────────────────────────────────────

/// A reclaimed point stays distinguishable from one that never existed, across
/// a restart. Without that, a host could only tell a user "no".
#[tokio::test]
async fn an_evicted_point_reports_evicted_after_a_save_and_resume() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    let retention = CheckpointRetention::new(Some(2), None);
    let id = SessionId::new();

    let (evicted, kept) = {
        let server = server_with(Arc::clone(&store), retention, false).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_session(&server, &id, 0);
        let evicted = capture(sessions, &id).await;
        capture(sessions, &id).await;
        let kept = capture(sessions, &id).await;
        assert!(matches!(
            sessions.checkpoints().status(&id, evicted).unwrap(),
            CheckpointStatus::Evicted { .. }
        ));
        sessions.save_session(&id).await.unwrap();
        (evicted, kept)
    };

    let server = server_with(Arc::clone(&store), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session(sessions.create_context(), &id)
        .await
        .unwrap();

    match sessions.checkpoints().status(&id, evicted).unwrap() {
        CheckpointStatus::Evicted { id: reported, .. } => assert_eq!(reported, evicted),
        other => panic!("a tombstone must survive a restart, got {other:?}"),
    }
    assert!(
        matches!(
            sessions.checkpoints().status(&id, kept).unwrap(),
            CheckpointStatus::Retained(_)
        ),
        "and the retained points come back too"
    );
}

/// Tombstones are compacted into a floor rather than accumulating, and every
/// id below the floor still answers `Evicted`.
#[tokio::test]
async fn old_tombstones_compact_into_a_floor() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    let retention = CheckpointRetention::new(Some(2), None);
    let server = server_with(Arc::clone(&store), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    let first = capture(sessions, &id).await;
    for _ in 0..20 {
        capture(sessions, &id).await;
    }

    sessions.save_session(&id).await.unwrap();
    let data = store.load(&id).await.unwrap().unwrap();
    assert!(
        data.checkpoint_history.tombstones.len() <= 2,
        "{} individual tombstones retained against a bound of 2",
        data.checkpoint_history.tombstones.len()
    );
    let floor = data
        .checkpoint_history
        .floor
        .expect("old removals fold into a floor marker");
    assert!(floor.id >= first, "the floor covers the earliest removals");

    assert!(
        matches!(
            sessions.checkpoints().status(&id, first).unwrap(),
            CheckpointStatus::Evicted { .. }
        ),
        "a compacted removal still reports as one"
    );
    assert_eq!(data.checkpoints.len(), 2, "and the payloads stay bounded");
}

/// The policy a host configured is the policy a consumer reads back, so a
/// consumer never has to restate a framework bound.
#[tokio::test]
async fn retention_and_usage_report_the_configured_policy() {
    let retention = CheckpointRetention::new(Some(5), Some(1 << 20));
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);

    assert_eq!(sessions.checkpoints().retention(), retention);

    capture(sessions, &id).await;
    let usage = sessions.checkpoints().usage(&id).unwrap();
    assert_eq!(usage.max_count, retention.max_count);
    assert_eq!(usage.max_bytes, retention.max_bytes);
    assert_eq!(usage.count, 1);
    assert!(usage.bytes > 0);
    assert_eq!(usage.pinned, 1, "the current point is held automatically");

    // The default is a product choice, not a measurement, and it is visible
    // rather than hidden in a constant at a call site.
    let default = CheckpointRetention::default();
    assert_eq!(default.max_count, Some(64));
    assert_eq!(default.max_bytes, Some(16 * 1024 * 1024));
    assert_eq!(CheckpointRetention::unbounded().max_count, None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Durability over both in-tree stores
// ─────────────────────────────────────────────────────────────────────────────

/// Neither in-tree store needed a change, which is the claim the whole
/// no-new-store design rests on. Proved against both.
async fn durability_round_trip(store: Arc<dyn SessionStore>) {
    let retention = CheckpointRetention::new(Some(8), None);
    let id = SessionId::new();

    let before = {
        let server = server_with(Arc::clone(&store), retention, true).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_session(&server, &id, 0);
        for _ in 0..3 {
            sessions.process_turn(&id).await.unwrap();
        }
        sessions.save_session(&id).await.unwrap();
        sessions.checkpoints().list(&id).unwrap()
    };
    assert_eq!(before.len(), 4);

    let server = server_with(Arc::clone(&store), retention, true).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session(sessions.create_context(), &id)
        .await
        .unwrap();

    let after = sessions.checkpoints().list(&id).unwrap();
    for point in &before {
        let restored = after
            .iter()
            .find(|meta| meta.id == point.id)
            .expect("every retained point survives the round-trip");
        assert_eq!(restored.resume_turn, point.resume_turn);
        assert_eq!(restored.turn, point.turn);
        assert_eq!(restored.origin, point.origin);
        assert_eq!(restored.parent, point.parent);
        assert_eq!(restored.bytes, point.bytes);
    }

    // A restored point is genuinely usable, not just listed.
    let outcome = sessions
        .checkpoints()
        .restore(&id, before[1].id)
        .await
        .expect("a point that survived a restart can be restored");
    assert_eq!(outcome.resume_turn, before[1].resume_turn);
    assert_eq!(
        sessions.session_info(&id).unwrap().turn_number,
        before[1].resume_turn
    );
}

#[tokio::test]
async fn checkpoints_round_trip_through_the_in_memory_store() {
    durability_round_trip(Arc::new(InMemoryStore::new())).await;
}

#[cfg(feature = "file-store")]
#[tokio::test]
async fn checkpoints_round_trip_through_the_file_store() {
    let dir = tempfile::tempdir().unwrap();
    durability_round_trip(Arc::new(polaris_sessions::FileStore::new(dir.path()))).await;
}

async fn restore_oldest_at_bound(retention: CheckpointRetention) {
    let server = server_with(Arc::new(InMemoryStore::new()), retention, false).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);
    let target = capture(sessions, &id).await;
    sessions.process_turn(&id).await.unwrap();
    capture(sessions, &id).await;
    let checkpoints = sessions.checkpoints();
    let outcome = checkpoints.restore(&id, target).await.unwrap();
    assert_eq!(checkpoints.current(&id).unwrap().unwrap().id, target);
    assert!(matches!(
        checkpoints.status(&id, target).unwrap(),
        CheckpointStatus::Retained(_)
    ));
    assert!(matches!(
        checkpoints.status(&id, outcome.original).unwrap(),
        CheckpointStatus::OffHistory(_)
    ));
    assert_eq!(checkpoints.usage(&id).unwrap().count, 2);
}

#[tokio::test]
async fn restore_oldest_at_count_bound_keeps_target_and_original() {
    restore_oldest_at_bound(CheckpointRetention::new(Some(2), None)).await;
}

#[tokio::test]
async fn restore_oldest_at_byte_bound_keeps_target_and_original() {
    let server = server_with(
        Arc::new(InMemoryStore::new()),
        CheckpointRetention::unbounded(),
        false,
    )
    .await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);
    let point = sessions
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    restore_oldest_at_bound(CheckpointRetention::new(None, Some(point.bytes * 2))).await;
}

#[tokio::test]
async fn refused_restore_releases_target_pin_and_preserves_explicit_current_pin() {
    let server = server_with(
        Arc::new(InMemoryStore::new()),
        CheckpointRetention::new(Some(2), None),
        false,
    )
    .await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id, 0);
    let target = capture(sessions, &id).await;
    sessions.process_turn(&id).await.unwrap();
    let current = capture(sessions, &id).await;
    let checkpoints = sessions.checkpoints();
    let pin = checkpoints.pin(&id, current).unwrap();
    assert!(matches!(
        checkpoints.restore(&id, target).await,
        Err(SessionError::CheckpointCapacity { .. })
    ));
    assert_eq!(checkpoints.current(&id).unwrap().unwrap().id, current);
    assert_eq!(checkpoints.usage(&id).unwrap().pinned, 1);
    assert_eq!(
        sessions
            .with_context(&id, |ctx| ctx.get_resource::<Counter>().unwrap().value)
            .await
            .unwrap(),
        1
    );
    capture(sessions, &id).await;
    assert!(matches!(
        checkpoints.status(&id, target).unwrap(),
        CheckpointStatus::Evicted { .. }
    ));
    drop(pin);
}
