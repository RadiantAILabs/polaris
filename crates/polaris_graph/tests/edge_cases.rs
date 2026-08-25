//! Edge case tests for graph execution.
//!
//! Tests covering error handling, timeouts, parallel failures, loop termination,
//! output chaining, recursion limits, and switch edge cases.

mod test_utils;

use polaris_graph::NodeId;
use polaris_graph::executor::{ErrorKind, ExecutionError, GraphExecutor};
use polaris_graph::graph::{Graph, GraphSignature};
use polaris_graph::hooks::schedule::{OnGraphFailure, OnGraphStart};
use polaris_graph::hooks::{GraphEvent, HooksAPI};
use polaris_graph::node::{ContextPolicy, DynamicSlot, RetryPolicy};
use polaris_graph::predicate::PredicateError;
use polaris_system::param::SystemContext;
use polaris_system::resource::LocalResource;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use test_utils::{
    ConsumerSystem, DecisionOutput, DecisionSystem, ErrorKindLog, EventuallySucceedsSystem,
    FailingSystem, FlagSystem, HandlerLog, HandlerSystem, InitialStateSystem, KindCheckingHandler,
    LoopIterationSystem, LoopState, ParamFailingSystem, ProducerOutput, ProducerSystem, SlowSystem,
    SuccessSystem, SwitchKeySystem, SwitchOutput, TestConfig, branch, create_test_server,
    get_hooks,
};

/// Asserts the structural precondition shared by runtime graph tests.
fn assert_graph_valid(graph: &Graph) {
    let validation = graph.validate();
    assert!(
        validation.is_ok(),
        "graph should be structurally valid before execution: {:?}",
        validation.errors
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// ERROR HANDLING TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that an error edge routes execution to the handler when a system fails.
#[tokio::test]
async fn error_handler_invoked_on_failure() {
    let mut graph = Graph::new();

    // Add a failing system
    let failing_id = graph.add_boxed_system(Box::new(FailingSystem));

    // Add error handler
    graph.add_error_handler_for(failing_id, |g| {
        g.add_boxed_system(Box::new(HandlerSystem));
    });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // Insert HandlerLog to track if handler was invoked
    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed via error handler");
    assert!(log.was_invoked(), "error handler should have been invoked");
}

/// Verifies that errors propagate when no error handler is present.
#[tokio::test]
async fn error_propagates_without_handler() {
    let mut graph = Graph::new();

    // Add a failing system with no error handler
    graph.add_boxed_system(Box::new(FailingSystem));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_err(),
        "execution should fail without error handler"
    );
    match result {
        Err(ExecutionError::SystemError(msg)) => {
            assert!(
                msg.contains("intentional failure"),
                "error message should contain failure reason"
            );
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected error, got success"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TIMEOUT TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that a timeout edge routes execution to the handler when a system times out.
#[tokio::test]
async fn timeout_triggers_handler() {
    use std::time::Duration;

    let mut graph = Graph::new();

    // Add a slow system that will timeout
    let slow_id = graph.add_boxed_system(Box::new(SlowSystem {
        duration: Duration::from_secs(10), // Long duration
    }));

    // Set a short timeout
    graph.set_timeout(slow_id.clone(), Duration::from_millis(10));

    // Add timeout handler
    graph.add_timeout_handler(slow_id.clone(), |g| {
        g.add_boxed_system(Box::new(HandlerSystem));
    });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // Insert HandlerLog to track if handler was invoked
    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_ok(),
        "execution should succeed via timeout handler"
    );
    assert!(
        log.was_invoked(),
        "timeout handler should have been invoked"
    );
}

/// Verifies that timeout returns an error when no timeout handler is present.
#[tokio::test]
async fn timeout_error_without_handler() {
    use std::time::Duration;

    let mut graph = Graph::new();

    // Add a slow system that will timeout
    let slow_id = graph.add_boxed_system(Box::new(SlowSystem {
        duration: Duration::from_secs(10), // Long duration
    }));

    // Set a short timeout with no handler
    graph.set_timeout(slow_id.clone(), Duration::from_millis(10));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_err(),
        "execution should fail without timeout handler"
    );
    match result {
        Err(ExecutionError::Timeout { node, timeout }) => {
            assert_eq!(node, slow_id, "timeout error should identify correct node");
            assert_eq!(
                timeout,
                Duration::from_millis(10),
                "timeout should match configured duration"
            );
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected timeout error, got success"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PARALLEL FAILURE TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that when one parallel branch fails, the entire parallel execution fails.
#[tokio::test]
async fn parallel_branch_failure_stops_execution() {
    let mut graph = Graph::new();

    // Add parallel with one failing branch
    graph.add_parallel(
        "parallel_with_failure",
        [
            branch(|g| {
                g.add_boxed_system(Box::new(SuccessSystem));
            }),
            branch(|g| {
                g.add_boxed_system(Box::new(FailingSystem));
            }),
        ],
    );

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_err(),
        "parallel execution should fail when any branch fails"
    );
    match result {
        Err(ExecutionError::SystemError(msg)) => {
            assert!(
                msg.contains("intentional failure"),
                "error should come from the failing branch"
            );
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected error, got success"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// DECISION EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that decision takes the false branch when predicate returns false.
#[tokio::test]
async fn decision_takes_false_branch() {
    let mut graph = Graph::new();

    // Add decision system that outputs take_true = false
    graph.add_boxed_system(Box::new(DecisionSystem { take_true: false }));

    // Add decision node with predicate that checks take_true
    graph.add_conditional_branch::<DecisionOutput, _, _, _>(
        "test_decision",
        |output| output.take_true,
        |g| {
            // True branch - should NOT execute
            g.add_boxed_system(Box::new(HandlerSystem));
        },
        |g| {
            // False branch - should execute
            g.add_boxed_system(Box::new(SuccessSystem));
        },
    );

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // Insert HandlerLog - if true branch runs, it will set invoked=true
    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed");
    assert!(
        !log.was_invoked(),
        "true branch should NOT have been invoked (false branch should run)"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// LOOP EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that loop returns `MaxIterationsExceeded` when predicate never terminates.
///
/// Uses a loop with termination predicate that never returns true.
/// The initial state is set before the loop, then the loop body updates it.
#[tokio::test]
async fn loop_max_iterations_exceeded_error() {
    let mut graph = Graph::new();
    let counter = Arc::new(Mutex::new(0usize));

    // Set initial state before the loop (so predicate has something to read)
    graph.add_boxed_system(Box::new(InitialStateSystem));

    // Create loop with termination predicate that never returns true
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "infinite_loop",
        |_state| false, // Never terminates
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    // Use executor with small max iterations
    let executor = GraphExecutor::new().with_default_max_iterations(5);

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(result.is_err(), "should fail with max iterations exceeded");
    match result {
        Err(ExecutionError::MaxIterationsExceeded { max, .. }) => {
            assert_eq!(max, 5, "max should match configured limit");
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected MaxIterationsExceeded, got success"),
    }

    // Verify loop ran 5 times before failing
    assert_eq!(*counter.lock().unwrap(), 5, "loop should have run 5 times");
}

/// Verifies that loop terminates early when predicate returns true.
///
/// The predicate checks state.iteration >= 3, and the loop body increments iteration.
/// Initial state is set before the loop with iteration = 0.
#[tokio::test]
async fn loop_predicate_terminates_early() {
    let mut graph = Graph::new();
    let counter = Arc::new(Mutex::new(0usize));

    // Set initial state before the loop
    graph.add_boxed_system(Box::new(InitialStateSystem));

    // Create loop with termination predicate that terminates when iteration >= 3
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "early_termination_loop",
        |state| state.iteration >= 3, // Terminate when iteration reaches 3
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    // Use executor with high max iterations (should not be reached)
    let executor = GraphExecutor::new().with_default_max_iterations(100);

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(result.is_ok(), "execution should succeed");

    // Loop starts with iteration=0, runs 3 times (producing 1, 2, 3),
    // then predicate sees iteration=3 and terminates
    assert_eq!(
        *counter.lock().unwrap(),
        3,
        "loop should have run exactly 3 times before predicate terminated"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// LOOP ENTRY GUARANTEE TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that an unseeded loop-first graph fails before any node runs.
///
/// The termination predicate is evaluated before the first iteration, so a
/// main-chain loop whose predicate input is neither produced earlier on the
/// chain nor present in the context is caught at run start with
/// `LoopPredicateInputMissingOnEntry` — not mid-run with a `PredicateError`.
#[tokio::test]
async fn loop_entry_predicate_input_missing_fails_before_any_node() {
    let mut graph = Graph::new();
    let counter = Arc::new(Mutex::new(0usize));

    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "unseeded_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    // The graph is structurally valid: the body produces the predicate type
    // (the necessary body-side check). The entry-side guarantee depends on
    // the live context, so it is enforced at run start instead.
    assert_graph_valid(&graph);

    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    match result {
        Err(ExecutionError::LoopPredicateInputMissingOnEntry {
            name, output_type, ..
        }) => {
            assert_eq!(name, "unseeded_loop", "error names the loop");
            assert!(
                output_type.contains("LoopState"),
                "error names the missing type: {output_type}"
            );
        }
        other => panic!("expected LoopPredicateInputMissingOnEntry, got: {other:?}"),
    }
    assert_eq!(
        *counter.lock().unwrap(),
        0,
        "no loop iteration may run when the entry check fails"
    );
}

/// The entry check fires before *any* node executes — not merely before the
/// loop. A system ahead of the loop on the chain must not run either: without
/// this flag, a zero iteration count could not distinguish the run-start
/// failure from the old mid-run predicate error (the body never ran in either
/// case).
#[tokio::test]
async fn loop_entry_check_fires_before_preceding_nodes_run() {
    let mut graph = Graph::new();
    let flag = Arc::new(Mutex::new(false));
    let counter = Arc::new(Mutex::new(0usize));

    // Produces `()`, which never credits the loop's predicate input.
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "unseeded_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        matches!(
            result,
            Err(ExecutionError::LoopPredicateInputMissingOnEntry { .. })
        ),
        "expected the run-start entry error, got: {result:?}"
    );
    assert!(
        !*flag.lock().unwrap(),
        "the entry check must fail before any node runs — the preceding system must not execute"
    );
    assert_eq!(*counter.lock().unwrap(), 0, "no loop iteration may run");
}

/// Verifies that a caller pre-seeding the predicate input into the context
/// satisfies the run-start check. Production by a system earlier in the graph
/// is the normative source (outputs are system work products); the caller
/// pre-seed via `SystemContext::insert_output` is the supported alternative
/// for code driving a graph directly.
#[tokio::test]
async fn loop_entry_predicate_input_seeded_via_context_succeeds() {
    let mut graph = Graph::new();
    let counter = Arc::new(Mutex::new(0usize));

    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "seeded_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();
    ctx.insert_output(LoopState { iteration: 0 });

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(result.is_ok(), "seeded loop should run: {result:?}");
    assert_eq!(
        *counter.lock().unwrap(),
        3,
        "loop should iterate until the predicate terminates it"
    );
}

/// Verifies the entry check at a shared scope boundary: a seed produced on the
/// parent chain is visible to the shared inner graph's loop.
#[tokio::test]
async fn loop_entry_input_crosses_shared_scope_boundary() {
    let counter = Arc::new(Mutex::new(0usize));

    let counter_clone = Arc::clone(&counter);
    let mut inner = Graph::new();
    inner.add_loop::<LoopState, _, _>(
        "inner_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(InitialStateSystem));
    graph.add_scope("episode", inner, ContextPolicy::shared());

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "parent-produced seed crosses a shared boundary: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "inner loop should iterate");
}

/// The error message names the loop, the missing type, and the remedy —
/// produce the input with a system upstream — plus the caller pre-seed
/// alternative for code driving the graph directly.
#[test]
fn loop_predicate_input_missing_on_entry_display() {
    let err = ExecutionError::LoopPredicateInputMissingOnEntry {
        node: NodeId::from_string("1"),
        name: "unseeded_loop",
        output_type: "LoopState",
    };
    let msg = format!("{err}");
    assert!(msg.contains("unseeded_loop"));
    assert!(msg.contains("LoopState"));
    assert!(msg.contains("before the loop"));
    assert!(msg.contains("insert_output"));
}

/// Verifies that outputs never cross a non-shared boundary: even a seeded
/// parent context cannot satisfy an isolated inner loop, and the failure is
/// the upfront entry error rather than a mid-run predicate error.
#[tokio::test]
async fn loop_entry_input_blocked_by_isolated_scope_fails_fast() {
    let counter = Arc::new(Mutex::new(0usize));

    let counter_clone = Arc::clone(&counter);
    let mut inner = Graph::new();
    inner.add_loop::<LoopState, _, _>(
        "inner_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    let mut graph = Graph::new();
    graph.add_scope("episode", inner, ContextPolicy::new());

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();
    // The parent seed does not cross the isolated boundary.
    ctx.insert_output(LoopState { iteration: 0 });

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    match result {
        Err(ExecutionError::LoopPredicateInputMissingOnEntry {
            name, output_type, ..
        }) => {
            assert_eq!(name, "inner_loop", "error names the inner loop");
            assert!(
                output_type.contains("LoopState"),
                "error names the missing type: {output_type}"
            );
        }
        other => panic!("expected LoopPredicateInputMissingOnEntry, got: {other:?}"),
    }
    assert_eq!(
        *counter.lock().unwrap(),
        0,
        "no inner iteration may run when the entry check fails"
    );
}

/// The dynamic twin of the isolated-scope fail-fast: a loop-heading candidate
/// behind a non-shared dynamic boundary cannot see the parent seed, and the
/// failure is the upfront entry error at the boundary. Scope and dynamic
/// nodes share `execute_embedded`; this pins the dynamic side against future
/// divergence.
#[tokio::test]
async fn loop_entry_input_blocked_by_isolated_dynamic_fails_fast() {
    let counter = Arc::new(Mutex::new(0usize));

    let counter_clone = Arc::clone(&counter);
    let mut candidate = Graph::new();
    candidate.add_loop::<LoopState, _, _>(
        "inner_loop",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    // The contract matches the candidate exactly: it demands the seed
    // (`require_output`) and advertises the body's production.
    let contract = GraphSignature::new()
        .require_output::<LoopState>()
        .produce::<LoopState>();
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| Arc::from("episode"),
        [("episode", candidate)],
        DynamicSlot::new(contract, ContextPolicy::new()),
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();
    // The parent seed does not cross the non-shared boundary.
    ctx.insert_output(LoopState { iteration: 0 });

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    match result {
        Err(ExecutionError::LoopPredicateInputMissingOnEntry {
            name, output_type, ..
        }) => {
            assert_eq!(name, "inner_loop", "error names the candidate's loop");
            assert!(
                output_type.contains("LoopState"),
                "error names the missing type: {output_type}"
            );
        }
        other => panic!("expected LoopPredicateInputMissingOnEntry, got: {other:?}"),
    }
    assert_eq!(
        *counter.lock().unwrap(),
        0,
        "no candidate iteration may run when the entry check fails"
    );
}

/// A max-iterations-only loop (`add_loop_n`, no termination predicate) heading
/// a graph needs no entry input: the run-start check skips loops without a
/// termination predicate.
#[tokio::test]
async fn loop_n_heading_graph_runs_with_empty_context() {
    let mut graph = Graph::new();
    let counter = Arc::new(Mutex::new(0usize));
    let counter_clone = Arc::clone(&counter);
    graph.add_loop_n("fixed_loop", 3, move |g| {
        g.add_boxed_system(Box::new(LoopIterationSystem {
            counter: Arc::clone(&counter_clone),
        }));
    });

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "predicate-free loop needs no entry seed: {result:?}"
    );
    assert_eq!(
        *counter.lock().unwrap(),
        3,
        "loop should run its fixed iteration count"
    );
}

/// Unit is the absence of data flow, so a `Predicate<()>` receives implicit
/// unit and can terminate a loop without a caller-seeded output.
#[tokio::test]
async fn unit_predicate_loop_runs_with_empty_context() {
    let flag = Arc::new(Mutex::new(false));
    let mut graph = Graph::new();
    graph.add_loop::<(), _, _>(
        "unit_loop",
        |()| true,
        |body| {
            body.add_boxed_system(Box::new(FlagSystem {
                flag: Arc::clone(&flag),
            }));
        },
    );

    assert_graph_valid(&graph);
    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    assert!(
        executor.validate_resources(&graph, &ctx, None).is_ok(),
        "unit never requires a stored output during pre-flight"
    );
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        result.is_ok(),
        "implicit unit satisfies the predicate: {result:?}"
    );
    assert!(
        !*flag.lock().unwrap(),
        "a true unit predicate terminates before the loop body runs"
    );
}

/// A parallel node's branches all run and their outputs merge back into the
/// parent context, including outputs produced across a nested scope boundary.
/// The entry check must preserve that nested production when crediting the
/// parallel node as a possible source for a downstream loop.
#[tokio::test]
async fn loop_entry_input_produced_by_scoped_parallel_branch_succeeds() {
    let counter = Arc::new(Mutex::new(0usize));

    let mut graph = Graph::new();
    graph.add_parallel(
        "fan_out",
        vec![
            branch(|g| {
                let mut inner = Graph::new();
                inner.add_boxed_system(Box::new(InitialStateSystem));
                g.add_scope("scoped_init", inner, ContextPolicy::new());
            }),
            branch(|g| {
                g.add_boxed_system(Box::new(SuccessSystem));
            }),
        ],
    );
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_parallel",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // Pre-flight and runtime must agree: the parallel branch's output counts.
    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight must accept a parallel-produced predicate input"
    );

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "parallel-produced seed satisfies the loop entry check: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

/// A scope's outputs merge back into the parent context on exit — even across
/// a non-shared boundary — so a scope producing the predicate input satisfies
/// a downstream loop's first termination check.
#[tokio::test]
async fn loop_entry_input_produced_by_isolated_scope_merges_back() {
    let counter = Arc::new(Mutex::new(0usize));

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(InitialStateSystem));

    let mut graph = Graph::new();
    graph.add_scope("init", inner, ContextPolicy::new());
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_scope",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight must accept a scope-produced predicate input"
    );

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "scope outputs merge back and satisfy the loop entry check: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

/// A custom executor may raise its recursion limit above the default 64. The
/// mandatory loop-entry analysis must therefore credit a producer nested more
/// than 64 scope boundaries deep instead of rejecting the graph before the
/// producing scope has a chance to run.
#[tokio::test]
async fn custom_recursion_limit_credits_deep_scope_producer() {
    let mut nested = Graph::new();
    nested.add_boxed_system(Box::new(InitialStateSystem));
    for _ in 0..65 {
        let mut outer = Graph::new();
        outer.add_scope("deep_init", nested, ContextPolicy::shared());
        nested = outer;
    }

    let counter = Arc::new(Mutex::new(0usize));
    let mut graph = Graph::new();
    graph.add_scope("deep_init_root", nested, ContextPolicy::shared());
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_deep_scope",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new().with_max_recursion_depth(128);
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight must honor the custom recursion limit and credit the deep producer"
    );
    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "the deep producer should satisfy the downstream loop: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

/// A decision branch producing the predicate input through a nested scope
/// passes the entry check (crediting is optimistic — the branch *may* run) and
/// executes fine when the producing branch is actually taken.
#[tokio::test]
async fn loop_entry_input_produced_by_taken_scoped_decision_branch_succeeds() {
    let counter = Arc::new(Mutex::new(0usize));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(DecisionSystem { take_true: true }));
    graph.add_conditional_branch::<DecisionOutput, _, _, _>(
        "maybe_init",
        |decision| decision.take_true,
        |g| {
            let mut inner = Graph::new();
            inner.add_boxed_system(Box::new(InitialStateSystem));
            g.add_scope("scoped_init", inner, ContextPolicy::new());
        },
        |g| {
            g.add_boxed_system(Box::new(SuccessSystem));
        },
    );
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_decision",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    assert!(
        graph
            .signature()
            .requires_outputs()
            .iter()
            .any(|access| access.type_name.contains("LoopState")),
        "composition remains pessimistic because the decision branch is conditional"
    );
    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight optimistically credits the scoped decision branch"
    );
    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "a taken branch's output satisfies the loop's first check: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

/// The entry check is a fail-fast courtesy, never the guarantee: an input
/// produced only conditionally passes it (crediting is optimistic), and if
/// the producing branch is *not* taken, the failure is the mid-run
/// [`PredicateError::OutputNotFound`] at the loop — exactly as documented on
/// [`ExecutionError::LoopPredicateInputMissingOnEntry`]. This is the
/// untaken-branch twin of
/// `loop_entry_input_produced_by_taken_scoped_decision_branch_succeeds`;
/// without it,
/// a regression to pessimistic crediting (rejecting this graph at run start)
/// would go uncaught.
#[tokio::test]
async fn loop_entry_input_from_untaken_decision_branch_defers_to_mid_run_error() {
    let flag = Arc::new(Mutex::new(false));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(DecisionSystem { take_true: false }));
    graph.add_conditional_branch::<DecisionOutput, _, _, _>(
        "maybe_init",
        |decision| decision.take_true,
        |g| {
            g.add_boxed_system(Box::new(InitialStateSystem));
        },
        |g| {
            g.add_boxed_system(Box::new(FlagSystem {
                flag: Arc::clone(&flag),
            }));
        },
    );
    graph.add_loop::<LoopState, _, _>(
        "loop_after_decision",
        |state| state.iteration >= 3,
        |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::new(Mutex::new(0)),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // Both static surfaces accept the composition: the producing branch *may*
    // run, so pre-flight and the run-start entry check must not reject it.
    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight credits the conditionally producing branch"
    );

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    match result {
        Err(ExecutionError::PredicateError(PredicateError::OutputNotFound { type_name })) => {
            assert!(
                type_name.contains("LoopState"),
                "the mid-run error names the missing type: {type_name}"
            );
        }
        other => panic!("expected the mid-run OutputNotFound at the loop, got: {other:?}"),
    }
    assert!(
        *flag.lock().unwrap(),
        "the untaken-branch path executed — the failure was mid-run, not at run start"
    );
}

/// A switch case may contain a dynamic boundary. The dynamic slot's contract
/// is the known interface at validation time, so its `produces` set must remain
/// visible when the case is credited as a possible producer.
#[tokio::test]
async fn loop_entry_input_produced_by_dynamic_in_taken_switch_case_succeeds() {
    let counter = Arc::new(Mutex::new(0usize));

    let mut candidate = Graph::new();
    candidate.add_boxed_system(Box::new(InitialStateSystem));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SwitchKeySystem { key: "seed" }));
    graph.add_switch::<SwitchOutput, _, _, _>(
        "maybe_init",
        |output| output.key,
        [(
            "seed",
            branch(|g| {
                g.add_dynamic(
                    "dynamic_init",
                    |_ctx| "only",
                    [("only", candidate)],
                    DynamicSlot::new(
                        GraphSignature::new().produce::<LoopState>(),
                        ContextPolicy::new(),
                    ),
                );
            }),
        )],
        Some(branch(|g| {
            g.add_boxed_system(Box::new(SuccessSystem));
        })),
    );
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_switch",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight credits the dynamic contract inside the switch case"
    );
    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "the selected dynamic candidate supplies the loop seed: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

/// The default switch branch participates in the same optimistic production
/// analysis as named cases, including outputs merged from a nested scope.
#[tokio::test]
async fn loop_entry_input_produced_by_scoped_switch_default_succeeds() {
    let counter = Arc::new(Mutex::new(0usize));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SwitchKeySystem { key: "unknown" }));
    graph.add_switch::<SwitchOutput, _, _, _>(
        "default_init",
        |output| output.key,
        [(
            "other",
            branch(|g| {
                g.add_boxed_system(Box::new(SuccessSystem));
            }),
        )],
        Some(branch(|g| {
            let mut inner = Graph::new();
            inner.add_boxed_system(Box::new(InitialStateSystem));
            g.add_scope("default_scope", inner, ContextPolicy::new());
        })),
    );
    let counter_clone = Arc::clone(&counter);
    graph.add_loop::<LoopState, _, _>(
        "loop_after_default",
        |state| state.iteration >= 3,
        move |g| {
            g.add_boxed_system(Box::new(LoopIterationSystem {
                counter: Arc::clone(&counter_clone),
            }));
        },
    );

    assert_graph_valid(&graph);
    let executor = GraphExecutor::new();
    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    assert!(
        executor.validate_resources(&graph, &ctx, hooks).is_ok(),
        "pre-flight credits the scoped default branch"
    );
    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_ok(),
        "the selected default scope supplies the loop seed: {result:?}"
    );
    assert_eq!(*counter.lock().unwrap(), 3, "loop should iterate");
}

// Note on provenance: outputs are the work products of systems. Hook and
// middleware code is not a sanctioned writer of the output channel, so no
// test here demonstrates seeding a loop's predicate input from them — the
// supported sources are production by an earlier system (in this graph or an
// enclosing shared chain) and a caller pre-seed via
// `SystemContext::insert_output`.

/// The entry check routes through the normal failure path, so `OnGraphFailure`
/// hooks observe it like any other execution error.
#[tokio::test]
async fn loop_entry_failure_fires_on_graph_failure_hook() {
    let mut graph = Graph::new();
    graph.add_loop::<LoopState, _, _>(
        "unseeded_loop",
        |state| state.iteration >= 3,
        |g| {
            g.add_boxed_system(Box::new(InitialStateSystem));
        },
    );

    assert_graph_valid(&graph);
    let failed = Arc::new(Mutex::new(false));
    let failed_clone = Arc::clone(&failed);
    let hooks = HooksAPI::new();
    hooks
        .register_observer::<OnGraphFailure, _>("record_failure", move |event: &GraphEvent| {
            if matches!(event, GraphEvent::GraphFailure { .. }) {
                *failed_clone.lock().unwrap() = true;
            }
        })
        .expect("hook registration should succeed");

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await;

    assert!(
        matches!(
            result,
            Err(ExecutionError::LoopPredicateInputMissingOnEntry { .. })
        ),
        "unseeded loop-first graph fails the entry check: {result:?}"
    );
    assert!(
        *failed.lock().unwrap(),
        "OnGraphFailure must fire for the entry-check error"
    );
}

/// Predicate input for the hook-provider mismatch test: implements
/// `LocalResource` so it can be registered with a provider hook.
#[derive(Clone, Debug)]
struct GateState {
    done: bool,
}
impl LocalResource for GateState {}

async fn advance_gate() -> GateState {
    GateState { done: true }
}

/// A provider hook inserts a *resource* (`ctx.insert`), not an output — it
/// cannot satisfy a termination predicate, which reads the output channel.
/// The entry check must therefore still fail, upfront.
#[tokio::test]
async fn hook_provided_resource_does_not_satisfy_loop_entry() {
    let mut graph = Graph::new();
    graph.add_loop::<GateState, _, _>(
        "gated_loop",
        |state| state.done,
        |g| {
            g.add_system(advance_gate);
        },
    );

    assert_graph_valid(&graph);
    let hooks = HooksAPI::new();
    hooks
        .register_provider::<OnGraphStart, GateState, _>("provide_gate_state", |_event| {
            Some(GateState { done: true })
        })
        .expect("hook registration should succeed");

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, Some(&hooks), None)
        .await;

    match result {
        Err(ExecutionError::LoopPredicateInputMissingOnEntry {
            name, output_type, ..
        }) => {
            assert_eq!(name, "gated_loop", "error names the loop");
            assert!(
                output_type.contains("GateState"),
                "error names the missing type: {output_type}"
            );
        }
        other => panic!("expected LoopPredicateInputMissingOnEntry, got: {other:?}"),
    }
}

/// Loops inside branch interiors execute conditionally and are not
/// entry-checked: a genuinely missing predicate input there surfaces as the
/// older mid-run predicate error when the branch actually runs.
#[tokio::test]
async fn branch_interior_loop_missing_input_fails_mid_run() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(DecisionSystem { take_true: true }));
    graph.add_conditional_branch::<DecisionOutput, _, _, _>(
        "route",
        |decision| decision.take_true,
        |g| {
            g.add_loop::<LoopState, _, _>(
                "interior_loop",
                |state| state.iteration >= 3,
                |body| {
                    body.add_boxed_system(Box::new(LoopIterationSystem {
                        counter: Arc::new(Mutex::new(0)),
                    }));
                },
            );
        },
        |g| {
            g.add_boxed_system(Box::new(SuccessSystem));
        },
    );

    assert_graph_valid(&graph);
    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await;

    assert!(
        matches!(
            result,
            Err(ExecutionError::PredicateError(
                PredicateError::OutputNotFound { .. }
            ))
        ),
        "a branch-interior loop keeps the mid-run predicate error: {result:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// OUTPUT CHAINING TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that system B can read system A's output via Out<T>.
#[tokio::test]
async fn output_chaining_between_systems() {
    let mut graph = Graph::new();
    let received = Arc::new(Mutex::new(None));

    // Producer outputs value 42
    graph.add_boxed_system(Box::new(ProducerSystem { value: 42 }));

    // Consumer reads producer's output
    graph.add_boxed_system(Box::new(ConsumerSystem {
        received: Arc::clone(&received),
    }));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed");
    assert_eq!(
        *received.lock().unwrap(),
        Some(42),
        "consumer should have received producer's output value"
    );
}

/// Verifies that predicate can read system output for branching decisions.
/// (This is implicitly tested in decision tests, but here we make it explicit.)
#[tokio::test]
async fn output_available_to_predicate() {
    let mut graph = Graph::new();
    let true_branch_called = Arc::new(Mutex::new(false));
    let false_branch_called = Arc::new(Mutex::new(false));

    // Producer outputs value 100
    graph.add_boxed_system(Box::new(ProducerSystem { value: 100 }));

    // Decision based on producer output
    let true_flag = Arc::clone(&true_branch_called);
    let false_flag = Arc::clone(&false_branch_called);
    graph.add_conditional_branch::<ProducerOutput, _, _, _>(
        "value_check",
        |output| output.value > 50, // True because 100 > 50
        move |g| {
            let flag = Arc::clone(&true_flag);
            g.add_boxed_system(Box::new(FlagSystem { flag }));
        },
        move |g| {
            let flag = Arc::clone(&false_flag);
            g.add_boxed_system(Box::new(FlagSystem { flag }));
        },
    );

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed");
    assert!(
        *true_branch_called.lock().unwrap(),
        "true branch should be called (100 > 50)"
    );
    assert!(
        !*false_branch_called.lock().unwrap(),
        "false branch should NOT be called"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// RECURSION LIMIT TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Marker for decision output in recursion test.
#[derive(Debug)]
struct RecursionMarker;

/// System that outputs recursion marker.
async fn recursion_marker() -> RecursionMarker {
    RecursionMarker
}

/// Verifies that deeply nested control flow hits the recursion limit.
#[tokio::test]
async fn recursion_limit_exceeded() {
    // Build a deeply nested decision structure that exceeds the recursion limit
    // Each decision adds 1 to the depth, so we need depth > max_recursion_depth
    fn build_nested_decisions(graph: &mut Graph, depth: usize) {
        if depth == 0 {
            graph.add_boxed_system(Box::new(SuccessSystem));
        } else {
            graph.add_system(recursion_marker);
            graph.add_conditional_branch::<RecursionMarker, _, _, _>(
                "nested_decision",
                |_| true, // Always take true branch
                |g| build_nested_decisions(g, depth - 1),
                |g| {
                    g.add_boxed_system(Box::new(SuccessSystem));
                },
            );
        }
    }

    let mut graph = Graph::new();
    // Build 70 levels of nesting (default max is 64)
    build_nested_decisions(&mut graph, 70);

    // Use executor with default recursion limit (64)
    let executor = GraphExecutor::new();

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_err(),
        "execution should fail with recursion limit exceeded"
    );
    match result {
        Err(ExecutionError::RecursionLimitExceeded { depth, max }) => {
            assert_eq!(max, 64, "max should be default (64)");
            assert_eq!(depth, 64, "depth should be at the limit");
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected RecursionLimitExceeded, got success"),
    }
}

/// Verifies that custom recursion limit is respected.
#[tokio::test]
async fn custom_recursion_limit_exceeded() {
    fn build_nested_decisions(graph: &mut Graph, depth: usize) {
        if depth == 0 {
            graph.add_boxed_system(Box::new(SuccessSystem));
        } else {
            graph.add_system(recursion_marker);
            graph.add_conditional_branch::<RecursionMarker, _, _, _>(
                "nested_decision",
                |_| true,
                |g| build_nested_decisions(g, depth - 1),
                |g| {
                    g.add_boxed_system(Box::new(SuccessSystem));
                },
            );
        }
    }

    let mut graph = Graph::new();
    // Build 10 levels of nesting
    build_nested_decisions(&mut graph, 10);

    // Use executor with custom low recursion limit (5)
    let executor = GraphExecutor::new().with_max_recursion_depth(5);

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = executor.execute(&graph, &mut ctx, hooks, None).await;

    assert!(
        result.is_err(),
        "execution should fail with custom recursion limit exceeded"
    );
    match result {
        Err(ExecutionError::RecursionLimitExceeded { depth, max }) => {
            assert_eq!(max, 5, "max should be custom limit (5)");
            assert_eq!(depth, 5, "depth should be at the custom limit");
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected RecursionLimitExceeded, got success"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SWITCH EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that switch routes to default case when no case matches.
#[tokio::test]
async fn switch_routes_to_default_when_no_match() {
    let mut graph = Graph::new();
    let default_called = Arc::new(Mutex::new(false));
    let case_a_called = Arc::new(Mutex::new(false));

    // Output key "unknown" which doesn't match any case
    graph.add_boxed_system(Box::new(SwitchKeySystem { key: "unknown" }));

    let default_flag = Arc::clone(&default_called);
    let case_a_flag = Arc::clone(&case_a_called);
    graph.add_switch::<SwitchOutput, _, _, _>(
        "test_switch",
        |output| output.key,
        [(
            "a",
            branch(move |g| {
                let flag = Arc::clone(&case_a_flag);
                g.add_boxed_system(Box::new(FlagSystem { flag }));
            }),
        )],
        Some(branch(move |g| {
            let flag = Arc::clone(&default_flag);
            g.add_boxed_system(Box::new(FlagSystem { flag }));
        })),
    );

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed via default");
    assert!(
        *default_called.lock().unwrap(),
        "default case should be called"
    );
    assert!(
        !*case_a_called.lock().unwrap(),
        "case 'a' should NOT be called"
    );
}

/// Verifies that switch returns error when no case matches and no default provided.
#[tokio::test]
async fn switch_error_when_no_match_and_no_default() {
    let mut graph = Graph::new();

    // Output key "unknown" which doesn't match any case
    graph.add_boxed_system(Box::new(SwitchKeySystem { key: "unknown" }));

    graph.add_switch::<SwitchOutput, _, _, _>(
        "test_switch",
        |output| output.key,
        [
            (
                "a",
                branch(|g| {
                    g.add_boxed_system(Box::new(SuccessSystem));
                }),
            ),
            (
                "b",
                branch(|g| {
                    g.add_boxed_system(Box::new(SuccessSystem));
                }),
            ),
        ],
        None, // No default
    );

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_err(),
        "execution should fail with no matching case"
    );
    match result {
        Err(ExecutionError::NoMatchingCase { key, .. }) => {
            assert_eq!(key, "unknown", "error should report the unmatched key");
        }
        Err(other) => panic!("unexpected error type: {other:?}"),
        Ok(_) => panic!("expected NoMatchingCase, got success"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// RESOURCE VALIDATION TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that hook-provided resources are recognized during validation.
///
/// `LoggingSystem` requires `SystemInfo` which is provided by `DevToolsPlugin`
/// via a hook on `OnSystemStart`. Validation should pass because the hook
/// tracks the resource type it provides.
#[test]
fn hook_provided_resources_pass_validation() {
    use test_utils::{ExecutionLog, LoggingSystem};

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(LoggingSystem));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    // LoggingSystem also requires ExecutionLog (not hook-provided)
    ctx.insert(ExecutionLog::default());

    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, hooks);

    assert!(
        result.is_ok(),
        "validation should pass when hooks provide required resources"
    );
}

/// Verifies that validation fails when hooks are not provided but system requires
/// hook-provided resources.
///
/// `LoggingSystem` requires `SystemInfo` (provided by `DevToolsPlugin` via hooks)
/// and `ExecutionLog`. Without hooks, validation should fail for `SystemInfo`.
#[test]
fn validation_fails_without_hooks_for_hook_provided_resources() {
    use test_utils::{ExecutionLog, LoggingSystem};

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(LoggingSystem));

    // Create context with ExecutionLog but without hooks/DevToolsPlugin
    let mut ctx = SystemContext::new();
    ctx.insert(ExecutionLog::default());

    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(
        result.is_err(),
        "validation should fail when hooks are not provided"
    );

    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1, "should have exactly one validation error");

    // Verify the error is about SystemInfo
    let error_msg = format!("{}", errors[0]);
    assert!(
        error_msg.contains("SystemInfo"),
        "error should mention SystemInfo: {error_msg}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// ERROR KIND TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that `CaughtError.kind == ErrorKind::Execution` for `SystemError::ExecutionError`.
#[tokio::test]
async fn error_kind_execution_for_execution_error() {
    let mut graph = Graph::new();

    let failing_id = graph.add_boxed_system(Box::new(FailingSystem));
    graph.add_error_handler_for(failing_id, |g| {
        g.add_boxed_system(Box::new(KindCheckingHandler));
    });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let log = HandlerLog::default();
    let kind_log = ErrorKindLog::default();
    ctx.insert(log.clone());
    ctx.insert(kind_log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;
    assert!(result.is_ok(), "execution should succeed via error handler");
    assert!(log.was_invoked(), "error handler should have been invoked");
    assert_eq!(
        kind_log.kind(),
        Some(ErrorKind::Execution),
        "error kind should be Execution for ExecutionError"
    );
}

/// Verifies that `CaughtError.kind == ErrorKind::ParamResolution` for `SystemError::ParamError`.
#[tokio::test]
async fn error_kind_param_resolution_for_param_error() {
    let mut graph = Graph::new();

    let failing_id = graph.add_boxed_system(Box::new(ParamFailingSystem));
    graph.add_error_handler_for(failing_id, |g| {
        g.add_boxed_system(Box::new(KindCheckingHandler));
    });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let log = HandlerLog::default();
    let kind_log = ErrorKindLog::default();
    ctx.insert(log.clone());
    ctx.insert(kind_log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;
    assert!(result.is_ok(), "execution should succeed via error handler");
    assert!(log.was_invoked(), "error handler should have been invoked");
    assert_eq!(
        kind_log.kind(),
        Some(ErrorKind::ParamResolution),
        "error kind should be ParamResolution for ParamError"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SYSTEM NODE BUILDER INTEGRATION TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Verifies that `system_boxed().on_error()` invokes the error handler on failure.
#[tokio::test]
async fn system_builder_error_handler_invoked_on_failure() {
    let mut graph = Graph::new();
    graph.system_boxed(Box::new(FailingSystem)).on_error(|g| {
        g.add_boxed_system(Box::new(HandlerSystem));
    });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(result.is_ok(), "execution should succeed via error handler");
    assert!(log.was_invoked(), "error handler should have been invoked");
}

/// Verifies that `system_boxed().with_timeout().on_timeout()` invokes the timeout handler.
#[tokio::test]
async fn system_builder_timeout_handler_invoked_on_timeout() {
    use std::time::Duration;

    let mut graph = Graph::new();
    graph
        .system_boxed(Box::new(SlowSystem {
            duration: Duration::from_secs(10),
        }))
        .with_timeout(Duration::from_millis(10))
        .on_timeout(|g| {
            g.add_boxed_system(Box::new(HandlerSystem));
        });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;

    assert!(
        result.is_ok(),
        "execution should succeed via timeout handler"
    );
    assert!(
        log.was_invoked(),
        "timeout handler should have been invoked"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// RETRY POLICY INTEGRATION TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// System with retry succeeds after transient failures.
#[tokio::test]
async fn retry_succeeds_after_transient_failures() {
    let attempts = Arc::new(AtomicU32::new(0));
    let mut graph = Graph::new();

    // Fails twice, succeeds on 3rd attempt. Policy allows 2 retries.
    graph
        .system_boxed(Box::new(EventuallySucceedsSystem {
            fail_count: 2,
            attempts: attempts.clone(),
        }))
        .with_retry(RetryPolicy::fixed(2, std::time::Duration::from_millis(1)));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;
    assert!(result.is_ok(), "should succeed after retries: {result:?}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "should have attempted 3 times"
    );
}

/// System with retry exhausted routes to error handler.
#[tokio::test]
async fn retry_exhausted_routes_to_error_handler() {
    let attempts = Arc::new(AtomicU32::new(0));
    let mut graph = Graph::new();

    // Fails 5 times, but policy only allows 2 retries (3 total attempts).
    graph
        .system_boxed(Box::new(EventuallySucceedsSystem {
            fail_count: 5,
            attempts: attempts.clone(),
        }))
        .with_retry(RetryPolicy::fixed(2, std::time::Duration::from_millis(1)))
        .on_error(|g| {
            g.add_boxed_system(Box::new(HandlerSystem));
        });

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let log = HandlerLog::default();
    ctx.insert(log.clone());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;
    assert!(result.is_ok(), "should route to error handler");
    assert!(log.was_invoked(), "error handler should have been invoked");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "should have attempted 3 times before giving up"
    );
}

/// System with retry + timeout retries on timeout.
#[tokio::test]
async fn retry_with_timeout_retries_on_timeout() {
    use std::time::Duration;

    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_clone = attempts.clone();

    // A system that is slow on first attempt but fast on second
    struct SlowThenFastSystem {
        attempts: Arc<AtomicU32>,
    }

    impl System for SlowThenFastSystem {
        type Output = ();

        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // First attempt: sleep longer than timeout
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
                Ok(())
            })
        }

        fn name(&self) -> &'static str {
            "slow_then_fast_system"
        }
    }

    let mut graph = Graph::new();
    graph
        .system_boxed(Box::new(SlowThenFastSystem {
            attempts: attempts_clone,
        }))
        .with_timeout(Duration::from_millis(10))
        .with_retry(RetryPolicy::fixed(1, Duration::from_millis(1)));

    let server = create_test_server();
    let hooks = get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await;
    assert!(
        result.is_ok(),
        "should succeed on second attempt: {result:?}"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "should have attempted twice"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SCOPE EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scope_error_propagates_to_parent() {
    // A failing system inside a scope should propagate the error to the parent graph.
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(FailingSystem));

    let mut graph = Graph::new();
    graph.add_scope("failing_scope", inner, ContextPolicy::shared());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        matches!(result, Err(ExecutionError::SystemError(_))),
        "error inside scope should propagate to parent, got: {result:?}"
    );
}

#[tokio::test]
async fn scope_error_propagates_isolated_mode() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(FailingSystem));

    let mut graph = Graph::new();
    graph.add_scope("failing_isolated", inner, ContextPolicy::new());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        matches!(result, Err(ExecutionError::SystemError(_))),
        "error inside isolated scope should propagate, got: {result:?}"
    );
}

#[tokio::test]
async fn scope_forward_missing_resource_hard_errors() {
    // `forward::<T>()` for a resource that isn't in the parent is a hard
    // error — symmetric with `forward_fresh`'s missing-factory behavior.
    let flag = Arc::new(Mutex::new(false));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));

    let policy = ContextPolicy::new().share_rest().forward::<TestConfig>();
    let mut graph = Graph::new();
    graph.add_scope("fwd_missing", inner, policy);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    match result {
        Err(ExecutionError::ScopeMissingResource {
            scope,
            resource,
            action,
        }) => {
            assert_eq!(scope, "fwd_missing");
            assert!(resource.contains("TestConfig"));
            assert_eq!(action, "forward");
        }
        other => panic!("expected ScopeMissingResource, got {other:?}"),
    }
    assert!(
        !*flag.lock().unwrap(),
        "inner system should not run when scope entry fails"
    );
}

#[tokio::test]
async fn scope_fork_missing_resource_hard_errors() {
    // `fork::<T>()` for a resource that isn't in the parent is a hard error —
    // the runtime safety net for the `fork` verb, mirroring `forward`. Covers
    // the `action: "fork"` branch of `ScopeMissingResource`.
    #[derive(Debug, Default)]
    struct Forkable {
        value: i32,
    }
    impl LocalResource for Forkable {}
    impl polaris_system::resource::ForkStrategy for Forkable {
        fn fork(&self) -> Self {
            Self { value: self.value }
        }
    }

    let flag = Arc::new(Mutex::new(false));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));

    let policy = ContextPolicy::new().share_rest().fork::<Forkable>();
    let mut graph = Graph::new();
    graph.add_scope("fork_missing", inner, policy);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    match result {
        Err(ExecutionError::ScopeMissingResource {
            scope,
            resource,
            action,
        }) => {
            assert_eq!(scope, "fork_missing");
            assert!(resource.contains("Forkable"));
            assert_eq!(action, "fork");
        }
        other => panic!("expected ScopeMissingResource, got {other:?}"),
    }
    assert!(
        !*flag.lock().unwrap(),
        "inner system should not run when scope entry fails"
    );
}
