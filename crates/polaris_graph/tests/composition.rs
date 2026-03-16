//! Integration tests demonstrating compositional graph building.
//!
//! Tests are organized by complexity level, with each level building on the previous:
//! 1. **Primitives**: Basic systems and execution tracking via `SystemInfo`
//! 2. **Atomic Builders**: Single control flow constructs (parallel, decision, loop, switch)
//! 3. **Composite Builders**: Nested control flow (decision containing parallel, etc.)
//! 4. **Complex Compositions**: Multi-level nesting to verify arbitrary depth execution
//! 5. **Property-Based**: Random fragment trees verified against predicted execution counts
//!
//! ## Fragment DSL
//!
//! The `Fragment` enum is a declarative DSL for building test graphs. Each variant maps to
//! a graph construct (node type or control flow pattern). Fragments are recursively built
//! into a `Graph`, and each `Track` leaf registers a `LoggingSystem` whose `NodeId` is
//! collected into a `TrackerNodes` list. After execution, the test verifies that each
//! tracker node fired the expected number of times.
//!
//! Decision and Switch fragments are **parameterized**: `Decision { take_true }` controls
//! which branch is taken, and `Switch { case }` selects between cases "a" and "b".
//! `Fallible { handler }` represents a system that always fails, routing to its error
//! handler body via an error edge. This enables both hand-written tests with explicit
//! branching and property-based tests that randomly generate fragment trees with varying
//! branch choices.
//!
//! ## Prediction Model
//!
//! Each `Fragment` can predict how many times each tracker will fire via `predicted_counts()`.
//! The prediction follows the same traversal order as `build()`:
//! - **Track**: fires once per enclosing execution (multiplied by any enclosing loops)
//! - **Seq/Par**: concatenate child predictions
//! - **Decision**: taken branch gets real counts, non-taken branch gets zeros
//! - **Loop { n }**: multiplies body counts by `n`
//! - **Switch**: selected case gets real counts, unselected case gets zeros
//! - **Fallible**: handler body predictions (failing system has no tracker)
//!
//! ## Property-Based Testing
//!
//! The `prop_tests` module uses `proptest` to generate random `Fragment` trees (depth 3,
//! 256 cases). At each non-leaf level, one of the 6 composite types (Seq, Par, Decision,
//! Loop, Switch, Fallible) is chosen with equal probability, ensuring broad coverage of
//! nesting combinations including error handling paths. The property asserts that per-node
//! execution counts match the predicted counts for every generated tree.
//!
//! ## Test Infrastructure
//!
//! Uses `Res<SystemInfo>` (injected by `DevToolsPlugin`) to verify that the *correct*
//! nodes executed, not just that some counter incremented. This catches bugs in slot
//! calculation or graph construction that wouldn't be caught by slot-based counting.

mod test_utils;

use polaris_graph::executor::{ExecutionError, GraphExecutor};
use polaris_graph::graph::Graph;
use polaris_graph::node::NodeId;
use test_utils::{
    DecisionOutput, DecisionSystem, ExecutionLog, FailingSystem, SwitchKeySystem, SwitchOutput,
    TrackerNodes, add_tracker, branch, create_test_server, get_hooks,
};

// ═══════════════════════════════════════════════════════════════════════════════
// FRAGMENT DSL
// ═══════════════════════════════════════════════════════════════════════════════

/// Declarative graph fragment for composable test builders.
/// Collects tracker `NodeId`s into a shared `TrackerNodes` during build.
///
/// `Debug` is derived so that `proptest` can display shrunk counterexamples.
#[derive(Clone, Debug)]
enum Fragment {
    /// Single tracking system.
    Track,
    /// Sequential execution: [a, b, c].
    Seq(Vec<Fragment>),
    /// Parallel branches.
    Par(Vec<Fragment>),
    /// Decision with parameterized predicate. When `take_true` is `true`, the
    /// `t` branch executes; otherwise the `f` branch executes.
    Decision {
        take_true: bool,
        t: Box<Fragment>,
        f: Box<Fragment>,
    },
    /// Bounded loop.
    Loop { n: usize, body: Box<Fragment> },
    /// Switch with parameterized discriminator. `case` selects which branch
    /// ("a" or "b") to execute.
    Switch {
        case: &'static str,
        a: Box<Fragment>,
        b: Box<Fragment>,
    },
    /// Fallible system with error handler. The system always fails, and the
    /// handler body executes via the error edge. Execution continues normally
    /// after the handler completes.
    Fallible { handler: Box<Fragment> },
}

impl Fragment {
    /// Build fragment into graph, collecting tracker node IDs.
    ///
    /// Registration order (which determines tracker index assignment):
    /// - Decision: true branch first, then false branch
    /// - Switch: case "a" first, then case "b"
    fn build(&self, g: &mut Graph, trackers: &TrackerNodes) {
        match self {
            Fragment::Track => {
                trackers.add(add_tracker(g));
            }
            Fragment::Seq(items) => {
                for item in items {
                    item.build(g, trackers);
                }
            }
            Fragment::Par(branches) => {
                let branch_fns: Vec<_> = branches
                    .iter()
                    .map(|b| {
                        let b = b.clone();
                        let trackers = trackers.clone();
                        branch(move |g| b.build(g, &trackers))
                    })
                    .collect();
                g.add_parallel("par", branch_fns);
            }
            Fragment::Decision { take_true, t, f } => {
                let t = t.clone();
                let f = f.clone();
                let t_trackers = trackers.clone();
                let f_trackers = trackers.clone();

                g.add_boxed_system(Box::new(DecisionSystem {
                    take_true: *take_true,
                }));
                g.add_conditional_branch::<DecisionOutput, _, _, _>(
                    "decision",
                    |output| output.take_true,
                    move |g| t.build(g, &t_trackers),
                    move |g| f.build(g, &f_trackers),
                );
            }
            Fragment::Loop { n, body } => {
                let body = body.clone();
                let trackers = trackers.clone();
                g.add_loop_n("loop", *n, move |g| body.build(g, &trackers));
            }
            Fragment::Switch { case, a, b } => {
                let a = a.clone();
                let b = b.clone();
                let a_trackers = trackers.clone();
                let b_trackers = trackers.clone();

                g.add_boxed_system(Box::new(SwitchKeySystem { key: case }));
                g.add_switch::<SwitchOutput, _, _, _>(
                    "switch",
                    |output| output.key,
                    [
                        ("a", branch(move |g| a.build(g, &a_trackers))),
                        ("b", branch(move |g| b.build(g, &b_trackers))),
                    ],
                    None,
                );
            }
            Fragment::Fallible { handler } => {
                let handler = handler.clone();
                let trackers = trackers.clone();
                g.system_boxed(Box::new(FailingSystem))
                    .on_error(move |g| handler.build(g, &trackers))
                    .done();
            }
        }
    }

    /// Returns `true` if this fragment always diverts execution away from the
    /// sequential path (the failing system takes the error edge, so the
    /// sequential continuation is unreachable).
    fn is_diverting(&self) -> bool {
        match self {
            Fragment::Fallible { .. } => true,
            Fragment::Seq(items) => items.iter().any(Fragment::is_diverting),
            _ => false,
        }
    }

    /// Returns the total number of tracker nodes in this fragment (structural).
    ///
    /// This is independent of branching choices — it counts all `Track` leaves
    /// in both taken and non-taken branches.
    fn tracker_count(&self) -> usize {
        match self {
            Fragment::Track => 1,
            Fragment::Seq(items) => items.iter().map(Fragment::tracker_count).sum(),
            Fragment::Par(branches) => branches.iter().map(Fragment::tracker_count).sum(),
            Fragment::Decision { t, f, .. } => t.tracker_count() + f.tracker_count(),
            Fragment::Loop { body, .. } => body.tracker_count(),
            Fragment::Switch { a, b, .. } => a.tracker_count() + b.tracker_count(),
            Fragment::Fallible { handler } => handler.tracker_count(),
        }
    }

    /// Returns a vector of zeros with length equal to `tracker_count()`.
    ///
    /// Used by `predicted_counts_inner` for non-taken branches: the trackers
    /// exist structurally but should never fire, so their expected count is 0.
    fn zero_counts(&self) -> Vec<usize> {
        vec![0; self.tracker_count()]
    }

    /// Returns per-tracker expected execution counts in registration order.
    ///
    /// The returned `Vec` has one entry per `Track` leaf in the fragment tree,
    /// ordered by the same depth-first traversal used in `build()`. Each entry
    /// is the number of times that tracker should fire during execution.
    fn predicted_counts(&self) -> Vec<usize> {
        self.predicted_counts_inner(1)
    }

    /// Inner recursive helper. `multiplier` accumulates from enclosing loops.
    ///
    /// For example, a `Track` inside `Loop { n: 3, .. }` inside `Loop { n: 2, .. }`
    /// would be called with `multiplier = 6` and return `vec![6]`.
    fn predicted_counts_inner(&self, multiplier: usize) -> Vec<usize> {
        match self {
            Fragment::Track => vec![multiplier],
            Fragment::Seq(items) => {
                let mut counts = Vec::new();
                let mut diverted = false;
                for item in items {
                    if diverted {
                        counts.extend(item.zero_counts());
                    } else {
                        counts.extend(item.predicted_counts_inner(multiplier));
                        if item.is_diverting() {
                            diverted = true;
                        }
                    }
                }
                counts
            }
            Fragment::Par(branches) => branches
                .iter()
                .flat_map(|b| b.predicted_counts_inner(multiplier))
                .collect(),
            Fragment::Decision { take_true, t, f } => {
                if *take_true {
                    let mut counts = t.predicted_counts_inner(multiplier);
                    counts.extend(f.zero_counts());
                    counts
                } else {
                    let mut counts = t.zero_counts();
                    counts.extend(f.predicted_counts_inner(multiplier));
                    counts
                }
            }
            Fragment::Loop { n, body } => body.predicted_counts_inner(multiplier * n),
            Fragment::Switch { case, a, b } => {
                if *case == "a" {
                    let mut counts = a.predicted_counts_inner(multiplier);
                    counts.extend(b.zero_counts());
                    counts
                } else {
                    let mut counts = a.zero_counts();
                    counts.extend(b.predicted_counts_inner(multiplier));
                    counts
                }
            }
            // FailingSystem has no tracker; only the handler body has trackers.
            Fragment::Fallible { handler } => handler.predicted_counts_inner(multiplier),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fragment DSL constructors
// ─────────────────────────────────────────────────────────────────────────────

fn track() -> Fragment {
    Fragment::Track
}

fn seq<I: IntoIterator<Item = Fragment>>(items: I) -> Fragment {
    Fragment::Seq(items.into_iter().collect())
}

fn par<I: IntoIterator<Item = Fragment>>(branches: I) -> Fragment {
    Fragment::Par(branches.into_iter().collect())
}

fn decision(take_true: bool, t: Fragment, f: Fragment) -> Fragment {
    Fragment::Decision {
        take_true,
        t: Box::new(t),
        f: Box::new(f),
    }
}

fn loop_n(n: usize, body: Fragment) -> Fragment {
    Fragment::Loop {
        n,
        body: Box::new(body),
    }
}

fn switch(case: &'static str, a: Fragment, b: Fragment) -> Fragment {
    Fragment::Switch {
        case,
        a: Box::new(a),
        b: Box::new(b),
    }
}

fn fallible(handler: Fragment) -> Fragment {
    Fragment::Fallible {
        handler: Box::new(handler),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TEST RUNNER
// ═══════════════════════════════════════════════════════════════════════════════

/// Executes a fragment, returns the execution log and expected tracker nodes.
async fn run_fragment(fragment: Fragment) -> Result<(ExecutionLog, Vec<NodeId>), ExecutionError> {
    let mut graph = Graph::new();
    let trackers = TrackerNodes::default();
    fragment.build(&mut graph, &trackers);
    let expected = trackers.into_vec();

    let server = create_test_server();
    let hooks = get_hooks(&server);

    let mut ctx = server.create_context();
    let log = ExecutionLog::default();
    ctx.insert(log.clone());
    GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await?;
    Ok((log, expected))
}

// ═══════════════════════════════════════════════════════════════════════════════
// ATOMIC FRAGMENT BUILDERS
// ═══════════════════════════════════════════════════════════════════════════════

/// Sequence of N tracking systems.
fn sequence_n(count: usize) -> Fragment {
    seq((0..count).map(|_| track()))
}

/// Parallel with 2 tracking branches.
fn parallel_2() -> Fragment {
    par([track(), track()])
}

/// Decision that always takes true branch.
fn decision_true() -> Fragment {
    decision(true, track(), track())
}

/// Loop that executes N times with a tracker body.
fn loop_body_n(iterations: usize) -> Fragment {
    loop_n(iterations, track())
}

/// Switch that routes to case "a".
fn switch_to_a() -> Fragment {
    switch("a", track(), track())
}

// ═══════════════════════════════════════════════════════════════════════════════
// COMPOSITE FRAGMENT BUILDERS (Nested Control Flow)
// ═══════════════════════════════════════════════════════════════════════════════

/// before -> parallel([a, b]) -> after
/// Slots: 0 (before), 1 (branch a), 2 (branch b), 3 (after)
fn parallel_with_before_after() -> Fragment {
    seq([track(), par([track(), track()]), track()])
}

/// decision -> `true_branch`: [before -> parallel -> after]
/// Slots: 0..3 (true branch with parallel), 4 (false branch)
fn decision_with_parallel() -> Fragment {
    decision(true, parallel_with_before_after(), track())
}

/// switch -> case "a": [before -> parallel -> after]
/// Slots: 0..3 (case a with parallel), 4 (case b)
fn switch_with_parallel() -> Fragment {
    switch("a", parallel_with_before_after(), track())
}

/// loop(N) -> body: [parallel([a, b])]
/// Slots: 0 (branch a), 1 (branch b) - executed N times each
fn loop_with_parallel(iterations: usize) -> Fragment {
    loop_n(iterations, par([track(), track()]))
}

// ═══════════════════════════════════════════════════════════════════════════════
// COMPLEX FRAGMENT COMPOSITIONS (Multi-level Nesting)
// ═══════════════════════════════════════════════════════════════════════════════

/// Deeply nested structure:
/// decision -> parallel([
///     decision -> parallel -> after,
///     loop(2) -> parallel
/// ]) -> `after_all`
///
/// Tests 3 levels of nesting with mixed control flow types.
/// Slots layout:
/// 0..4: first parallel branch (decision with parallel)
/// 5..6: second parallel branch (loop with parallel, 2 iterations)
/// 7: after all
/// 8: outer false branch (not taken)
fn complex_nested() -> Fragment {
    decision(
        true,
        seq([
            par([decision_with_parallel(), loop_with_parallel(2)]),
            track(), // after_all
        ]),
        track(), // outer false branch
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
// TESTS: Progressively More Complex
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies sequential execution: 3 systems in order.
#[tokio::test]
async fn test_sequence() {
    let (log, nodes) = run_fragment(sequence_n(3)).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1);
    assert_eq!(log.count(&nodes[1]), 1);
    assert_eq!(log.count(&nodes[2]), 1);
    // Verify execution order
    assert_eq!(log.executed(), nodes);
}

/// Verifies parallel execution: both branches run.
#[tokio::test]
async fn test_parallel() {
    let (log, nodes) = run_fragment(parallel_2()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "branch a should execute");
    assert_eq!(log.count(&nodes[1]), 1, "branch b should execute");
}

/// Verifies decision execution: only true branch runs.
#[tokio::test]
async fn test_decision() {
    let (log, nodes) = run_fragment(decision_true()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "true branch should execute");
    assert_eq!(log.count(&nodes[1]), 0, "false branch should not execute");
}

/// Verifies loop execution: body runs N times.
#[tokio::test]
async fn test_loop() {
    let (log, nodes) = run_fragment(loop_body_n(5)).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 5, "loop body should execute 5 times");
}

/// Verifies switch execution: only matched case runs.
#[tokio::test]
async fn test_switch() {
    let (log, nodes) = run_fragment(switch_to_a()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "case a should execute");
    assert_eq!(log.count(&nodes[1]), 0, "case b should not execute");
}

/// Verifies parallel with convergence: before -> parallel -> after all execute.
#[tokio::test]
async fn test_parallel_converges() {
    let (log, nodes) = run_fragment(parallel_with_before_after()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "before should execute");
    assert_eq!(log.count(&nodes[1]), 1, "branch a should execute");
    assert_eq!(log.count(&nodes[2]), 1, "branch b should execute");
    assert_eq!(
        log.count(&nodes[3]),
        1,
        "after should execute (convergence)"
    );
}

/// Verifies decision containing parallel: all inner nodes execute including after.
#[tokio::test]
async fn test_decision_with_nested_parallel() {
    let (log, nodes) = run_fragment(decision_with_parallel()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "before parallel should execute");
    assert_eq!(log.count(&nodes[1]), 1, "parallel branch a should execute");
    assert_eq!(log.count(&nodes[2]), 1, "parallel branch b should execute");
    assert_eq!(log.count(&nodes[3]), 1, "after parallel should execute");
    assert_eq!(log.count(&nodes[4]), 0, "false branch should not execute");
}

/// Verifies switch containing parallel: all inner nodes execute including after.
#[tokio::test]
async fn test_switch_with_nested_parallel() {
    let (log, nodes) = run_fragment(switch_with_parallel()).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "before parallel should execute");
    assert_eq!(log.count(&nodes[1]), 1, "parallel branch a should execute");
    assert_eq!(log.count(&nodes[2]), 1, "parallel branch b should execute");
    assert_eq!(log.count(&nodes[3]), 1, "after parallel should execute");
    assert_eq!(log.count(&nodes[4]), 0, "case b should not execute");
}

/// Verifies loop containing parallel: parallel executes on each iteration.
#[tokio::test]
async fn test_loop_with_nested_parallel() {
    let (log, nodes) = run_fragment(loop_with_parallel(3)).await.unwrap();

    assert_eq!(
        log.count(&nodes[0]),
        3,
        "parallel branch a should execute 3 times"
    );
    assert_eq!(
        log.count(&nodes[1]),
        3,
        "parallel branch b should execute 3 times"
    );
}

/// 3 levels of nesting with mixed control flow.
/// Structure: decision -> parallel([decision->parallel->after, loop->parallel]) -> after
#[tokio::test]
async fn test_complex_nested_composition() {
    let (log, nodes) = run_fragment(complex_nested()).await.unwrap();

    // First parallel branch: decision -> parallel -> after (nodes 0-4)
    assert_eq!(log.count(&nodes[0]), 1, "inner decision: before parallel");
    assert_eq!(log.count(&nodes[1]), 1, "inner decision: parallel branch a");
    assert_eq!(log.count(&nodes[2]), 1, "inner decision: parallel branch b");
    assert_eq!(log.count(&nodes[3]), 1, "inner decision: after parallel");
    assert_eq!(
        log.count(&nodes[4]),
        0,
        "inner decision: false branch not taken"
    );

    // Second parallel branch: loop(2) -> parallel (nodes 5-6, each 2x)
    assert_eq!(
        log.count(&nodes[5]),
        2,
        "loop parallel branch a (2 iterations)"
    );
    assert_eq!(
        log.count(&nodes[6]),
        2,
        "loop parallel branch b (2 iterations)"
    );

    // After the outer parallel
    assert_eq!(log.count(&nodes[7]), 1, "after outer parallel");

    // False branch not taken
    assert_eq!(log.count(&nodes[8]), 0, "outer false branch not taken");
}

/// Verifies that a fallible system routes to its error handler.
#[tokio::test]
async fn test_fallible_handler_executes() {
    let (log, nodes) = run_fragment(fallible(track())).await.unwrap();
    assert_eq!(log.count(&nodes[0]), 1, "error handler tracker should fire");
}

/// Verifies fallible in a sequence: before -> fallible(handler) -> after.
/// The "after" node is unreachable because the error edge diverts execution
/// away from the sequential path (the sequential edge from `FailingSystem` to
/// the after node is only taken on success).
#[tokio::test]
async fn test_fallible_in_sequence() {
    let fragment = seq([track(), fallible(track()), track()]);
    let (log, nodes) = run_fragment(fragment).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "before should execute");
    assert_eq!(log.count(&nodes[1]), 1, "error handler should execute");
    assert_eq!(
        log.count(&nodes[2]),
        0,
        "after should not execute (unreachable after error diversion)"
    );
}

/// Verifies fallible inside a loop: handler fires on each iteration.
#[tokio::test]
async fn test_fallible_in_loop() {
    let fragment = loop_n(3, fallible(track()));
    let (log, nodes) = run_fragment(fragment).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 3, "error handler should fire 3 times");
}

/// Verifies fallible with a multi-node handler body.
#[tokio::test]
async fn test_fallible_with_seq_handler() {
    let fragment = fallible(seq([track(), track()]));
    let (log, nodes) = run_fragment(fragment).await.unwrap();

    assert_eq!(log.count(&nodes[0]), 1, "handler step 1 should fire");
    assert_eq!(log.count(&nodes[1]), 1, "handler step 2 should fire");
}

/// Parameterized depth test: builds N levels of nested decisions, each containing parallel.
#[tokio::test]
async fn test_arbitrary_depth_nesting() {
    const DEPTH: usize = 5;

    /// Recursively builds nested decisions with parallel branches.
    fn nested_decisions(depth: usize) -> Fragment {
        if depth == 0 {
            track()
        } else {
            // decision -> true: parallel([nested, nested]), false: empty seq
            decision(
                true,
                par([nested_decisions(depth - 1), nested_decisions(depth - 1)]),
                seq([]), // Empty false branch
            )
        }
    }

    let result = run_fragment(nested_decisions(DEPTH)).await;

    assert!(
        result.is_ok(),
        "Depth {} nesting failed: {:?}",
        DEPTH,
        result.err()
    );
}

/// Demonstrates arbitrary composition with the Fragment DSL.
#[tokio::test]
async fn test_arbitrary_composition() {
    // Complex nested structure - fully declarative
    let complex = seq([
        track(),
        par([
            decision(true, par([track(), track()]), track()),
            loop_n(2, par([track(), track()])),
        ]),
        track(),
    ]);

    let (log, nodes) = run_fragment(complex).await.unwrap();

    // Verify execution counts:
    // nodes[0]: initial track
    assert_eq!(log.count(&nodes[0]), 1, "initial track");
    // nodes[1-2]: decision true branch parallel
    assert_eq!(log.count(&nodes[1]), 1, "decision true branch a");
    assert_eq!(log.count(&nodes[2]), 1, "decision true branch b");
    // nodes[3]: decision false branch (not taken)
    assert_eq!(log.count(&nodes[3]), 0, "decision false branch not taken");
    // nodes[4-5]: loop parallel (2 iterations each)
    assert_eq!(log.count(&nodes[4]), 2, "loop branch a (2 iterations)");
    assert_eq!(log.count(&nodes[5]), 2, "loop branch b (2 iterations)");
    // nodes[6]: final track
    assert_eq!(log.count(&nodes[6]), 1, "final track");
}

// ═══════════════════════════════════════════════════════════════════════════════
// PROPERTY-BASED TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Property-based tests that generate random `Fragment` trees and verify that
/// execution counts match the predicted counts from `predicted_counts()`.
///
/// ## Property Under Test
///
/// **For all valid fragment trees, `predicted_counts()` equals the execution counts.**
///
/// This tests agreement between two implementations:
/// 1. `predicted_counts()` — a simple recursive prediction model
/// 2. `Fragment::build()` + `GraphExecutor::execute()` — the real implementation
///
/// ## Ground Truth
///
/// This property test alone cannot catch bugs where both implementations have
/// the same error. Ground truth comes from the hand-written unit tests above
/// (`test_loop`, `test_decision`, etc.) which assert hard-coded expected values.
///
/// Given that basic behaviors are verified by unit tests, this property test
/// verifies that **nested compositions** of those behaviors produce consistent
/// results across many randomly generated combinations.
///
/// ## Strategy Design
///
/// `arb_fragment(depth)` generates fragment trees recursively:
/// - **Leaf level** (`depth == 0`): Always produces `Track` — the only leaf type.
/// - **Inner levels** (`depth > 0`): Chooses among all 6 composite types with
///   equal probability. `Track` is excluded at inner levels so every generated
///   tree reaches full depth, preventing trivial trees.
///
/// With 6 composite types at each of 3 levels, running 256 cases provides good
/// coverage of nesting combinations.
///
/// ## Parameterized Branching
///
/// - `Decision`: `take_true` is randomly `true` or `false`, exercising both branches
/// - `Switch`: `case` is randomly `"a"` or `"b"`, exercising both cases
/// - `Loop`: iteration count ranges from 1 to 4
/// - `Seq`: 1 to 3 children
/// - `Par`: 2 to 4 branches
///
/// ## Async Handling
///
/// `proptest` does not natively support async test functions. Each test case
/// creates a `tokio::runtime::Runtime` and uses `block_on()` to run the async
/// `run_fragment` function synchronously within the proptest closure.
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    /// Generates a random `Fragment` tree of the given depth.
    ///
    /// At `depth == 0`, only `Track` leaves are produced.
    /// At `depth > 0`, only composite types are generated (equal weight),
    /// ensuring every tree reaches full depth. Includes `Fallible` to exercise
    /// error edge routing under arbitrary nesting.
    fn arb_fragment(depth: u32) -> BoxedStrategy<Fragment> {
        if depth == 0 {
            Just(Fragment::Track).boxed()
        } else {
            prop_oneof![
                prop::collection::vec(arb_fragment(depth - 1), 1..=3usize).prop_map(Fragment::Seq),
                prop::collection::vec(arb_fragment(depth - 1), 2..=4usize).prop_map(Fragment::Par),
                (
                    any::<bool>(),
                    arb_fragment(depth - 1),
                    arb_fragment(depth - 1)
                )
                    .prop_map(|(b, t, f)| decision(b, t, f)),
                (1..=4usize, arb_fragment(depth - 1)).prop_map(|(n, body)| loop_n(n, body)),
                (
                    prop_oneof![Just("a"), Just("b")],
                    arb_fragment(depth - 1),
                    arb_fragment(depth - 1),
                )
                    .prop_map(|(c, a, b)| switch(c, a, b)),
                arb_fragment(depth - 1).prop_map(fallible),
            ]
            .boxed()
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// For every randomly generated fragment tree, the per-node
        /// execution count must match the prediction from `predicted_counts()`.
        #[test]
        fn prop_per_node_execution_matches_prediction(fragment in arb_fragment(3)) {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                let expected = fragment.predicted_counts();
                let (log, nodes) = run_fragment(fragment).await.expect("execution");
                assert_eq!(nodes.len(), expected.len(), "tracker count mismatch");
                for (i, (node, expected_count)) in nodes.iter().zip(&expected).enumerate() {
                    prop_assert_eq!(
                        log.count(node),
                        *expected_count,
                        "tracker[{}]", i
                    );
                }
                Ok(())
            })?;
        }
    }
}
