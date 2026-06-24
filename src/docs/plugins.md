Core infrastructure plugins for the Polaris runtime, plus a catalog of every
plugin shipped by this workspace.

This module re-exports the plugin groups and individual plugins from
[`polaris_core_plugins`]. It also serves as the documentation home for the
**Plugin Catalog** below — a single index of every `Plugin` implementation
exported by `polaris-ai`, with links to each plugin's rustdoc page (which is
the canonical source for its provided resources, APIs, and dependencies).

[`polaris_core_plugins`]: crate::plugins

# Plugin Groups

[`DefaultPlugins`](crate::plugins::DefaultPlugins) bundles the standard set of infrastructure plugins
suitable for most applications:

```no_run
use polaris_ai::plugins::DefaultPlugins;
use polaris_ai::system::server::Server;
use polaris_ai::system::plugin::PluginGroup;

let mut server = Server::new();
server.add_plugins(DefaultPlugins::new().build());
```

[`MinimalPlugins`](crate::plugins::MinimalPlugins) provides a lighter set for testing and constrained
environments.

Groups support customization through a builder:

```no_run
# use polaris_ai::plugins::{DefaultPlugins, MinimalPlugins};
# use polaris_ai::system::server::Server;
# use polaris_ai::system::plugin::PluginGroup;
# let mut server = Server::new();
server.add_plugins(
    DefaultPlugins::new()
        .build()
);
```

# Plugin Catalog

Every `Plugin` implementation exported by `polaris-ai`, grouped by area. Each
entry links to the plugin's rustdoc page, which documents its provided
resources, APIs, dependencies, and a registration example per the project
plugin documentation standard.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/plugin_catalog.rs`) that scans the workspace for `impl Plugin for X`
> and asserts each name appears below. Adding a new plugin without listing it
> here will fail CI.

## Layer 1 — Infrastructure

Plugins that provide foundational resources shared by every Polaris
application. Most ship via [`polaris_ai::plugins`](crate::plugins).

| Plugin | What it enables |
|--------|-----------------|
| [`ServerInfoPlugin`](crate::plugins::ServerInfoPlugin) | Server identity, version, and runtime metadata as a global [`ServerInfo`](crate::plugins::ServerInfo) resource. |
| [`TimePlugin`](crate::plugins::TimePlugin) | Wall-clock time via the [`Clock`](crate::plugins::Clock) resource; mockable in tests with `MockClock`. |
| [`TracingPlugin`](crate::plugins::TracingPlugin) | Console / structured tracing subscriber, graph and system instrumentation hooks, and the [`TracingLayers`](crate::plugins::TracingLayers) for plugins that need to push their own subscriber layer. |
| [`PersistencePlugin`](crate::plugins::PersistencePlugin) | The [`PersistenceAPI`](crate::plugins::PersistenceAPI) — a registry of `Storable` resource serializers used by sessions and other state-bearing plugins. |
| [`OpenTelemetryPlugin`](crate::plugins::OpenTelemetryPlugin) *(feature `otel`)* | Adds an OTLP export layer to the tracing subscriber for distributed tracing backends. |
| [`DevToolsPlugin`](crate::graph::DevToolsPlugin) | Graph-level developer tooling: per-node [`SystemInfo`](crate::graph::SystemInfo) records, execution event tracing, and graph introspection. Lives in [`polaris_ai::graph`](crate::graph). |

## Layer 3 — Tools

Tool registry and discovery primitives.

| Plugin | What it enables |
|--------|-----------------|
| [`ToolsPlugin`](crate::tools::ToolsPlugin) | The global [`ToolRegistry`](crate::tools::ToolRegistry) — registration, lookup, and invocation of `#[tool]` / `#[toolset]` definitions, plus the tool permission model. With the `dashboard` feature, also mounts `GET /v1/tools` — a frozen snapshot of registered tools, schemas, and permissions for an external dashboard frontend to consume. |

## Layer 3 — Models & Providers

Provider-agnostic LLM access plus concrete provider plugins.

| Plugin | What it enables |
|--------|-----------------|
| [`ModelsPlugin`](crate::models::ModelsPlugin) | The global [`ModelRegistry`](crate::models::ModelRegistry) — provider-keyed model lookup so consumers depend on the registry, not on a specific provider. With the `dashboard` feature, also mounts `GET /v1/models/providers` — a frozen snapshot of registered providers for an external dashboard frontend to consume. |
| [`TokenizerPlugin`](crate::models::TokenizerPlugin) | Pluggable [`Tokenizer`](crate::models::Tokenizer) resource for token counting and prompt budgeting; backed by tiktoken when the `tiktoken` feature is on. |
| [`AnthropicPlugin`](crate::models::AnthropicPlugin) *(feature `anthropic`)* | Registers the Anthropic provider (Messages API, including tool use and streaming) with [`ModelRegistry`](crate::models::ModelRegistry). |
| [`OpenAiPlugin`](crate::models::OpenAiPlugin) *(feature `openai`)* | Registers the `OpenAI` provider (Chat Completions, tool use, streaming) with [`ModelRegistry`](crate::models::ModelRegistry). |
| [`BedrockPlugin`](crate::models::BedrockPlugin) *(feature `bedrock`)* | Registers AWS Bedrock-hosted models (Anthropic, Meta, etc.) with [`ModelRegistry`](crate::models::ModelRegistry). |

## Layer 3 — Sessions

Session lifecycle, turn execution, and (optional) HTTP/dashboard surfaces.

| Plugin | What it enables |
|--------|-----------------|
| [`SessionsPlugin`](crate::sessions::SessionsPlugin) | The [`SessionsAPI`](crate::sessions::SessionsAPI) — agent-type registration, session creation, scoped sessions ([`SessionGuard`](crate::sessions::SessionGuard)), turn execution, checkpointing, and persistence via a pluggable [`SessionStore`](crate::sessions::SessionStore). |
| [`HttpPlugin`](crate::sessions::HttpPlugin) *(feature `sessions-http`)* | Registers REST endpoints for sessions and an `HttpIOProvider` that bridges HTTP request/response (and SSE) to in-process [`UserIO`](crate::plugins::UserIO). Requires [`AppPlugin`](crate::app::AppPlugin). |

## Layer 3 — HTTP App Runtime

Shared axum-based HTTP runtime that other plugins build on top of.

| Plugin | What it enables |
|--------|-----------------|
| [`AppPlugin`](crate::app::AppPlugin) | The shared HTTP server: [`HttpRouter`](crate::app::HttpRouter) for plugin-composed routes, the [`ServerHandle`](crate::app::ServerHandle) global resource, the [`WsRouter`](crate::app::WsRouter), and pluggable [`AuthProvider`](crate::app::AuthProvider). |
| [`RequestContextPlugin`](crate::app::RequestContextPlugin) | Per-request [`RequestContext`](crate::app::RequestContext) injection — propagates HTTP headers and identity into agent system contexts. |

## Layer 3 — Shell

Shell command execution behind the project permission model.

| Plugin | What it enables |
|--------|-----------------|
| [`ShellPlugin`](crate::shell::ShellPlugin) | A configured [`ShellExecutor`](crate::shell::ShellExecutor) resource and shell-execution tools, gated by [`ShellPermission`](crate::shell::ShellPermission). |

## Layer 3 — Dashboard

The umbrella `dashboard` feature extends Layer-3 plugins with HTTP surfaces
intended for an external dashboard frontend. It flips the per-crate
`dashboard` features in `polaris_tools` and `polaris_models`; the host
plugins then mount their endpoints during `build()` and depend on
[`AppPlugin`](crate::app::AppPlugin).

| Plugin | What the `dashboard` feature enables |
|--------|--------------------------------------|
| [`ToolsPlugin`](crate::tools::ToolsPlugin) | Mounts `GET /v1/tools` — a frozen [`ToolsSnapshot`](crate::tools::dashboard::ToolsSnapshot) of registered tools, schemas, and permissions. |
| [`ModelsPlugin`](crate::models::ModelsPlugin) | Mounts `GET /v1/models/providers` — a frozen [`ModelsSnapshot`](crate::models::dashboard::ModelsSnapshot) of registered model providers. |

# Observability

Graph, model, and tool tracing are always on — [`TracingPlugin`](crate::plugins::TracingPlugin)
unconditionally registers graph middleware via [`crate::graph::MiddlewareAPI`] and
decorates both the global [`crate::models::ModelRegistry`] and [`crate::tools::ToolRegistry`].
With no subscriber attached the spans have no observable cost.

| Feature | Public item to look for | Existing surface it changes | Runtime/API surface |
|---------|--------------------------|----------------------------|---------------------|
| `otel` | [`OpenTelemetryPlugin`](crate::plugins::OpenTelemetryPlugin) | Integrates with [`TracingPlugin`](crate::plugins::TracingPlugin) and [`TracingLayers`](crate::plugins::TracingLayers) | Pushes an OTLP export layer into the tracing subscriber |

If you are trying to answer "what does the `otel` feature export?", the public
type is [`OpenTelemetryPlugin`](crate::plugins::OpenTelemetryPlugin) under
[`polaris_ai::plugins`](crate::plugins). This is separate from the crate-local
`otel` feature on `polaris_app`.

# Related

- [Plugin trait and lifecycle](crate::system) -- the `Plugin` trait definition
- [Graph hooks](crate::graph) -- hook system that tracing plugins use
- [Feature flags](crate#observability) -- enabling tracing features
