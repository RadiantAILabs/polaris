# AGENTS.md

This file provides guidance to general-purpose coding agents when working with code in this repository.

`AGENTS.md` is the single source of truth for coding-agent guidance in this repository. `CLAUDE.md` is a symlink to this file, so both names resolve to the same content. Edit `AGENTS.md`; never edit `CLAUDE.md` directly.

## Project Overview

Polaris is a Rust-based modular framework for building AI agents using ECS-inspired system architecture. Agents are defined as directed graphs of async functions where each node executes when invoked and edges determine control flow.

**Repository**: <https://github.com/RadiantAILabs/polaris>
**Rust Edition**: 2024 (MSRV: 1.97.0)
**License**: Apache-2.0

## Build Commands

```bash
# Build
cargo build                    # Debug build
cargo build --release         # Release build

# Testing (full suite with format + clippy + tests)
cargo make test

# Individual test commands
cargo test --verbose          # Run tests only
cargo fmt -- --check          # Check formatting
cargo clippy --all-targets --all-features -- -D warnings
cargo make test-rustdoc       # Build docs with rustdoc warnings denied

# Fix formatting
cargo fmt
```

Uses `cargo-make` for task orchestration (see `Makefile.toml`).

## Architecture

See `docs/taxonomy.md` for the full layered architecture (Layer 1: System Framework, Layer 2: Graph Execution, Layer 3: Plugins) and `docs/philosophy.md` for design principles.

**Key mental model:** [Resources](docs/reference/resources.md) give systems runtime capabilities through typed parameters (`Res<T>`, `ResMut<T>`), while [APIs](docs/reference/api.md) let plugins coordinate during `build()` and `ready()`. [Plugins](docs/reference/plugins.md) own the lifecycle and may provide or extend both surfaces, including registries for routes, tools, hooks, middleware, and model providers.

### Crate Structure

- `polaris-ai` (root package, imported as `polaris_ai`) - Re-exports `polaris_internal`
- `crates/polaris_internal` - Internal facade re-exporting crates across all three layers
- `crates/polaris_system` - Layer 1: System framework
- `crates/polaris_system/system_macros` - Procedural macros for system definitions
- `crates/polaris_graph` - Layer 2: Graph-based agent execution primitives
- `crates/polaris_agent` - Layer 2: Agent trait and extension methods
- `crates/polaris_core_plugins` - Core plugins (ServerInfo, Tracing, Time, Random, etc.)
- `crates/polaris_models` - Layer 3: LLM provider interfaces and implementations
- `crates/polaris_model_providers` - Layer 3: LLM provider implementations
- `crates/polaris_tools` - Layer 3: Tool definitions and registry
- `crates/polaris_shell` - Layer 3: Shell command execution with permission model
- `crates/polaris_app` - Layer 3: Shared HTTP server runtime (axum, AppPlugin, HttpRouter)
- `crates/polaris_sessions` - Layer 3: Agent registration, sessions, turns, checkpoints, persistence, and HTTP adapters
- `examples/` - Example agents and applications

## Linting Configuration

Strict Clippy rules enforced (see `Cargo.toml` workspace lints):
- `missing_docs` - Document all public items
- `undocumented_unsafe_blocks` - Require safety comments
- `print_stdout` / `print_stderr` - No direct printing (use tracing)
- `allow_attributes_without_reason` - Use `#[expect(..., reason = "...")]` instead of `#[allow]`

Custom rules in `.clippy.toml`:
- `disallowed-names = ["e"]` - No single-letter error bindings

## Key Files

- `docs/philosophy.md` - Core design principles and architectural philosophy
- `docs/taxonomy.md` - Layered architecture and concept classification
- `docs/reference/system.md` - System primitives, `#[system]` macro, parameters
- `docs/reference/context.md` - SystemContext lifecycle, hierarchy, resource resolution, graph context flow
- `docs/reference/plugins.md` - Plugin system for compositional architecture
- `docs/reference/graph.md` - Graph construction, execution, error handling, hooks, middleware
- `docs/reference/agents.md` - Agent trait and pattern implementations
- `docs/reference/sessions.md` - SessionsAPI, turn execution, checkpoints, persistence
- `docs/reference/http.md` - HTTP handler integration, deferred router construction, HttpIOProvider
- `docs/reference/scheduling.md` - Server lifecycle, tick scheduling, plugin update ordering
- `docs/reference/data-flow.md` - Decision guide for `Res<T>` vs `ResMut<T>` vs `Out<T>`
- `docs/reference/tools.md` - Tool definitions, `ToolRegistry`, permissions, strictness, exposure, and selection
- `docs/reference/model-providers.md` - `LlmProvider` trait, adding a custom provider
- `docs/reference/devtools.md` - `SystemInfo`, event tracing, debugging graph execution
- `docs/reference/testing.md` - Testing strategy per architecture layer
- `CONTRIBUTING.md` - Public development setup and contribution workflow
- `docs/contributing/review-standards.md` - Public pull request review standards
- `Makefile.toml` - Build task definitions
- `.clippy.toml` - Clippy configuration

## Quick Navigation (Code Entry Points)

Before reading the entire file, read the first 100 lines when navigating to these key files:

| Concept | File | Purpose |
|---------|------|---------|
| Server and plugin lifecycle | `crates/polaris_system/src/server.rs` | Plugin orchestration, tick scheduling |
| System macro | `crates/polaris_system/system_macros/src/lib.rs` | `#[system]` proc macro (HRTB workaround) |
| Public API surface | `crates/polaris_system/src/api.rs` | Re-exports and public types |
| Graph structure | `crates/polaris_graph/src/graph/mod.rs` | Node/edge storage, lookup, append, and duplication |
| Graph builder | `crates/polaris_graph/src/graph/builder.rs` | Builder methods for systems and control-flow nodes |
| Graph validation | `crates/polaris_graph/src/graph/validation.rs` | Structural graph validation |
| Graph signatures | `crates/polaris_graph/src/graph/signature.rs` | `GraphSignature` derivation and comparison for Dynamic contracts |
| Graph executor | `crates/polaris_graph/src/executor/mod.rs` | Executor configuration, validation, and top-level execution |
| Graph traversal | `crates/polaris_graph/src/executor/run.rs` | Async traversal and context-boundary execution |
| Dynamic candidates | `crates/polaris_graph/src/registry.rs`, `crates/polaris_graph/src/selector.rs` | Per-session candidate registry and selector abstraction |
| Node types | `crates/polaris_graph/src/node.rs` | System, Decision, Switch, Parallel, etc. |
| Edge types | `crates/polaris_graph/src/edge.rs` | Sequential, Conditional, LoopBack, etc. |
| Hooks API | `crates/polaris_graph/src/hooks/api.rs` | Hook registration and invocation |
| Hook schedules | `crates/polaris_graph/src/hooks/schedule.rs` | Lifecycle event markers |
| Hook events | `crates/polaris_graph/src/hooks/events.rs` | Event data for hooks |
| DevTools | `crates/polaris_graph/src/dev.rs` | `DevToolsPlugin` and `SystemInfo` |
| Core plugins impl | `crates/polaris_core_plugins/` | Default plugins (for example ServerInfo, Tracing, Time) |
| OpenTelemetry export | `crates/polaris_core_plugins/src/otel_plugin.rs` | OTLP setup and `OpenTelemetryPlugin` lifecycle |
| Span processor extension | `crates/polaris_core_plugins/src/span_processor_registry.rs` | `SpanProcessorRegistry` fan-out extension point |
| App HTTP runtime | `crates/polaris_app/src/plugin.rs` | `AppPlugin` lifecycle, `ServerHandle` global resource |
| Route registration | `crates/polaris_app/src/router.rs` | `HttpRouter` API for plugin-based route composition |
| Auth extension | `crates/polaris_app/src/auth.rs` | `AuthProvider` trait for pluggable authentication |
| HTTP IO bridging | `crates/polaris_sessions/src/http/io.rs` | `HttpIOProvider` - channels bridging HTTP to `UserIO` |
| Sessions API | `crates/polaris_sessions/src/api.rs` | `SessionsAPI`, turn execution, checkpoints |
| Sessions plugin | `crates/polaris_sessions/src/lib.rs` | `SessionsPlugin`, re-exports |
| Session RAII guard | `crates/polaris_sessions/src/guard.rs` | `SessionGuard` - auto-cleanup on drop |
| Session HTTP handlers | `crates/polaris_sessions/src/http/handlers.rs` | REST endpoint implementations |
| Session HTTP plugin | `crates/polaris_sessions/src/http/mod.rs` | `HttpPlugin`, endpoint table |
| Middleware API | `crates/polaris_graph/src/middleware/mod.rs` | `MiddlewareAPI`, target types, handler trait |
| SystemContext | `crates/polaris_system/src/param/mod.rs` | Context struct, `Res<T>`, `ResMut<T>`, hierarchy |
| Execution errors | `crates/polaris_graph/src/executor/error.rs` | `ExecutionError`, `CaughtError`, `ErrOut` |

## Discovery and Integration

For *"how do I X?"* answers — what plugins/APIs/resources to combine for a given goal — see [`docs/reference/guide.md`](docs/reference/guide.md).

For per-thing documentation standards (every plugin, API, and resource has required rustdoc sections), see:

- [`docs/reference/plugins.md#documentation-standard`](docs/reference/plugins.md#documentation-standard)
- [`docs/reference/api.md#documentation-standard`](docs/reference/api.md#documentation-standard)
- [`docs/reference/resources.md#documentation-standard`](docs/reference/resources.md#documentation-standard)

The catalogs (every shipped plugin, API, and resource) live at [`src/docs/plugins.md`](src/docs/plugins.md), [`src/docs/apis.md`](src/docs/apis.md), and [`src/docs/resources.md`](src/docs/resources.md). Each has a CI drift guard under `tests/`. The `/review-docs` skill checks PRs against the standards.

Contributor-facing workflow and review requirements live in [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`docs/contributing/review-standards.md`](docs/contributing/review-standards.md). Keep agent review guidance consistent with those public standards.

## Common Integration Patterns

These map high-level goals to the files and patterns needed:

| Goal | Pattern | Key Files | Reference Doc |
|------|---------|-----------|---------------|
| **Run one-shot agent** | `sessions.run_oneshot::<T>(&agent_type, \|ctx\| { ctx.insert(...) })` | `polaris_sessions/src/api.rs` | [Sessions - One-Shot](docs/reference/sessions.md#one-shot-execution) |
| **Multi-turn with cleanup** | `sessions.scoped_session(&agent_type, \|ctx\| { ... })` -> `guard.process_turn()` | `polaris_sessions/src/guard.rs` | [Sessions - Scoped Sessions](docs/reference/sessions.md#scoped-sessions-raii-guard) |
| **Execute agent from HTTP** | `add_routes_with` -> `State<SessionsAPI>` -> `HttpIOProvider` -> `process_turn` | `polaris_sessions/src/http/handlers.rs`, `polaris_sessions/src/http/io.rs` | [HTTP Integration](docs/reference/http.md) |
| **Register HTTP routes from a plugin** | `server.api::<HttpRouter>().add_routes(router)` (stateless) or `add_routes_with(\|server\| ...)` (needs another plugin's API) in `build()` | `polaris_app/src/router.rs` | [HTTP Integration](docs/reference/http.md) |
| **Access Polaris APIs from HTTP handlers** | `add_routes_with` closure resolves APIs against `&Server` during `AppPlugin::ready()`, then `.with_state(api)` on the returned `Router` | `polaris_sessions/src/http/mod.rs` | [HTTP Integration - Deferred Router Construction](docs/reference/http.md#deferred-router-construction) |
| **Create contexts outside the server** | `ContextFactory` from `server.context_factory()` in `ready()` | `polaris_system/src/server.rs` | [Execution Context - ContextFactory](docs/reference/context.md#via-contextfactory) |
| **Manage agent sessions** | `SessionsAPI` - register agent, create session, process turns | `polaris_sessions/src/api.rs` | [Sessions](docs/reference/sessions.md) |
| **Inject per-turn resources** | Setup closure in `process_turn_with(\|ctx\| { ctx.insert(...) })` | `polaris_sessions/src/api.rs` | [Sessions - Turn Execution](docs/reference/sessions.md#turn-execution) |
| **Bridge HTTP IO to agent** | `HttpIOProvider::new()` -> send input -> inject `UserIO` -> drain output | `polaris_sessions/src/http/io.rs` | [HTTP Integration - HttpIOProvider](docs/reference/http.md#httpioprovider-bridging-http-to-agent-io) |
| **Understand context flow per node** | Parallel forks children; Loop shares context; Scope and Dynamic apply a `ContextPolicy` boundary | `polaris_graph/src/executor/run.rs` | [Execution Context - Graph Flow](docs/reference/context.md#context-flow-through-graph-execution) |
| **Isolate or selectively share resources in a subgraph** | `ContextPolicy::new()` plus per-resource crossing verbs, or `ContextPolicy::shared()` for no boundary | `polaris_graph/src/node.rs`, `polaris_graph/src/executor/run.rs` | [Graph - Scope](docs/reference/graph.md#scope) |
| **Duplicate a graph to explore variants** | `base.duplicate()`, then mutate the independent copy | `polaris_graph/src/graph/mod.rs` | [Graph - Duplicating a Graph](docs/reference/graph.md#duplicating-a-graph) |
| **Find nodes in a built graph** | `find_node_by_name`, `find_nodes_by_name`, or `find_system_by_name` | `polaris_graph/src/graph/mod.rs` | [Graph - Finding Nodes by Name](docs/reference/graph.md#finding-nodes-by-name) |
| **Select or swap a subgraph at runtime** | `add_dynamic(...)`, or `add_dynamic_registry(...)` with a per-session `SubgraphRegistry`; candidates satisfy a `GraphSignature` contract | `polaris_graph/src/graph/builder.rs`, `polaris_graph/src/graph/signature.rs`, `polaris_graph/src/registry.rs` | [Graph - Dynamic](docs/reference/graph.md#dynamic) |
| **Add middleware to graph execution** | `MiddlewareAPI::register_system()` in plugin `build()` | `polaris_graph/src/middleware/` | [Graph - Middleware](docs/reference/graph.md#middleware) |
| **Handle system errors in graph** | Fallible system + error edge + `ErrOut<CaughtError>` handler | `polaris_graph/src/executor/error.rs` | [Graph - Error Handling](docs/reference/graph.md#error-handling) |
| **Schedule plugin updates** | `tick_schedules()` + `update()` + `server.tick::<S>()` | `polaris_system/src/server.rs` | [Scheduling](docs/reference/scheduling.md) |
| **Extend OpenTelemetry export** | Enable the `otel` feature, then add a processor directly with `OpenTelemetryPlugin::with_span_processor` or contribute through `Extends<SpanProcessorRegistry>` | `polaris_core_plugins/src/otel_plugin.rs`, `polaris_core_plugins/src/span_processor_registry.rs` | [Plugin Capabilities](docs/reference/plugins.md#capability-based-dependencies), [API Catalog](src/docs/apis.md) |

## Quick Reference: Common Modifications

| Task | Primary Files | Secondary Files |
|------|---------------|-----------------|
| Add node type | `polaris_graph/src/node.rs` | `graph/builder.rs`, `graph/validation.rs`, `executor/mod.rs`, `executor/run.rs` |
| Add edge type | `polaris_graph/src/edge.rs` | `graph/mod.rs`, `graph/validation.rs`, `executor/run.rs` |
| Add hook schedule | `polaris_graph/src/hooks/schedule.rs` | `hooks/events.rs`, `executor/mod.rs`, `executor/run.rs` |
| Add plugin | New file in the owning Layer 3 crate | Crate `lib.rs`, [plugin guide](docs/reference/plugins.md), [plugin catalog](src/docs/plugins.md) |
| Add API | Providing plugin and API type | [API guide](docs/reference/api.md), [API catalog](src/docs/apis.md) |
| Register hooks or middleware | Plugin `build()` via `HooksAPI` or `MiddlewareAPI` | [Graph - Hooks](docs/reference/graph.md#hooks), [Graph - Middleware](docs/reference/graph.md#middleware) |
| Define system | Any file with `#[system]` macro | - |
| Add resource | Plugin file | Register in `build()` |
| Add tool | `polaris_tools/src/` with `#[tool]` macro | Register in plugin via `ToolRegistry` |
| Add model provider | `polaris_model_providers/src/{provider}/` | `provider.rs`, `plugin.rs`, feature flag in `Cargo.toml` |
| Add HTTP routes | Plugin with `HttpRouter::add_routes` | `polaris_app/src/router.rs` |
| Add unit tests | Same file in `#[cfg(test)]` block | - |
| Add integration tests | `crates/*/tests/*.rs` | - |

### Adding a Node Type

1. Add the struct and `Node` enum variant in `crates/polaris_graph/src/node.rs`.
2. Add builder and validation support in `graph/builder.rs` and `graph/validation.rs`.
3. Add validation/execution dispatch in `executor/mod.rs` and `executor/run.rs`.
4. Add focused unit and integration tests. See [Graph](docs/reference/graph.md) for the canonical behavior contract.

### Adding an Edge Type

1. Add the struct and `Edge` enum variant in `crates/polaris_graph/src/edge.rs`.
2. Update graph construction/validation in `graph/mod.rs` and `graph/validation.rs`.
3. Add traversal logic in `executor/run.rs`.
4. Add focused unit and integration tests. See [Graph](docs/reference/graph.md) for the canonical behavior contract.

### Adding a Plugin

Plugins own composition during the server lifecycle: they may provide resources or APIs and extend registries by contributing routes, tools, hooks, middleware, model providers, or other capabilities. Keep this section as a repository checklist; use the linked references for the actual contracts:

- [Plugin lifecycle, capability dependencies, and documentation standard](docs/reference/plugins.md)
- [API ownership and composition policies](docs/reference/api.md)
- [Resource scopes and documentation standard](docs/reference/resources.md)
- [Hooks](docs/reference/graph.md#hooks) and [middleware](docs/reference/graph.md#middleware)
- [Integration guide](docs/reference/guide.md) and the [plugin](src/docs/plugins.md), [API](src/docs/apis.md), and [resource](src/docs/resources.md) catalogs

Repository checklist:

1. Create the plugin in the Layer 3 crate that owns the capability and export it from that crate's `lib.rs`.
2. Declare capability relationships with the `#[plugin]` typed build parameters where possible; reserve `dependencies()` for pure ordering or lifecycle relationships that are not represented by a capability.
3. Register or extend the relevant resources, APIs, hooks, middleware, routes, tools, or provider registries in the lifecycle phase required by their reference documentation.
4. Add the plugin to `DefaultPlugins` or `MinimalPlugins` only when it belongs in that default composition.
5. Add focused tests, document the plugin using the [plugin documentation standard](docs/reference/plugins.md#documentation-standard), and add or update catalog and integration-guide entries.
6. If capability declarations change the representative plugin graph, regenerate and commit `examples/plugins.lock` with `POLARIS_BLESS_PLUGINS_LOCK=1 cargo test -p examples --test plugins_lock`.

## Implementation Status

| Component | Status | Notes |
|-----------|--------|-------|
| Layer 1: System Framework | Complete | Systems, Resources, Plugins, Server |
| Layer 2: Graph Execution | Complete | System, Decision, Switch, Parallel, Loop, Scope, and Dynamic nodes; all 6 edge types |
| Layer 2: Agent Trait | Complete | Agent trait for pattern definition |
| Layer 3: LLM Providers | Complete | Anthropic, OpenAI, Bedrock via `polaris_model_providers` |
| Layer 3: Tool Registry | Complete | `#[tool]` / `#[toolset]` macros, `ToolRegistry`, `ToolsPlugin` |
| Layer 3: HTTP App Runtime | Complete | `polaris_app`: `AppPlugin`, `HttpRouter`, `AuthProvider` |
| Layer 3: Sessions | Complete | `SessionsPlugin`, `SessionsAPI`, turn execution, persistence, and HTTP adapters |
| Layer 3: Agent Implementations | Example available | ReAct lives in `examples/`; concrete agent-pattern plugins remain downstream concerns |

## Rustdoc Examples

Prefer `no_run` or fully compilable examples over `ignore`. Use `ignore` only as a last resort when the snippet genuinely cannot compile in a doctest context due to external dependencies or runtime requirements.

## Documentation Guidelines

When modifying any file in `docs/**/*.md`, always check other documentation files for inconsistencies. Concepts, terminology, and examples should remain consistent across all documentation.

**Precedence order** (highest to lowest):
1. `docs/philosophy.md` - Core design principles (authoritative source of truth)
2. `docs/taxonomy.md` - Layered architecture and concept classification
3. `docs/reference/*.md` - Pattern implementations and detailed specifications

If a change affects repository navigation or coding-agent guidance, update `AGENTS.md`. `CLAUDE.md` is a symlink and follows it automatically; never replace or edit the symlink directly.

## Implementation Guidelines

When planning and implementing new features, refer to `docs/philosophy.md` and `docs/taxonomy.md` to ensure alignment with core design principles and architecture.

## Contribution Workflow

Use [`CONTRIBUTING.md`](CONTRIBUTING.md) as the public workflow and [`docs/contributing/review-standards.md`](docs/contributing/review-standards.md) as the public review contract. The rules below summarize repository-specific agent guidance and must remain consistent with those documents.

### Layer Isolation

| Layer | Crates | Rule |
|-------|--------|------|
| **1 - System Framework** | `polaris_system`, `system_macros` | Own dedicated ticket. Changes affect everything. |
| **2 - Graph Execution** | `polaris_graph`, `polaris_agent` | Own dedicated ticket. Changes affect all agents. |
| **3 - Plugins** | `polaris_core_plugins`, `polaris_models`, `polaris_model_providers`, `polaris_tools`, `polaris_shell`, `polaris_app`, `polaris_sessions` | Must not modify Layer 1 or 2. Isolated changes. |

The root `polaris-ai` package and `polaris_internal` re-exports are updated in the same ticket as the change they expose. Prefer tickets to touch one crate and one architecture layer. Wider changes are acceptable when splitting them would make the implementation less coherent or safe.

### Branch Naming

- With Shortcut story: `sc-{id}/{short-description}` (for example `sc-3154/add-session-plugin`)
- Without story: `feat/`, `fix/`, `refactor/`, `docs/`, `test/`, `chore/` prefix

### Commit Messages

Conventional commit style: `<type>: <short summary>`. Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`.

### Pull Requests

- Title: imperative, under 70 chars, typed (for example `feat: add ...`)
- Description: explain why and link the Shortcut story or GitHub issue when applicable
- One logical change per PR
- Tests required for features and fixes

### Available Skills

- `/create-ticket` — create a well-scoped Shortcut story
- `/evaluate-ticket` — evaluate tickets against quality checklist
- `/roadmap` — decompose an initiative into sequenced tickets
- `/review-pr` — review a PR against contribution standards

## Ignored Files

- `temp/*` - Temporary files
- `logs/*` - Log files
- `target/*` - Build artifacts
- `data/*` - Application artifacts
- `.claude/` - Agent-tool metadata; ignore unless the task explicitly targets it
- `.cargo/` - Cargo configuration metadata
