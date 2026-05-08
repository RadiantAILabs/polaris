# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-05-07

### Added

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

- **BREAKING — `HttpIOProvider` relocated** from `polaris_app` to `polaris_sessions::http`. The move breaks a `polaris_core_plugins → polaris_app → polaris_core_plugins` dependency cycle that `TracingPlugin`'s dashboard contribution would otherwise introduce. Downstream consumers must update `use polaris_app::HttpIOProvider` to `use polaris_sessions::http::HttpIOProvider`. The type's API is unchanged.

- **BREAKING — `HttpIOProvider::new(input_buffer)` → `HttpIOProvider::new(input_buffer, output_buffer)`** (`polaris_sessions::http`). The output channel is now bounded with explicit per-call capacity; agents that emit faster than the consumer drains apply backpressure via `await` instead of growing memory unbounded. SSE turn streams use `tokio_stream::wrappers::ReceiverStream` instead of the unbounded variant. The `process_turn_stream` handler documents that turns are not aborted on client disconnect — disconnects propagate via channel close → `IOError::Closed` on the next agent send.

- **`polaris_sessions::http::HttpPlugin`** refactored to use `add_routes_with`. `DeferredState` (`Arc<OnceLock<SessionsAPI>>`) pattern removed; routes are now constructed in `ready()` via the deferred builder with direct `with_state(SessionsAPI)`. `HttpPlugin` is now a unit struct; the separate `ready()` implementation is gone.

- **SSE error events hardened** — error payloads now use a structured `{ code, message }` JSON envelope; HTTP error codes centralized in `polaris_sessions::http::error`.

- **Top-level re-exports** — `polaris_dashboard` re-exported as `polaris_ai::dashboard`; layer table and quick-reference in `src/lib.rs` updated to mention `add_routes_with` and the new `dashboard` module.

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
