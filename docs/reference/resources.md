---
title: Resources
---

# Resources

Resources are how Polaris gives systems their capabilities. An LLM provider, a
tool registry, a memory backend, a request context — each exists as a resource
that systems consume through typed parameters
([`Res<T>`](crate::system::param::Res),
[`ResMut<T>`](crate::system::param::ResMut)). Plugins are the producers;
systems are the consumers.

This page is the canonical reference for the **resource concept** — the trait
hierarchy, the two scopes, and the [Documentation Standard](#documentation-standard)
every exported resource type must follow. For the question *"do I reach for
`Res`, `ResMut`, or `Out`?"*, see [data-flow.md](./data-flow.md) — that's the
parameter-type decision guide and is not duplicated here.

## The Two Scopes

A resource is either **global** (server-wide, one instance shared by every
agent context) or **local** (one fresh instance per agent context). The scope
is fixed at definition time by which trait the type implements; it cannot be
changed at use sites.

```rust
use polaris_system::resource::{GlobalResource, LocalResource};

/// Read-only registry — one instance shared by every system in every session.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry { /* ... */ }
impl GlobalResource for ToolRegistry {}

/// Per-agent scratchpad — every new context gets a fresh instance.
#[derive(Debug, Default)]
pub struct AgentMemory { pub messages: Vec<Message> }
impl LocalResource for AgentMemory {}
```

| Trait | Scope | Lifetime | Mutability from systems | Typical use |
|-------|-------|----------|-------------------------|-------------|
| [`GlobalResource`](crate::system::resource::GlobalResource) | Server | From `insert_global` until shutdown | `Res<T>` only (read-only) | Registries, LLM provider handles, configuration |
| [`LocalResource`](crate::system::resource::LocalResource) | Agent context | Per-context (session, scoped session, or one-shot) | `Res<T>` (read), `ResMut<T>` (exclusive write) | Conversation history, working state, per-turn input |

### Why two scopes (and not one)

The split exists because mutation semantics are scope-dependent. A global
resource must be safe to read from many concurrent contexts, so the framework
disallows `ResMut<GlobalResource>` at compile time. Local resources are owned
by one context at a time, so the framework can hand out exclusive `&mut T`
without coordination. The choice of scope is a contract with the framework
about who shares the value.

## Registering Resources

Globals are inserted by value; locals are registered with a factory closure
that the framework calls each time a new context is created.

```rust
impl Plugin for MyPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_global(ToolRegistry::default());
        server.register_local(AgentMemory::default);  // factory
    }
}
```

A consuming plugin doesn't need to know whether the resource is global or
local — `Res<T>` resolves both. The difference matters when *defining* the
resource (which trait to impl) and when *deciding what data should live there*
(see [data-flow.md](./data-flow.md)).

### Per-turn locals

Local resources can also be inserted into a single agent context at runtime —
typically inside a session setup closure on a specific turn:

```rust
sessions.process_turn_with(session_id, |ctx| {
    ctx.insert(RequestContext::from_headers(&headers));
}).await?;
```

The inserted value is visible to every system in *that* graph execution and
discarded when the context drops. This is the standard way to flow HTTP
request data into a graph without making the resource server-wide.

## Resource Hierarchy and Resolution

`Res<T>` resolves bottom-up: current context's locals → parent context's locals
→ globals. A child can shadow a parent's local resource with its own, and the
shadow lasts only as long as the child context. This is how the executor
hands per-iteration state to a loop body without polluting the surrounding
session.

The full resolution rules — including how `Parallel` nodes fork contexts and
how `Scope` nodes choose between inherit / fresh / hybrid modes — are
documented in [context.md](./context.md#resource-lookup-order).

## Documentation Standard

Every `pub` type implementing [`GlobalResource`] or [`LocalResource`] that is
exported by this workspace **and is intended for downstream consumption** must
include rustdoc covering the sections below. *Intended for downstream
consumption* means: a downstream system would plausibly write
`fn my_system(x: Res<T>)`. Internal-only resources (e.g., a plugin's private
state that no system reads) are exempt and should be marked `#[doc(hidden)]`
or kept non-`pub`.

The catalog drift guard at `tests/resource_catalog.rs` enforces that every
consumer-facing resource appears in the [Resource Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/resources/);
documentation-standard conformance itself is checked by `/review-docs` on
every PR.

### Required Sections

| Section | What it must contain |
|---------|----------------------|
| **Purpose + when-to-use** | What capability the resource exposes from a *system author's* perspective. *"Use this when your system needs to send messages to an LLM"*, not *"wraps an HTTP client"*. The "when" is the load-bearing part — it's what makes the resource discoverable. |
| **Scope + why** | One of `Global` or `Local`, with one sentence justifying the choice. Global resources should explain why they're safe to share across contexts; locals should explain why they need per-context isolation. |
| **Provided by** | The plugin that calls `insert_global` / `register_local` for this type. Include the plugin's feature gate if any. If the resource is consumer-supplied (no plugin registers it by default), say so. |
| **Access pattern** | Which parameter types are valid (`Res<T>` only, or `Res<T>` + `ResMut<T>`), and the mutation contract — *"every push is observed by the next system"*, *"reads see a consistent snapshot for the duration of one system call"*, etc. For globals, also note any interior mutability that bypasses `&T` (`Arc<Mutex<_>>` inside, etc.). |
| **Alternatives** | Related or variant resources that consumers might also be looking for: trait alternatives ([`LlmProvider`](crate::models::LlmProvider) implementations), scope alternatives (a Global registry vs. a per-session Local override), or upstream substitutes (`MockClock` for `Clock` in tests). Empty `_none_` row is fine if there are no alternatives. |
| **Example system** | Rustdoc code block showing a real-shape `#[system]` that consumes the resource. Not a `let _ = ctx.get_resource::<T>()` snippet — an actual parameter-typed system, since that's the consumption pattern systems use. |

### Conditional Sections

| Section | Include when | What it must contain |
|---------|--------------|----------------------|
| **Serialization** | The resource implements [`Storable`](crate::sessions::Storable) and is persisted across session checkpoints | Note that the resource is checkpointed, which plugin registers the serializer (typically [`SessionsPlugin`](crate::sessions::SessionsPlugin) via [`PersistenceAPI`](crate::plugins::PersistenceAPI)), and any non-obvious fields that aren't serialized. |
| **Lifecycle hooks** | The resource's value changes meaning across hook boundaries (e.g., reset at `OnTurnStart`, finalized at `OnGraphComplete`) | Note which schedules transition the resource's state and what the values mean before vs. after. |

### Canonical Exemplars

- [`ServerInfo`](crate::plugins::ServerInfo) — minimal Global: read-only metadata, no mutation, single provider.
- [`Clock`](crate::plugins::Clock) — Global with substitutable implementation: `Clock` is the trait, `WallClock` is the default, `MockClock` is the test alternative. Demonstrates how to document a resource whose alternatives matter.
- [`AgentMemory`](crate::sessions::AgentMemory) (or equivalent session-scoped scratchpad) — Local with `ResMut<T>` mutation contract.
- [`RequestContext`](crate::app::RequestContext) — per-turn Local, inserted at request boundary, demonstrates the runtime-insertion pattern.

### Why this matters

Resources are the surface area downstream systems consume against. When a
consumer writes `fn my_system(x: Res<T>)`, they need to know — without
reading the resource's source — what `T` represents, who's responsible for
putting it there, what mutation looks like, and whether there's a better
choice for their case. A missing resource doc forces them to grep for who
calls `insert_global::<T>` and reverse-engineer the contract.

## Anti-Patterns

**Smuggling system-to-system data through `ResMut<T>`.** If one system produces
a value and the next consumes it, use [`Out<T>`](crate::system::param::Out)
(see [data-flow.md](./data-flow.md)). Reaching for `ResMut` because both
systems "need access to the same state" usually means the state is masquerading
as a resource — it's actually a return value.

**Exposing internal collections as `GlobalResource`.** A plugin's private
working state should not be a public resource. If no downstream system reads
it, keep the type non-`pub` or `#[doc(hidden)]` and mutate it through the
plugin's own machinery. The Resource Catalog and the [Documentation
Standard](#documentation-standard) are for consumer-facing resources only.

**Documenting only the type, not the contract.** A resource's doc should answer
*"what happens when I read it inside a system"*, not just *"what fields does
it have"*. The fields are visible from the struct definition; the contract
(consistency, mutation visibility, hook-driven transitions) is the load-bearing
part of the doc.
