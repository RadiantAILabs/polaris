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

**Dynamic selection** (runtime choice among signature-checked subgraphs;
[`MatchingMode`](crate::graph::MatchingMode) selects how strictly a candidate is
held to the slot contract, and see
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
| **Sequential** | Linear flow (created automatically by builder chaining; at most one per node) |
| **Conditional** | True/false branch from a Decision node |
| **Parallel** | Fork to parallel branches |
| **`LoopBack`** | End of loop body back to loop node |
| **Error** | Route from failed system to error handler subgraph (at most one per node) |
| **Timeout** | Route from timed-out system to timeout handler (at most one per node) |

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

# Cancellation

A run carries at most one stop signal, split in two:
[`CancellationSource`](crate::graph::CancellationSource) raises it,
[`RunCancellation`](crate::graph::RunCancellation) — an ordinary per-run
`LocalResource` — notices it. Whoever drives the executor inserts the observer
into the run's root context; a caller that inserts nothing gets today's
behaviour, and the run is not cancellable.

```no_run
# use polaris_ai::graph::{CancellationSource, Graph, GraphExecutor};
# use polaris_ai::system::param::SystemContext;
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
# let graph = Graph::new();
let source = CancellationSource::new();
let mut ctx = SystemContext::new();
ctx.insert(source.token());

// From a signal-listener task, a UI callback, or another task:
source.cancel();

# let executor = GraphExecutor::new();
let outcome = executor.execute(&graph, &mut ctx, None, None).await;
// Err(ExecutionError::Interrupted { run_id, node, nodes_executed, elapsed })
# Ok(())
# }
```

The executor checks the signal at the node boundary — before starting a node,
and before letting a node-attributable error propagate — and **never drops a
running future**, including a parallel sibling's. An interrupt is not a
failure of any system: no error or timeout handler is entered, the last node's
hooks fire normally, and `OnGraphFailure` reports
[`ExecutionError::Interrupted`](crate::graph::ExecutionError). A run that
reaches its terminal node completes — including a final parallel node, unless
one of its branches declined a node because the signal was raised.

How promptly a node stops is the system's choice, and both choices are safe: a
system that races `RunCancellation::run_until_cancelled` returns at once, and
one that never reads the token runs to completion while the run stops at the
next boundary. Sessions raise the signal through
[`SessionsAPI`](crate::sessions::SessionsAPI)'s `interrupt_turn`.
Details live in the repository's `docs/reference/graph.md` under
"Cancellation".

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
connectivity, predicate/branch presence, and that no node declares more than
one route for the same outcome — success, error, or timeout: lookup is
first-match, so a second route is never taken, and it is reported as
[`ValidationError::AmbiguousRoute`](crate::graph::ValidationError::AmbiguousRoute)).
`executor.validate_resources()`
checks that all `Res<T>`, `ResMut<T>`, and `Out<T>` parameters can be
satisfied before execution. Independently, the executor always verifies at
run start that every main-chain loop's termination predicate input is
produced before the loop or already present in the context, failing fast
with `LoopPredicateInputMissingOnEntry` otherwise.

Conditional production is phase-specific. Composition signatures are
pessimistic: they erase a declared requirement only for production the
runtime guarantees. A parallel node's branches all run and a scope's
interior always merges back, so what those regions guarantee is credited to
the enclosing chain; a decision or switch case, a loop body, and a dynamic
candidate are not guaranteed to run, so their outputs are not. Production
behind an error or timeout handler is not credited either -- a caught
failure diverts into the handler and ends the region, so the region
completes with that output missing. Pre-flight and run-start checks are
optimistic instead: they credit every branch that might run, preserving
outputs from nested scopes, parallel branches, and dynamic contracts. After
a branch is selected, execution observes the live output channel exactly and
returns a typed missing-output error if the selected path did not produce
the value.

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
