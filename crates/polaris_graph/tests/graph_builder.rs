//! Tests for the Graph builder API.
//!
//! These tests verify the graph construction functionality:
//! - Creating empty graphs
//! - Adding system nodes
//! - Sequential chaining
//! - Conditional branches
//! - Parallel branches
//! - Loops (predicate-based and iteration-based)
//! - Complex graph compositions

use polaris_graph::graph::Graph;
use polaris_graph::node::Node;

// ─────────────────────────────────────────────────────────────────────────────
// Test Systems
// ─────────────────────────────────────────────────────────────────────────────

async fn test_system() -> String {
    "hello".to_string()
}

async fn first_step() -> i32 {
    1
}

async fn second_step() -> i32 {
    2
}

async fn third_step() -> i32 {
    3
}

async fn before_decision() -> bool {
    true
}

async fn true_path_system() -> String {
    "true".to_string()
}

async fn false_path_system() -> String {
    "false".to_string()
}

async fn after_decision() -> String {
    "after".to_string()
}

async fn branch_a() -> i32 {
    1
}

async fn branch_b() -> i32 {
    2
}

async fn loop_body() -> i32 {
    42
}

async fn reason() -> String {
    "reasoning".to_string()
}

async fn invoke_tool() -> String {
    "tool_result".to_string()
}

async fn observe() -> String {
    "observed".to_string()
}

async fn respond() -> String {
    "response".to_string()
}

async fn finalize() -> String {
    "done".to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Basic Graph Creation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn new_graph_is_empty() {
    let graph = Graph::new();
    assert!(graph.is_empty());
    assert_eq!(graph.node_count(), 0);
    assert_eq!(graph.edge_count(), 0);
    assert!(graph.entry().is_none());
}

#[test]
fn add_single_system() {
    let mut graph = Graph::new();
    graph.add_system(test_system);

    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.edge_count(), 0);
    assert!(graph.entry().is_some());

    let node = graph.get_node(graph.entry().unwrap()).unwrap();
    // Name contains the function path
    assert!(node.name().contains("test_system"));
}

#[test]
fn add_sequential_systems() {
    let mut graph = Graph::new();
    graph
        .add_system(first_step)
        .add_system(second_step)
        .add_system(third_step);

    assert_eq!(graph.node_count(), 3);
    assert_eq!(graph.edge_count(), 2); // first->second, second->third
}

#[test]
fn system_node_stores_type_info() {
    use std::any::TypeId;

    let mut graph = Graph::new();
    graph.add_system(first_step); // returns i32

    let node = graph.get_node(graph.entry().unwrap()).unwrap();
    if let Node::System(sys_node) = node {
        assert_eq!(sys_node.output_type_id(), TypeId::of::<i32>());
        assert!(sys_node.output_type_name().contains("i32"));
    } else {
        panic!("Expected SystemNode");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conditional Branches
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DecisionOutput {
    should_branch: bool,
}

async fn decision_system() -> DecisionOutput {
    DecisionOutput {
        should_branch: true,
    }
}

#[test]
fn add_conditional_branch() {
    let mut graph = Graph::new();
    graph
        .add_system(before_decision)
        .add_system(decision_system)
        .add_conditional_branch::<DecisionOutput, _, _, _>(
            "decision",
            |output| output.should_branch,
            |g| {
                g.add_system(true_path_system);
            },
            |g| {
                g.add_system(false_path_system);
            },
        )
        .add_system(after_decision);

    // Nodes: before, decision_system, decision, true_path, false_path, after
    assert!(graph.node_count() >= 5);
}

// ─────────────────────────────────────────────────────────────────────────────
// Parallel Branches
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn add_parallel_branches() {
    let mut graph = Graph::new();
    graph.add_parallel(
        "parallel",
        vec![
            |g: &mut Graph| {
                g.add_system(branch_a);
            },
            |g: &mut Graph| {
                g.add_system(branch_b);
            },
        ],
    );

    // Nodes: parallel, branch_a, branch_b
    assert!(graph.node_count() >= 3);
}

// ─────────────────────────────────────────────────────────────────────────────
// Loops
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct LoopState {
    #[expect(dead_code, reason = "used for testing struct completeness")]
    iteration: i32,
    done: bool,
}

async fn loop_init() -> LoopState {
    LoopState {
        iteration: 0,
        done: false,
    }
}

#[test]
fn add_loop_with_predicate() {
    let mut graph = Graph::new();
    graph.add_system(loop_init).add_loop::<LoopState, _, _>(
        "loop",
        |state| state.done,
        |g| {
            g.add_system(loop_body);
        },
    );

    // Nodes: loop_init, loop, loop_body
    assert!(graph.node_count() >= 3);
}

#[test]
fn add_loop_with_iterations() {
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 10, |g| {
        g.add_system(loop_body);
    });

    // Nodes: loop, loop_body
    assert!(graph.node_count() >= 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Complex Graphs
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ReasoningResult {
    needs_tool: bool,
}

async fn reasoning() -> ReasoningResult {
    ReasoningResult { needs_tool: true }
}

#[test]
fn complex_graph() {
    let mut graph = Graph::new();
    graph
        .add_system(reason)
        .add_system(reasoning)
        .add_conditional_branch::<ReasoningResult, _, _, _>(
            "needs_tool",
            |result| result.needs_tool,
            |g| {
                g.add_system(invoke_tool).add_system(observe);
            },
            |g| {
                g.add_system(respond);
            },
        )
        .add_system(finalize);

    assert!(!graph.is_empty());
    assert!(graph.entry().is_some());
}

// ─────────────────────────────────────────────────────────────────────────────
// ID Allocation Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Verifies that the shared `IdAllocator` ensures unique IDs across all
/// subgraphs, regardless of nesting depth.
#[test]
fn no_id_collision_in_deep_nesting() {
    use polaris_graph::edge::Edge;
    use polaris_graph::node::Node;
    use std::collections::HashSet;

    let mut graph = Graph::new();

    // Build a deeply nested structure:
    // parallel -> [
    //   loop -> conditional -> [true_branch, false_branch],
    //   loop -> system
    // ]
    graph.add_parallel(
        "outer_parallel",
        vec![
            |g: &mut Graph| {
                g.add_loop_n("inner_loop_1", 3, |g| {
                    g.add_system(first_step)
                        .add_conditional_branch::<i32, _, _, _>(
                            "nested_decision",
                            |_| true,
                            |g| {
                                g.add_system(true_path_system);
                            },
                            |g| {
                                g.add_system(false_path_system);
                            },
                        );
                });
            },
            |g: &mut Graph| {
                g.add_loop_n("inner_loop_2", 2, |g| {
                    g.add_system(second_step);
                });
            },
            |g: &mut Graph| {
                g.add_system(third_step);
            },
        ],
    );

    // Collect all node IDs
    let node_ids: HashSet<_> = graph.nodes().iter().map(Node::id).collect();

    // All node IDs should be unique (set size equals node count)
    assert_eq!(
        node_ids.len(),
        graph.node_count(),
        "Node ID collision detected! Expected {} unique IDs but found {}",
        graph.node_count(),
        node_ids.len()
    );

    // Collect all edge IDs
    let edge_ids: HashSet<_> = graph.edges().iter().map(Edge::id).collect();

    // All edge IDs should be unique
    assert_eq!(
        edge_ids.len(),
        graph.edge_count(),
        "Edge ID collision detected! Expected {} unique IDs but found {}",
        graph.edge_count(),
        edge_ids.len()
    );
}

/// Verifies sequential ID allocation across subgraphs.
#[test]
fn ids_are_sequential_across_subgraphs() {
    let mut graph = Graph::new();

    // Add systems and a conditional branch
    graph
        .add_system(first_step)
        .add_system(second_step)
        .add_conditional_branch::<i32, _, _, _>(
            "branch",
            |_| true,
            |g| {
                g.add_system(true_path_system);
            },
            |g| {
                g.add_system(false_path_system);
            },
        )
        .add_system(third_step);

    // Collect node IDs
    let node_ids: Vec<_> = graph.nodes().iter().map(Node::id).collect();

    // Verify all IDs are unique (no collisions)
    let unique_ids: std::collections::HashSet<_> = node_ids.iter().collect();
    assert_eq!(
        node_ids.len(),
        unique_ids.len(),
        "All node IDs should be unique, found {} duplicates",
        node_ids.len() - unique_ids.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// SystemNodeBuilder
// ─────────────────────────────────────────────────────────────────────────────

async fn fallback_system() -> String {
    "fallback".to_string()
}

async fn timeout_handler() -> String {
    "timeout".to_string()
}

#[test]
fn system_builder_on_error_attaches_error_handler() {
    let mut graph = Graph::new();
    graph
        .system(first_step)
        .on_error(|g| {
            g.add_system(fallback_system);
        })
        .done()
        .add_system(second_step);

    // Should have: first_step, fallback_system, second_step = 3 nodes
    assert_eq!(graph.node_count(), 3);
    // Should have error edge + sequential edge = at least 2 edges
    assert!(graph.edge_count() >= 2);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn system_builder_with_timeout_and_on_timeout() {
    use std::time::Duration;

    let mut graph = Graph::new();
    graph
        .system(first_step)
        .with_timeout(Duration::from_secs(30))
        .on_timeout(|g| {
            g.add_system(timeout_handler);
        })
        .done()
        .add_system(second_step);

    // first_step, timeout_handler, second_step = 3 nodes
    assert_eq!(graph.node_count(), 3);

    // Verify timeout was set on the system node
    let entry = graph.entry().unwrap();
    if let Node::System(sys) = graph.get_node(entry).unwrap() {
        assert_eq!(sys.timeout, Some(Duration::from_secs(30)));
    } else {
        panic!("Expected system node");
    }

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn system_builder_on_error_and_on_timeout_chaining() {
    use std::time::Duration;

    let mut graph = Graph::new();
    graph
        .system(first_step)
        .on_error(|g| {
            g.add_system(fallback_system);
        })
        .with_timeout(Duration::from_secs(10))
        .on_timeout(|g| {
            g.add_system(timeout_handler);
        });

    // first_step, fallback_system, timeout_handler = 3 nodes
    assert_eq!(graph.node_count(), 3);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn system_builder_done_continues_fluent_chain() {
    let mut graph = Graph::new();
    graph
        .system(first_step)
        .done()
        .add_system(second_step)
        .add_system(third_step);

    assert_eq!(graph.node_count(), 3);
    assert_eq!(graph.edge_count(), 2); // first->second, second->third
}

#[test]
fn system_builder_id_returns_correct_node_id() {
    let mut graph = Graph::new();
    let builder = graph.system(first_step);
    let id = builder.id();

    // The id should match the entry point since it's the first node
    assert_eq!(graph.entry().unwrap(), id);
}

#[test]
fn system_builder_node_connected_sequentially() {
    let mut graph = Graph::new();
    graph.add_system(first_step);
    graph.system(second_step);

    // first_step → second_step via sequential edge
    assert_eq!(graph.node_count(), 2);
    assert_eq!(graph.edge_count(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pipe (reusable graph fragments)
// ─────────────────────────────────────────────────────────────────────────────

fn reusable_fragment(g: &mut Graph) {
    g.add_system(second_step).add_system(third_step);
}

#[test]
fn pipe_preserves_fluent_chain() {
    // first_step -> second_step -> third_step -> finalize
    let mut graph = Graph::new();
    graph
        .add_system(first_step)
        .pipe(reusable_fragment)
        .add_system(finalize);

    assert_eq!(graph.node_count(), 4);
    assert_eq!(graph.edge_count(), 3);
}

#[test]
fn pipe_sets_entry_when_first() {
    let mut graph = Graph::new();
    graph.pipe(|g| {
        g.add_system(first_step);
    });

    assert_eq!(graph.node_count(), 1);
    assert!(graph.entry().is_some());
}

#[test]
fn pipe_with_empty_closure_is_noop() {
    let mut graph = Graph::new();
    graph
        .add_system(first_step)
        .pipe(|_| {})
        .add_system(second_step);

    assert_eq!(graph.node_count(), 2);
    assert_eq!(graph.edge_count(), 1);
}

#[test]
fn pipe_composes_multiple_fragments() {
    fn frag_a(g: &mut Graph) {
        g.add_system(first_step);
    }
    fn frag_b(g: &mut Graph) {
        g.add_system(second_step);
    }
    fn frag_c(g: &mut Graph) {
        g.add_system(third_step);
    }

    let mut graph = Graph::new();
    graph.pipe(frag_a).pipe(frag_b).pipe(frag_c);

    assert_eq!(graph.node_count(), 3);
    assert_eq!(graph.edge_count(), 2);
}

#[test]
fn pipe_works_with_control_flow_inside() {
    fn conditional_fragment(g: &mut Graph) {
        g.add_conditional_branch::<bool, _, _, _>(
            "inner_decision",
            |val| *val,
            |g| {
                g.add_system(true_path_system);
            },
            |g| {
                g.add_system(false_path_system);
            },
        );
    }

    let mut graph = Graph::new();
    graph
        .add_system(before_decision)
        .pipe(conditional_fragment)
        .add_system(finalize);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

// ─────────────────────────────────────────────────────────────────────────────
// Append (graph + graph sequential composition)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn append_connects_two_graphs() {
    let mut left = Graph::new();
    left.add_system(first_step).add_system(second_step);

    let mut right = Graph::new();
    right.add_system(third_step).add_system(finalize);

    left.append(right).unwrap();

    assert_eq!(left.node_count(), 4);
    // first->second, second->third (append edge), third->finalize
    assert_eq!(left.edge_count(), 3);
    assert!(left.entry().is_some());

    let result = left.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn append_empty_other_is_noop() {
    let mut graph = Graph::new();
    graph.add_system(first_step);

    let empty = Graph::new();
    graph.append(empty).unwrap();

    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn append_into_empty_self_adopts_other() {
    let mut empty = Graph::new();

    let mut other = Graph::new();
    other.add_system(first_step).add_system(second_step);

    empty.append(other).unwrap();

    assert_eq!(empty.node_count(), 2);
    assert_eq!(empty.edge_count(), 1);
    assert!(empty.entry().is_some());
}

#[test]
fn append_with_control_flow() {
    let mut left = Graph::new();
    left.add_system(before_decision)
        .add_conditional_branch::<bool, _, _, _>(
            "decision",
            |val| *val,
            |g| {
                g.add_system(true_path_system);
            },
            |g| {
                g.add_system(false_path_system);
            },
        );

    let mut right = Graph::new();
    right.add_system(finalize);

    left.append(right).unwrap();

    let result = left.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn last_node_accessor() {
    let mut graph = Graph::new();
    assert!(graph.last_node().is_none());

    graph.add_system(first_step);
    assert!(graph.last_node().is_some());

    graph.add_system(second_step);
    let last = graph.last_node().unwrap();
    // last_node should differ from entry (which is first_step)
    assert_ne!(graph.entry().unwrap(), last);
}

// ─────────────────────────────────────────────────────────────────────────────
// Global error handler (add_error_handler)
// ─────────────────────────────────────────────────────────────────────────────

mod test_utils;

use polaris_graph::edge::Edge;
use test_utils::{FailingSystem, SuccessSystem};

#[test]
fn add_error_handler_wires_fallible_nodes() {
    let mut graph = Graph::new();

    // FailingSystem is fallible (is_fallible() == true)
    let fallible_id = graph.add_boxed_system(Box::new(FailingSystem));
    // SuccessSystem is infallible (is_fallible() == false)
    let _infallible_id = graph.add_boxed_system(Box::new(SuccessSystem));

    graph.add_error_handler(|g| {
        g.add_system(fallback_system);
    });

    // Count error edges
    let error_edges: Vec<_> = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)))
        .collect();

    // Only the fallible node should have an error edge
    assert_eq!(error_edges.len(), 1);
    assert_eq!(error_edges[0].from(), fallible_id);
}

#[test]
fn add_error_handler_skips_existing_error_edges() {
    let mut graph = Graph::new();

    // Two fallible nodes
    let fallible_a = graph.add_boxed_system(Box::new(FailingSystem));
    let fallible_b = graph.add_boxed_system(Box::new(FailingSystem));

    // Manually wire an error handler for fallible_a
    graph.add_error_handler_for(fallible_a.clone(), |g| {
        g.add_system(respond);
    });

    // Now add global error handler — should only wire fallible_b
    graph.add_error_handler(|g| {
        g.add_system(fallback_system);
    });

    // Count error edges sourced from each fallible node
    let errors_from_a = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)) && edge.from() == fallible_a)
        .count();
    let errors_from_b = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)) && edge.from() == fallible_b)
        .count();

    // fallible_a: 1 (manual), fallible_b: 1 (global)
    assert_eq!(errors_from_a, 1);
    assert_eq!(errors_from_b, 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Closure-based error handler (add_error_handler_fn)
// ─────────────────────────────────────────────────────────────────────────────

use polaris_graph::CaughtError;

#[test]
fn error_handler_fn_wires_to_all_fallible_systems() {
    let mut graph = Graph::new();

    let fallible_id = graph.add_boxed_system(Box::new(FailingSystem));
    let _infallible_id = graph.add_boxed_system(Box::new(SuccessSystem));

    graph.add_error_handler_fn(|_error: &CaughtError| -> String { "handled".to_string() });

    let error_edges: Vec<_> = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)))
        .collect();

    assert_eq!(error_edges.len(), 1);
    assert_eq!(error_edges[0].from(), fallible_id);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[tokio::test]
async fn error_handler_fn_executes_closure() {
    use polaris_graph::executor::GraphExecutor;
    use test_utils::create_test_server;

    let mut graph = Graph::new();

    graph.add_boxed_system(Box::new(FailingSystem));
    graph.add_error_handler_fn(|err: &CaughtError| -> String {
        format!("handled: {}", err.message)
    });

    let server = create_test_server();
    let hooks = test_utils::get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await
        .expect("execution should succeed via closure error handler");

    let output = result.output::<String>();
    assert!(output.is_some(), "closure output should be in the result");
    assert!(
        output.unwrap().starts_with("handled: "),
        "output should contain the handled message"
    );
}

#[test]
fn error_handler_fn_for_specific_nodes() {
    let mut graph = Graph::new();

    let fallible_a = graph.add_boxed_system(Box::new(FailingSystem));
    let fallible_b = graph.add_boxed_system(Box::new(FailingSystem));

    graph.add_error_handler_fn_for([fallible_a.clone()], |_err: &CaughtError| -> String {
        "handled".to_string()
    });

    let errors_from_a = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)) && edge.from() == fallible_a)
        .count();
    let errors_from_b = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)) && edge.from() == fallible_b)
        .count();

    assert_eq!(errors_from_a, 1, "fallible_a should have an error edge");
    assert_eq!(errors_from_b, 0, "fallible_b should NOT have an error edge");
}

#[test]
fn system_node_builder_on_error_fn() {
    let mut graph = Graph::new();
    graph
        .system_boxed(Box::new(FailingSystem))
        .on_error_fn(|_err: &CaughtError| -> String { "recovered".to_string() })
        .done()
        .add_system(second_step);

    assert_eq!(graph.node_count(), 3);

    let error_edge_count = graph
        .edges()
        .iter()
        .filter(|edge| matches!(edge, Edge::Error(_)))
        .count();
    assert_eq!(error_edge_count, 1);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[tokio::test]
async fn system_node_builder_on_error_fn_executes() {
    use polaris_graph::executor::GraphExecutor;
    use test_utils::create_test_server;

    let mut graph = Graph::new();
    graph
        .system_boxed(Box::new(FailingSystem))
        .on_error_fn(|err: &CaughtError| -> String { format!("recovered: {}", err.message) });

    let server = create_test_server();
    let hooks = test_utils::get_hooks(&server);
    let mut ctx = server.create_context();

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, hooks, None)
        .await
        .expect("execution should succeed via on_error_fn handler");

    let output = result.output::<String>();
    assert!(output.is_some(), "closure output should be in the result");
    assert!(
        output.unwrap().starts_with("recovered: "),
        "output should contain the recovered message"
    );
}
