//! Graph-level behaviour of optional global reads (`Option<Res<T>>`).
//!
//! An optional global read is not a requirement: pre-run resource checks,
//! signatures, scope contracts and dynamic-slot matching must all ignore it,
//! while `GraphExecutor::absent_optional_globals` reports it when the server
//! has no such global.

use polaris_graph::{
    AbsentOptionalGlobal, ContextPolicy, DynamicSlot, ExecutionError, Graph, GraphExecutor,
    GraphSignature, NodeId, ResourceValidationError, SubgraphRegistry,
};
use polaris_system::param::{Res, SystemContext};
use polaris_system::resource::{GlobalResource, LocalResource, Resources};
use polaris_system::system;
use polaris_system::system::SystemError;
use std::any::{TypeId, type_name};
use std::sync::Arc;

#[derive(Debug)]
struct Approval {
    approver: &'static str,
}

impl GlobalResource for Approval {}

#[derive(Debug)]
struct Audit;

impl GlobalResource for Audit {}

/// A local resource used to prove required reads are still checked.
#[derive(Debug)]
struct Memory;

impl LocalResource for Memory {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Decision {
    approved_by: Option<&'static str>,
}

#[system]
async fn ask_human(approval: Option<Res<Approval>>) -> Decision {
    Decision {
        approved_by: approval.map(|approval| approval.approver),
    }
}

#[system]
async fn ask_human_twice(_first: Option<Res<Approval>>, _second: Option<Res<Approval>>) {}

#[system]
async fn ask_human_with_audit(_approval: Option<Res<Approval>>, _audit: Option<Res<Audit>>) {}

#[system]
async fn may_fail() -> Result<(), SystemError> {
    Err(SystemError::ExecutionError("approval needed".into()))
}

#[system]
async fn needs_memory(_memory: Res<Memory>) -> u8 {
    0
}

fn ctx_without_globals() -> SystemContext<'static> {
    SystemContext::with_globals(Arc::new(Resources::new()))
}

fn ctx_with_approval() -> SystemContext<'static> {
    let mut globals = Resources::new();
    globals.insert(Approval { approver: "ops" });
    SystemContext::with_globals(Arc::new(globals))
}

fn ask_human_graph() -> Graph {
    let mut graph = Graph::new();
    graph.add_system(ask_human);
    graph
}

fn assert_single_absent_approval(absent: &[AbsentOptionalGlobal], expected_node: &NodeId) {
    assert_eq!(absent.len(), 1, "got {absent:?}");
    assert_eq!(&absent[0].node, expected_node, "got {absent:?}");
    assert_eq!(
        absent[0].type_id,
        TypeId::of::<Approval>(),
        "got {absent:?}"
    );
    assert_eq!(
        absent[0].resource_type,
        type_name::<Approval>(),
        "got {absent:?}"
    );
    assert!(
        absent[0].system_name.contains("ask_human"),
        "got {absent:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Pre-run resource check
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_resources_passes_when_optional_global_is_absent() {
    let graph = ask_human_graph();

    let result = GraphExecutor::new().validate_resources(&graph, &ctx_without_globals(), None);

    assert!(
        result.is_ok(),
        "an absent optional global is allowed: {result:?}"
    );
}

#[test]
fn validate_resources_still_rejects_missing_required_resource() {
    let mut graph = ask_human_graph();
    graph.add_system(needs_memory);

    let errors = GraphExecutor::new()
        .validate_resources(&graph, &ctx_without_globals(), None)
        .expect_err("the required Res<Memory> is missing");

    assert_eq!(errors.len(), 1, "only the required read fails: {errors:?}");
    match &errors[0] {
        ResourceValidationError::MissingResource { type_id, .. } => {
            assert_eq!(*type_id, TypeId::of::<Memory>());
        }
        other => panic!("expected MissingResource for Memory, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Execution
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn graph_runs_with_absent_optional_global() {
    let graph = ask_human_graph();
    let mut ctx = ctx_without_globals();

    assert!(graph.validate().is_ok());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("an absent optional global must not fail the run");

    assert_eq!(
        result.output::<Decision>(),
        Some(&Decision { approved_by: None })
    );
}

#[tokio::test]
async fn graph_runs_with_present_optional_global() {
    let graph = ask_human_graph();
    let mut ctx = ctx_with_approval();

    assert!(graph.validate().is_ok());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("run must succeed");

    assert_eq!(
        result.output::<Decision>(),
        Some(&Decision {
            approved_by: Some("ops")
        })
    );
}

#[tokio::test]
async fn isolated_scope_still_sees_optional_global() {
    // A scope that shares nothing with its parent still sees globals.
    let mut graph = Graph::new();
    graph.add_scope("isolated", ask_human_graph(), ContextPolicy::new());
    let mut ctx = ctx_with_approval();

    assert!(graph.validate().is_ok());

    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .expect("run must succeed");

    assert_eq!(
        result.output::<Decision>(),
        Some(&Decision {
            approved_by: Some("ops")
        })
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// absent_optional_globals report
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn report_lists_absent_top_level_read() {
    let graph = ask_human_graph();
    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &graph.nodes()[0].id());
}

#[test]
fn report_is_empty_when_global_present() {
    let absent =
        GraphExecutor::new().absent_optional_globals(&ask_human_graph(), &ctx_with_approval());

    assert!(absent.is_empty(), "got {absent:?}");
}

#[test]
fn report_lists_absent_read_for_context_without_globals() {
    let graph = ask_human_graph();
    let absent = GraphExecutor::new().absent_optional_globals(&graph, &SystemContext::new());

    assert_single_absent_approval(&absent, &graph.nodes()[0].id());
}

#[test]
fn report_ignores_required_reads() {
    let mut graph = Graph::new();
    graph.add_system(needs_memory);

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert!(
        absent.is_empty(),
        "required reads belong to validate_resources: {absent:?}"
    );
}

#[test]
fn report_walks_isolated_scope_interior() {
    let inner = ask_human_graph();
    let expected_node = inner.nodes()[0].id();
    let mut graph = Graph::new();
    graph.add_scope("isolated", inner, ContextPolicy::new());

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &expected_node);
}

#[test]
fn report_walks_loop_body() {
    let mut graph = Graph::new();
    let mut expected_node = None;
    graph.add_loop_n("retry", 2, |body| {
        expected_node = Some(body.add_system_node(ask_human));
    });

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &expected_node.unwrap());
}

#[test]
fn report_walks_both_conditional_branch_interiors() {
    let mut graph = Graph::new();
    let mut true_node = None;
    let mut false_node = None;
    graph.add_conditional_branch::<bool, _, _, _>(
        "approval route",
        |flag| *flag,
        |branch| {
            true_node = Some(branch.add_system_node(ask_human));
        },
        |branch| {
            false_node = Some(branch.add_system_node(ask_human));
        },
    );

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_eq!(absent.len(), 2, "got {absent:?}");
    assert_single_absent_approval(&absent[..1], &true_node.unwrap());
    assert_single_absent_approval(&absent[1..], &false_node.unwrap());
}

#[test]
fn report_walks_parallel_branch_interior() {
    let mut graph = Graph::new();
    let mut expected_node = None;
    graph.add_parallel(
        "approval",
        [|branch: &mut Graph| {
            expected_node = Some(branch.add_system_node(ask_human));
        }],
    );

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &expected_node.unwrap());
}

#[test]
fn report_walks_error_handler_interior() {
    let mut graph = Graph::new();
    graph.add_system(may_fail);
    let mut expected_node = None;
    graph.add_error_handler(|handler| {
        expected_node = Some(handler.add_system_node(ask_human));
    });
    assert!(graph.validate().is_ok());

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &expected_node.unwrap());
}

#[test]
fn report_walks_both_inline_dynamic_candidates() {
    let first = ask_human_graph();
    let first_node = first.nodes()[0].id();
    let second = ask_human_graph();
    let second_node = second.nodes()[0].id();
    let slot = DynamicSlot::new(
        GraphSignature::new().produce::<Decision>(),
        ContextPolicy::shared(),
    );
    let mut graph = Graph::new();
    graph.add_dynamic(
        "route",
        |_ctx| "first",
        [("first", first), ("second", second)],
        slot,
    );
    assert!(graph.validate().is_ok());

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_eq!(absent.len(), 2, "got {absent:?}");
    assert_single_absent_approval(&absent[..1], &first_node);
    assert_single_absent_approval(&absent[1..], &second_node);
}

#[test]
fn report_walks_isolated_inline_dynamic_candidate() {
    let candidate = ask_human_graph();
    let expected_node = candidate.nodes()[0].id();
    let slot = DynamicSlot::new(
        GraphSignature::new().produce::<Decision>(),
        ContextPolicy::new(),
    );
    let mut graph = Graph::new();
    graph.add_dynamic("route", |_ctx| "ask", [("ask", candidate)], slot);
    assert!(graph.validate().is_ok());

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_single_absent_approval(&absent, &expected_node);
}

#[test]
fn report_lists_the_same_absent_type_once_per_optional_read() {
    // The report rustdoc promises "listed once per read", not once per type.
    let mut graph = Graph::new();
    let expected_node = graph.add_system_node(ask_human_twice);

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert_eq!(absent.len(), 2, "got {absent:?}");
    assert_single_absent_approval(&absent[..1], &expected_node);
    assert_single_absent_approval(&absent[1..], &expected_node);
    assert_eq!(absent[0], absent[1]);
}

#[test]
fn report_lists_only_the_absent_optional_global_in_a_system() {
    let mut globals = Resources::new();
    globals.insert(Audit);
    let ctx = SystemContext::with_globals(Arc::new(globals));
    let mut graph = Graph::new();
    let expected_node = graph.add_system_node(ask_human_with_audit);

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx);

    assert_single_absent_approval(&absent, &expected_node);
}

#[tokio::test]
async fn empty_report_past_recursion_cap_does_not_prove_absence() {
    let inner = ask_human_graph();
    let expected_node = inner.nodes()[0].id();
    let mut middle = Graph::new();
    middle.add_scope("middle", inner, ContextPolicy::shared());
    let mut graph = Graph::new();
    graph.add_scope("outer", middle, ContextPolicy::shared());
    let mut ctx = ctx_without_globals();
    let capped = GraphExecutor::new().with_max_recursion_depth(1);

    // The report is infallible and skips the read at depth 2, past the cap.
    let absent = capped.absent_optional_globals(&graph, &ctx);
    assert!(absent.is_empty(), "got {absent:?}");
    let errors = capped.validate_resources(&graph, &ctx, None).unwrap_err();
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ResourceValidationError::ValidationDepthExceeded { depth: 2, max: 1 }
        )),
        "got {errors:?}"
    );

    // Under the default cap, the same context and graph expose the read.
    let uncapped = GraphExecutor::new();
    assert_single_absent_approval(
        &uncapped.absent_optional_globals(&graph, &ctx),
        &expected_node,
    );
    assert!(uncapped.validate_resources(&graph, &ctx, None).is_ok());

    // Execution rejects the nesting before the deepest system can run.
    assert!(graph.validate().is_ok());
    let error = capped
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ExecutionError::RecursionLimitExceeded { .. }),
        "got {error:?}"
    );
}

#[test]
fn report_skips_registry_backed_candidates() {
    // Registry candidates are only known at run time, so they are not listed.
    let contract = GraphSignature::new().produce::<Decision>();
    let mut graph = Graph::new();
    graph.add_dynamic_registry(
        "route",
        |_ctx| "ask",
        DynamicSlot::new(contract, ContextPolicy::shared()),
    );

    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    assert!(absent.is_empty(), "got {absent:?}");
}

#[test]
fn report_display_names_system_node_and_type() {
    let graph = ask_human_graph();
    let expected_node = graph.nodes()[0].id();
    let absent = GraphExecutor::new().absent_optional_globals(&graph, &ctx_without_globals());

    let message = absent[0].to_string();

    assert!(message.contains("ask_human"), "got {message:?}");
    assert!(
        message.contains(&format!("({expected_node})")),
        "got {message:?}"
    );
    assert!(message.contains(type_name::<Approval>()), "got {message:?}");
    assert!(message.contains("None"), "got {message:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Signatures, contracts and slot matching
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn signature_has_no_requirement_for_optional_global() {
    let signature = ask_human_graph().signature();

    assert!(
        signature.requires().is_empty(),
        "optional reads are not requirements: {:?}",
        signature.requires()
    );
    assert!(signature.compatible_with(&GraphSignature::new().produce::<Decision>()));
}

#[test]
fn exact_slot_admits_candidate_with_optional_global() {
    let slot = DynamicSlot::new(
        GraphSignature::new().produce::<Decision>(),
        ContextPolicy::shared(),
    );
    let mut graph = Graph::new();
    graph.add_dynamic("route", |_ctx| "ask", [("ask", ask_human_graph())], slot);

    let result = graph.validate();

    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn registry_admits_candidate_with_optional_global() {
    let mut registry = SubgraphRegistry::new(GraphSignature::new().produce::<Decision>());

    let result = registry.register("ask", ask_human_graph());

    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn declared_scope_contract_need_not_list_optional_global() {
    let mut graph = Graph::new();
    graph.add_scope_with_contract(
        "approval",
        ask_human_graph(),
        ContextPolicy::new(),
        GraphSignature::new().produce::<Decision>(),
    );

    let result = graph.validate();

    assert!(result.is_ok(), "no contract drift expected: {result:?}");
    assert!(
        GraphExecutor::new()
            .validate_resources(&graph, &ctx_without_globals(), None)
            .is_ok(),
        "the contract demands no resources, so an absent global passes"
    );
}
