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

The macro validates the signature at expansion and rejects, with an error at the offending token: non-`async` functions; generic functions and `where` clauses (the generated struct carries no generics, so they would be silently discarded); parameter patterns that are not simple identifiers; a parameter named `ctx` (it collides with the context binding the generated body uses); and parameter names under the reserved `__polaris` prefix (they collide with generated locals).

Attributes written on the function are forwarded rather than silently discarded: doc comments land on the generated struct, `cfg`/`cfg_attr` gate every generated item, and lint attributes (`allow`, `expect`, `warn`, `deny`, `forbid`) scope the generated `run` method, which contains the function body. Any other attribute — another attribute macro, `#[inline]` — has no meaningful target after expansion and is rejected at that attribute. The rejection applies to attributes left for `#[system]` itself to handle: another attribute macro still composes when written *above* `#[system]`, since attribute macros expand outside-in and it transforms the original function before `#[system]` sees it.

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

There are four built-in parameter types:

| Type | Resolution Scope | Access | Concurrent Borrows |
|------|------------------|--------|-------------------|
| `Res<T>` | Hierarchy (local → parents → global) | Immutable | Permitted |
| `ResMut<T>` | Current context only | Exclusive | None |
| `Out<T>` | Current context outputs | Immutable | Permitted |
| `ErrOut<T>` | Current context outputs (error-edge only) | Immutable | Permitted |

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

**`ErrOut<T>`** provides immutable access to an error-context output produced when a system fails and the executor routes to an error edge. The executor inserts a `CaughtError` into the destination context before invoking the handler, which reads it via `ErrOut<CaughtError>`.

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

`ErrOut<T>` is populated only on the error path. See [Graph — Error Handling](graph.md#error-handling) for how error edges route to handler subgraphs.

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

### Parameter Inspection

Parameter *values* can be captured for observability. Selection is per system and per parameter, via the macro:

```rust
#[system(inspect(memory, upstream, return))]
async fn plan(
    config: Res<Config>,          // not captured
    mut memory: ResMut<Memory>,   // captured
    upstream: Out<Draft>,         // captured
) -> Plan {
    // ...
}
```

A type becomes renderable by deriving `Debug` — there is nothing to implement, so this works on resource types from crates you do not own. Naming a parameter whose type is not `Debug` is a compile error **at that parameter**, not a silent omission. Bare `#[system(inspect)]` selects every parameter, which requires all of them to be `Debug` — but not the return value, which is not a parameter and must be named explicitly (`inspect(.., return)`). `return` selects the system's own return value: for a fallible system (one whose return type is spelled literally `Result<T, SystemError>` — fallibility detection is syntactic) the recorded value is the extracted success value and a failure records nothing, while a `Result` with any other error type is an ordinary output value, so its `Err` arm is recorded like any other value.

Parameters selected with `inspect(..)` are captured at `Phase::Before`, meaning "this value went in" — those records survive a later fetch failure or a body failure. Only the `return` record implies the system succeeded; a sink must not infer success from the presence of parameter records.

Capture happens at parameter resolution, inside the generated body. That is the only point where a parameter is still statically typed, so there is no downcast and no reflection: a value whose `ResMut` borrow is held is still readable, because the capture *is* that borrow rather than a competing one.

Two axes are deliberately separate:

| Axis | Decided by | Effect |
|------|-----------|--------|
| **Capturability** | `#[system(inspect(..))]` at compile time | Which parameters *can* be recorded; where the `Debug` bound lands |
| **Activation** | `InspectionPolicy` at run time, via `InspectionAPI` (or raw `SystemContext::replace_inspection()` without the plugin) | Whether capture is live, without a rebuild |

An un-annotated system emits no capture at all and imposes no bound on its parameters. With no sink installed, an annotated system pays a single `Option` check and never formats a value; a sink receives the value as a closure, so declining a record costs no formatting either. Child contexts inherit the sink, so installing it once at the root covers every scope, branch, and loop iteration beneath it. A sink runs synchronously on the system's execution path: it must not block, and a panic it raises fails the system being observed — only panics inside the value's `Debug` impl are absorbed.

One sink is installed at a time, so a second `replace_inspection()` replaces the first. The replaced sink is *returned* rather than dropped silently, and `inspection_arc()` hands back an owned `Arc` (unlike `inspection()`, which is a borrow). Together those let a plugin chain onto a sink a caller already installed instead of cutting it off. Chaining is not idempotent — re-applying it to a context that already carries the wrapper grows the chain a link at a time — so a consumer that may run more than once against the same context must guard on sink identity. A chained sink also does not inherit the wrapped sink's policy — it renders values the other sink would have withheld — and a panic in one branch starves the branches behind it. Both argue for delivering to a single installed sink that fans out flatly over wrapping sinks around one another; the framework bounds none of it, so the guard is the caller's. See [Execution Context — Chaining onto an Installed Sink](./context.md#chaining-onto-an-installed-sink).

Rendering treats the `Debug` impl as untrusted code: formatting **stops** at a byte cap rather than materializing the full value (a multi-megabyte history costs at most the cap), and a `Debug` impl that panics is absorbed at the render boundary and recorded as `Opaque` — a buggy formatter cannot fail the system it observes. Records carry the inner resource type (`Memory`, not `ResMut<'_, Memory>`) as declared at the parameter, so they group by resource across wrapper and lifetime spellings — but not across path spellings: `Res<Deep>` and `Res<nested::Deep>` record different names for the same resource.

Captured values render through the type's **own** `Debug` impl, so the standard redaction idiom composes: a hand-written `Debug` that masks a secret field is honored by the capture path (note the flip side: a derived `Debug` escapes string contents via `escape_debug` where a hand-written one need not, so a hand-written impl can put raw control characters into the *rendering* — the shipped tracing listener then escapes the whole rendering again when it emits it, so neither can forge log structure there, but a listener that writes the rendering verbatim gets no such protection). Three gates guard sensitive data: selection (do not name a parameter in `inspect(..)` whose derived `Debug` would expose credentials); the runtime policy, which is off by default — enabling it exposes selected renderings to every registered listener; and, in the plugin layer, runtime `RedactionRules` — a covered value is delivered as `Redacted` without its `Debug` ever running, so the value never reaches a `String`. Rule matching is deliberately generous, because a withholding control that under-matches leaks: `redact_param` matches the binding name exactly, while `redact_type` strips paths on both sides *and* descends into the recorded spelling, so one rule on `ApiCredentials` covers `credentials::ApiCredentials`, `Option<ApiCredentials>`, `Vec<ApiCredentials>`, and `Box<dyn ApiCredentials>` alike — whitespace between two identifiers is preserved rather than dropped precisely so the last of those cannot glue into `dynApiCredentials` and escape the rule. Name the sensitive type itself rather than a container spelling; a rule that names a container matches that spelling whole and so would miss the same container nested one level deeper. A type **alias** records the alias, which no rule on the underlying name can see — mask in the type's own `Debug` impl when a value must never render anywhere.

Layer 1 supplies only this mechanism — the `Inspection` rendering, the `InspectionSink` trait, and the capture. Recording policy, listener registration, and export to telemetry live in the plugin layer: `InspectionPlugin` (`polaris_core_plugins`) installs a fan-out sink on every graph run, gates delivery through the runtime `InspectionPolicy` (off by default; on, or narrowed to named systems, via `InspectionAPI`), withholds `RedactionRules`-covered values (set at build with `InspectionPlugin::with_redactions`, added at run time with the bounded `InspectionAPI::add_redacted_param` / `add_redacted_type` operations so two holders cannot un-redact one another), lets any plugin sign up a listener through `Extends<InspectionSinkRegistry>` (optionally under a static `InspectionListenerName`, so `InspectionAPI` can discover and switch it off and on at run time), and ships a tracing listener registered under the name `INSPECTION_TRACING_LISTENER` that lands records on the per-step span. Unknown toggles return `None` without allocating registry state. The framework stores no records — listeners bring their own storage.

The two activation routes do not compose: the plugin installs its fan-out at the start of every graph run, so it replaces a sink placed on the context by `replace_inspection()` rather than chaining onto it. Pick one — register a listener, or drop the plugin and keep the manual sink. The first run that displaces a *foreign* sink logs a warning on the `polaris::inspection` target (the plugin recognizes its own fan-out by identity, so a context reused across turns is not reported as a displacement), which is also the target the plugin's records use and the spelling of the shipped listener's typed identity. `RUST_LOG="info,polaris::inspection=off"` suppresses rendered values without turning recording off for other listeners, and because the listener consults the filter before rendering, a filtered-out target means the value is never formatted for it at all; `InspectionAPI::disable_listener(INSPECTION_TRACING_LISTENER)` stops the shipped listener the same way with no environment change.

Two properties of the exported event are worth knowing when reading it back. The value is emitted through `Debug`, so it arrives quoted and escaped and a rendering containing newlines cannot forge extra log structure. And a separate `polaris.inspection.rendering` field says which `Inspection` variant produced it (`text`, `redacted`, `opaque`, `unrenderable`) — key alerting on that rather than on the text, because a value whose `Debug` writes the literal `<redacted>` is otherwise indistinguishable from the sentinel.

Enabling is **process-wide**. The policy has no session, run, or tenant axis and one fan-out serves every concurrent run, so turning recording on to chase one session records every other session executing at the same time and ships all of it to every listener. Narrow with `enable_only`, put `RedactionRules` in place before enabling rather than after, and keep the window short.

Two limitations are worth stating plainly: a hand-written `impl System` gets no capture, since the mechanism lives in the macro; and `ResMut` records the value going *in*, not the mutated result.

## ContextFactory

`ContextFactory` is a clonable handle that creates fresh `SystemContext` instances outside of direct `Server` access. It captures the server's global resources and local resource factories, enabling context creation from HTTP handlers, background tasks, or any code that does not hold a `&Server` reference.

```rust
let factory = server.context_factory();

// Move factory to another thread, create contexts freely
let ctx = factory.create_context();
```

Each call to `create_context()` produces a `SystemContext<'static>` with a shared reference to global resources and fresh instances of all registered local resources.

### Deferred Binding

`ContextFactory` uses deferred binding when created during the plugin `ready()` phase. This is necessary because `Server::insert_global()` requires exclusive access to the global resource `Arc` (via `Arc::get_mut`). A direct `Arc::clone` during `ready()` would bump the reference count and prevent any downstream plugin from registering global resources.

When `context_factory()` is called during `ready()`, the factory stores a deferred handle instead of a direct `Arc` reference. The handle is resolved at the end of `Server::finish()`, after all plugins have completed their `ready()` phase and all global resources are registered. Calling `create_context()` before `finish()` completes will panic.

Outside of the `ready()` phase (before `finish()` starts or after it completes), `context_factory()` returns a direct reference with no deferred resolution.

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
