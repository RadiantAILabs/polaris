Directed-graph execution primitives for agent behavior.

Agent logic in Polaris is expressed as a directed graph where nodes represent
computation or control flow and edges define connections. This module provides
the graph structure, a builder API, and an executor (Layer 2).

# Graph Construction

```no_run
# use polaris_ai::graph::Graph;
# async fn receive_input() {}
# async fn reason() {}
# async fn respond() {}
let mut graph = Graph::new();
graph
    .add_system(receive_input)
    .add_system(reason)
    .add_system(respond);
```

The first node added is the entry point. Each `add_system` call connects the
new node to the previous one via a sequential edge.

# Node Types

| Type | Purpose | Context behavior |
|------|---------|-----------------|
| **System** | Execute a system function | Runs in parent context |
| **Decision** | Binary branch on a typed predicate | Runs in parent context |
| **Switch** | Multi-way branch on a string discriminator | Runs in parent context |
| **Parallel** | Fork into concurrent branches | Each branch gets `ctx.child()` |
| **Loop** | Repeat body until predicate or limit | Same context across iterations |
| **Scope** | Embedded subgraph with configurable isolation | Shared, Inherit, or Isolated mode |
| **Dynamic** | Select a signature-checked candidate subgraph at runtime | Like Scope, per its `ContextPolicy` |

## Construction Patterns

**Conditional branch:**

```no_run
# use polaris_ai::graph::Graph;
# struct ReasoningResult { needs_tool: bool }
# async fn reason() -> ReasoningResult { ReasoningResult { needs_tool: false } }
# async fn execute_tool() {}
# async fn respond() {}
# let mut graph = Graph::new();
graph.add_system(reason)
    .add_conditional_branch::<ReasoningResult, _, _, _>(
        "needs_tool",
        |r| r.needs_tool,
        |g| { g.add_system(execute_tool); },
        |g| { g.add_system(respond); },
    );
```

**Multi-way branch:**

```no_run
# use polaris_ai::graph::Graph;
# struct ClassificationResult { category: &'static str }
# async fn classify() -> ClassificationResult { ClassificationResult { category: "question" } }
# async fn answer() {}
# async fn execute() {}
# async fn fallback() {}
# let mut graph = Graph::new();
graph.add_system(classify)
    .add_switch::<ClassificationResult, _, _, _>(
        "route",
        |r| r.category,
        vec![
            ("question", Box::new(|g: &mut Graph| { g.add_system(answer); }) as Box<dyn FnOnce(&mut Graph)>),
            ("task", Box::new(|g: &mut Graph| { g.add_system(execute); })),
        ],
        Some(Box::new(|g: &mut Graph| { g.add_system(fallback); })),
    );
```

**Parallel execution:**

```no_run
# use polaris_ai::graph::Graph;
# async fn tool_a() {}
# async fn tool_b() {}
# let mut graph = Graph::new();
graph.add_parallel("execute_tools", vec![
    |g: &mut Graph| { g.add_system(tool_a); },
    |g: &mut Graph| { g.add_system(tool_b); },
]);
```

**Loop** (the termination predicate is evaluated *before* each iteration,
including the first — so its input must be produced before the loop, normally
by an init system; a caller driving the graph directly may instead pre-seed
the context before executing):

```no_run
# use polaris_ai::graph::Graph;
# struct LoopState { is_done: bool }
# async fn init_state() -> LoopState { LoopState { is_done: false } }
# async fn reason() -> LoopState { LoopState { is_done: true } }
# async fn act() {}
# async fn observe() {}
# let mut graph = Graph::new();
graph.add_system(init_state); // the first termination check reads this
graph.add_loop::<LoopState, _, _>(
    "react_loop",
    |state| state.is_done,
    |g| { g.add_system(reason).add_system(act).add_system(observe); },
);
```

**Dynamic selection** (runtime choice among signature-checked subgraphs; see
[`SubgraphRegistry`](crate::graph::SubgraphRegistry) for a per-session,
swappable candidate set):

```no_run
# use polaris_ai::graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature};
# async fn use_tool() -> i32 { 1 }
# async fn respond() -> i32 { 2 }
# let mut graph = Graph::new();
let mut tool = Graph::new();
tool.add_system(use_tool);
let mut reply = Graph::new();
reply.add_system(respond);

graph.add_dynamic(
    "route",
    |_ctx| "respond", // any `impl Into<Arc<str>>` key
    [("tool", tool), ("respond", reply)],
    DynamicSlot::new(
        GraphSignature::new().produce::<i32>(),
        ContextPolicy::shared(),
    )
    .with_default_key("respond"),
);
```

# Edge Types

| Type | Purpose |
|------|---------|
| **Sequential** | Linear flow (created automatically by builder chaining) |
| **Conditional** | True/false branch from a Decision node |
| **Parallel** | Fork to parallel branches |
| **`LoopBack`** | End of loop body back to loop node |
| **Error** | Route from failed system to error handler subgraph |
| **Timeout** | Route from timed-out system to timeout handler |

# Execution

[`GraphExecutor`](crate::graph::GraphExecutor) traverses the graph, executing nodes and following edges.
System outputs are stored in the context keyed by `TypeId` and read via
[`Out<T>`](crate::system::param::Out).

A total execution time limit can be set on the `Graph` itself or on the
`GraphExecutor`. Graph-level declarations travel with the graph; the executor
value acts as a fallback. When both are set, the graph wins.

```no_run
# use polaris_ai::graph::{Graph, GraphExecutor};
# use polaris_ai::system::param::SystemContext;
# use std::time::Duration;
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Graph-level (travels with the graph):
let mut graph = Graph::new();
graph.with_max_duration(Duration::from_secs(30));

// Executor-level (fallback default across all graphs):
let executor = GraphExecutor::new()
    .with_max_duration(Duration::from_secs(60));

# let mut ctx = SystemContext::new();
let result = executor.execute(&graph, &mut ctx, None, None).await?;
# Ok(())
# }
```

# Error Handling

Two error categories with distinct semantics:

- **Agentic errors** -- anticipated failures (LLM refusal, tool error).
  Systems return `Result<T, SystemError>` and are marked fallible. Error
  handler subgraphs provide recovery logic within the graph.
- **Infrastructure errors** -- missing resources, network partitions.
  Propagate as [`ExecutionError`](crate::graph::ExecutionError) to the caller; not routed to error handlers.

```no_run
# use polaris_ai::graph::{Graph, RetryPolicy};
# use std::time::Duration;
# async fn risky_operation() -> Result<(), polaris_ai::system::system::SystemError> { Ok(()) }
# async fn fallback() {}
# async fn global_fallback() {}
# async fn next_step() {}
# let mut graph = Graph::new();
// Per-node error handler
graph.system(risky_operation)
    .on_error(|h: &mut Graph| { h.add_system(fallback); })
    .with_timeout(Duration::from_secs(30))
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .done();

// Global error handler (auto-wires all fallible nodes)
graph.add_error_handler(|g| { g.add_system(global_fallback); });
```

# Hooks

Extension points for observing and modifying execution at lifecycle events,
registered via [`HooksAPI`](crate::graph::hooks::HooksAPI).

**Observer hooks** -- side-effect-only (logging, metrics).
**Provider hooks** -- inject resources before a system executes.

Schedules: `OnGraphStart/Complete/Failure`, `OnSystemStart/Complete/Error`,
`OnDecisionStart/Complete`, `OnSwitchStart/Complete`, `OnLoopStart/Iteration/End`,
`OnParallelStart/Complete`, `OnScopeStart/Complete`, `OnDynamicStart/Complete`.

# Middleware

Wraps execution units with logic that spans the unit's duration (e.g., tracing
spans). Registered via [`MiddlewareAPI`](crate::graph::middleware::MiddlewareAPI) against a
target type (Graph, System, Decision, Switch, Loop, Parallel, Scope, Dynamic).

```no_run
# use polaris_ai::graph::middleware::{MiddlewareAPI, info::SystemInfo};
# use polaris_ai::system::param::SystemContext;
let mw = MiddlewareAPI::new();
mw.register_system("timer", |info: SystemInfo, ctx, next| {
    Box::pin(async move {
        let start = std::time::Instant::now();
        let result = next.run(ctx).await;
        result
    })
});
```

# Validation

`graph.validate()` checks structural validity (entry point, edge
connectivity, predicate/branch presence). `executor.validate_resources()`
checks that all `Res<T>`, `ResMut<T>`, and `Out<T>` parameters can be
satisfied before execution. Independently, the executor always verifies at
run start that every main-chain loop's termination predicate input is
produced before the loop or already present in the context, failing fast
with `LoopPredicateInputMissingOnEntry` otherwise.

Conditional production is phase-specific. Composition signatures are
pessimistic: a decision or switch branch is not guaranteed to run, so its
outputs cannot erase a declared requirement. Pre-flight and run-start checks
are optimistic: they credit every branch that might run, preserving outputs
from nested scopes, parallel branches, and dynamic contracts. After a branch
is selected, execution observes the live output channel exactly and returns a
typed missing-output error if the selected path did not produce the value.

Every such check follows the layer's verification-phase rules. Checks run
at one of five phases -- Rust compile time, graph validate time,
composition time (signature matching), run start, execution time -- and
each phase may reject only on facts no later phase could change: validate
time sees only the fragment's own structure, signature derivation is
pessimistic (interfaces never under-claim), run-start checks are
optimistic (they reject only what cannot succeed on any path), and
execution-time failures are typed and routed, never panics. The full rules
live in the repository's `docs/reference/graph.md` under "Verification
Phases".

# Related

- [Systems and parameters](crate::system) -- the primitives that graph nodes execute
- [Agent trait](crate::agent) -- packaging graphs as reusable behavior patterns
- [Sessions](crate::sessions) -- session-managed graph execution
