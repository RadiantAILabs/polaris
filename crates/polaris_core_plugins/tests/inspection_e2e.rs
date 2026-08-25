//! End-to-end tests for the inspection recording pipeline.
//!
//! Drives real graphs through servers built with [`InspectionPlugin`] and a
//! consumer plugin that registers a listener via
//! `Extends<InspectionSinkRegistry>`: the runtime switch in both directions,
//! exact metadata and renderings per selection, narrowing, redaction (the
//! covered value's `Debug` never runs), concurrent records off parallel
//! branches, and the displacement warning for a manually installed sink.

use parking_lot::Mutex;
use polaris_core_plugins::{
    INSPECTION_TRACING_LISTENER, InspectionAPI, InspectionPlugin, InspectionPolicy,
    InspectionSinkRegistry, RedactionRules,
};
use polaris_graph::MiddlewareAPI;
use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_graph::node::ContextPolicy;
use polaris_system::param::inspect::{Inspection, InspectionSink, ParamKind, ParamMeta, Phase};
use polaris_system::param::{Out, Res, ResMut, SystemContext};
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::Registry;

/// Installs `subscriber` for this thread, then re-evaluates callsite interest.
///
/// `tracing` caches a callsite's `Interest` the first time it is hit, and the
/// cache is process-wide: if a sibling test reaches one of these callsites with
/// no subscriber installed, it is cached as `never` and a thread-local
/// subscriber installed afterwards sees nothing. Tests asserting an event
/// arrives would fail; tests asserting one is absent would pass without testing
/// anything. Rebuilding after the install re-evaluates against this subscriber.
fn set_default_and_rebuild<S>(subscriber: S) -> tracing::subscriber::DefaultGuard
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    install_dispatcher_floor();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    guard
}

/// Keeps one do-nothing dispatcher registered for the whole test binary.
///
/// Scoped subscribers come and go as tests start and finish, and `tracing`
/// recomputes callsite interest and the global max level from whichever
/// dispatchers are registered at that moment. When the last one unregisters
/// those collapse to "nothing is enabled", and a test installing its own
/// subscriber concurrently can lose that write — spans and events then go
/// missing on a thread that does have a subscriber. A floor dispatcher that
/// never unregisters keeps the count above zero so the collapse never happens.
/// It reports `Interest::sometimes` so each callsite is resolved per call
/// against the thread's current subscriber, and `enabled` is `false` so it
/// records nothing itself.
fn install_dispatcher_floor() {
    static FLOOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    // Constructing a `Dispatch` is what enters it into the registry that
    // interest and max-level are computed from; keeping it alive forever is
    // what keeps it there. Deliberately *not* `set_global_default` — that slot
    // belongs to `TracingPlugin::ready()`, which expects to win it.
    FLOOR.get_or_init(|| tracing::Dispatch::new(FloorSubscriber));
}

/// The floor dispatcher installed by [`install_dispatcher_floor`].
struct FloorSubscriber;

impl tracing::Subscriber for FloorSubscriber {
    fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

// ─────────────────────────────────────────────────────────────────────────────
// The listener and the consumer plugin that registers it
// ─────────────────────────────────────────────────────────────────────────────

/// Listener handle that stores every record it receives.
#[derive(Clone, Default)]
struct CollectingListener(Arc<Mutex<Vec<(ParamMeta, Inspection)>>>);

impl CollectingListener {
    fn records(&self) -> Vec<(ParamMeta, Inspection)> {
        self.0.lock().clone()
    }

    fn find(&self, system: &str, param: &str) -> Option<(ParamMeta, Inspection)> {
        self.records()
            .into_iter()
            .find(|(meta, _)| meta.system == system && meta.param == param)
    }

    fn clear(&self) {
        self.0.lock().clear();
    }
}

impl InspectionSink for CollectingListener {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        self.0.lock().push((meta, render()));
    }
}

/// A consumer plugin signing its listener up during `build()`.
struct ListenerPlugin(CollectingListener);

#[plugin(id = "test::listener", version = "0.1.0")]
impl Plugin for ListenerPlugin {
    fn build(&self, mut registry: Extends<InspectionSinkRegistry>) {
        registry.push(self.0.clone());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The two-step agent graph
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Memory {
    entries: Vec<String>,
}

impl LocalResource for Memory {}

#[derive(Debug)]
struct Config {
    multiplier: i32,
}

impl LocalResource for Config {}

#[derive(Debug)]
struct Payload {
    value: i32,
}

#[system(inspect(memory, return))]
async fn plan_step(mut memory: ResMut<Memory>) -> Payload {
    memory.entries.push("planned".to_owned());
    Payload { value: 6 }
}

#[system(inspect(payload, config))]
async fn act_step(payload: Out<Payload>, config: Res<Config>) -> i32 {
    payload.value * config.multiplier
}

fn two_step_graph() -> Graph {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(plan_step()));
    graph.add_boxed_system(Box::new(act_step()));
    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );
    graph
}

async fn run_graph(server: &Server) {
    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut ctx = server.create_context();
    ctx.insert(Memory {
        entries: vec!["seed".to_owned()],
    });
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&two_step_graph(), &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");
}

// ─────────────────────────────────────────────────────────────────────────────
// The test
// ─────────────────────────────────────────────────────────────────────────────

fn text_of(inspection: &Inspection) -> &str {
    match inspection {
        Inspection::Text { value, .. } => value,
        other => panic!("expected a rendered value, got {other:?}"),
    }
}

/// A server with [`InspectionPlugin`] (tracing sink opted out) and one
/// registered collecting listener — the shared fixture of the single-invariant
/// tests below.
async fn server_with_listener(listener: &CollectingListener) -> Server {
    let mut server = Server::new();
    server.add_plugins(InspectionPlugin::default().without_tracing_sink());
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");
    server
}

#[tokio::test]
async fn recording_is_off_until_explicitly_enabled() {
    let listener = CollectingListener::default();
    let server = server_with_listener(&listener).await;

    run_graph(&server).await;

    assert!(
        listener.records().is_empty(),
        "recording must be off until explicitly enabled"
    );
}

#[tokio::test]
async fn an_enabled_run_records_metadata_and_exact_renderings_per_selection() {
    let listener = CollectingListener::default();
    let server = server_with_listener(&listener).await;

    server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI")
        .enable();
    run_graph(&server).await;

    let records = listener.records();
    assert_eq!(records.len(), 4, "two selections per step, got {records:?}");

    let (memory_meta, memory_value) = listener
        .find("plan_step", "memory")
        .expect("plan_step's memory parameter must be recorded");
    assert_eq!(memory_meta.kind, ParamKind::ResMut);
    assert_eq!(memory_meta.phase, Phase::Before);
    assert_eq!(memory_meta.type_name, "Memory");
    assert_eq!(
        text_of(&memory_value),
        r#"Memory { entries: ["seed"] }"#,
        "the pre-body memory value must render exactly"
    );

    let (return_meta, return_value) = listener
        .find("plan_step", "return")
        .expect("plan_step's return value must be recorded");
    assert_eq!(return_meta.kind, ParamKind::Return);
    assert_eq!(return_meta.phase, Phase::After);
    assert_eq!(return_meta.type_name, "Payload");
    assert_eq!(text_of(&return_value), "Payload { value: 6 }");

    let (payload_meta, payload_value) = listener
        .find("act_step", "payload")
        .expect("act_step's payload parameter must be recorded");
    assert_eq!(payload_meta.kind, ParamKind::Out);
    // `Before`, not `After`: `act_step` *reads* the output channel plan_step
    // wrote, so this parameter is an input to the body like any other. Only a
    // `Return` selection is captured after it.
    assert_eq!(payload_meta.phase, Phase::Before);
    assert_eq!(payload_meta.type_name, "Payload");
    assert_eq!(text_of(&payload_value), "Payload { value: 6 }");

    let (config_meta, config_value) = listener
        .find("act_step", "config")
        .expect("act_step's config parameter must be recorded");
    assert_eq!(config_meta.kind, ParamKind::Res);
    assert_eq!(config_meta.phase, Phase::Before);
    assert_eq!(config_meta.type_name, "Config");
    assert_eq!(text_of(&config_value), "Config { multiplier: 7 }");
}

#[tokio::test]
async fn narrowing_at_runtime_delivers_only_the_named_step() {
    let listener = CollectingListener::default();
    let server = server_with_listener(&listener).await;

    server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI")
        .enable_only(["act_step"]);
    run_graph(&server).await;

    let systems: Vec<&'static str> = listener
        .records()
        .iter()
        .map(|(meta, _)| meta.system)
        .collect();
    assert_eq!(
        systems,
        vec!["act_step", "act_step"],
        "narrowing must exclude the other step"
    );
}

#[tokio::test]
async fn disabling_at_runtime_stops_delivery_again() {
    let listener = CollectingListener::default();
    let server = server_with_listener(&listener).await;
    let inspection = server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI");

    inspection.enable();
    run_graph(&server).await;
    assert!(
        !listener.records().is_empty(),
        "sanity: the enabled window must deliver"
    );

    listener.clear();
    inspection.disable();
    run_graph(&server).await;
    assert!(
        listener.records().is_empty(),
        "the switch must work both ways at run time"
    );
}

#[tokio::test]
async fn multiple_listeners_each_receive_every_recording() {
    let first = CollectingListener::default();
    let second = CollectingListener::default();

    let mut server = Server::new();
    server.add_plugins(InspectionPlugin::default().without_tracing_sink());
    server.add_plugins(ListenerPlugin(first.clone()));
    server.add_plugins(SecondListenerPlugin(second.clone()));
    server.finish().await.expect("server must build");

    server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI")
        .enable();
    run_graph(&server).await;

    let first_records = first.records();
    assert_eq!(first_records.len(), 4);
    assert_eq!(
        first_records,
        second.records(),
        "independently registered listeners must see the same complete records, \
         metadata and rendered values alike"
    );
}

#[tokio::test]
async fn a_listener_registered_before_the_provider_still_receives_records() {
    let listener = CollectingListener::default();

    // The consumer is registered *before* InspectionPlugin: the capability
    // resolver must order its build() after the registry's provider.
    let mut server = Server::new();
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.add_plugins(InspectionPlugin::default().without_tracing_sink());
    server
        .finish()
        .await
        .expect("the resolver must order the listener after its provider");

    server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI")
        .enable();
    run_graph(&server).await;

    assert_eq!(
        listener.records().len(),
        4,
        "registration order must not affect delivery"
    );
}

#[tokio::test]
async fn a_non_default_initial_policy_delivers_from_the_first_run() {
    let listener = CollectingListener::default();

    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::systems(["plan_step"]))
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    // No InspectionAPI interaction: the initial policy alone admits records.
    run_graph(&server).await;

    let systems: Vec<&'static str> = listener
        .records()
        .iter()
        .map(|(meta, _)| meta.system)
        .collect();
    assert_eq!(
        systems,
        vec!["plan_step", "plan_step"],
        "the initial policy must narrow delivery from the very first run"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Sink inheritance through child contexts: branches, scopes, loop iterations
// ─────────────────────────────────────────────────────────────────────────────

#[system(inspect(config))]
async fn branch_step(config: Res<Config>) -> i32 {
    config.multiplier
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_branch_children_inherit_the_run_root_sink() {
    // What this pins: the middleware installs one sink on the run's *root*
    // context and each parallel branch's child context inherits it, so all eight
    // branches deliver. It does not pin the fan-out's lock-free invariant —
    // branches are polled cooperatively on a single task, so their records never
    // actually overlap. That invariant is pinned by
    // `concurrent_records_invoke_listeners_without_holding_the_fanout_locks` in
    // the crate's unit tests, which drives two real threads into one fan-out.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut graph = Graph::new();
    graph.add_parallel(
        "fan",
        (0..8).map(|_| {
            |branch: &mut Graph| {
                branch.add_boxed_system(Box::new(branch_step()));
            }
        }),
    );

    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );

    let mut ctx = server.create_context();
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");

    let records = listener.records();
    assert_eq!(
        records.len(),
        8,
        "every branch must deliver its record — got {records:?}"
    );
    for (meta, value) in &records {
        assert_eq!(meta.system, "branch_step");
        assert_eq!(text_of(value), "Config { multiplier: 7 }");
    }
}

#[system(inspect(config))]
async fn scoped_step(config: Res<Config>) -> i32 {
    config.multiplier
}

#[system(inspect(config))]
async fn looped_step(config: Res<Config>) -> i32 {
    config.multiplier
}

#[tokio::test]
async fn scope_and_loop_children_inherit_the_run_root_sink() {
    // The plugin claims every system in a run inherits the sink, "including
    // scopes, branches, and loop iterations". A scope crosses a context
    // boundary and a loop re-enters one, and a regression there is silent —
    // recording simply stops inside them — so both are pinned here.
    //
    // One `ContextPolicy` mode is enough: `child()` and `child_filtered()` both
    // clone the sink handle, and the executor creates children only through
    // those two, so the other modes cannot differ on this axis.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut scoped = Graph::new();
    scoped.add_boxed_system(Box::new(scoped_step()));
    let mut graph = Graph::new();
    graph.add_scope("scoped", scoped, ContextPolicy::new().share_rest());
    graph.add_loop_n("twice", 2, |body| {
        body.add_boxed_system(Box::new(looped_step()));
    });

    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );

    let mut ctx = server.create_context();
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");

    let records = listener.records();
    let count = |system: &str| {
        records
            .iter()
            .filter(|(meta, _)| meta.system == system)
            .count()
    };
    assert_eq!(
        count("scoped_step"),
        1,
        "a scope's child context must inherit the sink — got {records:?}"
    );
    assert_eq!(
        count("looped_step"),
        2,
        "every loop iteration must keep recording — got {records:?}"
    );
    for (_, value) in &records {
        assert_eq!(text_of(value), "Config { multiplier: 7 }");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Redaction through a real run
// ─────────────────────────────────────────────────────────────────────────────

/// A resource whose `Debug` would expose a secret, and counts its own renders.
///
/// The counter rides on the instance rather than living in a `static`: a
/// process-wide one is correct only while exactly one test ever constructs a
/// `Vault`, which is a constraint no compiler enforces and the next test to
/// need one would silently break.
struct Vault {
    key: String,
    renders: Arc<AtomicUsize>,
}

impl LocalResource for Vault {}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.renders.fetch_add(1, Ordering::SeqCst);
        f.debug_struct("Vault").field("key", &self.key).finish()
    }
}

#[system(inspect(vault))]
async fn open_vault(vault: Res<Vault>) {
    let _ = &vault.key;
}

#[tokio::test]
async fn a_redacted_value_arrives_as_redacted_and_its_debug_never_runs() {
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .with_redactions(RedactionRules::new().redact_type("Vault"))
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(open_vault()));
    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );

    let renders = Arc::new(AtomicUsize::new(0));
    let mut ctx = server.create_context();
    ctx.insert(Vault {
        key: "hunter2".to_owned(),
        renders: Arc::clone(&renders),
    });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");

    let (meta, value) = listener
        .find("open_vault", "vault")
        .expect("a covered record must still be delivered");
    assert_eq!(meta.type_name, "Vault");
    assert_eq!(
        value,
        Inspection::Redacted,
        "a covered value must arrive as the redaction sentinel"
    );
    assert_eq!(
        renders.load(Ordering::SeqCst),
        0,
        "the covered value's Debug must never run — the secret must not reach a String"
    );
}

/// A second, independently registered consumer plugin.
struct SecondListenerPlugin(CollectingListener);

#[plugin(id = "test::listener2", version = "0.1.0")]
impl Plugin for SecondListenerPlugin {
    fn build(&self, mut registry: Extends<InspectionSinkRegistry>) {
        registry.push(self.0.clone());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Displacing a manually installed Layer-1 sink
// ─────────────────────────────────────────────────────────────────────────────

/// Pulls the `message` field off an event.
#[derive(Default)]
struct MessageVisitor(Option<String>);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

/// Collects the warnings the plugin emits on its own target.
#[derive(Clone, Default)]
struct WarningCapture(Arc<Mutex<Vec<String>>>);

impl WarningCapture {
    fn messages(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarningCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != "polaris::inspection" || *metadata.level() != tracing::Level::WARN {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.0 {
            self.0.lock().push(message);
        }
    }
}

/// Runs the graph against a context that already carries `sink`.
async fn run_graph_with_sink(server: &Server, sink: Arc<dyn InspectionSink>) {
    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut ctx = server.create_context().with_inspection(sink);
    ctx.insert(Memory {
        entries: vec!["seed".to_owned()],
    });
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&two_step_graph(), &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");
}

#[tokio::test]
async fn displacing_a_manually_installed_sink_warns_once() {
    // Thread-local subscriber (a global install would clash with the other
    // tests sharing this binary), plus an interest rebuild so a sibling test
    // that reached this callsite unsubscribed cannot have disabled it. `#[tokio::test]` keeps the future on this
    // thread, so the middleware's warning lands in this capture.
    let capture = WarningCapture::default();
    let _guard = set_default_and_rebuild(Registry::default().with(capture.clone()));

    let registered = CollectingListener::default();
    let manual = CollectingListener::default();

    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(registered.clone()));
    server.finish().await.expect("server must build");

    // Two runs, each handing the executor a context that already has a sink.
    run_graph_with_sink(&server, Arc::new(manual.clone())).await;
    run_graph_with_sink(&server, Arc::new(manual.clone())).await;

    assert!(
        manual.records().is_empty(),
        "the plugin's fan-out replaces a manually installed sink, so it must \
         receive nothing — got {:?}",
        manual.records()
    );
    assert!(
        !registered.records().is_empty(),
        "the replacement fan-out must still deliver to registered listeners"
    );

    let warnings = capture.messages();
    assert_eq!(
        warnings.len(),
        1,
        "displacement must warn once, not once per run — got {warnings:?}"
    );
    assert!(
        warnings[0].contains("replace_inspection"),
        "the warning must name the call being overridden, got {:?}",
        warnings[0]
    );
}

#[tokio::test]
async fn reusing_one_context_across_runs_is_not_a_displacement() {
    // Thread-local subscriber (a global install would clash with the other
    // tests sharing this binary), plus an interest rebuild so a sibling test
    // that reached this callsite unsubscribed cannot have disabled it.
    let capture = WarningCapture::default();
    let _guard = set_default_and_rebuild(Registry::default().with(capture.clone()));

    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut ctx = server.create_context();
    ctx.insert(Memory {
        entries: vec!["seed".to_owned()],
    });
    ctx.insert(Config { multiplier: 7 });

    // Two turns against one context — the shape `SessionsAPI` executes every
    // turn in. The first run leaves the fan-out on the context; the second must
    // recognize its own sink rather than report a displacement nobody caused
    // (and so spend the one-shot warning meant for a real one).
    for _ in 0..2 {
        GraphExecutor::new()
            .execute(&two_step_graph(), &mut ctx, None, Some(middleware))
            .await
            .expect("graph execution should succeed");
    }

    assert!(
        capture.messages().is_empty(),
        "a context already carrying the plugin's own sink is not a displacement \
         — got {:?}",
        capture.messages()
    );
    assert_eq!(
        listener.records().len(),
        8,
        "both turns must record — got {:?}",
        listener.records()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The shipped tracing listener, through a plugin-built server
// ─────────────────────────────────────────────────────────────────────────────

/// Pulls `polaris.inspection.system` off a value event.
#[derive(Default)]
struct SystemFieldVisitor(Option<String>);

impl Visit for SystemFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "polaris.inspection.system" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn core::fmt::Debug) {}
}

/// Collects the system named by every exported value event — what the shipped
/// tracing listener, and only it, puts on the subscriber.
#[derive(Clone, Default)]
struct ExportCapture(Arc<Mutex<Vec<String>>>);

impl ExportCapture {
    fn exported(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ExportCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != "polaris::inspection" || *metadata.level() != tracing::Level::INFO {
            return;
        }
        let mut visitor = SystemFieldVisitor::default();
        event.record(&mut visitor);
        if let Some(system) = visitor.0 {
            self.0.lock().push(system);
        }
    }
}

#[tokio::test]
async fn without_tracing_sink_leaves_the_shipped_listener_unregistered() {
    // Thread-local subscriber (a global install would clash with the other
    // tests sharing this binary), plus an interest rebuild so a sibling test
    // that reached this callsite unsubscribed cannot have disabled it.
    let capture = ExportCapture::default();
    let _guard = set_default_and_rebuild(Registry::default().with(capture.clone()));

    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    run_graph(&server).await;

    assert_eq!(
        listener.records().len(),
        4,
        "sanity: recording is on, so an empty export is the opt-out working \
         rather than nothing happening"
    );
    assert!(
        capture.exported().is_empty(),
        "without_tracing_sink must leave the shipped listener unregistered — \
         exported {:?}",
        capture.exported()
    );
}

#[tokio::test]
async fn the_shipped_listener_exports_and_toggles_under_its_public_name() {
    // Thread-local subscriber (a global install would clash with the other
    // tests sharing this binary), plus an interest rebuild so a sibling test
    // that reached this callsite unsubscribed cannot have disabled it.
    let capture = ExportCapture::default();
    let _guard = set_default_and_rebuild(Registry::default().with(capture.clone()));

    // The default plugin — shipped tracing listener registered, under whatever
    // name `build()` chose. `INSPECTION_TRACING_LISTENER` is the documented handle for it,
    // so the toggle below is what pins the two together.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(InspectionPlugin::new().with_policy(InspectionPolicy::All));
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");
    let inspection = server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI");

    // Assert the exported *names*, not just how many arrived: a count cannot
    // tell "every admitted record was exported" from "some other record was
    // exported four times". Sequential graph, so the order is deterministic —
    // both of plan_step's selections, then both of act_step's.
    let one_run = ["plan_step", "plan_step", "act_step", "act_step"];
    run_graph(&server).await;
    assert_eq!(
        capture.exported(),
        one_run,
        "the shipped listener must export every admitted record, and only those"
    );

    assert_eq!(
        inspection.disable_listener(INSPECTION_TRACING_LISTENER),
        Some(true),
        "the shipped listener must be discoverable under its public name"
    );
    run_graph(&server).await;
    assert_eq!(
        capture.exported(),
        one_run,
        "disable_listener(INSPECTION_TRACING_LISTENER) must stop export from a \
         plugin-built server"
    );
    assert_eq!(
        listener.records().len(),
        8,
        "other listeners must keep receiving while export is off"
    );

    assert_eq!(
        inspection.enable_listener(INSPECTION_TRACING_LISTENER),
        Some(false),
        "the shipped listener must remain registered while disabled"
    );
    run_graph(&server).await;
    assert_eq!(
        capture.exported(),
        [one_run, one_run].concat(),
        "re-enabling the name must restore export"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The misconfiguration, and the boundary of what a rule can withhold
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_listener_without_its_provider_fails_the_build() {
    // The natural misconfiguration: a plugin signs up for the registry on a
    // server that never added the plugin providing it. The capability resolver
    // must reject that at `finish()` rather than silently never delivering.
    let mut server = Server::new();
    server.add_plugins(ListenerPlugin(CollectingListener::default()));

    let failure = server
        .finish()
        .await
        .expect_err("a consumer with no provider must not build");

    let message = failure.to_string();
    assert!(
        message.contains("InspectionSinkRegistry"),
        "the error must name the capability that went unsatisfied — got {message}"
    );
}

/// A credential reached only through an enclosing type's `Debug`.
#[derive(Debug)]
struct ApiCredentials {
    key: String,
}

#[derive(Debug)]
struct AppConfig {
    credentials: ApiCredentials,
}

impl LocalResource for AppConfig {}

#[system(inspect(config))]
async fn read_config(config: Res<AppConfig>) {
    let _ = &config.credentials.key;
}

#[tokio::test]
async fn a_rule_cannot_withhold_a_credential_nested_inside_a_recorded_value() {
    // Pins the documented boundary rather than a desired behavior: matching is
    // on the record's metadata, so a rule naming a type that no parameter
    // *declares* is inert even when an enclosing value's `Debug` prints it.
    // If redaction ever learns to descend into values, this test should fail
    // and be rewritten — that is the point of having it.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .with_redactions(RedactionRules::new().redact_type("ApiCredentials"))
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(read_config()));
    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );

    let mut ctx = server.create_context();
    ctx.insert(AppConfig {
        credentials: ApiCredentials {
            key: "sk-nested".to_owned(),
        },
    });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");

    let (meta, value) = listener
        .find("read_config", "config")
        .expect("the config parameter must be recorded");
    assert_eq!(meta.type_name, "AppConfig");
    let Inspection::Text { value, .. } = value else {
        panic!("a rule on a type no parameter declares must not withhold: {value:?}");
    };
    assert!(
        value.contains("sk-nested"),
        "the nested credential is rendered today — the controls are non-selection \
         or a masking Debug, not a redaction rule — got {value}"
    );
}

#[tokio::test]
async fn a_run_outside_the_servers_middleware_records_nothing() {
    // The plugin installs its fan-out *as middleware*, so a run executed
    // without it captures nothing however permissive the policy is. Documented
    // as a limitation; pinned here so it stays a known one.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");

    let mut ctx = server.create_context();
    ctx.insert(Memory {
        entries: vec!["seed".to_owned()],
    });
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&two_step_graph(), &mut ctx, None, None)
        .await
        .expect("graph execution should succeed");

    assert!(
        listener.records().is_empty(),
        "a run that bypasses the server's MiddlewareAPI installs no sink, so \
         nothing can be captured — got {:?}",
        listener.records().len()
    );
}

/// A system written by hand rather than through `#[system]`.
///
/// Reads the same `Config` that `act_step` has selected for inspection, so the
/// only difference from a recorded system is where the code came from.
struct HandWrittenSystem;

impl System for HandWrittenSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let config = ctx
                .get_resource::<Config>()
                .expect("Config must be in the context");
            let _ = config.multiplier;
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "hand_written_system"
    }
}

#[tokio::test]
async fn a_hand_written_system_records_nothing_even_wide_open() {
    // Capture is emitted by the `#[system]` macro from its `inspect(..)` list.
    // A hand-written `impl System` has no such list and calls nothing that
    // would record, so it stays invisible however permissive the policy is.
    // Documented as a limitation on InspectionPlugin; pinned here so it stays a
    // known one rather than turning into a silent hole.
    let listener = CollectingListener::default();
    let mut server = Server::new();
    server.add_plugins(
        InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .without_tracing_sink(),
    );
    server.add_plugins(ListenerPlugin(listener.clone()));
    server.finish().await.expect("server must build");
    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("InspectionPlugin must provide MiddlewareAPI");

    // Both kinds of system in one graph, under one policy and one listener.
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(HandWrittenSystem));
    graph.add_boxed_system(Box::new(plan_step()));
    assert!(
        graph.validate().is_ok(),
        "the graph under test must be well-formed"
    );

    let mut ctx = server.create_context();
    ctx.insert(Memory {
        entries: vec!["seed".to_owned()],
    });
    ctx.insert(Config { multiplier: 7 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(middleware))
        .await
        .expect("graph execution should succeed");

    assert!(
        listener
            .records()
            .iter()
            .all(|(meta, _)| meta.system != "hand_written_system"),
        "a hand-written system has no inspect(..) list and must record nothing \
         — got {:?}",
        listener.records()
    );
    // Control: the macro-written system in the same run did record, so the
    // absence above is a property of how the system was written — not of the
    // policy, the listener, or the middleware being off.
    assert!(
        listener.find("plan_step", "memory").is_some(),
        "control: the macro-written step in the same graph must still record"
    );
}

#[tokio::test]
async fn listeners_survive_cleanup_because_nothing_removes_them() {
    // `InspectionSinkRegistry` documents that listeners are never removed and
    // that a name plus `disable_listener` is the substitute. That is a claim
    // about teardown as much as about run time, and the plugin implements no
    // `cleanup()` — so pin that the contract holds from the far side rather
    // than only asserting it on a server nobody has torn down.
    let listener = CollectingListener::default();
    let mut server = server_with_listener(&listener).await;
    // Cloned, not borrowed: `cleanup()` takes `&mut server`, and outliving that
    // is the point — the handle an operator kept must still work afterwards.
    let inspection = server
        .api::<InspectionAPI>()
        .expect("InspectionPlugin must provide InspectionAPI")
        .clone();
    inspection.enable();

    run_graph(&server).await;
    assert_eq!(
        listener.records().len(),
        4,
        "baseline: the listener receives before cleanup"
    );
    listener.clear();

    server.cleanup().await;

    // Cleanup runs plugin teardown; it does not tear down the server, so the
    // registry and its listeners are still live and still wired to the policy.
    run_graph(&server).await;
    assert_eq!(
        listener.records().len(),
        4,
        "a listener is never removed, so cleanup must not silently unsubscribe \
         it — got {:?}",
        listener.records()
    );
    assert_eq!(
        inspection.policy(),
        InspectionPolicy::All,
        "cleanup must not reset the runtime policy either"
    );
}
