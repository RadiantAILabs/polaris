//! Span-capture tests for the `polaris.session.turn` span.
//!
//! `process_turn` stamps the `invoke_agent` semantics onto the turn
//! span (agent identity, `otel.name`/`otel.kind`) and, when execution fails,
//! records `otel.status_code` and a derived `error.type`. These tests install
//! a thread-local capturing subscriber, drive one real turn, and assert on the
//! attributes the span actually carries.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_graph::executor::ExecutionError;
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::{Res, ResMut};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, Registry};

// ─────────────────────────────────────────────────────────────────────────────
// Agents
// ─────────────────────────────────────────────────────────────────────────────

/// A local resource the successful agent flips, so the turn does real work.
#[derive(Debug, Clone, Default)]
struct Marker {
    hit: bool,
}
impl LocalResource for Marker {}

#[system]
async fn touch_marker(mut marker: ResMut<Marker>) {
    marker.hit = true;
}

struct MarkerAgent;

impl Agent for MarkerAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(touch_marker);
    }

    fn name(&self) -> &'static str {
        "MarkerAgent"
    }
}

/// A resource the failing agent requires but the session never inserts, so
/// `Res<MissingInput>` fails to resolve and the turn ends in an
/// `ExecutionError`.
#[derive(Debug, Clone)]
struct MissingInput;
impl LocalResource for MissingInput {}

#[system]
async fn needs_missing(_input: Res<MissingInput>) {}

struct FailingAgent;

impl Agent for FailingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(needs_missing);
    }

    fn name(&self) -> &'static str {
        "FailingAgent"
    }
}

/// An agent that supplies the optional version and description identity, so the
/// turn span's handling of present values can be tested alongside
/// [`MarkerAgent`]'s absent-value case.
struct IdentifiedAgent;

impl Agent for IdentifiedAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(touch_marker);
    }

    fn name(&self) -> &'static str {
        "IdentifiedAgent"
    }

    fn version(&self) -> Option<&str> {
        Some("2.1.0")
    }

    fn description(&self) -> Option<&str> {
        Some("Touches the marker, with a name tag on")
    }
}

/// A redeployed build of [`IdentifiedAgent`] with same agent-type name, bumped
/// version and description.
struct RedeployedAgent;

impl Agent for RedeployedAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(touch_marker);
    }

    fn name(&self) -> &'static str {
        "IdentifiedAgent"
    }

    fn version(&self) -> Option<&str> {
        Some("3.0.0")
    }

    fn description(&self) -> Option<&str> {
        Some("Redeployed marker toucher")
    }
}

/// An agent that supplies only a version, leaving `description` at its default
/// `None`.
struct VersionOnlyAgent;

impl Agent for VersionOnlyAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(touch_marker);
    }

    fn name(&self) -> &'static str {
        "VersionOnlyAgent"
    }

    fn version(&self) -> Option<&str> {
        Some("1.4.2")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Span capture
// ─────────────────────────────────────────────────────────────────────────────

type Fields = Arc<Mutex<HashMap<String, String>>>;

/// Collects every string-valued field it visits into a flat map. Numeric
/// fields (e.g. `polaris.session.turn_number`) are ignored — the turn's
/// identity attributes are all strings.
struct FieldVisitor(HashMap<String, String>);

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        // `%value` fields (Display) reach the visitor through `record_debug`
        // wrapping a `format_args!`, whose Debug is the plain Display string.
        self.0
            .entry(field.name().to_string())
            .or_insert_with(|| format!("{value:?}"));
    }
}

/// Captures the fields recorded on the `polaris.session.turn` span — both the
/// ones set at creation and the ones `.record()`ed afterwards
/// (`otel.name`/`otel.kind`, and on failure `otel.status_code`/`error.type`).
struct TurnSpanCapture {
    fields: Fields,
}

impl TurnSpanCapture {
    fn merge(&self, visitor: FieldVisitor) {
        let mut fields = self.fields.lock().unwrap();
        for (key, value) in visitor.0 {
            fields.insert(key, value);
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for TurnSpanCapture
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: LayerContext<'_, S>) {
        if attrs.metadata().name() != "polaris.session.turn" {
            return;
        }
        let mut visitor = FieldVisitor(HashMap::new());
        attrs.record(&mut visitor);
        self.merge(visitor);
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: LayerContext<'_, S>) {
        let is_turn = ctx
            .span(id)
            .is_some_and(|span_ref| span_ref.name() == "polaris.session.turn");
        if !is_turn {
            return;
        }
        let mut visitor = FieldVisitor(HashMap::new());
        values.record(&mut visitor);
        self.merge(visitor);
    }
}

/// Builds a minimal sessions server (auto-checkpoint off) backed by `store` and
/// registers `agent`.
async fn server_with_store(agent: impl Agent, store: Arc<InMemoryStore>) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store).without_auto_checkpoint());
    server.finish().await.unwrap();
    server
        .api::<SessionsAPI>()
        .unwrap()
        .register_agent(agent)
        .unwrap();
    server
}

/// Builds a minimal sessions server (fresh store, auto-checkpoint off) and
/// registers `agent`.
async fn server_with(agent: impl Agent) -> Server {
    server_with_store(agent, Arc::new(InMemoryStore::new())).await
}

fn get(fields: &Fields, key: &str) -> Option<String> {
    fields.lock().unwrap().get(key).cloned()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// A successful turn stamps the `invoke_agent` identity onto the turn
/// span and leaves the error attributes unset.
#[tokio::test(flavor = "current_thread")]
async fn turn_span_records_invoke_agent_semantics() {
    let fields: Fields = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(TurnSpanCapture {
        fields: Arc::clone(&fields),
    });
    // Thread-local (free `set_default`) rather than a global install: the
    // current-thread runtime keeps the turn on this thread, and a global
    // subscriber would clash with other tests in the same binary.
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = server_with(MarkerAgent).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("MarkerAgent"),
            |ctx| {
                ctx.insert(Marker::default());
            },
        )
        .unwrap();

    sessions.process_turn(&id).await.unwrap();

    assert_eq!(
        get(&fields, "gen_ai.operation.name").as_deref(),
        Some("invoke_agent")
    );
    assert_eq!(
        get(&fields, "gen_ai.agent.name").as_deref(),
        Some("MarkerAgent")
    );
    assert_eq!(
        get(&fields, "gen_ai.conversation.id").as_deref(),
        Some(id.to_string().as_str())
    );
    assert_eq!(
        get(&fields, "otel.name").as_deref(),
        Some("invoke_agent MarkerAgent")
    );
    assert_eq!(get(&fields, "otel.kind").as_deref(), Some("Internal"));
    // Success leaves the error placeholders as `Empty` — never recorded.
    assert_eq!(get(&fields, "otel.status_code"), None);
    assert_eq!(get(&fields, "error.type"), None);
    assert_eq!(
        get(&fields, "gen_ai.agent.version"),
        None,
        "agent supplying no version should leave gen_ai.agent.version unrecorded"
    );
    assert_eq!(
        get(&fields, "gen_ai.agent.description"),
        None,
        "agent supplying no description should leave gen_ai.agent.description unrecorded"
    );
}

/// When an agent supplies a version and description, the turn span records
/// both onto its `gen_ai.agent.*` fields.
#[tokio::test(flavor = "current_thread")]
async fn turn_span_records_optional_agent_identity_when_present() {
    let fields: Fields = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(TurnSpanCapture {
        fields: Arc::clone(&fields),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = server_with(IdentifiedAgent).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("IdentifiedAgent"),
            |ctx| {
                ctx.insert(Marker::default());
            },
        )
        .unwrap();

    sessions.process_turn(&id).await.unwrap();

    assert_eq!(
        get(&fields, "gen_ai.agent.version").as_deref(),
        Some("2.1.0"),
        "agent-supplied version should be recorded onto gen_ai.agent.version"
    );
    assert_eq!(
        get(&fields, "gen_ai.agent.description").as_deref(),
        Some("Touches the marker, with a name tag on"),
        "agent-supplied description should be recorded onto gen_ai.agent.description"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn turn_span_records_partial_agent_identity() {
    let fields: Fields = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(TurnSpanCapture {
        fields: Arc::clone(&fields),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = server_with(VersionOnlyAgent).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("VersionOnlyAgent"),
            |ctx| {
                ctx.insert(Marker::default());
            },
        )
        .unwrap();

    sessions.process_turn(&id).await.unwrap();

    assert_eq!(
        get(&fields, "gen_ai.agent.version").as_deref(),
        Some("1.4.2"),
        "the supplied version should be recorded even when description is absent"
    );
    assert_eq!(
        get(&fields, "gen_ai.agent.description"),
        None,
        "an absent description should stay unrecorded even when version is present"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn turn_span_reflects_redeployed_agent_version_after_resume() {
    let fields: Fields = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(TurnSpanCapture {
        fields: Arc::clone(&fields),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let store = Arc::new(InMemoryStore::new());
    let id = SessionId::new();

    // First deployment: create a session under v2.1.0 and persist it.
    {
        let server = server_with_store(IdentifiedAgent, Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions
            .create_session_with(
                server.create_context(),
                &id,
                &AgentTypeId::from_name("IdentifiedAgent"),
                |ctx| {
                    ctx.insert(Marker::default());
                },
            )
            .unwrap();
        sessions.save_session(&id).await.unwrap();
    }

    // Redeploy: fresh server on the same store, same agent-type name at v3.0.0.
    let server = server_with_store(RedeployedAgent, store).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session_with(server.create_context(), &id, |ctx| {
            ctx.insert(Marker::default());
        })
        .await
        .unwrap();

    sessions.process_turn(&id).await.unwrap();

    assert_eq!(
        get(&fields, "gen_ai.agent.version").as_deref(),
        Some("3.0.0"),
        "a session resumed after a redeploy should report the newly-registered version"
    );
    assert_eq!(
        get(&fields, "gen_ai.agent.description").as_deref(),
        Some("Redeployed marker toucher"),
        "a session resumed after a redeploy should report the newly-registered description"
    );
}

/// A failing turn records `otel.status_code = "ERROR"` and derives
/// `error.type` from the actual `ExecutionError` variant — not the old
/// hardcoded `"graph_execution_error"` string.
#[tokio::test(flavor = "current_thread")]
async fn turn_span_records_error_status_on_failure() {
    let fields: Fields = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(TurnSpanCapture {
        fields: Arc::clone(&fields),
    });
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = server_with(FailingAgent).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    // Deliberately omit `MissingInput` so the turn fails.
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("FailingAgent"),
            |_| {},
        )
        .unwrap();

    let err = sessions.process_turn(&id).await.unwrap_err();
    let SessionError::Execution(exec_err) = &err else {
        panic!("expected an execution error, got {err:?}");
    };

    assert_eq!(get(&fields, "otel.status_code").as_deref(), Some("ERROR"));

    // `FailingAgent`'s `Res<MissingInput>` cannot resolve, so the turn fails with
    // `ExecutionError::SystemError`, whose `Debug` discriminant is what the span
    // records. Assert that literal directly — deriving the expected value from
    // `exec_err` (as an earlier revision did) merely re-runs the production code
    // path and proves nothing.
    assert!(
        matches!(exec_err, ExecutionError::SystemError(_)),
        "missing Res<MissingInput> should surface as ExecutionError::SystemError, got {exec_err:?}"
    );
    let recorded = get(&fields, "error.type").expect("error.type should be recorded");
    assert_eq!(recorded, "SystemError");
    assert_ne!(recorded, "graph_execution_error");

    // Identity attributes are still present on a failing turn.
    assert_eq!(
        get(&fields, "gen_ai.agent.name").as_deref(),
        Some("FailingAgent")
    );
}
