A catalog of every consumer-facing
[`GlobalResource`](crate::system::resource::GlobalResource) and
[`LocalResource`](crate::system::resource::LocalResource) exported by
`polaris-ai`.

Resources are how the framework hands capabilities to user code. A system
written as `fn my_system(x: Res<T>)` is asking *"someone please put a `T` in
my context"* — these entries list every `T` the workspace ships, who puts it
there, and how it should be consumed.

See [`resource`](crate::system::resource) for the trait, and the
[Resources reference](https://docs.rs/polaris-ai/latest/polaris_ai/system/resource/)
for the concept and documentation standard.

# How to use this catalog

Each entry below links to the canonical rustdoc page for that resource. That
page is the authoritative source for the resource's purpose, scope choice,
provider plugin, access pattern, alternatives, and example consumer system —
per the
[Resource Documentation Standard](https://docs.rs/polaris-ai/latest/polaris_ai/system/resource/index.html#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/resource_catalog.rs`) that scans the workspace for `impl
> GlobalResource for X` / `impl LocalResource for X` and asserts each
> consumer-facing name appears below. Resources marked `#[doc(hidden)]` or
> kept non-`pub` are exempt — those are internal plugin state, not part of the
> consumer surface.

# Layer 2 — Graph Execution

Per-execution metadata exposed to systems and middleware.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`SystemInfo`](crate::graph::SystemInfo) | Local | Inspect the current node — name, system type, retry attempt, etc. Provided by [`DevToolsPlugin`](crate::graph::DevToolsPlugin); used by tracing, logging, and custom instrumentation systems. |

# Layer 3 — Models

LLM access and tokenization.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`ModelRegistry`](crate::models::ModelRegistry) | Global | Look up an [`LlmProvider`](crate::models::llm::LlmProvider) by `provider/model` key. Provided by [`ModelsPlugin`](crate::models::ModelsPlugin); typical consumers are the systems that turn user input into LLM requests. |
| [`Tokenizer`](crate::models::Tokenizer) | Global | Count tokens for prompt budgeting and cost estimation. Provided by [`TokenizerPlugin`](crate::models::TokenizerPlugin) (backed by tiktoken under feature `tiktoken`). |

# Layer 3 — Tools

Function-calling registry.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`ToolRegistry`](crate::tools::ToolRegistry) | Global | Look up, schema, and invoke `#[tool]` / `#[toolset]` definitions. Provided by [`ToolsPlugin`](crate::tools::ToolsPlugin); consumed by the tool-dispatch systems inside agent graphs. |

# Layer 3 — Sessions

Per-session and per-turn context.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`SessionInfo`](crate::sessions::SessionInfo) | Local | The current session's identity (id, agent type) — readable by any system inside a session-driven turn. Provided by [`SessionsPlugin`](crate::sessions::SessionsPlugin) when it creates the context. |

# Layer 3 — HTTP App Runtime

Per-request context flowing from the HTTP boundary into agent systems.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`AppConfig`](crate::app::AppConfig) | Global | HTTP server configuration (bind address, etc.). Provided by [`AppPlugin`](crate::app::AppPlugin). |
| [`RequestContext`](crate::app::RequestContext) | Local | Per-request identity and metadata propagated from the HTTP layer into the agent context. Provided by [`RequestContextPlugin`](crate::app::RequestContextPlugin) (inserted on each turn driven by an HTTP request). |
| [`HttpHeaders`](crate::app::HttpHeaders) | Local | The original HTTP request headers, available to systems that need to inspect non-identity headers (correlation IDs, content negotiation, etc.). Provided by [`RequestContextPlugin`](crate::app::RequestContextPlugin). |

# Layer 3 — Core Infrastructure

Cross-cutting capabilities every agent typically needs.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`ServerInfo`](crate::plugins::ServerInfo) | Global | Framework version and debug-mode flag. Provided by [`ServerInfoPlugin`](crate::plugins::ServerInfoPlugin). |
| [`Clock`](crate::plugins::Clock) | Global | Wall-clock time. Provided by [`TimePlugin`](crate::plugins::TimePlugin); substitutable with `MockClock` under feature `test-utils`. |
| [`Stopwatch`](crate::plugins::Stopwatch) | Local | Per-turn elapsed-time tracking. Provided by [`TimePlugin`](crate::plugins::TimePlugin). |
| [`TracingConfig`](crate::plugins::TracingConfig) | Global | Current tracing log level (and, with the `dashboard` feature, the span-buffer capacity). Provided by [`TracingPlugin`](crate::plugins::TracingPlugin); consumed mostly by other plugins, occasionally by systems that adapt output verbosity. |
| [`UserIO`](crate::plugins::UserIO) | Local | Per-turn bidirectional I/O channel (used by interactive agents and the HTTP/SSE bridge). Provided by [`IOProvider`](crate::plugins::IOProvider) implementations — `StdioIOProvider` for CLIs, `HttpIOProvider` for HTTP, `MockIOProvider` under `test-utils`. |

# Layer 3 — Shell

Shell command execution.

| Resource | Scope | What systems use it for |
|----------|-------|-------------------------|
| [`ShellExecutor`](crate::shell::ShellExecutor) | Global | Execute shell commands behind the [`ShellPermission`](crate::shell::ShellPermission) gate. Provided by [`ShellPlugin`](crate::shell::ShellPlugin). |

# Related

- [Resources reference](https://docs.rs/polaris-ai/latest/polaris_ai/system/resource/) — the trait, the two scopes, and the documentation standard
- [Data Flow Patterns](https://docs.rs/polaris-ai/latest/polaris_ai/) — when to reach for `Res<T>` vs `ResMut<T>` vs `Out<T>` (the `Data Flow Patterns` section of the crate docs)
- [API Catalog](crate::apis) — the plugin-coordination counterpart to resources
- [Plugin Catalog](crate::plugins) — which plugin provides each resource
- [Integration Guide](https://docs.rs/polaris-ai/latest/polaris_ai/#common-integration-patterns) — *"how do I X?"* answers that combine plugins, APIs, and resources
