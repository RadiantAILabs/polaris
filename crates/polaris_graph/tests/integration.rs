//! Integration tests for the full Server → Graph → Executor flow.
//!
//! These tests verify that all layers work together correctly:
//! - Layer 1: `polaris_system` (`Server`, `Resources`, `SystemContext`)
//! - Layer 2: `polaris_graph` (`Graph`, `Nodes`, `Edges`, `Executor`)
//!
//! Tests validate the core philosophy:
//! - Systems are pure functions with dependency injection
//! - `GlobalResource` is read-only, shared across all contexts
//! - `LocalResource` is mutable, isolated per context
//! - Graphs define execution flow
//! - Outputs chain between systems
//! - Scope execution with Shared, Inherit, and Isolated modes

mod test_utils;

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_graph::middleware::MiddlewareAPI;
use polaris_graph::node::ContextPolicy;
use polaris_system::param::{Res, ResMut, SystemAccess, SystemContext, SystemParam};
use polaris_system::resource::{GlobalResource, LocalResource, Resources};
use polaris_system::server::Server;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::{Arc, Mutex};
use test_utils::{
    ConsumerSystem, FlagSystem, ProducerSystem, ReadConfigCapture, SuccessSystem, TestConfig,
    WriteConfigCapture,
};

// ─────────────────────────────────────────────────────────────────────────────
// Test Resources
// ─────────────────────────────────────────────────────────────────────────────

/// Global configuration - read-only, shared across all agents.
#[derive(Debug)]
struct AppConfig {
    multiplier: i32,
}
impl GlobalResource for AppConfig {}

/// Local memory - mutable, isolated per agent execution.
#[derive(Debug)]
struct AgentMemory {
    history: Vec<i32>,
}
impl LocalResource for AgentMemory {}

impl AgentMemory {
    fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test Output Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ComputeResult {
    value: i32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Test Systems
// ─────────────────────────────────────────────────────────────────────────────

/// System that reads global config and mutates local memory.
struct ComputeSystem {
    input: i32,
}

impl System for ComputeSystem {
    type Output = ComputeResult;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            // Read global config (Res<T>)
            let config = Res::<AppConfig>::fetch(ctx)?;

            // Mutate local memory (ResMut<T>)
            let mut memory = ResMut::<AgentMemory>::fetch(ctx)?;

            let result = self.input * config.multiplier;
            memory.history.push(result);

            Ok(ComputeResult { value: result })
        })
    }

    fn name(&self) -> &'static str {
        "compute_system"
    }

    fn access(&self) -> SystemAccess {
        // Declare resource requirements for validation:
        // - Res<AppConfig> = read access to global config
        // - ResMut<AgentMemory> = write access to local memory
        let mut access = SystemAccess::new();
        access.merge(&<Res<AppConfig> as SystemParam>::access());
        access.merge(&<ResMut<AgentMemory> as SystemParam>::access());
        access
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn full_server_graph_executor_flow() {
    // 1. Setup Server with global and local resources
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 10 });
    server.register_local(AgentMemory::new);

    // 2. Build a graph with sequential systems
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 5 }));
    graph.add_boxed_system(Box::new(ComputeSystem { input: 3 }));

    // 3. Create execution context from server
    let mut ctx = server.create_context();

    // 4. Execute graph with context
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        result.is_ok(),
        "Integration test failed: {:?}",
        result.err()
    );

    // 5. Verify results
    // - Global config was read correctly (5 * 10 = 50, 3 * 10 = 30)
    // - Local memory was mutated with both results
    let memory = ctx.get_resource::<AgentMemory>().unwrap();
    assert_eq!(memory.history, vec![50, 30]);

    // - Execution stats and output are correct
    let stats = result.unwrap();
    assert_eq!(stats.nodes_executed, 2);

    // - Last output is available on the result
    let output = stats.output::<ComputeResult>().unwrap();
    assert_eq!(output.value, 30);
}

#[tokio::test]
async fn multiple_agents_have_isolated_memory() {
    // Setup server with shared global config
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 2 });
    server.register_local(AgentMemory::new);

    // Build graph
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 7 }));

    let executor = GraphExecutor::new();

    // Execute with first agent context
    let mut ctx1 = server.create_context();
    let _ = executor
        .execute(&graph, &mut ctx1, None, None)
        .await
        .unwrap();

    // Execute with second agent context
    let mut ctx2 = server.create_context();
    let _ = executor
        .execute(&graph, &mut ctx2, None, None)
        .await
        .unwrap();

    // Execute first agent again
    let _ = executor
        .execute(&graph, &mut ctx1, None, None)
        .await
        .unwrap();

    // Agent 1 has two entries (ran twice)
    let memory1 = ctx1.get_resource::<AgentMemory>().unwrap();
    assert_eq!(memory1.history, vec![14, 14]);

    // Agent 2 has one entry (ran once) - completely isolated
    let memory2 = ctx2.get_resource::<AgentMemory>().unwrap();
    assert_eq!(memory2.history, vec![14]);
}

#[tokio::test]
async fn child_context_inherits_globals_with_own_locals() {
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 5 });
    server.register_local(AgentMemory::new);

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 4 }));

    let executor = GraphExecutor::new();

    // Create parent context
    let parent_ctx = server.create_context();

    // Create child context with its own local resources
    let mut child_ctx = parent_ctx.child().with(AgentMemory::new());

    // Execute on child
    let result = executor.execute(&graph, &mut child_ctx, None, None).await;
    assert!(result.is_ok(), "Child execution failed: {:?}", result.err());

    // Child's memory should have the result
    let child_memory = child_ctx.get_resource::<AgentMemory>().unwrap();
    assert_eq!(child_memory.history, vec![20]); // 4 * 5 = 20

    // Parent's memory should be untouched
    let parent_memory = parent_ctx.get_resource::<AgentMemory>().unwrap();
    assert!(parent_memory.history.is_empty());
}

#[tokio::test]
async fn global_resource_shared_across_contexts() {
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 7 });
    server.register_local(AgentMemory::new);

    // Multiple contexts all see the same global config
    let ctx1 = server.create_context();
    let ctx2 = server.create_context();
    let child = ctx1.child();

    // All contexts should see multiplier = 7
    let config1 = ctx1.get_resource::<AppConfig>().unwrap();
    let config2 = ctx2.get_resource::<AppConfig>().unwrap();
    let config_child = child.get_resource::<AppConfig>().unwrap();

    assert_eq!(config1.multiplier, 7);
    assert_eq!(config2.multiplier, 7);
    assert_eq!(config_child.multiplier, 7);
}

// ─────────────────────────────────────────────────────────────────────────────
// Conditional Branch Integration Tests
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Decision {
    take_branch_a: bool,
}

#[derive(Debug, Clone)]
struct BranchResult {
    branch_name: &'static str,
}

#[tokio::test]
async fn conditional_branch_with_resources() {
    async fn make_decision() -> Decision {
        Decision {
            take_branch_a: true,
        }
    }

    async fn branch_a() -> BranchResult {
        BranchResult {
            branch_name: "branch_a",
        }
    }

    async fn branch_b() -> BranchResult {
        BranchResult {
            branch_name: "branch_b",
        }
    }

    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 1 });

    let mut graph = Graph::new();
    graph.add_system(make_decision);
    graph.add_conditional_branch::<Decision, _, _, _>(
        "decision",
        |d| d.take_branch_a,
        |g| {
            g.add_system(branch_a);
        },
        |g| {
            g.add_system(branch_b);
        },
    );

    let mut ctx = server.create_context();
    let executor = GraphExecutor::new();

    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let output = result.output::<BranchResult>().unwrap();
    assert_eq!(output.branch_name, "branch_a");
}

// ─────────────────────────────────────────────────────────────────────────────
// Loop Integration Tests
// ─────────────────────────────────────────────────────────────────────────────
//
// NOTE: The termination predicate is checked BEFORE each iteration and reads
// from `Out<T>`. A system must be added before the loop that produces the
// initial output value.

#[derive(Debug, Clone)]
struct LoopCounter {
    count: i32,
    done: bool,
}
impl LocalResource for LoopCounter {}

struct IncrementAndCheck;

impl System for IncrementAndCheck {
    type Output = LoopCounter;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut counter = ResMut::<LoopCounter>::fetch(ctx)?;
            counter.count += 1;
            if counter.count >= 5 {
                counter.done = true;
            }
            Ok(LoopCounter {
                count: counter.count,
                done: counter.done,
            })
        })
    }

    fn name(&self) -> &'static str {
        "increment_and_check"
    }
}

/// System to prime loop output before entering the loop.
async fn init_loop_counter() -> LoopCounter {
    LoopCounter {
        count: 0,
        done: false,
    }
}

#[tokio::test]
async fn loop_with_local_resource_state() {
    let mut server = Server::new();
    server.register_local(|| LoopCounter {
        count: 0,
        done: false,
    });

    let mut graph = Graph::new();
    // Prime output before loop (requirement 1)
    graph.add_system(init_loop_counter);
    graph.add_loop::<LoopCounter, _, _>(
        "counting_loop",
        |state| state.done,
        |g| {
            g.add_boxed_system(Box::new(IncrementAndCheck));
        },
    );

    let mut ctx = server.create_context();
    let executor = GraphExecutor::new();

    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok(), "Expected Ok, got {:?}", result);

    let counter = ctx.get_resource::<LoopCounter>().unwrap();
    assert_eq!(counter.count, 5);
    assert!(counter.done);
}

// ─────────────────────────────────────────────────────────────────────────────
// Eager Resource Validation Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn validate_resources_passes_when_all_resources_present() {
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 10 });
    server.register_local(AgentMemory::new);

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 5 }));

    let ctx = server.create_context();
    let executor = GraphExecutor::new();

    // Validation should pass when all resources are available
    let result = executor.validate_resources(&graph, &ctx, None);
    assert!(result.is_ok(), "Validation failed: {:?}", result.err());
}

#[tokio::test]
async fn validate_resources_detects_missing_global_resource() {
    use polaris_graph::executor::ResourceValidationError;
    use polaris_system::param::AccessMode;

    // Server WITHOUT AppConfig (but with AgentMemory registered)
    let mut server = Server::new();
    server.register_local(AgentMemory::new);

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 5 }));

    let ctx = server.create_context();
    let executor = GraphExecutor::new();

    // Validation should fail - AppConfig is missing
    let result = executor.validate_resources(&graph, &ctx, None);
    assert!(result.is_err(), "Expected validation to fail");

    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1); // Only AppConfig is missing (AgentMemory was registered)

    // Check that it's specifically a read access error for AppConfig
    if let ResourceValidationError::MissingResource {
        resource_type,
        access_mode,
        ..
    } = &errors[0]
    {
        assert!(resource_type.contains("AppConfig"));
        assert_eq!(*access_mode, AccessMode::Read);
    } else {
        panic!("Expected MissingResource error");
    }
}

#[tokio::test]
async fn validate_resources_detects_missing_local_resource() {
    use polaris_graph::executor::ResourceValidationError;
    use polaris_system::param::AccessMode;

    // Server with AppConfig but WITHOUT AgentMemory
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 10 });
    // Note: NOT registering AgentMemory

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 5 }));

    let ctx = server.create_context();
    let executor = GraphExecutor::new();

    // Validation should fail - AgentMemory is missing
    let result = executor.validate_resources(&graph, &ctx, None);
    assert!(result.is_err(), "Expected validation to fail");

    let errors = result.unwrap_err();
    assert_eq!(errors.len(), 1); // Only AgentMemory is missing

    // Check that it's specifically a write access error (ResMut)
    if let ResourceValidationError::MissingResource {
        resource_type,
        access_mode,
        ..
    } = &errors[0]
    {
        assert!(resource_type.contains("AgentMemory"));
        assert_eq!(*access_mode, AccessMode::Write);
    } else {
        panic!("Expected MissingResource error");
    }
}

#[tokio::test]
async fn validate_resources_checks_hierarchy() {
    // Test that Res<T> validation walks up the parent chain
    let mut server = Server::new();
    server.insert_global(AppConfig { multiplier: 10 });
    server.register_local(AgentMemory::new);

    let parent_ctx = server.create_context();

    // Create a child context - it should still be able to read AppConfig
    // through the parent/globals chain
    let child_ctx = parent_ctx.child().with(AgentMemory::new());

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(ComputeSystem { input: 5 }));

    let executor = GraphExecutor::new();

    // Validation should pass because child can read AppConfig through hierarchy
    let result = executor.validate_resources(&graph, &child_ctx, None);
    assert!(result.is_ok(), "Validation failed: {:?}", result.err());
}

// ─────────────────────────────────────────────────────────────────────────────
// Diverging and Converging Paths Integration Tests
// ─────────────────────────────────────────────────────────────────────────────
//
// These tests verify execution of diamond-pattern graphs:
// A -> [B, C] -> D (diverge then converge)

/// Result type for diamond pattern tests.
#[derive(Debug, Clone)]
struct DiamondResult {
    step: &'static str,
    value: i32,
}

/// Tests parallel diverge/converge execution:
/// `entry` -> [`branch_a`, `branch_b`] (concurrent) -> `after_join`
///
/// Verifies that:
/// 1. Both branches execute (total node count)
/// 2. The join waits for all branches
/// 3. Execution continues after the join
#[tokio::test]
async fn parallel_diamond_execution() {
    async fn entry_step() -> DiamondResult {
        DiamondResult {
            step: "entry",
            value: 1,
        }
    }

    async fn branch_a_step() -> DiamondResult {
        DiamondResult {
            step: "branch_a",
            value: 10,
        }
    }

    async fn branch_b_step() -> DiamondResult {
        DiamondResult {
            step: "branch_b",
            value: 20,
        }
    }

    async fn after_join_step() -> DiamondResult {
        DiamondResult {
            step: "after_join",
            value: 100,
        }
    }

    let server = Server::new();

    let mut graph = Graph::new();
    graph
        .add_system(entry_step)
        .add_parallel(
            "diamond_fork",
            vec![
                |g: &mut Graph| {
                    g.add_system(branch_a_step);
                },
                |g: &mut Graph| {
                    g.add_system(branch_b_step);
                },
            ],
        )
        .add_system(after_join_step);

    let mut ctx = server.create_context();
    let executor = GraphExecutor::new();

    let result = executor.execute(&graph, &mut ctx, None, None).await;
    assert!(result.is_ok(), "Execution failed: {:?}", result.err());

    // Verify execution stats
    // Nodes: entry (1) + parallel (1) + branch_a (1) + branch_b (1) + after_join (1) = 5
    let stats = result.unwrap();
    assert_eq!(stats.nodes_executed, 5);

    // Final output should be from the after_join step
    let output = stats.output::<DiamondResult>().unwrap();
    assert_eq!(output.step, "after_join");
    assert_eq!(output.value, 100);
}

/// Tests that outputs produced in parallel branches are visible after the join.
#[tokio::test]
async fn parallel_outputs_visible_after_join() {
    #[derive(Debug, Clone)]
    struct BranchAOutput {
        value: i32,
    }

    #[derive(Debug, Clone)]
    struct BranchBOutput {
        label: &'static str,
    }

    async fn branch_a_sys() -> BranchAOutput {
        BranchAOutput { value: 42 }
    }

    async fn branch_b_sys() -> BranchBOutput {
        BranchBOutput { label: "hello" }
    }

    /// System after join that reads outputs from both branches.
    struct ReadBranchOutputs;

    impl System for ReadBranchOutputs {
        type Output = DiamondResult;

        fn run<'a>(
            &'a self,
            ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async move {
                use polaris_system::param::{Out, SystemParam};

                let a = Out::<BranchAOutput>::fetch(ctx)?;
                let b = Out::<BranchBOutput>::fetch(ctx)?;

                Ok(DiamondResult {
                    step: "after_join",
                    value: a.value + b.label.len() as i32,
                })
            })
        }

        fn name(&self) -> &'static str {
            "read_branch_outputs"
        }
    }

    let server = Server::new();

    let mut graph = Graph::new();
    graph
        .add_parallel(
            "fork",
            vec![
                |g: &mut Graph| {
                    g.add_system(branch_a_sys);
                },
                |g: &mut Graph| {
                    g.add_system(branch_b_sys);
                },
            ],
        )
        .add_boxed_system(Box::new(ReadBranchOutputs));

    let mut ctx = server.create_context();
    let executor = GraphExecutor::new();

    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let output = result.output::<DiamondResult>().unwrap();
    assert_eq!(output.step, "after_join");
    assert_eq!(output.value, 42 + 5); // 42 + len("hello")
}

/// Tests conditional diverge/converge execution:
/// `decision` -> (true) -> `true_step` -> converge
#[tokio::test]
async fn conditional_diverge_converge_diamond() {
    #[derive(Debug, Clone)]
    struct RouteDecision {
        take_true_path: bool,
    }

    async fn make_decision() -> RouteDecision {
        RouteDecision {
            take_true_path: true,
        }
    }

    async fn true_branch() -> DiamondResult {
        DiamondResult {
            step: "true_branch",
            value: 10,
        }
    }

    async fn false_branch() -> DiamondResult {
        DiamondResult {
            step: "false_branch",
            value: 20,
        }
    }

    async fn converge_step() -> DiamondResult {
        DiamondResult {
            step: "converge",
            value: 100,
        }
    }

    let server = Server::new();

    let mut graph = Graph::new();
    graph
        .add_system(make_decision)
        .add_conditional_branch::<RouteDecision, _, _, _>(
            "route",
            |d| d.take_true_path,
            |g| {
                g.add_system(true_branch);
            },
            |g| {
                g.add_system(false_branch);
            },
        )
        .add_system(converge_step);

    let mut ctx = server.create_context();
    let executor = GraphExecutor::new();

    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // Final output should be from the converge step (after the branch)
    let output = result.output::<DiamondResult>().unwrap();
    assert_eq!(output.step, "converge");
    assert_eq!(output.value, 100);
}

// ═══════════════════════════════════════════════════════════════════════════════
// SCOPE EXECUTION TESTS
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scope_shared_executes_inner_graph() {
    let mut inner = Graph::new();
    let flag = Arc::new(Mutex::new(false));
    inner.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));

    let mut graph = Graph::new();
    graph.add_scope("shared_scope", inner, ContextPolicy::shared());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok(), "scope execution should succeed");
    assert!(*flag.lock().unwrap(), "inner system should have executed");
}

#[tokio::test]
async fn scope_shared_outputs_visible_after_scope() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ProducerSystem { value: 42 }));

    let received = Arc::new(Mutex::new(None));
    let mut graph = Graph::new();
    graph
        .add_scope("producer_scope", inner, ContextPolicy::shared())
        .add_boxed_system(Box::new(ConsumerSystem {
            received: Arc::clone(&received),
        }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *received.lock().unwrap(),
        Some(42),
        "output from scope should be visible to subsequent systems"
    );
}

#[tokio::test]
async fn scope_shared_reads_parent_resources() {
    let captured = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigCapture {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("config_scope", inner, ContextPolicy::shared());

    let mut ctx = SystemContext::new().with(TestConfig { value: 99 });
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *captured.lock().unwrap(),
        Some(99),
        "shared scope should read parent resources"
    );
}

#[tokio::test]
async fn scope_inherit_executes_inner_graph() {
    let mut inner = Graph::new();
    let flag = Arc::new(Mutex::new(false));
    inner.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));

    let mut graph = Graph::new();
    graph.add_scope("inherit_scope", inner, ContextPolicy::inherit());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert!(*flag.lock().unwrap(), "inner system should have executed");
}

#[tokio::test]
async fn scope_inherit_merges_outputs_back() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ProducerSystem { value: 77 }));

    let received = Arc::new(Mutex::new(None));
    let mut graph = Graph::new();
    graph
        .add_scope("inherit_scope", inner, ContextPolicy::inherit())
        .add_boxed_system(Box::new(ConsumerSystem {
            received: Arc::clone(&received),
        }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *received.lock().unwrap(),
        Some(77),
        "outputs from inherit scope should be merged back to parent"
    );
}

#[tokio::test]
async fn scope_inherit_reads_parent_resources_via_chain() {
    let captured = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigCapture {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("inherit_scope", inner, ContextPolicy::inherit());

    let mut ctx = SystemContext::new().with(TestConfig { value: 55 });
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *captured.lock().unwrap(),
        Some(55),
        "inherit scope should read parent resources via chain"
    );
}

#[tokio::test]
async fn scope_inherit_forward_clones_resource() {
    let captured = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigCapture {
        captured: Arc::clone(&captured),
    }));

    let policy = ContextPolicy::inherit().forward::<TestConfig>();

    let mut graph = Graph::new();
    graph.add_scope("inherit_fwd", inner, policy);

    let mut ctx = SystemContext::new().with(TestConfig { value: 33 });

    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *captured.lock().unwrap(),
        Some(33),
        "forwarded resource should be readable in child"
    );
}

#[tokio::test]
async fn scope_isolated_executes_inner_graph() {
    let mut inner = Graph::new();
    let flag = Arc::new(Mutex::new(false));
    inner.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag),
    }));

    let mut graph = Graph::new();
    graph.add_scope("isolated_scope", inner, ContextPolicy::isolated());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert!(*flag.lock().unwrap(), "inner system should have executed");
}

#[tokio::test]
async fn scope_isolated_merges_outputs_back() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ProducerSystem { value: 101 }));

    let received = Arc::new(Mutex::new(None));
    let mut graph = Graph::new();
    graph
        .add_scope("isolated_scope", inner, ContextPolicy::isolated())
        .add_boxed_system(Box::new(ConsumerSystem {
            received: Arc::clone(&received),
        }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *received.lock().unwrap(),
        Some(101),
        "outputs from isolated scope should be merged back"
    );
}

#[tokio::test]
async fn scope_isolated_forward_clones_resource() {
    let captured = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadConfigCapture {
        captured: Arc::clone(&captured),
    }));

    let policy = ContextPolicy::isolated().forward::<TestConfig>();

    let mut graph = Graph::new();
    graph.add_scope("isolated_fwd_mut", inner, policy);

    let mut ctx = SystemContext::new().with(TestConfig { value: 77 });

    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *captured.lock().unwrap(),
        Some(77),
        "forward should make resource accessible in isolated scope"
    );
}

#[tokio::test]
async fn scope_in_sequential_chain() {
    let flag_before = Arc::new(Mutex::new(false));

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ProducerSystem { value: 10 }));

    let received = Arc::new(Mutex::new(None));
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(FlagSystem {
        flag: Arc::clone(&flag_before),
    }));
    graph
        .add_scope("middle_scope", inner, ContextPolicy::shared())
        .add_boxed_system(Box::new(ConsumerSystem {
            received: Arc::clone(&received),
        }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert!(*flag_before.lock().unwrap(), "system before scope ran");
    assert_eq!(
        *received.lock().unwrap(),
        Some(10),
        "system after scope consumed scope output"
    );
}

#[tokio::test]
async fn scope_node_count_in_result() {
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(SuccessSystem));

    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SuccessSystem));
    graph.add_scope("scope", inner, ContextPolicy::shared());
    graph.add_boxed_system(Box::new(SuccessSystem));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // 3 nodes in parent + 1 inner node executed via scope
    assert_eq!(
        result.nodes_executed,
        3 + 1,
        "should count parent nodes + inner scope nodes"
    );
}

#[tokio::test]
async fn scope_nested_inherits_outputs() {
    // Outer scope (Shared) → Inner scope (Shared) → ProducerSystem
    // Consumer after both scopes should see the output.
    let mut innermost = Graph::new();
    innermost.add_boxed_system(Box::new(ProducerSystem { value: 99 }));

    let mut outer_inner = Graph::new();
    outer_inner.add_scope("inner_scope", innermost, ContextPolicy::shared());

    let received = Arc::new(Mutex::new(None));
    let mut graph = Graph::new();
    graph
        .add_scope("outer_scope", outer_inner, ContextPolicy::shared())
        .add_boxed_system(Box::new(ConsumerSystem {
            received: Arc::clone(&received),
        }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    assert_eq!(
        *received.lock().unwrap(),
        Some(99),
        "output from nested scope should propagate to parent"
    );
}

#[tokio::test]
async fn scope_inside_parallel_branch() {
    // Parallel with two branches: one containing a scope with a producer,
    // one containing a plain producer with a different value.
    let mut scope_inner = Graph::new();
    scope_inner.add_boxed_system(Box::new(ProducerSystem { value: 50 }));

    let mut graph = Graph::new();
    graph.add_parallel(
        "fork",
        vec![
            |g: &mut Graph| {
                let mut inner = Graph::new();
                inner.add_boxed_system(Box::new(ProducerSystem { value: 50 }));
                g.add_scope("scoped_branch", inner, ContextPolicy::shared());
            },
            |g: &mut Graph| {
                g.add_boxed_system(Box::new(SuccessSystem));
            },
        ],
    );

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    let result = result.unwrap();
    // parallel(1) + branch_a scope(1) + inner system(1) + branch_b system(1) = 4
    assert_eq!(
        result.nodes_executed, 4,
        "should execute parallel(1) + scope(1) + inner(1) + branch_b(1)"
    );
}

#[tokio::test]
async fn scope_inherit_write_isolation() {
    // In Inherit mode, writes inside the scope go to the child's local scope.
    // The parent's resource should remain unchanged after the scope completes.
    let captured_before = Arc::new(Mutex::new(None));
    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(WriteConfigCapture {
        new_value: 999,
        captured: Arc::clone(&captured_before),
    }));

    let policy = ContextPolicy::inherit().forward::<TestConfig>();

    let mut graph = Graph::new();
    graph.add_scope("inherit_write", inner, policy);

    let mut ctx = SystemContext::new().with(TestConfig { value: 42 });
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_ok());
    // The scope system saw the original value via the forwarded clone
    assert_eq!(
        *captured_before.lock().unwrap(),
        Some(42),
        "scope system should see original forwarded value"
    );
    // The parent's resource should be unchanged — writes went to the child
    let parent_config = ctx.get_resource::<TestConfig>().unwrap();
    assert_eq!(
        parent_config.value, 42,
        "parent resource should be unchanged after inherit scope writes"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Scope Middleware
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scope_middleware_handler_executes() {
    let invoked = Arc::new(Mutex::new(false));
    let invoked_clone = Arc::clone(&invoked);

    let mw = MiddlewareAPI::new();
    mw.register_scope("test_scope_mw", move |_info, ctx, next| {
        let invoked = Arc::clone(&invoked_clone);
        Box::pin(async move {
            *invoked.lock().unwrap() = true;
            next.run(ctx).await
        })
    });

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(SuccessSystem));

    let mut graph = Graph::new();
    graph.add_scope("mw_scope", inner, ContextPolicy::shared());

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, Some(&mw)).await;

    assert!(result.is_ok(), "execution should succeed");
    assert!(
        *invoked.lock().unwrap(),
        "scope middleware handler should have been invoked"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Isolated Scope with Globals
// ─────────────────────────────────────────────────────────────────────────────

/// Global resource for testing isolated scope globals inheritance.
#[derive(Debug)]
struct GlobalConfig {
    name: String,
}
impl GlobalResource for GlobalConfig {}

/// System that reads a global resource.
struct ReadGlobalSystem {
    captured: Arc<Mutex<Option<String>>>,
}

impl System for ReadGlobalSystem {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let captured = Arc::clone(&self.captured);
        Box::pin(async move {
            let config = ctx
                .get_resource::<GlobalConfig>()
                .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
            *captured.lock().unwrap() = Some(config.name.clone());
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "read_global_system"
    }
}

#[tokio::test]
async fn scope_isolated_inherits_global_resources() {
    let captured = Arc::new(Mutex::new(None));

    let mut inner = Graph::new();
    inner.add_boxed_system(Box::new(ReadGlobalSystem {
        captured: Arc::clone(&captured),
    }));

    let mut graph = Graph::new();
    graph.add_scope("iso_globals", inner, ContextPolicy::isolated());

    // Create a context with global resources
    let mut globals = Resources::new();
    globals.insert(GlobalConfig {
        name: "global_value".to_string(),
    });
    let globals = Arc::new(globals);
    let mut ctx = SystemContext::with_globals(globals);

    let executor = GraphExecutor::new();
    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        result.is_ok(),
        "isolated scope should access globals, got: {:?}",
        result.unwrap_err()
    );
    assert_eq!(
        *captured.lock().unwrap(),
        Some("global_value".to_string()),
        "isolated scope should have read the global resource"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Graph Timeout Tests
// ─────────────────────────────────────────────────────────────────────────────

use polaris_graph::ExecutionError;
use test_utils::SlowSystem;

#[tokio::test]
async fn graph_timeout_fires_when_exceeded() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SlowSystem {
        duration: std::time::Duration::from_millis(200),
    }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new().with_max_duration(std::time::Duration::from_millis(50));

    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(result.is_err(), "expected timeout error");
    let err = result.unwrap_err();
    assert!(
        matches!(err, ExecutionError::GraphTimeout { .. }),
        "expected GraphTimeout, got: {err}"
    );
}

#[tokio::test]
async fn graph_timeout_does_not_fire_when_within_limit() {
    let mut graph = Graph::new();
    graph.add_boxed_system(Box::new(SlowSystem {
        duration: std::time::Duration::from_millis(50),
    }));

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new().with_max_duration(std::time::Duration::from_millis(500));

    let result = executor.execute(&graph, &mut ctx, None, None).await;

    assert!(
        result.is_ok(),
        "expected success within timeout, got: {:?}",
        result.unwrap_err()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// ExecutionResult Typed Output Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn execution_result_contains_typed_output() {
    async fn compute() -> i32 {
        42
    }

    let mut graph = Graph::new();
    graph.add_system(compute);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(result.output::<i32>(), Some(&42));
}

#[tokio::test]
async fn execution_result_output_wrong_type_returns_none() {
    async fn compute() -> i32 {
        42
    }

    let mut graph = Graph::new();
    graph.add_system(compute);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert!(
        result.output::<String>().is_none(),
        "requesting wrong type should return None"
    );
}

#[tokio::test]
async fn execution_result_has_output() {
    async fn compute() -> i32 {
        42
    }

    let mut graph = Graph::new();
    graph.add_system(compute);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert!(result.has_output(), "result should have an output");
}

#[tokio::test]
async fn execution_result_contains_last_system_output() {
    async fn first() -> i32 {
        10
    }
    async fn second() -> i32 {
        20
    }

    let mut graph = Graph::new();
    graph.add_system(first).add_system(second);

    let mut ctx = SystemContext::new();
    let executor = GraphExecutor::new();
    let result = executor
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<i32>(),
        Some(&20),
        "output should be from the last system executed"
    );
}
