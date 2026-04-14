---
notion_page: https://www.notion.so/radiant-ai/HTTP-Integration-342afe2e695d80d2a428fb68c84c1334
title: HTTP Integration
---

# HTTP Integration

`polaris_app` provides the shared HTTP server runtime. `polaris_sessions` provides optional REST endpoints via its `http` feature. This document covers how to register routes, access Polaris resources from axum handlers, and wire HTTP requests to agent execution.

## AppPlugin and HttpRouter

`AppPlugin` owns the axum HTTP server lifecycle. `HttpRouter` is a build-time API that plugins use to register route fragments. All fragments are merged when `AppPlugin` enters `ready()`.

```rust
use polaris_app::{AppPlugin, AppConfig, HttpRouter};

let mut server = Server::new();
server.add_plugins(AppPlugin::new(AppConfig::new()));
```

### Registering Routes from a Plugin

```rust
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

Key points:
- Routes are registered during `build()` via `HttpRouter::add_routes()`
- `HttpRouter` uses interior mutability (`RwLock`) — `server.api::<HttpRouter>()` returns `&HttpRouter`
- Declare `AppPlugin` as a dependency so the `HttpRouter` API exists
- All route fragments are merged into a single router in `AppPlugin::ready()`

### Authentication

```rust
// In your plugin's build():
server.api::<HttpRouter>()
    .unwrap()
    .set_auth(MyAuthProvider::new());
```

The `AuthProvider` trait is synchronous. For async auth, register a Tower layer directly on your router instead.

## The DeferredState Pattern

The core challenge: routes are registered in `build()`, but the APIs they need (e.g., `SessionsAPI`) are only available in `ready()`. The `DeferredState` pattern bridges this gap using `Arc<OnceLock<T>>`.

```rust
use std::sync::{Arc, OnceLock};

pub(crate) type DeferredState = Arc<OnceLock<SessionsAPI>>;

pub struct MyHttpPlugin {
    state: DeferredState,
}

impl Plugin for MyHttpPlugin {
    fn build(&self, server: &mut Server) {
        let state = Arc::clone(&self.state);
        let router = Router::new()
            .route("/my-endpoint", get(my_handler))
            .with_state(state);  // axum receives the Arc<OnceLock<...>>

        server.api::<HttpRouter>().unwrap().add_routes(router);
    }

    async fn ready(&self, server: &mut Server) {
        // Now the API is available — fill the OnceLock
        let api = server.api::<SessionsAPI>().unwrap().clone();
        self.state.set(api).expect("ready() called once");
    }
}

// Handler extracts the deferred state
async fn my_handler(
    State(deferred): State<DeferredState>,
) -> Result<Json<MyResponse>, ApiError> {
    let sessions = deferred.get().ok_or(ApiError::NotReady)?; // 503 if not ready
    // ... use sessions
}
```

This pattern is used by `polaris_sessions::HttpPlugin` for all session REST endpoints.

## HttpIOProvider: Bridging HTTP to Agent IO

`HttpIOProvider` connects an HTTP handler to the agent's `UserIO` abstraction via tokio channels. The handler pre-loads user input and collects agent output.

```rust
use polaris_app::HttpIOProvider;
use polaris_core_plugins::{IOMessage, UserIO};

// 1. Create channels
let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1);
let provider = Arc::new(provider);

// 2. Send user message, then close input
input_tx.send(IOMessage::user_text("hello")).await?;
drop(input_tx);

// 3. Execute turn, injecting the IO provider
let io = Arc::clone(&provider);
let result = sessions.try_process_turn_with(&session_id, move |ctx| {
    ctx.insert(UserIO::new(io));
}).await?;

// 4. Collect output messages
let mut messages = Vec::new();
while let Ok(msg) = output_rx.try_recv() {
    messages.push(msg);
}
```

### Channel Design

- **Input channel**: Bounded (typically buffer size 1 for single-message turns). Sender is dropped after loading to signal end-of-input.
- **Output channel**: Unbounded to prevent deadlock — the handler only drains after the turn completes. A bounded output would deadlock if the agent produced more messages than capacity.

## Full Flow: HTTP Request to Agent Response

Here is the complete flow from the sessions `HttpPlugin` handler:

```text
HTTP POST /v1/sessions/{id}/turns
  │
  ├── 1. Extract DeferredState → get SessionsAPI (503 if not ready)
  ├── 2. Create HttpIOProvider (input_tx, output_rx)
  ├── 3. Send user message via input_tx, drop sender
  ├── 4. Call sessions.try_process_turn_with()
  │       ├── Lock session context (or return 409 SessionBusy)
  │       ├── Inject SessionInfo (session_id, turn_number)
  │       ├── Call setup closure → ctx.insert(UserIO::new(provider))
  │       ├── Execute graph (systems read UserIO via Res<UserIO>)
  │       ├── Auto-checkpoint (if enabled)
  │       └── Return ExecutionResult
  ├── 5. Drain output_rx → collect IOMessages
  └── 6. Return JSON response with messages + execution metadata
```

### Reference Implementation

The canonical example is `polaris_sessions::http::handlers::process_turn`:

```rust
pub(crate) async fn process_turn(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
    Json(body): Json<ProcessTurnRequest>,
) -> Result<Json<ProcessTurnResponse>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);

    let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1);
    let provider = Arc::new(provider);

    input_tx.send(IOMessage::user_text(body.message)).await
        .map_err(|_| ApiError::IoChannelClosed)?;
    drop(input_tx);

    let io_provider = Arc::clone(&provider);
    let result = sessions
        .try_process_turn_with(&session_id, move |ctx| {
            ctx.insert(UserIO::new(io_provider));
        })
        .await?;

    let mut messages = Vec::new();
    while let Ok(msg) = output_rx.try_recv() {
        messages.push(msg);
    }

    let info = sessions.session_info(&session_id)?;
    Ok(Json(ProcessTurnResponse {
        messages,
        execution: TurnExecutionMetadata {
            nodes_executed: result.nodes_executed,
            duration_ms: result.duration.as_millis() as u64,
            turn_number: info.turn_number,
        },
    }))
}
```

## Writing Custom HTTP Handlers

To add your own endpoints that interact with Polaris resources:

### Step 1: Define your plugin with deferred state

```rust
struct MyPlugin {
    state: Arc<OnceLock<MyAPI>>,
}

impl MyPlugin {
    fn new() -> Self {
        Self { state: Arc::new(OnceLock::new()) }
    }
}
```

### Step 2: Register routes in build(), populate state in ready()

```rust
impl Plugin for MyPlugin {
    fn build(&self, server: &mut Server) {
        let state = Arc::clone(&self.state);
        let router = Router::new()
            .route("/my/endpoint", post(handle_request))
            .with_state(state);
        server.api::<HttpRouter>().unwrap().add_routes(router);
    }

    async fn ready(&self, server: &mut Server) {
        let api = server.api::<MyAPI>().unwrap().clone();
        self.state.set(api).unwrap();
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}
```

### Step 3: Write handler functions

```rust
type DeferredMyAPI = Arc<OnceLock<MyAPI>>;

async fn handle_request(
    State(deferred): State<DeferredMyAPI>,
    Json(body): Json<RequestBody>,
) -> Result<Json<ResponseBody>, StatusCode> {
    let api = deferred.get().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    // Use api...
    Ok(Json(ResponseBody { /* ... */ }))
}
```

## Session HTTP Endpoints

When the `http` feature is enabled on `polaris_sessions`, `HttpPlugin` registers 11 REST endpoints:

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/sessions` | Create a new session |
| `GET` | `/v1/sessions` | List live sessions |
| `GET` | `/v1/sessions/stored` | List persisted sessions |
| `GET` | `/v1/sessions/{id}` | Get session info |
| `DELETE` | `/v1/sessions/{id}` | Delete a session |
| `POST` | `/v1/sessions/{id}/turns` | Process a turn |
| `POST` | `/v1/sessions/{id}/checkpoints` | Create a checkpoint |
| `GET` | `/v1/sessions/{id}/checkpoints` | List checkpoints |
| `POST` | `/v1/sessions/{id}/rollback` | Rollback to checkpoint |
| `POST` | `/v1/sessions/{id}/save` | Persist to store |
| `POST` | `/v1/sessions/{id}/resume` | Resume from store |

### Setup

```rust
use polaris_sessions::{SessionsPlugin, HttpPlugin};
use polaris_app::{AppPlugin, AppConfig};

server
    .add_plugins(PersistencePlugin)
    .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
    .add_plugins(AppPlugin::new(AppConfig::new()))
    .add_plugins(HttpPlugin::new());
```

## Key Files

| File | Purpose |
|------|---------|
| `polaris_app/src/plugin.rs` | `AppPlugin` lifecycle, `ServerHandle` |
| `polaris_app/src/router.rs` | `HttpRouter` API for route registration |
| `polaris_app/src/io.rs` | `HttpIOProvider` channel bridging |
| `polaris_app/src/auth.rs` | `AuthProvider` trait |
| `polaris_app/src/config.rs` | `AppConfig` (bind address, CORS) |
| `polaris_sessions/src/http/mod.rs` | `HttpPlugin`, endpoint table |
| `polaris_sessions/src/http/handlers.rs` | Handler implementations |
| `polaris_sessions/src/http/error.rs` | `ApiError` enum |
| `examples/src/bin/http.rs` | Complete HTTP server example |
