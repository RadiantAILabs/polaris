#![cfg_attr(docsrs_dep, feature(doc_cfg))]

//! A modular framework for building AI agents in Rust.
//!
//! Polaris is an ECS-inspired runtime for composing AI agents as directed
//! graphs of systems. It provides layered abstractions — from low-level
//! dependency-injected systems, through graph-based execution, up to
//! session management and HTTP serving.
//!
//! # Why Polaris
//!
//! Building performant AI agents is a design problem. The bottleneck is not
//! compute, APIs, or infrastructure — it is discovering how an agent should
//! behave for a given use case, and being able to change that behavior quickly
//! when it turns out to be wrong.
//!
//! Polaris provides composable primitives without prescribing how they should
//! be assembled. There is no default execution loop. Agent behavior is
//! constructed from small, replaceable parts, and the framework imposes no
//! opinion on the result.
//!
//! # Architecture
//!
//! Polaris is organized into three layers. Lower layers are fixed primitives;
//! upper layers are swappable.
//!
//! | Layer | Name | Modules | Scope |
//! |-------|------|---------|-------|
//! | **1** | System Framework | [`system`] | Systems, resources, plugins, server |
//! | **2** | Graph Execution | [`graph`], [`agent`] | Directed-graph model, agent trait |
//! | **3** | Plugins | [`tools`], [`models`], [`plugins`], [`sessions`], [`app`], [`shell`] | LLM providers, tools, HTTP, sessions |
//!
//! **Layer 1** provides the ECS-inspired primitives: systems as pure async
//! functions, resources as shared state, dependency injection via typed
//! parameters, plugins as the unit of composition, and the
//! [`Server`](system::server::Server) runtime.
//!
//! **Layer 2** defines how agents are structured: a directed graph of nodes
//! (computation, control flow) connected by edges (sequential, conditional,
//! parallel, looping). The [`Agent`](agent::Agent) trait packages a behavior
//! pattern as a reusable graph builder.
//!
//! **Layer 3** delivers every optional capability through plugins: LLM
//! providers, tool registries, session management, HTTP serving, and more.
//! Every component is replaceable.
//!
//! # Quick Start
//!
//! ```no_run
//! # use polaris_ai::polaris_system;
//! use polaris_ai::prelude::*;
//! use polaris_ai::system::system;
//! use polaris_ai::system::server::Server;
//! use polaris_ai::plugins::MinimalPlugins;
//! use polaris_ai::graph::GraphExecutor;
//!
//! #[system]
//! async fn greet() -> String {
//!     "Hello from Polaris!".to_string()
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut server = Server::new();
//! server.add_plugins(MinimalPlugins.build());
//! server.finish().await.unwrap();
//!
//! let graph = {
//!     let mut g = Graph::new();
//!     g.add_system(greet);
//!     g
//! };
//!
//! let mut ctx = server.create_context();
//! let executor = GraphExecutor::new();
//! let result = executor.execute(&graph, &mut ctx, None, None).await?;
//! let output = result.output::<String>();
//! # Ok(())
//! # }
//! ```
//!
//! # Core Concepts
//!
//! ## Systems and Resources
//!
//! A **system** is a pure async function that declares its dependencies as
//! typed parameters. The `#[system]` macro generates the boilerplate:
//!
//! ```no_run
//! # use polaris_ai::polaris_system;
//! # use polaris_ai::system::system;
//! # use polaris_ai::system::param::Res;
//! # use polaris_ai::system::resource::LocalResource;
//! # #[derive(Clone)] struct LlmClient;
//! # impl LocalResource for LlmClient {}
//! # #[derive(Clone)] struct Memory;
//! # impl LocalResource for Memory {}
//! # struct ReasoningResult { action: String }
//! #[system]
//! async fn reason(llm: Res<LlmClient>, memory: Res<Memory>) -> ReasoningResult {
//!     // Access resources, produce output
//!     ReasoningResult { action: "search".into() }
//! }
//! ```
//!
//! **Resources** are how agents get capabilities. An LLM provider, a tool
//! registry, a memory backend — each exists as a resource in the
//! [`SystemContext`](system::param::SystemContext):
//!
//! | Parameter | Resolution | Access | Use for |
//! |-----------|------------|--------|---------|
//! | [`Res<T>`](system::param::Res) | Hierarchy (local → parent → global) | Immutable | Config, registries, per-request input |
//! | [`ResMut<T>`](system::param::ResMut) | Current context only | Exclusive | Accumulated state (conversation history) |
//! | [`Out<T>`](system::param::Out) | Previous system output | Immutable | System-to-system data handoff |
//! | [`ErrOut<T>`](system::param::ErrOut) | Error-edge output | Immutable | Error context in handler subgraphs |
//!
//! ## Graphs
//!
//! Agent logic is expressed as a directed graph of systems and control flow:
//!
//! ```no_run
//! # use polaris_ai::graph::Graph;
//! # struct ReasoningResult { needs_tool: bool }
//! # async fn reason() -> ReasoningResult { ReasoningResult { needs_tool: false } }
//! # async fn execute_tool() {}
//! # async fn respond() {}
//! let mut graph = Graph::new();
//! graph
//!     .add_system(reason)
//!     .add_conditional_branch::<ReasoningResult, _, _, _>(
//!         "needs_tool",
//!         |r| r.needs_tool,
//!         |g| { g.add_system(execute_tool); },
//!         |g| { g.add_system(respond); },
//!     );
//! ```
//!
//! **Node types:** System, Decision, Switch, Parallel, Loop, Scope.
//! **Edge types:** `Sequential`, `Conditional`, `Parallel`, `LoopBack`, `Error`, `Timeout`.
//!
//! The graph's full topology is inspectable, validated before execution, and
//! restructured by rewiring edges. See the [`graph`] module for the full API.
//!
//! ## Plugins
//!
//! Every capability is delivered through plugins registered at startup:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use polaris_ai::system::server::Server;
//! # use polaris_ai::plugins::{DefaultPlugins, MinimalPlugins};
//! # use polaris_ai::tools::ToolsPlugin;
//! # use polaris_ai::sessions::{SessionsPlugin, store::memory::InMemoryStore};
//! # use polaris_ai::system::plugin::PluginGroup;
//! let mut server = Server::new();
//! server
//!     .add_plugins(DefaultPlugins::new().build())
//!     .add_plugins(ToolsPlugin)
//!     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
//! ```
//!
//! Plugins have a lifecycle: `build()` → `ready()` → `update()` → `cleanup()`.
//! Dependencies are declared and resolved automatically. See the [`system`]
//! module for the `Plugin` trait and the [`plugins`] module for built-in
//! plugin groups.
//!
//! # Data Flow Patterns
//!
//! Choosing the right parameter type is critical for correct data flow:
//!
//! | Pattern | Use | Avoid |
//! |---------|-----|-------|
//! | Step A's result feeds step B | `Out<T>` — A returns `T`, B declares `Out<T>` | `ResMut<SharedState>` with `Option` fields |
//! | Immutable per-request input | `Res<T>` via `ctx.insert(T)` in setup closure | `ResMut<WorkingState>` with `.input.clone()` |
//! | Accumulated state (history, counters) | `ResMut<T>` — local resource | `Out<T>` — outputs are per-system |
//! | Shared server-wide config | `Res<T>` — global resource | `ResMut<T>` — compile error on globals |
//! | Error context in handler | `ErrOut<CaughtError>` | Custom `ResMut<LastError>` |
//!
//! ## Data Lifetimes
//!
//! | Data lives… | Mechanism |
//! |-------------|-----------|
//! | Server lifetime | `GlobalResource` + `Res<T>` |
//! | Session lifetime | `LocalResource` inserted at session creation |
//! | Single turn | `LocalResource` inserted in `process_turn_with` |
//! | Between two systems | Return value + `Out<T>` |
//! | Error handler subgraph | `ErrOut<CaughtError>` |
//!
//! # Common Integration Patterns
//!
//! | Goal | Pattern | Entry point |
//! |------|---------|-------------|
//! | Run one-shot agent | `sessions.run_oneshot(&agent_type, \|ctx\| { ... })` | [`sessions`] |
//! | Multi-turn with cleanup | `sessions.scoped_session(&agent_type, \|ctx\| { ... })` | [`sessions`] |
//! | Execute agent from HTTP | `HttpRouter::add_routes_with` → `SessionsAPI` → `HttpIOProvider` | [`app`] |
//! | Register HTTP routes | `server.api::<HttpRouter>().add_routes(router)` (stateless) / `add_routes_with(\|server\| ...)` (stateful) | [`app`] |
//! | Add tools for LLM | `#[tool]` macro + `ToolRegistry` | [`tools`] |
//! | Add model provider | Implement `LlmProvider` + register via plugin | [`models`] |
//! | Handle system errors | Fallible system + error edge + `ErrOut<CaughtError>` | [`graph`] |
//! | Schedule plugin updates | `tick_schedules()` + `server.tick::<S>()` | [`system`] |
//!
//! # Crate Organisation
//!
//! | Module | Crate | Purpose |
//! |--------|-------|---------|
//! | [`system`] | `polaris_system` | ECS-inspired systems, resources, and plugins |
//! | [`graph`] | `polaris_graph` | Directed-graph execution primitives |
//! | [`agent`] | `polaris_agent` | Agent trait for reusable behavior patterns |
//! | [`tools`] | `polaris_tools` | Tool framework for LLM-callable functions |
//! | [`models`] | `polaris_models` / `polaris_model_providers` | Model registry and provider implementations |
//! | [`plugins`] | `polaris_core_plugins` | Core infrastructure plugins (time, tracing, persistence) |
//! | [`sessions`] | `polaris_sessions` | Session management and orchestration |
//! | [`shell`] | `polaris_shell` | Shell command execution with permission model |
//! | [`app`] | `polaris_app` | HTTP server runtime with plugin integration |
//!
//! # Exploration Map
//!
//! | If you want to find… | Start here |
//! |----------------------|------------|
//! | System primitives, resources, and plugin lifecycle | [`system`] |
//! | Graph nodes, edges, execution, hooks, and middleware | [`graph`] |
//! | LLM providers and provider plugins | [`models`] |
//! | Core infrastructure plugins and observability | [`plugins`] |
//! | Session lifecycle, persistence, and HTTP session routes | [`sessions`] |
//! | Feature-gated exports and which module owns them | [Feature Export Map](#feature-export-map) |
//!
//! # Feature Flags
//!
//! Every workspace feature that downstream consumers might want is surfaced
//! through `polaris-ai` directly, so consumers don't need to depend on the
//! inner crates (`polaris_app`, `polaris_core_plugins`, etc.). All features
//! are opt-in **except `file-store`**, which is on by default to preserve the
//! historical session-store behavior — set `default-features = false` to opt
//! out. Features that would otherwise be ambiguous at the top level are
//! prefixed with the sub-crate's short name (e.g. `sessions-http`); features
//! that are already unambiguous keep their original name (e.g. `anthropic`).
//!
//! ## Model Providers
//!
//! | Feature | Exported item | Find it under |
//! |---------|---------------|---------------|
//! | `anthropic` | [`models::AnthropicPlugin`] | [`models`] |
//! | `openai` | [`models::OpenAiPlugin`] | [`models`] |
//! | `bedrock` | [`models::BedrockPlugin`] | [`models`] |
//!
//! ## Observability
//!
//! Graph, model, and tool tracing are always on — [`plugins::TracingPlugin`]
//! unconditionally registers graph middleware and decorates the global
//! [`models::ModelRegistry`] and [`tools::ToolRegistry`]. With no subscriber
//! attached the spans have no observable cost.
//!
//! | Feature | Exported item | Effect |
//! |---------|---------------|--------|
//! | `otel` | [`plugins::OpenTelemetryPlugin`] | Adds OTLP export via the tracing subscriber and switches HTTP request spans in [`app`] to `OTel` HTTP semantic-convention field names |
//! | `dashboard` | [`tools::dashboard::ToolsSnapshot`], [`models::dashboard::ModelsSnapshot`] | Mounts endpoints — `GET /v1/tools` on [`tools::ToolsPlugin`] and `GET /v1/models/providers` on [`models::ModelsPlugin`] — for an external dashboard frontend |
//!
//! ## Tokenization
//!
//! | Feature | Exported item | Effect |
//! |---------|---------------|--------|
//! | `tiktoken` | [`models::tokenizer::TiktokenCounter`] and [`models::tokenizer::EncodingFamily`] | Enables tiktoken-backed counting and [`models::TokenizerPlugin::default`] |
//!
//! ## Sessions
//!
//! | Feature | Exported item | Find it under |
//! |---------|---------------|---------------|
//! | `sessions-http` | [`sessions::HttpPlugin`] and [`sessions::http`] | [`sessions`] |
//! | `file-store` *(default)* | [`sessions::FileStore`] | [`sessions`] |
//!
//! ## Testing
//!
//! | Feature | Exported item | Effect |
//! |---------|---------------|--------|
//! | `test-utils` | [`plugins::MockClock`], [`plugins::MockIOProvider`] | Mocks for clock and user IO; intended for downstream `dev-dependencies` |
//!
//! ## Feature Coverage Map
//!
//! Use this table when the question is “what does feature `X` expose,
//! modify, or wire up at runtime?”
//!
//! | Feature | Adds public items | Also changes | Runtime surface |
//! |---------|-------------------|--------------|-----------------|
//! | `anthropic` | [`models::anthropic`], [`models::AnthropicPlugin`] | Makes the `anthropic/...` provider family available through [`models::ModelRegistry`] once registered | [`models::AnthropicPlugin`] registers the Anthropic provider |
//! | `openai` | [`models::openai`], [`models::OpenAiPlugin`] | Makes the `openai/...` provider family available through [`models::ModelRegistry`] once registered | [`models::OpenAiPlugin`] registers the `OpenAI` provider |
//! | `bedrock` | [`models::bedrock`], [`models::BedrockPlugin`] | Makes the `bedrock/...` provider family available through [`models::ModelRegistry`] once registered | [`models::BedrockPlugin`] registers the Bedrock provider |
//! | `otel` | [`plugins::OpenTelemetryPlugin`] | Integrates with [`plugins::TracingPlugin`] / [`plugins::TracingLayers`] and switches HTTP request spans in [`app`] to `OTel` HTTP semantic-convention field names | [`plugins::OpenTelemetryPlugin`] pushes an OTLP export layer into the tracing subscriber; [`app::AppPlugin`] emits spans with `http.request.method` / `url.path` / `http.response.status_code` fields (plus `otel.name` / `otel.kind`) instead of the `polaris.http.*` defaults. With [`plugins::OpenTelemetryPlugin`] installed, it also extracts incoming W3C `traceparent` headers to continue a distributed trace; requests with no upstream context start a fresh trace. |
//! | `tiktoken` | [`models::tokenizer::TiktokenCounter`], [`models::tokenizer::EncodingFamily`] | Adds [`Default`] for [`models::TokenizerPlugin`] and changes what [`models::TokenizerPlugin::default`] builds | [`models::TokenizerPlugin::default`] registers a global [`models::Tokenizer`] backed by [`models::tokenizer::TiktokenCounter`] |
//! | `sessions-http` | [`sessions::http`], [`sessions::HttpPlugin`], [`sessions::http::models`] | Adds request/response model types and HTTP-facing session APIs under [`sessions`] | [`sessions::HttpPlugin`] registers routes through [`app::HttpRouter`] and depends on [`app::AppPlugin`] + [`sessions::SessionsPlugin`] |
//! | `file-store` *(default)* | [`sessions::FileStore`] | Pulls `tokio/fs` into the dep graph | Lets [`sessions::SessionsPlugin`] use a filesystem-backed [`sessions::SessionStore`] |
//! | `dashboard` | [`tools::dashboard::ToolsSnapshot`], [`models::dashboard::ModelsSnapshot`] | Activates the per-crate `dashboard` features on [`tools::ToolsPlugin`] and [`models::ModelsPlugin`] so each host plugin mounts its dashboard HTTP surface | [`tools::ToolsPlugin`] mounts a frozen-snapshot `/v1/tools` endpoint, and [`models::ModelsPlugin`] mounts `/v1/models/providers` |
//! | `typegen` | None at runtime | Enables `ts-rs` derives on canonical session wire types (`SessionMetadata`, `SessionStatus`, …) | Running `cargo test --features typegen` regenerates the TypeScript bindings under `bindings/ts/src/` |
//! | `test-utils` | [`plugins::MockClock`], [`plugins::MockIOProvider`] | None at runtime | Provides mocks for downstream test suites that exercise [`plugins::TimePlugin`] / [`plugins::IOProvider`] |

// Re-export crates under their original names so proc-macro-generated code
// can resolve `polaris::polaris_tools`, `polaris::polaris_system`, etc.
#[doc(hidden)]
pub use polaris_internal::polaris_core_plugins;
#[doc(hidden)]
pub use polaris_internal::polaris_models;
#[doc(hidden)]
pub use polaris_internal::polaris_system;
#[doc(hidden)]
pub use polaris_internal::polaris_tools;

/// Re-export all common types for easy access.
///
/// # Examples
///
/// ```
/// use polaris_ai::prelude::*;
///
/// let graph = Graph::new();
/// ```
pub mod prelude {
    pub use polaris_internal::prelude::*;
}

#[doc = include_str!("docs/system.md")]
pub mod system {
    #[doc(inline)]
    pub use polaris_internal::system::*;
}

#[doc = include_str!("docs/graph.md")]
pub mod graph {
    #[doc(inline)]
    pub use polaris_internal::graph::*;
}

#[doc = include_str!("docs/agent.md")]
pub mod agent {
    #[doc(inline)]
    pub use polaris_internal::agent::*;
}

#[doc = include_str!("docs/tools.md")]
pub mod tools {
    #[doc(inline)]
    pub use polaris_internal::tools::*;
}

#[doc = include_str!("docs/models.md")]
pub mod models {
    #[doc(inline)]
    pub use polaris_internal::models::*;
}

#[doc = include_str!("docs/plugins.md")]
pub mod plugins {
    #[doc(inline)]
    pub use polaris_internal::plugins::*;
}

#[doc = include_str!("docs/sessions.md")]
pub mod sessions {
    #[doc(inline)]
    pub use polaris_internal::sessions::*;
}

#[doc = include_str!("docs/shell.md")]
pub mod shell {
    #[doc(inline)]
    pub use polaris_internal::shell::*;
}

#[doc = include_str!("docs/app.md")]
pub mod app {
    #[doc(inline)]
    pub use polaris_internal::app::*;
}

#[doc = include_str!("docs/apis.md")]
pub mod apis {}

#[doc = include_str!("docs/resources.md")]
pub mod resources {}

/// Contributor-facing reference docs included here purely so the rustdoc gate
/// (`cargo doc -D warnings`) validates their `crate::…` intra-doc links against
/// this crate's module tree. Hidden from the rendered API docs — these are
/// authored under `docs/reference/` and are not part of the public surface.
#[doc(hidden)]
pub mod _reference_doc_link_check {
    #[doc = include_str!("../docs/reference/guide.md")]
    pub mod guide {}

    #[doc = include_str!("../docs/reference/resources.md")]
    pub mod resources {}
}
