---
notion_page: https://www.notion.so/radiant-ai/Sessions-342afe2e695d80f8a9bef73eb04d596d
title: Sessions
---

# Sessions

`polaris_sessions` provides server-managed sessions that own live `SystemContext` instances. Sessions handle context creation, graph execution, checkpointing, and persistence through the `SessionsAPI`.

## Overview

A session binds a `SystemContext` to an agent's graph and executor, managing the lifecycle of a single agent conversation. The `SessionsAPI` is the primary interface — it is registered as an `API` by `SessionsPlugin` and accessed via `server.api::<SessionsAPI>()`.

```rust
use polaris_sessions::{SessionsAPI, SessionsPlugin, SessionId};
use polaris_sessions::store::memory::InMemoryStore;

// Setup
server.add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
server.run().await;

let sessions = server.api::<SessionsAPI>().unwrap();
```

`SessionsAPI` is cheaply cloneable (backed by `Arc<SessionsInner>`) — suitable for sharing across HTTP handlers and background tasks.

## Agent Registration

Before creating sessions, register agent types:

```rust
sessions.register_agent(MyReActAgent)?;
```

Registration validates the agent's graph at registration time. Structural errors return `SessionError::GraphValidation`. Warnings are logged but do not prevent registration.

```rust
// Query registered agents
let agents: Vec<&str> = sessions.registered_agents();
let agent_type: Option<AgentTypeId> = sessions.find_agent_type("react");
```

## Session Lifecycle

### Create

```rust
let ctx = sessions.create_context();
let session_id = SessionId::default(); // random ID

// Basic creation
sessions.create_session(ctx, &session_id, &agent_type)?;

// With initializer (inject resources before first turn)
sessions.create_session_with(ctx, &session_id, &agent_type, |ctx| {
    ctx.insert(AgentConfig::new("claude-sonnet-4-6"));
})?;

// With custom executor settings
sessions.create_session_with_executor(
    ctx, &session_id, &agent_type,
    GraphExecutor::new().with_default_max_iterations(10),
    |ctx| { ctx.insert(AgentConfig::new("claude-sonnet-4-6")); },
)?;
```

The `init` closure runs after the agent's `setup()` method, receiving the context for per-session resource injection.

### Delete

```rust
sessions.delete_session(&session_id).await?;
```

## Turn Execution

### process_turn

Acquires the session's context lock (waits if busy), executes the agent's graph, and returns the result.

```rust
let result = sessions.process_turn(&session_id).await?;
// result.nodes_executed, result.duration
```

### process_turn_with

Same as `process_turn`, but accepts a setup closure to inject per-turn resources before execution.

```rust
let result = sessions.process_turn_with(&session_id, |ctx| {
    ctx.insert(UserIO::new(io_provider));
    ctx.insert(TurnConfig { temperature: 0.7 });
}).await?;
```

### try_process_turn_with

Non-blocking variant — returns `SessionError::SessionBusy` immediately if the session is already executing, instead of waiting for the lock.

```rust
let result = sessions.try_process_turn_with(&session_id, |ctx| {
    ctx.insert(UserIO::new(io_provider));
}).await?;
```

### Execution Flow

When any `process_turn` variant is called:

1. **Lock context** — `tokio::sync::Mutex` guards the `SystemContext`
2. **Inject SessionInfo** — session ID and turn number inserted into context
3. **Call setup closure** — caller injects per-turn resources (e.g., `UserIO`)
4. **Execute graph** — `GraphExecutor::execute()` runs the agent's graph with hooks and middleware
5. **Auto-checkpoint** — if enabled, context is serialized (in-memory, does not block result)
6. **Increment turn number** — atomic counter advanced
7. **Return** `ExecutionResult` — `{ nodes_executed, duration, final_output }`

## Recipes

### One-Shot Execution

The stateless "run once, extract result, clean up" pattern is the common case for synchronous HTTP endpoints and short-lived background jobs. `run_oneshot` encapsulates the full lifecycle — create session, execute turn, extract typed output, delete session — in a single call:

```rust
let sessions = server.api::<SessionsAPI>().unwrap();
let agent_type = sessions.find_agent_type("normalize").unwrap();

let output: NormalizedResult = sessions
    .run_oneshot(&agent_type, |ctx| {
        ctx.insert(InputPayload { /* ... */ });
    })
    .await?;
```

The setup closure runs before the first (and only) turn — use it to inject per-request resources via `ctx.insert()`. The type parameter `T` is the output type of the agent's terminal system, extracted from `ExecutionResult::output::<T>()` internally. If the graph completes but doesn't produce `T`, `SessionError::OutputNotFound` is returned.

Session cleanup is guaranteed in all exit paths (success and error). The ephemeral session is never persisted to the backing store.

#### Manual pattern

For cases that need a custom `GraphExecutor` or fine-grained control, compose the primitives directly. Note that `ExecutionResult` now carries the terminal output — use `result.output::<T>()` to extract it:

```rust
let session_id = SessionId::default();
let ctx = sessions.create_context();

sessions.create_session_with(ctx, &session_id, &agent_type, |ctx| {
    ctx.insert(InputPayload { /* ... */ });
})?;

let result = sessions.process_turn(&session_id).await?;
let output: Option<&NormalizedResult> = result.output::<NormalizedResult>();

sessions.delete_session(&session_id).await.ok();
```

When using the manual pattern, treat `delete_session` as cleanup — call it in every exit path (including `?` bail-outs). Skipping it leaks the session's context, graph, and checkpoint vector until the server exits. For guaranteed cleanup, use [`scoped_session`](#scoped-sessions-raii-guard) instead.

### Per-Request Input Injection

To pass arbitrary per-request data (e.g., a decoded HTTP body, auth claims, a correlation ID) into a session, insert it as a `LocalResource` in the `init` or `setup` closure and consume it via `Res<T>` in a system:

```rust
#[derive(Clone)]
struct RequestPayload {
    body: IntakeBody,
    correlation_id: CorrelationId,
}

impl LocalResource for RequestPayload {}

// Inject at session creation:
sessions.create_session_with(ctx, &session_id, &agent_type, |ctx| {
    ctx.insert(RequestPayload { body, correlation_id });
})?;

// Or at turn time:
sessions.process_turn_with(&session_id, |ctx| {
    ctx.insert(RequestPayload { body, correlation_id });
}).await?;
```

The system reads it normally:

```rust
#[system]
async fn normalize(payload: Res<RequestPayload>) -> NormalizedResult {
    normalize_body(&payload.body)
}
```

Prefer this over mutable god-structs (`ResMut<WorkingState>` with `Option` fields): inputs are immutable, downstream systems declare exactly what they read, and the context enforces one `T` per scope.

For the standard `x-trace-id` / `x-correlation-id` / `x-request-id` headers specifically, use `RequestContextPlugin` — insert `HttpHeaders(headers)` in the setup closure and read `Res<RequestContext>` in systems. See [HTTP Integration](http.md#requestcontext-trace-and-correlation-ids).

See [Data Flow Patterns](data-flow.md) for a decision table on when to use `Res<T>` vs. `ResMut<T>` vs. `Out<T>`.

### Scoped Sessions (RAII Guard)

For multi-turn flows that still need guaranteed cleanup, use `scoped_session` to get a `SessionGuard` that deletes the session on drop:

```rust
let guard = sessions.scoped_session(&agent_type, |ctx| {
    ctx.insert(AgentConfig::new("claude-sonnet-4-6"));
})?;

// Run multiple turns
guard.process_turn().await?;
guard.process_turn_with(|ctx| {
    ctx.insert(NextInput { text: "continue".into() });
}).await?;

// Read context between turns
let state = guard.with_context(|ctx| {
    ctx.get_resource::<ConversationState>().unwrap().clone()
}).await?;

// Session is automatically deleted when `guard` drops
```

The guard delegates `process_turn`, `process_turn_with`, and `with_context` to the inner `SessionsAPI`. Cleanup is asynchronous via `tokio::spawn` — the `Drop` impl schedules deletion rather than blocking.

Use `scoped_session` when you need multi-step control (multiple turns, context inspection between turns) with cleanup guarantees. For single-turn request→response, prefer `run_oneshot` which is simpler.

## Checkpointing and Rollback

Checkpoints are in-memory snapshots of session resource state at a specific turn.

### Auto-Checkpoint

Enabled by default. After every successful turn, the context is serialized and stored as a checkpoint. Failures are logged but never propagate.

```rust
// Disable auto-checkpoint
SessionsPlugin::new(store).without_auto_checkpoint();
```

### Manual Checkpoint

```rust
let turn: TurnNumber = sessions.checkpoint(&session_id).await?;
```

### List and Rollback

```rust
let checkpoints: Vec<TurnNumber> = sessions.list_checkpoints(&session_id)?;
sessions.rollback(&session_id, target_turn).await?;
```

Rollback restores the context to the checkpointed state. Checkpoints newer than the target turn are discarded. The turn number is reset to match the checkpoint.

## Persistence (Save/Resume)

Sessions can be persisted to a `SessionStore` backend and later resumed.

```rust
// Save to store
sessions.save_session(&session_id).await?;

// Resume from store
sessions.resume_session(&session_id, &agent_type).await?;
```

The `SessionStore` trait is implemented by:
- `InMemoryStore` — in-process HashMap (default, no durability)
- `FileStore` — filesystem-backed (requires `file-store` feature)

Serialization uses `ResourceSerializer` instances registered via `PersistencePlugin`.

## SessionsPlugin Setup

```rust
use polaris_sessions::{SessionsPlugin, SessionsAPI};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_core_plugins::PersistencePlugin;

let mut server = Server::new();
server
    .add_plugins(MinimalPlugins.build())
    .add_plugins(PersistencePlugin)
    .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));

server.run().await;

let sessions = server.api::<SessionsAPI>().unwrap();
```

During `ready()`, `SessionsPlugin` captures the `ContextFactory`, `HooksAPI`, `MiddlewareAPI`, and serializer snapshot from `PersistenceAPI`.

## Internal State

Each session holds:

| Field | Type | Purpose |
|-------|------|---------|
| `ctx` | `tokio::sync::Mutex<SystemContext<'static>>` | Async-safe context access |
| `graph` | `Graph` | Agent's execution graph |
| `executor` | `GraphExecutor` | Per-session executor config |
| `agent_type` | `AgentTypeId` | Registered agent identifier |
| `turn_number` | `AtomicU32` | Current turn counter |
| `checkpoints` | `parking_lot::Mutex<Vec<Checkpoint>>` | In-memory checkpoint history |

## Error Types

| Error | Cause |
|-------|-------|
| `SessionError::AgentNotFound` | Agent type not registered |
| `SessionError::SessionNotFound` | Session ID not in live sessions |
| `SessionError::SessionAlreadyExists` | Duplicate session ID |
| `SessionError::SessionBusy` | `try_process_turn` when lock is held |
| `SessionError::TurnNotFound` | Rollback target doesn't exist |
| `SessionError::GraphValidation` | Agent graph has structural errors |
| `SessionError::Execution` | Graph execution failed |
| `SessionError::Setup` | Agent `setup()` returned error |
| `SessionError::OutputNotFound` | `run_oneshot` graph completed but didn't produce expected type |

## Key Files

| File | Purpose |
|------|---------|
| `polaris_sessions/src/api.rs` | `SessionsAPI`, `SessionsPlugin`, turn execution |
| `polaris_sessions/src/guard.rs` | `SessionGuard` — RAII session cleanup |
| `polaris_sessions/src/error.rs` | `SessionError` enum |
| `polaris_sessions/src/info.rs` | `SessionInfo`, `SessionMetadata` |
| `polaris_sessions/src/store/mod.rs` | `SessionStore` trait, `SessionId`, `SessionData` |
| `polaris_sessions/src/store/memory.rs` | `InMemoryStore` implementation |
| `polaris_sessions/src/store/file.rs` | `FileStore` implementation |
