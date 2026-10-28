//! Integration tests for building graphs from runtime data.
//!
//! A caller that only knows a graph's shape at runtime — for example, a JSON
//! plan over a registry of pre-registered systems and predicates — cannot
//! name a node's input type as a generic parameter at the call site, and its
//! node names, switch case keys, and discriminator results are owned
//! `String`s rather than `&'static str` literals. These tests build such a
//! graph through the boxed builder API (`add_loop_boxed`, `add_switch_boxed`)
//! with predicates and discriminators pulled out of a `HashMap` keyed by
//! `String`, exactly as a plan interpreter would.

mod test_utils;

use polaris_graph::executor::{ExecutionError, GraphExecutor};
use polaris_graph::graph::Graph;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::hooks::events::GraphEvent;
use polaris_graph::hooks::schedule::{OnLoopStart, OnSwitchComplete, OnSwitchStart};
use polaris_graph::predicate::{BoxedDiscriminator, BoxedPredicate};
use polaris_system::param::SystemContext;
use polaris_system::resource::LocalResource;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// The loop's termination signal.
#[derive(Debug)]
struct RuntimeLoopState {
    iteration: usize,
}

/// Seeds `RuntimeLoopState` before the loop, satisfying the loop-entry check.
struct LoopSeedSystem;

impl System for LoopSeedSystem {
    type Output = RuntimeLoopState;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(RuntimeLoopState { iteration: 0 }) })
    }

    fn name(&self) -> &'static str {
        "loop_seed_system"
    }
}

/// Increments the previous `RuntimeLoopState`, or starts at 1 if this is the
/// first iteration of a fresh execution — a pure function of the context, so
/// the same graph can be executed repeatedly without shared mutable state.
struct LoopBodySystem;

impl System for LoopBodySystem {
    type Output = RuntimeLoopState;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let iteration = ctx
                .get_output::<RuntimeLoopState>()
                .map(|prev| prev.iteration)
                .unwrap_or(0);
            Ok(RuntimeLoopState {
                iteration: iteration + 1,
            })
        })
    }

    fn name(&self) -> &'static str {
        "loop_body_system"
    }
}

/// The switch discriminator's input.
#[derive(Debug)]
struct RuntimeRouteOutput {
    action: String,
}

/// Which case the caller wants selected on this run, read from the context so
/// the same graph can be executed for multiple switch outcomes.
#[derive(Clone)]
struct RouteChoice(String);

impl LocalResource for RouteChoice {}

/// Produces `RuntimeRouteOutput` from the caller-seeded `RouteChoice`.
struct RouteSystem;

impl System for RouteSystem {
    type Output = RuntimeRouteOutput;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let choice = ctx
                .get_resource::<RouteChoice>()
                .expect("RouteChoice must be seeded by the caller");
            Ok(RuntimeRouteOutput {
                action: choice.0.clone(),
            })
        })
    }

    fn name(&self) -> &'static str {
        "route_system"
    }
}

/// A switch case's body: records which case ran as its output, so a test can
/// read back the selected case via `ExecutionResult::output`.
struct RecordSystem {
    label: String,
}

impl System for RecordSystem {
    type Output = String;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let label = self.label.clone();
        Box::pin(async move { Ok(label) })
    }

    fn name(&self) -> &'static str {
        "record_system"
    }
}

/// Builds a graph containing a loop and a switch entirely from runtime data:
/// `String` node names and case keys, with the loop's termination predicate
/// and the switch's discriminator taken out of a registry keyed by `String`
/// rather than named as generic parameters at this call site.
fn build_from_plan(
    loop_name: String,
    switch_name: String,
    case_keys: &[String],
    mut predicates: HashMap<String, BoxedPredicate>,
    mut discriminators: HashMap<String, BoxedDiscriminator>,
) -> Graph {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(LoopSeedSystem));

    let termination = predicates
        .remove("loop_done")
        .expect("the registry must carry the loop's termination predicate");
    graph.add_loop_boxed(loop_name, termination, |g| {
        g.add_boxed_system(Box::new(LoopBodySystem));
    });

    graph.add_boxed_system(Box::new(RouteSystem));

    let discriminator = discriminators
        .remove("route_by_action")
        .expect("the registry must carry the switch's discriminator");
    let cases: Vec<(String, Box<dyn FnOnce(&mut Graph)>)> = case_keys
        .iter()
        .map(|key| {
            let label = key.clone();
            (
                key.clone(),
                Box::new(move |g: &mut Graph| {
                    g.add_boxed_system(Box::new(RecordSystem { label }));
                }) as Box<dyn FnOnce(&mut Graph)>,
            )
        })
        .collect();
    graph.add_switch_boxed(switch_name, discriminator, cases, None);

    graph
}

/// A fresh registry with the loop predicate and switch discriminator this
/// plan's graph needs, keyed exactly as `build_from_plan` looks them up.
fn plan_registry() -> (
    HashMap<String, BoxedPredicate>,
    HashMap<String, BoxedDiscriminator>,
) {
    let mut predicates: HashMap<String, BoxedPredicate> = HashMap::new();
    predicates.insert(
        "loop_done".to_string(),
        BoxedPredicate::new::<RuntimeLoopState>(|state| state.iteration >= 2),
    );

    let mut discriminators: HashMap<String, BoxedDiscriminator> = HashMap::new();
    discriminators.insert(
        "route_by_action".to_string(),
        BoxedDiscriminator::new::<RuntimeRouteOutput, _>(|out| out.action.clone()),
    );

    (predicates, discriminators)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// A graph built entirely from a runtime plan — `String` names and case
/// keys, boxed predicate/discriminator pulled from a registry — validates
/// and executes correctly for at least two switch outcomes.
#[tokio::test]
async fn runtime_plan_builds_and_executes_loop_and_switch() {
    let case_keys = vec!["case_a".to_string(), "case_b".to_string()];
    let (predicates, discriminators) = plan_registry();

    let graph = build_from_plan(
        format!("loop_{}", 1),
        format!("switch_{}", 1),
        &case_keys,
        predicates,
        discriminators,
    );

    assert!(
        graph.validate().is_ok(),
        "a graph built entirely from runtime data must still validate: {:?}",
        graph.validate().errors
    );

    for chosen in &case_keys {
        let mut ctx = SystemContext::new();
        ctx.insert(RouteChoice(chosen.clone()));
        let executor = GraphExecutor::new();
        let result = executor
            .execute(&graph, &mut ctx, None, None)
            .await
            .unwrap_or_else(|err| panic!("execution should succeed for case {chosen}: {err:?}"));
        assert_eq!(
            result.output::<String>(),
            Some(chosen),
            "the case selected by the runtime discriminator must be the one that ran"
        );
    }
}

/// The signature derived from a graph built entirely from runtime data must
/// equal the signature derived from the same shape built with the typed
/// generic builders — the boxed path must not under- or over-credit outputs.
#[test]
fn runtime_plan_signature_matches_typed_equivalent() {
    let case_keys = vec!["case_a".to_string(), "case_b".to_string()];
    let (predicates, discriminators) = plan_registry();

    let runtime_graph = build_from_plan(
        "runtime_loop".to_string(),
        "runtime_switch".to_string(),
        &case_keys,
        predicates,
        discriminators,
    );

    let mut typed_graph = Graph::new();
    typed_graph.add_boxed_system(Box::new(LoopSeedSystem));
    typed_graph.add_loop::<RuntimeLoopState, _, _>(
        "typed_loop",
        |state| state.iteration >= 2,
        |g| {
            g.add_boxed_system(Box::new(LoopBodySystem));
        },
    );
    typed_graph.add_boxed_system(Box::new(RouteSystem));
    typed_graph.add_switch::<RuntimeRouteOutput, _, _, _>(
        "typed_switch",
        |out| {
            if out.action == "case_a" {
                "case_a"
            } else {
                "case_b"
            }
        },
        vec![
            (
                "case_a",
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(RecordSystem {
                        label: "case_a".to_string(),
                    }));
                }) as Box<dyn FnOnce(&mut Graph)>,
            ),
            (
                "case_b",
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(RecordSystem {
                        label: "case_b".to_string(),
                    }));
                }),
            ),
        ],
        None,
    );

    assert_eq!(
        runtime_graph.signature(),
        typed_graph.signature(),
        "a graph built entirely from runtime data must derive the same signature \
         as the same shape built with the typed generic builders"
    );
}

/// Hooks report the exact runtime-constructed names and case key — not just
/// a `&'static str` literal — proving the boxed path carries owned names all
/// the way through execution and event reporting.
#[tokio::test]
async fn runtime_plan_hooks_report_runtime_names() {
    let case_keys = vec!["case_a".to_string(), "case_b".to_string()];
    let (predicates, discriminators) = plan_registry();

    let loop_name = format!("loop_{}", 7);
    let switch_name = format!("switch_{}", 7);
    let chosen_case = "case_b".to_string();

    let graph = build_from_plan(
        loop_name.clone(),
        switch_name.clone(),
        &case_keys,
        predicates,
        discriminators,
    );

    let hooks = HooksAPI::new();
    let events: Arc<Mutex<Vec<GraphEvent>>> = Arc::new(Mutex::new(Vec::new()));

    let log = Arc::clone(&events);
    hooks
        .register_observer::<OnLoopStart, _>("capture_loop_start", move |event: &GraphEvent| {
            log.lock().unwrap().push(event.clone());
        })
        .expect("hook registration should succeed");
    let log = Arc::clone(&events);
    hooks
        .register_observer::<OnSwitchStart, _>("capture_switch_start", move |event: &GraphEvent| {
            log.lock().unwrap().push(event.clone());
        })
        .expect("hook registration should succeed");
    let log = Arc::clone(&events);
    hooks
        .register_observer::<OnSwitchComplete, _>(
            "capture_switch_complete",
            move |event: &GraphEvent| {
                log.lock().unwrap().push(event.clone());
            },
        )
        .expect("hook registration should succeed");

    let mut ctx = SystemContext::new();
    ctx.insert(RouteChoice(chosen_case.clone()));
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect("execution should succeed");

    let events = events.lock().unwrap();

    let loop_start = events
        .iter()
        .find(|e| matches!(e, GraphEvent::LoopStart { .. }))
        .expect("OnLoopStart hook should have fired");
    assert!(
        matches!(loop_start, GraphEvent::LoopStart { node_name, .. } if *node_name == loop_name),
        "the loop hook must report the runtime-constructed loop name, got {loop_start:?}"
    );

    let switch_start = events
        .iter()
        .find(|e| matches!(e, GraphEvent::SwitchStart { .. }))
        .expect("OnSwitchStart hook should have fired");
    assert!(
        matches!(switch_start, GraphEvent::SwitchStart { node_name, .. } if *node_name == switch_name),
        "the switch hook must report the runtime-constructed switch name, got {switch_start:?}"
    );

    let switch_complete = events
        .iter()
        .find(|e| matches!(e, GraphEvent::SwitchComplete { .. }))
        .expect("OnSwitchComplete hook should have fired");
    assert!(
        matches!(
            switch_complete,
            GraphEvent::SwitchComplete { selected_case, .. } if *selected_case == chosen_case
        ),
        "the switch hook must report the runtime-constructed case key that was selected, got {switch_complete:?}"
    );
}

/// A loop built through the boxed API whose predicate input is not
/// guaranteed on entry — nothing produces it earlier and nothing seeds it —
/// is still rejected by the loop-entry check, exactly as the typed `add_loop`
/// is.
#[tokio::test]
async fn loop_boxed_predicate_input_not_guaranteed_is_rejected() {
    let mut graph = Graph::new();
    let termination = BoxedPredicate::new::<RuntimeLoopState>(|state| state.iteration >= 2);
    let loop_name = format!("gate_{}", "unseeded");
    graph.add_loop_boxed(loop_name.clone(), termination, |g| {
        g.add_boxed_system(Box::new(LoopBodySystem));
    });

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let err = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a loop whose predicate input is not guaranteed on entry must be rejected");

    assert!(
        matches!(
            err,
            ExecutionError::LoopPredicateInputMissingOnEntry { ref name, .. } if *name == loop_name
        ),
        "expected LoopPredicateInputMissingOnEntry naming the runtime-constructed loop, got {err:?}"
    );
}
