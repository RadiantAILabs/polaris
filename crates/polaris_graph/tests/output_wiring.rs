//! Executed-graph and validation coverage for `read_from`: binding a
//! reader's `Out<T>` to one specific producer node, chosen by the graph.

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::{Graph, ValidationError, ValidationResult};
use polaris_graph::node::{ContextPolicy, RetryPolicy};
use polaris_system::param::{
    ErrOut, ErrorContext, Out, OutAll, SystemAccess, SystemContext, SystemParam,
};
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Val(i32);

async fn produce_a() -> Val {
    Val(1)
}

async fn produce_b() -> Val {
    Val(2)
}

#[polaris_system::system]
async fn read_val(v: Out<Val>) -> i32 {
    v.0
}

/// Returns the one error `result` reports, failing the test if it reports
/// any other number of errors.
fn only_error(result: &ValidationResult) -> &ValidationError {
    assert_eq!(result.errors().len(), 1, "{result}");
    &result.errors()[0]
}

// ─────────────────────────────────────────────────────────────────────────────
// Sequential; two producers of `T`, reader gets the wired one
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reader_gets_the_wired_producers_value_not_the_newest_one() {
    let mut graph = Graph::new();
    let a = graph.system(produce_a).id();
    graph.add_system(produce_b);
    graph.system(read_val).read_from::<Val>(a);
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<i32>(),
        Some(&1),
        "the wired read resolves to produce_a's value even though produce_b \
         wrote a newer entry of the same type"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Decision: one arm can read a sibling's producer only across a `read_from`;
// structurally, cross-arm wiring is rejected, and a wire to a producer from
// before the decision is admitted in either arm.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn decision_reader_cannot_read_a_sibling_arms_producer() {
    let mut graph = Graph::new();
    let mut producer = None;
    let mut reader = None;
    graph.add_conditional_branch::<Val, _, _, _>(
        "dec",
        |v| v.0 > 0,
        |g| {
            producer = Some(g.system(produce_a).id());
        },
        |g| {
            reader = Some(g.system(read_val).id());
        },
    );
    let (reader, producer) = (reader.unwrap(), producer.unwrap());
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == reader && *p == producer
        ),
        "{result}"
    );
}

#[tokio::test]
async fn decision_reader_can_read_a_producer_from_before_the_decision_in_either_arm() {
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let mut reader = None;
    graph.add_conditional_branch::<Val, _, _, _>(
        "dec",
        |v| v.0 > 0,
        |g| {
            reader = Some(g.system(read_val).id());
        },
        |g| {
            g.add_system(produce_b);
        },
    );
    graph.read_from::<Val>(reader.unwrap(), producer);
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<i32>(), Some(&1));
}

// ─────────────────────────────────────────────────────────────────────────────
// Switch: same two rules as decision, exercised with multiple cases and a
// default arm ("all-arm").
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn switch_reader_in_one_case_cannot_read_a_sibling_cases_producer() {
    let mut graph = Graph::new();
    let mut producer = None;
    let mut reader = None;
    graph.add_switch::<Val, _, _, _>(
        "sw",
        |_| "a",
        vec![
            (
                "a",
                Box::new(|g: &mut Graph| {
                    producer = Some(g.system(produce_a).id());
                }) as Box<dyn FnOnce(&mut Graph)>,
            ),
            (
                "b",
                Box::new(|g: &mut Graph| {
                    reader = Some(g.system(read_val).id());
                }),
            ),
        ],
        Some(Box::new(|g: &mut Graph| {
            g.add_system(produce_b);
        })),
    );
    let (reader, producer) = (reader.unwrap(), producer.unwrap());
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == reader && *p == producer
        ),
        "{result}"
    );
}

#[test]
fn switch_reader_after_the_switch_cannot_read_a_producer_confined_to_one_case() {
    // Every case and the default arm are dead ends unless the switch itself
    // is chained past — a producer confined to just one of them is not
    // guaranteed once we're back on the parent chain.
    let mut graph = Graph::new();
    let mut producer = None;
    graph.add_switch::<Val, _, _, _>(
        "sw",
        |_| "a",
        vec![(
            "a",
            Box::new(|g: &mut Graph| {
                producer = Some(g.system(produce_a).id());
            }) as Box<dyn FnOnce(&mut Graph)>,
        )],
        Some(Box::new(|g: &mut Graph| {
            g.add_system(produce_b);
        })),
    );
    let reader = graph.system(read_val).id();
    let producer = producer.unwrap();
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == reader && *p == producer
        ),
        "{result}"
    );
}

#[tokio::test]
async fn switch_reader_in_every_case_can_read_a_producer_from_before_the_switch() {
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let readers = Arc::new(Mutex::new(Vec::new()));
    let readers_a = Arc::clone(&readers);
    let readers_b = Arc::clone(&readers);
    let readers_default = Arc::clone(&readers);
    graph.add_switch::<Val, _, _, _>(
        "sw",
        |_| "a",
        vec![
            (
                "a",
                Box::new(move |g: &mut Graph| {
                    readers_a.lock().unwrap().push(g.system(read_val).id());
                }) as Box<dyn FnOnce(&mut Graph)>,
            ),
            (
                "b",
                Box::new(move |g: &mut Graph| {
                    readers_b.lock().unwrap().push(g.system(read_val).id());
                }),
            ),
        ],
        Some(Box::new(move |g: &mut Graph| {
            readers_default
                .lock()
                .unwrap()
                .push(g.system(read_val).id());
        })),
    );
    let readers = readers.lock().unwrap().clone();
    for reader in readers {
        graph.read_from::<Val>(reader, producer.clone());
    }
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<i32>(), Some(&1));
}

// ─────────────────────────────────────────────────────────────────────────────
// Parallel: no sibling reads before join; a producer inside a branch is
// guaranteed once merged back after the join.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn parallel_reader_cannot_read_a_sibling_branchs_producer() {
    let mut graph = Graph::new();
    let mut producer = None;
    let mut reader = None;
    graph.add_parallel(
        "par",
        [
            Box::new(|g: &mut Graph| {
                producer = Some(g.system(produce_a).id());
            }) as Box<dyn FnOnce(&mut Graph)>,
            Box::new(|g: &mut Graph| {
                reader = Some(g.system(read_val).id());
            }),
        ],
    );
    let (reader, producer) = (reader.unwrap(), producer.unwrap());
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == reader && *p == producer
        ),
        "{result}"
    );
}

#[test]
fn parallel_reader_inside_a_branch_cannot_read_a_producer_from_before_the_fork() {
    // Each branch gets a fresh, isolated output store, so a producer from
    // before the fork is not visible to a reader inside a branch either.
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let mut reader = None;
    graph.add_parallel(
        "par",
        [Box::new(|g: &mut Graph| {
            reader = Some(g.system(read_val).id());
        }) as Box<dyn FnOnce(&mut Graph)>],
    );
    let reader = reader.unwrap();
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == reader && *p == producer
        ),
        "{result}"
    );
}

#[tokio::test]
async fn parallel_reader_after_the_join_can_read_a_producer_from_inside_a_branch() {
    let mut graph = Graph::new();
    let mut producer = None;
    graph.add_parallel(
        "par",
        [Box::new(|g: &mut Graph| {
            producer = Some(g.system(produce_a).id());
        }) as Box<dyn FnOnce(&mut Graph)>],
    );
    let reader = graph.system(read_val).id();
    graph.read_from::<Val>(reader, producer.unwrap());
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<i32>(), Some(&1));
}

// ─────────────────────────────────────────────────────────────────────────────
// Retry: a wire composes normally with a retry policy on the producer.
// ─────────────────────────────────────────────────────────────────────────────

struct FlakyProducer {
    calls: Arc<Mutex<u32>>,
}

impl System for FlakyProducer {
    type Output = Val;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                Err(SystemError::ExecutionError("transient".into()))
            } else {
                Ok(Val(42))
            }
        })
    }

    fn name(&self) -> &'static str {
        "flaky_producer"
    }
}

#[tokio::test]
async fn reader_gets_the_wired_producers_value_after_a_retry() {
    let mut graph = Graph::new();
    let producer = graph
        .system_boxed(Box::new(FlakyProducer {
            calls: Arc::new(Mutex::new(0)),
        }))
        .with_retry(RetryPolicy::fixed(1, Duration::from_millis(1)))
        .id();
    graph.system(read_val).read_from::<Val>(producer);
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<i32>(),
        Some(&42),
        "the wire resolves to the retried producer's successful value"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Error handler: a reader inside a failing node's handler can read a
// producer from before that node.
// ─────────────────────────────────────────────────────────────────────────────

struct AlwaysFails;

impl System for AlwaysFails {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Err(SystemError::ExecutionError("boom".into())) })
    }

    fn name(&self) -> &'static str {
        "always_fails"
    }
}

#[tokio::test]
async fn error_handler_reader_can_read_a_producer_from_before_the_failing_node() {
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let risky = graph.system_boxed(Box::new(AlwaysFails)).id();
    let mut reader = None;
    graph.add_error_handler_for([risky], |g| {
        reader = Some(g.system(read_val).id());
    });
    graph.read_from::<Val>(reader.unwrap(), producer);
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<i32>(),
        Some(&1),
        "the error handler's reader resolves the wire to the pre-failure producer"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scopes: wiring validates recursively inside a scope's embedded graph, and
// a wire cannot cross into a different `Graph` instance.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reader_and_producer_can_be_wired_inside_a_shared_scope() {
    let mut inner = Graph::new();
    let producer = inner.system(produce_a).id();
    inner.add_system(produce_b);
    let reader = inner.system(read_val).id();
    inner.read_from::<Val>(reader, producer);

    let mut graph = Graph::new();
    graph.add_scope("scope", inner, ContextPolicy::shared());
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(result.output::<i32>(), Some(&1));
}

#[test]
fn read_from_cannot_reference_a_producer_in_a_different_graph_instance() {
    let mut outer = Graph::new();
    let outer_producer = outer.system(produce_a).id();

    let mut inner = Graph::new();
    let reader = inner.system(read_val).id();
    // `outer_producer` is a valid `NodeId` value, but it names a node that
    // lives in a different `Graph` instance than `inner` — `read_from` takes
    // a bare `NodeId`, so nothing stops a caller from passing one across
    // graphs; validation is what catches it.
    inner.read_from::<Val>(reader.clone(), outer_producer.clone());

    outer.add_scope("scope", inner, ContextPolicy::shared());
    let scope = outer.last_node().unwrap();

    let result = outer.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ScopeGraphInvalid { node, inner, .. }
                if *node == scope
                    && matches!(
                        &**inner,
                        ValidationError::ReadFromProducerMissing { reader: r, producer: p, .. }
                            if *r == reader && *p == outer_producer
                    )
        ),
        "{result}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Duplicated graph: `Graph::duplicate` remaps wiring bindings along with node
// ids, so a clone still wires and executes correctly.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicate_remaps_read_from_bindings() {
    let mut original = Graph::new();
    let producer = original.system(produce_a).id();
    original.add_system(produce_b);
    original.system(read_val).read_from::<Val>(producer);
    assert!(original.validate().is_ok(), "{}", original.validate());

    let clone = original.duplicate();
    assert!(clone.validate().is_ok(), "{}", clone.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&clone, &mut ctx, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.output::<i32>(),
        Some(&1),
        "the clone's remapped wire still resolves to produce_a's value"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Loop backward wire: reads the producer's entry from an earlier iteration.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counter(u32);

async fn seed_counter() -> Counter {
    Counter(0)
}

#[polaris_system::system]
async fn advance(counter: Out<Counter>) -> Counter {
    Counter(counter.0 + 1)
}

/// Records what it saw on `Option<Out<Counter>>`, wired backward to
/// `advance`'s entry from the previous iteration.
struct RememberPrevious {
    seen: Arc<Mutex<Vec<Option<Counter>>>>,
}

impl System for RememberPrevious {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let previous = <Option<Out<Counter>> as SystemParam>::fetch(ctx)
                .map_err(SystemError::ParamError)?;
            self.seen.lock().unwrap().push(previous.map(|v| *v));
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "remember_previous"
    }

    fn access(&self) -> SystemAccess {
        <Option<Out<Counter>> as SystemParam>::access()
    }
}

#[test]
fn loop_backward_wire_is_admitted_by_validate() {
    let mut graph = Graph::new();
    graph.add_system(seed_counter);
    let seen = Arc::new(Mutex::new(Vec::new()));
    graph.add_loop_n("loop", 3, |g| {
        let remember = g
            .system_boxed(Box::new(RememberPrevious {
                seen: Arc::clone(&seen),
            }))
            .id();
        let advance_id = g.system(advance).id();
        g.read_from::<Counter>(remember, advance_id);
    });

    assert!(graph.validate().is_ok(), "{}", graph.validate());
}

#[test]
fn backward_wire_outside_a_loop_is_rejected() {
    // Same shape as the loop body above, but the two nodes are not inside
    // any loop — decision 6's exception does not apply, so the ordinary
    // forward-guarantee failure stands.
    let mut graph = Graph::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let remember = graph.system_boxed(Box::new(RememberPrevious { seen })).id();
    let advance_id = graph.system(advance).id();
    graph.read_from::<Counter>(remember.clone(), advance_id.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromNotGuaranteed { reader: r, producer: p, .. }
                if *r == remember && *p == advance_id
        ),
        "{result}"
    );
}

#[tokio::test]
async fn loop_backward_wire_reads_the_previous_iterations_entry() {
    let mut graph = Graph::new();
    graph.add_system(seed_counter);
    let seen = Arc::new(Mutex::new(Vec::new()));
    graph.add_loop_n("loop", 3, |g| {
        let remember = g
            .system_boxed(Box::new(RememberPrevious {
                seen: Arc::clone(&seen),
            }))
            .id();
        let advance_id = g.system(advance).id();
        g.read_from::<Counter>(remember, advance_id);
    });
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        vec![None, Some(Counter(1)), Some(Counter(2))],
        "iteration 0 sees no prior write from `advance`; each later \
         iteration sees the previous iteration's value, since `remember` \
         runs before `advance` updates its entry for the current iteration"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// `validate_output_wiring` checks beyond producer resolution: a backward wire
// must be read optionally, a binding must name a type the reader actually
// reads through `Out<T>`/`Option<Out<T>>`, and `OutAll<T>`/`ErrOut<T>` do not
// make a type bindable even though they also read `outputs`.
// ─────────────────────────────────────────────────────────────────────────────

#[polaris_system::system]
async fn require_previous_counter(counter: Out<Counter>) -> i32 {
    counter.0 as i32
}

#[test]
fn backward_wire_through_out_without_option_is_rejected() {
    // Same shape as `loop_backward_wire_is_admitted_by_validate`, but the
    // reader binds through `Out<Counter>` (required) instead of
    // `Option<Out<Counter>>` (tolerates the missing first-iteration entry).
    let mut graph = Graph::new();
    graph.add_system(seed_counter);
    let mut wire = None;
    graph.add_loop_n("loop", 3, |g| {
        let reader = g.system(require_previous_counter).id();
        let advance_id = g.system(advance).id();
        g.read_from::<Counter>(reader.clone(), advance_id.clone());
        wire = Some((reader, advance_id));
    });
    let (reader, advance_id) = wire.unwrap();

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromBackwardRequiresOptional { reader: r, producer: p, .. }
                if *r == reader && *p == advance_id
        ),
        "{result}"
    );
}

#[test]
fn binding_on_a_type_the_reader_never_reads_is_rejected() {
    // `produce_b` takes no parameters, so it does not read `Val` through
    // `Out`/`Option<Out>` — a binding naming it as a reader is a mistake
    // (typo'd type, wrong node) the graph should catch at validate() time.
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let reader = graph.system(produce_b).id();
    graph.read_from::<Val>(reader.clone(), producer);

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromUnread { reader: r, .. } if *r == reader
        ),
        "{result}"
    );
}

#[polaris_system::system]
async fn sum_vals(all: OutAll<Val>) -> i32 {
    all.iter().map(|entry| entry.0).sum()
}

#[test]
fn binding_on_a_type_read_only_through_out_all_is_rejected() {
    // `OutAll<T>` returns every visible producer's entry; pinning it to one
    // producer would contradict its purpose, so it does not make the type
    // bindable — the binding here would have no effect.
    let mut graph = Graph::new();
    let producer = graph.system(produce_a).id();
    let reader = graph.system(sum_vals).id();
    graph.read_from::<Val>(reader.clone(), producer);

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromUnread { reader: r, .. } if *r == reader
        ),
        "{result}"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FailVal(i32);

impl ErrorContext for FailVal {}

async fn produce_fail_val() -> FailVal {
    FailVal(9)
}

#[polaris_system::system]
async fn read_fail_val(v: ErrOut<FailVal>) -> i32 {
    v.0
}

#[test]
fn binding_on_a_type_read_only_through_err_out_is_rejected() {
    // `ErrOut<T>` always reads the newest entry regardless of any installed
    // binding, so it does not make the type bindable either — same
    // `ReadFromUnread` outcome as the `OutAll<T>`-only case above.
    let mut graph = Graph::new();
    let producer = graph.system(produce_fail_val).id();
    let reader = graph.system(read_fail_val).id();
    graph.read_from::<FailVal>(reader.clone(), producer);

    // `ErrOut<T>` also needs an error edge into the reader, which this graph
    // does not have, so validation reports that alongside the wiring error.
    let result = graph.validate();
    assert_eq!(result.errors().len(), 2, "{result}");
    assert!(
        matches!(
            &result.errors()[0],
            ValidationError::ReadFromUnread { reader: r, .. } if *r == reader
        ),
        "{result}"
    );
    assert!(
        matches!(
            &result.errors()[1],
            ValidationError::MissingEdgeRequirement { node, .. } if *node == reader
        ),
        "{result}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Producer checks: a binding must name a system node other than the reader,
// and that system's output type must be the bound type.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn reader_wired_to_itself_is_rejected() {
    let mut graph = Graph::new();
    graph.add_system(produce_a);
    let reader = graph.system(read_val).id();
    graph.read_from::<Val>(reader.clone(), reader.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromSelf { node, .. } if *node == reader
        ),
        "{result}"
    );
}

#[test]
fn reader_wired_to_a_scope_node_is_rejected() {
    // The scope's embedded graph produces `Val`, but the scope node itself
    // is not a system, so it cannot be named as a producer.
    let mut inner = Graph::new();
    inner.add_system(produce_a);

    let mut graph = Graph::new();
    graph.add_scope("scope", inner, ContextPolicy::shared());
    let scope = graph.last_node().unwrap();
    let reader = graph.system(read_val).id();
    graph.read_from::<Val>(reader.clone(), scope.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromProducerNotASystem { reader: r, producer: p, .. }
                if *r == reader && *p == scope
        ),
        "{result}"
    );
}

#[test]
fn reader_wired_to_a_loop_node_is_rejected() {
    // The loop body produces `Val`, but the loop node itself is not a
    // system, so it cannot be named as a producer.
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 2, |g| {
        g.add_system(produce_a);
    });
    let loop_node = graph.last_node().unwrap();
    let reader = graph.system(read_val).id();
    graph.read_from::<Val>(reader.clone(), loop_node.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromProducerNotASystem { reader: r, producer: p, .. }
                if *r == reader && *p == loop_node
        ),
        "{result}"
    );
}

#[test]
fn reader_wired_to_a_producer_of_another_type_is_rejected() {
    // `read_val` reads `Val`, but `seed_counter` outputs `Counter`.
    let mut graph = Graph::new();
    let producer = graph.system(seed_counter).id();
    let reader = graph.system(read_val).id();
    graph.read_from::<Val>(reader.clone(), producer.clone());

    let result = graph.validate();
    assert!(
        matches!(
            only_error(&result),
            ValidationError::ReadFromTypeMismatch {
                reader: r,
                producer: p,
                expected,
                actual,
                ..
            } if *r == reader
                && *p == producer
                && *expected == std::any::type_name::<Val>()
                && *actual == std::any::type_name::<Counter>()
        ),
        "{result}"
    );
}
