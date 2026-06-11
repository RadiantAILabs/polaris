//! Per-resource [`ContextPolicy`] verb tests.
//!
//! Verifies that `share`, `forward`, `fork`, `forward_fresh`, `exclude`, and
//! `share_rest` produce the expected runtime behavior at scope boundaries.

mod test_utils;

use polaris_graph::executor::{ExecutionError, GraphExecutor};
use polaris_graph::graph::Graph;
use polaris_graph::middleware::MiddlewareAPI;
use polaris_graph::middleware::info::ScopeInfo;
use polaris_graph::node::{ContextMode, ContextPolicy};
use polaris_system::param::{ParamError, SystemContext};
use polaris_system::resource::{ForkStrategy, LocalResource};
use polaris_system::server::Server;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────────
// Test resources
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct CloneableState {
    value: i32,
}
impl LocalResource for CloneableState {}

#[derive(Default)]
struct FragmentStore {
    entries: Vec<String>,
}
impl LocalResource for FragmentStore {}
impl ForkStrategy for FragmentStore {
    fn fork(&self) -> Self {
        // Fresh-empty fork — common pattern.
        FragmentStore::default()
    }
}

#[derive(Clone)]
struct TokenBudget {
    remaining: Arc<AtomicU64>,
}
impl LocalResource for TokenBudget {}
impl ForkStrategy for TokenBudget {
    fn fork(&self) -> Self {
        // Shared-atomic fork — parent and child compete on the same pool.
        Self {
            remaining: Arc::clone(&self.remaining),
        }
    }
}

#[derive(Default)]
struct Counter {
    value: u32,
}
impl LocalResource for Counter {}

// ─────────────────────────────────────────────────────────────────────────────
// Probe systems — capture observed values from the child scope
// ─────────────────────────────────────────────────────────────────────────────

struct ReadCloneable {
    out: Arc<Mutex<Option<i32>>>,
}
impl System for ReadCloneable {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let out = Arc::clone(&self.out);
        Box::pin(async move {
            let r = ctx
                .get_resource::<CloneableState>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            *out.lock().unwrap() = Some(r.value);
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "read_cloneable"
    }
}

struct MutateCloneable {
    new_value: i32,
}
impl System for MutateCloneable {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let new_value = self.new_value;
        Box::pin(async move {
            let mut r = ctx
                .get_resource_mut::<CloneableState>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            r.value = new_value;
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "mutate_cloneable"
    }
}

struct PushFragment {
    text: &'static str,
}
impl System for PushFragment {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut s = ctx
                .get_resource_mut::<FragmentStore>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            s.entries.push(self.text.to_string());
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "push_fragment"
    }
}

struct ReadFragmentCount {
    out: Arc<Mutex<Option<usize>>>,
}
impl System for ReadFragmentCount {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let out = Arc::clone(&self.out);
        Box::pin(async move {
            let s = ctx
                .get_resource::<FragmentStore>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            *out.lock().unwrap() = Some(s.entries.len());
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "read_fragment_count"
    }
}

struct DeductBudget {
    amount: u64,
}
impl System for DeductBudget {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let amount = self.amount;
        Box::pin(async move {
            let b = ctx
                .get_resource::<TokenBudget>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            b.remaining.fetch_sub(amount, Ordering::SeqCst);
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "deduct_budget"
    }
}

struct ReadCounter {
    out: Arc<Mutex<Option<u32>>>,
}
impl System for ReadCounter {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let out = Arc::clone(&self.out);
        Box::pin(async move {
            let c = ctx
                .get_resource::<Counter>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            *out.lock().unwrap() = Some(c.value);
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "read_counter"
    }
}

struct ResourceMissing {
    saw_missing: Arc<Mutex<bool>>,
}
impl System for ResourceMissing {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let saw = Arc::clone(&self.saw_missing);
        Box::pin(async move {
            let result = ctx.get_resource::<CloneableState>();
            *saw.lock().unwrap() = result.is_err();
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "resource_missing"
    }
}

/// Captures the [`ParamError`] variant the child sees when it can't reach
/// `CloneableState`. Returns `Ok(())` regardless so the executor doesn't
/// short-circuit on the system failure.
struct CaptureMissingKind {
    captured: Arc<Mutex<Option<ParamError>>>,
}
impl System for CaptureMissingKind {
    type Output = ();
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let slot = Arc::clone(&self.captured);
        Box::pin(async move {
            if let Err(err) = ctx.get_resource::<CloneableState>() {
                *slot.lock().unwrap() = Some(err);
            }
            Ok(())
        })
    }
    fn name(&self) -> &'static str {
        "capture_missing_kind"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// share — child reads parent without copy
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn share_lets_child_read_parent_resource() {
    let captured = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCloneable {
        out: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "shared_one",
        inner,
        ContextPolicy::new().share::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 42 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(*captured.lock().unwrap(), Some(42));
}

#[tokio::test]
async fn share_does_not_allow_unrelated_resource() {
    // The child only shares Counter; CloneableState must be invisible and
    // the failure must be reported as `ResourceOutOfScope` (not a plain
    // not-found) since the parent does have it.
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("shared_one", inner, ContextPolicy::new().share::<Counter>());

    let mut ctx = SystemContext::new().with(CloneableState { value: 7 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    match err {
        ParamError::ResourceOutOfScope(name) => assert!(
            name.contains("CloneableState"),
            "error names the out-of-scope type, got {name}"
        ),
        other => panic!("expected ResourceOutOfScope, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// forward — clone into child; mutations don't leak back
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn forward_clones_resource_into_child() {
    // Read parent's value into the child first, then mutate the child's
    // copy. The starting-value assertion proves the clone made it across;
    // the parent-unchanged assertion proves the mutation did not leak back.
    let observed_before_mutation = Arc::new(Mutex::new(None));

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCloneable {
        out: Arc::clone(&observed_before_mutation),
    }));
    inner.add_boxed_system(Box::new(MutateCloneable { new_value: 99 }));

    let mut graph = Graph::new();
    graph.add_scope(
        "forwarded",
        inner,
        ContextPolicy::new().forward::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        *observed_before_mutation.lock().unwrap(),
        Some(1),
        "child must see the cloned starting value"
    );
    // Parent's value is unchanged: the clone is one-way.
    let parent = ctx.get_resource::<CloneableState>().unwrap();
    assert_eq!(parent.value, 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// fork — designer-defined semantics (fresh-empty / Arc-shared)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fork_fresh_empty_isolates_state() {
    let count = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(PushFragment { text: "child" }));
    inner.add_boxed_system(Box::new(ReadFragmentCount {
        out: Arc::clone(&count),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "fragment_fork",
        inner,
        ContextPolicy::new().fork::<FragmentStore>(),
    );

    let mut ctx = SystemContext::new().with(FragmentStore {
        entries: vec!["parent".into(), "older".into()],
    });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // Child's FragmentStore started empty (fork → default), then got "child".
    assert_eq!(*count.lock().unwrap(), Some(1));
    // Parent unchanged.
    let parent = ctx.get_resource::<FragmentStore>().unwrap();
    assert_eq!(parent.entries.len(), 2);
}

#[tokio::test]
async fn fork_arc_shared_lets_child_mutate_parent_atomic() {
    let parent_remaining = Arc::new(AtomicU64::new(1000));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(DeductBudget { amount: 250 }));

    let mut graph = Graph::new();
    graph.add_scope(
        "budget_fork",
        inner,
        ContextPolicy::new().fork::<TokenBudget>(),
    );

    let mut ctx = SystemContext::new().with(TokenBudget {
        remaining: Arc::clone(&parent_remaining),
    });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(parent_remaining.load(Ordering::SeqCst), 750);
}

// ─────────────────────────────────────────────────────────────────────────────
// forward_fresh — re-invokes registered factory
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn forward_fresh_creates_clean_instance_from_factory() {
    let count = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCounter {
        out: Arc::clone(&count),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "fresh_counter",
        inner,
        ContextPolicy::new().forward_fresh::<Counter>(),
    );

    // Register the factory and seed the parent counter.
    let mut server = Server::new();
    server.register_local(Counter::default);
    server.finish().await.unwrap();
    let mut ctx = server.create_context();
    ctx.get_resource_mut::<Counter>().unwrap().value = 99;

    // Parent has Counter { value: 99 }. Run the scope under that.
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // Child saw a fresh counter (default) — the value is 0, not 99.
    assert_eq!(*count.lock().unwrap(), Some(0));
    // Parent's counter is unchanged.
    assert_eq!(ctx.get_resource::<Counter>().unwrap().value, 99);
}

#[tokio::test]
async fn forward_fresh_errors_when_no_factory_registered() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCounter {
        out: Arc::new(Mutex::new(None)),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "fresh_no_factory",
        inner,
        ContextPolicy::new().forward_fresh::<Counter>(),
    );

    // No factory registered — only insert manually.
    let mut ctx = SystemContext::new().with(Counter::default());

    // `graph.validate()` is structural only (no context), so it passes here;
    // the factory check lives in `executor.validate_resources(ctx)`. This test
    // deliberately skips that resource-validation pass to exercise the runtime
    // safety net — execution must still surface `ScopeMissingFactory`.
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    let err = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap_err();

    match err {
        ExecutionError::ScopeMissingFactory { scope, resource } => {
            assert_eq!(scope, "fresh_no_factory");
            assert!(resource.contains("Counter"));
        }
        other => panic!("expected ScopeMissingFactory, got {other}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// share_rest + exclude — catch-all and override
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn share_rest_exposes_all_parent_resources() {
    let observed = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCloneable {
        out: Arc::clone(&observed),
    }));

    let mut graph = Graph::new();
    graph.add_scope("share_rest", inner, ContextPolicy::new().share_rest());

    let mut ctx = SystemContext::new().with(CloneableState { value: 11 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(*observed.lock().unwrap(), Some(11));
}

#[tokio::test]
async fn forward_after_share_rest_still_clones_into_child() {
    // A `forward::<T>()` declared *after* `share_rest()` overrides the
    // catch-all for that type: T is cloned into the child's local scope
    // (write-isolated) rather than chain-read by reference. Mutating the
    // child's copy must not leak back to the parent.
    let observed = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCloneable {
        out: Arc::clone(&observed),
    }));
    inner.add_boxed_system(Box::new(MutateCloneable { new_value: 99 }));

    let mut graph = Graph::new();
    graph.add_scope(
        "rest_then_forward",
        inner,
        ContextPolicy::new()
            .share_rest()
            .forward::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 7 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        Some(7),
        "child must see the cloned starting value"
    );
    assert_eq!(
        ctx.get_resource::<CloneableState>().unwrap().value,
        7,
        "forward clone must be write-isolated even under share_rest"
    );
}

#[tokio::test]
async fn exclude_blocks_resource_under_share_rest() {
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "share_rest_minus_one",
        inner,
        ContextPolicy::new()
            .share_rest()
            .exclude::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 11 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    match err {
        ParamError::ResourceOutOfScope(name) => assert!(
            name.contains("CloneableState"),
            "exclude under share_rest must surface ResourceOutOfScope with the type name, got {name}"
        ),
        other => panic!("expected ResourceOutOfScope, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure isolation — new() with no verbs blocks parent locals
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn new_with_no_verbs_blocks_parent_locals() {
    let saw_missing = Arc::new(Mutex::new(false));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ResourceMissing {
        saw_missing: Arc::clone(&saw_missing),
    }));

    let mut graph = Graph::new();
    graph.add_scope("isolated", inner, ContextPolicy::new());

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert!(
        *saw_missing.lock().unwrap(),
        "ContextPolicy::new() must not let the child see the parent's local"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Diagnostic — distinguish "blocked by ContextPolicy" from plain not-found
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn isolated_scope_reports_out_of_scope_when_parent_has_resource() {
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("isolated", inner, ContextPolicy::new());

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    match err {
        ParamError::ResourceOutOfScope(name) => {
            assert!(
                name.contains("CloneableState"),
                "error names the out-of-scope type, got {name}"
            );
        }
        other => panic!("expected ResourceOutOfScope, got {other:?}"),
    }
}

#[tokio::test]
async fn share_rest_with_exclude_reports_out_of_scope() {
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "share_rest_minus_one",
        inner,
        ContextPolicy::new()
            .share_rest()
            .exclude::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    assert!(
        matches!(err, ParamError::ResourceOutOfScope(_)),
        "exclude under share_rest should report ResourceOutOfScope, got {err:?}"
    );
}

#[tokio::test]
async fn isolated_scope_reports_plain_not_found_when_parent_lacks_resource() {
    // Parent doesn't have CloneableState anywhere — true not-found, not blocked.
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("isolated", inner, ContextPolicy::new());

    let mut ctx = SystemContext::new();
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    assert!(
        matches!(err, ParamError::ResourceNotFound(_)),
        "no parent resource → plain ResourceNotFound, got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Write isolation — forwarded mutations stay in the child
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn forward_writes_do_not_propagate_to_parent() {
    // Property: a child scope that forwards a resource owns its own copy;
    // mutations there must not leak into the parent's copy. (Companion to
    // `forward_clones_resource_into_child` — same shape, framed around
    // write isolation rather than the clone direction.)
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(MutateCloneable { new_value: 99 }));

    let mut graph = Graph::new();
    graph.add_scope(
        "writes_isolated",
        inner,
        ContextPolicy::new().forward::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        ctx.get_resource::<CloneableState>().unwrap().value,
        1,
        "mutation in child must not escape to parent"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Nested scopes — forward_fresh walks ancestor chain to find the factory
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn forward_fresh_walks_ancestor_chain_for_factory() {
    // Two-level scope: outer and inner both forward_fresh::<Counter>().
    // Counter's factory is registered at the root only — the inner scope
    // must reach two levels up to find it.
    let inner_observed = Arc::new(Mutex::new(None));

    let mut innermost = Graph::new();
    innermost.add_boxed_system(Box::new(ReadCounter {
        out: Arc::clone(&inner_observed),
    }));

    let mut middle = Graph::new();
    middle.add_scope(
        "inner_scope",
        innermost,
        ContextPolicy::new().forward_fresh::<Counter>(),
    );

    let mut graph = Graph::new();
    graph.add_scope(
        "outer_scope",
        middle,
        ContextPolicy::new().forward_fresh::<Counter>(),
    );

    let mut server = Server::new();
    server.register_local(Counter::default);
    server.finish().await.unwrap();
    let mut ctx = server.create_context();
    ctx.get_resource_mut::<Counter>().unwrap().value = 42;

    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        *inner_observed.lock().unwrap(),
        Some(0),
        "inner scope must see a fresh Counter, not the root's value"
    );
    assert_eq!(
        ctx.get_resource::<Counter>().unwrap().value,
        42,
        "root Counter must be unchanged"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// share + ResMut misuse — Share doesn't insert into the child's locals,
// so a child system requesting ResMut<T> hits ResourceNotFound at runtime.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn share_with_resmut_in_child_fails_at_runtime() {
    let captured: Arc<Mutex<Option<ParamError>>> = Arc::new(Mutex::new(None));

    /// Probe that requests `ResMut<CloneableState>` and captures the failure
    /// kind. Returns `Ok(())` so the executor doesn't short-circuit.
    struct CaptureMutMissingKind {
        captured: Arc<Mutex<Option<ParamError>>>,
    }
    impl System for CaptureMutMissingKind {
        type Output = ();
        fn run<'a>(
            &'a self,
            ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            let slot = Arc::clone(&self.captured);
            Box::pin(async move {
                if let Err(err) = ctx.get_resource_mut::<CloneableState>() {
                    *slot.lock().unwrap() = Some(err);
                }
                Ok(())
            })
        }
        fn name(&self) -> &'static str {
            "capture_mut_missing_kind"
        }
    }

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(CaptureMutMissingKind {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope(
        "share_then_write",
        inner,
        ContextPolicy::new().share::<CloneableState>(),
    );

    let mut ctx = SystemContext::new().with(CloneableState { value: 7 });
    assert!(graph.validate().is_ok());
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let err = captured.lock().unwrap().take().expect("expected an error");
    assert!(
        matches!(err, ParamError::ResourceNotFound(_)),
        "share() does not insert a child-local; ResMut must report \
         ResourceNotFound, got {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Middleware sees the right ContextMode in ScopeInfo
// ─────────────────────────────────────────────────────────────────────────────

async fn observe_scope_mode_via_middleware(policy: ContextPolicy, expected: ContextMode) {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadCloneable {
        out: Arc::new(Mutex::new(None)),
    }));

    let mut graph = Graph::new();
    graph.add_scope("middleware_scope", inner, policy);

    let observed: Arc<Mutex<Option<ContextMode>>> = Arc::new(Mutex::new(None));
    let observed_clone = Arc::clone(&observed);

    let middleware = MiddlewareAPI::new();
    middleware.register_scope("capture_mode", move |info: ScopeInfo, ctx, next| {
        let observed = Arc::clone(&observed_clone);
        Box::pin(async move {
            *observed.lock().unwrap() = Some(info.mode);
            next.run(ctx).await
        })
    });

    let mut ctx = SystemContext::new().with(CloneableState { value: 1 });
    let executor = GraphExecutor::new();
    executor
        .execute(&graph, &mut ctx, None, Some(&middleware))
        .await
        .unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        Some(expected),
        "ScopeInfo.mode mismatch"
    );
}

#[tokio::test]
async fn middleware_sees_shared_mode() {
    observe_scope_mode_via_middleware(ContextPolicy::shared(), ContextMode::Shared).await;
}

#[tokio::test]
async fn middleware_sees_inherit_mode() {
    observe_scope_mode_via_middleware(ContextPolicy::new().share_rest(), ContextMode::Inherit)
        .await;
}

#[tokio::test]
async fn middleware_sees_isolated_mode() {
    observe_scope_mode_via_middleware(
        ContextPolicy::new().share::<CloneableState>(),
        ContextMode::Isolated,
    )
    .await;
}
