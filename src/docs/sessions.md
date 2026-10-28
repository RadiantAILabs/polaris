Session management and agent orchestration.

Sessions bind a [`SystemContext`](crate::system::param::SystemContext) to an
agent's graph and executor, managing the lifecycle of a single agent
conversation. `SessionsAPI` is the primary interface.

# Setup

```no_run
# use std::sync::Arc;
# use polaris_ai::system::server::Server;
# use polaris_ai::system::plugin::PluginGroup;
# use polaris_ai::plugins::{MinimalPlugins, PersistencePlugin};
use polaris_ai::sessions::{SessionsAPI, SessionsPlugin, SessionId};
use polaris_ai::sessions::store::memory::InMemoryStore;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
# let mut server = Server::new();
# server.add_plugins(MinimalPlugins.build());
# server.add_plugins(PersistencePlugin);
server.add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
server.finish().await.unwrap();

let sessions = server.api::<SessionsAPI>().unwrap();
# Ok(())
# }
```

# Session Lifecycle

**Register agents** before creating sessions:

```no_run
# use polaris_ai::sessions::SessionsAPI;
# use polaris_ai::agent::Agent;
# use polaris_ai::graph::Graph;
# struct MyReActAgent;
# impl Agent for MyReActAgent {
#     fn build(&self, graph: &mut Graph) {}
#     fn name(&self) -> &'static str { "ReActAgent" }
# }
# fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
sessions.register_agent(MyReActAgent)?;
let agent_type = sessions.find_agent_type("ReActAgent").unwrap();
# Ok(())
# }
```

**Execute turns:**

```no_run
# use polaris_ai::sessions::SessionsAPI;
# use polaris_ai::sessions::SessionId;
# async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
# let session_id = SessionId::default();
// Basic turn
let result = sessions.process_turn(&session_id).await?;

// With per-turn resource injection
let result = sessions.process_turn_with(&session_id, |ctx| {
    // ctx.insert(UserIO::new(io_provider));
}).await?;

// Non-blocking (returns SessionBusy if lock held)
let result = sessions.try_process_turn_with(&session_id, |ctx| {
    // ctx.insert(UserIO::new(io_provider));
}).await?;
# Ok(())
# }
```

# Capability Contracts

Named slot signatures let out-of-process callers discover capable agents
without re-implementing signature checks (which cannot be sound across
binaries — `TypeId`s do not unify). The plugin defining a capability
registers a contract; every agent whose cached signature satisfies it by
subsumption advertises the contract's name via
[`agent_type_infos`](crate::sessions::SessionsAPI::agent_type_infos) and
authenticated `GET /v1/sessions/agent-types/details`:

Contract names must be non-empty, have no surrounding whitespace, and contain
no control characters. `register_contract` validates string inputs, while
direct construction uses `ContractName::new`.

```no_run
# use polaris_ai::sessions::SessionsAPI;
# use polaris_ai::graph::GraphSignature;
# #[derive(Clone)] struct TraceLine(String);
# impl polaris_ai::system::resource::LocalResource for TraceLine {}
# #[derive(Clone)] struct Triples(u32);
# fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
sessions.register_contract(
    "self-learn",
    GraphSignature::new().require_read::<TraceLine>().produce::<Triples>(),
)?;

// The registrant asserts at startup that its own agent satisfies it.
let diff = sessions.contract_diff("self-learn", "ExtractionAgent")?;
assert!(diff.is_empty(), "agent drifted from its contract: {diff}");
# Ok(())
# }
```

# Recipes

## One-Shot Execution

The common "run once, extract result, clean up" pattern:

```no_run
# use polaris_ai::sessions::{SessionsAPI, AgentTypeId};
# async fn example(sessions: &SessionsAPI, agent_type: &AgentTypeId) -> Result<(), Box<dyn std::error::Error>> {
let output: String = sessions
    .run_oneshot(agent_type, |ctx| {
        // ctx.insert(InputPayload { ... });
    })
    .await?;
# Ok(())
# }
```

Session cleanup is guaranteed in all exit paths. The ephemeral session is
never persisted.

## Scoped Sessions (RAII Guard)

For multi-turn flows with guaranteed cleanup:

```no_run
# use polaris_ai::sessions::{SessionsAPI, AgentTypeId};
# async fn example(sessions: &SessionsAPI, agent_type: &AgentTypeId) -> Result<(), Box<dyn std::error::Error>> {
let guard = sessions.scoped_session(agent_type, |ctx| {
    // ctx.insert(AgentConfig::new("claude-sonnet-4-6"));
})?;

guard.process_turn().await?;
guard.process_turn_with(|ctx| {
    // ctx.insert(NextInput { text: "continue".into() });
}).await?;
// Session deleted when `guard` drops
# Ok(())
# }
```

## Per-Request Input Injection

Insert as a `LocalResource` in the setup closure, consume via
[`Res<T>`](crate::system::param::Res):

```no_run
# use polaris_ai::polaris_system;
# use polaris_ai::system::{system, system::SystemError};
# use polaris_ai::system::param::Res;
# use polaris_ai::system::resource::LocalResource;
# #[derive(Clone)]
# struct RequestPayload { body: String }
# impl LocalResource for RequestPayload {}

#[system]
async fn normalize(payload: Res<RequestPayload>) -> String {
    payload.body.clone()
}
```

# Interrupting a Turn

`interrupt_turn` stops the turn a session is currently running. It is
synchronous and never touches the session's context lock — the running turn
holds that lock for its whole duration — so it does not wait for the turn it is
stopping. It does take two short internal locks (the session registry and the
turn's cancellation slot), so it is not safe to call from a raw OS signal
handler: a signal that arrives while the interrupted thread holds one of those
locks would deadlock. Call it from an ordinary task, a UI callback, or an HTTP
handler.

To stop a turn on `SIGINT`, let the runtime deliver the signal to a task
(`tokio::signal::ctrl_c()` does this) and have that task call `interrupt_turn`.
The turn runs on its own task:

```no_run
# use polaris_ai::sessions::{SessionError, SessionsAPI, SessionId};
# async fn example(sessions: SessionsAPI, session_id: SessionId) {
// Task 1: runs the turn and sees how it ended.
match sessions.process_turn(&session_id).await {
    Err(SessionError::Interrupted { turn, .. }) => {
        // Recorded as interrupted, session idle, `turn_number` advanced.
        // A checkpoint was attempted, subject to the checkpoint policy.
    }
    Ok(_) | Err(_) => {}
}
# }
```

```no_run
# use polaris_ai::sessions::{SessionsAPI, SessionId};
# async fn ctrl_c() { /* in Tokio: `tokio::signal::ctrl_c().await` */ }
# async fn example(sessions: SessionsAPI, session_id: SessionId) -> Result<(), Box<dyn std::error::Error>> {
// Task 2: waits for the signal, then raises it for the turn.
ctrl_c().await;
let was_running = sessions.interrupt_turn(&session_id)?;  // Ok(false) when idle
# Ok(())
# }
```

`Ok(true)` means only that a turn was in flight. A raise while idle is not
latched into the next turn, an unknown session is `SessionNotFound`, and
escalation policy ("a second press quits") belongs to the host.

An interrupted turn is deliberately **not** a failed one: it is recorded
`Interrupted`, `turn_number` advances, a checkpoint is attempted, and the
error is `SessionError::Interrupted` — never
`SessionError::Execution(ExecutionError::Interrupted { .. })`. The checkpoint
is *attempted*, not guaranteed: it is skipped when auto-checkpoint is disabled,
and a refusal for capacity is logged rather than returned, so use
`checkpoints().current` to see whether a point was retained. How promptly the
turn ends is the agent's business; see
[cancellation](crate::graph::CancellationSource) in the graph layer.

# Checkpointing

Auto-checkpoint is enabled by default. It captures at session creation, at
[`resume_session`](crate::sessions::SessionsAPI::resume_session), and after
every completed or interrupted turn. A failed turn is not captured. Capture is
synchronous: it runs while the turn still holds the session's context lock, so
its serialization cost adds to turn latency. A capture that fails, including a
refusal for capacity, is logged at `warn` and never fails the turn. The
retained points are durable: `save_session` writes them to the configured store
and `resume_session` reads them back. Manual checkpoint and rollback:

```no_run
# use polaris_ai::sessions::{SessionsAPI, SessionId};
# async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
# let session_id = SessionId::default();
let turn = sessions.checkpoint(&session_id).await?;
# let target_turn = turn;
sessions.rollback(&session_id, target_turn).await?;
# Ok(())
# }
```

For restore, pinning and retention bounds, use the `checkpoints()` handle. When a
bound is reached, a capture evicts the oldest unpinned point, and is refused with
`SessionError::CheckpointCapacity` if nothing can be evicted. The current point,
both ends of an unfinished restore and pinned points are never evicted. The one
exception is `restore`: its snapshot of the state being left may replace the
previous current point, unless that point is pinned. Dropping a `CheckpointPin`
never waits on checkpoint encoding or eviction.

If a restore fails partway, the session context is incoherent. Turns, captures and
`save_session` return `SessionError::RestoreFailed` until a restore succeeds, and
`save_session` leaves the stored session unchanged. Retrying the restore reuses
the snapshot taken before the failed attempt, so it needs no free slot.

# Persistence

```no_run
# use polaris_ai::sessions::{SessionsAPI, SessionId, AgentTypeId};
# use polaris_ai::system::param::SystemContext;
# async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
# let session_id = SessionId::default();
# let ctx = SystemContext::new();
sessions.save_session(&session_id).await?;    // persist to store
sessions.resume_session(ctx, &session_id).await?; // resume
# Ok(())
# }
```

Backends: `InMemoryStore` (default), `FileStore` (with `file-store` feature).

`FileStore` limits what it writes as well as what it reads: by default 64 MiB per
file, 1024 checkpoints and 1024 tombstones. `save` refuses a session over a limit
with `FileStoreError::SaveTooLarge`, `SaveTooManyCheckpoints` or
`SaveTooManyTombstones` and writes nothing, so a stored session can always be
resumed. With unbounded checkpoint retention a long-lived session can reach the
limits; raise them with `FileStore::with_load_limits`.

# History and Channel IO

Turn history (`turn_history`, `turn`, `record_turn_messages`), uptime queries,
[`models`](crate::sessions::models), and [`ChannelIOProvider`](crate::sessions::HttpIOProvider)
are available without `sessions-http`, including with default features disabled.
History is bounded and in memory; programmatic callers attach captured messages
with `record_turn_messages`. Existing HTTP type paths remain compatible re-exports.

# Feature-Gated HTTP Surface

| Feature | Adds public items | Existing surface it uses | Runtime effect |
|---------|-------------------|--------------------------|----------------|
| `sessions-http` | [`HttpPlugin`](crate::sessions::HttpPlugin), [`http`](crate::sessions::http), [`http::routes`](crate::sessions::http::routes), [`http::models`](crate::sessions::http::models) | Integrates with [`crate::app::HttpRouter`] and requires [`HttpPlugin`](crate::sessions::HttpPlugin) to be registered alongside [`crate::app::AppPlugin`] | Registers REST routes and handlers for session management; models and channel IO do not require this feature |

# Related

- [HTTP integration](crate::app) -- serving sessions over HTTP
- [Agent trait](crate::agent) -- defining the agents that sessions execute
- [Systems](crate::system) -- the context and resources that sessions manage
