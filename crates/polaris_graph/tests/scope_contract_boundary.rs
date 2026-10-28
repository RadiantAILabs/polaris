//! Runtime behavior of a declared scope contract's merge-back boundary.
//!
//! `add_scope_with_contract` is more than a validate-time interface: on a
//! non-`shared()` policy the executor filters what actually crosses back
//! into the parent to the contract's `produces`, dropping everything else
//! the interior wrote. These tests exercise that filter end to end —
//! executed graphs, not just derived signatures — since the signature side
//! is covered in `polaris_graph::graph::signature`'s unit tests and
//! `tests/validation.rs`.

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::{Graph, GraphSignature};
use polaris_graph::node::ContextPolicy;
use polaris_system::param::{Out, SystemContext};
use polaris_system::system;
use polaris_system::system::{BoxFuture, System, SystemError};

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

async fn produce_secret_u8() -> u8 {
    7
}

async fn produce_wrapper_i32() -> i32 {
    42
}

/// A system that always fails, so its wired error handler runs.
struct FallibleProducer;
impl System for FallibleProducer {
    type Output = i32;
    fn run<'a>(&'a self, _ctx: &'a SystemContext<'_>) -> BoxFuture<'a, Result<i32, SystemError>> {
        Box::pin(async { Err(SystemError::ExecutionError("intentional failure".into())) })
    }
    fn name(&self) -> &'static str {
        "fallible_producer"
    }
}

/// The error handler's own output — an interior detail just like any other
/// undeclared production, even though it only exists on the failure path.
async fn produce_handler_extra_u16() -> u16 {
    999
}

// ─────────────────────────────────────────────────────────────────────────────
// Only declared outputs leave
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn undeclared_interior_output_does_not_reach_the_parent() {
    let mut inner = Graph::new();
    inner.add_system(produce_secret_u8);

    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        "worker",
        inner,
        ContextPolicy::new(),
        GraphSignature::new(), // declares nothing
    );
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert!(
        !ctx.contains_output::<u8>(),
        "the interior's undeclared u8 must not merge into the parent"
    );
}

#[tokio::test]
async fn a_parent_output_survives_an_undeclared_same_typed_interior_write() {
    let mut inner = Graph::new();
    inner.add_system(produce_secret_u8); // writes u8 = 7, undeclared

    let mut graph = Graph::new();
    graph.add_scope_with_contract("worker", inner, ContextPolicy::new(), GraphSignature::new());
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    // The parent already has its own u8 before the scope runs.
    let mut ctx = SystemContext::new();
    ctx.insert_output(100u8);

    // `u8` is also the graph's only ever "last written" type (the scope's own
    // write is filtered out, so nothing overwrites `last_type_id`), so the
    // executor's own end-of-run `take_last()` moves it into the
    // `ExecutionResult` — check it there rather than via `ctx.get_output`,
    // which is only reliable for types other than the terminal one.
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<u8>(),
        Some(&100),
        "the interior's undeclared write to the same type must never reach the parent, \
         so the parent's own value is left exactly as it was"
    );
}

#[tokio::test]
async fn a_declared_produce_reaches_the_parent() {
    let mut inner = Graph::new();
    inner.add_system(produce_wrapper_i32);

    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        "worker",
        inner,
        ContextPolicy::new(),
        GraphSignature::new().produce::<i32>(),
    );
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // `i32` is the run's only output, so it is also the "last written" type
    // and the executor's `take_last()` moves it into the `ExecutionResult`.
    assert_eq!(
        result.output::<i32>(),
        Some(&42),
        "declared produce crosses"
    );
}

/// Marker produced by a declared scope's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State;

/// A private type the same interior also writes, after `State`, that the
/// contract does not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Secret;

async fn produce_state() -> State {
    State
}

#[polaris_system::system]
async fn produce_secret_from_state(_state: Out<State>) -> Secret {
    Secret
}

/// Regression test: an isolated declared
/// scope contract naming only `produce::<State>()`, whose interior writes
/// `State` and then `Secret` (in that order). `Secret` is filtered out at the
/// boundary, but before this fix `take_last` — and so
/// `ExecutionResult::output`— tracked only whichever *type* was written last
/// in the child, so a filtered-out last write left the promoted `State` entry
/// unreachable as the run's final output even though it did cross the
/// boundary. The newest *allowed* entry must win instead.
#[tokio::test]
async fn filtered_merge_promotes_the_newest_allowed_entry_even_when_the_childs_last_write_is_dropped()
 {
    let mut inner = Graph::new();
    inner
        .add_system(produce_state)
        .add_system(produce_secret_from_state);

    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        "isolated",
        inner,
        ContextPolicy::new(),
        GraphSignature::new().produce::<State>(),
    );
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<State>(),
        Some(&State),
        "the newest allowed entry must win take_last even though Secret — the \
         child's chronologically last write — was dropped at the boundary"
    );
    assert!(
        !ctx.contains_output::<Secret>(),
        "Secret stays private, as an undeclared interior output always does"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Plain `add_scope` keeps today's unfiltered merge
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plain_add_scope_still_merges_every_interior_output_unfiltered() {
    let mut inner = Graph::new();
    inner.add_system(produce_secret_u8);

    let mut graph = Graph::new();
    graph.add_scope("worker", inner, ContextPolicy::new());
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<u8>(),
        Some(&7),
        "no contract means no filter — behavior is unchanged from before"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// `shared()` has no boundary, so a declared contract cannot hide anything
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shared_policy_has_no_boundary_to_filter() {
    let mut inner = Graph::new();
    inner
        .add_system(produce_secret_u8)
        .add_system(produce_wrapper_i32);

    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        "worker",
        inner,
        ContextPolicy::shared(),
        GraphSignature::new().produce::<i32>(), // u8 is guaranteed too, but left undeclared
    );
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // `i32` runs last, so it is the "last written" type moved into the
    // `ExecutionResult`; `u8` is not, so it is still directly readable on
    // `ctx` — proving `shared()` really did leave it visible rather than
    // this assertion accidentally relying on `take_last` semantics.
    assert_eq!(
        result.output::<i32>(),
        Some(&42),
        "declared produce is present"
    );
    assert!(
        ctx.contains_output::<u8>(),
        "shared() runs the interior directly in the parent context — every write is \
         visible immediately, declared or not, since there is no child context to filter"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Interior error/timeout handler outputs are filtered like anything else
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn interior_handler_output_is_filtered_like_any_other() {
    let mut inner = Graph::new();
    let fail_id = inner.add_boxed_system(Box::new(FallibleProducer));
    inner.add_error_handler_for(fail_id, |g| {
        g.add_system(produce_handler_extra_u16);
    });

    let mut graph = Graph::new();
    graph.add_scope_with_contract("worker", inner, ContextPolicy::new(), GraphSignature::new());
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;

    assert!(
        result.is_ok(),
        "the interior catches its own failure via the error edge: {result:?}"
    );
    assert!(
        !ctx.contains_output::<u16>(),
        "the handler's own output is undeclared, so it is filtered the same as a \
         main-path output — no special case for handler subgraphs"
    );
    assert!(
        !ctx.contains_output::<i32>(),
        "the failing node's output was never produced in the first place"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Three-level nesting: each level hides its own internals
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn produce_l2_wrap(l3_wrap: Out<i8>) -> i16 {
    i16::from(*l3_wrap) * 10
}

async fn produce_l3_wrap() -> i8 {
    5
}

async fn produce_l2_secret() -> u16 {
    100
}

#[tokio::test]
async fn three_level_nesting_hides_internals_at_each_level() {
    // Level 3 (innermost): guarantees a private u8 and a wrapper i8; only the
    // wrapper is declared.
    let mut level3 = Graph::new();
    level3
        .add_system(produce_secret_u8)
        .add_system(produce_l3_wrap);
    let level3_contract = GraphSignature::new().produce::<i8>();

    // Level 2: embeds level 3, reads its wrapper internally (proving i8 is
    // visible *inside* level 2), guarantees its own private u16, and
    // publishes only its own wrapper i16.
    let mut level2 = Graph::new();
    level2.add_scope_with_contract("l3", level3, ContextPolicy::new(), level3_contract.clone());
    level2
        .add_system(produce_l2_secret)
        .add_system(produce_l2_wrap);
    let level2_contract = GraphSignature::new().produce::<i16>();

    // Level 1 (the graph under test): embeds level 2 and nothing else.
    let mut graph = Graph::new();
    graph.add_scope_with_contract("l2", level2, ContextPolicy::new(), level2_contract.clone());

    assert!(graph.validate().is_ok(), "{}", graph.validate());
    assert_eq!(
        graph.signature(),
        GraphSignature::new().produce::<i16>(),
        "only the outermost wrapper is part of the interface"
    );

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // A successful run already proves `i8` was visible *inside* level 2
    // (`produce_l2_wrap` reads it via `Out<i8>`, an unconditional dependency
    // that would otherwise fail resolution). What's left to check is that
    // none of the hidden internals leaked all the way out to level 1. `i16`
    // is the run's only surfaced output, so it is the "last written" type
    // and lands in the `ExecutionResult` rather than staying in `ctx`.
    assert_eq!(
        result.output::<i16>(),
        Some(&50),
        "5 (l3 wrap) * 10, proving the value actually flowed through both boundaries"
    );
    assert!(
        !ctx.contains_output::<u8>(),
        "level 3's private u8 never left level 3"
    );
    assert!(
        !ctx.contains_output::<i8>(),
        "level 3's wrapper crossed into level 2 but not out to level 1"
    );
    assert!(
        !ctx.contains_output::<u16>(),
        "level 2's private u16 never left level 2"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Run-entry loop check respects the boundary
// ─────────────────────────────────────────────────────────────────────────────

/// A loop whose predicate reads a type a sealed scope keeps private must fail
/// the run-entry check, not mid-run with a `PredicateError`.
#[tokio::test]
async fn loop_entry_check_does_not_credit_a_sealed_scope_private_output() {
    use polaris_graph::executor::ExecutionError;

    let mut inner = Graph::new();
    inner.add_system(produce_secret_u8);

    let mut graph = Graph::new();
    graph
        .add_scope_with_contract(
            "sealed",
            inner,
            ContextPolicy::new(),
            GraphSignature::new(), // keeps u8 private
        )
        .add_loop::<u8, _, _>(
            "reads_private",
            |n: &u8| *n > 0,
            |g| {
                g.add_system(produce_secret_u8);
            },
        );

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;

    assert!(
        matches!(
            result,
            Err(ExecutionError::LoopPredicateInputMissingOnEntry { .. })
        ),
        "expected the run-entry error, got: {result:?}"
    );
}

/// A declared scope can take a name built at runtime.
#[tokio::test]
async fn declared_scope_accepts_a_runtime_name() {
    let mut inner = Graph::new();
    inner.add_system(produce_wrapper_i32);

    let name = format!("block_{}", 3);
    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        name,
        inner,
        ContextPolicy::new(),
        GraphSignature::new().produce::<i32>(),
    );
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<i32>(), Some(&42));
}
