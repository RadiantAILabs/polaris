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

Children also inherit the parameter-inspection sink ([System — Parameter Inspection](./system.md#parameter-inspection)): both `child()` and `child_filtered()` carry it, so a sink installed at the root covers every scope, branch, and loop iteration beneath it — scope filtering isolates resources, never observability.

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

## Writes Return What They Displaced

Every context write returns the value it overwrote, matching `HashMap::insert`:

| Write | Returns |
|-------|---------|
| `insert::<R>()` / `insert_resource::<R>()` | `Option<R>` |
| `insert_boxed()` / `insert_boxed_with_factory()` | `Option<Box<dyn Any + Send + Sync>>` |
| `insert_output::<O>()` | `Option<O>` |
| `insert_output_boxed()` | `Option<Box<dyn Any + Send + Sync>>` |
| `replace_inspection()` | `Option<Arc<dyn InspectionSink>>` |
| `with_inspection()` | `Self` — **discards** the displaced sink |

`Some` means the write clobbered a value the caller may not have known was there — two independent `ctx.insert(Memory { .. })` calls are otherwise indistinguishable from one. The scope is *this* context only: shadowing a parent's resource returns `None`, since nothing in this scope was displaced. (`replace_inspection()` is the one row where that reads differently — see below.)

`None` is not quite a *guarantee* that nothing was displaced. One pathological case returns `None` after dropping a value: a slot occupied under `T`'s type ID but holding some other type, which is only reachable once an earlier `insert_boxed()` has already violated its own type-correctness contract. Nothing well-behaved reaches it, but `Some` is the load-bearing half of the signal — treat `None` as "nothing this scope knew about" rather than "nothing at all".

The type-erased forms return the displaced value still boxed, because the concrete type is not known at that boundary; downcast it if you need the value. Only the value comes back: `insert_boxed_with_factory()` drops the displaced entry's factory and clone function rather than returning them. Neither is recoverable afterwards, so capture what you need *before* writing — `factory_fn_by_type_id()` hands out a clone of the factory still in force.

Ignoring the return is fine and is what most call sites do — the `insert*` family is not `#[must_use]`, because the executor overwrites outputs by design. The point is that an overwrite is *observable* when a caller cares. `replace_inspection()` is the exception and *is* `#[must_use]`: the sink it displaces belongs to whoever installed it, so dropping that handle cuts them off silently — bind it to `_` if losing it is genuinely what you mean. One write is deliberately outside the contract: `Outputs::merge_from()`, the parallel-branch join, reports nothing, because there overwriting is the defined outcome rather than an accident.

`with_inspection()` is the one write that cannot report: it returns `Self`, so the builder chain has nowhere to hand the displaced sink back and drops it silently. On a context you just created that is exactly right. On one that may already carry a sink — including any `child()`, which inherits the handle by value (below) — use `inspection_arc()` and `replace_inspection()` instead.

### `replace_inspection()` is scoped differently

The "this context only" rule reads off the *hierarchy*, and the sink does not live in the hierarchy. Resources are inherited by reference — a child walks up to the parent — so a child insert shadows and returns `None`. The inspection sink is inherited **by value**: `child()` clones the handle into the child's own slot. Replacing it on a child therefore *does* displace something, and returns the inherited sink:

```rust
let mut child = parent.child();
child.insert(Memory { .. });   // -> None                 (parent's copy is shadowed)
let _ = child.replace_inspection(my_sink); // -> Some(parent_sink)   (inherited by value)
```

The parent keeps its own sink either way; the child's replacement is local to the child.

### Chaining onto an Installed Sink

`replace_inspection()` is the case where the displaced return matters most, because it makes composition possible. Paired with `inspection_arc()`, which hands back an owned `Arc` rather than the borrow `inspection()` returns, a plugin can chain onto whatever the caller installed instead of cutting it off:

```rust
// Forward every record to both sinks rather than replacing the caller's.
// `replace_inspection` is `#[must_use]`; here the displaced sink is the one
// being chained onto, so the return is deliberately discarded.
if let Some(existing) = ctx.inspection_arc() {
    let _ = ctx.replace_inspection(Arc::new(Tee(existing, my_sink)));
} else {
    let _ = ctx.replace_inspection(my_sink);
}
```

**Applying this more than once to the same context requires an identity guard.** The recipe is not idempotent: each application wraps whatever is installed in a *new* `Tee`, so a consumer that runs it per graph execution against a context reused across turns grows the chain by one link per run — deepening `record()` recursion without bound and delivering one duplicate record per link. Testing `inspection_arc().is_some()` does not help, and neither does comparing against your own sink, because the wrapper hides it. Keep the handle you installed and skip when it is still the one in force:

```rust
// `installed_by_us: Option<Arc<dyn InspectionSink>>` persists across applications.
let live = ctx.inspection_arc();
let ours_still_in_force = match (&installed_by_us, &live) {
    (Some(mine), Some(live)) => Arc::ptr_eq(mine, live),
    _ => false,
};
if !ours_still_in_force {
    // `my_sink` is cloned, not moved: the guard runs on every application.
    let wrapper: Arc<dyn InspectionSink> = match live {
        Some(existing) => Arc::new(Tee(existing, Arc::clone(&my_sink))),
        None => Arc::clone(&my_sink),
    };
    let _ = ctx.replace_inspection(Arc::clone(&wrapper));
    installed_by_us = Some(wrapper);
}
```

If someone else replaced the sink in between, the guard correctly falls through and re-chains onto the new one.

### What Chaining Does Not Give You

Chaining is the escape hatch, not the default way to add a sink. Delivering to a single installed sink that fans out to N listeners is flat rather than nested: there is no `record()` recursion to deepen, no chain to re-apply, and no identity guard for any consumer to get wrong. Where something above this layer already offers that fan-out, use it in preference to wrapping.

Two properties a chained sink does **not** inherit from the sink it wraps:

- **Policy does not reach it.** A sink that filters, masks, or withholds does so *inside its own* `record()`. A sink chained beside it receives the un-rendered closure straight from the capture site, so it will render values the wrapped sink would have withheld. Chain a sink only when you own the values it will see; a sink that enforces policy belongs at the head of its own delivery path, never beside one.
- **Failure is not isolated.** `record()` runs synchronously on the observed system's path, and a wrapper that forwards in order stops forwarding once a branch panics. A panicking sink therefore fails the observed system *and* starves every sink behind it in the chain — so a third-party sink can silence the telemetry it was chained onto.

Neither hazard is bounded by the framework: nothing checks chain depth, deduplicates a re-applied wrapper, or isolates a branch. The identity guard above is the consumer's to write, which is the strongest argument for delivering to one sink that fans out flatly over wrapping sinks around one another.

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

Each branch gets its own **child context** via `ctx.child()` and runs concurrently with isolated local writes. `Res<T>` reads walk the parent chain, but **outputs do not**: the child starts with an empty output store, so a branch cannot read an `Out<T>` produced upstream of the parallel node. Put values needed across the fork in a resource on the parent context and read them in each branch with `Res<T>`. After all branches complete successfully, their outputs are merged back into the parent in declaration order; at run time, duplicate types use the last branch's value.

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

### Dynamic

A Dynamic node runs its **selected candidate** through the same boundary machinery as Scope — both node types share one executor path (`execute_embedded`), so every policy shape, verb, and runtime behavior in the Scope table above applies verbatim, with the candidate graph in place of the embedded graph. Two Dynamic-specific points:

- **Selection happens in the parent context.** The selector reads the parent's resources and outputs to choose a candidate key *before* any child context exists; only the chosen candidate then runs through the `ContextPolicy` boundary declared on the node's `DynamicSlot`.
- **Free outputs never cross a non-shared boundary inward.** A slot contract with a nonempty `requires_outputs` under a non-shared policy is rejected at `validate_resources` time (`DynamicContractOutputsBlocked`), because child contexts start with empty outputs and never walk the parent chain for them.

See [Graph — Dynamic](graph.md#dynamic) for selection, contracts, and candidate sources.

## Outputs

System return values are stored in the context's output storage, keyed by `TypeId`. Downstream systems access them via `Out<T>`.

```rust
#[system]
async fn reason() -> ReasoningResult { /* ... */ }

#[system]
async fn act(reasoning: Out<ReasoningResult>) -> ActionResult { /* ... */ }
```

- If multiple systems return the same type, last-write-wins — the write returns the value it displaced, so the clobber is observable (see [Writes Return What They Displaced](#writes-return-what-they-displaced))
- Outputs persist for the duration of graph execution
- `ErrOut<T>` reads error context from a failed system (via error edges)
- Outputs are cleared between agent runs via `ctx.clear_outputs()`

### Output Merging

When child contexts (from Parallel, Scope, or Dynamic nodes) complete, their outputs are merged into the parent via `ctx.outputs_mut().merge_from(child_outputs)`. A scope or dynamic `ContextPolicy` controls which resources enter the child; it does not filter outputs returning to the parent. Merge is deterministic — branches are processed in order, so the last branch's output wins for duplicate types.

## Resource Validation

Before execution, `GraphExecutor::validate_resources()` checks that all resources and outputs required by systems exist or can be produced:

- `Res<T>` — checked against the full hierarchy (local + parents + globals)
- `ResMut<T>` — checked against local scope only
- Hook-provided resources (`OnGraphStart`, `OnSystemStart`) are considered available
- `Out<T>` — validated along the linear (sequential) chain. Each system's declared output dependencies are checked against outputs produced by preceding nodes plus outputs already present in the context (callers may seed via `insert_output`), and each loop's termination predicate input is checked the same way (the predicate is evaluated before the first iteration, so the loop body cannot satisfy it). Hook-provided resource types never count: provider hooks insert resources, while `Out<T>`, predicates, and dynamic `requires_outputs` read the separate output channel. The pre-flight/run-start model is optimistic: Decision, Switch, Loop, and Parallel contribute every output type their subgraphs may deposit, including outputs behind nested Scope boundaries, handler paths, and Dynamic contracts. This does not change pessimistic composition signatures, where conditional production is not guaranteed and therefore cannot erase a free-output requirement. Shared-boundary embedded graphs (scope graphs, inline dynamic candidates) are validated recursively against the outputs available at their chain position; non-shared ones against an empty output set, since outputs never cross a non-shared boundary inward. Conditional and parallel branch interiors are not individually validated because their execution paths are dynamic.
- Scope nodes are validated recursively with synthetic child contexts matching runtime behavior

```rust
executor.validate_resources(&graph, &ctx, Some(&hooks))?;
```

This pass is run-start analysis performed ahead of time, and it credits no more than the executor's mandatory run-start checks do — a graph that passes `validate_resources` is never refused at run start by the same analysis. See [Graph — Verification Phases](graph.md#verification-phases) for the phase rules that bind it.

## Key Files

| File | Purpose |
|------|---------|
| `polaris_system/src/param/mod.rs` | `SystemContext` struct, `Res<T>`, `ResMut<T>`, `Out<T>`, `ErrOut<T>` |
| `polaris_system/src/resource/resource.rs` | `Resources` container, `GlobalResource`, `LocalResource`, RAII guards |
| `polaris_system/src/resource/output.rs` | `Outputs` container, output merging |
| `polaris_system/src/server.rs` | `Server::create_context()`, `ContextFactory`, deferred binding |
| `polaris_graph/src/executor/mod.rs` | `validate_resources()`, scope validation |
| `polaris_graph/src/executor/run.rs` | Per-node context management (parallel children, scope modes, output merging) |
| `polaris_graph/src/node.rs` | `ContextPolicy`, `ContextMode` (high-level summary), `ResourceCrossing` |
| `polaris_system/src/resource/resource.rs` | `ForkStrategy` trait |
| `polaris_system/src/param/mod.rs` | `ParentFilter`, `SystemContext::child_filtered` |
