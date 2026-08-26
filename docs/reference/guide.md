---
title: Integration Guide
---

# Integration Guide

This is the *"how do I X?"* index for the Polaris workspace. It maps high-level
goals to the plugins, APIs, resources, and code patterns that combine to solve
them.

The deeper "what is a plugin / API / resource?" docs live in
[plugins.md](./plugins.md), [api.md](./api.md), and [resources.md](./resources.md).
This page is the front door — start here when you have a goal, then follow the
links into the specifics.

> **Discoverability promise.** Every plugin/API/resource exported by this
> workspace appears in one of the catalogs:
> [Plugin Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/),
> [API Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/apis/),
> [Resource Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/resources/).
> If a goal below points at a name that doesn't appear in a catalog, that's a
> bug — please open an issue.

## Common Integration Patterns

The most common things downstream consumers want to do, and what they reach for.

| Goal | Pattern | Key types | Reference |
|------|---------|-----------|-----------|
| Run a one-shot agent | `sessions.run_oneshot::<T>(&agent_type, \|ctx\| { ctx.insert(...); })` | [`SessionsAPI`](crate::sessions::SessionsAPI) | [Sessions — One-Shot](./sessions.md#one-shot-execution) |
| Run a multi-turn agent with cleanup | `sessions.scoped_session(&agent_type, ...)` → `guard.process_turn()` | [`SessionGuard`](crate::sessions::SessionGuard) | [Sessions — Scoped Sessions](./sessions.md#scoped-sessions-raii-guard) |
| Execute an agent from an HTTP request | `add_routes_with(...)` → `State<SessionsAPI>` → `HttpIOProvider` → `process_turn` | [`HttpRouter`](crate::app::HttpRouter), [`HttpIOProvider`](crate::sessions::http::HttpIOProvider) | [HTTP Integration](./http.md) |
| Register HTTP routes from a plugin | `server.api::<HttpRouter>().add_routes(router)` / `add_routes_with(\|server\| ...)`; use protected variants for private metadata or admin routes | [`HttpRouter`](crate::app::HttpRouter) | [HTTP Integration](./http.md) |
| Access Polaris APIs from HTTP handlers | `add_routes_with` closure resolves APIs against `&Server` during `AppPlugin::ready()`, then `.with_state(api)` on the returned `Router` | [`HttpRouter`](crate::app::HttpRouter) | [HTTP — Deferred Router Construction](./http.md#deferred-router-construction) |
| Create contexts outside the server | `ContextFactory` from `server.context_factory()` in `ready()` | [`ContextFactory`](crate::system::server::ContextFactory) | [Context — `ContextFactory`](./context.md#via-contextfactory) |
| Manage agent sessions | `SessionsAPI` — register agent, create session, process turns | [`SessionsAPI`](crate::sessions::SessionsAPI) | [Sessions](./sessions.md) |
| Advertise or discover agent capabilities | `sessions.register_contract(name, GraphSignature)` — satisfying agents advertise the name via `agent_type_infos()` / authenticated `GET /v1/sessions/agent-types/details` | [`SessionsAPI`](crate::sessions::SessionsAPI), [`GraphSignature`](crate::graph::GraphSignature) | [Sessions — Capability Contracts](./sessions.md#capability-contracts) |
| Mount the canonical sessions REST routes on your own router | `polaris_sessions::http::routes(sessions)` — the mount-point-relative public route table carries no auth and intentionally excludes private graph-signature metadata | [`routes`](crate::sessions::http::routes), [`SessionsAPI`](crate::sessions::SessionsAPI) | [HTTP — Session HTTP Endpoints](./http.md#session-http-endpoints) |
| Inject per-turn resources | Setup closure on `process_turn_with(\|ctx\| { ctx.insert(...); })` | `Res<T>`, [`LocalResource`](crate::system::resource::LocalResource) | [Sessions — Turn Execution](./sessions.md#turn-execution) |
| Bridge HTTP I/O to an agent | `HttpIOProvider::new()` → send input → inject `UserIO` → drain output | [`HttpIOProvider`](crate::sessions::http::HttpIOProvider), [`UserIO`](crate::plugins::UserIO) | [HTTP — `HttpIOProvider`](./http.md#httpioprovider-bridging-http-to-agent-io) |
| Understand context flow per graph node | Parallel forks children; Loop shares context; Scope composes per-resource crossing verbs (`share`/`forward`/`fork`/`forward_fresh`/`exclude`/`share_rest`) | [`ContextPolicy`](crate::graph::ContextPolicy) | [Context — Graph Flow](./context.md#context-flow-through-graph-execution) |
| Isolate or selectively share resources in a sub-graph | `ContextPolicy::new()` + per-resource verbs (or `ContextPolicy::shared()` for no boundary at all) | [`ContextPolicy`](crate::graph::ContextPolicy), [`ForkStrategy`](crate::system::resource::ForkStrategy) | [Graph — Scope](./graph.md#scope) |
| Duplicate a graph to explore variants | `base.duplicate()`, then mutate the independent copy | [`Graph`](crate::graph::Graph) | [Graph — Duplicating a Graph](./graph.md#duplicating-a-graph) |
| Find a node in a built graph by name | `graph.find_node_by_name(name)` / `find_nodes_by_name(name)` (all matches) / `find_system_by_name(name)` (skips control-flow nodes) | [`Graph`](crate::graph::Graph) | [Graph — Finding Nodes by Name](./graph.md#finding-nodes-by-name) |
| Select or swap a subgraph behind a slot at runtime | `add_dynamic(...)` for a fixed candidate set, or `add_dynamic_registry(...)` + a per-session `SubgraphRegistry` to swap candidates between turns; both signature-checked against a `GraphSignature` contract | [`DynamicNode`](crate::graph::DynamicNode), [`SubgraphRegistry`](crate::graph::SubgraphRegistry), [`GraphSignature`](crate::graph::GraphSignature) | [Graph — Dynamic](./graph.md#dynamic) |
| Add middleware to graph execution | `MiddlewareAPI::register_system()` in plugin `build()` | [`MiddlewareAPI`](crate::graph::MiddlewareAPI) | [Graph — Middleware](./graph.md#middleware) |
| Handle system errors in a graph | Fallible system + error edge + `ErrOut<CaughtError>` handler | [`ErrOut`](crate::system::param::ErrOut), [`CaughtError`](crate::graph::CaughtError) | [Graph — Error Handling](./graph.md#error-handling) |
| Schedule plugin updates | `tick_schedules()` + `update()` + `server.tick::<S>()` | — | [Scheduling](./scheduling.md) |
| Persist resources across session restart | Implement `Storable` + register via [`PersistenceAPI`](crate::plugins::PersistenceAPI) | [`PersistenceAPI`](crate::plugins::PersistenceAPI) | [Sessions — Persistence](./sessions.md#persistence-saveresume) |
| Add LLM tracing or token-cost annotation | [`TracingPlugin`](crate::plugins::TracingPlugin) is always on; declare per-model rates via [`LlmProvider::pricing()`](crate::models::llm::LlmProvider::pricing) to annotate spans with cost | [`TracingPlugin`](crate::plugins::TracingPlugin), [`LlmProvider`](crate::models::llm::LlmProvider) | [`DevTools`](./devtools.md) |
| Capture system parameter values for observability | `#[system(inspect(a, b, return))]` selects at compile time; [`InspectionPlugin`](crate::plugins::InspectionPlugin) delivers — enable via [`InspectionAPI`](crate::plugins::InspectionAPI), listen via `Extends<InspectionSinkRegistry>`; per-deployment start policy from an app-named environment variable via [`InspectionPlugin::with_policy_from_env`](crate::plugins::InspectionPlugin::with_policy_from_env) (spec: `off` / `all` / comma-separated system names; unset stays off) | [`InspectionAPI`](crate::plugins::InspectionAPI), [`InspectionSinkRegistry`](crate::plugins::InspectionSinkRegistry), [`InspectionSink`](crate::system::param::inspect::InspectionSink) | [System — Parameter Inspection](./system.md#parameter-inspection) |
| Keep credentials out of captured values (and out of telemetry export) | [`RedactionRules`](crate::plugins::RedactionRules) rules by binding name or type — a covered value arrives as `Redacted` with its `Debug` never run; set at build via `InspectionPlugin::with_redactions`, add from an app-named environment variable via [`with_redactions_from_env`](crate::plugins::InspectionPlugin::with_redactions_from_env) (spec: `param:<binding>` / `type:<Type>` entries; adds to baked-in rules, never drops one), add at run time via [`InspectionAPI::add_redacted_param`](crate::plugins::InspectionAPI::add_redacted_param) / [`add_redacted_type`](crate::plugins::InspectionAPI::add_redacted_type). To cut export alone, `disable_listener(INSPECTION_TRACING_LISTENER)` or `RUST_LOG="info,polaris::inspection=off"` | [`RedactionRules`](crate::plugins::RedactionRules), [`InspectionAPI`](crate::plugins::InspectionAPI), [`INSPECTION_TRACING_LISTENER`](crate::plugins::INSPECTION_TRACING_LISTENER) | [System — Parameter Inspection](./system.md#parameter-inspection) |
| Add a second inspection sink without cutting off the first | `ctx.inspection_arc()` to take the installed handle, wrap it, then `ctx.replace_inspection(wrapper)`; guard re-application by `Arc::ptr_eq` on the handle you installed | [`InspectionSink`](crate::system::param::inspect::InspectionSink), [`SystemContext`](crate::system::param::SystemContext) | [Context — Chaining onto an Installed Sink](./context.md#chaining-onto-an-installed-sink) |
| Add a new LLM provider | Implement [`LlmProvider`](crate::models::llm::LlmProvider), register with [`ModelRegistry`](crate::models::ModelRegistry) from a plugin's `ready()` | [`LlmProvider`](crate::models::llm::LlmProvider) | [Model Providers](./model-providers.md) |
| Cache an LLM prompt prefix to cut input cost | `llm.builder().cache_prefix()` for the stable system+tools prefix; `.cache_breakpoint()` as you assemble the window for incremental history caching | [`LlmRequestBuilder`](crate::models::llm::LlmRequestBuilder), [`CacheControl`](crate::models::llm::CacheControl) | [Model Providers — Prompt Caching](./model-providers.md#prompt-caching) |
| Make a function callable by an LLM | `#[tool]` macro on the function, register with [`ToolRegistry`](crate::tools::ToolRegistry) from a plugin | [`ToolRegistry`](crate::tools::ToolRegistry) | [Tools](./tools.md) |
| Run shell commands from a tool | [`ShellPlugin`](crate::shell::ShellPlugin) + [`ShellPermission`](crate::shell::ShellPermission) gate | [`ShellExecutor`](crate::shell::ShellExecutor) | — |
| Authenticate HTTP requests | Implement [`AuthProvider`](crate::app::AuthProvider), register via [`HttpRouter`](crate::app::HttpRouter) | [`AuthProvider`](crate::app::AuthProvider) | [HTTP — Authentication](./http.md#authentication) |
| Generate TypeScript bindings | Gate types with `#[cfg_attr(feature = "typegen", derive(TS), ts(export))]`, run `cargo test --features typegen` | — | [Typegen](./typegen.md) |
| Mock the clock in tests | [`MockClock`](crate::plugins::MockClock) under feature `test-utils` | [`Clock`](crate::plugins::Clock), [`MockClock`](crate::plugins::MockClock) | — |

## Quick Reference: Common Modifications

The most common framework-extension tasks and where to make the change.

| Task | Primary files | Secondary files |
|------|---------------|-----------------|
| Add a node type | `polaris_graph/src/node.rs` | `executor.rs`, `graph.rs` |
| Add an edge type | `polaris_graph/src/edge.rs` | `executor.rs`, `graph.rs` |
| Add a hook schedule | `polaris_graph/src/hooks/schedule.rs` | `hooks/events.rs`, `executor.rs` |
| Add a plugin | New file in `polaris_core_plugins/src/` | `polaris_core_plugins/src/lib.rs` |
| Define a system | Any file with the `#[system]` macro | — |
| Add a resource | Plugin file | Register in `build()`; see [resources.md — Documentation Standard](./resources.md#documentation-standard) |
| Add a tool | `polaris_tools/src/` with the `#[tool]` macro | Register in a plugin via [`ToolRegistry`](crate::tools::ToolRegistry) |
| Add a model provider | `polaris_model_providers/src/{provider}/` | `provider.rs`, `plugin.rs`, feature flag in `Cargo.toml` |
| Add HTTP routes | Plugin using `HttpRouter::add_routes` | `polaris_app/src/router.rs` |
| Add a `TS`-derived type | Gate with `#[cfg_attr(feature = "typegen", derive(TS), ts(export))]`; run `cargo test --features typegen`; commit `bindings/ts/src/`; add to `bindings/ts/src/index.ts` | See [typegen.md](./typegen.md) |
| Add unit tests | Same file, `#[cfg(test)]` block | — |
| Add integration tests | `crates/*/tests/*.rs` | — |

## Step-by-Step: Adding a Node Type

1. `crates/polaris_graph/src/node.rs` — add struct and enum variant
2. `crates/polaris_graph/src/executor.rs` — add execution logic in `run_node()`
3. `crates/polaris_graph/src/graph.rs` — add a builder method if needed
4. Add tests in `node.rs` under `#[cfg(test)]`

## Step-by-Step: Adding an Edge Type

1. `crates/polaris_graph/src/edge.rs` — add struct and enum variant
2. `crates/polaris_graph/src/executor.rs` — add traversal logic
3. `crates/polaris_graph/src/graph.rs` — add a builder method if needed
4. Add tests in `edge.rs` under `#[cfg(test)]`

## Step-by-Step: Adding a Plugin

1. Create `crates/polaris_core_plugins/src/my_plugin.rs`
2. Define resource types implementing [`GlobalResource`](crate::system::resource::GlobalResource) or [`LocalResource`](crate::system::resource::LocalResource)
3. Implement [`Plugin`](crate::system::plugin::Plugin) with `build()`, `ready()`, and `cleanup()` as needed
4. Add `mod my_plugin;` and re-export in `crates/polaris_core_plugins/src/lib.rs`
5. Add to [`DefaultPlugins`](crate::plugins::DefaultPlugins) or [`MinimalPlugins`](crate::plugins::MinimalPlugins) if appropriate
6. Document the plugin per [plugins.md — Documentation Standard](./plugins.md#documentation-standard)
7. Add a row to the [Plugin Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/) (the catalog drift guard at `tests/plugin_catalog.rs` enforces this)

## Step-by-Step: Adding an API

1. Define a `pub struct` implementing [`API`](crate::system::api::API)
2. Choose a [composition policy](./api.md#composition-policy) (open extension, provider-scoped, single-replace) and pick the matching interior-mutability pattern
3. Have a plugin call `server.insert_api(...)` in its `build()`
4. Document the API per [api.md — Documentation Standard](./api.md#documentation-standard)
5. Add a row to the [API Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/apis/) (the catalog drift guard at `tests/api_catalog.rs` enforces this)

## Step-by-Step: Adding a Resource

1. Define a `pub struct` implementing [`GlobalResource`](crate::system::resource::GlobalResource) or [`LocalResource`](crate::system::resource::LocalResource)
2. Have a plugin register it via `insert_global` (global) or `register_local` (local) in its `build()`
3. Document the resource per [resources.md — Documentation Standard](./resources.md#documentation-standard)
4. If the resource is consumer-facing, add a row to the [Resource Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/resources/) (the catalog drift guard at `tests/resource_catalog.rs` enforces this)
5. If the resource must survive checkpoints, implement [`Storable`](crate::plugins::Storable) and register with [`PersistenceAPI`](crate::plugins::PersistenceAPI)

## When this guide doesn't have your answer

1. Search the [Plugin Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/),
   [API Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/apis/), and
   [Resource Catalog](https://docs.rs/polaris-ai/latest/polaris_ai/resources/) for
   anything that looks adjacent — the per-item rustdoc is the canonical source.
2. Check the [reference docs index](./) for a topic-specific page.
3. If after both steps the answer still isn't obvious, that's a discoverability
   bug — please open an issue describing the goal and where you looked. The
   guide and catalogs are meant to make this *findable*, so missing entries
   are bugs in the discovery surface, not user error.
