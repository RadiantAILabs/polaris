---
notion_page: https://www.notion.so/radiant-ai/Graph-327afe2e695d80ffb941f98a5ec6d3ee
title: Graph Execution
---

# Graph Execution

Agent logic in Polaris is expressed as a directed graph of systems and control flow constructs. The `polaris_graph` crate provides the graph structure, a builder API for constructing it, and an executor for running it.

## Graphs

A `Graph` is a directed graph where nodes represent computation or control flow and edges define the connections between them. The graph is constructed using a builder API that handles node allocation, edge creation, and subgraph composition.

```rust
use polaris_graph::Graph;

let mut graph = Graph::new();
graph
    .add_system(receive_input)
    .add_system(reason)
    .add_system(respond);
```

The first node added becomes the graph's entry point. Each subsequent call to the builder connects the new node to the previous one via a sequential edge. This implicit chaining means that for linear pipelines, the builder reads as a sequence of steps.

Before execution, a graph can be validated via `graph.validate()`, which checks that: the graph has a valid entry point; all edges reference valid nodes; decision and switch nodes have the required predicates and branches; parallel nodes have branches; and loop nodes have a body and termination condition or iteration limit. Advanced checks include verifying that loop termination predicates can read outputs produced within the loop body and warning about conflicting output types in parallel branches.

A separate runtime validation pass, `executor.validate_resources()`, checks that all `Res<T>`, `ResMut<T>`, and `Out<T>` parameters can be satisfied before execution begins. Output reachability is validated along the linear (sequential) chain — each system's `Out<T>` dependencies are checked against outputs produced by preceding systems. Non-system nodes contribute all output types reachable from their subgraphs. See [Execution Context — Resource Validation](context.md#resource-validation) for details.

## Adding System Nodes

There are three methods for adding a system node to a graph, each suited to a different use case.

**`add_system`** — the most common method. Adds the node and returns `&mut Self` for fluent chaining. Use this for simple linear pipelines where no per-node configuration is needed.

```rust
graph
    .add_system(step_a)
    .add_system(step_b)
    .add_system(step_c);
```

**`add_system_node`** — adds the node and returns its `NodeId`. Use this when you need the ID for later reference, such as wiring conditional branches or attaching edges manually.

```rust
let reason_id = graph.add_system_node(reason);
let act_id = graph.add_system_node(act);
```

**`system`** — adds the node and returns a `SystemNodeBuilder` for configuring error handling, timeouts, and retry policies. Call `.done()` to return to `&mut Graph` for continued chaining.

```rust
graph.system(risky_operation)
    .with_timeout(Duration::from_secs(30))
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .on_error(|h: &mut Graph| { h.add_system(fallback); })
    .on_timeout(|h: &mut Graph| { h.add_system(timeout_handler); })
    .done()
    .add_system(next_step);
```

All three methods accept any type implementing `IntoSystemNode`, which includes bare async functions and `(schedule, system)` tuples for attaching custom hook schedules.

## Construction Patterns

### Sequential

Systems are connected in order. Each `add_system` call appends a node and links it to the previous one.

```rust
graph
    .add_system(reason)
    .add_system(act)
    .add_system(respond);
```

### Conditional Branch

A decision node evaluates a typed predicate against a system output and routes execution to one of two subgraphs. The type parameter specifies which system output type to read (e.g., `Out<ReasoningResult>`), and the predicate closure receives a reference to that output and returns a boolean.

```rust
graph
    .add_system(reason)
    .add_conditional_branch::<ReasoningResult, _, _, _>(
        "should_use_tool",
        |result| result.needs_tool,
        |g| g.add_system(execute_tool),
        |g| g.add_system(respond),
    );
```

After the selected branch completes, execution continues from the decision node's next sequential edge.

### Multi-Way Branch

A switch node evaluates a discriminator against a system output that returns a string key, then routes to the matching case subgraph. The type parameter specifies which system output type to read (e.g., `Out<ClassificationResult>`), and the discriminator closure receives a reference to that output and returns a case key.

```rust
graph
    .add_system(classify)
    .add_switch::<ClassificationResult, _, _>(
        "route",
        |result| result.category,
        vec![
            ("question", |g: &mut Graph| { g.add_system(answer); }),
            ("task", |g: &mut Graph| { g.add_system(execute); }),
        ],
        Some(|g: &mut Graph| { g.add_system(fallback); }),
    );
```

### Parallel Execution

A parallel node forks execution across multiple subgraphs. Each branch receives its own child context. Branches run concurrently — if any branch fails, the remaining branches are cancelled and the error propagates.

The parallel node is both the entry and exit point. Once all branches complete and their outputs are merged, execution continues from the parallel node's outgoing sequential edge.

```rust
graph
    .add_system(plan_tools)
    .add_parallel("execute_tools", vec![
        |g: &mut Graph| g.add_system(tool_a),
        |g: &mut Graph| g.add_system(tool_b),
    ])
    .add_system(aggregate_results);
```

### Loop

A loop node repeats its body subgraph until a termination predicate returns true or an iteration limit is reached. The termination predicate is evaluated before each iteration. The context persists across iterations, so outputs from iteration N are available to iteration N+1.

```rust
graph.add_loop::<LoopState, _, _>(
    "react_loop",
    |state| state.is_done || state.iterations >= 10,
    |g| {
        g.add_system(reason)
         .add_system(act)
         .add_system(observe);
    },
);
```

For loops that should run a fixed number of times without a predicate, `add_loop_n` accepts only an iteration count.

### Scope

A scope node executes an embedded graph with a configurable context boundary. A `ContextPolicy` is constructed upfront and passed to `add_scope`; it determines how the parent's `SystemContext` is shared with the inner graph:

| Policy | Context | Reads | Writes | Output Merge |
|--------|---------|-------|--------|--------------|
| `ContextPolicy::shared()` | Same as parent | Parent's resources | Parent's resources | Shared (no merge) |
| `ContextPolicy::inherit()` | `ctx.child()` | Walk parent chain + globals | Child's local scope | Merged back |
| `ContextPolicy::isolated()` | Fresh context (globals only) | Only globals + forwarded | Own local scope | Merged back |

```rust
// Inherit mode: child reads from parent, writes locally
graph.add_scope("sub_agent", inner_graph, ContextPolicy::inherit());

// Isolated mode with resource forwarding
let policy = ContextPolicy::isolated().forward::<Memory>();
graph.add_scope("sandboxed", inner_graph, policy);
```

Resource forwarding requires the type to implement `Clone`. The clone function is captured at policy-build time, so no separate `register_clone_fn()` call is needed at runtime.

After the inner graph completes, child outputs are merged back into the parent context. See [Execution Context — Context Flow](context.md#context-flow-through-graph-execution) for details.

### Per-Node Context Semantics

Different node types have different relationships to the `SystemContext`:

| Node Type | Creates Child Context? | Context Behavior |
|-----------|----------------------|------------------|
| **System** | No | Executes in parent's context directly |
| **Decision** | No | Evaluates predicate and branches in parent's context |
| **Switch** | No | Evaluates discriminator and routes in parent's context |
| **Loop** | No | Body runs in same context across iterations; outputs persist between iterations |
| **Parallel** | Yes (per branch) | Each branch gets `ctx.child()`; outputs merged back after all branches complete |
| **Scope** | Depends on mode | Shared: no; Inherit: `ctx.child()`; Isolated: fresh `SystemContext::with_globals()` |

## Nodes

Nodes are the vertices of the graph. Each node has a unique ID allocated.

```rust
pub enum Node {
    System(SystemNode),
    Decision(DecisionNode),
    Switch(SwitchNode),
    Parallel(ParallelNode),
    Loop(LoopNode),
    Scope(ScopeNode),
}
```

Most builder methods return `&mut Self` for chaining. When a `NodeId` is needed (for example, to attach an error handler), `add_system_node` returns the ID directly.

## Edges

Edges define the connections between nodes. They are stored in a flat vector alongside the nodes.

```rust
pub enum Edge {
    Sequential(SequentialEdge),
    Conditional(ConditionalEdge),
    Parallel(ParallelEdge),
    LoopBack(LoopBackEdge),
    Error(ErrorEdge),
    Timeout(TimeoutEdge),
}
```

`SequentialEdge` connects one node to the next and is the primary mechanism for linear flow. The builder creates these automatically when chaining nodes.

`ErrorEdge` and `TimeoutEdge` define fallback paths from a system node to a handler subgraph.

`LoopBackEdge` connects the end of a loop body back to the loop node.

## Execution

The `GraphExecutor` traverses a graph starting from the entry node, executing each node and following edges to determine the next step.

```rust
pub struct GraphExecutor;

impl GraphExecutor {
    pub async fn execute(
        &self,
        graph: &Graph,
        ctx: &mut SystemContext<'_>,
        hooks: Option<&HooksAPI>,
        middleware: Option<&MiddlewareAPI>,
    ) -> Result<ExecutionResult, ExecutionError>;
}
```

When a system returns a value, the executor inserts it into the context's output storage keyed by `TypeId`. Downstream systems access it via `Out<T>`, which fetches from the same storage. If multiple systems return the same type, the last write wins. Outputs persist for the duration of graph execution.

After graph execution completes, the `ExecutionResult` contains the terminal system's output. Use `result.output::<T>()` to downcast:

```rust
let result = executor.execute(&graph, &mut ctx, None, None).await?;
let answer = result.output::<MyOutput>(); // Option<&MyOutput>
```

### Graph-Level Timeout

A total execution time limit can be set at two levels: on the `Graph` itself (via `with_max_duration`) or on the `GraphExecutor` (via `with_max_duration`). When the timeout elapses, the executor returns `ExecutionError::GraphTimeout` and fires `OnGraphFailure` hooks — unlike wrapping with `tokio::time::timeout` externally, which bypasses hooks and middleware.

**Graph-level timeout** — declared as part of the graph definition, travels with the graph. Use this when the agent author knows the intended time budget for their graph:

```rust
let mut graph = Graph::new();
graph
    .with_max_duration(Duration::from_secs(30))
    .add_system(step_one)
    .add_system(step_two);
```

**Executor-level timeout** — a fallback default applied by the runtime across all graphs it executes:

```rust
let executor = GraphExecutor::new()
    .with_max_duration(Duration::from_secs(60));

let result = executor.execute(&graph, &mut ctx, None, None).await;
// On timeout: Err(ExecutionError::GraphTimeout { elapsed, max })
```

**Precedence** — when both are set, the graph's `max_duration` wins. The executor's value is only used as a fallback when the graph does not declare one:

```rust
let effective_timeout = graph.max_duration().or(executor_max_duration);
```

**Scope graphs** — a scope node's embedded `Graph` can declare its own `max_duration`, which is enforced independently around that scope's execution. This lets authors give nested subgraphs their own time budgets without affecting the parent. When a scope graph times out, `OnScopeComplete` is **not** fired for that scope. The `ExecutionError::GraphTimeout` propagates up, and `OnGraphFailure` fires on the top-level (parent) graph — there is no scope-specific failure hook.

**`Graph::append`** — when a graph with `max_duration` is appended into another, the appended graph's timeout is discarded. The receiving graph's timeout policy takes precedence. A warning is logged if the appended graph had a timeout set but the receiver did not.

#### Cancel Safety

Graph-level timeout is a **hard abort**. When it fires mid-execution, tokio drops the currently-running system's future at its next `.await` point. Concretely:

- The cancelled system's Rust `Drop` impls run (RAII cleanup works), but any `async` code after the cancellation point does not execute.
- Writes already made to `SystemContext` via `ResMut<T>` persist; writes that would have happened after the cancellation are lost.
- External side effects (HTTP calls, DB writes) dispatched before the `.await` may have committed remotely but the response is never observed — the system cannot distinguish "never happened" from "happened but unacknowledged."
- The cancelled system's **error and timeout edges are not invoked** — graph-level timeout bypasses node-dispatch entirely.
- `OnSystemComplete` / `OnSystemError` hooks for the in-flight system do not fire. Only `OnGraphFailure` fires on the top-level graph, with `ExecutionError::GraphTimeout { elapsed, max }` — which does not identify which node was executing when the timeout fired.
- For scope graphs using `ContextPolicy::shared()`, the parent context may retain partial writes from systems that ran before the timeout fired.

This is distinct from **per-node timeout** (`SystemNodeBuilder::with_timeout` or `Graph::set_timeout(node_id, duration)`), which integrates with retry policies and timeout handler edges for structured recovery. Use per-node timeout when you need handler-based recovery; use graph-level timeout for hard time budgets.

Subgraph execution (branches, loop bodies, case handlers) is recursive with depth tracking. The default recursion limit is 64.

## Error Handling

Errors in graph execution fall into two categories with distinct handling semantics.

**Agentic errors** are anticipated failure modes within a system's domain — an LLM refusing a prompt, a tool returning an invalid result, a validation check failing. These are errors the agent is designed to reason about and recover from. Systems signal agentic errors by returning `Result<T, SystemError>` and are marked fallible by the `#[system]` macro (via `is_fallible() = true`). Error handler nodes are part of the agent's own graph and represent recovery logic the agent controls.

**Infrastructure errors** are failures outside the agent's responsibility — a missing resource, a network partition, a misconfigured context. These are not wired to error handler nodes because the agent cannot meaningfully recover from them within its graph. Instead, they propagate as `ExecutionError` from `executor.execute()`, where the agent implementer handles them directly.

This separation is enforced by the builder: `add_error_handler()` only auto-wires nodes where `is_fallible()` returns `true`. Infrastructure failures (e.g., `ParamError` from a missing resource) bypass error handler nodes entirely and escalate to the caller. Manual `System` implementations that can fail with agentic errors must override `is_fallible()` to return `true` for error handler wiring to apply.

### Error Edges

When a system node fails, the executor checks for an `ErrorEdge` from that node. If one exists, execution continues at the error handler subgraph. If none exists, the error propagates and execution stops.

```rust
// Per-node error handler:
let risky_id = graph.add_system_node(risky_operation);
graph.add_error_handler_for(risky_id, |g| {
    g.add_system(fallback_operation);
});

// Global error handler (auto-wires all fallible nodes without an existing error edge):
graph.add_error_handler(|g| {
    g.add_system(global_fallback);
});

// Closure-based error handler (no system definition needed):
graph.add_error_handler_fn(|error: &CaughtError| -> ErrorResponse {
    ErrorResponse { code: 500, message: error.message.to_string() }
});

// Closure-based handler for specific nodes:
graph.add_error_handler_fn_for([risky_id], |error: &CaughtError| -> String {
    format!("handled: {}", error.message)
});
```

The closure-based variants (`add_error_handler_fn`, `add_error_handler_fn_for`) are a convenience for trivial error mapping where defining a full `#[system]` function would be boilerplate. The closure receives `&CaughtError` and returns a value of type `T`, which is stored as the handler's output. The `SystemNodeBuilder` also exposes `on_error_fn` for inline use:

```rust
graph.system(risky_operation)
    .on_error_fn(|error: &CaughtError| -> ErrorResponse {
        ErrorResponse { code: 500, message: error.message.to_string() }
    })
    .done()
    .add_system(next_step);
```

### Timeout Handling

A system node can have a timeout set via `set_timeout`. The executor wraps the system call in `tokio::time::timeout`. If the timeout elapses, the executor checks for a `TimeoutEdge`. If one exists, execution continues at the timeout handler. If none exists, the executor returns `ExecutionError::Timeout`.

```rust
let slow_id = graph.add_system_node(slow_operation);
graph.set_timeout(slow_id, Duration::from_secs(5));
graph.add_timeout_handler(slow_id, |g| {
    g.add_system(timeout_fallback);
});
```

### Retry Policy

A system node can optionally have a retry policy. By default, no retry policy is set — a failed or timed-out system node immediately triggers its error or timeout edge. When a retry policy is configured, the executor retries the system up to `max_retries` additional times before giving up.

Two strategies are available:

- **Fixed** — constant delay between retries.
- **Exponential** — delay doubles each attempt (`2^attempt * initial_delay`), optionally capped by a maximum delay.

```rust
use std::time::Duration;
use polaris_graph::RetryPolicy;

// Fixed: retry up to 3 times with 100ms between attempts
graph
    .system(flaky_operation)
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .done();

// Exponential backoff: retry up to 5 times, starting at 50ms, capped at 2s
graph
    .system(network_call)
    .with_retry(
        RetryPolicy::exponential(5, Duration::from_millis(50))
            .with_max_delay(Duration::from_secs(2)),
    )
    .done();
```

Both errors and timeouts count as failed attempts. After all retries are exhausted, the final outcome is forwarded to the error or timeout edge as usual.

### ExecutionError Variants

| Variant | Cause |
|---------|-------|
| `EmptyGraph` | Graph has no entry point |
| `NodeNotFound(NodeId)` | Referenced node not in graph |
| `NoNextNode(NodeId)` | No sequential edge from node (terminal) |
| `MissingPredicate(NodeId)` | Decision/loop node missing its predicate |
| `MissingBranch { node, branch }` | Decision node missing true/false branch target |
| `MissingDiscriminator(NodeId)` | Switch node missing its discriminator |
| `NoMatchingCase { node, key }` | Switch: no case for key and no default |
| `SystemError(Arc<str>)` | System execution returned an error |
| `PredicateError(PredicateError)` | Predicate evaluation failed |
| `MaxIterationsExceeded { node, max }` | Loop exceeded iteration limit |
| `NoTerminationCondition(NodeId)` | Loop has neither predicate nor `max_iterations` |
| `Timeout { node, timeout }` | System execution exceeded timeout |
| `GraphTimeout { elapsed, max }` | Total graph execution exceeded `max_duration` |
| `RecursionLimitExceeded { depth, max }` | Nested control flow too deep (default: 64) |
| `MiddlewareError { middleware, message }` | A middleware layer failed |
| `InternalError(String)` | Framework invariant violated |
| `Unimplemented(&str)` | Feature not yet implemented |

### Error Context (ErrOut)

When a system fails and an error edge exists, the executor stores a `CaughtError` in the outputs before routing to the handler. Error handler systems read it via `ErrOut<CaughtError>`:

```rust
use polaris_graph::CaughtError;
use polaris_system::param::ErrOut;

#[system]
async fn handle_error(error: ErrOut<CaughtError>) -> RecoveryState {
    tracing::error!(
        "[{}] {} failed after {:?}: {}",
        error.node_id, error.system_name, error.duration, error.message
    );
    RecoveryState::default()
}
```

`CaughtError` contains: `message`, `system_name`, `node_id`, `duration`, and `kind` (`Execution` or `ParamResolution`).

## Hooks

The hook system provides extension points for observing and modifying graph execution at specific lifecycle events. Hooks are registered by plugins during the build phase via `HooksAPI` and invoked by the executor at runtime.

There are two kinds of hooks. **Observer hooks** are side-effect-only callbacks for logging, metrics, and tracing. **Provider hooks** inject resources into the `SystemContext` before a system executes, making them available to the system via `Res<T>`.

### Schedules

Each hook is registered against one or more schedule types. The executor invokes hooks for a given schedule at the corresponding point in graph traversal. All hooks receive a `&GraphEvent` and match on the relevant variant for typed access.

**Graph-level:** `OnGraphStart`, `OnGraphComplete`, `OnGraphFailure` — fired before execution begins, after it completes, and when it fails.

**System-level:** `OnSystemStart`, `OnSystemComplete`, `OnSystemError` — fired around each system node's execution.

**Decision:** `OnDecisionStart`, `OnDecisionComplete` — fired before a decision node evaluates its predicate and after a branch has executed.

**Switch:** `OnSwitchStart`, `OnSwitchComplete` — fired before a switch node evaluates its discriminator and after a case has executed.

**Loop:** `OnLoopStart`, `OnLoopIteration`, `OnLoopEnd` — fired before the loop begins, at the start of each iteration, and after the loop completes.

**Parallel:** `OnParallelStart`, `OnParallelComplete` — fired before parallel branches start and after all branches complete.

**Scope:** `OnScopeStart`, `OnScopeComplete` — fired before scope entry and after scope completion. Includes `context_mode` and inner node count.

When multiple hooks are registered for the same schedule, they execute in registration order, and each hook sees context changes made by previous hooks.

### Custom System Schedules

System nodes can be tagged with custom schedules. When the executor runs a tagged system, it re-emits the standard system lifecycle events (`SystemStart`, `SystemComplete`, `SystemError`) on each custom schedule in addition to the built-in schedules. This allows hooks to subscribe to lifecycle events for specific systems rather than all systems.

Define a custom schedule by implementing `Schedule`, then attach it when adding the system to the graph:

```rust
struct OnToolCall;
impl Schedule for OnToolCall {}

graph.add_system((OnToolCall, execute_tool));
```

Multiple custom schedules can be attached using a tuple:

```rust
graph.add_system(((OnToolCall, OnExpensiveOp), execute_tool));
```

### DevToolsPlugin

`DevToolsPlugin` demonstrates provider hooks. It registers a hook on `OnSystemStart` that injects `SystemInfo` into the context before each system executes. Systems can then access the current node ID and system name via `Res<SystemInfo>`:

```rust
#[system]
async fn my_system(info: Res<SystemInfo>) {
    println!("Running node {:?}: {}", info.node_id(), info.system_name());
}
```

## Middleware

Middleware wraps execution units with custom logic that must span the unit's duration — for example, holding a tracing span open across a system's execution, which is impossible with two disconnected hook events.

Each middleware is registered against a target type that determines which execution unit it wraps. The handler receives typed `info` metadata, `&mut SystemContext`, and a `Next` value. Every handler **must** call `next.run(ctx)` exactly once — dropping `next` without invoking it produces an `ExecutionError::InternalError`.

### Standalone Registration

```rust
use polaris_graph::middleware::{MiddlewareAPI, info::SystemInfo};

let mw = MiddlewareAPI::new();
mw.register_system("timer", |info: SystemInfo, ctx, next| {
    Box::pin(async move {
        let start = std::time::Instant::now();
        let result = next.run(ctx).await;
        tracing::info!("{}: {:?}", info.node_name, start.elapsed());
        result
    })
});

// Pass to the executor:
executor.execute(&graph, &mut ctx, None, Some(&mw)).await?;
```

### Plugin Registration

In a plugin, register middleware via `MiddlewareAPI` during `build()`:

```rust
impl Plugin for TracingPlugin {
    fn build(&self, server: &mut Server) {
        let mw = server.api::<MiddlewareAPI>()
            .expect("GraphPlugin must be added first");

        mw.register_system("tracing::system", |info: SystemInfo, ctx, next| {
            Box::pin(async move {
                let span = tracing::info_span!("system", name = info.node_name);
                let _guard = span.enter();
                next.run(ctx).await
            })
        });

        mw.register_graph("tracing::graph", |info: GraphInfo, ctx, next| {
            Box::pin(async move {
                let span = tracing::info_span!("graph", nodes = info.node_count);
                let _guard = span.enter();
                next.run(ctx).await
            })
        });
    }
}
```

### Targets

| Target | Info type | Scope |
|--------|-----------|-------|
| `GraphExecution` | `GraphInfo` | Entire graph run |
| `System` | `SystemInfo` | Single system node |
| `Decision` | `DecisionInfo` | Decision node evaluation |
| `Switch` | `SwitchInfo` | Switch node evaluation |
| `Loop` | `LoopInfo` | Entire loop node |
| `LoopIteration` | `LoopIterationInfo` | Single loop iteration |
| `Parallel` | `ParallelInfo` | Entire parallel node |
| `ParallelBranch` | `ParallelBranchInfo` | Single parallel branch |
| `Scope` | `ScopeInfo` | Scope node execution |

### Layer Ordering

Multiple middlewares on the same target form a chain. The last registered is outermost. Hooks execute inside all middleware layers, between the innermost middleware and the execution unit. If A is registered before B:

```text
B (enter) → A (enter) → hooks → execute → hooks → A (exit) → B (exit)
```
