# polaris_core

Core infrastructure plugins for Polaris.

## Overview

This crate provides foundational plugins that most Polaris applications need.

| Plugin | Resources | Scope | Purpose |
|--------|-----------|-------|---------|
| `ServerInfoPlugin` | `ServerInfo` | Global | Server metadata (version, debug mode) |
| `TimePlugin` | `Clock`, `Stopwatch` | Global, Local | Time utilities with mockable clock |
| `TracingPlugin` | `TracingConfig`, `TracingLayersApi` | Global, Build-time | Logging, observability, and instrumentation via `tracing` |
| `OpenTelemetryPlugin` | — | — | OTLP trace export via `tracing-opentelemetry` |
| `IOPlugin` | `UserIO` | Local | Abstracted IO for user interaction and tool integration |

Each plugin may be added individually, or altogether through the `DefaultPlugins` plugin group:

```rust
use polaris_system::server::Server;
use polaris_system::plugin::PluginGroup;
use polaris_core_plugins::DefaultPlugins;

Server::new()
    .add_plugins(DefaultPlugins.build())
    .run();
```

`MinimalPlugins` provides an alternative bundle for testing, without tracing output. This is especially useful for unit tests where tracing is not necessary. It includes the following plugins:

- `ServerInfoPlugin`
- `TimePlugin`

```rust
use polaris_core_plugins::MinimalPlugins;

Server::new()
    .add_plugins(MinimalPlugins.build())
    .run();
```

## Plugin Usage Examples

### ServerInfoPlugin

Provides a `ServerInfo` resource with version and debug mode:

```rust
use polaris_core_plugins::{ServerInfoPlugin, ServerInfo};
use polaris_system::param::Res;

#[system]
async fn check_mode(info: Res<ServerInfo>) {
    if info.debug {
        // Enable extra diagnostics
    }
}
```

### TimePlugin

Provides `Clock` (global, mockable) and `Stopwatch` (per-agent timer):

```rust
use polaris_core_plugins::{TimePlugin, Clock, Stopwatch};
use polaris_system::param::{Res, ResMut};

#[system]
async fn timed_work(clock: Res<Clock>, mut sw: ResMut<Stopwatch>) {
    let start = clock.now();
    // ... work ...
    sw.lap();
}
```

MockClock allows deterministic time control in tests, enabling testing time-dependent systems without real delays. For example:

```rust
use std::sync::Arc;
use std::time::{Duration, Instant};
use polaris_core_plugins::{TimePlugin, MockClock};

let mock = Arc::new(MockClock::new(Instant::now()));
let plugin = TimePlugin::with_clock(mock.clone());

// In tests, advance time without waiting
mock.advance(Duration::from_secs(60));
```

### TracingPlugin

Registers a shared `tracing` subscriber. No output layers are included by default — call `with_fmt` to add console output, or use `DefaultPlugins` which enables fmt automatically.

Other plugins (e.g., `OpenTelemetryPlugin`) can push additional layers via `TracingLayersApi` during their `build()` phase. The subscriber is installed once in `TracingPlugin::ready()` with all accumulated layers.

```rust
use polaris_core_plugins::{TracingPlugin, FmtConfig, TracingFormat};
use tracing::Level;

// Development: colored pretty output
let dev = TracingPlugin::default()
    .with_level(Level::DEBUG)
    .with_fmt(FmtConfig::default());

// Production: JSON for log aggregation
let prod = TracingPlugin::default()
    .with_level(Level::INFO)
    .with_fmt(
        FmtConfig::default()
            .format(TracingFormat::Json)
            .env_filter("polaris=info,hyper=warn")
    );
```

When the `models_tracing` or `tools_tracing` features are enabled, `TracingPlugin` also instruments LLM and tool calls with OpenTelemetry semantic convention spans. Enable `.with_capture_genai_content()` to record full request/response content on those spans.

### OpenTelemetryPlugin

Exports `tracing` spans as OpenTelemetry traces via OTLP. Depends on `TracingPlugin`.

```rust
use polaris_core_plugins::OpenTelemetryPlugin;

OpenTelemetryPlugin::new("http://localhost:4318/v1/traces")
    .with_service_name("my-agent")
    .with_env_filter("polaris=debug,hyper=warn")
    .with_resource_attribute("deployment.environment.name", "production")
    .with_export_header("x-api-key", api_key);
```

### IOPlugin

Provides an abstracted `UserIO` resource for user interaction, which can be implemented by different providers (e.g., terminal, web):

```rust
use polaris_core_plugins::{IOPlugin, UserIO, IOMessage};
use polaris_system::param::ResMut;

#[system]
async fn interact(mut io: ResMut<UserIO>) {
    io.send(IOMessage::from_agent("Hello, user!")).await;
    if let Some(response) = io.receive().await {
        // Process user response
    }
}
```

## Feature Flags

| Feature | Description |
|---------|-------------|
| `test-utils` | Enables `MockClock` and `MockIOProvider` for library consumers |
| `graph_tracing` | Enables graph execution span instrumentation (node, loop, parallel, decision, switch) |
| `models_tracing` | Enables LLM call instrumentation (adds `polaris_models` dependency) |
| `tools_tracing` | Enables tool call instrumentation (adds `polaris_tools` dependency, implies `models_tracing`) |

## License

Apache-2.0
