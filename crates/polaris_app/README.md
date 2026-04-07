# polaris_app

Shared HTTP server runtime for Polaris products. Provides an axum-based server that integrates with the Polaris plugin lifecycle.

## Quick Start

```rust
use polaris_system::server::Server;
use polaris_app::{AppPlugin, AppConfig};

let mut server = Server::new();
server.add_plugins(
    AppPlugin::new(AppConfig::new().with_port(8080))
);
// Add other plugins that register routes...
server.finish(); // Server starts listening on ready()
```

## Adding Routes

Any plugin that depends on `AppPlugin` can register routes via the `HttpRouter` API:

```rust
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use polaris_app::{AppPlugin, HttpRouter};
use axum::{Router, routing::get};

struct HealthPlugin;

impl Plugin for HealthPlugin {
    const ID: &'static str = "myapp::health";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let router = Router::new()
            .route("/healthz", get(|| async { "ok" }));
        server.api::<HttpRouter>()
            .expect("AppPlugin must be added first")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}
```

Multiple plugins can register routes independently -- they are all merged before the server starts.

## HttpIOProvider

Bridges HTTP requests to agent `UserIO` via tokio channels. Used by session HTTP endpoints to connect an HTTP handler to an agent's IO abstraction:

```rust
use polaris_app::HttpIOProvider;
use polaris_core_plugins::{IOMessage, UserIO};
use std::sync::Arc;

let (provider, input_tx, mut output_rx) = HttpIOProvider::new(32);

// Pre-load user input from HTTP request body
// input_tx.send(IOMessage::user_text("hello")).await;

// Wrap as UserIO for the agent
// let user_io = UserIO::new(Arc::new(provider));

// Collect agent output for HTTP response
// let response = output_rx.recv().await;
```

## Configuration

```rust
use polaris_app::AppConfig;

AppConfig::new()
    .with_host("0.0.0.0")        // Default: 127.0.0.1
    .with_port(8080)              // Default: 3000
    .with_cors_origin("http://localhost:3000")
    .with_cors_origin("https://app.example.com");
```

Empty CORS origins (default) allows any origin.

## Middleware

Applied automatically to all routes:

| Layer | Description |
|-------|-------------|
| Request ID | Injects UUID `x-request-id` header on every request |
| Tracing | Logs request/response spans via `tracing` |
| CORS | Configurable allowed origins, methods, headers |
| Propagate | Copies `x-request-id` to response headers |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `polaris_system` | Plugin trait, Server, GlobalResource, API |
| `polaris_core_plugins` | IOProvider trait, IOMessage types |
| `axum` | HTTP framework |
| `tower-http` | CORS, tracing, request ID middleware |
| `tokio` | Async runtime, channels, networking |
