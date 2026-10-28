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

**Key mental model:** Resources (both Global and Local) are how we give agents capabilities. An LLM provider, a tool registry, or a memory backend each exists as a resource. Systems are how agents interact with those capabilities, accessing them through typed parameters (`Res<T>`, `ResMut<T>`).

### Crate Structure

- `polaris-ai` (root package, imported as `polaris_ai`) - Re-exports `polaris_internal`
- `crates/polaris_internal` - Re-exports Layer 1 and Layer 2 crates
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
- `docs/reference/http.md` - HTTP handler integration, deferred router construction, ChannelIOProvider
- `docs/reference/scheduling.md` - Server lifecycle, tick scheduling, plugin update ordering
- `docs/reference/data-flow.md` - Decision guide for `Res<T>` vs `ResMut<T>` vs `Out<T>`
- `docs/reference/tools.md` - Tool definitions, `ToolRegistry`, permission model
- `docs/reference/model-providers.md` - `LlmProvider` trait, adding a custom provider
- `docs/reference/devtools.md` - `SystemInfo`, event tracing, debugging graph execution
- `docs/reference/testing.md` - Testing strategy per architecture layer
- `Makefile.toml` - Build task definitions
- `.clippy.toml` - Clippy configuration

## Quick Navigation (Code Entry Points)

Before reading the entire file, read the first 100 lines when navigating to these key files:

| Concept | File | Purpose |
|---------|------|---------|
| Server and plugin lifecycle | `crates/polaris_system/src/server.rs` | Plugin orchestration, tick scheduling |
| System macro | `crates/polaris_system/system_macros/src/lib.rs` | `#[system]` proc macro (HRTB workaround) |
| Public API surface | `crates/polaris_system/src/api.rs` | Re-exports and public types |
| Graph builder | `crates/polaris_graph/src/graph.rs` | Graph construction and node/edge storage |
| Graph execution | `crates/polaris_graph/src/executor.rs` | Async traversal, resource resolution |
| Node types | `crates/polaris_graph/src/node.rs` | System, Decision, Switch, Parallel, etc. |
| Edge types | `crates/polaris_graph/src/edge.rs` | Sequential, Conditional, LoopBack, etc. |
| Hooks API | `crates/polaris_graph/src/hooks/api.rs` | Hook registration and invocation |
| Hook schedules | `crates/polaris_graph/src/hooks/schedule.rs` | Lifecycle event markers |
| Hook events | `crates/polaris_graph/src/hooks/events.rs` | Event data for hooks |
| DevTools | `crates/polaris_graph/src/dev.rs` | `DevToolsPlugin` and `SystemInfo` |
| Core plugins impl | `crates/polaris_core_plugins/` | Default plugins (for example ServerInfo, Tracing, Time) |
| App HTTP runtime | `crates/polaris_app/src/plugin.rs` | `AppPlugin` lifecycle, `ServerHandle` global resource |
| Route registration | `crates/polaris_app/src/router.rs` | `HttpRouter` API for plugin-based route composition |
| Auth extension | `crates/polaris_app/src/auth.rs` | `AuthProvider` trait for pluggable authentication |
| Channel IO bridging | `crates/polaris_sessions/src/io.rs` | `ChannelIOProvider` - caller-owned channels bridging to `UserIO` |
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

The catalogs (every shipped plugin, API, and resource) live at [`src/docs/plugins.md`](src/docs/plugins.md), [`src/docs/apis.md`](src/docs/apis.md), and [`src/docs/resources.md`](src/docs/resources.md). Each has a CI drift guard under `tests/`.

## Common Integration Patterns

These map high-level goals to the files and patterns needed:

| Goal | Pattern | Key Files | Reference Doc |
|------|---------|-----------|---------------|
| **Run one-shot agent** | `sessions.run_oneshot::<T>(&agent_type, \|ctx\| { ctx.insert(...); })` | `polaris_sessions/src/api.rs` | [Sessions - One-Shot](docs/reference/sessions.md#one-shot-execution) |
| **Multi-turn with cleanup** | `sessions.scoped_session(&agent_type, \|ctx\| { ... })` -> `guard.process_turn()` | `polaris_sessions/src/guard.rs` | [Sessions - Scoped Sessions](docs/reference/sessions.md#scoped-sessions-raii-guard) |
| **Execute agent from HTTP** | `add_routes_with` -> `State<SessionsAPI>` -> `ChannelIOProvider` -> `process_turn` | `polaris_sessions/src/http/handlers.rs`, `polaris_sessions/src/io.rs` | [HTTP Integration](docs/reference/http.md) |
| **Register HTTP routes from a plugin** | `server.api::<HttpRouter>().add_routes(router)` (stateless) or `add_routes_with(\|server\| ...)` (needs another plugin's API) in `build()`; use protected variants for private metadata/admin routes | `polaris_app/src/router.rs` | [HTTP Integration](docs/reference/http.md) |
| **Access Polaris APIs from HTTP handlers** | `add_routes_with` closure resolves APIs against `&Server` during `AppPlugin::ready()`, then `.with_state(api)` on the returned `Router` | `polaris_sessions/src/http/mod.rs` | [HTTP Integration - Deferred Router Construction](docs/reference/http.md#deferred-router-construction) |
| **Create contexts outside the server** | `ContextFactory` from `server.context_factory()` in `ready()` | `polaris_system/src/server.rs` | [Execution Context - ContextFactory](docs/reference/context.md#via-contextfactory) |
| **Manage agent sessions** | `SessionsAPI` - register agent, create session, process turns | `polaris_sessions/src/api.rs` | [Sessions](docs/reference/sessions.md) |
| **Advertise or discover agent capabilities** | `sessions.register_contract(name, GraphSignature)` - satisfying agents advertise the name via `agent_type_infos()` / authenticated `GET /v1/sessions/agent-types/details` | `polaris_sessions/src/api.rs` | [Sessions - Capability Contracts](docs/reference/sessions.md#capability-contracts) |
| **Mount the canonical sessions REST routes on your own router** | `polaris_sessions::http::routes(sessions)` - mount-point-relative public `Router`; carries no auth and excludes private graph-signature metadata | `polaris_sessions/src/http/mod.rs` | [HTTP Integration - Session HTTP Endpoints](docs/reference/http.md#session-http-endpoints) |
| **Inject per-turn resources** | Setup closure in `process_turn_with(\|ctx\| { ctx.insert(...); })` | `polaris_sessions/src/api.rs` | [Sessions - Turn Execution](docs/reference/sessions.md#turn-execution) |
| **Bridge caller channels to agent** | `ChannelIOProvider::new()` -> send input -> inject `UserIO` -> drain output | `polaris_sessions/src/io.rs` | [HTTP Integration - ChannelIOProvider](docs/reference/http.md#channelioprovider-bridging-a-callers-channels-to-agent-io) |
| **Understand context flow per node** | Parallel creates children; Loop shares context; Scope has 3 modes; Dynamic runs its selected candidate through a Scope-like boundary | `polaris_graph/src/executor/run.rs` | [Execution Context - Graph Flow](docs/reference/context.md#context-flow-through-graph-execution) |
| **Build a graph whose shape is known only at runtime** | Owned names and keys -> `NodeName`; pre-register `BoxedPredicate` / `BoxedDiscriminator`; assemble with `add_conditional_branch_boxed`, `add_loop_boxed`, or `add_switch_boxed`; use `.named(...)` for per-system runtime names | `polaris_graph/src/predicate.rs`, `polaris_graph/src/graph/builder.rs`, `polaris_graph/src/node.rs` | [Graph — Building Graphs from Runtime Data](docs/reference/graph.md#building-graphs-from-runtime-data) |
| **Compose an isolated subgraph without erasing its interface** | `add_scope_with_contract(name, graph, policy, contract)` — include eager `forward`/`fork` inputs in `GraphSignature`; declared scopes cannot express `forward_fresh` factory requirements | `polaris_graph/src/graph/builder.rs`, `polaris_graph/src/graph/signature.rs` | [Graph — Declared scope contracts](docs/reference/graph.md#declared-scope-contracts) |
| **Run an escalation ladder behind one dynamic slot** | `DynamicSlot::new(contract, policy).with_matching(MatchingMode::Substitutable)` — contravariant inputs, covariant outputs, so a rung consuming *less* than the contract is admitted; `Exact` is the default. On a registry-backed slot the *registry's* mode is the only gate — mirror it. Extra candidate outputs are dropped at the boundary (`validate()` warns for inline candidates) | `polaris_graph/src/graph/signature.rs`, `polaris_graph/src/node.rs`, `polaris_graph/src/registry.rs` | [Graph — Dynamic](docs/reference/graph.md#dynamic) |
| **Read one producer's `T` when several systems return `T`** | `graph.system(reader).read_from::<T>(producer_id)`, or `graph.read_from::<T>(reader_id, producer_id)` for a loop back-wire (read as `Option<Out<T>>`); `validate()` rejects wires it cannot honor | `polaris_graph/src/graph/builder.rs`, `polaris_graph/src/graph/validation.rs`, `polaris_system/src/param/read_from.rs` | [Graph — `read_from`](docs/reference/graph.md#reading-a-specific-producer-read_from) |
| **Control how many earlier loop iterations stay readable** | `set_loop_retention(loop_id, n)` / `set_loop_retention_for_type::<T>(loop_id, n)` (per-type wins); default 1, 0 keeps none; only the newest iteration leaves the loop; a failed or dropped loop discards all its iterations | `polaris_graph/src/graph/builder.rs`, `polaris_graph/src/executor/run.rs`, `polaris_system/src/param/output_layers.rs` | [Graph — Loop output retention](docs/reference/graph.md#loop-output-retention) |
| **See which node wrote each same-typed output** | `OutAll<T>` -> `ProducerInfo::of(entry.producer())` (node, name, run id, path, loop iteration; `None` for caller-seeded outputs); `ProducerKey` itself is opaque | `polaris_system/src/param/out_all.rs`, `polaris_graph/src/producer.rs` | [Context — Producer Keys](docs/reference/context.md#producer-keys) |
| **Accept caller-supplied systems without system/marker generics** | `erase(system)` at call site -> `TypedSystem<O>` helper parameter -> `graph.system_boxed(stage).with_schedules(Role)` | `polaris_system/src/system/mod.rs`, `polaris_graph/src/graph/builder.rs` | [System - Typed Erasure](docs/reference/system.md#typed-erasure-for-composition) |
| **Add middleware to graph execution** | `MiddlewareAPI::register_system()` in plugin `build()` | `polaris_graph/src/middleware/` | [Graph - Middleware](docs/reference/graph.md#middleware) |
| **Handle system errors in graph** | Fallible system + error edge + `ErrOut<CaughtError>` handler | `polaris_graph/src/executor/error.rs` | [Graph - Error Handling](docs/reference/graph.md#error-handling) |
| **Capture system parameter values for observability** | `#[system(inspect(a, b, return))]` selects at compile time; `InspectionPlugin` delivers - enable via `InspectionAPI`, listen via `Extends<InspectionSinkRegistry>`; per-deployment start policy via `InspectionPlugin::with_policy_from_env` (app-named env var; unset stays off) | `polaris_core_plugins/src/inspection.rs`, `polaris_system/src/param/inspect.rs` | [System - Parameter Inspection](docs/reference/system.md#parameter-inspection) |
| **Keep credentials out of captured values and telemetry export** | `RedactionRules` rules by binding name or type (covered values arrive `Redacted`, `Debug` never run) - `InspectionPlugin::with_redactions` at build (or `with_redactions_from_env` — adds, never drops), `InspectionAPI::add_redacted_param` / `add_redacted_type` at run time; `disable_listener(INSPECTION_TRACING_LISTENER)` cuts export alone | `polaris_core_plugins/src/inspection.rs` | [System - Parameter Inspection](docs/reference/system.md#parameter-inspection) |
| **Chain an inspection sink onto one already installed** | `ctx.inspection_arc()` -> wrap -> `ctx.replace_inspection(wrapper)`; guard re-application with `Arc::ptr_eq` on the handle you installed | `polaris_system/src/param/mod.rs` | [Execution Context - Chaining onto an Installed Sink](docs/reference/context.md#chaining-onto-an-installed-sink) |
| **Schedule plugin updates** | `tick_schedules()` + `update()` + `server.tick::<S>()` | `polaris_system/src/server.rs` | [Scheduling](docs/reference/scheduling.md) |
| **Set an output token limit** | Ceiling at registration: `AnthropicPlugin` / `BedrockPlugin::with_max_tokens` (default 4096); `LlmRequestBuilder::max_output_tokens` lowers it for one call, never raises it; OpenAI sends the request value as given | `polaris_model_providers/src/anthropic/`, `polaris_model_providers/src/bedrock/`, `polaris_models/src/llm/builder.rs` | [Model Providers — Output token limits](docs/reference/model-providers.md#output-token-limits) |
| **Price token usage** | Providers default to `PriceStore::builtin()` (no network, no disk); opt in with `with_pricing_store(PriceStore::live())`; `with_pricing_override` for one provider's rate | `polaris_model_providers/src/pricing.rs` | [Model Providers — Pricing](docs/reference/model-providers.md#pricing) |
| **Use `openai.*` or model-specific fields on Bedrock** | `BedrockPlugin` (passes the SDK config the Responses endpoint needs) + `with_additional_model_request_fields(json)` | `polaris_model_providers/src/bedrock/provider.rs` | [Model Providers — AWS Bedrock](docs/reference/model-providers.md#aws-bedrock) |

## Quick Reference: Common Modifications

| Task | Primary Files | Secondary Files |
|------|---------------|-----------------|
| Add node type | `polaris_graph/src/node.rs` | `executor.rs`, `graph.rs` |
| Add edge type | `polaris_graph/src/edge.rs` | `executor.rs`, `graph.rs` |
| Add hook schedule | `polaris_graph/src/hooks/schedule.rs` | `hooks/events.rs`, `executor.rs` |
| Add plugin | New file in `polaris_core_plugins/src/` | `polaris_core_plugins/src/lib.rs` |
| Define system | Any file with `#[system]` macro | - |
| Add resource | Plugin file | Register in `build()` |
| Add tool | `polaris_tools/src/` with `#[tool]` macro | Register in plugin via `ToolRegistry` |
| Add model provider | `polaris_model_providers/src/{provider}/` | `provider.rs`, `plugin.rs`, feature flag in `Cargo.toml` |
| Add HTTP routes | Plugin with `HttpRouter::add_routes` | `polaris_app/src/router.rs` |
| Add unit tests | Same file in `#[cfg(test)]` block | - |
| Add integration tests | `crates/*/tests/*.rs` | - |

### Adding a Node Type

1. `crates/polaris_graph/src/node.rs`: Add struct + enum variant
2. `crates/polaris_graph/src/executor.rs`: Add execution logic in `run_node()`
3. `crates/polaris_graph/src/graph.rs`: Add builder method if needed
4. Add tests in `node.rs` `#[cfg(test)]` block

### Adding an Edge Type

1. `crates/polaris_graph/src/edge.rs`: Add struct + enum variant
2. `crates/polaris_graph/src/executor.rs`: Add traversal logic
3. `crates/polaris_graph/src/graph.rs`: Add builder method if needed
4. Add tests in `edge.rs` `#[cfg(test)]` block

### Adding a Plugin

1. Create `crates/polaris_core_plugins/src/my_plugin.rs`
2. Define resource type implementing `GlobalResource` or `LocalResource`
3. Implement `Plugin` trait with `build()`, `ready()`, `cleanup()`
4. Declare capability relationships: a provider declares `provides(...)`; a consumer takes typed `build` parameters (`Requires<T>` / `Extends<T>` / `Optional<T>`). Prefer the `#[plugin]` macro, which derives `access()` from those parameters + `provides(...)` (see `docs/reference/plugins.md` → "The `#[plugin]` macro"); the macro-free form declares `access()` by hand (used when `build` also needs `&mut Server`, e.g. it inserts a global *and* extends a registry). Capability `T` types implement `Contract` for their version. Reserve `dependencies()` for pure ordering that maps to no capability, or for relationships that live only in `ready()`. The resolver verifies every declared `provides` was actually inserted. Changing a plugin's capability declarations may shift the resolved graph — regenerate the lockfile with `POLARIS_BLESS_PLUGINS_LOCK=1 cargo test -p examples --test plugins_lock` and commit `examples/plugins.lock`.
5. Add `mod my_plugin;` and re-export in `crates/polaris_core_plugins/src/lib.rs`
6. Optionally add to `DefaultPlugins` or `MinimalPlugins`
7. Add full doc comment on the plugin struct (see below)

**Plugin documentation standard** - every exported `Plugin` struct must include:

- A summary of what the plugin does and when to use it
- `# Resources Provided` - table with columns: Resource, Scope (Global / Local), Description
- `# APIs Provided` - if the plugin exposes any `API` types, list each with a description of what it enables for consumer plugins (omit section if no APIs)
- `# Dependencies` - list of required plugins, or "None"
- `# Example` - code showing registration and typical usage

Reference: `ServerInfoPlugin` in `crates/polaris_core_plugins/src/server_info.rs`

## Implementation Status

| Component | Status | Notes |
|-----------|--------|-------|
| Layer 1: System Framework | Complete | Systems, Resources, Plugins, Server |
| Layer 2: Graph Execution | Complete | All 7 node types, all 6 edge types |
| Layer 2: Agent Trait | Complete | Agent trait for pattern definition |
| Layer 3: LLM Providers | Complete | Anthropic, OpenAI, Bedrock via `polaris_model_providers` |
| Layer 3: Tool Registry | Complete | `#[tool]` / `#[toolset]` macros, `ToolRegistry`, `ToolsPlugin` |
| Layer 3: HTTP App Runtime | Complete | `polaris_app`: `AppPlugin`, `HttpRouter`, `AuthProvider` |
| Layer 3: Agent Plugins | Planned | ReAct exists, ReWOO / LLMCompiler documented |

## Rustdoc Examples

Prefer `no_run` or fully compilable examples over `ignore`. Use `ignore` only as a last resort when the snippet genuinely cannot compile in a doctest context due to external dependencies or runtime requirements.

## Documentation Guidelines

When modifying any file in `docs/**/*.md`, always check other documentation files for inconsistencies. Concepts, terminology, and examples should remain consistent across all documentation.

**Precedence order** (highest to lowest):
1. `docs/philosophy.md` - Core design principles (authoritative source of truth)
2. `docs/taxonomy.md` - Layered architecture and concept classification
3. `docs/reference/*.md` - Pattern implementations and detailed specifications

If a change affects repository navigation or coding-agent guidance, update `AGENTS.md` and any agent-specific mirrors such as `CLAUDE.md` together.

## Implementation Guidelines

When planning and implementing new features, refer to `docs/philosophy.md` and `docs/taxonomy.md` to ensure alignment with core design principles and architecture.

## Contribution Workflow

### Layer Isolation

| Layer | Crates | Rule |
|-------|--------|------|
| **1 - System Framework** | `polaris_system`, `system_macros` | Own dedicated PR. Changes affect everything. |
| **2 - Graph Execution** | `polaris_graph`, `polaris_agent` | Own dedicated PR. Changes affect all agents. |
| **3 - Plugins** | `polaris_core_plugins`, `polaris_models`, `polaris_model_providers`, `polaris_tools`, `polaris_shell`, `polaris_app` | Must not modify Layer 1 or 2. Isolated changes. |

Re-export crates (`polaris`, `polaris_internal`) are updated in the same PR as the change they re-export. PRs should touch 1 crate only (2 acceptable for wide-scale refactors).

### Branch Naming

- Use a `feat/`, `fix/`, `refactor/`, `docs/`, `chore/` prefix

### Commit Messages

Conventional commit style: `<type>: <short summary>`. Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`.

### Pull Requests

- Title: imperative, under 70 chars, typed (for example `feat: add ...`)
- Description: explain why, link the related issue
- One logical change per PR
- Tests required for features and fixes

## Ignored Files

- `temp/*` - Temporary files
- `logs/*` - Log files
- `target/*` - Build artifacts
- `data/*` - Application artifacts
- `.claude/` - Agent-tool metadata; ignore unless the task explicitly targets it
- `.cargo/` - Cargo configuration metadata
