---
notion_page: https://www.notion.so/radiant-ai/Plugins-327afe2e695d80cdaa48fa1cb9c63f67
title: Plugin System
---

# Plugin System

Plugins are the fundamental unit of composition in Polaris. Every piece of functionality, from core infrastructure like logging and tracing to agent-specific features like tools and memory, is delivered through plugins. This makes the framework extensible while keeping the core minimal.

## Plugin Trait

A plugin is any type that implements the `Plugin` trait. The trait has one required method (`build`) and several optional lifecycle hooks.

```rust
pub trait Plugin: Send + Sync + 'static {
    /// Configures the server. Called once when the plugin is added.
    fn build(&self, server: &mut Server);

    /// Called after all plugins have been built.
    async fn ready(&self, _server: &mut Server) {}

    /// Called when a schedule this plugin registered for is triggered.
    fn update(&self, _server: &mut Server, _schedule: ScheduleId) {}

    /// Called when the server is shutting down.
    async fn cleanup(&self, _server: &mut Server) {}

    /// Declares which schedules this plugin wants to receive updates on.
    fn tick_schedules(&self) -> Vec<ScheduleId> { Vec::new() }

    /// Returns the plugin's name for debugging and dependency resolution.
    fn name(&self) -> &str { std::any::type_name::<Self>() }

    /// Declares plugins that must be added before this one.
    /// The server will panic if dependencies are not satisfied.
    fn dependencies(&self) -> Vec<PluginId> { Vec::new() }
}
```

## Lifecycle

The `Plugin` trait exposes lifecycle methods that the server calls at different stages of its lifetime.

### Startup

The server resolves dependencies before calling any lifecycle methods. It ensures that every plugin ID returned by `dependencies()` corresponds to a registered plugin. If any dependency is missing, or a circular dependency is detected, the server will panic.

The server then calls `build()` on each plugin in the order they are registered.

Once all plugins are built, the server then calls `ready()` on each plugin in dependency order. All resources registered during `build()` are available. This method is intended for validation, cross-plugin initialization, and API registration. See [api.md](./api.md) for how plugins expose and consume capabilities through the `API` primitive.

### Execution

During agent execution, the server calls `update()` on plugins that declared interest in a given schedule via `tick_schedules()`. Tick order follows the same dependency ordering as startup. Plugins that did not declare interest in a schedule will not receive updates for it.

### Shutdown

The server calls `cleanup()` on each plugin in reverse dependency order. Plugins that depend on other plugins are cleaned up before their dependencies.

## Dependencies

Plugins declare their dependencies by returning a list of plugin IDs from `dependencies()`. The server validates that all declared dependencies are present and creates the dependency graph to determine execution order across all lifecycle phases.

```rust
impl Plugin for ToolsPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_global(ToolRegistry::default());
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<TracingPlugin>(),
            PluginId::of::<ServerInfoPlugin>(),
        ]
    }
}
```

If one or more dependencies are missing when `Server::finish()` runs, the server panics with a message listing **every** missing dependency and the plugins that required it — so all problems can be fixed in one pass rather than one rebuild at a time.

### Auto-registering defaults

A plugin can offer zero-config defaults for its dependencies via `default_dependencies()`. Each offered plugin must implement `Default`. During `finish()`, before dependency validation, the server walks these offers and auto-registers any declared dependency the user did not add explicitly. Auto-registration is recursive (an auto-registered default may pull in its own defaults) and emits a `tracing::info!` so the applied defaults are visible. An explicitly added plugin always wins over a default.

```rust
impl Plugin for SessionsPlugin {
    fn build(&self, server: &mut Server) { /* ... */ }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PersistencePlugin>()]
    }

    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new().add::<PersistencePlugin>()
    }
}
```

Use this only when the dependency has a sensible default. Dependencies without a default still panic when missing.

## Server Access

Each lifecycle method receives a mutable reference to the `Server`. During `build()`, this is primarily used to register resources that the plugin provides.

The server supports two resource scopes.

**GlobalResource** is server-lifetime and read-only. All agents share the same instance. Configuration, registries, and LLM providers are typical global resources.

**LocalResource** is per-agent and mutable. A factory function creates a fresh instance for each agent context. Conversation history, scratchpads, and per-agent state are typical local resources.

```rust
use polaris_system::resource::{GlobalResource, LocalResource};

#[derive(Debug, Clone)]
pub struct ToolRegistry { /* ... */ }
impl GlobalResource for ToolRegistry {}

#[derive(Debug, Default)]
pub struct AgentMemory { pub messages: Vec<Message> }
impl LocalResource for AgentMemory {}

impl Plugin for MyPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_global(ToolRegistry::default());
        server.register_local(|| AgentMemory::default());
    }
}
```

State shared across agents belongs in `insert_global()`. State isolated per agent belongs in `register_local()`.

Systems in graphs may later access resources via `Res<T>` and `ResMut<T>` as explained in [systems documentation](./system.md).

## Scheduled Updates

Plugins may subscribe to server events by implementing `tick_schedules()`, which returns the set of schedules the plugin is interested in. When a subscribed schedule is triggered, the server calls `update()` with a `ScheduleId` identifying which schedule fired.

The server delivers updates to subscribed plugins in dependency order.

```rust
use polaris_graph::hooks::schedule::{OnGraphComplete, OnSystemComplete};

impl Plugin for MetricsPlugin {
    fn tick_schedules(&self) -> Vec<ScheduleId> {
        vec![
            ScheduleId::of::<OnSystemComplete>(),
            ScheduleId::of::<OnGraphComplete>(),
        ]
    }

    fn update(&self, server: &mut Server, schedule: ScheduleId) {
        if schedule == ScheduleId::of::<OnSystemComplete>() {
            self.collect_turn_metrics(server);
        } else if schedule == ScheduleId::of::<OnGraphComplete>() {
            self.report_metrics(server);
        }
    }
}
```

## Execution Hooks

Separately from scheduled updates, plugins can register lifecycle hooks that fire during graph execution — for example, before and after each system runs, or when a loop iteration begins. This is done through `HooksAPI`. Hook schedules and the executor's invocation of hooks are covered in [graph.md](./graph.md#hooks).

## Plugin Groups

Related plugins can be bundled into groups. Groups support customization through a builder that allows adding, removing, and reordering plugins.

```rust
pub struct DefaultPlugins;

impl PluginGroup for DefaultPlugins {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::new()
            .add(ServerInfoPlugin)
            .add(TimePlugin)
            .add(TracingPlugin)
    }
}
```

Groups can be customized at the call site:

```rust
Server::new()
    .add_plugins(
        DefaultPlugins
            .build()
            .disable::<TracingPlugin>()
            .add(CustomTracingPlugin { level: Level::DEBUG })
    )
    .run();
```

## Examples

### Basic Plugin

```rust
pub struct MyPlugin {
    pub api_key: String,
}

impl Plugin for MyPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_global(MyConfig {
            api_key: self.api_key.clone(),
        });
        server.register_local(MyState::default);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ServerInfoPlugin>()]
    }
}
```

### Configurable Plugin with Builder

```rust
pub struct AdvancedPlugin {
    enable_caching: bool,
    cache_ttl: Duration,
    max_retries: usize,
}

impl AdvancedPlugin {
    pub fn new() -> Self {
        Self {
            enable_caching: true,
            cache_ttl: Duration::from_secs(300),
            max_retries: 3,
        }
    }

    pub fn with_caching(mut self, enabled: bool) -> Self {
        self.enable_caching = enabled;
        self
    }

    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    pub fn with_max_retries(mut self, retries: usize) -> Self {
        self.max_retries = retries;
        self
    }
}

impl Plugin for AdvancedPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_global(RetryConfig {
            max_retries: self.max_retries,
        });

        if self.enable_caching {
            let ttl = self.cache_ttl;
            server.register_local(move || Cache::new(ttl));
        }
    }
}
```

### Plugin with Sub-Plugins

```rust
pub struct FullAgentPlugin;

impl Plugin for FullAgentPlugin {
    fn build(&self, server: &mut Server) {
        server.add_plugins(ToolsPlugin);
        server.add_plugins(MemoryPlugin::default());
        server.add_plugins(ReActAgentPlugin::default());
        server.insert_global(AgentMetrics::default());
    }
}
```

## Testing

Plugins can be tested in isolation by assembling a minimal server with only the relevant dependencies.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct MockLLMPlugin {
        responses: Vec<String>,
    }

    impl Plugin for MockLLMPlugin {
        fn build(&self, server: &mut Server) {
            let provider = MockLLMProvider::new(self.responses.clone());
            server.insert_global(LLM::new(Box::new(provider)));
        }
    }

    #[tokio::test]
    async fn plugin_registers_resources() {
        let mut server = Server::new();
        server.add_plugins(MinimalPlugins.build());
        server.add_plugins(MyPlugin { api_key: "test".into() });
        server.finish().await;

        let ctx = server.create_context();
        assert!(ctx.contains_resource::<MyConfig>());
    }

    #[tokio::test]
    async fn agent_with_mock_llm() {
        let mut server = Server::new();
        server
            .add_plugins(MinimalPlugins.build())
            .add_plugins(MockLLMPlugin {
                responses: vec!["Hello!".into()],
            })
            .add_plugins(MyAgentPlugin);
        server.update();
    }
}
```

## Documentation Standard

Every `pub` `Plugin` struct exported by this workspace must include rustdoc
covering the sections below. The standard exists so downstream consumers can
answer *"which plugin do I need for X?"* from a plugin's documentation alone —
without reading source.

The catalog drift guard at `tests/plugin_catalog.rs` enforces that every plugin
appears in the [Plugin Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/);
documentation-standard conformance itself is checked by `/review-docs` on every
PR.

### Required Sections

| Section | What it must contain |
|---------|----------------------|
| **Summary + when-to-use** | Opening paragraph describing what the plugin does and the situation in which a consumer should reach for it. |
| **Resources Provided** | Markdown table with columns `Resource \| Scope \| Description`. If the plugin registers no resources, include the section with a single `_none_` row explaining why (e.g., *"only mounts HTTP routes"* or *"only extends another plugin's API"*). |
| **APIs Provided** | Markdown table with columns `API \| Description`. Same `_none_` convention. |
| **Dependencies** | Either `None.` or a list of required plugins. If `default_dependencies()` is used to auto-register defaults, say so. |
| **Example** | A rustdoc code block (`no_run` acceptable) showing registration alongside dependencies and a typical usage pattern — a system that reads the resource, a request that hits a route, etc. |

### Conditional Sections

Include only when the plugin actually provides that kind of capability.
Omitting empty sections keeps docs tight; the alternative (`_none_` rows
everywhere) bloats every plugin.

| Section | Include when | What it must contain |
|---------|--------------|----------------------|
| **Routes Provided** | The plugin calls [`HttpRouter::add_routes`](crate::app::HttpRouter) (or `_with`) | Table with columns `Method \| Path \| Description`. Note the route prefix and which `axum::extract` types each handler reads. |
| **Tools Provided** | The plugin registers `#[tool]` definitions with [`ToolRegistry`](crate::tools::ToolRegistry) | Table with columns `Tool \| Description`. |
| **Hooks Registered** | The plugin registers hooks via `HooksAPI` | Table with columns `Schedule \| Description`. |
| **Middleware Registered** | The plugin registers middleware via [`MiddlewareAPI`](crate::graph::MiddlewareAPI) | Table with columns `Target \| Behavior \| Description`. |
| **Lifecycle** | The plugin uses `tick_schedules()` **or** is feature-gated **or** has non-trivial `ready()` / `cleanup()` behavior | Bullet list naming the tick schedules subscribed to, the `cfg` feature flags that gate the plugin, and any cross-plugin work done in `ready()` (e.g., decorating another plugin's registry). |
| **Extends** | The plugin contributes to another plugin's API surface — adds routes to [`HttpRouter`](crate::app::HttpRouter), registers a provider with [`ModelRegistry`](crate::models::ModelRegistry), pushes a layer through [`TracingLayers`](crate::plugins::TracingLayers), etc. | List of other plugins this plugin composes with and what it contributes. This is the discoverability signal that flags the plugin as a composer rather than a standalone provider — it lets a consumer answer *"what plugins decorate the model registry?"* by grepping the docs. |

### Canonical Exemplars

These plugins satisfy the standard and can be copied as a starting point:

- [`ServerInfoPlugin`](crate::plugins::ServerInfoPlugin) — minimal plugin: registers one resource, no APIs, no dependencies.
- [`SessionsPlugin`](crate::sessions::SessionsPlugin) — full plugin: registers an API, has dependencies, uses `default_dependencies()`.
- [`HttpPlugin`](crate::sessions::HttpPlugin) *(feature `sessions-http`)* — extender: provides no resources or APIs of its own, contributes routes to [`HttpRouter`](crate::app::HttpRouter) and uses the **Extends** section.
- [`AnthropicPlugin`](crate::models::AnthropicPlugin) *(feature `anthropic`)* — feature-gated extender: contributes a provider to [`ModelRegistry`](crate::models::ModelRegistry).

### Why this matters

The framework's discoverability promise — *"a consumer with goal X can find which
plugins, APIs, and resources to combine"* — only holds if every plugin
describes what it provides, depends on, and extends. The
[integration guide](./guide.md) and the [Plugin Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/)
can route a consumer to the right plugin, but they land on its rustdoc page
needing to confirm fit in one glance. Missing sections force them to read
source.

## Anti-Patterns

**Relying on insertion order instead of dependencies.** If a plugin requires another plugin's resources, that relationship should be declared in `dependencies()` rather than assumed from the order of `add_plugins()` calls.

**Circular dependencies.** If two plugins depend on each other, the shared functionality should be factored into a third plugin that both depend on.

**Documenting only the happy path.** If a plugin can fail at `build()` (e.g., missing config), warn in `ready()` (e.g., `expect_api` miss), or no-op without a feature flag, surface those behaviors in the rustdoc. A plugin's documentation is what downstream consumers debug against when wiring breaks.
