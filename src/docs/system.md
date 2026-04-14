ECS-inspired systems, resources, plugins, and the server runtime.

This module provides Polaris's foundational primitives (Layer 1). All other
crates build on these abstractions.

# Systems

A **system** is a pure async function that declares its dependencies as typed
parameters. The `#[system]` macro generates the
`System` trait implementation:

```no_run
# use polaris_ai::polaris_system;
use polaris_ai::system::system;
use polaris_ai::system::param::Res;
use polaris_ai::system::resource::LocalResource;
# #[derive(Clone)] struct LlmClient;
# impl LocalResource for LlmClient {}
# #[derive(Clone)] struct Memory;
# impl LocalResource for Memory {}
# struct ReasoningResult { action: String }

#[system]
async fn reason(llm: Res<LlmClient>, memory: Res<Memory>) -> ReasoningResult {
    ReasoningResult { action: "search".into() }
}
```

Systems that may fail return `Result<T, SystemError>`. The macro sets
`is_fallible()` to `true` automatically, enabling error-edge wiring in the
graph. See [Graph error handling](crate::graph) for details.

Zero-parameter async functions implement `IntoSystem` via a blanket impl
and do not need the macro.

# Parameters

| Type | Resolution | Access | Use for |
|------|------------|--------|---------|
| [`Res<T>`](crate::system::param::Res) | Hierarchy (local -> parent -> global) | Immutable | Config, registries, per-request input |
| [`ResMut<T>`](crate::system::param::ResMut) | Current context only | Exclusive | Accumulated state (history, counters) |
| [`Out<T>`](crate::system::param::Out) | Previous system output | Immutable | System-to-system data handoff |
| [`ErrOut<T>`](crate::system::param::ErrOut) | Error-edge output | Immutable | Error context in handler subgraphs |

See [Data flow patterns](crate#data-flow-patterns) for a decision guide.

# `SystemContext`

[`SystemContext`](crate::system::param::SystemContext) is the execution context flowing
through every system. It holds resources, outputs, and an optional parent
reference forming a hierarchy:

```text
Server (globals: Config, ToolRegistry, ModelRegistry)
   |
   +-- Agent Context (locals: AgentConfig)
          |
          +-- Session Context (locals: ConversationHistory)
                 |
                 +-- Turn Context (locals: Scratchpad, UserIO)
```

**Resource lookup order** for `Res<T>`: local resources -> parent chain
(closest scope shadows) -> global resources.

`ResMut<T>` skips the hierarchy entirely and only accesses the current
context. `T` must implement `LocalResource`.

# Resources

| Scope | Registered via | Access | Lifetime | Mutation |
|-------|---------------|--------|----------|----------|
| [`GlobalResource`](crate::system::resource::GlobalResource) | `server.insert_global(T)` | `Res<T>` only | Server lifetime | Compile-time rejected |
| [`LocalResource`](crate::system::resource::LocalResource) | `server.register_local(\|\| T)` | `Res<T>` or `ResMut<T>` | Per-context | Allowed via `ResMut<T>` |

# Plugins

The [`Plugin`](crate::system::plugin::Plugin) trait is the unit of composition. Every
capability is delivered through plugins with a strict lifecycle:

1. **`build()`** -- register resources, APIs, routes (dependency order)
2. **`ready()`** -- resolve APIs, bind deferred state (dependency order)
3. **`update()`** -- respond to schedule ticks (dependency order)
4. **`cleanup()`** -- release resources (reverse dependency order)

Dependencies are declared via `dependencies()` and topologically sorted.
Missing or circular dependencies panic at startup.

# `ContextFactory`

[`ContextFactory`](crate::system::server::ContextFactory) creates `SystemContext` instances
outside of direct `Server` access -- from HTTP handlers, background tasks,
or any code without `&Server`. When obtained during `ready()`, it uses
deferred binding (`Arc<OnceLock>`) that resolves at the end of
`Server::finish()`.

# Related

- [Graph execution](crate::graph) -- how systems are composed into agent behavior
- [Data flow patterns](crate#data-flow-patterns) -- choosing `Res` vs `ResMut` vs `Out`
- [Sessions](crate::sessions) -- session-managed context creation and turn execution
