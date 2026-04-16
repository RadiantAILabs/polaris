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

**Loop:**

```no_run
# use polaris_ai::graph::Graph;
# struct LoopState { is_done: bool }
# async fn reason() -> LoopState { LoopState { is_done: true } }
# async fn act() {}
# async fn observe() {}
# let mut graph = Graph::new();
graph.add_loop::<LoopState, _, _>(
    "react_loop",
    |state| state.is_done,
    |g| { g.add_system(reason).add_system(act).add_system(observe); },
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
`OnParallelStart/Complete`, `OnScopeStart/Complete`.

# Middleware

Wraps execution units with logic that spans the unit's duration (e.g., tracing
spans). Registered via [`MiddlewareAPI`](crate::graph::middleware::MiddlewareAPI) against a
target type (Graph, System, Decision, Switch, Loop, Parallel, Scope).

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
satisfied before execution.

# Related

- [Systems and parameters](crate::system) -- the primitives that graph nodes execute
- [Agent trait](crate::agent) -- packaging graphs as reusable behavior patterns
- [Sessions](crate::sessions) -- session-managed graph execution
