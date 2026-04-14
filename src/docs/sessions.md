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
server.finish().await;

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

# Checkpointing

Auto-checkpoint is enabled by default (after every successful turn).
Manual checkpoint and rollback:

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

# Related

- [HTTP integration](crate::app) -- serving sessions over HTTP
- [Agent trait](crate::agent) -- defining the agents that sessions execute
- [Systems](crate::system) -- the context and resources that sessions manage
