//! Edge case tests for graph execution.
//!
//! Tests covering error handling, timeouts, parallel failures, loop termination,
//! output chaining, recursion limits, and switch edge cases.

mod test_utils;

use polaris_graph::executor::{ErrorKind, ExecutionError, GraphExecutor};
use polaris_graph::graph::Graph;
use polaris_graph::node::RetryPolicy;
use polaris_system::param::SystemContext;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use test_utils::{
    ConsumerSystem, DecisionOutput, DecisionSystem, ErrorKindLog, EventuallySucceedsSystem,
    FailingSystem, FlagSystem, HandlerLog, HandlerSystem, InitialStateSystem, KindCheckingHandler,
    LoopIterationSystem, LoopState, ParamFailingSystem, ProducerOutput, ProducerSystem, SlowSystem,
    SuccessSystem, SwitchKeySystem, SwitchOutput, branch, create_test_server, get_hooks,
};

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
