---
notion_page: https://www.notion.so/radiant-ai/Design-MCP-Client-32fafe2e695d801fab2ff699ef8a139f
title: "Design: HTTP & MCP Infrastructure"
---

# Design: HTTP & MCP Infrastructure

**Status:** Draft (App infrastructure implemented; HTTP endpoints and MCP planned)
**Layer:** 3 (Plugin-Provided Abstractions)
**Crates:** `polaris_app` (implemented), `polaris_sessions` (HTTP endpoints planned), `polaris_mcp` (planned)
**Date:** 2026-03-31

**Supersedes:** `http-app.md` (planned, never written)

**Roadmap tickets:**
- Phase 1J: sc-3223 — `polaris_app` crate with `AppPlugin` (**implemented**)
- Phase 1K: sc-3224–sc-3227 — HTTP session endpoints
- Phase 1H: sc-3203 — MCP client
- Phase 3B: sc-3218 — MCP server (`polaris-mcp` binary)

---

## Why One Document?

The App infrastructure (`polaris_app`), HTTP session endpoints, and MCP server all share a common HTTP runtime. Documenting them together makes the layering explicit:

```
┌─────────────────────────────────────────────────────────────┐
│  Consumers (register routes during build)                   │
│                                                             │
│  HttpPlugin    McpServerPlugin    (future plugins)          │
│  (Phase 1K)            (Phase 3B)                           │
└──────────────┬──────────────┬──────────────┬────────────────┘
               │              │              │
               ▼              ▼              ▼
┌─────────────────────────────────────────────────────────────┐
│  polaris_app — Shared HTTP Runtime (Phase 1J)               │
│                                                             │
│  AppPlugin     HttpRouter    AppConfig    ServerHandle      │
│  Middleware    HttpIOProvider AuthProvider                  │
└─────────────────────────────────────────────────────────────┘
```

The MCP *client* is independent of `polaris_app` — it connects to external servers, not serves them. It is included here because the MCP *crate* houses both client and server roles, and the server role depends on the App infrastructure.

---

# Part 1: App Infrastructure (`polaris_app`)

*Phase 1J, sc-3223. **Implemented.***

## Motivation

Multiple Polaris products need an HTTP server: `polaris-http` (agent REST API), `polaris-mcp` (MCP server with SSE), `polaris-arena` (web dashboard). Rather than each product embedding its own server setup, `polaris_app` provides a shared axum-based runtime where plugins register route fragments during build and the server merges them with a common middleware stack.

Axum was chosen for:
- Native Tower middleware support (aligns with Polaris's composable middleware philosophy)
- `Router::merge()` for plugin-based route composition
- Built-in SSE support (`axum::response::Sse`)
- Tokio-native runtime alignment

## Components

### `AppConfig`

Server-wide configuration registered as a `GlobalResource`.

```rust
pub struct AppConfig {
    host: String,           // default: "127.0.0.1"
    port: u16,              // default: 3000
    cors_origins: Vec<String>, // default: empty (allows any origin)
}

impl GlobalResource for AppConfig {}

impl AppConfig {
    pub fn new() -> Self;
    pub fn with_host(mut self, host: impl Into<String>) -> Self;
    pub fn with_port(mut self, port: u16) -> Self;
    pub fn with_cors_origin(mut self, origin: impl Into<String>) -> Self;
    pub fn host(&self) -> &str;
    pub fn port(&self) -> u16;
    pub fn cors_origins(&self) -> &[String];
    pub fn addr(&self) -> String; // "host:port"
}
```

### `HttpRouter`

Build-time API for route registration. Plugins call `add_routes()` during their `build()` phase; `AppPlugin` consumes all registered fragments in `ready()`.

```rust
pub struct HttpRouter {
    routes: RwLock<Vec<axum::Router>>,
    auth: RwLock<Option<Arc<dyn AuthProvider>>>,
}

impl API for HttpRouter {}

impl HttpRouter {
    pub fn add_routes(&self, router: axum::Router);
    pub fn set_auth(&self, provider: impl AuthProvider);
    pub(crate) fn take_routes(&self) -> Vec<axum::Router>;
    pub(crate) fn take_auth(&self) -> Option<Arc<dyn AuthProvider>>;
}
```

Interior mutability via `RwLock` allows multiple plugins to register routes on the same `&HttpRouter` reference without exclusive ownership.

**Usage pattern:**

```rust
impl Plugin for MyPlugin {
    fn build(&self, server: &mut Server) {
        let router = Router::new()
            .route("/v1/my-endpoint", get(handler))
            .with_state(my_state);

        server.api::<HttpRouter>()
            .expect("AppPlugin must be added first")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}
```

### `AuthProvider`

Pluggable authentication via a synchronous trait. Synchronous by design — most auth only requires inspecting headers.

```rust
pub trait AuthProvider: Send + Sync + std::fmt::Debug + 'static {
    fn authenticate(&self, parts: &http::request::Parts) -> Result<(), AuthRejection>;
}

pub type AuthRejection = Box<axum::response::Response>;
```

Registered via `HttpRouter::set_auth()` during build. Only one provider is supported (last registration wins). Auth is applied as middleware between CORS and tracing so that CORS preflight passes without auth but rejected requests are still traced.

### `ServerHandle`

Registered as an `API` during `ready()`. Other plugins access it via `server.api::<ServerHandle>()` to trigger graceful shutdown. Uses `API` rather than `GlobalResource` because it is a plugin-only control surface, not a resource accessed by systems at execution time.

```rust
pub struct ServerHandle {
    shutdown_tx: parking_lot::Mutex<Option<watch::Sender<bool>>>,
    handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl API for ServerHandle {}

impl ServerHandle {
    /// Sends the shutdown signal. Returns `true` if sent, `false` if already shut down.
    pub fn shutdown(&self) -> bool;
}
```

### Tower Middleware Stack

Applied in `ready()` after merging all route fragments. Execution order for incoming requests (inside-out):

| Order | Layer | Purpose |
|-------|-------|---------|
| 1 (outer) | **SetRequestIdLayer** | Injects UUID `x-request-id` header |
| 2 | **TraceLayer** | Logs request/response spans via `tracing` |
| 3 | **Auth** (optional) | `AuthProvider` check; CORS preflight bypasses |
| 4 | **CorsLayer** | Configurable allowed origins, methods, headers |
| 5 (inner) | **PropagateHeaderLayer** | Copies `x-request-id` from request to response |

```rust
pub(crate) fn apply_middleware(
    router: axum::Router,
    config: &AppConfig,
    auth: Option<Arc<dyn AuthProvider>>,
) -> axum::Router;

pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
```

CORS with empty origins allows any origin — restrict in production via `AppConfig::with_cors_origin()`.

### `HttpIOProvider`

Bridges HTTP request/response to the Polaris agent IO system via tokio channels. Created per-request.

```rust
pub struct HttpIOProvider {
    input_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    output_tx: mpsc::UnboundedSender<IOMessage>,
}

impl HttpIOProvider {
    /// Returns (provider, input_tx, output_rx).
    /// Give the provider to UserIO::new().
    /// HTTP handler sends via input_tx and collects from output_rx.
    /// Output channel is unbounded to prevent deadlock when the agent
    /// produces more messages than a bounded buffer would allow.
    pub fn new(input_buffer: usize) -> (Self, mpsc::Sender<IOMessage>, mpsc::UnboundedReceiver<IOMessage>);
}

impl IOProvider for HttpIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError>;
    async fn receive(&self) -> Result<IOMessage, IOError>;
}
```

**Flow:**

```
HTTP handler                HttpIOProvider               Agent graph
───────────                 ──────────────               ───────────
 input_tx.send(msg)  ──────► input_rx ──────────────► UserIO.receive()
 output_rx.recv()    ◄────── output_tx ◄──────────── UserIO.send(msg)
```

Both directions return `IOError::Closed` when the counterpart channel drops.

## `AppPlugin` Lifecycle

```rust
pub struct AppPlugin {
    config: AppConfig,
    listener: Mutex<Option<tokio::net::TcpListener>>,
}

impl Plugin for AppPlugin {
    const ID: &'static str = "polaris::app";
    const VERSION: Version = Version::new(0, 0, 1);
}
```

### `AppPlugin::with_listener()`

Injects a pre-bound `TcpListener` into `AppPlugin`. When set, `ready()` uses this listener instead of binding from `AppConfig`. This eliminates TOCTOU port races in tests: bind to port `0`, read the assigned port, then pass the listener to guarantee the port is reserved.

```rust
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
let port = listener.local_addr()?.port();

server.add_plugins(
    AppPlugin::new(AppConfig::new().with_port(port))
        .with_listener(listener),
);
```

| Phase | Behavior |
|-------|----------|
| **`build()`** | Inserts `AppConfig` as global resource. Registers `HttpRouter` as build-time API. |
| **`ready()`** | Takes all route fragments from `HttpRouter`, merges into single `axum::Router`, applies middleware stack. Uses injected listener if available, otherwise binds to `AppConfig::addr()`. Spawns axum server on background tokio task. Registers `ServerHandle` as an API. |
| **`cleanup()`** | Sends shutdown signal via `ServerHandle`, awaits graceful drain with 5-second timeout. |

No dependencies — `AppPlugin` is a foundation plugin that other plugins depend on.

---

# Part 2: HTTP Session Endpoints

*Phase 1K, sc-3224–sc-3227. Session management endpoints (sc-3224) **implemented**; turn processing (sc-3225) **implemented**; checkpoints (sc-3226) and SSE streaming (sc-3227) **planned**.*

## Motivation

`polaris_sessions` provides session management (create, turns, checkpoints, rollback) as an in-process API (`SessionsAPI`). HTTP endpoints wrap this API for web UIs, mobile clients, and service-to-service integration. The endpoints live in `polaris_sessions` behind a `feature = "http"` flag — keeping transport and domain in one crate while remaining fully additive.

## `HttpPlugin`

Registers REST + SSE routes via `HttpRouter`. Declares dependencies on both `AppPlugin` and `SessionsPlugin`. Uses a `DeferredState` pattern (see below) to bridge the gap between route registration in `build()` and `SessionsAPI` availability in `ready()`.

```rust
pub(crate) type DeferredState = Arc<OnceLock<SessionsAPI>>;

pub struct HttpPlugin {
    state: DeferredState,
}

impl Plugin for HttpPlugin {
    const ID: &'static str = "polaris::sessions::http";

    fn build(&self, server: &mut Server) {
        let state = Arc::clone(&self.state);
        let router = Router::new()
            .route("/v1/sessions", post(create_session).get(list_sessions))
            .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
            .with_state(state);

        server.api::<HttpRouter>()
            .expect("AppPlugin required")
            .add_routes(router);
    }

    async fn ready(&self, server: &mut Server) {
        let sessions = server.api::<SessionsAPI>()
            .expect("SessionsPlugin required");
        self.state.set(sessions).unwrap();
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<AppPlugin>(),
            PluginId::of::<SessionsPlugin>(),
        ]
    }
}
```

### `DeferredState` Pattern

Axum routes must be registered with their state during `build()`, but `SessionsAPI` is not available until `ready()`. `DeferredState` bridges this gap using `Arc<OnceLock<SessionsAPI>>`:

1. **`build()`** — routes are registered with an empty `DeferredState` handle
2. **`ready()`** — `SessionsAPI` is written into the `OnceLock`
3. **Handlers** — extract the API via `deferred.get()`, returning `ApiError::NotReady` (503) if the lock has not been filled

This pattern is reusable by any plugin that needs to register routes before its state is fully initialized.

### `SessionMetadata`

Response type for session information, serialized to JSON by the HTTP handlers.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMetadata {
    pub session_id: SessionId,
    pub agent_type: AgentTypeId,
    pub turn_number: u32,
    pub created_at: String, // ISO 8601 UTC
    pub status: SessionStatus,
}
```

### `ApiError`

Enum mapping domain errors to HTTP status codes. Each variant produces a JSON error body with a machine-readable `code` and human-readable `message`.

| Variant | HTTP Status | When |
|---------|-------------|------|
| `NotReady` | `503 Service Unavailable` | `DeferredState` not yet initialized (server still starting) |
| `Session(SessionError)` | Depends on inner | Domain errors from `SessionsAPI` (404, 400, 422, 500) |

## Endpoints

### Session Management (sc-3224)

| Method | Path | Description | Response |
|--------|------|-------------|----------|
| `POST` | `/v1/sessions` | Create session | `201 Created` + session info |
| `GET` | `/v1/sessions` | List sessions | `200 OK` + session ID array |
| `GET` | `/v1/sessions/{id}` | Get session info | `200 OK` + session detail |
| `DELETE` | `/v1/sessions/{id}` | Delete session | `204 No Content` |

**Create session request:**

```json
{
  "agent_type": "claude_code",
  "session_id": "optional-custom-id"
}
```

**Session info response:**

```json
{
  "session_id": "session_abc123",
  "agent_type": "claude_code",
  "turn_number": 0,
  "created_at": "2026-03-31T12:00:00Z",
  "status": "active"
}
```

**Error response format:**

```json
{
  "error": {
    "code": "session_not_found",
    "message": "Session session_xyz does not exist"
  }
}
```

| Error | HTTP Status |
|-------|-------------|
| `SessionNotFound` | 404 |
| `AgentNotFound` | 400 |
| `GraphValidation` | 422 |
| `Store` | 500 |

### Turn Processing (sc-3225)

`POST /v1/sessions/{id}/turns`

Uses `HttpIOProvider` to bridge the request body into the agent's `UserIO.receive()` and collect `UserIO.send()` output into the response.

**Request:**

```json
{
  "message": "Fix the bug in auth.rs"
}
```

**Buffered JSON response** (default):

```json
{
  "messages": [
    {
      "content": { "Text": "I'll look at auth.rs..." },
      "source": { "Agent": "planner" },
      "metadata": {}
    }
  ],
  "execution": {
    "nodes_executed": 5,
    "duration_ms": 1200,
    "turn_number": 3
  }
}
```

Messages are serialized `IOMessage` values directly — no intermediate DTO.

**IO bridging flow:**

1. Handler creates `HttpIOProvider::new(1)`, gets `(provider, input_tx, output_rx)`
2. Handler sends user message via `input_tx`, then drops `input_tx`
3. Handler calls `sessions.try_process_turn_with(id, |ctx| { ctx.insert(UserIO::new(provider)); })`
4. Agent graph executes, reading from `UserIO.receive()` and writing to `UserIO.send()`
5. Handler drains all messages from `output_rx` into response body

### SSE Streaming (sc-3227)

`POST /v1/sessions/{id}/turns` with `Accept: text/event-stream`

Returns an SSE stream via `axum::response::Sse`. A streaming `HttpIOProvider` variant forwards `UserIO.send()` calls as real-time SSE events rather than buffering.

**Streaming granularity:** The current `IOStream` type (`Pin<Box<dyn Stream<Item = Result<IOMessage, IOError>>>>`) yields whole `IOMessage`s. For true SSE streaming, partial LLM tokens need to flow incrementally — SSE clients (web UIs, MCP SSE transport) expect token-by-token delivery, not waiting for a complete message. This requires:

1. A sub-message streaming mechanism — either chunking `IOContent::Text` into partial tokens at the `IOProvider` level, or introducing a new `IOContent::TextDelta(String)` variant for incremental text.
2. `Serialize` on `IOSource`, `IOContent`, and `IOMessage` for `axum::response::sse::Event::json_data()`.
3. Integration with `LlmProvider::stream()` / `StreamEvent` so LLM token deltas flow through the IO system without buffering into complete messages first.

This is a cross-cutting concern touching `polaris_core_plugins` (IO types), `polaris_app` (SSE adapter), and `polaris_model_providers`. It should be a separate ticket from sc-3227, which can initially ship with message-level granularity and upgrade to token-level streaming once the IO layer supports it.

**SSE event types:**

| Event | Data | When |
|-------|------|------|
| `message` | Agent text output | `UserIO.send()` with text content |
| `tool_call` | Tool name + arguments | Tool invocation begins |
| `tool_result` | Tool output | Tool invocation completes |
| `done` | `ExecutionResult` JSON | Turn complete |

**Example SSE stream:**

```
event: message
data: {"content": {"type": "text", "text": "I'll look at auth.rs..."}, "source": "agent"}

event: tool_call
data: {"tool": "read", "arguments": {"path": "src/auth.rs"}}

event: tool_result
data: {"tool": "read", "result": "...file contents..."}

event: message
data: {"content": {"type": "text", "text": "Found the issue..."}, "source": "agent"}

event: done
data: {"status": "completed", "turn_number": 3, "duration_ms": 1200}
```

Falls back to buffered JSON response when the `Accept` header does not request `text/event-stream`.

Builds on completed `LlmProvider::stream()` + `StreamEvent` infrastructure.

### Checkpoint & Rollback (sc-3226)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/sessions/{id}/checkpoints` | Create checkpoint at current turn |
| `GET` | `/v1/sessions/{id}/checkpoints` | List checkpoints with turn numbers |
| `POST` | `/v1/sessions/{id}/rollback` | Rollback to specified turn |

**Rollback request:**

```json
{
  "turn_number": 2
}
```

Thin wrappers over existing `SessionsAPI::checkpoint()` and `SessionsAPI::rollback()`.

---

# Part 3: MCP Client

*Phase 1H, sc-3203. **Planned.***

## Motivation

Polaris needs to consume external MCP tool servers as if they were native Polaris tools. The challenge is not just transport plumbing:

1. **Tool schemas must be available before the first model call.** The registry must be populated at build time, even though actual server connections are deferred to runtime.
2. **MCP tool execution is remote.** Transport failure, server restarts, and protocol errors must translate cleanly into Polaris `ToolError`s.
3. **Namespacing is required.** External tools cannot be allowed to collide with native Polaris tool names.

This design targets the MCP protocol version pinned in the roadmap, `2025-11-25`, and bridges MCP tools into `ToolRegistry` as native tools.

## Goals

- Support MCP servers over stdio and Streamable HTTP (with backwards compatibility for `2024-11-05` HTTP+SSE servers).
- Register bridge tools into `ToolRegistry` at build time; connect and discover lazily on first use.
- Expose each remote tool as a normal Polaris `Tool`.
- Preserve remote input schemas.
- Keep reconnection behavior safe for potentially side-effecting tool calls.

## Non-Goals

- MCP prompts and resources in the first implementation.
- MCP server mode. That is a separate roadmap item (sc-3218). See [Part 4: MCP Server](#part-4-mcp-server) below.
- Protocol versions newer than `2025-11-25` in this ticket.

## Components

```rust
pub struct McpPlugin {
    pub servers: Vec<McpServerConfig>,
}

pub struct McpRegistry {
    clients: IndexMap<String, Arc<McpClient>>,
}

pub struct McpClient {
    server_name: String,
    config: McpServerConfig,
    state: Mutex<ClientState>,
}

enum ClientState {
    Disconnected,
    Connected {
        transport: Arc<dyn McpTransport>,
        tools: Vec<McpToolSpec>,
    },
}

pub struct McpToolBridge {
    namespaced_name: String,
    server_name: String,
    remote_name: String,
    client: Arc<McpClient>, // lazily initialized on first execute()
}
```

## Startup Model

`McpPlugin::build()` registers `McpToolBridge` entries into `ToolRegistry` using configuration data, but does **not** connect to any MCP server. Connection, protocol initialization, and tool discovery are deferred to the first `execute()` call on any bridge tool from a given server (lazy initialization).

This works because `build()` is sync and `ToolRegistry` is mutable only during build. By requiring tool schemas in the plugin config (or registering a generic passthrough schema when schemas are omitted), the plugin can populate `ToolRegistry` without network I/O. Actual transport setup happens at runtime when a tool is first invoked.

**Schema availability strategies (ordered by preference):**

1. **Pre-declared schemas in config** — each server entry lists tool names and their input schemas. `build()` registers fully-typed bridge tools. This is the recommended path.
2. **No pre-declared schemas** — `build()` registers bridge tools with a generic passthrough schema (`{ "type": "object", "additionalProperties": true }`). On first connect the bridge discovers the real schemas and can update its validation, but the registry entry already exists.

**Tradeoff:** the first tool call to a given server is slower (transport open + `initialize` + `tools/list`), and discovery errors surface at runtime rather than startup. This is the correct tradeoff for remote services that may go down independently of the Polaris process — a server that was healthy at startup can still fail seconds later, so build-time connectivity checks provide only false confidence.

`McpPlugin` depends on `ToolsPlugin` and mutates the build-phase `ToolRegistry` directly.

## Transport Model

Two transports are supported, matching MCP `2025-11-25`:

- `StdioTransport` — child process, JSON-RPC over stdin/stdout
- `StreamableHttpTransport` — single HTTP endpoint, POST for JSON-RPC, optional SSE response streaming

Both expose one common request/response interface:

```rust
pub trait McpTransport: Send + Sync + 'static {
    fn initialize(&self) -> Result<InitializeResult, McpError>;
    fn list_tools(&self) -> Result<Vec<McpToolSpec>, McpError>;
    fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<McpToolResult, McpError>> + Send + '_>>;
}
```

The interface is intentionally tool-focused. Prompts and resources can extend it later without changing the bridge contract.

## Configuration

### `McpServerConfig`

```rust
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransportConfig,
    pub tools: Vec<McpToolSchema>,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub reconnect: ReconnectPolicy,
}

pub struct McpToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub enum McpTransportConfig {
    Stdio(StdioConfig),
    StreamableHttp(StreamableHttpConfig),
}
```

### Naming Rules

Each bridged tool is exposed as:

```text
mcp_<server_name>__<tool_name>
```

Examples:

- `mcp_github__create_issue`
- `mcp_linear__search_issues`

Rules:

- `server_name` is required and user-supplied in config
- names are lowercased and non-alphanumeric characters normalize to `_`
- if normalization causes a collision, plugin build fails fast

The `mcp_` prefix avoids ambiguity with native tools and keeps all remote tools easy to identify.

## Protocol Lifecycle

### Initialization

Initialization is lazy — it happens on the first `execute()` call to any `McpToolBridge` whose server has not yet been connected. Each `McpClient` tracks its connection state and performs the handshake at most once:

1. open transport (spawn process for stdio, or POST `InitializeRequest` to MCP endpoint for Streamable HTTP)
2. send `initialize`
3. verify the negotiated protocol version is `2025-11-25`
4. send `notifications/initialized`
5. call `tools/list` (validates that expected tools exist on the remote; logs warnings for mismatches against config)

If any step fails, the `execute()` call that triggered initialization returns `ToolError::ExecutionError` with the underlying `McpError`. Subsequent calls will re-attempt initialization (the client resets to unconnected state on failure).

**At build time**, `McpPlugin::build()` only registers `McpToolBridge` entries into `ToolRegistry` using schemas from config. No transport is opened and no network I/O occurs.

### Shutdown

- stdio transport terminates the child process
- Streamable HTTP transport sends `DELETE` to MCP endpoint with `MCP-Session-Id` to explicitly terminate the session, then closes HTTP client state

Runtime shutdown is best-effort only.

## Transport Details

### `StdioTransport`

`StdioTransport` spawns a child process and speaks JSON-RPC over stdin/stdout.

Configuration:

```rust
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_dir: Option<PathBuf>,
}
```

Behavior:

- one long-lived child process per server
- one background reader loop demultiplexing responses by request ID
- stderr captured for logging/debugging only, never parsed as protocol data

### `StreamableHttpTransport`

`StreamableHttpTransport` communicates with a remote MCP server over Streamable HTTP — the primary HTTP transport in MCP `2025-11-25`, replacing the standalone HTTP+SSE transport from `2024-11-05`.

The server exposes a single MCP endpoint. The client sends all JSON-RPC messages as HTTP POST requests. The server responds with either `application/json` (single response) or `text/event-stream` (SSE stream that may include server-initiated requests/notifications before the final response).

Configuration:

```rust
pub struct StreamableHttpConfig {
    pub url: Url,
    pub headers: BTreeMap<String, String>,
}
```

Behavior:

- all client→server messages are HTTP POST to the single MCP endpoint
- client includes `Accept: application/json, text/event-stream` on all POSTs
- server may respond with JSON or SSE; client handles both
- optional GET to the MCP endpoint opens an SSE stream for server-initiated messages (notifications, requests unrelated to any in-flight POST)
- session management via `MCP-Session-Id` header (assigned by server in `InitializeResult` response, included by client on all subsequent requests)
- `MCP-Protocol-Version: 2025-11-25` header on all requests after initialization
- resumability via SSE event IDs and `Last-Event-ID` header on reconnect
- reconnects SSE streams on disconnect using backoff

### Backwards Compatibility with `2024-11-05`

For servers still running the old HTTP+SSE transport, the client follows the MCP spec's backwards compatibility procedure:

1. POST `InitializeRequest` to the server URL
2. If it succeeds → Streamable HTTP
3. If it fails with 400/404/405 → GET the URL expecting an SSE stream with an `endpoint` event → old HTTP+SSE transport

This detection is automatic and transparent to `McpToolBridge`.

## Tool Schema Translation

### Definition Mapping

Each MCP tool becomes a Polaris `ToolDefinition`:

```rust
ToolDefinition {
    name: namespaced_name,
    description: remote.description.unwrap_or_default(),
    parameters: normalized_input_schema,
}
```

### Schema Normalization Rules

1. If the MCP tool has an object `inputSchema`, pass it through unchanged.
2. If the schema is absent, use:

    ```json
    { "type": "object", "properties": {}, "additionalProperties": true }
    ```

3. If the schema root is not an object, build fails for that tool.

Polaris tool calls are object-argument based. Failing fast on non-object schemas is better than inventing implicit wrappers that the model never sees documented.

### Execution Result Shape

`McpToolBridge::execute()` returns JSON with the remote result preserved:

```json
{
  "server": "github",
  "tool": "create_issue",
  "content": [...],
  "structured_content": null,
  "is_error": false
}
```

If the remote server returns structured content metadata later, it should be placed in `structured_content` without changing the outer shape.

## Error Mapping

### Internal Error Type

```rust
pub enum McpError {
    Startup(String),
    Transport(String),
    Protocol(String),
    Timeout(String),
    RemoteToolError(String),
}
```

### Mapping to `ToolError`

At runtime, bridge errors are translated as:

- invalid local arguments before the request is sent -> `ToolError::ParameterError`
- transport disconnect or timeout -> `ToolError::ExecutionError`
- protocol violation -> `ToolError::ExecutionError`
- remote tool application error -> `ToolError::ExecutionError`

Build-time failures map to `ToolError::RegistryError` if surfaced while registering tools.

### Unknown Tools

If a tool disappears after startup, `McpToolBridge` still exists locally but the remote call fails with `ToolError::ExecutionError("remote tool no longer available")`. Polaris should not silently unregister tools mid-session.

## Reconnection Strategy

### Policy

```rust
pub struct ReconnectPolicy {
    pub max_attempts: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}
```

### Safe Retry Rules

The client distinguishes **reconnect** from **retry**.

- `initialize` and `tools/list` may be retried automatically after reconnect.
- `tools/call` may reconnect the transport but must **not** replay the remote call automatically once the request may have been sent.

This avoids duplicating side effects on remote systems such as issue creation, deploys, or shell commands.

### Runtime Behavior

If a request fails before bytes are written, the bridge may reconnect and retry once.

If a request fails after write but before a response arrives:

- reconnect the client
- return an execution error
- let the caller decide whether retrying is safe

## `McpPlugin`

`McpPlugin` depends on `ToolsPlugin`. It has **no dependency** on `AppPlugin` — the MCP client is independent of the shared HTTP runtime.

Build-phase behavior (`build()` — sync, no network I/O):

1. load `ToolRegistry` mutably from the server
2. for each configured server, create an `Arc<McpClient>` in `Disconnected` state
3. for each tool declared in the server's `tools` config, register an `McpToolBridge` into `ToolRegistry` using the pre-declared schema
4. if a server config has an empty `tools` list, register a single sentinel bridge per server that discovers and dynamically registers tools on first connect (fallback path — pre-declared schemas are preferred)
5. insert `McpRegistry` as a global resource for diagnostics and future APIs

`build()` stays sync by deferring all network I/O to runtime. The first `McpToolBridge::execute()` call for a given server triggers lazy initialization: transport open, `initialize` handshake, and `tools/list` validation. This means:

- Build never blocks on remote servers.
- Discovery errors surface at runtime as `ToolError::ExecutionError`.
- The first tool call to each server pays a one-time latency cost for connection setup.

This is the correct tradeoff for remote services whose availability is independent of the Polaris process lifecycle.

```rust
impl Plugin for McpPlugin {
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }
}
```

---

# Part 4: MCP Server

*Phase 3B, sc-3218. **Planned.***

When Polaris itself *serves* as an MCP server (the `polaris-mcp` product), it uses `polaris_app` infrastructure to expose Polaris agent patterns as MCP tools over SSE + JSON-RPC.

## How It Uses App Infrastructure

- **Route registration via `HttpRouter`**: MCP server endpoints (SSE event stream, JSON-RPC request handler) are registered via `HttpRouter::add_routes()` during `build()`, following the same composition pattern as `HttpPlugin`.
- **Tower middleware**: The shared middleware stack — CORS, request tracing, request ID injection — applies automatically to MCP server endpoints with no additional code.
- **`HttpIOProvider`**: Bridges SSE connections from MCP clients to the Polaris agent IO system via tokio channels. The agent processes incoming MCP tool calls through `UserIO.receive()` and emits results through `UserIO.send()`, which the HTTP handler sends back as SSE events or JSON-RPC responses.
- **`AuthProvider`**: MCP server can register an `AuthProvider` to gate access to exposed tools.
- **Graceful shutdown**: `AppPlugin` handles connection draining via `ServerHandle`, ensuring in-flight MCP requests complete before the server stops.

## Crate Structure

The `polaris_mcp` crate supports both client and server roles without coupling them:

```
polaris_mcp
├── client/              # MCP client (sc-3203)
│   ├── McpClient        # Lazy connection management
│   ├── McpToolBridge     # Polaris Tool impl wrapping remote MCP tools
│   ├── McpPlugin         # Registers bridges into ToolRegistry
│   ├── StdioTransport    # Spawns child process, JSON-RPC over stdin/stdout
│   └── StreamableHttpTransport  # Streamable HTTP (POST + optional SSE)
├── server/              # MCP server (sc-3218)
│   ├── McpServerPlugin   # Registers SSE + JSON-RPC routes via HttpRouter
│   └── uses HttpIOProvider to bridge MCP clients → agent IO
└── protocol/            # Shared protocol types
    ├── McpError
    ├── McpToolSpec
    ├── McpToolResult
    └── JSON-RPC message types
```

## Dependency Graph

```
McpPlugin (client)
  └── depends on: ToolsPlugin

McpServerPlugin (server)
  ├── depends on: AppPlugin     ← shared HTTP runtime
  └── depends on: ToolsPlugin   ← reads registry to advertise tools as MCP tools
```

The client side has **no dependency** on `polaris_app`. The server side declares `AppPlugin` as a plugin dependency, which ensures:
- `HttpRouter` API is available during `build()` for route registration
- `AppConfig` global resource is available for reading server configuration
- `AppPlugin::ready()` runs first, starting the HTTP server before the MCP server plugin attempts to serve

## Shared Protocol Types

While the client and server transports are distinct (outgoing vs. incoming), they share protocol-level types:

| Concern | Client (`StreamableHttpTransport`) | Server (`McpServerPlugin`) |
|---------|------------------------|---------------------------|
| Protocol version | MCP `2025-11-25` | MCP `2025-11-25` |
| HTTP + SSE | POSTs JSON-RPC, consumes optional SSE responses | Produces SSE events via axum `Sse` response type |
| JSON-RPC | Sends requests, receives responses | Receives requests, sends responses |
| Tool schemas | Reads remote schemas → registers into `ToolRegistry` | Reads local `ToolRegistry` → advertises via `tools/list` |
| Reconnection | `ReconnectPolicy` for outgoing connections | Connection lifecycle managed by `AppPlugin` graceful shutdown |
| IO bridging | Direct: `McpToolBridge` calls transport | Channel-based: `HttpIOProvider` bridges HTTP ↔ agent `UserIO` |

Protocol types (`McpError`, `McpToolSpec`, `McpToolResult`, JSON-RPC request/response/notification envelopes) live in the shared `protocol` module and are used by both client and server.

## Why Not Share Transport Code?

The client's `StreamableHttpTransport` (outgoing `reqwest` POST + SSE response parsing) and the server's MCP endpoint (incoming axum POST handler + `Sse` response streaming) solve opposite problems. Attempting to unify them behind a common abstraction would create a leaky interface that fits neither role well. The correct sharing boundary is the protocol message types (JSON-RPC envelopes, `MCP-Session-Id` / `MCP-Protocol-Version` header constants), not the transport mechanics.

---

# Implementation Plan

## Phase 1J: App Infrastructure (**Done**)

`polaris_app` crate with `AppPlugin`, `HttpRouter`, `AppConfig`, `HttpIOProvider`, Tower middleware stack, `AuthProvider`, `ServerHandle`.

## Phase 1K: HTTP Session Endpoints

1. Add `feature = "http"` to `polaris_sessions` with `polaris_app` dependency.
2. Implement `HttpPlugin` with route registration.
3. **sc-3224**: Session CRUD endpoints (`POST/GET/DELETE /v1/sessions`).
4. **sc-3225**: Turn processing endpoint with `HttpIOProvider` IO bridging.
5. **sc-3226**: Checkpoint and rollback endpoints.
6. **sc-3227**: SSE streaming variant for turn execution (`Accept: text/event-stream`).

## Phase 1H: MCP Client

1. Create `polaris_mcp` crate with `protocol/` module.
2. Add config types (`McpServerConfig`, `McpToolSchema`), `McpError`, `McpToolSpec`, `McpToolResult`.
3. Implement `StdioTransport`.
4. Implement `McpClient` with lazy `ClientState` (Disconnected / Connected).
5. Implement `McpToolBridge` with lazy initialization on first `execute()`.
6. Implement `McpPlugin` — sync `build()` registers bridges from config schemas, no network I/O.
7. Add tests:
   - `build()` registers tools into `ToolRegistry` without connecting to any server
   - first `execute()` triggers initialization handshake
   - initialization failure returns `ToolError::ExecutionError` and resets to `Disconnected`
   - subsequent calls re-attempt initialization after failure
   - namespacing is stable and collision-safe
   - remote tool errors map to `ToolError::ExecutionError`
8. Implement `StreamableHttpTransport` (POST + optional SSE response, session management via `MCP-Session-Id`).
9. Add backwards compatibility detection for `2024-11-05` HTTP+SSE servers.
10. Add reconnect policy handling.
11. Add tests:
    - SSE stream reconnect on disconnect with `Last-Event-ID` resumption
    - no automatic replay of possibly side-effecting `tools/call`
    - lazy initialization fails cleanly on protocol-version mismatch
    - first-call latency is bounded by `startup_timeout`
    - backwards compatibility detection: Streamable HTTP vs. legacy HTTP+SSE
    - `MCP-Session-Id` and `MCP-Protocol-Version` headers sent on all requests

## Phase 3B: MCP Server

1. Add `server/` module to `polaris_mcp` behind `feature = "server"` with `polaris_app` dependency.
2. Implement `McpServerPlugin` — registers SSE + JSON-RPC routes via `HttpRouter`.
3. Build `polaris-mcp` binary wiring `AppPlugin` + `McpServerPlugin` + agent patterns.

---

# Open Questions

1. MCP `2025-11-25` deprecates the standalone HTTP+SSE transport in favor of Streamable HTTP. The client should implement backwards compatibility detection (POST → fallback to GET+SSE) for older servers, but Streamable HTTP is the primary path.
2. MCP prompts and resources are intentionally omitted. If Polaris later wants to expose them, that should happen through explicit new bridge types rather than overloading the tool bridge.
3. `HttpPlugin` endpoint paths (`/v1/sessions`) should align with any future OpenAPI spec. The `/v1/` prefix leaves room for breaking changes via version bumps.
4. SSE streaming for turn execution (sc-3227) has two levels: message-level (each `IOMessage` becomes an SSE event) and token-level (partial LLM tokens stream incrementally). The current `IOStream` yields whole `IOMessage`s and `HttpIOProvider` does not implement `stream()`. Message-level SSE can ship first; token-level streaming requires changes to `IOContent` (e.g., a `TextDelta` variant), `Serialize` impls on IO types, and integration with `LlmProvider::stream()` / `StreamEvent`. This should be a separate ticket.
