//! Sessions API, plugin, and internal session state.
//!
//! [`SessionsAPI`] is the primary interface for managing agent sessions.
//! It is registered as an [`API`](polaris_system::api::API) by [`SessionsPlugin`]
//! and accessed via `server.api::<SessionsAPI>()`.

use crate::error::SessionError;
use crate::info::SessionInfo;
use crate::store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore};
use hashbrown::HashMap;
use parking_lot::RwLock;
use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, ResourceSerializer};
use polaris_graph::MiddlewareAPI;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::{ExecutionResult, Graph, GraphExecutor};
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoint (internal)
// ─────────────────────────────────────────────────────────────────────────────

/// A snapshot of session resources at a specific turn.
struct Checkpoint {
    turn_number: u32,
    data: SessionData,
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionState (internal)
// ─────────────────────────────────────────────────────────────────────────────

/// Live state for a single session.
///
/// The context is held in a `tokio::sync::Mutex` because it is held across
/// the async `execute()` call. Checkpoints use a `parking_lot::Mutex`
/// because they are only accessed synchronously.
struct SessionState {
    ctx: tokio::sync::Mutex<SystemContext<'static>>,
    graph: Graph,
    executor: GraphExecutor,
    agent_type: AgentTypeId,
    turn_number: AtomicU32,
    checkpoints: parking_lot::Mutex<Vec<Checkpoint>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionsAPI
// ─────────────────────────────────────────────────────────────────────────────

/// Server API for session lifecycle management.
///
/// Provides methods to register agents, create/resume sessions, execute turns,
/// checkpoint/rollback state, and persist sessions to a [`SessionStore`].
///
/// # Auto-Checkpoint
///
/// When enabled (the default), a background task creates a checkpoint after
/// every successful [`process_turn`](Self::process_turn). This provides
/// automatic rollback points without blocking the turn result. Checkpoint
/// failures are logged but never propagate as errors. Disable via
/// [`SessionsPlugin::without_auto_checkpoint`] if unwanted. Checkpoints
/// are stored in memory and are not persisted to the backing store.
///
/// # Interior Mutability
///
/// All methods take `&self` and use internal locks for thread safety.
pub struct SessionsAPI {
    store: Arc<dyn SessionStore>,
    serializers: RwLock<Vec<Arc<dyn ResourceSerializer>>>,
    agents: RwLock<HashMap<AgentTypeId, Arc<dyn Agent>>>,
    sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>,
    auto_checkpoint: AtomicBool,
}

impl API for SessionsAPI {}

impl SessionsAPI {
    /// Creates a new sessions API with the given store backend.
    ///
    /// Auto-checkpoint is enabled by default.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            serializers: RwLock::new(Vec::new()),
            agents: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            auto_checkpoint: AtomicBool::new(true),
        }
    }

    /// Snapshots the current set of serializers from [`PersistenceAPI`].
    ///
    /// Called during the plugin ready phase.
    pub fn set_serializers(&self, serializers: Vec<Arc<dyn ResourceSerializer>>) {
        *self.serializers.write() = serializers;
    }

    /// Sets whether auto-checkpoint is enabled.
    pub fn set_auto_checkpoint(&self, enabled: bool) {
        self.auto_checkpoint.store(enabled, Ordering::Relaxed);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Agent registration
    // ─────────────────────────────────────────────────────────────────────

    /// Registers an agent type so sessions can be created for it.
    ///
    /// Validates the agent's graph at registration time. Returns an error
    /// if the graph has structural errors. Warnings are logged but do not
    /// prevent registration.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::GraphValidation`] if the agent's graph
    /// contains structural errors.
    pub fn register_agent(&self, agent: impl Agent) -> Result<(), SessionError> {
        let graph = agent.to_graph();
        let result = graph.validate();

        if result.is_ok() && !result.warnings.is_empty() {
            tracing::warn!(agent = agent.name(), "{}", result);
        } else if result.is_err() {
            tracing::error!(agent = agent.name(), "{}", result);
            return Err(SessionError::GraphValidation {
                agent_name: agent.name().to_owned(),
                result,
            });
        } else {
            tracing::info!(agent = agent.name(), "{}", result);
        };

        let id = AgentTypeId::from_name(agent.name());
        self.agents.write().insert(id, Arc::new(agent));
        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Session lifecycle
    // ─────────────────────────────────────────────────────────────────────

    /// Creates a new session for the given agent type.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered.
    pub fn create_session(
        &self,
        server: &Server,
        id: &SessionId,
        agent_type: &AgentTypeId,
    ) -> Result<(), SessionError> {
        self.create_session_with(server, id, agent_type, |_| {})
    }

    /// Creates a new session for the given agent type with an initializer.
    ///
    /// Each session receives its own [`GraphExecutor`] instance. The `init`
    /// closure receives the freshly created context and may insert initial
    /// resources before the first turn.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered.
    pub fn create_session_with(
        &self,
        server: &Server,
        id: &SessionId,
        agent_type: &AgentTypeId,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        self.create_session_with_executor(server, id, agent_type, GraphExecutor::new(), init)
    }

    /// Creates a new session with a custom [`GraphExecutor`].
    ///
    /// Use this to configure executor settings such as max iterations
    /// on a per-session basis.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered.
    pub fn create_session_with_executor(
        &self,
        server: &Server,
        id: &SessionId,
        agent_type: &AgentTypeId,
        executor: GraphExecutor,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        let agent = self
            .agents
            .read()
            .get(agent_type)
            .cloned()
            .ok_or_else(|| SessionError::AgentNotFound(agent_type.to_string()))?;

        let graph = agent.to_graph();
        let mut ctx = server.create_context();
        init(&mut ctx);

        agent
            .setup(&mut ctx)
            .map_err(|source| SessionError::Setup {
                agent_name: agent.name().to_owned(),
                source,
            })?;

        let state = Arc::new(SessionState {
            ctx: tokio::sync::Mutex::new(ctx),
            graph,
            executor,
            agent_type: *agent_type,
            turn_number: AtomicU32::new(0),
            checkpoints: parking_lot::Mutex::new(Vec::new()),
        });

        self.sessions.write().insert(id.clone(), state);
        Ok(())
    }

    /// Executes a single turn for the session.
    ///
    /// [`SessionInfo`] is injected into the context before execution.
    /// The turn number is incremented after execution completes.
    ///
    /// When auto-checkpoint is enabled, a background task serializes the
    /// context state after the turn completes. This does not block the
    /// return of the execution result.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// or [`SessionError::Execution`] if the graph execution fails.
    pub async fn process_turn(
        &self,
        server: &Server,
        id: &SessionId,
    ) -> Result<ExecutionResult, SessionError> {
        self.process_turn_with(server, id, |_| {}).await
    }

    /// Executes a single turn for the session with a setup closure.
    ///
    /// Before execution, [`SessionInfo`] is injected into the context and
    /// the `setup` closure is called to prepare turn-specific resources.
    /// The turn number is incremented after execution completes.
    ///
    /// When auto-checkpoint is enabled, a background task serializes the
    /// context state after the turn completes. This does not block the
    /// return of the execution result.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// or [`SessionError::Execution`] if the graph execution fails.
    pub async fn process_turn_with(
        &self,
        server: &Server,
        id: &SessionId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        let state = self.get_state(id)?;

        let mut ctx = state.ctx.lock().await;
        let turn = state.turn_number.load(Ordering::Acquire);

        // Inject session metadata.
        ctx.insert(SessionInfo {
            session_id: id.clone(),
            turn_number: turn,
        });

        setup(&mut ctx);

        let hooks = server.api::<HooksAPI>();
        let middleware = server.api::<MiddlewareAPI>();
        let result = state
            .executor
            .execute(&state.graph, &mut ctx, hooks, middleware)
            .await?;

        state.turn_number.store(turn + 1, Ordering::Release);

        // Auto-checkpoint: serialize while we still hold the lock
        // TODO @localminimum: look into doing this in a background task
        // to avoid any potential latency impact on the turn result.
        // Get profiling data to see if this is actually a problem
        // worth optimizing.
        if self.auto_checkpoint.load(Ordering::Relaxed) {
            let serializers = self.serializers.read().clone();
            match serialize_context(&serializers, state.agent_type, turn, &ctx) {
                Ok(data) => {
                    state.checkpoints.lock().push(Checkpoint {
                        turn_number: turn,
                        data,
                    });
                }
                Err(err) => {
                    tracing::warn!(
                        session = %id,
                        "auto-checkpoint failed: {err}"
                    );
                }
            }
        }

        Ok(result)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Checkpoint / rollback
    // ─────────────────────────────────────────────────────────────────────

    /// Creates a checkpoint of the session's current resource state.
    ///
    /// Returns the turn number at which the checkpoint was taken.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] or a persistence error
    /// if serialization fails.
    pub async fn checkpoint(&self, id: &SessionId) -> Result<u32, SessionError> {
        let state = self.get_state(id)?;
        let ctx = state.ctx.lock().await;
        let turn = state.turn_number.load(Ordering::Acquire);

        let serializers = self.serializers.read().clone();
        let data = serialize_context(&serializers, state.agent_type, turn, &ctx)?;

        state.checkpoints.lock().push(Checkpoint {
            turn_number: turn,
            data,
        });
        Ok(turn)
    }

    /// Rolls back the session to a previously checkpointed turn.
    ///
    /// Checkpoints newer than the target turn are discarded.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist, [`SessionError::TurnNotFound`] if no checkpoint exists for
    /// the given turn, or a persistence error on deserialization.
    pub async fn rollback(&self, id: &SessionId, turn: u32) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let mut ctx = state.ctx.lock().await;

        let mut checkpoints = state.checkpoints.lock();
        let checkpoint = checkpoints
            .iter()
            .find(|cp| cp.turn_number == turn)
            .ok_or(SessionError::TurnNotFound(turn))?;

        let serializers = self.serializers.read().clone();
        deserialize_into_context(&serializers, &checkpoint.data, &mut ctx)?;

        state
            .turn_number
            .store(checkpoint.turn_number, Ordering::Release);

        // Discard checkpoints newer than the rollback target.
        checkpoints.retain(|cp| cp.turn_number <= turn);

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Persistence (store)
    // ─────────────────────────────────────────────────────────────────────

    /// Serializes the session and persists it to the backing store.
    ///
    /// # Errors
    ///
    /// Returns a persistence or store error on failure.
    pub async fn save_session(&self, id: &SessionId) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let ctx = state.ctx.lock().await;
        let turn = state.turn_number.load(Ordering::Acquire);

        let serializers = self.serializers.read().clone();
        let data = serialize_context(&serializers, state.agent_type, turn, &ctx)?;

        self.store.save(id, &data).await
    }

    /// Loads a session from the backing store with the default executor.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the store has no data
    /// for this ID, [`SessionError::AgentNotFound`] if the agent type from
    /// the stored data has not been registered, or a [`SessionError::Persistence`].
    pub async fn resume_session(
        &self,
        server: &Server,
        id: &SessionId,
    ) -> Result<(), SessionError> {
        self.resume_session_with(server, id, |_| {}).await
    }

    /// Loads a session from the backing store with an initializer.
    ///
    /// The `init` closure receives the context after deserialization and
    /// may inject non-persisted resources before [`Agent::setup`] runs.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the store has no data
    /// for this ID, [`SessionError::AgentNotFound`] if the agent type from
    /// the stored data has not been registered, or a persistence error.
    pub async fn resume_session_with(
        &self,
        server: &Server,
        id: &SessionId,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        self.resume_session_with_executor(server, id, GraphExecutor::new(), init)
            .await
    }

    /// Loads a session from the backing store with a custom executor and initializer.
    ///
    /// Creates a fresh context, deserializes persisted resources, calls `init`,
    /// then runs [`Agent::setup`].
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the store has no data
    /// for this ID, [`SessionError::AgentNotFound`] if the agent type from
    /// the stored data has not been registered, or a persistence error.
    pub async fn resume_session_with_executor(
        &self,
        server: &Server,
        id: &SessionId,
        executor: GraphExecutor,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        let data = self
            .store
            .load(id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(id.clone()))?;

        // Find the registered agent whose type name matches the stored data.
        let (agent_type, agent) = {
            let agents = self.agents.read();
            agents
                .iter()
                .find(|(k, _)| k.as_str() == data.agent_type)
                .map(|(k, v)| (*k, Arc::clone(v)))
                .ok_or_else(|| SessionError::AgentNotFound(data.agent_type.clone()))?
        };

        let graph = agent.to_graph();
        let mut ctx = server.create_context();

        let serializers = self.serializers.read().clone();
        deserialize_into_context(&serializers, &data, &mut ctx)?;

        init(&mut ctx);

        agent
            .setup(&mut ctx)
            .map_err(|source| SessionError::Setup {
                agent_name: agent.name().to_owned(),
                source,
            })?;

        let state = Arc::new(SessionState {
            ctx: tokio::sync::Mutex::new(ctx),
            graph,
            executor,
            agent_type,
            turn_number: AtomicU32::new(data.turn_number),
            checkpoints: parking_lot::Mutex::new(Vec::new()),
        });

        self.sessions.write().insert(id.clone(), state);
        Ok(())
    }

    /// Re-runs [`Agent::setup`] on a live session.
    ///
    /// Useful after operations that replace the context (e.g., rollback),
    /// which may lose non-persisted resources that `setup` provides.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist, or [`SessionError::Setup`] if the agent's setup fails.
    pub async fn setup_session(&self, id: &SessionId) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let agent = self
            .agents
            .read()
            .get(&state.agent_type)
            .cloned()
            .ok_or_else(|| SessionError::AgentNotFound(state.agent_type.to_string()))?;

        let mut ctx = state.ctx.lock().await;
        agent
            .setup(&mut ctx)
            .map_err(|source| SessionError::Setup {
                agent_name: agent.name().to_owned(),
                source,
            })?;
        Ok(())
    }

    /// Removes the session from memory and deletes it from the backing store.
    ///
    /// # Errors
    ///
    /// Returns a store error on failure.
    pub async fn delete_session(&self, id: &SessionId) -> Result<(), SessionError> {
        self.sessions.write().remove(id);
        self.store.delete(id).await
    }

    /// Lists all session IDs known to the backing store.
    ///
    /// # Errors
    ///
    /// Returns a store error on failure.
    pub async fn list_sessions(&self) -> Result<Vec<SessionId>, SessionError> {
        self.store.list().await
    }

    // ─────────────────────────────────────────────────────────────────────
    // Context access
    // ─────────────────────────────────────────────────────────────────────

    /// Provides mutable access to a session's context outside of a turn.
    ///
    /// An example use is for injecting or inspecting resources after
    /// [`resume_session`](Self::resume_session).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub async fn with_context<R>(
        &self,
        id: &SessionId,
        f: impl FnOnce(&mut SystemContext<'static>) -> R,
    ) -> Result<R, SessionError> {
        let state = self.get_state(id)?;
        let mut ctx = state.ctx.lock().await;
        Ok(f(&mut ctx))
    }

    // ─────────────────────────────────────────────────────────────────────
    // Helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Looks up a live session by ID.
    fn get_state(&self, id: &SessionId) -> Result<Arc<SessionState>, SessionError> {
        self.sessions
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::SessionNotFound(id.clone()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Serialization helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Serializes all registered resources from a context into [`SessionData`].
fn serialize_context(
    serializers: &[Arc<dyn ResourceSerializer>],
    agent_type: AgentTypeId,
    turn_number: u32,
    ctx: &SystemContext<'_>,
) -> Result<SessionData, SessionError> {
    let mut resources = Vec::new();
    for ser in serializers {
        if let Some(value) = ser.save(ctx)? {
            resources.push(ResourceEntry {
                plugin_id: ser.plugin_id().to_owned(),
                storage_key: ser.storage_key().to_owned(),
                version: ser.schema_version().to_owned(),
                data: value,
            });
        }
    }
    Ok(SessionData {
        agent_type: agent_type.as_str().to_owned(),
        turn_number,
        resources,
    })
}

/// Deserializes [`SessionData`] entries back into a context by matching
/// each entry to the appropriate serializer via `(plugin_id, storage_key)`.
fn deserialize_into_context(
    serializers: &[Arc<dyn ResourceSerializer>],
    data: &SessionData,
    ctx: &mut SystemContext<'static>,
) -> Result<(), SessionError> {
    for entry in &data.resources {
        if let Some(ser) = serializers.iter().find(|s| {
            s.plugin_id() == entry.plugin_id
                && s.storage_key() == entry.storage_key
                && s.schema_version() == entry.version
        }) {
            ser.load(entry.data.clone(), ctx)?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionsPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Plugin that provides session management via [`SessionsAPI`].
///
/// Requires [`PersistencePlugin`] to be present for resource serialization.
///
/// # Auto-Checkpoint
///
/// By default, a background checkpoint is created after every successful
/// [`process_turn`](SessionsAPI::process_turn). Call
/// [`without_auto_checkpoint`](Self::without_auto_checkpoint) to disable.
pub struct SessionsPlugin {
    store: Arc<dyn SessionStore>,
    auto_checkpoint: bool,
}

impl SessionsPlugin {
    /// Creates a new sessions plugin backed by the given store.
    ///
    /// Auto-checkpoint is enabled by default.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            auto_checkpoint: true,
        }
    }

    /// Disables automatic checkpointing after each successful turn.
    #[must_use]
    pub fn without_auto_checkpoint(mut self) -> Self {
        self.auto_checkpoint = false;
        self
    }
}

impl Plugin for SessionsPlugin {
    const ID: &'static str = "polaris::sessions";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let api = SessionsAPI::new(Arc::clone(&self.store));
        api.set_auto_checkpoint(self.auto_checkpoint);
        server.insert_api(api);
    }

    fn ready(&self, server: &mut Server) {
        let persistence = server
            .api::<PersistenceAPI>()
            .expect("SessionsPlugin requires PersistencePlugin");
        let sessions = server
            .api::<SessionsAPI>()
            .expect("SessionsAPI should be present after build");
        sessions.set_serializers(persistence.serializers());
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PersistencePlugin>()]
    }
}
