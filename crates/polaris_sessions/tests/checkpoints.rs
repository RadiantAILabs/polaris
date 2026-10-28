//! Checkpoint identity, resume semantics and branch-safe restore.
//!
//! Retention lives in `checkpoint_retention.rs`; this file is about what a
//! checkpoint *is* and what restoring one does. The legacy
//! `checkpoint`/`list_checkpoints`/`rollback` contract is proved separately, and
//! unmodified, by `http_checkpoints.rs`.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::{
    PersistenceError, PersistencePlugin, ResourceSerializer, Storable,
};
use polaris_graph::graph::Graph;
use polaris_sessions::checkpoint::{
    CheckpointOrigin, CheckpointRetention, CheckpointStatus, RestoreFailureCause,
};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionInfo, SessionsAPI, SessionsPlugin};
use polaris_system::param::{Res, ResMut, SystemContext};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Counts turns and records which turn number actually executed, so a test can
/// assert the *run identity* a restore resumed at rather than only the counter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Storable)]
#[storable(key = "Counter")]
struct Counter {
    value: u32,
    turns_seen: Vec<u32>,
}
impl LocalResource for Counter {}

#[system]
async fn increment(mut counter: ResMut<Counter>, info: Res<SessionInfo>) {
    counter.value += 1;
    counter.turns_seen.push(info.turn_number);
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

/// A hand-written serializer for [`Counter`], so a test can move the schema
/// version or make a load fail — neither of which a `Storable` derive can do,
/// because both are compile-time constants there.
struct CounterSerializer {
    version: &'static str,
    fail_on_load: bool,
}

impl CounterSerializer {
    fn at(version: &'static str) -> Arc<dyn ResourceSerializer> {
        Arc::new(Self {
            version,
            fail_on_load: false,
        })
    }

    fn failing() -> Arc<dyn ResourceSerializer> {
        Arc::new(Self {
            version: "1.0.0",
            fail_on_load: true,
        })
    }
}

impl ResourceSerializer for CounterSerializer {
    fn plugin_id(&self) -> &'static str {
        "test"
    }

    fn storage_key(&self) -> &'static str {
        "Counter"
    }

    fn schema_version(&self) -> &'static str {
        self.version
    }

    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError> {
        if !ctx.contains_local_resource::<Counter>() {
            return Ok(None);
        }
        let guard = ctx
            .get_resource::<Counter>()
            .map_err(|err| PersistenceError::ResourceAccess(format!("{err}")))?;
        Ok(Some(serde_json::to_value(&*guard)?))
    }

    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError> {
        if self.fail_on_load {
            return Err(PersistenceError::ResourceAccess(
                "deliberate load failure".to_owned(),
            ));
        }
        let counter: Counter = serde_json::from_value(value)?;
        ctx.insert(counter);
        Ok(())
    }
}

/// A server with auto-checkpoint left at its default — **on**. Every existing
/// checkpoint test builds its server with `without_auto_checkpoint()`, which is
/// why the auto path's numbering defect had no coverage.
async fn test_server(store: Arc<InMemoryStore>) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store));
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(vec![CounterSerializer::at("1.0.0")]);
    sessions.register_agent(CounterAgent).unwrap();
    server
}

fn create_session(server: &Server, id: &SessionId) {
    server
        .api::<SessionsAPI>()
        .unwrap()
        .create_session_with(
            server.create_context(),
            id,
            &AgentTypeId::from_name("CounterAgent"),
            |ctx| {
                ctx.insert(Counter::default());
            },
        )
        .unwrap();
}

async fn counter(sessions: &SessionsAPI, id: &SessionId) -> Counter {
    sessions
        .with_context(id, |ctx| ctx.get_resource::<Counter>().unwrap().clone())
        .await
        .unwrap()
}

// ─────────────────────────────────────────────────────────────────────────────
// Identity
// ─────────────────────────────────────────────────────────────────────────────

/// Creation boundary plus one point per turn, with distinct ids and resume
/// turns that count up from zero.
#[tokio::test]
async fn auto_checkpoints_have_distinct_ids_and_monotonic_resume_turns() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }

    let points = sessions.checkpoints().list(&id).unwrap();
    assert_eq!(
        points.len(),
        4,
        "a before-first-prompt boundary plus one point per turn"
    );

    let ids: Vec<u64> = points.iter().map(|meta| meta.id.as_u64()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(ids, sorted, "ids are distinct and strictly increasing");

    let resume: Vec<u32> = points.iter().map(|meta| meta.resume_turn).collect();
    assert_eq!(
        resume,
        vec![0, 1, 2, 3],
        "resume_turn is the turn to run next, so the boundary is 0 and turn N's point is N + 1"
    );

    // The display labels are *not* identities, and two points share one here:
    // the creation boundary and the post-turn-0 point are both labelled 0.
    assert_eq!(sessions.list_checkpoints(&id).unwrap(), vec![0, 0, 1, 2]);

    let origins: Vec<CheckpointOrigin> = points.iter().map(|meta| meta.origin).collect();
    assert!(
        origins
            .iter()
            .all(|origin| *origin == CheckpointOrigin::Auto)
    );

    assert_eq!(
        points[0].parent, None,
        "the first point in a session has no parent"
    );
    assert_eq!(
        points[3].parent,
        Some(points[2].id),
        "each later point hangs off the one that was current"
    );
    assert_eq!(
        sessions.checkpoints().current(&id).unwrap().unwrap().id,
        points[3].id
    );
}

/// The fix for the auto path's numbering, on the default-on path that had the
/// defect. Contrast with `rollback` below, which still has it.
#[tokio::test]
async fn restoring_an_auto_checkpoint_does_not_reuse_a_run_identity() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }
    assert_eq!(counter(sessions, &id).await.turns_seen, vec![0, 1, 2]);

    // The point captured automatically after turn 0 — labelled 0, resumes at 1.
    let after_turn_zero = sessions
        .checkpoints()
        .list(&id)
        .unwrap()
        .into_iter()
        .find(|meta| meta.resume_turn == 1)
        .expect("the post-turn-0 automatic point");
    assert_eq!(
        after_turn_zero.turn, 0,
        "its display label is the turn that ran"
    );

    let outcome = sessions
        .checkpoints()
        .restore(&id, after_turn_zero.id)
        .await
        .unwrap();
    assert_eq!(outcome.resume_turn, 1);
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 1);

    sessions.process_turn(&id).await.unwrap();
    let seen = counter(sessions, &id).await.turns_seen;
    assert_eq!(
        seen,
        vec![0, 1],
        "the restored state had run turn 0, and the next run is turn 1 - not a second turn 0"
    );
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 2);
}

/// The residual defect, pinned as behaviour so nobody "fixes" it by accident:
/// the legacy `rollback` of an *automatic* checkpoint re-uses run identity
/// `(session, N)`. Recorded, documented on `rollback`, and deliberately not
/// changed here — `restore` above is where correct behaviour lives.
#[tokio::test]
async fn rollback_of_an_auto_checkpoint_still_reuses_the_run_identity() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    for _ in 0..2 {
        sessions.process_turn(&id).await.unwrap();
    }
    assert_eq!(counter(sessions, &id).await.turns_seen, vec![0, 1]);

    // Label 1 is the automatic point taken after turn 1 completed.
    sessions.rollback(&id, 1).await.unwrap();
    assert_eq!(
        sessions.session_info(&id).unwrap().turn_number,
        1,
        "rollback sets the counter to the label, which is the turn that already ran"
    );

    sessions.process_turn(&id).await.unwrap();
    assert_eq!(
        counter(sessions, &id).await.turns_seen,
        vec![0, 1, 1],
        "so turn 1 runs twice: the documented hazard, unchanged"
    );
}

/// Legacy rollback discards future points, but the identity-aware surface
/// remembers that they existed and reports them as evicted.
#[tokio::test]
async fn rollback_reports_discarded_points_as_evicted() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }
    let discarded: Vec<_> = sessions
        .checkpoints()
        .list(&id)
        .unwrap()
        .into_iter()
        .filter(|meta| meta.turn > 0)
        .map(|meta| meta.id)
        .collect();
    assert!(
        !discarded.is_empty(),
        "the fixture must include future points"
    );

    sessions.rollback(&id, 0).await.unwrap();

    for checkpoint in discarded {
        assert!(
            matches!(
                sessions.checkpoints().status(&id, checkpoint).unwrap(),
                CheckpointStatus::Evicted { id: evicted, .. } if evicted == checkpoint
            ),
            "rollback-discarded checkpoint {checkpoint} must remain known as evicted"
        );
    }
}

/// Every origin stores resume semantics rather than a display label.
#[tokio::test]
async fn resume_turn_is_correct_for_auto_manual_and_pre_restore() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id);

    sessions.process_turn(&id).await.unwrap();
    let auto = checkpoints.current(&id).unwrap().unwrap();
    assert_eq!(
        (auto.origin, auto.turn, auto.resume_turn),
        (CheckpointOrigin::Auto, 0, 1)
    );

    let manual = checkpoints
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    assert_eq!(
        (manual.origin, manual.turn, manual.resume_turn),
        (CheckpointOrigin::Manual, 1, 1),
        "a manual capture labels itself with the counter, and resumes at the same turn"
    );

    let outcome = checkpoints.restore(&id, auto.id).await.unwrap();
    let pre_restore = checkpoints
        .list(&id)
        .unwrap()
        .into_iter()
        .find(|meta| meta.id == outcome.original)
        .expect("the pre-restore point is retained");
    assert_eq!(pre_restore.origin, CheckpointOrigin::PreRestore);
    assert_eq!(
        pre_restore.resume_turn, 1,
        "it captures the state being left, which would have run turn 1 next"
    );

    // Both automatic and manual points describe the same state and resume the
    // same way, even though their labels differ.
    assert_eq!(auto.resume_turn, manual.resume_turn);
    assert_ne!(auto.turn, manual.turn);
}

/// A restart keeps identity unique: a resumed session never hands out an id a
/// previous process already used.
#[tokio::test]
async fn identity_stays_unique_across_a_restart() {
    let store = Arc::new(InMemoryStore::new());
    let id = SessionId::new();

    let before = {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_session(&server, &id);
        sessions.process_turn(&id).await.unwrap();
        sessions.save_session(&id).await.unwrap();
        sessions.checkpoints().list(&id).unwrap()
    };

    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session(sessions.create_context(), &id)
        .await
        .unwrap();

    let after = sessions.checkpoints().list(&id).unwrap();
    for restored in &before {
        assert!(
            after.iter().any(|meta| meta.id == restored.id
                && meta.resume_turn == restored.resume_turn
                && meta.turn == restored.turn),
            "checkpoint {} survived the restart with its identity and resume turn",
            restored.id
        );
    }

    let fresh = sessions
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    let highest_before = before.iter().map(|meta| meta.id.as_u64()).max().unwrap();
    assert!(
        fresh.id.as_u64() > highest_before,
        "a new capture allocates above every id the previous process used"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Boundaries and branches
// ─────────────────────────────────────────────────────────────────────────────

/// Session creation and resume each take a boundary point, so "before the
/// first prompt" and "as resumed" are both addressable.
#[tokio::test]
async fn session_creation_and_resume_take_boundary_checkpoints() {
    let store = Arc::new(InMemoryStore::new());
    let id = SessionId::new();

    {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_session(&server, &id);

        let boundary = sessions.checkpoints().list(&id).unwrap();
        assert_eq!(boundary.len(), 1, "a fresh session has its creation point");
        assert_eq!(boundary[0].resume_turn, 0);
        assert_eq!(boundary[0].origin, CheckpointOrigin::Auto);

        sessions.process_turn(&id).await.unwrap();
        sessions.save_session(&id).await.unwrap();
    }

    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session(sessions.create_context(), &id)
        .await
        .unwrap();

    let points = sessions.checkpoints().list(&id).unwrap();
    assert_eq!(
        points.len(),
        3,
        "the two persisted points plus a resume boundary"
    );
    let resumed = points.last().unwrap();
    assert_eq!(
        resumed.resume_turn, 1,
        "the boundary resumes at the stored counter"
    );
    assert_eq!(
        sessions.checkpoints().current(&id).unwrap().unwrap().id,
        resumed.id
    );
}

/// A rewind after a rewind is well defined: each restore opens a revision, the
/// abandoned chain stays listed, and it reports itself as off-history rather
/// than being discarded by a numeric turn test.
#[tokio::test]
async fn a_second_restore_branches_and_the_abandoned_chain_reports_off_history() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id);

    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }
    let target = checkpoints
        .list(&id)
        .unwrap()
        .into_iter()
        .find(|meta| meta.resume_turn == 1)
        .unwrap();
    let abandoned: Vec<_> = checkpoints
        .list(&id)
        .unwrap()
        .into_iter()
        .filter(|meta| meta.resume_turn > 1)
        .collect();

    let first = checkpoints.restore(&id, target.id).await.unwrap();
    assert_eq!(first.revision.as_u32(), 1);

    // Nothing was discarded, unlike `rollback`.
    assert!(
        abandoned.iter().all(|meta| checkpoints
            .list(&id)
            .unwrap()
            .iter()
            .any(|kept| kept.id == meta.id)),
        "a restore retains the chain it left; retention is what bounds growth"
    );
    for meta in &abandoned {
        assert!(
            matches!(
                checkpoints.status(&id, meta.id).unwrap(),
                CheckpointStatus::OffHistory(_)
            ),
            "the abandoned chain is off the current history path"
        );
    }
    assert!(matches!(
        checkpoints.status(&id, target.id).unwrap(),
        CheckpointStatus::Retained(_)
    ));

    // A second restore to the same point must work, and must open a further
    // revision rather than colliding with the first.
    sessions.process_turn(&id).await.unwrap();
    let second = checkpoints.restore(&id, target.id).await.unwrap();
    assert_eq!(second.revision.as_u32(), 2);
    assert!(second.revision > first.revision);
    assert_ne!(second.original, first.original);
    assert_eq!(second.resume_turn, 1);

    // Parentage, not turn arithmetic, is what makes that decidable.
    let next = checkpoints
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    assert_eq!(next.parent, Some(target.id));
    assert_eq!(next.revision.as_u32(), 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Restore failure semantics
// ─────────────────────────────────────────────────────────────────────────────

/// A partial restore is reported, and `restore_exact` refuses it without
/// writing anything.
#[tokio::test]
async fn restore_reports_unmatched_entries_and_restore_exact_refuses_them() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id);

    sessions.process_turn(&id).await.unwrap();
    let target = checkpoints.current(&id).unwrap().unwrap();
    sessions.process_turn(&id).await.unwrap();
    let before = counter(sessions, &id).await;

    // The resource's schema moves under the checkpoint, exactly as a version
    // bump between two releases would.
    sessions.set_serializers(vec![CounterSerializer::at("2.0.0")]);

    let err = checkpoints
        .restore_exact(&id, target.id)
        .await
        .expect_err("an exact restore must refuse an entry it cannot load");
    match err {
        SessionError::RestoreIncomplete {
            target: refused,
            unmatched,
            ..
        } => {
            assert_eq!(refused, target.id);
            assert_eq!(unmatched.len(), 1);
            assert_eq!(unmatched[0].storage_key, "Counter");
            assert_eq!(unmatched[0].version, "1.0.0");
        }
        other => panic!("expected RestoreIncomplete, got {other:?}"),
    }
    assert_eq!(
        counter(sessions, &id).await,
        before,
        "a refused exact restore leaves the context byte-identical"
    );
    assert_eq!(
        checkpoints.usage(&id).unwrap().pinned,
        1,
        "the refused restore releases its temporary target pin"
    );
    assert_eq!(
        checkpoints.list(&id).unwrap().len(),
        3,
        "and takes no pre-restore checkpoint either"
    );

    // The permissive mode restores what it can and says what it could not.
    let outcome = checkpoints.restore(&id, target.id).await.unwrap();
    assert!(outcome.restored.is_empty());
    assert_eq!(outcome.unmatched.len(), 1);
    assert_eq!(outcome.unmatched[0].version, "1.0.0");
    assert_eq!(
        outcome.resume_turn, 1,
        "the turn counter still moves to the target's resume turn"
    );
}

/// The resources a restore did not rewind are named rather than left for the
/// caller to discover.
#[tokio::test]
async fn restore_reports_resources_it_carried_over() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id);

    // Capture with no serializer registered at all, so the checkpoint holds
    // nothing while the context holds a live `Counter`.
    sessions.set_serializers(Vec::new());
    let empty = checkpoints
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    sessions.set_serializers(vec![CounterSerializer::at("1.0.0")]);

    let outcome = checkpoints.restore(&id, empty.id).await.unwrap();
    assert!(outcome.restored.is_empty());
    assert!(outcome.unmatched.is_empty());
    assert_eq!(
        outcome.carried_over.len(),
        1,
        "the live Counter matched no entry in the target, so it was not rewound"
    );
    assert_eq!(outcome.carried_over[0].storage_key, "Counter");
}

/// A restore that fails partway leaves the context unknown, so the session
/// refuses to run another turn on it.
#[tokio::test]
async fn a_failed_restore_marks_the_session_incoherent_and_process_turn_refuses() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();
    let id = SessionId::new();
    create_session(&server, &id);

    sessions.process_turn(&id).await.unwrap();
    let target = checkpoints.current(&id).unwrap().unwrap();
    sessions.process_turn(&id).await.unwrap();

    sessions.set_serializers(vec![CounterSerializer::failing()]);
    let err = checkpoints
        .restore(&id, target.id)
        .await
        .expect_err("a load failure must not be reported as a restore");
    let original = match err {
        SessionError::RestoreFailed {
            target: failed,
            original,
            source,
            ..
        } => {
            assert_eq!(failed, target.id);
            assert!(
                matches!(
                    source,
                    RestoreFailureCause::ResourceLoad(inner)
                        if matches!(*inner, SessionError::Persistence(_))
                ),
                "the first failure must retain its typed resource-load cause"
            );
            original
        }
        other => panic!("expected RestoreFailed, got {other:?}"),
    };

    // The recovery route survives, and is held against eviction.
    assert!(matches!(
        checkpoints.status(&id, original).unwrap(),
        CheckpointStatus::Retained(_)
    ));

    let refused = sessions
        .process_turn(&id)
        .await
        .expect_err("a turn on an unknown context must be refused");
    match refused {
        SessionError::RestoreFailed {
            target: refused_target,
            original: refused_original,
            source: RestoreFailureCause::Incoherent { reason },
            ..
        } => {
            assert_eq!(refused_target, target.id);
            assert_eq!(refused_original, original);
            assert!(reason.contains("deliberate load failure"));
        }
        other => panic!("expected a typed incoherence refusal, got {other:?}"),
    }

    // A restore that succeeds clears the refusal.
    sessions.set_serializers(vec![CounterSerializer::at("1.0.0")]);
    checkpoints.restore(&id, target.id).await.unwrap();
    sessions
        .process_turn(&id)
        .await
        .expect("a successful restore restores coherence");
}

/// An id that belongs to another session is unknown here, and restoring it
/// fails with a checkpoint error rather than the legacy turn error.
#[tokio::test]
async fn an_id_from_another_session_is_unknown_not_a_turn_error() {
    let server = test_server(Arc::new(InMemoryStore::new())).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let checkpoints = sessions.checkpoints();

    let first = SessionId::new();
    let second = SessionId::new();
    create_session(&server, &first);
    create_session(&server, &second);

    let elsewhere = checkpoints
        .capture(&first, CheckpointOrigin::Manual)
        .await
        .unwrap();

    assert!(
        matches!(
            checkpoints.status(&second, elsewhere.id).unwrap(),
            CheckpointStatus::Unknown(_)
        ),
        "never claiming an eviction it cannot know about is the point of Unknown"
    );

    let err = checkpoints
        .restore(&second, elsewhere.id)
        .await
        .unwrap_err();
    match err {
        SessionError::CheckpointNotFound {
            session,
            checkpoint,
        } => {
            assert_eq!(session, second);
            assert_eq!(checkpoint, elsewhere.id);
        }
        other => panic!("expected CheckpointNotFound, got {other:?}"),
    }

    let legacy = sessions.rollback(&second, 99).await.unwrap_err();
    assert!(
        matches!(legacy, SessionError::TurnNotFound(99)),
        "the legacy path keeps its own error: {legacy:?}"
    );
}

#[tokio::test]
async fn failed_restore_can_recover_at_two_point_bound() {
    let mut server = Server::new();
    server.add_plugins(PersistencePlugin).add_plugins(
        SessionsPlugin::new(Arc::new(InMemoryStore::new()))
            .without_auto_checkpoint()
            .with_checkpoint_retention(CheckpointRetention::new(Some(2), None)),
    );
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(CounterAgent).unwrap();
    sessions.set_serializers(vec![CounterSerializer::at("1.0.0")]);
    let id = SessionId::new();
    create_session(&server, &id);
    let checkpoints = sessions.checkpoints();
    checkpoints
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    sessions.process_turn(&id).await.unwrap();
    let target = checkpoints
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    sessions.set_serializers(vec![CounterSerializer::failing()]);
    let original = match checkpoints.restore(&id, target.id).await.unwrap_err() {
        SessionError::RestoreFailed { original, .. } => original,
        other => panic!("expected load failure, got {other:?}"),
    };
    assert!(matches!(checkpoints.restore(&id, target.id).await,
        Err(SessionError::RestoreFailed { original: held, .. }) if held == original));
    assert!(matches!(
        checkpoints.capture(&id, CheckpointOrigin::Manual).await,
        Err(SessionError::RestoreFailed { .. })
    ));
    assert!(matches!(
        checkpoints.status(&id, original).unwrap(),
        CheckpointStatus::Retained(_)
    ));
    sessions.set_serializers(vec![CounterSerializer::at("1.0.0")]);
    let outcome = checkpoints.restore(&id, original).await.unwrap();
    assert_eq!(
        outcome.original, original,
        "recovery reuses the coherent original"
    );
    assert_eq!(counter(sessions, &id).await.value, 1);
    sessions.process_turn(&id).await.unwrap();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Secondary(u32);
impl LocalResource for Secondary {}

struct SecondarySerializer {
    fail: bool,
}
impl ResourceSerializer for SecondarySerializer {
    fn plugin_id(&self) -> &'static str {
        "test"
    }
    fn storage_key(&self) -> &'static str {
        "Secondary"
    }
    fn schema_version(&self) -> &'static str {
        "1.0.0"
    }
    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError> {
        let value = ctx.get_resource::<Secondary>().unwrap();
        Ok(Some(serde_json::to_value(&*value)?))
    }
    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError> {
        if self.fail {
            return Err(PersistenceError::ResourceAccess(
                "secondary load failed".into(),
            ));
        }
        ctx.insert(serde_json::from_value::<Secondary>(value)?);
        Ok(())
    }
}

#[tokio::test]
async fn partial_restore_cannot_be_saved_or_captured_as_healthy() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);
    sessions
        .with_context(&id, |ctx| {
            ctx.insert(Secondary(0));
        })
        .await
        .unwrap();
    sessions.set_serializers(vec![
        CounterSerializer::at("1.0.0"),
        Arc::new(SecondarySerializer { fail: false }),
    ]);
    let target = sessions
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    sessions.process_turn(&id).await.unwrap();
    sessions
        .with_context(&id, |ctx| {
            ctx.insert(Secondary(1));
        })
        .await
        .unwrap();
    sessions.save_session(&id).await.unwrap();
    sessions.set_serializers(vec![
        CounterSerializer::at("1.0.0"),
        Arc::new(SecondarySerializer { fail: true }),
    ]);
    assert!(matches!(
        sessions.checkpoints().restore(&id, target.id).await,
        Err(SessionError::RestoreFailed { .. })
    ));
    assert_eq!(
        counter(sessions, &id).await.value,
        0,
        "first resource was rewound"
    );
    assert_eq!(
        sessions
            .with_context(&id, |ctx| ctx.get_resource::<Secondary>().unwrap().0)
            .await
            .unwrap(),
        1,
        "second resource was not rewound"
    );
    let saved = sessions.save_session(&id).await;
    let captured = sessions
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await;
    // Resume the last coherent save in a fresh server.
    let resumed = test_server(store).await;
    let resumed_sessions = resumed.api::<SessionsAPI>().unwrap();
    resumed_sessions.set_serializers(vec![
        CounterSerializer::at("1.0.0"),
        Arc::new(SecondarySerializer { fail: false }),
    ]);
    resumed_sessions
        .resume_session(resumed_sessions.create_context(), &id)
        .await
        .unwrap();
    assert_eq!(
        counter(resumed_sessions, &id).await.value,
        1,
        "refused save preserves the coherent stored state"
    );
    assert!(matches!(saved, Err(SessionError::RestoreFailed { .. })));
    assert!(matches!(captured, Err(SessionError::RestoreFailed { .. })));
}
