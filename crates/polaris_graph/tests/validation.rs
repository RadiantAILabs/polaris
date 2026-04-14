//! Tests for Graph validation.
//!
//! These tests verify the `Graph::validate()` functionality:
//! - Entry point validation
//! - Node reference validation
//! - Decision node requirements
//! - Parallel node requirements
//! - Loop node requirements
//! - Scope node validation (empty graph, resource access rules)
//! - Error display formatting

mod test_utils;

use polaris_graph::CaughtError;
use polaris_graph::executor::{GraphExecutor, ResourceValidationError};
use polaris_graph::graph::{Graph, ValidationError, ValidationWarning};
use polaris_graph::hooks::HooksAPI;
use polaris_graph::hooks::schedule::OnGraphStart;
use polaris_graph::node::{ContextPolicy, NodeId};
use polaris_system::param::{
    Access, AccessMode, ERROR_CONTEXT, ErrOut, SystemAccess, SystemContext, SystemParam,
};
use polaris_system::resource::LocalResource;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::any::TypeId;
use test_utils::{ReadConfigSystem, SuccessSystem, TestConfig, WriteConfigSystem};

// ─────────────────────────────────────────────────────────────────────────────
// Test Systems
// ─────────────────────────────────────────────────────────────────────────────

async fn first_step() -> i32 {
    1
}

async fn second_step() -> i32 {
    2
}

async fn true_path_system() -> String {
    "true".to_string()
}

async fn false_path_system() -> String {
    "false".to_string()
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

// ─────────────────────────────────────────────────────────────────────────────
// Entry Point Validation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_empty_graph_fails() {
    let graph = Graph::new();
    let result = graph.validate();

    assert!(result.is_err());
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::NoEntryPoint))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Valid Graph Structures
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_simple_graph_succeeds() {
    let mut graph = Graph::new();
    graph.add_system(first_step).add_system(second_step);

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn validate_graph_with_conditional_branch_succeeds() {
    #[derive(Debug)]
    struct DecisionOutput {
        should_branch: bool,
    }

    async fn decision_system() -> DecisionOutput {
        DecisionOutput {
            should_branch: true,
        }
    }

    let mut graph = Graph::new();
    graph
        .add_system(decision_system)
        .add_conditional_branch::<DecisionOutput, _, _, _>(
            "branch",
            |output| output.should_branch,
            |g| {
                g.add_system(true_path_system);
            },
            |g| {
                g.add_system(false_path_system);
            },
        );

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn validate_graph_with_parallel_succeeds() {
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

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn validate_graph_with_loop_succeeds() {
    let mut graph = Graph::new();
    graph.add_loop_n("loop", 5, |g| {
        g.add_system(loop_body);
    });

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

// ─────────────────────────────────────────────────────────────────────────────
// Error Display Formatting
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validation_error_no_entry_point_display() {
    let err = ValidationError::NoEntryPoint;
    assert_eq!(format!("{err}"), "graph has no entry point");
}

#[test]
fn validation_error_invalid_entry_point_display() {
    let err = ValidationError::InvalidEntryPoint(NodeId::from_string("5"));
    let msg = format!("{err}");
    assert!(msg.contains("invalid node"));
    assert!(msg.contains("node_5"));
}

#[test]
fn validation_error_missing_predicate_display() {
    let err = ValidationError::MissingPredicate {
        node: NodeId::from_string("3"),
        name: "decision",
    };
    let msg = format!("{err}");
    assert!(msg.contains("decision"));
    assert!(msg.contains("missing predicate"));
}

#[test]
fn validation_error_missing_branch_display() {
    let err = ValidationError::MissingBranch {
        node: NodeId::from_string("2"),
        name: "choice",
        branch: "true",
    };
    let msg = format!("{err}");
    assert!(msg.contains("choice"));
    assert!(msg.contains("true branch"));
}

#[test]
fn validation_error_no_termination_condition_display() {
    let err = ValidationError::NoTerminationCondition {
        node: NodeId::from_string("1"),
        name: "infinite_loop",
    };
    let msg = format!("{err}");
    assert!(msg.contains("termination condition"));
    assert!(msg.contains("infinite_loop"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Error Trait Implementation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validation_error_implements_error_trait() {
    fn assert_error<E: std::error::Error>() {}
    assert_error::<ValidationError>();
}

// ─────────────────────────────────────────────────────────────────────────────
// Parallel Output Conflict Detection
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_parallel_conflicting_outputs_warns() {
    // Both branches produce the same output type (i32)
    let mut graph = Graph::new();
    graph.add_parallel(
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

    let result = graph.validate();
    assert!(result.is_ok(), "graph should be structurally valid");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w, ValidationWarning::ConflictingParallelOutputs { .. })),
        "expected ConflictingParallelOutputs warning, got: {:?}",
        result.warnings
    );
}

#[test]
fn validate_parallel_different_outputs_no_warning() {
    // Branches produce different output types (i32 vs String)
    async fn string_branch() -> String {
        "hello".to_string()
    }

    let mut graph = Graph::new();
    graph.add_parallel(
        "no_conflict",
        vec![
            |g: &mut Graph| {
                g.add_system(branch_a);
            },
            |g: &mut Graph| {
                g.add_system(string_branch);
            },
        ],
    );

    let result = graph.validate();
    assert!(result.is_ok(), "graph should be structurally valid");
    assert!(
        result.warnings.is_empty(),
        "expected no warnings, got: {:?}",
        result.warnings
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Loop Predicate Output Validation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validate_loop_predicate_output_not_produced() {
    #[derive(Debug)]
    struct LoopState {
        done: bool,
    }

    // Loop predicate reads LoopState, but body only produces i32
    let mut graph = Graph::new();
    graph.add_loop::<LoopState, _, _>(
        "bad_loop",
        |state| state.done,
        |g| {
            g.add_system(loop_body); // produces i32, not LoopState
        },
    );

    let result = graph.validate();
    assert!(result.is_err());
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::LoopPredicateOutputNotProduced { .. })),
        "expected LoopPredicateOutputNotProduced error, got: {:?}",
        result.errors
    );
}

#[test]
fn validate_loop_predicate_output_produced() {
    #[derive(Debug)]
    struct LoopState {
        done: bool,
    }

    async fn produce_loop_state() -> LoopState {
        LoopState { done: true }
    }

    // Loop predicate reads LoopState, body produces LoopState
    let mut graph = Graph::new();
    graph.add_loop::<LoopState, _, _>(
        "good_loop",
        |state| state.done,
        |g| {
            g.add_system(produce_loop_state);
        },
    );

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

// ─────────────────────────────────────────────────────────────────────────────
// Edge Requirement Validation
// ─────────────────────────────────────────────────────────────────────────────

/// A system that declares it requires an error edge (like one using `CaughtError`).
struct ErrorHandlerSystem;

impl System for ErrorHandlerSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "error_handler_system"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.require_context(ERROR_CONTEXT);
        access
    }
}

#[test]
fn validate_missing_error_edge_for_caught_error_system() {
    let mut graph = Graph::new();
    // Place the error-requiring system on a normal sequential path
    graph.add_boxed_system(Box::new(ErrorHandlerSystem));

    let result = graph.validate();
    assert!(result.is_err());
    assert!(
        result.errors.iter().any(|err| matches!(
            err,
            ValidationError::MissingEdgeRequirement {
                requirement: ERROR_CONTEXT,
                ..
            }
        )),
        "expected MissingEdgeRequirement error, got: {:?}",
        result.errors
    );
}

#[test]
fn validate_error_edge_satisfies_requirement() {
    let mut graph = Graph::new();

    // Add a normal system first, then attach an error handler that requires error edge
    let source_id = graph.add_system_node(first_step);
    graph.add_error_handler_for(source_id, |g| {
        g.add_boxed_system(Box::new(ErrorHandlerSystem));
    });

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

#[test]
fn context_requirements_affect_is_empty() {
    let access = SystemAccess::default();
    assert!(access.is_empty());

    let mut access = SystemAccess::default();
    access.require_context(ERROR_CONTEXT);
    assert!(!access.is_empty());
}

#[test]
fn context_requirements_merge() {
    let mut a = SystemAccess::default();
    a.require_context(ERROR_CONTEXT);

    let mut b = SystemAccess::default();
    b.require_context("timeout");

    a.merge(&b);
    assert!(a.context_requirements.contains(&ERROR_CONTEXT));
    assert!(a.context_requirements.contains(&"timeout"));
}

#[test]
fn context_requirements_no_duplicates() {
    let mut access = SystemAccess::default();
    access.require_context(ERROR_CONTEXT);
    access.require_context(ERROR_CONTEXT);
    assert_eq!(access.context_requirements.len(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// ErrOut<CaughtError> Validation
// ─────────────────────────────────────────────────────────────────────────────

/// System that delegates its access to `ErrOut<CaughtError>`.
struct ErrOutHandlerSystem;

impl System for ErrOutHandlerSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "err_out_handler_system"
    }

    fn access(&self) -> SystemAccess {
        <ErrOut<CaughtError>>::access()
    }
}

#[test]
fn err_out_param_declares_error_context_requirement() {
    let access = <ErrOut<CaughtError>>::access();
    assert!(
        access.context_requirements.contains(&ERROR_CONTEXT),
        "ErrOut<CaughtError> should declare error context requirement"
    );
}

#[test]
fn err_out_param_rejected_without_error_edge() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ErrOutHandlerSystem));

    let result = graph.validate();
    assert!(result.is_err());
    assert!(
        result.errors.iter().any(|err| matches!(
            err,
            ValidationError::MissingEdgeRequirement {
                requirement: ERROR_CONTEXT,
                ..
            }
        )),
        "expected MissingEdgeRequirement, got: {:?}",
        result.errors
    );
}

#[test]
fn err_out_param_accepted_behind_error_edge() {
    let mut graph = Graph::new();

    let source_id = graph.add_system_node(first_step);
    graph.add_error_handler_for(source_id, |g| {
        g.add_boxed_system(Box::new(ErrOutHandlerSystem));
    });

    let result = graph.validate();
    assert!(result.is_ok(), "Validation failed: {:?}", result.errors);
}

// ─────────────────────────────────────────────────────────────────────────────
// ValidationResult API
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn validation_result_warnings_preserved_with_errors() {
    // Build a graph that has both a warning (parallel output conflict)
    // and an error (error-handler system without error edge).
    let mut graph = Graph::new();
    graph.add_parallel(
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

    // This system requires an error edge but is on a sequential path → error
    graph.add_boxed_system(Box::new(ErrorHandlerSystem));

    let result = graph.validate();
    assert!(result.is_err(), "should have errors");
    assert!(
        result.has_warnings(),
        "warnings should be preserved even when errors exist"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scope Validation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scope_with_empty_inner_graph_reports_error() {
    let inner = Graph::new();
    let mut graph = Graph::new();
    graph.add_scope("empty_scope", inner, ContextPolicy::shared());

    let result = graph.validate();
    assert!(
        result
            .errors
            .iter()
            .any(|err| { matches!(err, ValidationError::EmptyScopeGraph { .. }) }),
        "should report EmptyScopeGraph error, got: {:?}",
        result.errors
    );
}

#[test]
fn scope_with_valid_inner_graph_passes_validation() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(SuccessSystem));

    let mut graph = Graph::new();
    graph.add_scope("valid_scope", inner, ContextPolicy::shared());

    let result = graph.validate();
    assert!(
        result.errors.is_empty(),
        "valid scope should pass validation, got: {:?}",
        result.errors
    );
}

#[test]
fn scope_isolated_forward_allows_write_access() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(WriteConfigSystem));

    let policy = ContextPolicy::isolated().forward::<TestConfig>();

    let mut graph = Graph::new();
    graph.add_scope("isolated_rw", inner, policy);

    let ctx = SystemContext::new().with(TestConfig { value: 1 });

    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(
        result.is_ok(),
        "write access to forwarded resource should pass validation, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn scope_isolated_forward_allows_read_access() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigSystem));

    let policy = ContextPolicy::isolated().forward::<TestConfig>();

    let mut graph = Graph::new();
    graph.add_scope("isolated_ro_read", inner, policy);

    let ctx = SystemContext::new().with(TestConfig { value: 1 });

    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(
        result.is_ok(),
        "read access to forwarded resource should pass validation, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn scope_inner_graph_invalid_propagates_errors() {
    // A scope graph with a structural error should produce ScopeGraphInvalid
    // wrapping the inner error. Here we use a loop whose predicate reads
    // LoopState but whose body only produces i32.
    #[derive(Debug)]
    struct LoopState {
        done: bool,
    }

    let mut inner = Graph::new();
    inner.add_loop::<LoopState, _, _>(
        "bad_loop",
        |state| state.done,
        |g| {
            g.add_system(loop_body); // produces i32, not LoopState
        },
    );

    let mut graph = Graph::new();
    graph.add_scope("invalid_inner_scope", inner, ContextPolicy::shared());

    let result = graph.validate();
    assert!(
        result
            .errors
            .iter()
            .any(|err| matches!(err, ValidationError::ScopeGraphInvalid { .. })),
        "should report ScopeGraphInvalid wrapping inner error, got: {:?}",
        result.errors
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scope Resource Validation — Negative Tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scope_isolated_without_forward_rejects_required_resource() {
    // An isolated scope whose inner system requires TestConfig (via read)
    // but the policy does not forward it — validation should fail.
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigSystem));

    let policy = ContextPolicy::isolated(); // no forward
    let mut graph = Graph::new();
    graph.add_scope("iso_no_fwd", inner, policy);

    let ctx = SystemContext::new().with(TestConfig { value: 1 });
    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(
        result.is_err(),
        "isolated scope without forwarding should fail resource validation"
    );
    let errors = result.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|err| { matches!(err, ResourceValidationError::MissingResource { .. }) }),
        "should report MissingResource, got: {errors:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scope Warning Propagation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn scope_propagates_inner_graph_warnings() {
    // A scope containing a parallel node with duplicate output types
    // should produce a ScopeGraphWarning wrapping ConflictingParallelOutputs.
    let mut inner = Graph::new();
    inner.add_parallel(
        "inner_conflict",
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
    graph.add_scope("warn_scope", inner, ContextPolicy::shared());

    let result = graph.validate();
    assert!(
        result.is_ok(),
        "scope with inner warnings should still pass validation, got errors: {:?}",
        result.errors
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w, ValidationWarning::ScopeGraphWarning { .. })),
        "expected ScopeGraphWarning wrapping inner ConflictingParallelOutputs, got: {:?}",
        result.warnings
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Output Reachability Validation
// ─────────────────────────────────────────────────────────────────────────────

/// System that produces `i32` output.
struct ProducesI32;

impl System for ProducesI32 {
    type Output = i32;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(42) })
    }

    fn name(&self) -> &'static str {
        "produces_i32"
    }
}

/// System that declares an `Out<i32>` dependency in its access.
struct ConsumesI32Output;

impl System for ConsumesI32Output {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "consumes_i32_output"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.outputs.push(Access {
            type_id: TypeId::of::<i32>(),
            type_name: std::any::type_name::<i32>(),
            mode: AccessMode::Write,
            is_global: false,
        });
        access
    }
}

/// System that declares an `Out<String>` dependency in its access.
struct ConsumesStringOutput;

impl System for ConsumesStringOutput {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "consumes_string_output"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.outputs.push(Access {
            type_id: TypeId::of::<String>(),
            type_name: std::any::type_name::<String>(),
            mode: AccessMode::Write,
            is_global: false,
        });
        access
    }
}

#[test]
fn validate_output_reachability_succeeds_for_linear_chain() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ProducesI32));
    graph.add_boxed_system(Box::new(ConsumesI32Output));

    let ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(
        result.is_ok(),
        "linear chain with matching output should pass validation, got: {:?}",
        result.unwrap_err()
    );
}

#[test]
fn validate_output_reachability_fails_for_missing_output() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ProducesI32));
    graph.add_boxed_system(Box::new(ConsumesStringOutput));

    let ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, None);

    assert!(result.is_err(), "should fail when output is not produced");
    let errors = result.unwrap_err();
    assert!(
        errors.iter().any(|err| matches!(
            err,
            ResourceValidationError::MissingOutput {
                system_name: "consumes_string_output",
                ..
            }
        )),
        "expected MissingOutput error for consumes_string_output, got: {errors:?}"
    );
}

/// A resource type used for testing hook-provided outputs.
#[derive(Clone)]
struct HookProvidedOutput;
impl LocalResource for HookProvidedOutput {}

/// System that declares an `Out<HookProvidedOutput>` dependency.
struct ConsumesHookOutput;

impl System for ConsumesHookOutput {
    type Output = ();

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok(()) })
    }

    fn name(&self) -> &'static str {
        "consumes_hook_output"
    }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::default();
        access.outputs.push(Access {
            type_id: TypeId::of::<HookProvidedOutput>(),
            type_name: std::any::type_name::<HookProvidedOutput>(),
            mode: AccessMode::Write,
            is_global: false,
        });
        access
    }
}

#[test]
fn validate_output_reachability_hook_provided_outputs_pass() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ConsumesHookOutput));

    let hooks = HooksAPI::new();
    hooks
        .register_provider::<OnGraphStart, HookProvidedOutput, _>("provide_hook_output", |_event| {
            Some(HookProvidedOutput)
        })
        .expect("hook registration should succeed");

    let ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.validate_resources(&graph, &ctx, Some(&hooks));

    assert!(
        result.is_ok(),
        "hook-provided output should pass validation, got: {:?}",
        result.unwrap_err()
    );
}
