//! Integration tests for [`Dynamic`](polaris_graph::node::Node::Dynamic) nodes:
//! inline selection, default fallback, missing-candidate errors, runtime registry
//! swaps, slot-contract validation, registry/slot contract mismatch, recursion
//! bounding, candidate-level timeout, hook/middleware observation, downstream
//! output propagation, `duplicate`, error/timeout-handler interface soundness,
//! duplicate inline keys, candidate-interior resource validation, boundary
//! crossing failures, parent error-handler bypass, and boxed (fallible)
//! selectors.

use polaris_graph::executor::{ExecutionError, GraphExecutor, ResourceValidationError};
use polaris_graph::graph::{Graph, GraphSignature, ValidationError};
use polaris_graph::hooks::HooksAPI;
use polaris_graph::hooks::events::GraphEvent;
use polaris_graph::hooks::schedule::{OnDynamicComplete, OnDynamicStart};
use polaris_graph::middleware::MiddlewareAPI;
use polaris_graph::middleware::info::DynamicInfo;
use polaris_graph::node::{ContextMode, ContextPolicy, DynamicSlot};
use polaris_graph::registry::{RegistryError, SubgraphRegistry};
use polaris_system::param::{AccessMode, SystemAccess, SystemContext};
use polaris_system::resource::{ForkStrategy, LocalResource};
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────
//
// These are bespoke rather than reused from `tests/test_utils.rs` on purpose:
// a dynamic candidate must satisfy a `produce::<i32>()` slot contract, but the
// shared helpers (`LoggingSystem`, `SlowSystem`, `ConsumerSystem`, …) all return
// `()`, which signature derivation filters out — so they can never sit behind a
// non-empty contract. The `Multiply`/`recorder` pattern also proves *which*
// candidate ran by the distinct product it records (e.g. `vec![300]` vs
// `vec![40]`), which an `ExecutionLog` of node ids cannot express as directly.

/// A shared input resource the candidates read across the scope boundary.
/// `Clone` so a non-`Shared` policy can [`forward`](ContextPolicy::forward) a
/// copy into the candidate's child context.
#[derive(Clone)]
struct Base {
    n: i32,
}
impl LocalResource for Base {}

/// Drives the selector: names the candidate key to run.
struct Choice {
    pick: &'static str,
}
impl LocalResource for Choice {}

/// A candidate system: reads [`Base`], records and returns `base.n * factor`.
struct Multiply {
    factor: i32,
    recorder: Arc<Mutex<Vec<i32>>>,
}

impl System for Multiply {
    type Output = i32;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let factor = self.factor;
        let recorder = Arc::clone(&self.recorder);
        Box::pin(async move {
            let base = ctx
                .get_resource::<Base>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            let value = base.n * factor;
            recorder.lock().unwrap().push(value);
            Ok(value)
        })
    }

    fn name(&self) -> &'static str {
        "multiply"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.add_read::<Base>();
        access
    }
}

/// A candidate system that produces an `i32` reading nothing — used to build a
/// candidate compatible with a contract that has no `requires` (so it can sit
/// in a registry whose contract diverges from a `Base`-reading slot).
struct Constant {
    value: i32,
}

impl System for Constant {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let value = self.value;
        Box::pin(async move { Ok(value) })
    }

    fn name(&self) -> &'static str {
        "constant"
    }
}

/// A candidate system that sleeps before returning — used to exercise the
/// candidate-level `max_duration` timeout enforced at the dynamic boundary.
struct Slow {
    delay: Duration,
}

impl System for Slow {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(0)
        })
    }

    fn name(&self) -> &'static str {
        "slow"
    }
}

/// A candidate system that always fails at runtime — used to prove a selected
/// candidate's own `SystemError` surfaces through the dynamic boundary rather
/// than being swallowed or re-attributed to the dynamic node.
struct FailingCandidate;

impl System for FailingCandidate {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async { Err(SystemError::ExecutionError("candidate blew up".into())) })
    }

    fn name(&self) -> &'static str {
        "failing_candidate"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

/// A [`ForkStrategy`] resource whose `fork` yields a fresh-empty ledger, so a
/// forked child observes an empty log where a `forward` (clone) child would see
/// the parent's entries — making the two boundary verbs distinguishable.
#[derive(Default)]
struct Ledger {
    entries: Vec<i32>,
}
impl LocalResource for Ledger {}
impl ForkStrategy for Ledger {
    fn fork(&self) -> Self {
        Ledger::default()
    }
}

/// A candidate system reading [`Ledger`]: records and returns the entry count it
/// observes, so a test can prove which side of a fork boundary it ran on.
struct CountLedger {
    recorder: Arc<Mutex<Vec<i32>>>,
}

impl System for CountLedger {
    type Output = i32;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let recorder = Arc::clone(&self.recorder);
        Box::pin(async move {
            let ledger = ctx
                .get_resource::<Ledger>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            let count = ledger.entries.len() as i32;
            recorder.lock().unwrap().push(count);
            Ok(count)
        })
    }

    fn name(&self) -> &'static str {
        "count_ledger"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.add_read::<Ledger>();
        access
    }
}

/// Builds a single-system candidate graph for `factor`.
fn candidate(factor: i32, recorder: &Arc<Mutex<Vec<i32>>>) -> Graph {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(Multiply {
        factor,
        recorder: Arc::clone(recorder),
    }));
    graph
}

/// A candidate that nests a `Scope` boundary — flat signature derivation cannot
/// see across it, so it must be rejected as a dynamic-node candidate.
fn candidate_nesting_a_scope(recorder: &Arc<Mutex<Vec<i32>>>) -> Graph {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(Multiply {
        factor: 10,
        recorder: Arc::clone(recorder),
    }));
    let mut candidate = Graph::new();
    candidate.add_scope("inner", inner, ContextPolicy::shared());
    candidate
}

/// A candidate that nests a `Dynamic` boundary — likewise rejected.
fn candidate_nesting_a_dynamic() -> Graph {
    let mut candidate = Graph::new();
    candidate.add_dynamic_registry(
        "inner",
        |_ctx| Arc::from("x"),
        DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
    );
    candidate
}

/// The slot contract every candidate must honor: reads `Base`, produces `i32`.
fn contract() -> GraphSignature {
    GraphSignature::new()
        .require_read::<Base>()
        .produce::<i32>()
}

/// Selector reading the `Choice` resource; falls back to `"a"` if absent.
fn pick_choice(ctx: &SystemContext<'_>) -> Arc<str> {
    match ctx.get_resource::<Choice>() {
        Ok(choice) => Arc::from(choice.pick),
        Err(_) => Arc::from("a"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inline selection
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inline_selects_candidate_by_key_under_share() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::new().share::<Base>()).with_default_key("a"),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new()
        .with(Base { n: 3 })
        .with(Choice { pick: "b" });
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("inline dynamic execution should succeed");

    // Candidate "b" (factor 100) ran against Base { n: 3 }, and its output
    // merged back across the (non-shared) `share::<Base>()` boundary into the
    // parent context — surfacing as the graph's final output. Without the
    // merge-back the parent would have produced no output at all.
    assert_eq!(*recorder.lock().unwrap(), vec![300]);
    assert_eq!(result.output::<i32>(), Some(&300));
}

#[tokio::test]
async fn inline_selects_other_candidate_under_shared_policy() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );

    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new()
        .with(Base { n: 4 })
        .with(Choice { pick: "a" });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("inline dynamic execution should succeed");

    assert_eq!(*recorder.lock().unwrap(), vec![40]);
}

#[tokio::test]
async fn forward_policy_copies_resource_into_dynamic_candidate() {
    // Every other test runs the candidate under a `Shared` policy (parent context
    // reused directly). This exercises a non-`Shared` policy through the dynamic
    // boundary: `forward::<Base>` clones the parent's `Base` into an isolated
    // child context, so the candidate runs against the copy via
    // `populate_child_locals` rather than the shared parent.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("a"),
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::new().forward::<Base>()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 6 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("forwarded dynamic execution should succeed");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![60],
        "candidate ran against the forwarded Base copy"
    );
}

#[tokio::test]
async fn falls_back_to_default_when_key_absent() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("missing"),
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );

    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 2 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("inline dynamic execution should succeed");

    // Selector's key was absent, so the default "a" (factor 10) ran.
    assert_eq!(*recorder.lock().unwrap(), vec![20]);
}

#[tokio::test]
async fn slot_default_key_accepts_a_runtime_string() {
    // The default key is a runtime `String`, not a `&'static str` — the key
    // asymmetry the reified slot removes (candidate keys were always runtime;
    // the default now is too). The computed key resolves and runs.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let runtime_default: String = format!("fall{}", "back");
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("missing"),
        [("fallback", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key(runtime_default),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 2 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("inline dynamic execution should succeed");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![20],
        "the runtime-string default key resolved and ran"
    );
}

#[tokio::test]
async fn missing_candidate_without_default_errors() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("nope"),
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("missing candidate with no default should error");

    assert!(
        matches!(err, ExecutionError::DynamicCandidateNotFound { .. }),
        "expected DynamicCandidateNotFound, got {err:?}"
    );
    assert!(
        recorder.lock().unwrap().is_empty(),
        "no candidate may run after the failed lookup, got {:?}",
        recorder.lock().unwrap()
    );
}

#[tokio::test]
async fn inline_default_key_also_absent_errors_on_the_default_key() {
    // Selector's key is absent *and* the configured default is also absent from
    // the inline set: the fallback lookup fails too, and the error names the
    // default key (not the selector's) — pinning the second-lookup branch.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("missing-selector-key"),
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared())
            .with_default_key("missing-default-key"),
    );

    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("neither the selector key nor the default is present");

    match err {
        ExecutionError::DynamicCandidateNotFound { key, .. } => assert_eq!(
            &*key, "missing-default-key",
            "the error must name the unresolved default key, not the selector's"
        ),
        other => panic!("expected DynamicCandidateNotFound, got {other:?}"),
    }
    assert!(
        recorder.lock().unwrap().is_empty(),
        "no candidate may run after the failed lookup, got {:?}",
        recorder.lock().unwrap()
    );
}

#[tokio::test]
async fn candidate_runtime_error_propagates_through_the_boundary() {
    // A selected candidate whose inner system fails at runtime surfaces its own
    // `SystemError` through the dynamic boundary — the error is neither swallowed
    // nor re-attributed to the dynamic node.
    let mut failing = Graph::new();
    failing.add_boxed_system(Box::new(FailingCandidate));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("boom"),
        [("boom", failing)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new();
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a failing candidate must surface its error");

    match err {
        ExecutionError::SystemError(message) => assert!(
            message.contains("candidate blew up"),
            "the candidate's own message should surface, got {message:?}"
        ),
        other => panic!("expected SystemError from the candidate, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Registry source (runtime swap)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_candidate_can_be_swapped_between_runs() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    // Seed the registry with candidate A behind "slot".
    let mut registry = SubgraphRegistry::new(contract());
    registry.register("slot", candidate(10, &recorder)).unwrap();

    let mut ctx = SystemContext::new().with(Base { n: 5 }).with(registry);
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry-backed dynamic execution should succeed");
    assert_eq!(*recorder.lock().unwrap(), vec![50]);

    // Swap "slot" to candidate B and run again — same slot, different graph.
    {
        let mut registry = ctx.get_resource_mut::<SubgraphRegistry>().unwrap();
        registry.remove("slot");
        registry
            .register("slot", candidate(100, &recorder))
            .unwrap();
    }
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry-backed dynamic execution should succeed after swap");
    assert_eq!(*recorder.lock().unwrap(), vec![50, 500]);
}

#[test]
fn registry_rejects_incompatible_candidate() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    // Contract promises a `String` output; the i32-producing, Base-reading
    // candidate cannot satisfy it, so `register` must reject it and leave the
    // slot empty.
    let mut registry = SubgraphRegistry::new(GraphSignature::new().produce::<String>());
    let err = registry
        .register("slot", candidate(10, &recorder))
        .unwrap_err();

    assert!(matches!(err, RegistryError::Incompatible { .. }));
    assert!(!registry.contains("slot"));
}

#[test]
fn registry_rejects_structurally_invalid_candidate() {
    // An empty graph has no entry point; `register` validates structure before
    // the signature check, so it is rejected as `Invalid`.
    let mut registry = SubgraphRegistry::new(contract());
    let err = registry.register("slot", Graph::new()).unwrap_err();

    assert!(matches!(err, RegistryError::Invalid { .. }));
    assert!(registry.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_flags_incompatible_inline_candidate() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    // Candidate produces i32 + reads Base, but the slot contract promises a
    // String output — a mismatch the validator must reject.
    graph.add_dynamic(
        "route",
        pick_choice,
        [("a", candidate(1, &recorder))],
        DynamicSlot::new(
            GraphSignature::new().produce::<String>(),
            ContextPolicy::shared(),
        ),
    );

    let result = graph.validate();
    assert!(!result.is_ok());
    let incompatible = result
        .errors
        .iter()
        .find_map(|err| match err {
            ValidationError::DynamicCandidateIncompatible { diff, .. } => Some(diff),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "expected DynamicCandidateIncompatible, got {:?}",
                result.errors
            )
        });

    // The diff pinpoints the mismatch: the candidate produces i32 (extra) and
    // reads Base (extra) where the slot wanted only a String output (missing).
    assert!(!incompatible.is_empty());
    assert_eq!(
        incompatible.missing_produces().len(),
        1,
        "slot wanted String"
    );
    assert!(
        !incompatible.extra_produces().is_empty(),
        "candidate produces i32 the slot did not"
    );
}

#[test]
fn validate_flags_empty_inline_source() {
    let mut graph = Graph::new();
    let empty: Vec<(&'static str, Graph)> = Vec::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        empty,
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let result = graph.validate();
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::EmptyDynamicSource { .. })),
        "expected EmptyDynamicSource, got {:?}",
        result.errors
    );
}

#[test]
fn validate_flags_structurally_invalid_inline_candidate() {
    let mut graph = Graph::new();
    // An empty candidate graph has no entry point — structurally invalid. The
    // validator wraps the inner error as `DynamicCandidateInvalid` and skips the
    // signature check, so no incompatibility error stacks on top.
    graph.add_dynamic(
        "route",
        pick_choice,
        [("a", Graph::new())],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let result = graph.validate();
    assert!(!result.is_ok());
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::DynamicCandidateInvalid { .. })),
        "expected DynamicCandidateInvalid, got {:?}",
        result.errors
    );
    assert!(
        !result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::DynamicCandidateIncompatible { .. })),
        "incompatibility must not stack on a structural error: {:?}",
        result.errors
    );
}

#[test]
fn validate_rejects_inline_candidate_that_nests_a_scope() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [("a", candidate_nesting_a_scope(&recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let result = graph.validate();
    assert!(!result.is_ok());
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::DynamicCandidateNested { .. })),
        "expected DynamicCandidateNested, got {:?}",
        result.errors
    );
    // The unreliable signature of a nested candidate must not also produce an
    // incompatibility error on top of the nesting rejection.
    assert!(
        !result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::DynamicCandidateIncompatible { .. })),
        "incompatibility must not stack on a nesting rejection: {:?}",
        result.errors
    );
}

#[test]
fn validate_rejects_inline_candidate_that_nests_a_dynamic() {
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [("a", candidate_nesting_a_dynamic())],
        DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
    );

    let result = graph.validate();
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::DynamicCandidateNested { .. })),
        "expected DynamicCandidateNested, got {:?}",
        result.errors
    );
}

#[test]
fn registry_rejects_nested_candidate() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut registry = SubgraphRegistry::new(contract());
    let err = registry
        .register("slot", candidate_nesting_a_scope(&recorder))
        .unwrap_err();

    assert!(
        matches!(err, RegistryError::NestedCandidate { .. }),
        "expected NestedCandidate, got {err:?}"
    );
    assert!(
        !registry.contains("slot"),
        "nested candidate was not stored"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Contract-vs-surroundings validation (`validate_resources`)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_resources_flags_contract_requires_missing_for_registry_slot() {
    // The headline case: a registry-backed slot whose candidate set is unknown
    // (and here empty) at validation time is still held to its contract. The
    // contract requires `Base`; the context has none, so the miss surfaces
    // pre-flight rather than only when a candidate is eventually selected.
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(
        graph.validate().is_ok(),
        "structure is fine; resources are not"
    );

    let ctx = SystemContext::new(); // no Base
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("contract requires Base, which the context lacks");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingResource { resource, .. }
                if resource.contains("Base")
        )),
        "expected DynamicContractMissingResource for Base, got {errors:?}"
    );
}

#[test]
fn validate_resources_flags_missing_require_write_resource() {
    // The `requires` axis carries an access mode: a slot contract declaring
    // `require_write::<Base>()` surfaces the same missing-resource error as a
    // `require_read`, but tagged `AccessMode::Write` — exercising the write arm
    // of the contract-requires check and proving the mode is carried through.
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(
            GraphSignature::new()
                .require_write::<Base>()
                .produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let ctx = SystemContext::new(); // no Base
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("contract requires ResMut<Base>, which the context lacks");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingResource { resource, mode, .. }
                if resource.contains("Base") && *mode == AccessMode::Write
        )),
        "expected DynamicContractMissingResource(Write) for Base, got {errors:?}"
    );
}

#[test]
fn validate_resources_checks_contract_requires_across_a_non_shared_boundary() {
    // Under a non-shared `share::<Base>()` policy the contract requires are
    // checked against the filtered child. `Base` reaches the child through the
    // shared parent chain, so validation passes; drop `Base` and it fails.
    let build = || {
        let mut graph = Graph::new();
        graph.add_dynamic_registry(
            "route",
            |_ctx| Arc::from("slot"),
            DynamicSlot::new(contract(), ContextPolicy::new().share::<Base>()),
        );
        graph
    };

    let with_base = SystemContext::new().with(Base { n: 1 });
    assert!(
        GraphExecutor::new()
            .validate_resources(&build(), &with_base, None)
            .is_ok(),
        "Base is reachable through the shared parent chain"
    );

    let without_base = SystemContext::new();
    let errors = GraphExecutor::new()
        .validate_resources(&build(), &without_base, None)
        .expect_err("Base is not reachable, so the contract cannot be honored");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingResource { .. }
        )),
        "expected DynamicContractMissingResource, got {errors:?}"
    );
}

#[test]
fn validate_resources_blocks_required_outputs_under_non_shared_policy() {
    // Free outputs never cross a non-shared boundary inward, so a contract that
    // requires one is statically unsatisfiable regardless of which candidate runs.
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(
            GraphSignature::new().require_output::<i32>(),
            ContextPolicy::new().forward::<Base>(),
        ),
    );

    let ctx = SystemContext::new().with(Base { n: 1 });
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("a required free output cannot cross a non-shared boundary");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractOutputsBlocked { .. }
        )),
        "expected DynamicContractOutputsBlocked, got {errors:?}"
    );
}

#[test]
fn validate_resources_flags_missing_required_output_under_shared_policy() {
    // A shared-policy slot may require a free output, but only if a system
    // produces it upstream. Nothing produces `Out<i32>` before the node here.
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(
            GraphSignature::new().require_output::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let ctx = SystemContext::new();
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("required output i32 is not produced upstream");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingOutput { .. }
        )),
        "expected DynamicContractMissingOutput, got {errors:?}"
    );
}

#[test]
fn validate_resources_accepts_required_output_produced_upstream() {
    // An upstream system produces i32; a shared-policy slot then requires
    // `Out<i32>`. The contract is satisfiable, so validation passes.
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(Constant { value: 1 })); // produces i32
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(
            GraphSignature::new().require_output::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let ctx = SystemContext::new();
    assert!(
        GraphExecutor::new()
            .validate_resources(&graph, &ctx, None)
            .is_ok(),
        "the required output is produced by the upstream system"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Registry source: runtime failure paths
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn registry_missing_key_without_default_errors() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("missing"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    // Registry present and seeded, but the selector's key isn't registered and
    // there is no default to fall back to.
    let mut registry = SubgraphRegistry::new(contract());
    registry
        .register("present", candidate(10, &recorder))
        .unwrap();
    let mut ctx = SystemContext::new().with(Base { n: 1 }).with(registry);

    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("missing registry key with no default should error");
    assert!(
        matches!(err, ExecutionError::DynamicCandidateNotFound { .. }),
        "expected DynamicCandidateNotFound, got {err:?}"
    );
    assert!(
        recorder.lock().unwrap().is_empty(),
        "no candidate may run after the failed lookup, got {:?}",
        recorder.lock().unwrap()
    );
}

#[tokio::test]
async fn registry_falls_back_to_default_when_key_absent() {
    // The default-fallback path is exercised for the inline source elsewhere;
    // this covers it for the registry source: the selector's key is absent, but
    // a registered default key resolves and runs.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("missing"),
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("fallback"),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut registry = SubgraphRegistry::new(contract());
    registry
        .register("fallback", candidate(10, &recorder))
        .unwrap();
    let mut ctx = SystemContext::new().with(Base { n: 7 }).with(registry);

    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry default fallback should run");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![70],
        "the registered default candidate ran when the selector's key was absent"
    );
}

#[tokio::test]
async fn registry_absent_from_context_errors() {
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        // Even the configured default cannot resolve when no registry is present:
        // the default key resolves through the same absent registry.
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("slot"),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    // No `SubgraphRegistry` inserted into the context — the most likely
    // misconfiguration for a registry-backed dynamic node. The error names the
    // real cause (missing registry) rather than a misleading candidate miss.
    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("registry-backed node with no registry in context should error");
    assert!(
        matches!(err, ExecutionError::DynamicRegistryMissing { .. }),
        "expected DynamicRegistryMissing, got {err:?}"
    );
}

#[tokio::test]
async fn registry_hidden_by_scope_policy_errors_out_of_scope() {
    // The registry lives in the parent context, but the dynamic node runs inside
    // an isolated scope whose policy does not share it. Resolution fails as
    // out-of-scope — carrying crossing-verb guidance — rather than as a
    // candidate miss or a missing registry.
    let mut inner = Graph::new();
    inner.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
    );
    let mut graph = Graph::new();
    graph.add_scope("iso", inner, ContextPolicy::new());

    let registry = SubgraphRegistry::new(GraphSignature::new());
    let mut ctx = SystemContext::new().with(registry);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a registry hidden by the enclosing scope policy should error");
    assert!(
        matches!(err, ExecutionError::DynamicRegistryOutOfScope { .. }),
        "expected DynamicRegistryOutOfScope, got {err:?}"
    );
}

#[tokio::test]
async fn register_overwrite_changes_running_candidate() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let mut registry = SubgraphRegistry::new(contract());
    registry.register("slot", candidate(10, &recorder)).unwrap();
    // Re-register the same key directly (no `remove` first): documented to
    // replace the existing candidate, not append.
    registry
        .register("slot", candidate(100, &recorder))
        .unwrap();
    assert_eq!(registry.len(), 1, "overwrite replaces, not appends");

    let mut ctx = SystemContext::new().with(Base { n: 5 }).with(registry);
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry-backed dynamic execution should succeed");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![500],
        "the overwriting candidate (factor 100) ran, not the replaced one"
    );
}

#[tokio::test]
async fn registry_contract_mismatch_is_refused() {
    // The node's slot requires `Base` and produces `i32`.
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    // The registry was built with a *different* contract (produces `i32`, reads
    // nothing). Its candidate is valid against the registry's own contract, but
    // the node's slot demands a `Base` read the registry never guarantees — so
    // running this candidate would bypass the parent graph's validation. The
    // executor must refuse rather than run it.
    let mut registry = SubgraphRegistry::new(GraphSignature::new().produce::<i32>());
    let mut constant = Graph::new();
    constant.add_boxed_system(Box::new(Constant { value: 7 }));
    registry
        .register("slot", constant)
        .expect("constant candidate is compatible with the registry's own contract");

    let mut ctx = SystemContext::new().with(Base { n: 1 }).with(registry);
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a registry whose contract diverges from the slot must be refused");
    assert!(
        matches!(err, ExecutionError::DynamicContractMismatch { .. }),
        "expected DynamicContractMismatch, got {err:?}"
    );
}

#[test]
fn self_referential_registry_candidate_is_refused_as_nested() {
    // A candidate that is itself a registry-backed dynamic node would recurse on
    // each execution. Rather than lean on the runtime recursion limit to bound
    // it, registration refuses the candidate up front: it nests a dynamic
    // boundary that flat signature derivation cannot see across. The unbounded
    // recursion is thus prevented at build time, not merely capped at run time.
    fn loop_graph() -> Graph {
        let mut graph = Graph::new();
        // A lone dynamic node has no aggregate IO, so an empty slot contract.
        graph.add_dynamic_registry(
            "loop",
            |_ctx| Arc::from("loop"),
            DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
        );
        graph
    }

    let mut registry = SubgraphRegistry::new(GraphSignature::new());
    let err = registry
        .register("loop", loop_graph())
        .expect_err("a candidate nesting a dynamic node must be refused");
    assert!(
        matches!(err, RegistryError::NestedCandidate { .. }),
        "expected NestedCandidate, got {err:?}"
    );
    assert!(registry.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// Candidate-level timeout
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn candidate_max_duration_times_out_at_the_boundary() {
    // The selected candidate carries its own `max_duration`; the dynamic
    // boundary wraps its execution in that timeout. A candidate that sleeps
    // longer than its budget fails with `GraphTimeout` rather than completing.
    let timeout_contract = GraphSignature::new().produce::<i32>();

    let mut slow = Graph::new();
    slow.add_boxed_system(Box::new(Slow {
        delay: Duration::from_millis(500),
    }));
    slow.with_max_duration(Duration::from_millis(20));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("slow"),
        [("slow", slow)],
        DynamicSlot::new(timeout_contract, ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new();
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("a candidate exceeding its max_duration should time out");
    assert!(
        matches!(err, ExecutionError::GraphTimeout { .. }),
        "expected GraphTimeout, got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Downstream output propagation
// ─────────────────────────────────────────────────────────────────────────────

/// A consumer placed after a dynamic node: reads the `Out<i32>` the selected
/// candidate produced and records what it saw.
struct ReadOutput {
    seen: Arc<Mutex<Option<i32>>>,
}

impl System for ReadOutput {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            let value = match ctx.get_output::<i32>() {
                Ok(out) => Some(*out),
                Err(_) => None,
            };
            *seen.lock().unwrap() = value;
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "read_output"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.add_output::<i32>();
        access
    }
}

#[tokio::test]
async fn downstream_system_reads_dynamic_output() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(None));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("b"),
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );
    // A consumer after the dynamic node reads the candidate's `Out<i32>`; it is
    // reachable only because the node's contract declares it produces `i32`.
    graph.add_boxed_system(Box::new(ReadOutput {
        seen: Arc::clone(&seen),
    }));
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 3 });
    // Pre-flight: output reachability must credit the contract's `produces` to
    // the downstream reader — without that credit, `validate_resources` would
    // report a spurious `MissingOutput` for `read_output`.
    assert!(
        GraphExecutor::new()
            .validate_resources(&graph, &ctx, None)
            .is_ok(),
        "the contract's produce::<i32>() satisfies the downstream Out<i32> read"
    );
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("dynamic + downstream consumer should execute");

    assert_eq!(*recorder.lock().unwrap(), vec![300]);
    assert_eq!(
        *seen.lock().unwrap(),
        Some(300),
        "downstream system should observe the candidate's merged-back Out<i32>"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Hooks and middleware
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dynamic_node_emits_start_and_complete_hooks() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("b"),
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );

    let events: Arc<Mutex<Vec<GraphEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let hooks = HooksAPI::new();
    let start_log = Arc::clone(&events);
    hooks
        .register_observer::<OnDynamicStart, _>("rec_start", move |event: &GraphEvent| {
            start_log.lock().unwrap().push(event.clone());
        })
        .expect("hook registration should succeed");
    let complete_log = Arc::clone(&events);
    hooks
        .register_observer::<OnDynamicComplete, _>("rec_complete", move |event: &GraphEvent| {
            complete_log.lock().unwrap().push(event.clone());
        })
        .expect("hook registration should succeed");

    let mut ctx = SystemContext::new().with(Base { n: 3 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect("dynamic execution should succeed");

    let events = events.lock().unwrap();
    assert_eq!(
        events.len(),
        2,
        "expected DynamicStart then DynamicComplete, got {events:?}"
    );
    assert!(
        matches!(
            &events[0],
            GraphEvent::DynamicStart {
                node_name: "route",
                mode: ContextMode::Shared,
                ..
            }
        ),
        "got {:?}",
        events[0]
    );
    assert!(
        matches!(
            &events[1],
            GraphEvent::DynamicComplete {
                node_name: "route",
                mode: ContextMode::Shared,
                nodes_executed: 1,
                selected,
                ..
            } if &**selected == "b"
        ),
        "expected DynamicComplete selecting 'b' with 1 node executed, got {:?}",
        events[1]
    );
}

#[tokio::test]
async fn dynamic_middleware_observes_info() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("b"),
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );

    let observed: Arc<Mutex<Option<DynamicInfo>>> = Arc::new(Mutex::new(None));
    let observed_clone = Arc::clone(&observed);
    let middleware = MiddlewareAPI::new();
    middleware.register_dynamic("capture_info", move |info: DynamicInfo, ctx, next| {
        let observed = Arc::clone(&observed_clone);
        Box::pin(async move {
            *observed.lock().unwrap() = Some(info.clone());
            next.run(ctx).await
        })
    });

    let mut ctx = SystemContext::new().with(Base { n: 2 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect("dynamic execution should succeed");

    let info = observed
        .lock()
        .unwrap()
        .clone()
        .expect("middleware should observe DynamicInfo");
    assert_eq!(info.node_name, "route");
    assert_eq!(info.mode, ContextMode::Shared);
    assert_eq!(
        info.candidate_count,
        Some(2),
        "two inline candidates, count known at the node"
    );
}

#[tokio::test]
async fn dynamic_middleware_reports_no_count_for_registry_source() {
    // A registry source's candidate set is per-session and unknown at the node,
    // so middleware sees `None` — distinct from `Some(0)` for an empty inline set.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let observed: Arc<Mutex<Option<DynamicInfo>>> = Arc::new(Mutex::new(None));
    let observed_clone = Arc::clone(&observed);
    let middleware = MiddlewareAPI::new();
    middleware.register_dynamic("capture_info", move |info: DynamicInfo, ctx, next| {
        let observed = Arc::clone(&observed_clone);
        Box::pin(async move {
            *observed.lock().unwrap() = Some(info.clone());
            next.run(ctx).await
        })
    });

    let mut registry = SubgraphRegistry::new(contract());
    registry.register("slot", candidate(10, &recorder)).unwrap();
    let mut ctx = SystemContext::new().with(Base { n: 2 }).with(registry);
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .expect("registry-backed dynamic execution should succeed");

    let info = observed
        .lock()
        .unwrap()
        .clone()
        .expect("middleware should observe DynamicInfo");
    assert_eq!(
        info.candidate_count, None,
        "registry candidate count is not known at the node"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// duplicate
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicate_preserves_inline_dynamic_selection() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut original = Graph::new();
    original.add_dynamic(
        "route",
        |_ctx| Arc::from("b"),
        [
            ("a", candidate(10, &recorder)),
            ("b", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()).with_default_key("a"),
    );

    let clone = original.duplicate();
    assert!(clone.validate().is_ok(), "{:?}", clone.validate().errors);

    // The clone mints a fresh node id but shares the selector and inline
    // candidate set via `Arc`, so it selects and runs the same candidate.
    let orig_id = original
        .nodes()
        .iter()
        .find(|node| node.name() == "route")
        .unwrap()
        .id();
    let clone_id = clone
        .nodes()
        .iter()
        .find(|node| node.name() == "route")
        .unwrap()
        .id();
    assert_ne!(
        orig_id, clone_id,
        "duplicate must remap the dynamic node id"
    );

    let mut ctx = SystemContext::new().with(Base { n: 3 });
    GraphExecutor::new()
        .execute(&clone, &mut ctx, None, None)
        .await
        .expect("cloned dynamic graph should execute");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![300],
        "clone ran candidate 'b' identically to the original"
    );
}

#[tokio::test]
async fn duplicate_preserves_registry_dynamic() {
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut original = Graph::new();
    original.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let clone = original.duplicate();
    assert!(clone.validate().is_ok(), "{:?}", clone.validate().errors);

    // The `Registry` source (with its reserved resource_type) survives the
    // clone, so the cloned node still resolves candidates from the registry.
    let mut registry = SubgraphRegistry::new(contract());
    registry.register("slot", candidate(7, &recorder)).unwrap();
    let mut ctx = SystemContext::new().with(Base { n: 6 }).with(registry);
    GraphExecutor::new()
        .execute(&clone, &mut ctx, None, None)
        .await
        .expect("cloned registry-backed dynamic graph should execute");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![42],
        "cloned registry-backed node resolved 'slot'"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler subgraphs are part of the interface
// ─────────────────────────────────────────────────────────────────────────────

/// A handler system producing a `u8` no slot contract in these tests sanctions.
struct HandlerExtra;

impl System for HandlerExtra {
    type Output = u8;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async { Ok(0) })
    }

    fn name(&self) -> &'static str {
        "handler_extra"
    }
}

#[test]
fn validate_rejects_inline_candidate_with_handler_io_outside_the_contract() {
    // The candidate's happy path matches the contract exactly; its error
    // handler produces a `u8` the contract never sanctioned. That output merges
    // back on the failure path, so admission must count it — a derivation that
    // skipped error edges would let it through unchecked.
    let mut sneaky = Graph::new();
    sneaky.add_boxed_system(Box::new(FailingCandidate));
    sneaky.add_error_handler(|g| {
        g.add_boxed_system(Box::new(HandlerExtra));
    });

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("sneaky"),
        [("sneaky", sneaky)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let result = graph.validate();
    assert!(
        result.errors.iter().any(|err| matches!(
            err,
            ValidationError::DynamicCandidateIncompatible { key, diff, .. }
                if &**key == "sneaky" && diff.extra_produces().len() == 1
        )),
        "expected DynamicCandidateIncompatible naming the handler's extra output, got {:?}",
        result.errors
    );
}

#[test]
fn validate_rejects_nested_boundary_hidden_inside_a_candidate_handler() {
    // A Scope boundary smuggled behind an error edge must trip the same
    // nested-candidate rejection as one on the happy path — otherwise a
    // registry-backed Dynamic inside a handler could re-enter selection at
    // runtime with IO the slot never saw.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(Multiply {
        factor: 10,
        recorder: Arc::clone(&recorder),
    }));

    let mut sneaky = Graph::new();
    sneaky.add_boxed_system(Box::new(FailingCandidate));
    sneaky.add_error_handler(|g| {
        g.add_scope("hidden", inner, ContextPolicy::shared());
    });

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("sneaky"),
        [("sneaky", sneaky)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let result = graph.validate();
    assert!(
        result.errors.iter().any(|err| matches!(
            err,
            ValidationError::DynamicCandidateNested { key, .. } if &**key == "sneaky"
        )),
        "expected DynamicCandidateNested for the handler-hidden scope, got {:?}",
        result.errors
    );
}

#[test]
fn registry_rejects_candidate_with_handler_io_outside_the_contract() {
    // Same guarantee at the registry admission point.
    let mut sneaky = Graph::new();
    sneaky.add_boxed_system(Box::new(FailingCandidate));
    sneaky.add_error_handler(|g| {
        g.add_boxed_system(Box::new(HandlerExtra));
    });

    let mut registry = SubgraphRegistry::new(GraphSignature::new().produce::<i32>());
    let err = registry
        .register("sneaky", sneaky)
        .expect_err("handler IO outside the contract must be refused");
    assert!(
        matches!(err, RegistryError::Incompatible { .. }),
        "expected Incompatible, got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Duplicate inline keys
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_flags_duplicate_inline_candidate_keys() {
    // Inline lookup is first-match: with two candidates keyed "a", the second
    // is validated as if it participates but can never be selected. That silent
    // dead weight is a misconfiguration, so `validate()` rejects it.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [
            ("a", candidate(10, &recorder)),
            ("a", candidate(100, &recorder)),
        ],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );

    let result = graph.validate();
    assert!(
        result.errors.iter().any(|err| matches!(
            err,
            ValidationError::DynamicCandidateDuplicate { key, name, .. }
                if &**key == "a" && *name == "route"
        )),
        "expected DynamicCandidateDuplicate for key 'a', got {:?}",
        result.errors
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Inline candidate interiors and boundary crossings (`validate_resources`)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_resources_recurses_into_inline_candidate_interiors() {
    // Beyond the contract-level check, `validate_resources` walks each inline
    // candidate's interior: the candidate's own system-level access is checked
    // against the boundary context. With `Base` absent, both the contract miss
    // *and* the interior system's miss must surface — the latter proves the
    // recursion runs (deleting it would leave only the contract error).
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        pick_choice,
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let ctx = SystemContext::new(); // no Base
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("Base is missing for both the contract and the interior system");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingResource { resource, .. }
                if resource.contains("Base")
        )),
        "expected the contract-level miss, got {errors:?}"
    );
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::MissingResource { system_name, resource_type, .. }
                if *system_name == "multiply" && resource_type.contains("Base")
        )),
        "expected the interior system's miss (the recursion), got {errors:?}"
    );
}

#[tokio::test]
async fn missing_forward_crossing_names_the_dynamic_boundary() {
    // A `forward::<Base>()` crossing on the slot's policy with `Base` absent
    // must fail both pre-flight and at runtime, labeled with the *dynamic
    // node's* name — pinning the generalization of the scope-crossing checks to
    // dynamic boundaries.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let build = |recorder: &Arc<Mutex<Vec<i32>>>| {
        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            pick_choice,
            [("a", candidate(10, recorder))],
            DynamicSlot::new(contract(), ContextPolicy::new().forward::<Base>()),
        );
        graph
    };

    let graph = build(&recorder);
    let ctx = SystemContext::new(); // no Base
    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx, None)
        .expect_err("forward::<Base> has no source resource");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::ScopeMissingResource { scope_name, resource, action, .. }
                if *scope_name == "route" && resource.contains("Base") && *action == "forward"
        )),
        "expected ScopeMissingResource naming the dynamic node, got {errors:?}"
    );

    let mut ctx = SystemContext::new(); // still no Base
    let err = GraphExecutor::new()
        .execute(&build(&recorder), &mut ctx, None, None)
        .await
        .expect_err("the runtime safety net must fail the same way");
    match err {
        ExecutionError::ScopeMissingResource {
            scope, resource, ..
        } => {
            assert_eq!(scope, "route", "the dynamic node labels the boundary");
            assert!(resource.contains("Base"), "got {resource}");
        }
        other => panic!("expected ScopeMissingResource, got {other:?}"),
    }
    assert!(
        recorder.lock().unwrap().is_empty(),
        "no candidate ran on either path"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Error routing at the dynamic boundary
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dynamic_failure_is_not_caught_by_a_parent_error_handler() {
    // Error edges route *system* failures. A dynamic node's failure — like a
    // scope's — propagates to the caller even if an error handler is attached
    // to the dynamic node itself. Pinned so the behavior can't drift silently;
    // if handler routing is ever extended to boundary nodes, this test should
    // be revisited deliberately.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut failing = Graph::new();
    failing.add_boxed_system(Box::new(FailingCandidate));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("boom"),
        [("boom", failing)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );
    let dynamic_id = graph
        .nodes()
        .iter()
        .find(|node| matches!(node, polaris_graph::node::Node::Dynamic(_)))
        .map(polaris_graph::node::Node::id)
        .expect("the graph contains the dynamic node");
    graph.add_error_handler_for([dynamic_id], |g| {
        g.add_boxed_system(Box::new(Multiply {
            factor: 1,
            recorder: Arc::clone(&recorder),
        }));
    });

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the dynamic failure propagates");
    assert!(
        matches!(err, ExecutionError::SystemError(_)),
        "the candidate's failure surfaces unhandled, got {err:?}"
    );
    assert!(
        recorder.lock().unwrap().is_empty(),
        "the handler attached to the dynamic node must not fire"
    );
}

#[tokio::test]
async fn dynamic_timeout_is_not_caught_by_a_parent_timeout_handler() {
    // The timeout twin of the error-handler bypass above: timeout edges route
    // *system* timeouts, so a candidate's `GraphTimeout` at the dynamic boundary
    // propagates even when a timeout handler is attached to the dynamic node
    // itself. Pinned so timeout routing cannot silently diverge from error
    // routing at the boundary; extend both deliberately or neither.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut slow = Graph::new();
    slow.add_boxed_system(Box::new(Slow {
        delay: Duration::from_millis(500),
    }));
    slow.with_max_duration(Duration::from_millis(20));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| "slow",
        [("slow", slow)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );
    let dynamic_id = graph
        .nodes()
        .iter()
        .find(|node| matches!(node, polaris_graph::node::Node::Dynamic(_)))
        .map(polaris_graph::node::Node::id)
        .expect("the graph contains the dynamic node");
    graph.add_timeout_handler([dynamic_id], |g| {
        g.add_boxed_system(Box::new(Multiply {
            factor: 1,
            recorder: Arc::clone(&recorder),
        }));
    });

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the candidate's timeout propagates");
    assert!(
        matches!(err, ExecutionError::GraphTimeout { .. }),
        "the boundary timeout surfaces unhandled, got {err:?}"
    );
    assert!(
        recorder.lock().unwrap().is_empty(),
        "the timeout handler attached to the dynamic node must not fire"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Boxed (fallible) selectors
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn boxed_selector_error_propagates_as_predicate_error() {
    use polaris_graph::predicate::PredicateError;
    use polaris_graph::selector::{BoxedSelector, ErasedSelector};

    // `add_dynamic_boxed` admits hand-implemented selectors, making the
    // fallible path reachable through the public builder: a selector error
    // surfaces as `PredicateError` before any candidate runs.
    struct AlwaysFails;
    impl ErasedSelector for AlwaysFails {
        fn select(&self, _ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
            Err(PredicateError::OutputNotFound { type_name: "Route" })
        }
        fn input_type_name(&self) -> &'static str {
            "Route"
        }
    }

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let selector: BoxedSelector = Box::new(AlwaysFails);
    let mut graph = Graph::new();
    graph.add_dynamic_boxed(
        "route",
        selector,
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the selector's error must surface");
    assert!(
        matches!(err, ExecutionError::PredicateError(_)),
        "expected PredicateError from the selector, got {err:?}"
    );
    assert!(
        recorder.lock().unwrap().is_empty(),
        "no candidate ran after the selector failed"
    );
}

#[tokio::test]
async fn boxed_selector_runs_registry_candidate_end_to_end() {
    use polaris_graph::predicate::PredicateError;
    use polaris_graph::selector::{BoxedSelector, ErasedSelector};

    // `add_dynamic_registry_boxed` is the fourth builder entry point; this
    // exercises it end-to-end: a hand-implemented selector resolves a key
    // against the per-session registry and the registered candidate runs.
    struct PickSlot;
    impl ErasedSelector for PickSlot {
        fn select(&self, _ctx: &SystemContext<'_>) -> Result<Arc<str>, PredicateError> {
            Ok(Arc::from("slot"))
        }
        fn input_type_name(&self) -> &'static str {
            "PickSlot"
        }
    }

    let recorder = Arc::new(Mutex::new(Vec::new()));
    let selector: BoxedSelector = Box::new(PickSlot);
    let mut graph = Graph::new();
    graph.add_dynamic_registry_boxed(
        "route",
        selector,
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut registry = SubgraphRegistry::new(contract());
    registry.register("slot", candidate(10, &recorder)).unwrap();
    let mut ctx = SystemContext::new().with(Base { n: 5 }).with(registry);
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("the boxed selector resolves the registry candidate");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![50],
        "the registered candidate ran via the boxed registry builder"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Warning propagation and error rendering
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidate_warnings_propagate_as_dynamic_candidate_warnings() {
    use polaris_graph::graph::ValidationWarning;

    // A structurally valid candidate can still carry warnings — here a parallel
    // node whose branches both produce `i32`, so the last branch silently wins
    // at merge time. `validate()` must wrap the inner warning as
    // `DynamicCandidateWarning` naming the node and candidate key, not drop it
    // at the boundary.
    async fn branch_a() -> i32 {
        1
    }
    async fn branch_b() -> i32 {
        2
    }

    let mut noisy = Graph::new();
    noisy.add_parallel(
        "conflict",
        vec![
            |g: &mut Graph| {
                g.add_system(branch_a);
            },
            |g: &mut Graph| {
                g.add_system(branch_b);
            },
        ],
    );

    let mut graph = Graph::new();
    // The parallel node merges its branches' `i32` back into the parent, so the
    // candidate's signature produces `i32` and the contract must sanction it.
    graph.add_dynamic(
        "route",
        |_ctx| "noisy",
        [("noisy", noisy)],
        DynamicSlot::new(
            GraphSignature::new().produce::<i32>(),
            ContextPolicy::shared(),
        ),
    );

    let result = graph.validate();
    assert!(
        result.is_ok(),
        "warnings must not fail validation, got errors: {:?}",
        result.errors
    );
    assert!(
        result.warnings.iter().any(|warn| matches!(
            warn,
            ValidationWarning::DynamicCandidateWarning { name: "route", key, inner, .. }
                if &**key == "noisy"
                    && matches!(
                        &**inner,
                        ValidationWarning::ConflictingParallelOutputs { .. }
                    )
        )),
        "expected DynamicCandidateWarning wrapping ConflictingParallelOutputs, got {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn candidate_not_found_error_display_debug_escapes_the_selector_key() {
    // The selector key may derive from model output. The error's Display must
    // Debug-escape it so an adversarial key cannot inject a raw newline (or
    // other control bytes) into log/error text — the same property already
    // pinned for `GraphEvent::DynamicComplete` rendering in `hooks/events.rs`.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| "evil\nkey",
        [("a", candidate(10, &recorder))],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Base { n: 1 });
    let err = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect_err("the injected key resolves no candidate");
    assert!(
        matches!(&err, ExecutionError::DynamicCandidateNotFound { key, .. } if &**key == "evil\nkey"),
        "expected DynamicCandidateNotFound carrying the raw key, got {err:?}"
    );

    let rendered = err.to_string();
    assert!(
        !rendered.contains('\n'),
        "the rendered error must not contain a raw newline: {rendered:?}"
    );
    assert!(
        rendered.contains("evil\\nkey"),
        "the key must render Debug-escaped: {rendered:?}"
    );
}

#[test]
fn validate_resources_recurses_into_interiors_across_a_non_shared_boundary() {
    // The non-shared sibling of
    // `validate_resources_recurses_into_inline_candidate_interiors`: under a
    // `share::<Base>()` policy the candidate interior is validated against the
    // *filtered child*, not the parent. With `Base` shared and present the
    // interior system resolves through the child's parent filter; with `Base`
    // absent both the contract miss and the interior system's miss surface.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let build = |recorder: &Arc<Mutex<Vec<i32>>>| {
        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            pick_choice,
            [("a", candidate(10, recorder))],
            DynamicSlot::new(contract(), ContextPolicy::new().share::<Base>()),
        );
        graph
    };

    let with_base = SystemContext::new().with(Base { n: 1 });
    assert!(
        GraphExecutor::new()
            .validate_resources(&build(&recorder), &with_base, None)
            .is_ok(),
        "the shared Base reaches the candidate interior through the child filter"
    );

    let without_base = SystemContext::new();
    let errors = GraphExecutor::new()
        .validate_resources(&build(&recorder), &without_base, None)
        .expect_err("Base is missing for both the contract and the interior system");
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::DynamicContractMissingResource { resource, .. }
                if resource.contains("Base")
        )),
        "expected the contract-level miss, got {errors:?}"
    );
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::MissingResource { system_name, resource_type, .. }
                if *system_name == "multiply" && resource_type.contains("Base")
        )),
        "expected the interior system's miss through the child filter, got {errors:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Boundary verbs, multi-candidate registry routing, per-session isolation, and
// multi-node execution counts
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fork_policy_forks_resource_into_dynamic_candidate() {
    // Sibling to `forward_policy_copies_resource_into_dynamic_candidate`: a
    // non-`Shared` policy whose crossing verb is `fork` rather than `forward`.
    // `Ledger::fork` yields a fresh-empty ledger, so the candidate must observe
    // an empty log (count 0) even though the parent's ledger holds an entry —
    // proving the child ran against `ForkStrategy::fork`, not a clone of the
    // parent (which `forward` would produce, yielding count 1).
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut forked = Graph::new();
    forked.add_boxed_system(Box::new(CountLedger {
        recorder: Arc::clone(&recorder),
    }));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("a"),
        [("a", forked)],
        DynamicSlot::new(
            GraphSignature::new()
                .require_read::<Ledger>()
                .produce::<i32>(),
            ContextPolicy::new().fork::<Ledger>(),
        ),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut ctx = SystemContext::new().with(Ledger { entries: vec![7] });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("forked dynamic execution should succeed");
    assert_eq!(
        *recorder.lock().unwrap(),
        vec![0],
        "candidate saw the fresh-forked ledger, not a clone of the parent's"
    );
}

#[tokio::test]
async fn registry_selector_routes_among_multiple_candidates_by_context() {
    // Registry execution tests elsewhere use a constant selector; this drives the
    // key from the `Choice` resource, so the runtime key must resolve among
    // several concurrently-registered registry candidates.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        pick_choice,
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut registry = SubgraphRegistry::new(contract());
    registry.register("a", candidate(10, &recorder)).unwrap();
    registry.register("b", candidate(100, &recorder)).unwrap();

    let mut ctx = SystemContext::new()
        .with(Base { n: 4 })
        .with(Choice { pick: "b" })
        .with(registry);

    // `Choice` picks "b" → the factor-100 candidate runs.
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry routing to 'b' should succeed");
    assert_eq!(*recorder.lock().unwrap(), vec![400]);

    // Flip the key to "a" (same registry, a different candidate) → factor 10.
    {
        let mut choice = ctx.get_resource_mut::<Choice>().unwrap();
        choice.pick = "a";
    }
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("registry routing to 'a' should succeed");
    assert_eq!(*recorder.lock().unwrap(), vec![400, 40]);
}

#[tokio::test]
async fn registry_candidates_are_isolated_per_session() {
    // Two independent sessions each carry their own `SubgraphRegistry` behind the
    // same slot key. A candidate registered in one context must not leak into the
    // other — the registry is a `Local` resource scoped to its own context.
    let rec_a = Arc::new(Mutex::new(Vec::new()));
    let rec_b = Arc::new(Mutex::new(Vec::new()));
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| Arc::from("slot"),
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let mut reg_a = SubgraphRegistry::new(contract());
    reg_a.register("slot", candidate(10, &rec_a)).unwrap();
    let mut ctx_a = SystemContext::new().with(Base { n: 3 }).with(reg_a);

    let mut reg_b = SubgraphRegistry::new(contract());
    reg_b.register("slot", candidate(100, &rec_b)).unwrap();
    let mut ctx_b = SystemContext::new().with(Base { n: 3 }).with(reg_b);

    GraphExecutor::new()
        .execute(&graph, &mut ctx_a, None, None)
        .await
        .expect("session A should run its own candidate");
    GraphExecutor::new()
        .execute(&graph, &mut ctx_b, None, None)
        .await
        .expect("session B should run its own candidate");

    // Each session ran only the candidate registered in its own registry.
    assert_eq!(*rec_a.lock().unwrap(), vec![30]);
    assert_eq!(*rec_b.lock().unwrap(), vec![300]);
}

#[tokio::test]
async fn dynamic_complete_reports_multi_node_candidate_execution_count() {
    // Every other execution test runs a single-system candidate, so
    // `DynamicComplete.nodes_executed` is only ever observed as 1. A candidate
    // with two chained systems must report 2 — proving the count reflects real
    // interior traversal rather than a fixed value.
    let recorder = Arc::new(Mutex::new(Vec::new()));
    let mut multi = Graph::new();
    multi.add_boxed_system(Box::new(Multiply {
        factor: 10,
        recorder: Arc::clone(&recorder),
    }));
    multi.add_boxed_system(Box::new(Multiply {
        factor: 100,
        recorder: Arc::clone(&recorder),
    }));

    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("multi"),
        [("multi", multi)],
        DynamicSlot::new(contract(), ContextPolicy::shared()),
    );
    assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors);

    let events: Arc<Mutex<Vec<GraphEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let hooks = HooksAPI::new();
    let complete_log = Arc::clone(&events);
    hooks
        .register_observer::<OnDynamicComplete, _>("rec_complete", move |event: &GraphEvent| {
            complete_log.lock().unwrap().push(event.clone());
        })
        .expect("hook registration should succeed");

    let mut ctx = SystemContext::new().with(Base { n: 3 });
    GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await
        .expect("dynamic execution should succeed");

    // Both interior systems ran, in order.
    assert_eq!(*recorder.lock().unwrap(), vec![30, 300]);

    let events = events.lock().unwrap();
    assert_eq!(
        events.len(),
        1,
        "expected a single DynamicComplete, got {events:?}"
    );
    assert!(
        matches!(
            &events[0],
            GraphEvent::DynamicComplete {
                node_name: "route",
                nodes_executed: 2,
                selected,
                ..
            } if &**selected == "multi"
        ),
        "expected DynamicComplete selecting 'multi' with 2 nodes executed, got {:?}",
        events[0]
    );
}
