HTTP server runtime with plugin-based route composition.

This module provides the shared HTTP server (`AppPlugin`), a route
registration API (`HttpRouter`), and IO bridging for connecting HTTP
requests to agent execution.

# `AppPlugin` and `HttpRouter`

`AppPlugin` owns the axum HTTP server lifecycle. `HttpRouter` is a build-time
API for registering route fragments from plugins.

```no_run
use polaris_ai::system::server::Server;
use polaris_ai::app::{AppPlugin, AppConfig, HttpRouter};

let mut server = Server::new();
server.add_plugins(
    AppPlugin::new(AppConfig::new().with_port(8080))
);
```

## Registering Routes from a Plugin

```ignore
use polaris_ai::system::plugin::{Plugin, PluginId, Version};
use polaris_ai::system::server::Server;
use polaris_ai::app::{AppPlugin, HttpRouter};
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

Routes are registered during `build()` and merged into a single router in
`AppPlugin::ready()`.

# The `DeferredState` Pattern

Routes are registered in `build()`, but the APIs they need (e.g.,
`SessionsAPI`) are only available in `ready()`. The `DeferredState` pattern
bridges this gap using `Arc<OnceLock<T>>`:

```ignore
use std::sync::{Arc, OnceLock};
use polaris_ai::system::plugin::{Plugin, PluginId, Version};
use polaris_ai::system::server::Server;
use polaris_ai::app::{AppPlugin, HttpRouter};
use polaris_ai::sessions::SessionsAPI;
use axum::{Router, routing::get, extract::State, Json};

type DeferredState = Arc<OnceLock<SessionsAPI>>;

struct MyHttpPlugin {
    state: DeferredState,
}

impl Plugin for MyHttpPlugin {
    const ID: &'static str = "myapp::http";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let state = Arc::clone(&self.state);
        let router = Router::new()
            .route("/endpoint", get(my_handler))
            .with_state(state);
        server.api::<HttpRouter>().unwrap().add_routes(router);
    }

    async fn ready(&self, server: &mut Server) {
        let api = server.api::<SessionsAPI>().unwrap().clone();
        self.state.set(api).expect("ready() called once");
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

async fn my_handler(
    State(deferred): State<DeferredState>,
) -> &'static str {
    let _sessions = deferred.get().expect("not ready");
    "ok"
}
```

# `HttpIOProvider`

Bridges HTTP requests to the agent's `UserIO` abstraction via tokio
channels:

```no_run
use polaris_ai::app::HttpIOProvider;
# use polaris_ai::plugins::IOMessage;
use std::sync::Arc;

let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1);
let provider = Arc::new(provider);
```

# Session HTTP Endpoints

With the `sessions-http` feature, `HttpPlugin` registers REST endpoints:

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/sessions` | Create session |
| `GET` | `/v1/sessions` | List sessions |
| `GET` | `/v1/sessions/{id}` | Get session info |
| `DELETE` | `/v1/sessions/{id}` | Delete session |
| `POST` | `/v1/sessions/{id}/turns` | Process turn |
| `POST` | `/v1/sessions/{id}/checkpoints` | Create checkpoint |
| `GET` | `/v1/sessions/{id}/checkpoints` | List checkpoints |
| `POST` | `/v1/sessions/{id}/rollback` | Rollback |
| `POST` | `/v1/sessions/{id}/save` | Persist |
| `POST` | `/v1/sessions/{id}/resume` | Resume |

# Related

- [Sessions](crate::sessions) -- the `SessionsAPI` that HTTP handlers delegate to
- [Systems](crate::system) -- plugin trait used for route registration
- [Feature flags](crate#sessions) -- enabling `sessions-http`
