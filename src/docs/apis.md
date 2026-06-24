A catalog of every plugin-to-plugin coordination [`API`](crate::system::api::API)
exported by `polaris-ai`.

APIs are the Layer-1 primitive plugins use to expose capabilities to other
plugins during the `build()` and `ready()` phases — the mechanism for
plugin-to-plugin coordination, complementary to resources (which carry state
into system execution). See [`api`](crate::system::api) for the trait and the
[API reference](https://docs.rs/polaris-ai/latest/polaris_ai/system/api/) for
the full primitive guide.

# How to use this catalog

Every entry below is a `#[doc(inline)]`-friendly link to the canonical rustdoc
page for that API. That page is the authoritative source for the API's
purpose, surface, lifecycle window, composition policy, and example consumers
— per the
[API Documentation Standard](https://docs.rs/polaris-ai/latest/polaris_ai/system/api/index.html#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans the workspace for `impl API for X` and
> asserts each name appears below. Adding a new API without listing it here
> will fail CI.

# Layer 2 — Graph Execution

Extension points for graph traversal and observability.

| API | Composition policy | What it enables |
|-----|--------------------|-----------------|
| [`MiddlewareAPI`](crate::graph::MiddlewareAPI) | Open extension | Register middleware that wraps system execution — instrumentation, retries, rate limiting. |
| [`HooksAPI`](crate::graph::hooks::HooksAPI) | Open extension | Register lifecycle hooks fired by the executor (`OnSystemStart`, `OnSystemComplete`, `OnGraphComplete`, etc.). |

# Layer 3 — HTTP App Runtime

Plugin-composed HTTP surface.

| API | Composition policy | What it enables |
|-----|--------------------|-----------------|
| [`HttpRouter`](crate::app::HttpRouter) | Open extension | Plugins contribute axum routes during `build()`; `AppPlugin` merges and serves them in `ready()`. |
| [`WsRouter`](crate::app::WsRouter) | Open extension | WebSocket route contributions, mirrored on `HttpRouter`. |
| [`ServerHandle`](crate::app::ServerHandle) | Provider-scoped | Bound address and shutdown signal for the running HTTP server, populated by `AppPlugin` in `ready()`. |

# Layer 3 — Sessions

Agent session lifecycle and turn execution.

| API | Composition policy | What it enables |
|-----|--------------------|-----------------|
| [`SessionsAPI`](crate::sessions::SessionsAPI) | Provider-scoped | Register agent types, create sessions, run turns, manage checkpoints. Writes flow through `SessionsPlugin`'s own machinery; consumers call its methods rather than mutating internal state directly. |

# Layer 3 — Core Infrastructure

Cross-cutting plugin coordination.

| API | Composition policy | What it enables |
|-----|--------------------|-----------------|
| [`PersistenceAPI`](crate::plugins::PersistenceAPI) | Open extension | Register serializers for `Storable` resources so session checkpoints can save/restore them. |
| [`TracingLayers`](crate::plugins::TracingLayers) | Open extension | Push `tracing_subscriber::Layer` implementations into the global subscriber. *Stored as a server resource rather than via `insert_api` — accessed through `get_resource_mut`.* |
| [`SpanProcessorRegistry`](crate::plugins::SpanProcessorRegistry) *(feature `otel`)* | Open extension | Push OpenTelemetry `SpanProcessor` implementations into the export fan-out. *A `Contract`-only registry stored as a server resource — contributed to via `Extends<SpanProcessorRegistry>` during `build()`.* |

# Layer 3 — Dashboard Snapshots

Frozen serializable snapshots exposed to external dashboard consumers via the
HTTP surface. These are queryable via `server.api::<T>()` rather than being
extended by other plugins.

| API | Composition policy | What it enables |
|-----|--------------------|-----------------|
| [`ModelsSnapshot`](crate::models::dashboard::ModelsSnapshot) *(feature `dashboard`)* | Single-replace | Frozen view of registered model providers, served at `GET /v1/models/providers`. |
| [`ToolsSnapshot`](crate::tools::dashboard::ToolsSnapshot) *(feature `dashboard`)* | Single-replace | Frozen view of registered tools, schemas, and permissions, served at `GET /v1/tools`. |

# Related

- [Plugin trait and lifecycle](crate::system) — when `insert_api` may be called
- [Plugin Catalog](crate::plugins) — which plugin provides each API
- [Resource Catalog](crate::resources) — the state-carrying counterpart to APIs
- [Integration Guide](https://docs.rs/polaris-ai/latest/polaris_ai/#common-integration-patterns) — *"how do I X?"* answers that combine plugins, APIs, and resources
