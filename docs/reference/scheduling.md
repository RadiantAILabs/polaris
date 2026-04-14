---
notion_page: https://www.notion.so/radiant-ai/Server-Lifecycle-Scheduling-342afe2e695d80928147e3d069bfb42b
title: Server Lifecycle & Scheduling
---

# Server Lifecycle & Scheduling

The `Server` manages a strict plugin lifecycle and a tick-based scheduling system for plugin updates. This document covers the full lifecycle sequence, tick scheduling mechanics, and plugin update ordering.

## Server Lifecycle

The server progresses through a linear state machine: `NotStarted` → `Building` → `Built`.

### finish() Sequence

`Server::finish()` (also called by `run()` and `run_once()`) executes the full lifecycle:

```text
1. Dependency Resolution
   └── Topologically sort plugins by declared dependencies
       └── Panics on missing dependencies or circular references

2. Build Phase (build_state = Building)
   └── Call plugin.build() on each plugin in dependency order
       └── Plugins register resources, APIs, routes, local factories

3. Ready Phase
   └── Call plugin.ready() on each plugin in dependency order
       └── Plugins resolve APIs, bind deferred state, start services

4. Schedule Registry
   └── For each plugin, record which schedules it declared via tick_schedules()
       └── Maps ScheduleId → Vec<plugin_index> (dependency-ordered)

5. Deferred Globals Resolution
   └── Fill the OnceLock<Arc<Resources>> so ContextFactory handles created
       during ready() can now resolve

6. build_state = Built
```

### Cleanup

```rust
server.cleanup().await;
```

Calls `plugin.cleanup()` in **reverse** dependency order (dependents before dependencies), allowing graceful resource release.

### Convenience Methods

| Method | Behavior |
|--------|----------|
| `server.run().await` | Calls `finish()` |
| `server.run_once().await` | Alias for `finish()` (testing) |
| `server.finish().await` | Full lifecycle (build + ready + schedule registry) |
| `server.cleanup().await` | Reverse-order cleanup |

## Tick Scheduling

Plugins can declare interest in schedule types via `tick_schedules()`. The server triggers ticks, and only plugins registered for that schedule receive `update()` calls.

### Declaring Schedules

```rust
use polaris_system::plugin::{Schedule, ScheduleId};

// Define a schedule marker type
pub struct PostAgentRun;
impl Schedule for PostAgentRun {}

// Plugin declares interest
impl Plugin for MetricsPlugin {
    fn tick_schedules(&self) -> Vec<ScheduleId> {
        vec![PostAgentRun::schedule_id()]
    }

    fn update(&self, server: &mut Server, _schedule: ScheduleId) {
        // Called when server.tick::<PostAgentRun>() fires
    }
}
```

### Triggering Ticks

Ticks are triggered by Layer 2 code (graph executor, session manager) or application code:

```rust
// Generic version
server.tick::<PostAgentRun>();

// Non-generic version (when schedule is dynamic)
server.tick_schedule(schedule_id);
```

### Update Ordering

Plugins are ticked in **dependency order** — the same order used for `build()` and `ready()`. This guarantees that if Plugin B depends on Plugin A, A's `update()` runs before B's.

### Schedule Registry

The schedule registry is built at the end of `finish()`:

```rust
// Internal: maps schedule → plugin indices (dependency-ordered)
schedule_registry: HashMap<ScheduleId, Vec<usize>>
```

When `tick_schedule()` fires, only plugins that registered for that schedule have their `update()` called. Plugins not interested in a schedule are skipped entirely.

### Multiple Schedules

A plugin can register for multiple schedules:

```rust
fn tick_schedules(&self) -> Vec<ScheduleId> {
    vec![
        PostAgentRun::schedule_id(),
        PostTurn::schedule_id(),
    ]
}

fn update(&self, server: &mut Server, schedule: ScheduleId) {
    if schedule == PostAgentRun::schedule_id() {
        // Handle post-agent-run
    } else if schedule == PostTurn::schedule_id() {
        // Handle post-turn
    }
}
```

## Plugin Lifecycle Methods

| Method | Phase | Order | Purpose |
|--------|-------|-------|---------|
| `build(&self, &mut Server)` | Build | Dependency order | Register resources, APIs, routes |
| `ready(&self, &mut Server)` | Ready | Dependency order | Resolve APIs, start services, bind state |
| `tick_schedules(&self)` | Post-finish | N/A (declarative) | Declare schedule interest |
| `update(&self, &mut Server, ScheduleId)` | Runtime | Dependency order | Respond to schedule ticks |
| `cleanup(&self, &mut Server)` | Shutdown | Reverse dependency order | Release resources, stop services |

## Dependency Resolution

Plugins declare dependencies via `dependencies()`:

```rust
fn dependencies(&self) -> Vec<PluginId> {
    vec![PluginId::of::<AppPlugin>()]
}
```

The server performs topological sort before the build phase:
- **Missing dependency** → panic with the missing plugin name
- **Circular dependency** → panic (detected during topological sort)
- **No dependencies** → plugins with no declared dependencies run in registration order relative to each other

## Hook Schedules vs Plugin Schedules

These are distinct scheduling mechanisms at different layers:

| | Hook Schedules (Layer 2) | Plugin Schedules (Layer 1) |
|---|---|---|
| Defined in | `polaris_graph/src/hooks/schedule.rs` | Any code implementing `Schedule` |
| Triggered by | Graph executor during traversal | `server.tick::<S>()` |
| Receiver | Hook callbacks (observer/provider) | Plugin `update()` method |
| Scope | Per-node execution events | Server-wide lifecycle events |
| Examples | `OnSystemStart`, `OnGraphComplete` | `PostAgentRun`, `PostTurn` |

## Key Files

| File | Purpose |
|------|---------|
| `polaris_system/src/server.rs` | `Server` struct, `finish()`, `tick()`, `cleanup()`, dependency resolution |
| `polaris_system/src/plugin.rs` | `Plugin` trait, `Schedule` trait, `ScheduleId` |
| `polaris_graph/src/hooks/schedule.rs` | Built-in hook schedule types |
