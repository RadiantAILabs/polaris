---
notion_page: https://www.notion.so/radiant-ai/Execution-Context-342afe2e695d80eebe23e03089a6f976
title: Execution Context
---

# Execution Context

`SystemContext` is the execution context that flows through every system, graph node, and session in Polaris. It holds resources, outputs, and an optional parent chain — everything a system needs to resolve its parameters.

## Structure

```rust
pub struct SystemContext<'parent> {
    parent:    Option<&'parent SystemContext<'parent>>,
    globals:   Option<Arc<Resources>>,
    resources: Resources,
    outputs:   Outputs,
}
```

| Field | Purpose |
|-------|---------|
| `parent` | Read-only reference to a parent context (hierarchy chain) |
| `globals` | Server-level global resources (`Arc`-shared) |
| `resources` | Local resources owned by this scope |
| `outputs` | Ephemeral return values from preceding systems |

## Context Hierarchy

Contexts form a parent-child tree. A child can read its parent's resources but cannot mutate them. Globals are shared across all levels via `Arc`.

```text
Server (globals: Config, ToolRegistry, ModelRegistry)
   │
   └── Agent Context (locals: AgentConfig)
          │
          └── Session Context (locals: ConversationHistory)
                 │
                 └── Turn Context (locals: Scratchpad, UserIO)
```

Root contexts (`SystemContext<'static>`) have no parent and keep globals alive via `Arc` reference counting. They can outlive the server.

## Resource Lookup Order

When a system declares `Res<T>`, the context searches:

1. **Local resources** owned by this context
2. **Parent chain** — walks upward, closest scope shadows
3. **Global resources** — server-level shared state

`ResMut<T>` skips the hierarchy entirely — it only accesses resources in the current scope. This is enforced at compile time: `ResMut<T>` requires `T: LocalResource`.

```rust
// Res<T>: walks hierarchy (local → parent → globals)
pub fn get_resource<R: Resource>(&self) -> Result<ResourceRef<R>, ParamError> {
    // 1. Check local
    // 2. Walk parent chain
    // 3. Check globals
}

// ResMut<T>: current scope only
pub fn get_resource_mut<R: Resource>(&self) -> Result<ResourceRefMut<R>, ParamError> {
    self.resources.get_mut::<R>()  // local scope only
}
```

### Shadowing

A child context can shadow a parent's resource. If both a parent and child have a resource of type `T`, `Res<T>` resolves to the child's copy.

## Global vs Local Resources

| Property | `GlobalResource` | `LocalResource` |
|----------|-------------------|-----------------|
| Registered via | `server.insert_global(T)` | `server.register_local(\|\| T)` |
| Access | `Res<T>` (read-only) | `Res<T>` or `ResMut<T>` |
| Lifetime | Server lifetime | Per-context (fresh from factory) |
| Storage | `Arc<Resources>` (shared) | `Resources` (owned per context) |
| Mutation | Compile-time rejected | Allowed via `ResMut<T>` |

### Registration

```rust
// Global: shared across all contexts, read-only
server.insert_global(Config { max_tokens: 2048 });

// Local: factory produces fresh instance per context
server.register_local(Memory::default);
```

`insert_global()` panics if contexts have already been created — because globals are stored in an `Arc`, and `Arc::get_mut` requires exclusive ownership.

### Borrow Rules

Resources are protected by `RwLock` within the `Resources` container:

- **Read + Read** — compatible (multiple `Res<T>` allowed)
- **Read + Write** — conflict (`Res<T>` and `ResMut<T>` to the same `T`)
- **Write + Write** — conflict (two `ResMut<T>` to the same `T`)

Conflicts return `ParamError::BorrowConflict`. RAII guards release locks on drop.

## Creating Contexts

### From the Server

```rust
// After server.finish(), creates a context with globals + fresh locals
let ctx = server.create_context();
```

`create_context()` produces a `SystemContext<'static>` by:
1. Cloning the `Arc<Resources>` for globals
2. Invoking each registered local factory to create fresh resource instances
3. Inserting the type-erased local resources via `insert_boxed()`

### Via ContextFactory

`ContextFactory` is a clonable handle for creating contexts outside of direct `Server` access — from HTTP handlers, background tasks, or any code without `&Server`.

```rust
// Obtain during plugin ready() phase
let factory = server.context_factory();

// Later, from anywhere:
let ctx = factory.create_context();
```

#### Deferred Binding

When `context_factory()` is called during the `ready()` phase, the factory stores a deferred handle (`Arc<OnceLock<Arc<Resources>>>`) instead of a direct `Arc` clone. This is necessary because `insert_global()` requires `Arc::get_mut` — a direct clone during `ready()` would bump the reference count and prevent downstream plugins from registering globals.

The deferred handle is resolved at the end of `Server::finish()`, after all plugins complete `ready()`. Calling `create_context()` before `finish()` completes will panic.

Outside of `ready()`, `context_factory()` returns a direct reference.

### For Testing

```rust
// Empty context (no globals, no parents)
let ctx = SystemContext::new();

// Builder pattern with local resources
let ctx = SystemContext::new()
    .with(Counter { value: 0 })
    .with(Memory::default());

// Child context
let child = ctx.child();
```

## Context Flow Through Graph Execution

The `GraphExecutor` receives `&mut SystemContext` and passes it through each node. Different node types have different context semantics:

### System, Decision, Switch

These nodes execute in the **parent's context** directly. No child context is created.

```text
ctx ──→ [System A] ──→ [Decision] ──→ [System B] ──→ ...
         │                 │               │
         └── same ctx ─────┘───────────────┘
```

### Parallel

Each branch gets its own **child context** via `ctx.child()`. Branches run concurrently with isolated writes but shared reads (via parent chain). After all branches complete, outputs are merged back into the parent in branch order (last-write-wins for duplicate types).

```text
ctx ──→ [Parallel]
           ├── child_0 ──→ [Branch A] ──→ merge outputs back
           └── child_1 ──→ [Branch B] ──→ merge outputs back
```

### Loop

The loop body executes in the **same context** across iterations. Outputs from iteration N are available to iteration N+1. The context persists until the loop completes.

### Scope

Scope nodes have configurable context isolation via `ContextPolicy`, constructed upfront and passed to `add_scope`:

| Policy | Context | Reads | Writes | Output Merge |
|--------|---------|-------|--------|--------------|
| `ContextPolicy::shared()` | Same as parent | Parent's resources | Parent's resources | Shared (no merge needed) |
| `ContextPolicy::inherit()` | `ctx.child()` | Walk parent chain + globals | Child's local scope | Merged back to parent |
| `ContextPolicy::isolated()` | `SystemContext::with_globals(arc)` | Only globals + forwarded | Own local scope | Merged back to parent |

**Resource forwarding**: `ContextPolicy::inherit()` and `::isolated()` can forward specific resources from parent to child via `.forward::<T>()`. The resource must implement `Clone`; the clone function is captured at policy-build time.

```rust
let policy = ContextPolicy::isolated().forward::<Memory>();
graph.add_scope("sub_agent", inner_graph, policy);
```

## Outputs

System return values are stored in the context's output storage, keyed by `TypeId`. Downstream systems access them via `Out<T>`.

```rust
#[system]
async fn reason() -> ReasoningResult { /* ... */ }

#[system]
async fn act(reasoning: Out<ReasoningResult>) -> ActionResult { /* ... */ }
```

- If multiple systems return the same type, last-write-wins
- Outputs persist for the duration of graph execution
- `ErrOut<T>` reads error context from a failed system (via error edges)
- Outputs are cleared between agent runs via `ctx.clear_outputs()`

### Output Merging

When child contexts (from Parallel or Scope nodes) complete, their outputs are merged into the parent via `ctx.outputs_mut().merge_from(child_outputs)`. Merge is deterministic — branches are processed in order, so the last branch's output wins for duplicate types.

## Resource Validation

Before execution, `GraphExecutor::validate_resources()` checks that all resources and outputs required by systems exist or can be produced:

- `Res<T>` — checked against the full hierarchy (local + parents + globals)
- `ResMut<T>` — checked against local scope only
- Hook-provided resources (`OnGraphStart`, `OnSystemStart`) are considered available
- `Out<T>` — validated along the linear (sequential) chain. Each system's declared output dependencies are checked against outputs produced by preceding systems. Non-system nodes (Decision, Switch, Loop, Parallel) contribute all output types reachable from their subgraphs. Scope nodes are skipped (outputs flow differently across scope boundaries). Conditional and parallel branches are not individually validated because their execution paths are dynamic.
- Scope nodes are validated recursively with synthetic child contexts matching runtime behavior

```rust
executor.validate_resources(&graph, &ctx, Some(&hooks))?;
```

## Key Files

| File | Purpose |
|------|---------|
| `polaris_system/src/param/mod.rs` | `SystemContext` struct, `Res<T>`, `ResMut<T>`, `Out<T>`, `ErrOut<T>` |
| `polaris_system/src/resource/resource.rs` | `Resources` container, `GlobalResource`, `LocalResource`, RAII guards |
| `polaris_system/src/server.rs` | `Server::create_context()`, `ContextFactory`, deferred binding |
| `polaris_graph/src/executor/mod.rs` | `validate_resources()`, scope validation |
| `polaris_graph/src/executor/run.rs` | Per-node context management (parallel children, scope modes, output merging) |
| `polaris_graph/src/node.rs` | `ContextMode`, `ContextPolicy`, `ResourceForward` |
