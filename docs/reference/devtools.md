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

## Key Files

| File | Purpose |
|------|---------|
| `polaris_graph/src/dev.rs` | `DevToolsPlugin`, `SystemInfo` |
| `polaris_graph/src/hooks/api.rs` | `HooksAPI` — the underlying hook registry |
| `polaris_graph/src/hooks/schedule.rs` | `AllGraphSchedules`, `OnSystemStart`, etc. |
