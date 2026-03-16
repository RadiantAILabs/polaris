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

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_system::param::{Res, ResMut, SystemAccess, SystemContext, SystemParam};
use polaris_system::resource::{GlobalResource, LocalResource};
use polaris_system::server::Server;
use polaris_system::system::{BoxFuture, System, SystemError};

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

    // - Last output is available
    let output = ctx.get_output::<ComputeResult>().unwrap();
    assert_eq!(output.value, 30);

    // - Execution stats are correct
    let stats = result.unwrap();
    assert_eq!(stats.nodes_executed, 2);
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

    let result = executor.execute(&graph, &mut ctx, None, None).await;
    assert!(result.is_ok());

    let output = ctx.get_output::<BranchResult>().unwrap();
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
    let output = ctx.get_output::<DiamondResult>().unwrap();
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

    let result = executor.execute(&graph, &mut ctx, None, None).await;
    assert!(result.is_ok(), "Execution failed: {:?}", result.err());

    let output = ctx.get_output::<DiamondResult>().unwrap();
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

    let result = executor.execute(&graph, &mut ctx, None, None).await;
    assert!(result.is_ok(), "Execution failed: {:?}", result.err());

    // Final output should be from the converge step (after the branch)
    let output = ctx.get_output::<DiamondResult>().unwrap();
    assert_eq!(output.step, "converge");
    assert_eq!(output.value, 100);
}
