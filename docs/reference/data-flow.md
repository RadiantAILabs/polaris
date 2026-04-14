---
notion_page: https://www.notion.so/radiant-ai/Data-Flow-Patterns-342afe2e695d80589006cb6e61ddb705
title: Data Flow Patterns
---

# Data Flow Patterns

Polaris systems declare their dependencies as typed parameters, and the framework wires data between systems through three mechanisms: resources (`Res<T>`, `ResMut<T>`) and outputs (`Out<T>`). This document is a decision guide for choosing the right one.

## Decision Table

| Pattern | Use | Avoid |
|---------|-----|-------|
| Step A's result feeds step B | **`Out<T>`** — A returns `T`, B declares `Out<T>` | `ResMut<SharedState>` with `Option` fields |
| Immutable per-request input (HTTP body, auth claims, correlation ID) | **`Res<T>`** via `ctx.insert(T)` in setup closure | `ResMut<WorkingState>` with `.input.clone()` |
| Accumulated cross-cutting state (issue list, conversation history, counters) | **`ResMut<T>`** — local resource mutated across systems | `Out<T>` — outputs are per-system, not accumulated |
| Shared server-wide config (model registry, tool registry) | **`Res<T>`** — global resource via `server.insert_global(T)` | `ResMut<T>` — compile error on `GlobalResource` |
| Error context inside an error handler | **`ErrOut<CaughtError>`** — injected by the executor on error edges | Reading from a custom `ResMut<LastError>` |
| Per-system lifecycle metadata (node ID, system name) | **`Res<SystemInfo>`** provided by `DevToolsPlugin` | Manual tagging in each system |

## The Three Mechanisms

### `Out<T>` — system-to-system handoff

A system's return type is inserted into the context's output store under its concrete type. The *next* system declaring `Out<T>` reads it by type. This is the primary data pipeline within a graph.

- **Lifetime:** current scope only. Parallel branches see only their own parent's outputs; loops overwrite each iteration.
- **Resolution:** by `TypeId`. Two systems that return the same `T` in a linear chain mean the second overwrites the first.
- **Use when:** the value is produced by one step and consumed by the next.

### `Res<T>` — immutable shared data

`Res<T>` resolves up the hierarchy: current locals → parent locals → globals. Good for data that's set once and read many times.

- **Lifetime:** resource's owning scope (usually the session or the server).
- **Resolution:** first match wins (child shadows parent).
- **Use when:** the data is input-like (config, request body, registry handle) or identity-like (session ID, user context).

### `ResMut<T>` — exclusive mutable state

`ResMut<T>` is strict: the resource must be in the *current* context (no hierarchy walk), `T` must be `LocalResource`, and only one borrow is allowed concurrently.

- **Lifetime:** current context only.
- **Use when:** state genuinely accumulates across systems in the same session or turn (conversation history, diagnostics list).
- **Not:** as a god-struct that holds everyone's data. If the struct has `Option<StepAResult>`, `Option<StepBResult>`, etc., you're rebuilding `Out<T>` badly.

## Common Anti-Patterns

### The Working-State God-Struct

```rust
struct WorkingState {
    input: Option<RequestBody>,
    normalized: Option<NormalizedResult>,
    summary: Option<Summary>,
    output: Option<FinalResponse>,
}
```

Every system takes `ResMut<WorkingState>` and fills in its field. This defeats the purpose of declaring dependencies: the type signature no longer tells you what each system reads or writes. It also loses the compile-time guarantee that `Out<NormalizedResult>` is non-`None` inside the summary step.

**Fix:** let each system return its result and declare `Out<PrevResult>` on the reader. Use `Res<RequestBody>` for the immutable input. Use a small `ResMut<Diagnostics>` only for genuinely cross-cutting state.

### Copying Input Through Mutable State

```rust
// Setup
ctx.insert(WorkingState { input: Some(body), ..Default::default() });

// System
async fn step_a(state: ResMut<WorkingState>) -> A {
    process(state.input.as_ref().unwrap().clone())
}
```

**Fix:** `ctx.insert(body)` directly, read as `Res<RequestBody>`. No `Option`, no clone-through-mutex.

### Using `Out<T>` for Accumulators

```rust
#[system]
async fn add_issue(prev: Out<IssueList>) -> IssueList {
    let mut list = prev.clone();
    list.push(/* ... */);
    list
}
```

In a loop, each iteration's output replaces the previous one, but the systems read/write cost grows linearly. This is also broken when the accumulator spans parallel branches (each branch sees only its parent's output).

**Fix:** `ResMut<IssueList>` is correct for accumulated state. Outputs are for point-to-point handoff, not shared buffers.

## Mapping to Lifetimes

| Data lives… | Use |
|-------------|-----|
| For the lifetime of the server | `GlobalResource` + `Res<T>` |
| For the lifetime of a session | `LocalResource` inserted in `create_session_with` + `Res<T>` or `ResMut<T>` |
| For a single turn | `LocalResource` inserted in `process_turn_with` + `Res<T>` or `ResMut<T>` |
| Between two adjacent systems | Return value + `Out<T>` |
| Inside an error handler subgraph | `ErrOut<CaughtError>` |

## See Also

- [Systems](system.md) — parameter kinds and the `#[system]` macro
- [Execution Context](context.md) — hierarchy, lookup order, scoping
- [Sessions — Recipes](sessions.md#recipes) — injecting per-request input, one-shot execution
- [Graph — Error Handling](graph.md#error-handling) — when `ErrOut<T>` is populated
