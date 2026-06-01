---
notion_page: https://www.notion.so/radiant-ai/Dev-Tools-342afe2e695d80a8a2a4e7f444ea7ef7
title: DevTools
---

# DevTools

`DevToolsPlugin` exposes per-system execution metadata and optional event-level tracing. It is the standard entry point for debugging graph execution and for writing systems that need to know *which* node they're running inside.

## Setup

```rust
use polaris_graph::dev::DevToolsPlugin;

let mut server = Server::new();
server.add_plugins(DevToolsPlugin::new());                  // SystemInfo injection only
server.add_plugins(DevToolsPlugin::new().with_event_tracing()); // + debug-level event logs
```

The plugin has no dependencies on other Polaris plugins but requires a `HooksAPI`; if none is registered, `DevToolsPlugin` installs one in its `build()` phase.

## `SystemInfo`

`SystemInfo` is a `LocalResource` injected by a provider hook on `OnSystemStart`, just before each system executes. Systems may read it via `Res<SystemInfo>`.

| Field | Accessor | Description |
|-------|----------|-------------|
| `node_id` | `info.node_id()` | `NodeId` of the currently executing graph node |
| `system_name` | `info.system_name()` | `&'static str` name of the system |

```rust
use polaris_graph::dev::SystemInfo;
use polaris_system::param::Res;
use polaris_system::system;

#[system]
async fn traced_step(info: Res<SystemInfo>) -> StepResult {
    tracing::info!(
        node = ?info.node_id(),
        system = info.system_name(),
        "running step"
    );
    StepResult::default()
}
```

Because `SystemInfo` is injected by a provider hook, it bypasses resource validation — systems declaring `Res<SystemInfo>` will not fail `validate_resources()` if `DevToolsPlugin` is registered. Without the plugin, those systems would fail at validation time.

## Event Tracing

Enabling `with_event_tracing()` registers an observer on every graph schedule (`AllGraphSchedules`) that emits each event via `tracing::debug!`. Events covered:

- Graph-level: `OnGraphStart`, `OnGraphComplete`, `OnGraphFailure`
- System-level: `OnSystemStart`, `OnSystemComplete`, `OnSystemError`
- Control flow: `OnDecisionStart/Complete`, `OnSwitchStart/Complete`, `OnLoopStart/Iteration/End`, `OnParallelStart/Complete`, `OnScopeStart/Complete`

Configure your `tracing_subscriber` to enable `DEBUG` level for the `polaris_graph` target to surface these logs:

```rust
tracing_subscriber::fmt()
    .with_env_filter("polaris_graph=debug")
    .init();
```

## Performance Notes

- **`SystemInfo` injection** is cheap — one allocation per system invocation (the `SystemInfo` struct is two fields).
- **Event tracing** fires on every lifecycle event. With `tracing` compiled in `release` but the subscriber disabled, the cost is a single level check per event. With debug logging enabled, expect measurable overhead on graph-heavy workloads.

## When to Enable

| Environment | Recommended |
|-------------|-------------|
| Local development / debugging | `DevToolsPlugin::new().with_event_tracing()` |
| CI / integration tests | `DevToolsPlugin::new()` (inject `SystemInfo`, no spam) |
| Production | `DevToolsPlugin::new()` only if systems depend on `Res<SystemInfo>`; otherwise omit |

For structured observability in production, prefer registering your own observer hooks on the specific schedules you care about (e.g., `OnSystemError` → metrics counter, `OnGraphComplete` → histogram).

## Dashboard-Feature Tracing Buffer and Span Store

`TracingPlugin` ships with an additional debugging surface gated on the `dashboard` Cargo feature of `polaris_core_plugins`: an in-memory `SpanBuffer` that records every closed `SpanRecord`, plus HTTP endpoints under `/v1/tracing/*` and `/v1/sessions/{id}/usage` that project it into runs, span trees, and token-usage rollups. See the [HTTP reference](./http.md#tracing-endpoints-dashboard-feature) for the endpoint list.

### Layers

The tracing-subscriber pipeline writes through a `RecordingLayer` for each enabled sink:

- **`SpanBuffer`** — bounded ring (default 1024 records). Cheap to query, but volatile: spans evict in FIFO order and the buffer is wiped at process exit. Backs the live `/v1/tracing/*` endpoints.
- **`SpanStorePlugin` (optional)** — durable companion. Installs its own `RecordingLayer` and routes every record carrying a `session_id` label through a pluggable `SpanStore` trait. Records are enqueued onto a bounded queue and drained by a single background writer task, never written inline: the tracing hot path never blocks, bursts past the queue bound are dropped with a rate-limited warning rather than spawning unbounded work, and the writer is drained on `cleanup()` so records emitted just before shutdown still reach the store. On `ready()` it hydrates the in-memory `SpanBuffer` from the store, so a resumed session reports non-empty runs immediately after boot. Buffer writes and store writes are independent — an unreachable store does not stall the in-memory pipeline.

Two backends ship in-tree:

- `InMemorySpanStore` — default for tests; exercises the trait surface without touching disk.
- `FileSpanStore` (feature-gated on `file-store`) — one JSON-lines file per session at `<base_dir>/<session_id>.jsonl`, append-only, recoverable from a partial trailing line. Each write is `fsync`ed before it reports success, so a record survives a crash once persisted; the writer batches records per drain so the `fsync` cost is paid once per batch rather than once per record.

Custom backends (Postgres, S3, …) implement `SpanStore` directly. Records without a `session_id` label are dropped on the storage path.

```rust,ignore
use polaris_core_plugins::{TracingPlugin, SpanStorePlugin, FileSpanStore};
use std::sync::Arc;

server
    .add_plugins(TracingPlugin::default())
    .add_plugins(SpanStorePlugin::new(Arc::new(
        FileSpanStore::new("/var/lib/polaris/spans")?,
    )));
```

### Token Usage and Pricing

`TracingPlugin` registers an empty `UsagePricing` API when the `dashboard` feature is enabled. Consumer plugins populate it from their own `build()`:

```rust,ignore
use polaris_core_plugins::{ModelPricing, UsagePricing};

server.api::<UsagePricing>().set(
    "anthropic",
    "claude-opus-4-7",
    ModelPricing::new(15.0, 75.0),
);
```

With at least one rate registered, the rollup endpoints surface `cost_usd` on both totals and per-(provider, model, agent-type) breakdown rows. Providers can also declare a default rate via `LlmProvider::pricing()` — `TracingLlmProvider` reads it at call time and writes `gen_ai.usage.cost_usd` on each chat span, so cost is visible in any OTel waterfall without round-tripping through the aggregator. `UsagePricing` is the consumer-side override layer at aggregation time.

## Key Files

| File | Purpose |
|------|---------|
| `polaris_graph/src/dev.rs` | `DevToolsPlugin`, `SystemInfo` |
| `polaris_graph/src/hooks/api.rs` | `HooksAPI` — the underlying hook registry |
| `polaris_graph/src/hooks/schedule.rs` | `AllGraphSchedules`, `OnSystemStart`, etc. |
| `polaris_core_plugins/src/tracing_plugin/buffer.rs` | `SpanBuffer`, `RunSummary`, `SpanTree` |
| `polaris_core_plugins/src/tracing_plugin/capture.rs` | `RecordingLayer`, `SpanRecordSink` trait |
| `polaris_core_plugins/src/tracing_plugin/span_store/` | `SpanStore` trait, `SpanStorePlugin`, `InMemorySpanStore`, `FileSpanStore` |
| `polaris_core_plugins/src/tracing_plugin/usage_pricing.rs` | `UsagePricing` API, `ModelPricing` |
