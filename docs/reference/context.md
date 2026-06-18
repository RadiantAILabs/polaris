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

Scope nodes have configurable context isolation via `ContextPolicy`, composed by chaining per-resource verbs onto `ContextPolicy::new()`. Two constructors anchor the surface:

| Constructor | Meaning |
|---|---|
| `ContextPolicy::shared()` | No boundary at all — the inner graph reuses the parent context. |
| `ContextPolicy::new()` | Empty per-resource policy — nothing crosses unless added. |

The policy is then composed from per-resource verbs:

| Verb | Mechanism | Requires of `T` | Use when |
|---|---|---|---|
| `share::<T>()` | Child reads via parent chain (`Res<T>` walks up); zero copy | nothing (any `LocalResource`) | Read-only access; large or expensive-to-clone resources |
| `forward::<T>()` | `Clone::clone` into child's local scope | `T: Clone` | Small mutable resource; child needs its own copy |
| `fork::<T>()` | `ForkStrategy::fork(&self)` into child's local scope | `T: ForkStrategy` | Stateful resource with non-`Clone` semantics (snapshot, fresh-empty, `Arc`-shared) |
| `forward_fresh::<T>()` | Re-invoke `T`'s registered factory | `T` registered via `Server::register_local(...)` | Resource that should start clean (counters, scratchpads, budgets) |
| `exclude::<T>()` | Suppresses any earlier verb / `share_rest()` for `T` | nothing | Combine with `share_rest()` to opt one resource out of the catch-all |
| `share_rest()` | Apply `share` to every resource not otherwise mentioned | nothing | "Mostly inherit, with a few overrides" pattern |

Verbs are applied in declaration order; later verbs override earlier ones for the same `T`. `share_rest()` only applies to types not otherwise named.

At runtime the executor branches on the policy:

| Policy shape | Context | Reads | Writes | Output Merge |
|---|---|---|---|---|
| `ContextPolicy::shared()` | Same as parent | Parent's resources | Parent's resources | Shared (no merge needed) |
| Any `share` verb / `share_rest()` | `ctx.child_filtered(parent_filter)` | Globals + parent chain (filtered by `share` / `share_rest` / `exclude`) + child locals | Child's local scope | Merged back to parent |
| Pure isolation (only `forward` / `fork` / `forward_fresh`) | `ctx.child_filtered(AllowOnly(empty))` | Globals + forwarded/forked/fresh locals | Child's local scope | Merged back to parent |

Every non-`shared()` policy builds the child via `child_filtered` and retains the (filtered) parent reference. For pure isolation the filter is an empty `AllowOnly`, so no parent local is readable — but keeping the reference lets a blocked read return `ParamError::ResourceOutOfScope` (naming the verb that would expose it) instead of an indistinct "not found". Globals still flow through the retained parent.

```rust
// Sub-agent: shared registry, fresh fragment store.
let policy = ContextPolicy::new()
    .share::<ToolRegistry>()
    .forward_fresh::<FragmentStore>();
graph.add_scope("sub_agent", inner_graph, policy);

// "Mostly inherit, but override one resource."
let policy = ContextPolicy::new()
    .fork::<FragmentStore>()
    .share_rest();
graph.add_scope("scope", inner_graph, policy);
```

### ParentFilter

`ParentFilter` is the Layer 1 primitive that backs the scope boundary at runtime. It is an opaque type in `polaris_system::param` with two construction modes:

- `ParentFilter::allow_all_except([TypeId, ...])` — used for the `share_rest()` case: parent-chain reads are allowed *except* for the listed type ids.
- `ParentFilter::allow_only([TypeId, ...])` — used for explicit-share policies: parent-chain reads are allowed *only* for the listed type ids.

`SystemContext::child_filtered(filter)` builds a child context whose parent-chain reads are gated by the filter. Globals remain reachable regardless of the filter — only locally-scoped resources walked through the parent chain are affected.

```rust
use std::any::TypeId;
use polaris_system::param::{ParentFilter, SystemContext};

let filter = ParentFilter::allow_all_except([TypeId::of::<Secret>()]);
let child = parent.child_filtered(filter);
// `Res<Secret>` walked from `child` will not see the parent's `Secret`,
// but globals and unfiltered locals remain visible.
```

Application code rarely constructs `ParentFilter` directly. `ContextPolicy` builds the appropriate filter internally via `policy.parent_filter()`, and the executor invokes `child_filtered` when entering a scope. The filter is the mechanism that translates a `share` / `share_rest` / `exclude` declaration into runtime read-gating.

Source: `polaris_system/src/param/mod.rs` — `ParentFilter`, `child_filtered`.

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
| `polaris_graph/src/node.rs` | `ContextPolicy`, `ContextMode` (high-level summary), `ResourceCrossing` |
| `polaris_system/src/resource/resource.rs` | `ForkStrategy` trait |
| `polaris_system/src/param/mod.rs` | `ParentFilter`, `SystemContext::child_filtered` |
