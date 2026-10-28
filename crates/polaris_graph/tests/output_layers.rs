//! End-to-end coverage for loop iteration output layers.
//!
//! Every loop iteration writes into its own output layer. These tests check
//! *which* iterations stay visible, not just how many: each write carries a
//! [`ProducerInfo`] whose `loop_iteration` names the iteration that wrote it.

use polaris_graph::ProducerInfo;
use polaris_graph::cancel::CancellationSource;
use polaris_graph::executor::{ExecutionError, GraphExecutor};
use polaris_graph::graph::Graph;
use polaris_graph::node::ContextPolicy;
use polaris_system::param::SystemContext;
use polaris_system::resource::Output;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn score() -> i32 {
    7
}

async fn trace() -> String {
    "trace".to_owned()
}

/// What one run of [`CaptureIterations`] saw: the loop iteration of every
/// visible entry of its type, oldest first.
type Seen = Arc<Mutex<Vec<Vec<u64>>>>;

/// Records, each time it runs, the loop iteration that wrote each visible
/// entry of `T`, oldest first.
struct CaptureIterations<T> {
    seen: Seen,
    marker: PhantomData<T>,
}

impl<T: Output> System for CaptureIterations<T> {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let iterations = ctx
                .outputs()
                .entries::<T>()
                .expect("read output entries")
                .iter()
                .map(|(producer, _)| {
                    ProducerInfo::of(producer)
                        .expect("every entry here was written by a graph node")
                        .loop_iteration()
                })
                .collect();
            self.seen.lock().expect("test mutex").push(iterations);
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "capture_iterations"
    }
}

fn capture<T: Output>(seen: &Seen) -> Box<CaptureIterations<T>> {
    Box::new(CaptureIterations::<T> {
        seen: Arc::clone(seen),
        marker: PhantomData,
    })
}

fn seen(captured: &Seen) -> Vec<Vec<u64>> {
    captured.lock().expect("test mutex").clone()
}

fn entry_count<T: Output>(ctx: &SystemContext<'_>) -> usize {
    ctx.outputs()
        .entries::<T>()
        .map(|entries| entries.len())
        .unwrap_or(0)
}

#[tokio::test]
async fn loop_retention_defaults_to_the_previous_iteration() {
    let scores = Seen::default();
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 3, |body| {
        body.add_system(score)
            .add_boxed_system(capture::<i32>(&scores));
    });

    GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await
        .expect("bounded loop executes");
    // Default retention keeps one completed iteration readable beside the
    // running one, and it is always the newest completed one.
    assert_eq!(seen(&scores), vec![vec![0], vec![0, 1], vec![1, 2]]);
}

#[tokio::test]
async fn loop_retention_keeps_three_iterations_and_type_override_wins() {
    let scores = Seen::default();
    let traces = Seen::default();
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 4, |body| {
        body.add_system(score).add_system(trace);
        body.add_boxed_system(capture::<i32>(&scores));
        body.add_boxed_system(capture::<String>(&traces));
    });
    let loop_id = graph.last_node().expect("loop was just added");
    graph
        .set_loop_retention(loop_id.clone(), 2)
        .set_loop_retention_for_type::<String>(loop_id, 1);

    GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await
        .expect("bounded loop executes");
    assert_eq!(
        seen(&scores),
        vec![vec![0], vec![0, 1], vec![0, 1, 2], vec![1, 2, 3]],
        "the two newest completed iterations stay readable"
    );
    assert_eq!(
        seen(&traces),
        vec![vec![0], vec![0, 1], vec![1, 2], vec![2, 3]],
        "the per-type override keeps only the newest completed iteration"
    );
}

#[tokio::test]
async fn a_retention_of_zero_means_the_same_on_both_setters() {
    let by_default = Seen::default();
    let by_type = Seen::default();

    let mut default_zero = Graph::new();
    default_zero.add_loop_n("loop", 3, |body| {
        body.add_system(score)
            .add_boxed_system(capture::<i32>(&by_default));
    });
    let loop_id = default_zero.last_node().expect("loop was just added");
    default_zero.set_loop_retention(loop_id, 0);

    let mut type_zero = Graph::new();
    type_zero.add_loop_n("loop", 3, |body| {
        body.add_system(score)
            .add_boxed_system(capture::<i32>(&by_type));
    });
    let loop_id = type_zero.last_node().expect("loop was just added");
    type_zero.set_loop_retention_for_type::<i32>(loop_id, 0);

    for graph in [&default_zero, &type_zero] {
        let mut ctx = SystemContext::new();
        GraphExecutor::new()
            .execute(graph, &mut ctx, None, None)
            .await
            .expect("bounded loop executes");
    }

    let only_the_running_iteration = vec![vec![0], vec![1], vec![2]];
    assert_eq!(
        seen(&by_default),
        only_the_running_iteration,
        "set_loop_retention(id, 0) keeps no completed iteration readable"
    );
    assert_eq!(
        seen(&by_type),
        only_the_running_iteration,
        "set_loop_retention_for_type::<T>(id, 0) means the same for T"
    );
}

#[tokio::test]
async fn a_retention_of_zero_still_promotes_the_last_iteration() {
    let after = Seen::default();
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 3, |body| {
        body.add_system(score);
    });
    let loop_id = graph.last_node().expect("loop was just added");
    graph.set_loop_retention(loop_id, 0);
    graph.add_boxed_system(capture::<i32>(&after));

    GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await
        .expect("bounded loop executes");
    assert_eq!(seen(&after), vec![vec![2]]);
}

#[tokio::test]
async fn loop_exit_promotes_only_the_newest_iteration() {
    let after = Seen::default();
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 4, |body| {
        body.add_system(score);
    });
    let loop_id = graph.last_node().expect("loop was just added");
    graph.set_loop_retention(loop_id, 3);
    graph.add_boxed_system(capture::<i32>(&after));

    GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await
        .expect("bounded loop executes");
    assert_eq!(
        seen(&after),
        vec![vec![3]],
        "only the last iteration's entry crosses the loop boundary"
    );
}

#[tokio::test]
async fn nested_loop_promotes_one_layer_into_its_enclosing_iteration() {
    let inner = Seen::default();
    let after_inner = Seen::default();
    let after_outer = Seen::default();
    let mut graph = Graph::new();
    let (inner_c, after_inner_c) = (Arc::clone(&inner), Arc::clone(&after_inner));
    graph.add_loop_n("outer", 2, move |outer| {
        let inner_c = Arc::clone(&inner_c);
        outer.add_loop_n("inner", 2, move |body| {
            body.add_system(score)
                .add_boxed_system(capture::<i32>(&inner_c));
        });
        outer.add_boxed_system(capture::<i32>(&after_inner_c));
    });
    graph.add_boxed_system(capture::<i32>(&after_outer));

    GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await
        .expect("nested loops execute");
    // `loop_iteration` reports the innermost loop's iteration.
    // Outer iteration 0: inner sees [0], then [0, 1]; only inner iteration 1
    // crosses the inner exit.
    // Outer iteration 1: the previous outer iteration's entry (inner
    // iteration 1) stays readable (default retention), so inner sees
    // [1, 0], then [1, 0, 1], and two entries remain after it.
    assert_eq!(
        seen(&inner),
        vec![vec![0], vec![0, 1], vec![1, 0], vec![1, 0, 1]]
    );
    assert_eq!(seen(&after_inner), vec![vec![1], vec![1, 1]]);
    assert_eq!(seen(&after_outer), vec![vec![1]]);
}

#[tokio::test]
async fn zero_iteration_loop_leaves_earlier_outputs_alone() {
    let mut graph = Graph::new();
    graph.add_system(score).add_loop::<i32, _, _>(
        "never",
        |_: &i32| true,
        |body| {
            body.add_system(trace);
        },
    );
    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("zero-iteration loop executes");
    assert_eq!(result.output::<i32>().copied(), Some(7));
    assert_eq!(entry_count::<String>(&ctx), 0);
}

struct FailOnSecondRun {
    runs: Arc<Mutex<u32>>,
}

impl System for FailOnSecondRun {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut runs = self.runs.lock().expect("test mutex");
            *runs += 1;
            if *runs == 2 {
                Err(SystemError::ExecutionError("second run fails".into()))
            } else {
                Ok(())
            }
        })
    }

    fn name(&self) -> &'static str {
        "fail_on_second_run"
    }
}

#[tokio::test]
async fn failed_loop_drops_every_iteration_layer() {
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 3, |body| {
        body.add_system(trace)
            .add_boxed_system(Box::new(FailOnSecondRun {
                runs: Arc::new(Mutex::new(0)),
            }));
    });
    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;
    match result {
        Err(ExecutionError::SystemError(message)) => assert!(
            message.contains("second run fails"),
            "the second iteration's system error fails the loop: {message}"
        ),
        other => panic!("expected the second iteration's SystemError, got {other:?}"),
    }
    assert_eq!(entry_count::<String>(&ctx), 0);
}

#[tokio::test]
async fn a_loop_that_exceeds_max_iterations_drops_every_iteration_layer() {
    let inside = Seen::default();
    let mut graph = Graph::new();
    graph.add_loop::<(), _, _>(
        "never_done",
        |_: &()| false,
        |body| {
            body.add_system(trace)
                .add_boxed_system(capture::<String>(&inside));
        },
    );
    let loop_id = graph.last_node().expect("loop was just added");
    graph.set_loop_max_iterations(loop_id.clone(), 2);

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;
    match result {
        Err(ExecutionError::MaxIterationsExceeded { node, max }) => {
            assert_eq!(node, loop_id);
            assert_eq!(max, 2);
        }
        other => panic!("expected MaxIterationsExceeded, got {other:?}"),
    }
    assert_eq!(
        seen(&inside),
        vec![vec![0], vec![0, 1]],
        "both iterations wrote before the limit was reached"
    );
    assert_eq!(
        entry_count::<String>(&ctx),
        0,
        "no iteration's output leaks out of a loop that hit its limit"
    );
}

/// Raises the run's cancellation signal.
struct RaiseCancellation {
    source: CancellationSource,
}

impl System for RaiseCancellation {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            self.source.cancel();
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "raise_cancellation"
    }
}

#[tokio::test]
async fn a_cancelled_loop_drops_every_iteration_layer() {
    let source = CancellationSource::new();
    let mut graph = Graph::new();
    let raiser = source.clone();
    graph.add_loop_n("loop", 3, move |body| {
        body.add_system(trace)
            .add_boxed_system(Box::new(RaiseCancellation { source: raiser }));
    });

    let mut ctx = SystemContext::new();
    ctx.insert(source.token());
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;
    assert!(
        matches!(result, Err(ExecutionError::Interrupted { .. })),
        "the raised signal stops the loop before its next iteration: {result:?}"
    );
    assert_eq!(
        entry_count::<String>(&ctx),
        0,
        "the iteration that ran before the stop does not leak its output"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Retention survives every builder that merges one graph into another
//
// A loop's retention is recorded on the graph its builder closure received.
// Each builder below flattens that temporary graph into its parent, and
// `append` flattens a whole subgraph, so each one has to carry the retention
// along. A loop that lost it would fall back to keeping one iteration:
// `[[0], [0, 1], [1, 2], [2, 3]]` for both types in `add_retained_loop`.
// ─────────────────────────────────────────────────────────────────────────────

async fn seed() -> u8 {
    1
}

struct Route;

async fn route() -> Route {
    Route
}

/// A fallible system that always fails, so an error handler is wired to it
/// and then runs.
struct AlwaysFails;

impl System for AlwaysFails {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Err(SystemError::ExecutionError("always fails".into())) })
    }

    fn name(&self) -> &'static str {
        "always_fails"
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

async fn slow() -> i32 {
    tokio::time::sleep(Duration::from_secs(10)).await;
    1
}

type CaseBuilder<'a> = Box<dyn FnOnce(&mut Graph) + 'a>;

/// Adds a four-iteration loop to `graph` and configures its retention on
/// `graph` itself: two iterations by default, none for `String`.
fn add_retained_loop(graph: &mut Graph, scores: &Seen, traces: &Seen) {
    graph.add_loop_n("retained", 4, |body| {
        body.add_system(score).add_system(trace);
        body.add_boxed_system(capture::<i32>(scores));
        body.add_boxed_system(capture::<String>(traces));
    });
    let loop_id = graph.last_node().expect("loop was just added");
    graph
        .set_loop_retention(loop_id.clone(), 2)
        .set_loop_retention_for_type::<String>(loop_id, 0);
}

async fn run(graph: &Graph) {
    GraphExecutor::new()
        .execute(graph, &mut SystemContext::new(), None, None)
        .await
        .expect("graph executes");
}

fn assert_configured_retention_applied(scores: &Seen, traces: &Seen) {
    assert_eq!(
        seen(scores),
        vec![vec![0], vec![0, 1], vec![0, 1, 2], vec![1, 2, 3]],
        "the loop's default of two iterations applies"
    );
    assert_eq!(
        seen(traces),
        vec![vec![0], vec![1], vec![2], vec![3]],
        "the loop's per-type override of zero applies"
    );
}

#[tokio::test]
async fn retention_set_on_an_appended_subgraph_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut subgraph = Graph::new();
    add_retained_loop(&mut subgraph, &scores, &traces);

    let mut graph = Graph::new();
    graph.add_system(seed);
    graph.append(subgraph).expect("subgraph appends");

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_on_a_subgraph_appended_to_an_empty_graph_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut subgraph = Graph::new();
    add_retained_loop(&mut subgraph, &scores, &traces);

    let mut graph = Graph::new();
    graph.append(subgraph).expect("subgraph appends");

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_survives_appending_through_a_second_graph() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut inner = Graph::new();
    add_retained_loop(&mut inner, &scores, &traces);
    let mut middle = Graph::new();
    middle.add_system(seed);
    middle.append(inner).expect("inner subgraph appends");

    let mut graph = Graph::new();
    graph.append(middle).expect("middle subgraph appends");

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_on_a_subgraph_nested_in_a_loop_body_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut subgraph = Graph::new();
    add_retained_loop(&mut subgraph, &scores, &traces);

    let mut graph = Graph::new();
    graph.add_loop_n("outer", 1, |body| {
        body.append(subgraph).expect("subgraph appends");
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_bounded_loop_body_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_loop_n("outer", 1, |body| {
        add_retained_loop(body, &scores, &traces);
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_predicate_loop_body_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let polls = Arc::new(AtomicUsize::new(0));
    let mut graph = Graph::new();
    graph.add_system(seed);
    // The predicate is polled before every iteration: false once, then true.
    graph.add_loop::<u8, _, _>(
        "outer",
        move |_| polls.fetch_add(1, Ordering::SeqCst) >= 1,
        |body| add_retained_loop(body, &scores, &traces),
    );

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_the_true_branch_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_conditional_branch::<Route, _, _, _>(
        "pick",
        |_| true,
        |branch| add_retained_loop(branch, &scores, &traces),
        |_| {},
    );

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_the_false_branch_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_conditional_branch::<Route, _, _, _>(
        "pick",
        |_| false,
        |_| {},
        |branch| add_retained_loop(branch, &scores, &traces),
    );

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_parallel_branch_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_parallel(
        "fan_out",
        [|branch: &mut Graph| add_retained_loop(branch, &scores, &traces)],
    );

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_switch_case_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let case: CaseBuilder<'_> = Box::new(|case| add_retained_loop(case, &scores, &traces));
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_switch::<Route, _, _, _>("route", |_| "go", vec![("go", case)], None);

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_the_switch_default_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let unmatched: CaseBuilder<'_> = Box::new(|_| {});
    let default: CaseBuilder<'_> = Box::new(|case| add_retained_loop(case, &scores, &traces));
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_switch::<Route, _, _, _>(
        "route",
        |_| "no_case_has_this_key",
        vec![("go", unmatched)],
        Some(default),
    );

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_blanket_error_handler_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(AlwaysFails));
    graph.add_error_handler(|handler| {
        add_retained_loop(handler, &scores, &traces);
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_targeted_error_handler_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    let failing = graph.add_boxed_system(Box::new(AlwaysFails));
    graph.add_error_handler_for([failing], |handler| {
        add_retained_loop(handler, &scores, &traces);
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_inside_a_timeout_handler_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    let slow_id = graph.add_system_node(slow);
    graph.set_timeout(slow_id.clone(), Duration::from_millis(10));
    graph.add_timeout_handler([slow_id], |handler| {
        add_retained_loop(handler, &scores, &traces);
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_survives_several_levels_of_nesting() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_loop_n("outer", 1, |body| {
        body.add_switch::<Route, _, _, _>(
            "route",
            |_| "go",
            vec![("go", |case: &mut Graph| {
                case.add_parallel(
                    "fan_out",
                    [|branch: &mut Graph| add_retained_loop(branch, &scores, &traces)],
                );
            })],
            None,
        );
    });

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_set_on_a_scope_graph_applies() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut inner = Graph::new();
    add_retained_loop(&mut inner, &scores, &traces);
    let mut graph = Graph::new();
    graph.add_scope("scope", inner, ContextPolicy::shared());

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
}

#[tokio::test]
async fn retention_survives_duplicating_a_merged_graph() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let mut subgraph = Graph::new();
    add_retained_loop(&mut subgraph, &scores, &traces);
    let mut graph = Graph::new();
    graph.add_system(seed);
    graph.append(subgraph).expect("subgraph appends");

    run(&graph.duplicate()).await;
    assert_configured_retention_applied(&scores, &traces);
}

// Parallel branches run in their own contexts, so loops in sibling branches do
// not see each other's outputs; the two tests below use that to merge several
// loops into one graph without one loop's promoted outputs reaching another.

#[tokio::test]
async fn retention_of_several_merged_loops_applies_to_each() {
    let (first_scores, first_traces) = (Seen::default(), Seen::default());
    let (second_scores, second_traces) = (Seen::default(), Seen::default());
    let first: CaseBuilder<'_> =
        Box::new(|branch| add_retained_loop(branch, &first_scores, &first_traces));
    let second: CaseBuilder<'_> =
        Box::new(|branch| add_retained_loop(branch, &second_scores, &second_traces));
    let mut graph = Graph::new();
    graph.add_parallel("fan_out", vec![first, second]);

    run(&graph).await;
    assert_configured_retention_applied(&first_scores, &first_traces);
    assert_configured_retention_applied(&second_scores, &second_traces);
}

#[tokio::test]
async fn an_unconfigured_loop_keeps_the_default_beside_a_merged_configured_one() {
    let (scores, traces) = (Seen::default(), Seen::default());
    let (plain_scores, plain_traces) = (Seen::default(), Seen::default());
    let configured: CaseBuilder<'_> =
        Box::new(|branch| add_retained_loop(branch, &scores, &traces));
    let unconfigured: CaseBuilder<'_> = Box::new(|branch| {
        branch.add_loop_n("plain", 4, |body| {
            body.add_system(score).add_system(trace);
            body.add_boxed_system(capture::<i32>(&plain_scores));
            body.add_boxed_system(capture::<String>(&plain_traces));
        });
    });
    let mut graph = Graph::new();
    graph.add_parallel("fan_out", vec![configured, unconfigured]);

    run(&graph).await;
    assert_configured_retention_applied(&scores, &traces);
    let default_of_one = vec![vec![0], vec![0, 1], vec![1, 2], vec![2, 3]];
    assert_eq!(
        seen(&plain_scores),
        default_of_one,
        "merging a configured loop does not change a neighbour's retention"
    );
    assert_eq!(seen(&plain_traces), default_of_one);
}

// `set_loop_max_iterations` is stored on the loop node, so it moves with the
// node; these keep that from regressing.

#[tokio::test]
async fn loop_max_iterations_set_on_an_appended_subgraph_applies() {
    let mut subgraph = Graph::new();
    subgraph.add_loop::<(), _, _>(
        "never_done",
        |_: &()| false,
        |body| {
            body.add_system(trace);
        },
    );
    let loop_id = subgraph.last_node().expect("loop was just added");
    subgraph.set_loop_max_iterations(loop_id.clone(), 2);

    let mut graph = Graph::new();
    graph.add_system(seed);
    graph.append(subgraph).expect("subgraph appends");

    let result = GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await;
    match result {
        Err(ExecutionError::MaxIterationsExceeded { node, max }) => {
            assert_eq!(node, loop_id);
            assert_eq!(max, 2);
        }
        other => panic!("expected MaxIterationsExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn loop_max_iterations_set_inside_a_branch_applies() {
    let mut graph = Graph::new();
    graph.add_system(route);
    graph.add_conditional_branch::<Route, _, _, _>(
        "pick",
        |_| true,
        |branch| {
            branch.add_loop::<(), _, _>(
                "never_done",
                |_: &()| false,
                |body| {
                    body.add_system(trace);
                },
            );
            let loop_id = branch.last_node().expect("loop was just added");
            branch.set_loop_max_iterations(loop_id, 3);
        },
        |_| {},
    );

    let result = GraphExecutor::new()
        .execute(&graph, &mut SystemContext::new(), None, None)
        .await;
    assert!(
        matches!(
            result,
            Err(ExecutionError::MaxIterationsExceeded { max: 3, .. })
        ),
        "the branch's limit of three applies: {result:?}"
    );
}
