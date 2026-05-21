# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] - 2026-05-20

### Added

- **Tracing-known sessions endpoint** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — `GET /v1/tracing/sessions?limit=N`, backed by the new `SpanBuffer::distinct_sessions(limit)` query and `SessionSummary { session_id, agent_name?, run_count, started_at?, last_seen_at? }` wire type. Decouples session discoverability from `SessionStore` membership: sessions removed from the store — including ephemeral one-shots reclaimed by `SessionsAPI::run_oneshot` — remain reachable via `/v1/tracing/sessions` as long as their spans are in the buffer. Pairs with `SpanStorePlugin` to extend the surface across process restarts. New TS binding `SessionSummary` exported via `bindings/ts/src/index.ts`. Contract pinned by `polaris_sessions::tests::run_oneshot_tracing_survival`.

- **`SpanStorePlugin`** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — durable companion to `TracingPlugin`'s span buffer. Persists every closed `SpanRecord` carrying a `session_id` label through a pluggable `SpanStore` trait, then hydrates the in-memory `SpanBuffer` on `ready()` so queries against a resumed session return non-empty immediately after boot. Without it, `SessionStore` resumes a session but the tracing surface reports zero runs because the buffer was wiped at process exit. The plugin installs its own `RecordingLayer` alongside the buffer layer — buffer writes and store writes are independent, so an unreachable store does not stall the in-memory pipeline. Two backends ship in-tree: `InMemorySpanStore` (default for tests; the trait surface without touching disk) and `FileSpanStore` (feature-gated on `file-store`; one JSON-lines file per session at `<base_dir>/<session_id>.jsonl`, append-only, recoverable from a partial trailing line). Custom backends (Postgres, S3, …) implement `SpanStore` directly. Records without a `session_id` label are dropped on the storage path. Coexists with the `TracingPlugin` dashboard surface (hydrates its buffer) and `OpenTelemetryPlugin` (independent layer). Re-exposes `SpanStoreHandle` as an API for downstream plugins that want direct store access. Contract pinned by `span_store_standalone`, `span_store_live_subscriber`, and `span_store_cross_restart` integration tests.

- **`SessionStatus::ReadOnly` + `SessionsAPI::run_oneshot_preserved`** (`polaris_sessions`) — new terminal session state. `run_oneshot_preserved` finalizes a one-shot turn while keeping the session record alive for read-only inspection (turn history, metadata, persistence); any mutating method returns `SessionError::ReadOnly`. Wire value: `"read_only"` on `SessionStatus`. Pairs with the new `GET /v1/sessions/{id}/turns` history endpoints so an operator can audit a completed run without keeping the agent mutable. Typegen bindings for `SessionStatus` regenerated.

- **Sessions HTTP endpoints** (`polaris_sessions`, `http` feature) — four new endpoints exposing the sessions surface:
  - `GET /v1/sessions/agent-types` — enumerates registered agent types. Response: `ListAgentTypesResponse { items: Vec<AgentTypeSummary> }`.
  - `GET /v1/sessions/{id}/turns` — per-session turn summaries with `?include=messages` opt-in to embed full IOMessage arrays for short sessions. Response: `ListTurnsResponse { items: Vec<TurnSummary> }`.
  - `GET /v1/sessions/{id}/turns/{n}` — full per-turn payload. Response: `Turn { turn, started_at, finished_at?, status, messages: Vec<IOMessage> }`.
  - `GET /v1/sessions/{id}/uptime?bucket=&since=&until=` — bucketed lifecycle time-series. `?bucket=` is a fixed enum (`1m`/`5m`/`15m`/`1h`, default `1m`); unknown values return 400. `?since=` / `?until=` are ISO 8601 with a 24h default range. Response: `SessionUptimeResponse { bucket, since, until, buckets: Vec<SessionUptimeBucket> }`.

- **Session lifecycle recorder** (`polaris_sessions::uptime`) — in-memory per-session transition log that backs the `sessions-uptime` endpoint. Records `Created`/`Active`/`Idle`/`Terminated` with timestamps; bucketing happens server-side at query time. No persistence in this release; entries are bounded and recycle as sessions terminate.

- **Tracing run inspection endpoints** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — three new endpoints over the existing `SpanBuffer`:
  - `GET /v1/tracing/runs?limit=N` — distinct `run_id`s observed in the ring buffer with `RunSummary { run_id, agent_name?, started_at, duration_ms?, outcome?, input_tokens, output_tokens }` per row.
  - `GET /v1/tracing/runs/{run_id}` — hierarchical `SpanTree` for one run. Embeds span event payloads by default; `?include=structure` returns tree shape + span metadata without payloads (paired with the per-span lookup below for on-demand payload fetch).
  - `GET /v1/tracing/runs/{run_id}/spans/{span_id}` — single-span payload lookup, used by frontends running in structure-only mode.

- **Token usage rollup endpoints** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — four new endpoints that aggregate the OpenTelemetry `GenAI` token-count attributes (`gen_ai.usage.input_tokens` / `output_tokens`) already recorded by the LLM tracing instrumentation. No new storage — totals derive from `SpanBuffer` on demand. Endpoints respond with `TokenUsageResponse { totals, by_model, by_provider, by_agent_type, source_span_count }`; breakdowns are sorted by descending `total_tokens`, and records missing an attribute attribute to the literal key `"unknown"`.
  - `GET /v1/tracing/usage[?label=key:value]` — buffer-wide totals, optionally filtered by correlation label.
  - `GET /v1/tracing/runs/{run_id}/usage` — per-run totals; 404 when the run is not in the buffer.
  - `GET /v1/sessions/{session_id}/usage` — per-session totals summed across every run still in the buffer; zeroed body (not 404) when the session has no LLM activity.
  - `GET /v1/sessions/{session_id}/runs/{run_id}/usage` — per-run totals, gated on session membership; 404 on cross-session lookup.

- **`UsagePricing` API** (`polaris_core_plugins`, `dashboard` feature) — opt-in per-`(provider, model)` pricing table consulted by the usage endpoints. `TracingPlugin` registers an empty `UsagePricing` API when built with the `dashboard` feature; consumer plugins populate it from their own `build()` via `server.api::<UsagePricing>().set(provider, model, ModelPricing::new(input_per_million_usd, output_per_million_usd))`. `ModelPricing` is `#[non_exhaustive]` and constructed through `ModelPricing::new` so future rate tiers do not break callers. With at least one rate registered, the aggregator multiplies matching buckets' tokens by the per-million-token rate and surfaces `cost_usd` on both totals and breakdown rows. Empty by default → `cost_usd` stays `null` end-to-end. Pricing is consumer-owned because provider rate cards change too quickly to maintain in-tree.

- **`LlmProvider::pricing()`** (`polaris_models`) — defaulted trait method that lets a provider publish per-million-token rates alongside its token usage. `TracingLlmProvider` reads the rate at call time and records the estimated cost as `gen_ai.usage.cost_usd` on chat spans, so cost surfaces in any OTel waterfall and on per-system breakdowns without round-tripping through the aggregator. Anthropic and OpenAI providers ship with Claude 4.x / GPT-5.x list prices; Bedrock and custom providers inherit the `None` default. `UsagePricing` remains the consumer-side override at aggregation time.

- **`RunSummary` token and cost totals** (`polaris_core_plugins::tracing_plugin`) — `RunSummary` now carries `input_tokens`, `output_tokens`, and `cost_usd`, summed across every record observed for the run. The aggregation reads the same `gen_ai.usage.input_tokens` / `output_tokens` / `cost_usd` fields the buffer-wide usage endpoint consumes, so the run summary agrees with `/v1/tracing/runs/{id}/usage`. Records without those fields contribute zero — a run with no LLM calls (or with a provider that hasn't declared pricing) reports `0/0/0.0`. Typegen bindings regenerated.

- **`SpanBuffer` query methods** — `distinct_runs(limit)`, `distinct_sessions(limit)`, `run_tree(run_id, view)`, `span(run_id, span_id)`, and the new `aggregate_usage(pricing)` / `aggregate_usage_for_run(run_id, pricing)` / `aggregate_usage_by_label(key, value, pricing)` for the tracing endpoints. All queries are O(buffer_size); aged-out runs return 404 / `None`.

- **`TreeView` enum** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — selects how much of a run `SpanBuffer::run_tree` materializes. `TreeView::Payloads` embeds event records (the historical default); `TreeView::Structure` returns tree shape and span metadata only, leaving payloads to be fetched lazily via `SpanBuffer::span`. Exported from `polaris_core_plugins` alongside `SpanTree`.

- **`TracingPlugin::pretty()` / `TracingPlugin::quiet()`** (`polaris_core_plugins`) — named constructors that make the default-vs-fmt trade-off explicit. `pretty()` attaches `FmtConfig::default()` so the subscriber emits human-readable console output at `INFO`; `quiet()` mirrors `Default::default()` / `new()` and attaches no `fmt` layer. Reach for `pretty()` when you want output out of the box, `quiet()` when another plugin (e.g. `OpenTelemetryPlugin`) is the only layer you want active. `DefaultPlugins` already installs the `pretty()` variant.

- **`Deserialize` on session HTTP wire types and the `IOMessage` family** (`polaris_sessions::http`, `polaris_core_plugins`) — `AgentTypeSummary`, `ListAgentTypesResponse`, `TurnStatus`, `TurnSummary`, `Turn`, and `ListTurnsResponse` now derive `Deserialize` in addition to `Serialize`, with the cascade extended through `IOSource`, `IOContent`, and `IOMessage`. Typed Rust clients can decode these payloads directly instead of round-tripping through `serde_json::Value`.

- **`TracingPlugin::default_dependencies()`** (`polaris_core_plugins`) — `TracingPlugin` is now the first in-tree consumer of the `Plugin::default_dependencies` mechanism. Adding it to a server without first registering `ServerInfoPlugin`, `ModelsPlugin`, or `ToolsPlugin` now auto-registers the missing ones (announced via `tracing::info!`) instead of panicking at `finish()`. `AppPlugin` is intentionally excluded because it requires explicit host/port configuration. Explicit registrations always win.

- **`FileSpanStore::load` size cap + `FileSpanStoreError::TooLarge`** (`polaris_core_plugins::tracing_plugin`, `file-store` feature) — `load` stats the session file before reading it and returns `SpanStoreError::Backend(FileSpanStoreError::TooLarge { path, size, limit })` when the file exceeds 64 MiB. The plugin is the only writer for a session file, so an oversize file is corrupt or hostile; refusing to read it caps hydration memory at a known bound instead of allocating proportional to file size.

- **`Server::expect_api`** (`polaris_system`) — thin wrapper around `Server::api` that emits a `tracing::warn!` with the API type name and a free-text purpose when the lookup misses, without changing the runtime semantics. Callers still receive `Option<A>` and decide how to handle absence. `SessionsPlugin::ready()` now uses it for its `HooksAPI` / `MiddlewareAPI` captures so a missing graph-instrumentation API surfaces a diagnostic instead of silently disabling span emission.

- **`Plugin::default_dependencies()`** (`polaris_system`) — a plugin can declare default instances for the dependencies it requires. During `Server::finish()` — before dependency validation — the server auto-registers any declared dependency the user did not provide explicitly, recursing into the defaulted plugin's own defaults. Explicit registrations always win; applied defaults are announced via `tracing::info!`. Dependencies without a default still panic when absent, and the panic now lists every missing dependency in one pass instead of one-at-a-time.

- **New TypeScript bindings** (`bindings/ts/`, `typegen` feature) — `AgentTypeSummary`, `BucketGranularity`, `ListAgentTypesResponse`, `ListTurnsResponse`, `RunSummary`, `SessionSummary`, `SessionUptimeBucket`, `SessionUptimeResponse`, `SpanEvent`, `SpanKind`, `SpanNode`, `SpanRecord`, `SpanTree`, `TokenUsageBreakdown`, `TokenUsageResponse`, `TokenUsageTotals`, `Turn`, `TurnStatus`, `TurnSummary`, `UptimeStatus`. All re-exported from `bindings/ts/src/index.ts`.

### Changed

- **`tracing_plugin` module layout** — the monolithic `span_buffer.rs` (≈1.5k lines mixing wire types, the in-memory ring, and the tracing-subscriber layer) has been split into three modules with disjoint responsibilities so the new `SpanStorePlugin` can reuse the wire types and the sink trait without dragging in the dashboard's ring buffer: `span_record` (always-on `SpanRecord` / `SpanKind` wire types), `buffer` (`dashboard`-gated `SpanBuffer` and its query projections — `RunSummary`, `SpanTree`, `SpanNode`, `SpanEvent`), and `capture` (`dashboard`-gated `RecordingLayer` + `SpanRecordSink` trait — the tracing-subscriber boundary that both the buffer and the store implement). The four instrumentation files (`genai_content`, `graph_middleware`, `llm_decorator`, `tool_decorator`) move under `tracing_plugin::instrument/` and are renamed to drop the `_decorator` / `_middleware` suffixes (`graph`, `llm`, `tool`, `genai_content`). **Breaking**: `SpanBufferLayer` is renamed to `RecordingLayer` and is now constructed via `RecordingLayer::new(buffer)` (buffer sink) or `RecordingLayer::with_sink(sink)` (custom sink); the type is exported from `polaris_core_plugins` and the `polaris-ai` umbrella. Wire types `SpanRecord` and `SpanKind` are now exported unconditionally (previously dashboard-gated) so the `file-store` feature can compile without pulling axum into the dep graph.

- **BREAKING — `TracingLayersApi` renamed to `TracingLayers`** (`polaris_core_plugins`) — the build-time layer registry is registered via `Server::insert_resource` and accessed via `get_resource_mut`, not via the `API` machinery, so the `Api` suffix was misleading. The type is now exported from `polaris_core_plugins` as `TracingLayers`. Migration: rename every `TracingLayersApi` reference in your imports, plugin `build()` bodies, and docs. Behavior is unchanged.

- **BREAKING — `SpanBuffer::run_tree` signature** (`polaris_core_plugins::tracing_plugin`, `dashboard` feature) — the second parameter changed from `bool` to the new `TreeView` enum. Migration: `buffer.run_tree(id, true)` → `buffer.run_tree(id, TreeView::Payloads)`, `buffer.run_tree(id, false)` → `buffer.run_tree(id, TreeView::Structure)`. The HTTP route contract is unchanged — `?include=structure` still selects the structure-only view; only the in-process Rust call site is affected.

### Fixed

- **Default `RequestContext` trace IDs collide on coarse clocks** (`polaris_app`) — `SystemTime::now()` isn't nanosecond-precise on macOS, so two back-to-back `RequestContext::default()` calls on the same thread could produce identical trace IDs. A process-wide atomic counter now guarantees uniqueness.

### Removed

- **`polaris_dashboard` crate** — the registry-only Layer 3 crate introduced in v0.4.0 has been removed. The HTTP surfaces previously gated behind it are now folded directly into their host plugins (`TracingPlugin`, `ToolsPlugin`, `ModelsPlugin`, and the sessions HTTP plugin), each exposing a single `dashboard` Cargo feature that enables its HTTP endpoints. Consumers that depended on `polaris_dashboard::{DashboardRegistry, DashboardPlugin, NavItem, Section, Panel, Transport, Manifest, RegistryEvent}` must migrate to the per-plugin HTTP surfaces directly; there is no in-core replacement for cross-plugin nav/panel descriptors. The `polaris_ai::dashboard` re-export and the `GET /v1/dashboard/manifest` endpoint are gone with the crate.

- **`SessionsDashboardPlugin`, `ToolsDashboardPlugin`, `ModelsDashboardPlugin`, `TracingDashboardPlugin`** — the standalone dashboard plugins introduced in v0.4.0 have been folded into their host plugins. `TracingPlugin` now mounts the tracing dashboard endpoints when built with the `dashboard` feature; the tools, models, and sessions HTTP surfaces do the same for their respective endpoints. Migration: register the host plugin and enable its `dashboard` feature instead of registering a separate `*DashboardPlugin`.

- **`dashboard-registry` Cargo feature, and the fine-grained `*-tracing` / `*-dashboard` / `serde` / `ws` flags** (`polaris-ai`, `polaris_internal`) — replaced by a single `dashboard` umbrella feature on the top-level crate that activates the `dashboard` feature on each host plugin. `polaris_app`'s `WsRouter` is now always available (no `ws` feature gate). Migration: drop the old feature names; add `dashboard` if you want the HTTP surfaces.

- **TypeScript bindings `Manifest`, `NavItem`, `Section`, `Panel`, `Transport`** — the manifest wire types are gone with `DashboardRegistry`. Files removed from `bindings/ts/src/` and dropped from `index.ts`.

- **`docs/reference/dashboard.md`** — the consumer-facing reference for the dashboard registry surface has been removed alongside the crate.

## [0.4.0] - 2026-05-11

### Added

- **`RunId` / `RunLabels` on `GraphEvent`** (`polaris_graph`) — every `GraphEvent` variant carries a `run_id` (freshly minted per `GraphExecutor::execute*` invocation) and an opaque `labels: RunLabels` bag, so hook handlers can correlate `GraphStart`/`SystemStart`/…/`GraphComplete` into a single trace and filter by application-level identifiers without any new layered dependencies. `ExecutionResult` exposes `run_id` for the same purpose. `GraphEvent::run_id()` and `GraphEvent::labels()` accessors land on the enum. `RunLabels` derives `PartialEq` / `Eq` for ergonomic `assert_eq!(event.labels(), &expected)` in hook tests.

- **`GraphExecutor::execute_with_labels`** (`polaris_graph`) — escape hatch for callers that want to attach correlation labels (e.g. `"session_id"`, `"agent_type"`) without the executor knowing anything about sessions or agents. `execute(...)` keeps the same signature and delegates to this with empty labels.

- **Session-tagged graph events** (`polaris_sessions`) — `SessionsAPI::process_turn*` calls `execute_with_labels` with `session_id` and `agent_type` so dashboards and tracing pipelines can scope live graph events to a session out of the box.

- **`AppConfig::with_public_path` / `with_public_prefix`** (`polaris_app`) — declarative allowlist for routes that should bypass `AuthProvider`. Use exact paths (`/healthz`, `/v1/auth/login`) or trailing-slash prefixes (`/dashboard/`) for hierarchical exemptions. The middleware consults the allowlist before invoking the provider, so consumers no longer hand-code `if path == "/healthz"` checks inside their `AuthProvider` impls. Empty allowlist (the default) preserves today's behavior — every request goes through the provider. Both builders panic at config time if the supplied path/prefix is empty or does not start with `/`; an empty prefix would otherwise have made every request public (`str::starts_with("")` is always `true`), silently disabling `AuthProvider`.

- **`PublicPath` / `PublicPrefix` newtypes** (`polaris_app`) — validated wrappers around the request paths and prefixes consumed by the allowlist above. The smart constructors `PublicPath::new` / `PublicPrefix::new` (and `TryFrom<&str>` / `TryFrom<String>`) return `Result<_, PublicRouteError>`; invalid input (empty string, missing leading `/`) is now impossible to represent at the type level. The `AppConfig::with_public_path` / `with_public_prefix` builders accept `impl Into<String>` and route through the same constructors, so the panic behavior is preserved. Use the newtypes directly when you want validation as a `Result` at the boundary. Exposed as `polaris_app::{PublicPath, PublicPrefix, PublicRouteError}`.

- **`polaris_dashboard` crate** — registry-only, zero-frontend Layer 3 plugin where any plugin can contribute nav items, sections, and panels to a dashboard or observability UI without depending on a downstream consumer repo. `DashboardRegistry` exposes chained `add_nav_item` / `add_section` / `add_panel` and `remove_*` for build-time suppression of upstream contributions (silent no-op when the id is absent). Descriptors `NavItem`, `Section`, `Panel`, `Transport` carry a free-form `kind: String` and `metadata: serde_json::Value`; seed `kind` vocabulary documented at the crate level (`list`, `detail`, `kv`, `log`, `timeseries`, `polaris-graph`, `otel-trace`, `external`). `DashboardPlugin` mounts `GET /v1/dashboard/manifest` via `add_routes_with`, freezes the registry in `ready()`, caches manifest bytes, and broadcasts `RegistryEvent::Ready(Arc<Manifest>)` once on freeze. `RegistryEvent` is `#[non_exhaustive]`. Opt-in — not in `DefaultPlugins`.

- **Core-plugin dashboard contributions** — `polaris_sessions`, `polaris_tools`, `polaris_models`, and `polaris_core_plugins`'s `TracingPlugin` each gain an opt-in `dashboard` feature that registers nav items, sections, and panels and the snapshot endpoints those panels point at. `ToolsSnapshot` and `ModelsSnapshot` mirror `DashboardRegistry`'s `OnceLock + Bytes` pattern — JSON frozen at `ready()` and served as cached bytes. `TracingPlugin` adds a span buffer surface for live trace inspection.

- **`typegen` workspace feature** — gates `ts-rs` derives on canonical wire types (`SessionId`, `AgentTypeId`, `SessionStatus`, `SessionMetadata`, `NavItem`, `Section`, `Panel`, `Transport`, `Manifest`). Off by default — `ts-rs` is absent from the release dep graph. `cargo test --features typegen` regenerates `bindings/ts/src/*.ts`; `TS_RS_EXPORT_DIR` in `.cargo/config.toml` keeps output at workspace root regardless of which crate runs the test. CI fails if regen produces a diff. Generated bindings are checked in and packaged as the private `@polaris/types` npm package. Contributor reference at `docs/reference/typegen.md`.

- **`WsRouter` API** (`polaris_app`, `ws` feature) — build-time API for WebSocket route registration, mirroring `HttpRouter`. Plugins register WS handlers via `server.api::<WsRouter>().add_routes(router)` during `build()`. WebSocket upgrade requests go through the same middleware stack (auth, CORS, tracing, request-ID) as REST routes. Gated behind the `ws` Cargo feature (`axum/ws`).

- **`SseIOResponse` adapter** (`polaris_sessions::http`) — wraps `mpsc::UnboundedReceiver<IOMessage>` as an axum SSE response. Each `IOMessage` becomes one Server-Sent Event with a source-based event type (`user`, `agent`, `system`, `external`) and JSON-serialized data payload. Includes 15-second keep-alive for long-running streams.

- **`HttpRouter::add_routes_with`** (`polaris_app`) — deferred router builder API. Plugins pass a closure that receives `&Server` and runs during `AppPlugin::ready()`, after every plugin's `build()` has completed. Enables typed `.with_state(T)` injection for state materialized by other plugins without the `OnceLock<T>` dance. Additive to the existing `add_routes` stateless API.

- **`IOProvider::close()`** (`polaris_core_plugins`) — new trait method that signals stream termination. Default implementation is a no-op; providers owning channel senders (e.g. `HttpIOProvider`) override it. Exposed through `UserIO::close()` for agent-side use.

- **`HttpIOProvider::close()`** drops the output sender so any receiver observes end-of-stream. Internal representation changed to `Mutex<Option<UnboundedSender>>` to support this; `Debug` impl now reports `closed` state instead of a derived projection.

- **Per-turn SSE endpoint** (`polaris_sessions`, `http` feature) — `POST /v1/sessions/{id}/turns/stream` streams each `IOMessage` as an SSE event as the agent emits it, instead of buffering until the turn completes. Pre-stream validation returns ordinary HTTP errors; the stream ends with a terminal `event: done` carrying `StreamTurnDone` on success or `event: error` with `{ code, message }` on failure. Keep-alive every 15s. Depends on new `tokio-stream` dep (gated by `http` feature).

- **`StreamTurnDone`** (`polaris_sessions::http`) — terminal SSE event payload for successful streaming turns, containing the same `TurnExecutionMetadata` as the buffered response.

- **`AppConfig::with_allow_any_cors_origin`** — explicit opt-in for `Access-Control-Allow-Origin: *`. The default no-origin path now warns and falls back to wildcard CORS only when no `AuthProvider` is registered; configuring auth without origins (or without this opt-in) panics at startup rather than silently exposing authenticated endpoints cross-origin.

- **`dashboard-registry` Cargo feature** (`polaris-ai`, `polaris_internal`) — gates the `polaris_dashboard` crate (and its `axum` runtime dep) so it stays out of the release dep graph until any `*-dashboard` umbrella or `DashboardPlugin` is opted into. All `*-dashboard` features now imply `dashboard-registry`.

### Changed

- **BREAKING — `GraphEvent` variants gained fields** (`polaris_graph`) — `run_id` and `labels` are new fields on every variant. Pattern matches that don't already use `..` need to either ignore the new fields or bind them. `event.run_id()` / `event.labels()` accessors mean most observers don't need to touch struct patterns.

- **BREAKING — `ExecutionResult` fields demoted to methods** (`polaris_graph`) — `nodes_executed: pub usize` and `duration: pub Duration` are now `pub(crate)` and exposed via `ExecutionResult::nodes_executed()` and `ExecutionResult::duration()`. The new `run_id()` accessor lands alongside them. Migration: `result.nodes_executed` → `result.nodes_executed()`, `result.duration` → `result.duration()`. Struct-pattern destructures (`let ExecutionResult { nodes_executed, duration, .. } = result;`) need to switch to the accessor calls. Encapsulating the fields lets the executor evolve the result shape (e.g. add `run_id`) without rippling through call sites.

- **BREAKING — `AppConfig::public_paths` / `public_prefixes` return types** (`polaris_app`) — accessors now return `&[PublicPath]` / `&[PublicPrefix]` instead of `&[String]`. Call `.as_str()` on the newtype to recover the previous `&str` view, e.g. `config.public_paths().iter().map(PublicPath::as_str)`. The newtypes guarantee that allowlist storage cannot hold empty or unanchored strings, which is the type-system version of the eager validation on the builders.

- **BREAKING — `HttpIOProvider` relocated** from `polaris_app` to `polaris_sessions::http`. The move breaks a `polaris_core_plugins → polaris_app → polaris_core_plugins` dependency cycle that `TracingPlugin`'s dashboard contribution would otherwise introduce. Downstream consumers must update `use polaris_app::HttpIOProvider` to `use polaris_sessions::http::HttpIOProvider`. The type's API is unchanged.

- **BREAKING — `HttpIOProvider::new(input_buffer)` → `HttpIOProvider::new(input_buffer, output_buffer)`** (`polaris_sessions::http`). The output channel is now bounded with explicit per-call capacity; agents that emit faster than the consumer drains apply backpressure via `await` instead of growing memory unbounded. SSE turn streams use `tokio_stream::wrappers::ReceiverStream` instead of the unbounded variant. The `process_turn_stream` handler documents that turns are not aborted on client disconnect — disconnects propagate via channel close → `IOError::Closed` on the next agent send.

- **`polaris_sessions::http::HttpPlugin`** refactored to use `add_routes_with`. `DeferredState` (`Arc<OnceLock<SessionsAPI>>`) pattern removed; routes are now constructed in `ready()` via the deferred builder with direct `with_state(SessionsAPI)`. `HttpPlugin` is now a unit struct; the separate `ready()` implementation is gone.

- **SSE error events hardened** — error payloads now use a structured `{ code, message }` JSON envelope; HTTP error codes centralized in `polaris_sessions::http::error`.

- **Top-level re-exports** — `polaris_dashboard` re-exported as `polaris_ai::dashboard`; layer table and quick-reference in `src/lib.rs` updated to mention `add_routes_with` and the new `dashboard` module.

- **Dashboard panel→section contract tightened** — every `Panel` is expected to belong to a `Section`. Canonical dashboard consumers group panels by section and **drop section-less panels**; the registry itself does not validate cross-references, so this is documentation-as-convention. Each `*DashboardPlugin` shipped in this release (`polaris_sessions`, `polaris_tools`, `polaris_models`, `polaris_core_plugins::TracingPlugin`) now contributes a default `<nav>-overview` `Section` and routes its first panel through it. External plugins contributing panels should register a corresponding section (typically a single `"overview"` section is enough) or set `panel.section_id` to an existing one.

- **`otel` feature docs corrected** (`polaris-ai`, `polaris_internal`) — the `otel` feature description in `Cargo.toml`, `polaris_internal/Cargo.toml`, and `src/lib.rs` previously claimed "OTel-aware HTTP middleware" and "end-to-end OTel context propagation". The feature only switches `polaris_app`'s HTTP request spans to OTel HTTP semantic-convention field names (`http.request.method`, `url.path`, `http.response.status_code`, plus `otel.name` / `otel.kind`); it does **not** extract incoming W3C `traceparent` headers. Docs now describe the actual behavior.

- **CORS `allow_headers` includes `Authorization`** (`polaris_app`) — the CORS layer previously whitelisted only `Content-Type`, which caused cross-origin preflights to strip `Authorization: Bearer …` headers before they reached the `AuthProvider`. Browser SPAs hosted on a separate origin (e.g. a `polaris-dashboard` deployment talking to a Polaris backend) now have their bearer-token preflights succeed without operator-side workarounds.

- **`AuthProvider` doc example uses constant-time comparison** (`polaris_app`) — the rustdoc example on `AuthProvider` previously compared bearer tokens with `==`, which short-circuits on the first mismatched byte and leaks the length of the matching prefix through timing. The example now demonstrates a constant-time comparison helper and points readers at `subtle::ConstantTimeEq`, `ring::constant_time`, and `openssl::memcmp::eq` for production use. Documentation-only — no runtime behavior change.

- **Dashboard bridge plan** (`docs/plans/polaris-dashboard.md`) substantially revised:
  - Expanded from 9 to 14 architectural decisions — adds the in-core `polaris_dashboard` crate (registry-only, zero frontend), free-form descriptors with curated `kind` vocabulary, `ts-rs` Rust→TS typegen, lean registry with broadcast channel.
  - New Layer A items: **A6** (`polaris_dashboard` crate), **A7** (`ts-rs` typegen wiring), **A8** (feature-gated core plugin contributions).
  - **B3** and **B4** marked superseded by A6; **B5** added for the external plugin's asset-serving role; **C6** added for the manifest-driven shell.
  - Removed legacy WS-envelope decision that mirrored the inherited frontend shape (envelope now designed with B2).
  - Subscription keying extended: `Agent::name` doubles as graph-identity key alongside session id — no separate `graph_id` needed.
  - Added "Naming Conventions" legend disambiguating `polaris_dashboard` (Rust crate) vs `polaris-dashboard` (Svelte repo) vs `polaris-dashboard-plugin` (consumer crate) vs `@polaris/dashboard` (npm package).

### Fixed

- **`stream_turn_busy` integration test** synchronizes on a `BlockingAgent`-emitted `system` message instead of a 100 ms wall-clock sleep. Removes a CI flake source. Test agents now send a `blocking-ready` signal as soon as the session lock is held; the test polls for that signal with a 5 s deadline.
- **`manifest_union::full_server_unions_manifest_and_serves_endpoints`** asserts the actual contents of `/v1/tools`, `/v1/models/providers`, and `/v1/tracing/spans` instead of discarding response bodies. A regression that returned bogus or empty payloads from those endpoints now surfaces.
- **Stale `crates/polaris_app/src/io.rs` references** in `CLAUDE.md` / `AGENT.md` updated to point at `crates/polaris_sessions/src/http/io.rs` (where `HttpIOProvider` actually lives after the relocation).

### Removed

- **`ApiError::NotReady`** (`polaris_sessions::http`) — unreachable after the `add_routes_with` refactor; the `SessionsAPI` is always ready before any handler runs.

## [0.3.0] - 2026-04-16

### Added

- **`ToolContext` and `#[context]` parameter injection** (`polaris_tools`) — per-invocation context propagation into `#[tool]` functions. `ToolContext` is a lightweight typed map that carries per-invocation state (session IDs, working directories, locales, dry-run flags, opaque backend handles, or any other caller-supplied value) from the calling system into tool execution. Values are stored behind `Arc`, so `ToolContext` is cheaply `Clone` regardless of whether individual value types are. Tools declare context dependencies with `#[context]` on parameters; these are extracted from `ToolContext` at runtime, do not appear in the LLM-facing JSON schema, and require `T: Clone`. `Option<T>` context params are `None` when absent instead of erroring.
- **`ToolRegistry::execute_with(name, args, ctx)`** (`polaris_tools`) — context-aware tool execution. The existing `execute(name, args)` remains as a convenience that passes an empty context.

### Changed

- **`Tool::execute` signature** (breaking) — now takes `&ToolContext` as a second parameter: `execute<'ctx>(&'ctx self, args: Value, ctx: &'ctx ToolContext) -> Pin<Box<...>>`. Existing manual `Tool` impls must add `_ctx: &'ctx ToolContext`. Macro-generated tools update automatically.
- **`#[context]` rejects nested `Option<Option<T>>`** — the `#[tool]` / `#[toolset]` macros now emit a compile-time error for `#[context]` parameters typed `Option<Option<T>>`. Use `Option<T>` for an optional context value; the outer `Option` already expresses absence.
- **`TracingPlugin::with_capture_genai_content` documentation** — doc comment now warns that tool arguments and results are captured verbatim on spans, so tools returning credentials, PII, or other secrets will have those values recorded when this flag is enabled.

## [0.2.2] - 2025-04-14

### Changed

- Bump workspace version to 0.2.2.

### Added

- **Graph-level `max_duration`** — builder-defined timeout for graph execution.

### Fixed

- Isolate non-fallible system body in inner async block for correct return handling.
