//! Integration tests for the graph hook system.
//!
//! Ensures lifecycle schedules (graph, system, decision, switch,
//! loop, parallel) and custom schedule markers attached to system nodes
//! are correctly invoked.

mod test_utils;

use polaris_graph::ExecutionError;
use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::hooks::events::GraphEvent;
use polaris_graph::hooks::schedule::{
    OnDecisionComplete, OnDecisionStart, OnGraphComplete, OnGraphFailure, OnGraphStart, OnLoopEnd,
    OnLoopIteration, OnLoopStart, OnParallelComplete, OnParallelStart, OnSwitchComplete,
    OnSwitchStart, OnSystemComplete, OnSystemError, OnSystemStart,
};
use polaris_graph::node::NodeId;
use polaris_system::param::SystemContext;
use polaris_system::plugin::Schedule;
use polaris_system::system;
use polaris_system::system::SystemError;
use std::sync::{Arc, Mutex};
use test_utils::{
    DecisionOutput, DecisionSystem, FailingSystem, SuccessSystem, SwitchKeySystem, SwitchOutput,
};

// ═══════════════════════════════════════════════════════════════════════════════
// Test Harness
// ═══════════════════════════════════════════════════════════════════════════════

/// A single record of what a hook observed.
#[derive(Debug, Clone)]
struct EventRecord {
    /// The schedule name that fired (e.g. "`OnSystemStart`" or a custom marker name).
    schedule_name: String,
    /// The full event data.
    event: GraphEvent,
}

/// Ordered invocation log shared between hooks.
type EventLog = Arc<Mutex<Vec<EventRecord>>>;

/// Registers recording observer hooks for the given schedule types.
macro_rules! register_recording_hooks {
    ($hooks:expr, $log:expr, $( $schedule:ty => $name:literal ),* $(,)?) => {
        $({
            let log_clone = $log.clone();
            let schedule_name = $name.to_string();
            $hooks.register_observer::<$schedule, _>(
                concat!("recorder_", $name),
                move |event: &GraphEvent| {
                    log_clone.lock().unwrap().push(EventRecord {
                        schedule_name: schedule_name.clone(),
                        event: event.clone(),
                    });
                },
            ).expect("hook registration should succeed");
        })*
    };
}

/// Registers recording hooks for all built-in schedules and returns the shared log.
fn register_all_builtin_hooks(hooks: &HooksAPI) -> EventLog {
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    register_recording_hooks!(hooks, log,
        OnGraphStart => "OnGraphStart",
        OnGraphComplete => "OnGraphComplete",
        OnGraphFailure => "OnGraphFailure",
        OnSystemStart => "OnSystemStart",
        OnSystemComplete => "OnSystemComplete",
        OnSystemError => "OnSystemError",
        OnDecisionStart => "OnDecisionStart",
        OnDecisionComplete => "OnDecisionComplete",
        OnSwitchStart => "OnSwitchStart",
        OnSwitchComplete => "OnSwitchComplete",
        OnLoopStart => "OnLoopStart",
        OnLoopIteration => "OnLoopIteration",
        OnLoopEnd => "OnLoopEnd",
        OnParallelStart => "OnParallelStart",
        OnParallelComplete => "OnParallelComplete",
    );
    log
}

/// Executes a graph with all built-in recording hooks, returning (result, log).
async fn execute_with_hooks(
    graph: &Graph,
) -> (
    Result<polaris_graph::ExecutionResult, ExecutionError>,
    EventLog,
) {
    execute_with_custom_hooks(graph, |_, _| {}).await
}

/// Executes a graph with built-in + custom recording hooks via a setup closure.
async fn execute_with_custom_hooks(
    graph: &Graph,
    setup: impl FnOnce(&HooksAPI, &EventLog),
) -> (
    Result<polaris_graph::ExecutionResult, ExecutionError>,
    EventLog,
) {
    let hooks = HooksAPI::new();
    let log = register_all_builtin_hooks(&hooks);
    setup(&hooks, &log);
    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(graph, &mut ctx, Some(&hooks)).await;
    (result, log)
}

/// Asserts that the recorded event log matches an expected sequence of
/// `(schedule_name, event_pattern)` pairs exactly — in order, count, and content.
macro_rules! assert_event_sequence {
    ($log:expr, [ $( $schedule:literal => $pattern:pat $(if $guard:expr)? ),* $(,)? ]) => {{
        let records = $log.lock().unwrap();
        let expected: &[&str] = &[$($schedule),*];
        let actual: Vec<&str> = records.iter().map(|r| r.schedule_name.as_str()).collect();
        assert_eq!(actual, expected, "event schedule sequence mismatch");

        let mut _iter = records.iter().enumerate();
        $(
            let (_i, _record) = _iter.next().unwrap();
            assert!(
                matches!(&_record.event, $pattern $(if $guard)?),
                "event[{}] ({}): expected {}, got {:?}",
                _i, $schedule, stringify!($pattern), _record.event
            );
        )*
    }};
}

/// Finds a node's ID by name in the graph. Panics if not found.
fn node_id_by_name(graph: &Graph, name: &str) -> NodeId {
    graph
        .nodes()
        .iter()
        .find(|n| n.name() == name)
        .unwrap_or_else(|| panic!("no node named {name:?}"))
        .id()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Custom System Schedules
// ═══════════════════════════════════════════════════════════════════════════════

struct MarkerA;
impl Schedule for MarkerA {}

struct MarkerB;
impl Schedule for MarkerB {}

// ═══════════════════════════════════════════════════════════════════════════════
// Built-in Schedule Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_system_lifecycle() {
    let mut graph = Graph::new();
    let system_id = graph.add_boxed_system(Box::new(SuccessSystem));

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 1 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_system" } if *node_id == system_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_system", duration } if *node_id == system_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 1, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn failing_system_lifecycle() {
    let mut graph = Graph::new();
    let failing_id = graph.add_boxed_system(Box::new(FailingSystem));

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_err());

    assert_event_sequence!(log, [
        "OnGraphStart"    => GraphEvent::GraphStart { node_count: 1 },
        "OnSystemStart"   => GraphEvent::SystemStart { node_id, system_name: "failing_system" } if *node_id == failing_id,
        "OnSystemError"   => GraphEvent::SystemError { node_id, system_name: "failing_system", error } if *node_id == failing_id && error.contains("intentional failure"),
        "OnGraphFailure"  => GraphEvent::GraphFailure { error } if matches!(error, ExecutionError::SystemError(_)),
    ]);
}

#[tokio::test]
async fn sequential_systems() {
    #[system]
    async fn sys_a() -> i32 {
        1
    }
    #[system]
    async fn sys_b() -> i32 {
        2
    }

    let mut graph = Graph::new();
    let sys_a_id = graph.add_system_node(sys_a);
    let sys_b_id = graph.add_system_node(sys_b);

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 2 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "sys_a" } if *node_id == sys_a_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "sys_a", duration } if *node_id == sys_a_id && !duration.is_zero(),
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "sys_b" } if *node_id == sys_b_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "sys_b", duration } if *node_id == sys_b_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 2, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn error_handler_recovery() {
    let mut graph = Graph::new();
    let failing_id = graph.add_boxed_system(Box::new(FailingSystem));
    let mut handler_id = None;
    graph.add_error_handler(failing_id.clone(), |g| {
        handler_id = Some(g.add_boxed_system(Box::new(SuccessSystem)));
    });
    let handler_id = handler_id.unwrap();

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok(), "error handler should recover: {:?}", result);

    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 2 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "failing_system" } if *node_id == failing_id,
        "OnSystemError"     => GraphEvent::SystemError { node_id, system_name: "failing_system", error } if *node_id == failing_id && error.contains("intentional failure"),
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_system" } if *node_id == handler_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_system", duration } if *node_id == handler_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 2, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn decision_hooks() {
    let mut graph = Graph::new();
    let decision_sys_id = graph.add_boxed_system(Box::new(DecisionSystem { take_true: true }));
    let mut true_branch_id = None;
    graph.add_conditional_branch::<DecisionOutput, _, _, _>(
        "test_decision",
        |d| d.take_true,
        |g| {
            true_branch_id = Some(g.add_boxed_system(Box::new(SuccessSystem)));
        },
        |g| {
            g.add_boxed_system(Box::new(SuccessSystem));
        },
    );
    let decision_id = node_id_by_name(&graph, "test_decision");
    let true_branch_id = true_branch_id.unwrap();

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"        => GraphEvent::GraphStart { node_count: 4 },
        "OnSystemStart"       => GraphEvent::SystemStart { node_id, system_name: "decision_system" } if *node_id == decision_sys_id,
        "OnSystemComplete"    => GraphEvent::SystemComplete { node_id, system_name: "decision_system", duration } if *node_id == decision_sys_id && !duration.is_zero(),
        "OnDecisionStart"     => GraphEvent::DecisionStart { node_id, node_name: "test_decision" } if *node_id == decision_id,
        "OnSystemStart"       => GraphEvent::SystemStart { node_id, system_name: "success_system" } if *node_id == true_branch_id,
        "OnSystemComplete"    => GraphEvent::SystemComplete { node_id, system_name: "success_system", duration } if *node_id == true_branch_id && !duration.is_zero(),
        "OnDecisionComplete"  => GraphEvent::DecisionComplete {
            node_id,
            node_name: "test_decision",
            selected_branch: "true",
        } if *node_id == decision_id,
        "OnGraphComplete"     => GraphEvent::GraphComplete { nodes_executed: 3, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn loop_hooks() {
    #[system]
    async fn loop_body() -> i32 {
        42
    }

    let mut graph = Graph::new();
    let mut body_id = None;
    graph.add_loop_n("test_loop", 3, |g| {
        body_id = Some(g.add_system_node(loop_body));
    });
    let loop_id = node_id_by_name(&graph, "test_loop");
    let body_id = body_id.unwrap();

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 2 },
        "OnLoopStart"       => GraphEvent::LoopStart {
            node_id,
            loop_name: "test_loop",
            max_iterations: Some(3),
        } if *node_id == loop_id,
        "OnLoopIteration"   => GraphEvent::LoopIteration { node_id, loop_name: "test_loop", iteration: 0 } if *node_id == loop_id,
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "loop_body" } if *node_id == body_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "loop_body", duration } if *node_id == body_id && !duration.is_zero(),
        "OnLoopIteration"   => GraphEvent::LoopIteration { node_id, loop_name: "test_loop", iteration: 1 } if *node_id == loop_id,
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "loop_body" } if *node_id == body_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "loop_body", duration } if *node_id == body_id && !duration.is_zero(),
        "OnLoopIteration"   => GraphEvent::LoopIteration { node_id, loop_name: "test_loop", iteration: 2 } if *node_id == loop_id,
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "loop_body" } if *node_id == body_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "loop_body", duration } if *node_id == body_id && !duration.is_zero(),
        "OnLoopEnd"         => GraphEvent::LoopEnd {
            node_id,
            loop_name: "test_loop",
            iterations: 3,
            nodes_executed: 3,
            duration,
        } if *node_id == loop_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 4, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn parallel_hooks() {
    #[system]
    async fn branch_a() -> i32 {
        1
    }
    #[system]
    async fn branch_b() -> i32 {
        2
    }

    let mut graph = Graph::new();
    let mut branch_a_id = None;
    let mut branch_b_id = None;
    graph.add_parallel(
        "test_parallel",
        vec![
            Box::new(|g: &mut Graph| {
                branch_a_id = Some(g.add_system_node(branch_a));
            }) as Box<dyn FnOnce(&mut Graph)>,
            Box::new(|g: &mut Graph| {
                branch_b_id = Some(g.add_system_node(branch_b));
            }),
        ],
    );
    let parallel_id = node_id_by_name(&graph, "test_parallel");
    let branch_a_id = branch_a_id.unwrap();
    let branch_b_id = branch_b_id.unwrap();

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"        => GraphEvent::GraphStart { node_count: 4 },
        "OnParallelStart"     => GraphEvent::ParallelStart {
            node_id,
            node_name: "test_parallel",
            branch_count: 2,
        } if *node_id == parallel_id,
        "OnSystemStart"       => GraphEvent::SystemStart { node_id, system_name: "branch_a" } if *node_id == branch_a_id,
        "OnSystemComplete"    => GraphEvent::SystemComplete { node_id, system_name: "branch_a", duration } if *node_id == branch_a_id && !duration.is_zero(),
        "OnSystemStart"       => GraphEvent::SystemStart { node_id, system_name: "branch_b" } if *node_id == branch_b_id,
        "OnSystemComplete"    => GraphEvent::SystemComplete { node_id, system_name: "branch_b", duration } if *node_id == branch_b_id && !duration.is_zero(),
        "OnParallelComplete"  => GraphEvent::ParallelComplete {
            node_id,
            node_name: "test_parallel",
            branch_count: 2,
            total_nodes_executed: 2,
            duration,
        } if *node_id == parallel_id && !duration.is_zero(),
        "OnGraphComplete"     => GraphEvent::GraphComplete { nodes_executed: 4, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn switch_hooks() {
    let mut graph = Graph::new();
    let switch_sys_id = graph.add_boxed_system(Box::new(SwitchKeySystem { key: "alpha" }));
    let mut alpha_id = None;
    graph.add_switch::<SwitchOutput, _, _, Box<dyn FnOnce(&mut Graph)>>(
        "test_switch",
        |out| out.key,
        vec![
            (
                "alpha",
                Box::new(|g: &mut Graph| {
                    alpha_id = Some(g.add_boxed_system(Box::new(SuccessSystem)));
                }) as Box<dyn FnOnce(&mut Graph)>,
            ),
            (
                "beta",
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(SuccessSystem));
                }),
            ),
        ],
        None,
    );
    let switch_id = node_id_by_name(&graph, "test_switch");
    let alpha_id = alpha_id.unwrap();

    let (result, log) = execute_with_hooks(&graph).await;
    assert!(result.is_ok());

    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 4 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "switch_key_system" } if *node_id == switch_sys_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "switch_key_system", duration } if *node_id == switch_sys_id && !duration.is_zero(),
        "OnSwitchStart"     => GraphEvent::SwitchStart {
            node_id,
            node_name: "test_switch",
            case_count: 2,
            has_default: false,
        } if *node_id == switch_id,
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_system" } if *node_id == alpha_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_system", duration } if *node_id == alpha_id && !duration.is_zero(),
        "OnSwitchComplete"  => GraphEvent::SwitchComplete {
            node_id,
            node_name: "test_switch",
            selected_case: "alpha",
            used_default: false,
        } if *node_id == switch_id,
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 3, duration } if !duration.is_zero(),
    ]);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Custom Schedule Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn marker_fires_for_marked_system() {
    #[system]
    async fn success_fn() -> i32 {
        42
    }

    let mut graph = Graph::new();
    let sys_id = graph.add_system_node((MarkerA, success_fn));

    let (result, log) = execute_with_custom_hooks(&graph, |hooks, log| {
        register_recording_hooks!(hooks, log, MarkerA => "MarkerA");
    })
    .await;
    assert!(result.is_ok());

    // Built-in hooks fire before markers at each lifecycle point.
    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 1 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_fn"} if *node_id == sys_id,
        "MarkerA"           => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == sys_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == sys_id && !duration.is_zero(),
        "MarkerA"           => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == sys_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 1, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn unmarked_system_does_not_fire_markers() {
    #[system]
    async fn success_fn() -> i32 {
        42
    }

    let mut graph = Graph::new();
    // First system: plain, no markers
    let unmarked_id = graph.add_boxed_system(Box::new(SuccessSystem));
    // Second system: marked with MarkerA
    let marked_id = graph.add_system_node((MarkerA, success_fn));

    let (result, log) = execute_with_custom_hooks(&graph, |hooks, log| {
        register_recording_hooks!(hooks, log, MarkerA => "MarkerA");
    })
    .await;
    assert!(result.is_ok());

    // MarkerA only fires for the second (marked) system, not the first.
    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 2 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_system" } if *node_id == unmarked_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_system", duration } if *node_id == unmarked_id && !duration.is_zero(),
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == marked_id,
        "MarkerA"           => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == marked_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == marked_id && !duration.is_zero(),
        "MarkerA"           => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == marked_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 2, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn multiple_markers_all_fire() {
    #[system]
    async fn success_fn() -> i32 {
        42
    }

    let mut graph = Graph::new();
    let sys_id = graph.add_system_node(((MarkerA, MarkerB), success_fn));

    let (result, log) = execute_with_custom_hooks(&graph, |hooks, log| {
        register_recording_hooks!(hooks, log, MarkerA => "MarkerA", MarkerB => "MarkerB");
    })
    .await;
    assert!(result.is_ok());

    // Both markers fire in schedule order (A before B) at each lifecycle point.
    assert_event_sequence!(log, [
        "OnGraphStart"      => GraphEvent::GraphStart { node_count: 1 },
        "OnSystemStart"     => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == sys_id,
        "MarkerA"           => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == sys_id,
        "MarkerB"           => GraphEvent::SystemStart { node_id, system_name: "success_fn" } if *node_id == sys_id,
        "OnSystemComplete"  => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == sys_id && !duration.is_zero(),
        "MarkerA"           => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == sys_id && !duration.is_zero(),
        "MarkerB"           => GraphEvent::SystemComplete { node_id, system_name: "success_fn", duration } if *node_id == sys_id && !duration.is_zero(),
        "OnGraphComplete"   => GraphEvent::GraphComplete { nodes_executed: 1, duration } if !duration.is_zero(),
    ]);
}

#[tokio::test]
async fn marker_fires_on_system_error() {
    #[system]
    async fn error_fn() -> Result<(), SystemError> {
        Err(SystemError::ExecutionError("intentional failure".into()))
    }

    let mut graph = Graph::new();
    let failing_id = graph.add_system_node((MarkerA, error_fn));

    let (result, log) = execute_with_custom_hooks(&graph, |hooks, log| {
        register_recording_hooks!(hooks, log, MarkerA => "MarkerA");
    })
    .await;
    assert!(result.is_err(), "failing system should produce an error");

    assert_event_sequence!(log, [
        "OnGraphStart"    => GraphEvent::GraphStart { node_count: 1 },
        "OnSystemStart"   => GraphEvent::SystemStart { node_id, system_name: "error_fn" } if *node_id == failing_id,
        "MarkerA"         => GraphEvent::SystemStart { node_id, system_name: "error_fn" } if *node_id == failing_id,
        "OnSystemError"   => GraphEvent::SystemError { node_id, system_name: "error_fn", error } if *node_id == failing_id && error.contains("intentional failure"),
        "MarkerA"         => GraphEvent::SystemError { node_id, system_name: "error_fn", error } if *node_id == failing_id && error.contains("intentional failure"),
        "OnGraphFailure"  => GraphEvent::GraphFailure { error } if matches!(error, ExecutionError::SystemError(_)),
    ]);
}
