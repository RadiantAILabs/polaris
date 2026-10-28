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
server.run().await.unwrap();

let sessions = server.api::<SessionsAPI>().unwrap();
```

`SessionsAPI` is cheaply cloneable (backed by `Arc<SessionsInner>`) — suitable for sharing across HTTP handlers and background tasks.

## Agent Registration

Before creating sessions, register agent types:

```rust
sessions.register_agent(MyReActAgent)?;
```

Registration validates the agent's graph at registration time. Structural errors return `SessionError::GraphValidation`. Warnings are logged but do not prevent registration. The graph built for validation is also used to cache the agent's `GraphSignature`, so signature reads (contract satisfaction, the authenticated agent-type details endpoint) never rebuild it.

```rust
// Query registered agents
let agents: Vec<&str> = sessions.registered_agents();
let agent_type: Option<AgentTypeId> = sessions.find_agent_type("react");
let signature: Option<GraphSignature> = sessions.agent_signature("react");
```

## Capability Contracts

A **capability contract** is a slot `GraphSignature` registered under a well-known name. It answers a question no out-of-process caller can answer soundly on its own — "which registered agents can do X?" — because signature matching compares `TypeId`s, which do not unify across separately compiled binaries. The check therefore runs in the server process: consumers register named contracts, the sessions layer matches each agent's cached signature against them, and satisfied contract names are advertised per agent only on trusted surfaces (in-process via `agent_type_infos()`, over HTTP via authenticated [`GET /v1/sessions/agent-types/details`](./http.md)). Remote callers with access to that private route filter on the advertised capability and pick by name, never re-implementing the check.

Contract names are validated coordination keys: they must be non-empty, have no leading or trailing whitespace, and contain no control characters. Use `ContractName::new` when constructing one directly; `register_contract` and `contract_diff` validate string inputs automatically.

```rust
use polaris_ai::graph::GraphSignature;

// The plugin that defines a capability registers its contract…
sessions.register_contract(
    "self-learn",
    GraphSignature::new().require_read::<TraceLine>().produce::<Triples>(),
)?;

// …and asserts at startup that its own agent satisfies it.
let diff = sessions.contract_diff("self-learn", "ExtractionAgent")?;
assert!(diff.is_empty(), "agent drifted from its contract: {diff}");

// Any consumer can snapshot every agent with its satisfied contracts.
for info in sessions.agent_type_infos() {
    println!("{}: {:?} — {}", info.name, info.contracts, info.signature);
}
```

Matching uses `GraphSignature::satisfies` — **subsumption**, not either of the relations a `Dynamic` slot substitutes candidates under (`compatible_with` / `substitutable_for`; see [Graph — Dynamic](./graph.md#dynamic)), whose `requires` axis runs the other way because their environment is a parent graph rather than the registrant. An agent's extra `requires` are presumed environment-provided by its registrant's setup (conservative derivation reports self-inserted resources as required), and extra `produces` merge back (inertly only where the driver's graph produces no colliding type — merge-back keeps each producer's entry, but a same-typed output becomes the newest one, so a plain `Out<T>` read in the driver sees it; see [Graph — Dynamic](./graph.md#extra-outputs-stop-at-the-dynamic-boundary) for how a slot boundary drops such extras instead); only a free-output read the slot never sanctioned disqualifies. The residual risk is deliberate: an agent whose extra requires nothing actually provides still advertises the contract and fails loudly on first turn — the same honor-system boundary as declared access generally.

Behavioral notes:

- **No contract is registered by default** — the registry is consumer-supplied, like `SubgraphRegistry`.
- **Order never matters.** Satisfaction is computed at read time from signatures cached at `register_agent`, so agents and contracts can be registered in either order and nothing re-evaluates on registration events.
- **Names are coordination points.** Re-registering a name with an identical signature is a no-op; a different signature returns `SessionError::ContractConflict` rather than silently redefining another plugin's capability.
- The term collides with `polaris_system::plugin::Contract` (the plugin-capability version marker); a capability contract here is a graph-interface slot signature, the same sense in which `Dynamic` slots call their signatures contracts.

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
3. **Inject the turn's cancellation token** — a fresh `RunCancellation`, inserted *before* the setup closure so a caller may deliberately override it
4. **Call setup closure** — caller injects per-turn resources (e.g., `UserIO`)
5. **Execute graph** — `GraphExecutor::execute()` runs the agent's graph with hooks and middleware
6. **Auto-checkpoint** — if enabled, the context is captured into the session's retained checkpoint set. This runs synchronously while the turn still holds the context lock, so its serialization cost adds to the turn's latency; a failed capture is warned, never returned. The retained set reaches the store only through `save_session`. Also taken after an interrupted turn; a failed turn is not captured (see [Auto-Checkpoint](#auto-checkpoint))
7. **Increment turn number** — atomic counter advanced
8. **Return** `ExecutionResult` — `{ nodes_executed, duration, final_output }`

### Interrupting a turn

`interrupt_turn` raises the signal for whatever turn the session is currently running. It is **synchronous and never touches the context lock** — a turn holds that lock for its entire duration, so a method that waited for it could not stop the thing holding it.

It does take two short internal locks, one on the session registry and one on the turn's cancellation slot, so it is **not safe to call from a raw OS signal handler**: a signal that arrives while the interrupted thread holds one of them would deadlock. Call it from an ordinary task, a UI callback, or an HTTP handler. To stop a turn on `SIGINT`, let the runtime deliver the signal to a task and call `interrupt_turn` from there:

```rust
// A task that waits for Ctrl-C, then raises the signal for the running turn.
// The turn itself runs on another task, so there is something to interrupt.
tokio::spawn({
    let sessions = sessions.clone();
    let session_id = session_id.clone();
    async move {
        tokio::signal::ctrl_c().await.ok();
        // Ok(true): a turn was in flight. Ok(false): the session was idle.
        let _ = sessions.interrupt_turn(&session_id);
    }
});
```

The whole contract a caller may rely on:

- `Ok(true)` means **a turn was in flight**, nothing finer. The session cannot distinguish a turn that is working from one parked waiting for user input; both hold the slot.
- A raise while the session is idle returns `Ok(false)` and is **not** latched into the next turn. A stray interrupt must not kill the next request.
- An unknown session returns `SessionError::SessionNotFound`.
- Escalation — "a second press quits" — belongs to the host and its own timer. A second call during the same turn also returns `Ok(true)`.

How promptly the turn ends is the agent's business: the executor stops at the next node boundary, and a system that reads `Res<RunCancellation>` can return sooner. See [Graph — Cancellation](graph.md#cancellation).

#### What an interrupted turn records

| | Completed | Failed | **Interrupted** |
|---|---|---|---|
| Turn history status | `Completed` | `Failed` | `Interrupted` |
| Lifecycle | `Idle` | `Idle` | `Idle` |
| `otel.status_code` | unset | `ERROR` | unset — a user's interrupt is not an error |
| `error.type` | unset | variant name | `"Interrupted"` |
| `turn_number` | advances | stays | **advances** |
| Auto-checkpoint | attempted | not taken | **attempted** |
| Return | `Ok(ExecutionResult)` | `Err(SessionError::Execution(..))` | `Err(SessionError::Interrupted { .. })` |

The number advances because `(session_id, turn_number)` is the run's identity and two runs must never share it — reusing the number would also shadow the interrupted turn's own history entry. The checkpoint is attempted because the state after an interrupted run is consistent by construction: every system either completed or returned by its own choice, so it is a legitimate rollback target, and the next turn builds on it either way. It is *attempted* rather than guaranteed: auto-checkpoint may be disabled, and `CheckpointRetention` may refuse the capture for capacity. Either way the refusal is warned, not fatal, so the interrupt contract holds; `checkpoints().current` says whether a point was actually retained.

The asymmetry with the `Failed` path — which still neither advances nor checkpoints — is deliberate and unchanged here.

`SessionError::Interrupted` is the **one** representation of the outcome. `SessionError::Execution(ExecutionError::Interrupted { .. })` is never constructed, on any of the three turn entry points, so a consumer matches one shape rather than two.

Over HTTP the outcome maps to the `turn_interrupted` error code with status `409`, on both the JSON turn response and the SSE terminal `event: error`. No endpoint *raises* the signal: the SSE handler's documented no-abort-on-client-disconnect behaviour is unchanged.

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

A checkpoint is a snapshot of a session's serialized resource state at a turn boundary. Checkpoints are **durable**: the retained set rides `SessionData` through whatever `SessionStore` the host configured, so `save_session` writes them and `resume_session` brings them back. They are also **bounded** — see [Retention](#checkpoint-retention) — because a set that grew without limit would be rewritten in full on every save.

Two surfaces exist. The older one identifies a snapshot by its turn number; the newer one, on the `SessionCheckpoints` handle, identifies it by a stable `CheckpointId`. Prefer the handle for anything that resumes execution after rewinding — a turn number is a display label, two snapshots can share one, and rollback resolves to whichever was captured first.

### Auto-Checkpoint

Enabled by default. A checkpoint is captured at session creation, at resume, and after every completed or interrupted turn; a failed turn is not captured. Failures — including a retention refusal — are warned, never propagated, so an automatic checkpoint never fails a user's turn.

```rust
// Disable auto-checkpoint
SessionsPlugin::new(store).without_auto_checkpoint();
```

### The checkpoint handle

```rust
use polaris_sessions::checkpoint::{CheckpointOrigin, CheckpointStatus};

let checkpoints = sessions.checkpoints();

// Capture, and hold the point across a user decision.
let point = checkpoints.capture(&session_id, CheckpointOrigin::Manual).await?;
let pin = checkpoints.pin(&session_id, point.id)?;

// Rewind. The state being left is captured first, so it stays recoverable.
let outcome = checkpoints.restore(&session_id, point.id).await?;
assert_eq!(outcome.resume_turn, point.resume_turn);
```

`CheckpointMeta` stores `resume_turn` — the turn the session runs **next** if restored here — separately from the display `turn` label. That is what makes an automatic capture resume as correctly as a manual one. `parent` and `revision` record branch structure, so "is this point on the current history path" is a field read rather than a numeric turn comparison, and `restore` re-parents onto a new revision instead of discarding newer points.

| Method | Description |
|--------|-------------|
| `capture` | Captures the current state. Refuses a read-only session. |
| `list` | Every retained point, oldest first, with metadata. |
| `current` / `revision` | Where the session is positioned, and on which history revision. |
| `restore` | Rewinds to a point, reporting what it restored, what it could not, and what it carried over. |
| `restore_exact` | Refuses rather than restoring partially; the context is untouched on refusal. |
| `status` | `Retained`, `OffHistory`, `Evicted` or `Unknown` for one id. |
| `retention` / `usage` | The configured bounds, and what this session is spending against them. |
| `pin` | Holds a point against eviction until the returned `CheckpointPin` drops. Dropping a pin is an atomic decrement: it does not wait on checkpoint encoding, eviction or the session registry. A pin does not prevent deleting the session. |

`restore` does **not** re-run `Agent::setup`; like `rollback` it leaves non-persisted resources alone and names them in `RestoreOutcome::carried_over`. Call `setup_session` afterwards if they need rebuilding.

#### Recovering from a failed restore

If a resource fails to load partway through `restore`, the context holds a mix of old and new state and the session is marked incoherent. Until a restore succeeds, `process_turn` and every capture (`capture`, the older `checkpoint`, and automatic ones) return `SessionError::RestoreFailed`. So does `save_session`, which checks before it serializes anything, so the last coherent session in the store is not overwritten. Recover by restoring a retained point.

The retry reuses the pre-restore snapshot from the failed attempt instead of capturing the mixed context. It therefore needs no free slot, even when the session is full at its checkpoint count or byte bound.

### Checkpoint retention

Retention is injected policy, not a constant at a call site:

```rust
use polaris_sessions::checkpoint::CheckpointRetention;

SessionsPlugin::new(store)
    .with_checkpoint_retention(CheckpointRetention::new(Some(32), Some(8 * 1024 * 1024)));
```

Both bounds are per session. The defaults — 64 checkpoints and 16 MiB — are initial product choices, not measured performance claims. Enforcement runs at one place: a capture evicts the oldest unpinned point that is off the current history path, then the oldest unpinned point on it, and **refuses** with `SessionError::CheckpointCapacity` when nothing is evictable. It never drops a point silently. A consumer discovers the bounds with `retention()` and `usage()` rather than restating them.

A capture never evicts the session's current point, either endpoint of a restore that has not finished, or a point held by a `CheckpointPin`. `restore` has one exception. It first protects its target, then captures the state being left. If nothing else can be evicted, that pre-restore snapshot replaces the previous current point, which then reports `Evicted`. This is what lets you restore the oldest point of a session that is full at two points and still keep both the target and the snapshot. An explicit pin always wins: if the previous current point is pinned, the capture is refused with `SessionError::CheckpointCapacity` and the context is left untouched.

An evicted point leaves a two-field tombstone, so `status` reports `Evicted` — distinct from `Unknown` — and keeps doing so after a restart. Old tombstones compact into a floor marker rather than accumulating.

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

> **Run-identity hazard.** An automatic checkpoint's label is the turn that just *ran*, so `rollback` of one sets the counter back to a number that has already been used and the next turn re-uses the run identity `(session_id, turn_number)` — which the rule above says must never happen. A manual checkpoint reads the already-incremented counter and does not have the problem. The behaviour is documented rather than changed, because changing a shipped method is a separate decision; `checkpoints().restore` resumes at the stored `resume_turn` and has no such hazard.

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

`FileStore` bounds what it writes as well as what it reads. By default a session file may be at most 64 MiB and may hold at most 1024 checkpoints and 1024 tombstones. `save` refuses a session over a bound with `FileStoreError::SaveTooLarge`, `SaveTooManyCheckpoints` or `SaveTooManyTombstones`, before it writes anything, so a stored session can always be resumed and the previous file is unchanged. With `CheckpointRetention::unbounded()` a long-lived session can reach them; raise them with `FileStore::with_load_limits(FileStoreLimits::new(..))`.

`save_session` returns `SessionError::RestoreFailed` while a failed restore has left the context incoherent, and writes nothing. See [Recovering from a failed restore](#recovering-from-a-failed-restore).

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

server.run().await.unwrap();

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
| `checkpoints` | `parking_lot::Mutex<CheckpointSet>` | Retained checkpoints, their identity counter, removal tombstones and pins. Persisted with the session, and bounded by `CheckpointRetention`. |

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
| `SessionError::Interrupted` | The turn was stopped by `interrupt_turn` — recorded, numbered, a checkpoint attempted (subject to the checkpoint policy), and distinct from a failure |
| `SessionError::CheckpointNotFound` | No retained checkpoint has that `CheckpointId` — ask `status` whether it was evicted or never existed |
| `SessionError::RestoreIncomplete` | `restore_exact` found an entry no registered serializer matches; nothing was written |
| `SessionError::RestoreFailed` | A resource failed to load partway through a restore; the context is unknown, so turns, captures and `save_session` are refused until a restore succeeds (see [Recovering from a failed restore](#recovering-from-a-failed-restore)) |
| `SessionError::CheckpointCapacity` | The retained set is at a bound and nothing is evictable |
| `SessionError::Setup` | Agent `setup()` returned error |
| `SessionError::OutputNotFound` | `run_oneshot` graph completed but didn't produce expected type |
| `SessionError::ContractNotFound` | Named capability contract not registered |
| `SessionError::ContractConflict` | `register_contract` reused an existing name with a different signature |
| `SessionError::InvalidContractName` | Contract name was empty or not normalized |

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

## History and channel IO without HTTP

`SessionsAPI::turn_history`, `turn`, `record_turn_messages`, and `uptime_buckets`
are available with default features and with `default-features = false`.
`Turn`, `TurnStatus`, `TurnSummary`, `BucketGranularity`, and
`SessionUptimeResponse` are re-exported at the sessions crate root; the full
value family lives in `polaris_sessions::models`. History retains at most 256
turns per session, with bounded messages and payload sizes per turn. Callers
attach captured IO using `record_turn_messages`; executing a turn alone does
not capture its messages. History is in memory and is not restored from the store.

`polaris_sessions::ChannelIOProvider` provides bounded input/output channels
for interactive, headless, or HTTP callers. Its historical `HttpIOProvider`
name remains as a compatibility alias. The `sessions-http` feature gates the
HTTP router, handlers, and `HttpPlugin`, including their axum/app dependencies.
Existing `http::models` and `http::HttpIOProvider` imports remain compatible
when that feature is enabled.
