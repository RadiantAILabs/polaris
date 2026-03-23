---
notion_page: https://www.notion.so/radiant-ai/System-327afe2e695d80679a9ad06058224db0
title: System Primitives
---

# System Primitives

## Overview

`polaris_system` provides the ECS-inspired primitives for building agents in Polaris.

## Systems

A system is a type implementing the `System` trait. Each system performs a single unit of computation, declaring its dependencies as function parameters.

The most common way to define a system is with the `#[system]` macro, which generates a `System` implementation from an async function.

```rust
use polaris_system_macros::system;
use polaris_system::param::{Res, ResMut};

#[system]
async fn reason(
    llm: Res<LLM>,
    memory: Res<Memory>,
) -> ReasoningResult {
    ReasoningResult { action: "search".into() }
}
```

As all state flows through parameters, a system has no hidden dependencies, which makes it testable in isolation and reusable across different graph topologies and agent patterns.

The macro generates two items: a struct that implements the `System` trait, and a factory function that returns an instance of that struct.

### Fallible Systems

Systems that may fail can return `Result<T, SystemError>`. The macro detects this pattern and extracts `T` as the system's output type. On success, `T` is stored in the context for downstream `Out<T>` access. On error, the `SystemError` propagates to the executor, which routes to an error edge or halts the graph.

```rust
#[system]
async fn reason(llm: Res<LLM>, memory: Res<Memory>) -> Result<ReasoningResult, SystemError> {
    let response = llm.generate(&memory.messages).await
        .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
    Ok(ReasoningResult { action: response.action })
}
```

In the example above, the output type is `ReasoningResult` (not `Result<ReasoningResult, SystemError>`), so downstream systems use `Out<ReasoningResult>`.

The `#[system]` macro sets `is_fallible()` to `true` for these systems automatically. This flag determines whether `add_error_handler()` auto-wires the node to an error handler subgraph. Manual `System` implementations that can fail with agentic errors must override `is_fallible()` to return `true`. See [Graph Execution — Error Handling](graph.md#error-handling) for the full error semantics.

<details>
<summary>Macro expansion details</summary>

For each parameter, the macro generates a call to `SystemParam::fetch()` to resolve the value from the `SystemContext`. It also generates an `access()` method that declares which resources the system reads or writes, enabling graph input validation.

For example, given the following input:

```rust
#[system]
async fn read_counter(counter: Res<Counter>, mut memory: ResMut<Memory>) -> Output {
    memory.record(counter.value);
    Output { value: counter.value }
}
```

The macro generates a `ReadCounterSystem` struct and a factory function `read_counter()`:

```rust
pub struct ReadCounterSystem;

impl System for ReadCounterSystem {
    type Output = Output;

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let counter = <Res<Counter> as SystemParam>::fetch(ctx)?;
            let mut memory = <ResMut<Memory> as SystemParam>::fetch(ctx)?;
            Ok({
                memory.record(counter.value);
                Output { value: counter.value }
            })
        })
    }

    fn name(&self) -> &'static str { "read_counter" }

    fn access(&self) -> SystemAccess {
        let mut access = SystemAccess::new();
        access.merge(&<Res<Counter> as SystemParam>::access());
        access.merge(&<ResMut<Memory> as SystemParam>::access());
        access
    }

    fn is_fallible(&self) -> bool {
        false // true when the return type is Result<T, SystemError>
    }
}

pub fn read_counter() -> ReadCounterSystem {
    ReadCounterSystem
}
```

</details>

### Zero-Parameter Systems

The primary purpose of the `#[system]` macro is to generate `SystemParam::fetch()` calls for each parameter. For async functions with no parameters, this code generation is unnecessary. These functions implement `IntoSystem` directly via a blanket implementation, and may be passed to `add_system()` without the macro:

```rust
async fn produce() -> Output {
    Output { value: 42 }
}

graph.add_system(produce);
```

## Context

The `SystemContext` is the execution context within which a system runs. It provides access to resources, outputs from previously executed systems, and optionally a parent context. This allows the creation of hierarchical contexts, mapping to the execution structure of an agent:

```rust
pub struct SystemContext<'parent> {
    parent: Option<&'parent SystemContext<'parent>>,
    globals: Option<&'parent Resources>,
    resources: Resources,
    outputs: Outputs,
}
```

```text
Server (global)
   │
   └── Agent Context
          │
          └── Session Context
                 │
                 └── Turn Context
```

When a system executes, the framework passes the current `SystemContext` to the system's `run` method. Each parameter is then resolved from this context before the system body executes.

## Parameters

Any type implementing `SystemParam` may be declared as a system parameter.

There are three built-in parameter types:

| Type | Resolution Scope | Access | Concurrent Borrows |
|------|------------------|--------|-------------------|
| `Res<T>` | Hierarchy (local → parents → global) | Immutable | Permitted |
| `ResMut<T>` | Current context only | Exclusive | None |
| `Out<T>` | Current context outputs | Immutable | Permitted |

**`Res<T>`** provides immutable access to a resource. `T` may implement either `GlobalResource` or `LocalResource`. Resolution traverses the `SystemContext` hierarchy upward, returning the first matching local resource or falling back to global resources. This shadowing semantic allows child contexts to override inherited resources. Multiple `Res<T>` borrows of the same type are permitted concurrently.

```rust
#[system]
async fn read_config(config: Res<Config>) -> Summary {
    Summary { prompt: config.system_prompt.clone() }
}
```

**`ResMut<T>`** provides exclusive mutable access to a resource in the current `SystemContext` only. `T` must implement `LocalResource`. A `ResMut<T>` borrow conflicts with any other concurrent borrow of `T`.

```rust
#[system]
async fn append_message(mut memory: ResMut<Memory>, response: Res<LLMResponse>) {
    memory.messages.push(response.message.clone());
}
```

**`Out<T>`** provides immutable access to the return value of a previously executed system. This is the mechanism for data flow between systems in a graph — one system's return value becomes another's `Out<T>` parameter.

```rust
#[system]
async fn execute(reasoning: Out<ReasoningResult>, tools: Res<ToolRegistry>) -> ToolResult {
    tools.execute(&reasoning.action).await
}
```

### Outputs

Outputs are the return values of systems. A system's return type is automatically inserted into the context's output store, and downstream systems read it via `Out<T>`.

For systems that produce multiple logical outputs, the return type should be a struct:

```rust
struct PlannerOutput {
    plan: Plan,
    confidence: f64,
}

#[system]
async fn plan(memory: Res<Memory>, llm: Res<LLM>) -> PlannerOutput {
    // ...
}
```

## Examples

A read-only system that computes a value from shared state:

```rust
#[system]
async fn score(config: Res<Config>, memory: Res<Memory>) -> Score {
    Score { value: memory.messages.len() as f64 * config.weight }
}
```

A system that mutates local state:

```rust
#[system]
async fn record_turn(mut history: ResMut<ConversationHistory>, response: Out<LLMResponse>) {
    history.turns.push(Turn {
        response: response.text.clone(),
        timestamp: Instant::now(),
    });
}
```

A chain of systems connected through outputs:

```rust
#[system]
async fn reason(llm: Res<LLM>, memory: Res<Memory>) -> ReasoningResult {
    ReasoningResult { action: "search".into(), query: "latest results".into() }
}

#[system]
async fn execute(reasoning: Out<ReasoningResult>, tools: Res<ToolRegistry>) -> ToolResult {
    tools.execute(&reasoning.action, &reasoning.query).await
}

#[system]
async fn synthesize(
    llm: Res<LLM>,
    reasoning: Out<ReasoningResult>,
    result: Out<ToolResult>,
) -> FinalResponse {
    llm.synthesize(&reasoning, &result).await
}
```
