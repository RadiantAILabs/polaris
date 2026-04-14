//! Sessions API, plugin, and internal session state.
//!
//! [`SessionsAPI`] is the primary interface for managing agent sessions.
//! It is registered as an [`API`](polaris_system::api::API) by [`SessionsPlugin`]
//! and accessed via `server.api::<SessionsAPI>()`.

use crate::error::SessionError;
use crate::guard::SessionGuard;
use crate::info::{SessionInfo, SessionMetadata, SessionStatus};
use crate::store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore, TurnNumber};
use hashbrown::{HashMap, hash_map::Entry};
use parking_lot::RwLock;
use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, ResourceSerializer};
use polaris_graph::MiddlewareAPI;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::{ExecutionResult, Graph, GraphExecutor};
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::resource::Output;
use polaris_system::server::{ContextFactory, Server};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoint (internal)
// ─────────────────────────────────────────────────────────────────────────────

/// A snapshot of session resources at a specific turn.
struct Checkpoint {
    turn_number: TurnNumber,
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
    created_at: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionsAPI
// ─────────────────────────────────────────────────────────────────────────────

/// Internal state shared by all clones of a [`SessionsAPI`].
struct SessionsInner {
    store: Arc<dyn SessionStore>,
    serializers: RwLock<Arc<Vec<Arc<dyn ResourceSerializer>>>>,
    agents: RwLock<HashMap<AgentTypeId, Arc<dyn Agent>>>,
    sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>,
    auto_checkpoint: AtomicBool,
    hooks: OnceLock<HooksAPI>,
    middleware: OnceLock<MiddlewareAPI>,
    context_factory: OnceLock<ContextFactory>,
}

/// Server API for session lifecycle management.
///
/// Provides methods to register agents, create/resume sessions, execute turns,
/// checkpoint/rollback state, and persist sessions to a [`SessionStore`].
///
/// # Cloning
///
/// `SessionsAPI` is cheaply cloneable (backed by `Arc`). Clones share
/// the same underlying state, making it suitable for use as shared state
/// in HTTP handlers.
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
///
/// # Examples
///
/// ```no_run
/// # use polaris_sessions::{SessionsAPI, SessionId, AgentTypeId};
/// # use polaris_system::param::SystemContext;
/// # async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
/// let agent_type = sessions.find_agent_type("my_agent").unwrap();
/// let id = SessionId::default();
/// let ctx = sessions.create_context();
///
/// // Create session, run a turn, then clean up
/// sessions.create_session(ctx, &id, &agent_type)?;
/// let result = sessions.process_turn(&id).await?;
/// sessions.delete_session(&id).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SessionsAPI {
    inner: Arc<SessionsInner>,
}

impl std::fmt::Debug for SessionsAPI {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let session_count = self.inner.sessions.read().len();
        f.debug_struct("SessionsAPI")
            .field("session_count", &session_count)
            .finish_non_exhaustive()
    }
}

impl API for SessionsAPI {}

impl SessionsAPI {
    /// Creates a new sessions API with the given store backend.
    ///
    /// Auto-checkpoint is enabled by default.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// assert!(sessions.registered_agents().is_empty());
    /// ```
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            inner: Arc::new(SessionsInner {
                store,
                serializers: RwLock::new(Arc::new(Vec::new())),
                agents: RwLock::new(HashMap::new()),
                sessions: RwLock::new(HashMap::new()),
                auto_checkpoint: AtomicBool::new(true),
                hooks: OnceLock::new(),
                middleware: OnceLock::new(),
                context_factory: OnceLock::new(),
            }),
        }
    }

    /// Snapshots the current set of serializers from [`PersistenceAPI`].
    ///
    /// Called during the plugin ready phase.
    pub fn set_serializers(&self, serializers: Vec<Arc<dyn ResourceSerializer>>) {
        *self.inner.serializers.write() = Arc::new(serializers);
    }

    /// Sets whether auto-checkpoint is enabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// sessions.set_auto_checkpoint(false);
    /// ```
    pub fn set_auto_checkpoint(&self, enabled: bool) {
        self.inner.auto_checkpoint.store(enabled, Ordering::Relaxed);
    }

    /// Stores the graph execution APIs (hooks and middleware) for use
    /// during turn processing.
    ///
    /// Called during the plugin ready phase.
    ///
    /// # Panics
    ///
    /// Panics if hooks or middleware have already been set.
    pub fn set_graph_apis(&self, hooks: Option<HooksAPI>, middleware: Option<MiddlewareAPI>) {
        if let Some(hooks) = hooks {
            self.inner
                .hooks
                .set(hooks)
                .unwrap_or_else(|_| panic!("hooks already set"));
        }
        if let Some(middleware) = middleware {
            self.inner
                .middleware
                .set(middleware)
                .unwrap_or_else(|_| panic!("middleware already set"));
        }
    }

    /// Sets the context factory used to create fresh system contexts.
    ///
    /// Called automatically by [`SessionsPlugin::ready()`]. Only call this
    /// manually if you are not using `SessionsPlugin`.
    ///
    /// # Panics
    ///
    /// Panics if a factory has already been set.
    pub fn set_context_factory(&self, factory: ContextFactory) {
        self.inner
            .context_factory
            .set(factory)
            .unwrap_or_else(|_| panic!("context factory already set"));
    }

    /// Creates a fresh [`SystemContext`] using the stored
    /// [`ContextFactory`](polaris_system::server::ContextFactory).
    ///
    /// # Panics
    ///
    /// Panics if [`set_context_factory`](Self::set_context_factory) has not
    /// been called.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use polaris_sessions::{SessionsAPI, SessionsPlugin};
    /// # use polaris_sessions::store::memory::InMemoryStore;
    /// # use polaris_core_plugins::PersistencePlugin;
    /// # use polaris_system::server::Server;
    /// # tokio_test::block_on(async {
    /// let mut server = Server::new();
    /// server
    ///     .add_plugins(PersistencePlugin)
    ///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    /// server.finish().await;
    ///
    /// let sessions = server.api::<SessionsAPI>().unwrap();
    /// let ctx = sessions.create_context();
    /// # });
    /// ```
    #[must_use]
    pub fn create_context(&self) -> SystemContext<'static> {
        self.inner
            .context_factory
            .get()
            .expect("context factory not set — call set_context_factory() after server.finish()")
            .create_context()
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
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    /// use polaris_agent::Agent;
    /// use polaris_graph::Graph;
    ///
    /// # async fn step() {}
    /// struct MyAgent;
    /// impl Agent for MyAgent {
    ///     fn name(&self) -> &'static str { "MyAgent" }
    ///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
    /// }
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// sessions.register_agent(MyAgent).unwrap();
    /// assert!(sessions.registered_agents().contains(&"MyAgent"));
    /// ```
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
        self.inner.agents.write().insert(id, Arc::new(agent));
        Ok(())
    }

    /// Returns the names of all registered agent types.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// assert!(sessions.registered_agents().is_empty());
    /// ```
    #[must_use]
    pub fn registered_agents(&self) -> Vec<&'static str> {
        self.inner
            .agents
            .read()
            .keys()
            .map(AgentTypeId::as_str)
            .collect()
    }

    /// Finds the [`AgentTypeId`] for a registered agent by name.
    ///
    /// Returns `None` if no agent with that name has been registered.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    /// use polaris_agent::Agent;
    /// use polaris_graph::Graph;
    ///
    /// # async fn step() {}
    /// struct MyAgent;
    /// impl Agent for MyAgent {
    ///     fn name(&self) -> &'static str { "MyAgent" }
    ///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
    /// }
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// assert!(sessions.find_agent_type("MyAgent").is_none());
    ///
    /// sessions.register_agent(MyAgent).unwrap();
    /// let found = sessions.find_agent_type("MyAgent");
    /// assert!(found.is_some());
    /// ```
    #[must_use]
    pub fn find_agent_type(&self, name: &str) -> Option<AgentTypeId> {
        self.inner
            .agents
            .read()
            .keys()
            .find(|k| k.as_str() == name)
            .copied()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Session lifecycle
    // ─────────────────────────────────────────────────────────────────────

    /// Creates a new session for the given agent type.
    ///
    /// The caller provides a pre-built [`SystemContext`], typically via
    /// [`Server::create_context()`] or
    /// [`ContextFactory::create_context()`](polaris_system::server::ContextFactory::create_context).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, AgentTypeId};
    /// # fn example(sessions: &SessionsAPI, agent_type: &AgentTypeId) {
    /// let id = SessionId::new();
    /// let ctx = sessions.create_context();
    /// sessions.create_session(ctx, &id, agent_type).unwrap();
    /// # }
    /// ```
    pub fn create_session(
        &self,
        ctx: SystemContext<'static>,
        id: &SessionId,
        agent_type: &AgentTypeId,
    ) -> Result<(), SessionError> {
        self.create_session_with(ctx, id, agent_type, |_| {})
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, AgentTypeId};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct AgentConfig { model_id: String }
    /// # impl LocalResource for AgentConfig {}
    /// # fn example(sessions: &SessionsAPI, agent_type: &AgentTypeId) {
    /// let id = SessionId::new();
    /// let ctx = sessions.create_context();
    /// sessions.create_session_with(ctx, &id, agent_type, |ctx| {
    ///     ctx.insert(AgentConfig { model_id: "claude-sonnet".into() });
    /// }).unwrap();
    /// # }
    /// ```
    pub fn create_session_with(
        &self,
        ctx: SystemContext<'static>,
        id: &SessionId,
        agent_type: &AgentTypeId,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        self.create_session_with_executor(ctx, id, agent_type, GraphExecutor::new(), init)
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, AgentTypeId};
    /// # use polaris_graph::GraphExecutor;
    /// # fn example(sessions: &SessionsAPI, agent_type: &AgentTypeId) {
    /// let id = SessionId::new();
    /// let executor = GraphExecutor::new().with_default_max_iterations(20);
    /// let ctx = sessions.create_context();
    /// sessions.create_session_with_executor(ctx, &id, agent_type, executor, |_| {}).unwrap();
    /// # }
    /// ```
    pub fn create_session_with_executor(
        &self,
        mut ctx: SystemContext<'static>,
        id: &SessionId,
        agent_type: &AgentTypeId,
        executor: GraphExecutor,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        let agent = self
            .inner
            .agents
            .read()
            .get(agent_type)
            .cloned()
            .ok_or_else(|| SessionError::AgentNotFound(agent_type.to_string()))?;

        let graph = agent.to_graph();
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
            created_at: utc_now_iso8601(),
        });

        match self.inner.sessions.write().entry(id.clone()) {
            Entry::Occupied(_) => return Err(SessionError::SessionAlreadyExists(id.clone())),
            Entry::Vacant(entry) => entry.insert(state),
        };
        Ok(())
    }

    /// Executes a single turn for the session.
    ///
    /// [`SessionInfo`] is injected into the context before execution.
    /// The turn number is incremented after execution completes.
    ///
    /// When auto-checkpoint is enabled, the context state is serialized
    /// after the turn completes. This does not block the return of the
    /// execution result.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// or [`SessionError::Execution`] if the graph execution fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let result = sessions.process_turn(id).await.unwrap();
    /// assert!(result.nodes_executed > 0);
    /// # }
    /// ```
    pub async fn process_turn(&self, id: &SessionId) -> Result<ExecutionResult, SessionError> {
        self.process_turn_with(id, |_| {}).await
    }

    /// Executes a single turn for the session with a setup closure.
    ///
    /// Before execution, [`SessionInfo`] is injected into the context and
    /// the `setup` closure is called to prepare turn-specific resources.
    /// The turn number is incremented after execution completes.
    ///
    /// When auto-checkpoint is enabled, the context state is serialized
    /// after the turn completes. This does not block the return of the
    /// execution result.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// or [`SessionError::Execution`] if the graph execution fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct UserInput(String);
    /// # impl LocalResource for UserInput {}
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let result = sessions.process_turn_with(id, |ctx| {
    ///     ctx.insert(UserInput("Hello, agent!".into()));
    /// }).await.unwrap();
    /// # }
    /// ```
    pub async fn process_turn_with(
        &self,
        id: &SessionId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        let state = self.get_state(id)?;
        let mut ctx = state.ctx.lock().await;
        self.execute_turn(id, &state, &mut ctx, setup).await
    }

    /// Attempts to execute a single turn without waiting for the lock.
    ///
    /// Identical to [`process_turn`](Self::process_turn) but returns
    /// [`SessionError::SessionBusy`] immediately if the session is
    /// already executing a turn, instead of waiting.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionBusy`] if the session context lock
    /// is held, [`SessionError::SessionNotFound`] if the session does not
    /// exist, or [`SessionError::Execution`] if graph execution fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, SessionError};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// match sessions.try_process_turn(id).await {
    ///     Ok(result) => { /* turn completed */ }
    ///     Err(SessionError::SessionBusy(_)) => { /* another turn in progress */ }
    ///     Err(other) => { /* handle error */ }
    /// }
    /// # }
    /// ```
    pub async fn try_process_turn(&self, id: &SessionId) -> Result<ExecutionResult, SessionError> {
        self.try_process_turn_with(id, |_| {}).await
    }

    /// Attempts to execute a single turn with a setup closure, without
    /// waiting for the lock.
    ///
    /// Identical to [`process_turn_with`](Self::process_turn_with) but returns
    /// [`SessionError::SessionBusy`] immediately if the session is
    /// already executing a turn, instead of waiting.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionBusy`] if the session context lock
    /// is held, [`SessionError::SessionNotFound`] if the session does not
    /// exist, or [`SessionError::Execution`] if graph execution fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId, SessionError};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct UserInput(String);
    /// # impl LocalResource for UserInput {}
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// match sessions.try_process_turn_with(id, |ctx| {
    ///     ctx.insert(UserInput("hello".into()));
    /// }).await {
    ///     Ok(result) => { /* turn completed */ }
    ///     Err(SessionError::SessionBusy(_)) => { /* another turn in progress */ }
    ///     Err(other) => { /* handle error */ }
    /// }
    /// # }
    /// ```
    pub async fn try_process_turn_with(
        &self,
        id: &SessionId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        let state = self.get_state(id)?;
        let mut ctx = state
            .ctx
            .try_lock()
            .map_err(|_| SessionError::SessionBusy(id.clone()))?;
        self.execute_turn(id, &state, &mut ctx, setup).await
    }

    /// Shared turn execution logic used by both blocking and try-lock variants.
    async fn execute_turn(
        &self,
        id: &SessionId,
        state: &SessionState,
        ctx: &mut SystemContext<'static>,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        let turn = state.turn_number.load(Ordering::Acquire);

        // Inject session metadata.
        ctx.insert(SessionInfo {
            session_id: id.clone(),
            turn_number: turn,
        });

        setup(ctx);

        let hooks = self.inner.hooks.get();
        let middleware = self.inner.middleware.get();

        let result = state
            .executor
            .execute(&state.graph, ctx, hooks, middleware)
            .await?;

        state.turn_number.store(turn + 1, Ordering::Release);

        // Auto-checkpoint: serialize while we still hold the lock
        // TODO @localminimum: look into doing this in a background task
        // to avoid any potential latency impact on the turn result.
        // Get profiling data to see if this is actually a problem
        // worth optimizing.
        if self.inner.auto_checkpoint.load(Ordering::Relaxed) {
            let serializers = Arc::clone(&self.inner.serializers.read());
            match serialize_context(&serializers, state.agent_type, turn, &state.created_at, ctx) {
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let turn = sessions.checkpoint(id).await.unwrap();
    /// let checkpoints = sessions.list_checkpoints(id).unwrap();
    /// assert!(checkpoints.contains(&turn));
    /// # }
    /// ```
    pub async fn checkpoint(&self, id: &SessionId) -> Result<TurnNumber, SessionError> {
        let state = self.get_state(id)?;
        let ctx = state.ctx.lock().await;
        let turn = state.turn_number.load(Ordering::Acquire);

        let serializers = Arc::clone(&self.inner.serializers.read());
        let data = serialize_context(
            &serializers,
            state.agent_type,
            turn,
            &state.created_at,
            &ctx,
        )?;

        state.checkpoints.lock().push(Checkpoint {
            turn_number: turn,
            data,
        });
        Ok(turn)
    }

    /// Returns the turn numbers of all checkpoints for the session.
    ///
    /// The returned list is ordered by creation time (oldest first).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let turns = sessions.list_checkpoints(id).unwrap();
    /// for turn in &turns {
    ///     // Each entry is the turn number at which the checkpoint was taken.
    /// }
    /// # }
    /// ```
    pub fn list_checkpoints(&self, id: &SessionId) -> Result<Vec<TurnNumber>, SessionError> {
        let state = self.get_state(id)?;
        let checkpoints = state.checkpoints.lock();
        Ok(checkpoints.iter().map(|cp| cp.turn_number).collect())
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
    ///
    /// # Examples
    ///
    /// After rollback, non-persisted resources may need to be restored
    /// via [`with_context`](Self::with_context) and [`setup_session`](Self::setup_session):
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// sessions.rollback(id, 2).await.unwrap();
    /// // Re-run agent setup to restore non-persisted resources.
    /// sessions.setup_session(id).await.unwrap();
    /// # }
    /// ```
    pub async fn rollback(&self, id: &SessionId, turn: TurnNumber) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let mut ctx = state.ctx.lock().await;

        let mut checkpoints = state.checkpoints.lock();
        let checkpoint = checkpoints
            .iter()
            .find(|cp| cp.turn_number == turn)
            .ok_or(SessionError::TurnNotFound(turn))?;

        let serializers = Arc::clone(&self.inner.serializers.read());
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// // Save after processing turns so the session can be resumed later.
    /// sessions.save_session(id).await.unwrap();
    /// # }
    /// ```
    pub async fn save_session(&self, id: &SessionId) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let ctx = state.ctx.lock().await;
        let turn = state.turn_number.load(Ordering::Acquire);

        let serializers = Arc::clone(&self.inner.serializers.read());
        let data = serialize_context(
            &serializers,
            state.agent_type,
            turn,
            &state.created_at,
            &ctx,
        )?;

        self.inner.store.save(id, &data).await
    }

    /// Loads a session from the backing store with the default executor.
    ///
    /// The caller provides a pre-built [`SystemContext`], typically via
    /// [`Server::create_context()`] or
    /// [`ContextFactory::create_context()`](polaris_system::server::ContextFactory::create_context).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the store has no data
    /// for this ID, [`SessionError::AgentNotFound`] if the agent type from
    /// the stored data has not been registered, or a [`SessionError::Persistence`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let ctx = sessions.create_context();
    /// sessions.resume_session(ctx, id).await.unwrap();
    /// // Session is now live and ready for process_turn().
    /// # }
    /// ```
    pub async fn resume_session(
        &self,
        ctx: SystemContext<'static>,
        id: &SessionId,
    ) -> Result<(), SessionError> {
        self.resume_session_with(ctx, id, |_| {}).await
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct Config { model_id: String }
    /// # impl LocalResource for Config {}
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let ctx = sessions.create_context();
    /// sessions.resume_session_with(ctx, id, |ctx| {
    ///     // Inject non-persisted config before Agent::setup runs.
    ///     ctx.insert(Config { model_id: "claude-sonnet".into() });
    /// }).await.unwrap();
    /// # }
    /// ```
    pub async fn resume_session_with(
        &self,
        ctx: SystemContext<'static>,
        id: &SessionId,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        self.resume_session_with_executor(ctx, id, GraphExecutor::new(), init)
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # use polaris_graph::GraphExecutor;
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct Config { model_id: String }
    /// # impl LocalResource for Config {}
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let ctx = sessions.create_context();
    /// let executor = GraphExecutor::new().with_default_max_iterations(10);
    /// sessions.resume_session_with_executor(ctx, id, executor, |ctx| {
    ///     ctx.insert(Config { model_id: "claude-sonnet".into() });
    /// }).await.unwrap();
    /// # }
    /// ```
    pub async fn resume_session_with_executor(
        &self,
        mut ctx: SystemContext<'static>,
        id: &SessionId,
        executor: GraphExecutor,
        init: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(), SessionError> {
        let data = self
            .inner
            .store
            .load(id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(id.clone()))?;

        // Find the registered agent whose type name matches the stored data.
        let (agent_type, agent) = {
            let agents = self.inner.agents.read();
            agents
                .iter()
                .find(|(k, _)| k.as_str() == data.agent_type)
                .map(|(k, v)| (*k, Arc::clone(v)))
                .ok_or_else(|| SessionError::AgentNotFound(data.agent_type.clone()))?
        };

        let graph = agent.to_graph();

        let serializers = Arc::clone(&self.inner.serializers.read());
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
            created_at: data.created_at.clone(),
        });

        self.inner.sessions.write().insert(id.clone(), state);
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// // After rollback, re-run setup to restore non-persisted resources.
    /// sessions.rollback(id, 0).await.unwrap();
    /// sessions.setup_session(id).await.unwrap();
    /// # }
    /// ```
    pub async fn setup_session(&self, id: &SessionId) -> Result<(), SessionError> {
        let state = self.get_state(id)?;
        let agent = self
            .inner
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// sessions.delete_session(id).await.unwrap();
    /// # }
    /// ```
    pub async fn delete_session(&self, id: &SessionId) -> Result<(), SessionError> {
        self.inner.sessions.write().remove(id);
        self.inner.store.delete(id).await
    }

    /// Lists all session IDs known to the backing store.
    ///
    /// # Errors
    ///
    /// Returns a store error on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI) {
    /// let stored = sessions.list_sessions().await.unwrap();
    /// for id in &stored {
    ///     // Each ID can be passed to resume_session().
    /// }
    /// # }
    /// ```
    pub async fn list_sessions(&self) -> Result<Vec<SessionId>, SessionError> {
        self.inner.store.list().await
    }

    /// Lists all live session IDs currently held in memory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// assert!(sessions.list_live_sessions().is_empty());
    /// ```
    #[must_use]
    pub fn list_live_sessions(&self) -> Vec<SessionId> {
        self.inner.sessions.read().keys().cloned().collect()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Session info
    // ─────────────────────────────────────────────────────────────────────

    /// Returns metadata for a live session.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let meta = sessions.session_info(id).unwrap();
    /// assert_eq!(meta.session_id, *id);
    /// # }
    /// ```
    pub fn session_info(&self, id: &SessionId) -> Result<SessionMetadata, SessionError> {
        let state = self.get_state(id)?;
        Ok(SessionMetadata {
            session_id: id.clone(),
            agent_type: state.agent_type,
            turn_number: state.turn_number.load(Ordering::Acquire),
            created_at: state.created_at.clone(),
            status: SessionStatus::Active,
        })
    }

    /// Returns metadata for all live sessions.
    ///
    /// Holds the session lock once for the full scan, avoiding per-session
    /// lock acquisition.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// assert!(sessions.list_session_metadata().is_empty());
    /// ```
    #[must_use]
    pub fn list_session_metadata(&self) -> Vec<SessionMetadata> {
        self.inner
            .sessions
            .read()
            .iter()
            .map(|(id, state)| SessionMetadata {
                session_id: id.clone(),
                agent_type: state.agent_type,
                turn_number: state.turn_number.load(Ordering::Acquire),
                created_at: state.created_at.clone(),
                status: SessionStatus::Active,
            })
            .collect()
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Debug, Clone)] struct History(Vec<String>);
    /// # impl LocalResource for History {}
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// // Read a resource from the session context.
    /// let history = sessions.with_context(id, |ctx| {
    ///     ctx.get_resource::<History>().ok().map(|h| h.0.clone())
    /// }).await.unwrap();
    ///
    /// // Inject a resource into the session context.
    /// sessions.with_context(id, |ctx| {
    ///     ctx.insert(History(vec!["Hello".into()]));
    /// }).await.unwrap();
    /// # }
    /// ```
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
    // Scoped sessions
    // ─────────────────────────────────────────────────────────────────────

    /// Creates a scoped session with automatic cleanup.
    ///
    /// Returns a [`SessionGuard`] that deletes the session when dropped.
    /// This is useful for multi-turn flows where you need guaranteed
    /// cleanup without manual [`delete_session`](Self::delete_session) calls.
    ///
    /// For single-turn patterns, prefer [`run_oneshot`](Self::run_oneshot).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered, or [`SessionError::Setup`] if agent setup fails.
    pub fn scoped_session(
        &self,
        agent_type: &AgentTypeId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<SessionGuard, SessionError> {
        let id = SessionId::default();
        let ctx = self.create_context();
        self.create_session_with(ctx, &id, agent_type, setup)?;
        Ok(SessionGuard::new(self.clone(), id))
    }

    // ─────────────────────────────────────────────────────────────────────
    // One-shot execution
    // ─────────────────────────────────────────────────────────────────────

    /// Executes a one-shot agent turn and returns the typed output.
    ///
    /// This is the convenience method for the common "request → response"
    /// pattern: create a transient session, execute one turn, extract the
    /// output, and clean up. Session cleanup is guaranteed in all exit
    /// paths (success or execution error).
    ///
    /// # Type Parameters
    ///
    /// `T` — the output type produced by the agent's terminal system.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AgentNotFound`] if the agent type has not
    /// been registered, [`SessionError::Setup`] if agent setup fails,
    /// [`SessionError::Execution`] if the graph fails, or
    /// [`SessionError::OutputNotFound`] if the graph completes but does
    /// not produce an output of type `T`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, AgentTypeId};
    /// # use polaris_system::resource::LocalResource;
    /// # #[derive(Clone)] struct MyInput(String);
    /// # impl MyInput { fn new(s: &str) -> Self { Self(s.into()) } }
    /// # impl LocalResource for MyInput {}
    /// # #[derive(Clone)] struct MyOutput;
    /// # async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
    /// let agent_type = AgentTypeId::from_name("MyAgent");
    /// let output: MyOutput = sessions
    ///     .run_oneshot(&agent_type, |ctx| {
    ///         ctx.insert(MyInput::new("input"));
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run_oneshot<T: Output + Clone>(
        &self,
        agent_type: &AgentTypeId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<T, SessionError> {
        let id = SessionId::default();
        let ctx = self.create_context();
        // If session creation fails, no session exists — propagate immediately.
        self.create_session_with(ctx, &id, agent_type, setup)?;

        // Execute turn and extract output. Capture the result so we can
        // guarantee cleanup before propagating errors.
        let result = async {
            let exec_result = self.process_turn(&id).await?;

            let output = exec_result
                .output::<T>()
                .cloned()
                .ok_or(SessionError::OutputNotFound(std::any::type_name::<T>()))?;

            Ok(output)
        }
        .await;

        // Always clean up the ephemeral session.
        let _ = self.delete_session(&id).await;

        result
    }

    // ─────────────────────────────────────────────────────────────────────
    // Helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Looks up a live session by ID.
    fn get_state(&self, id: &SessionId) -> Result<Arc<SessionState>, SessionError> {
        self.inner
            .sessions
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::SessionNotFound(id.clone()))
    }
}

/// Returns the current UTC time formatted as ISO 8601 (e.g. `2026-03-31T12:00:00Z`).
fn utc_now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Serialization helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Serializes all registered resources from a context into [`SessionData`].
fn serialize_context(
    serializers: &[Arc<dyn ResourceSerializer>],
    agent_type: AgentTypeId,
    turn_number: TurnNumber,
    created_at: &str,
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
        created_at: created_at.to_owned(),
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
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use polaris_sessions::SessionsPlugin;
/// use polaris_sessions::store::memory::InMemoryStore;
/// use polaris_core_plugins::PersistencePlugin;
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
/// ```
pub struct SessionsPlugin {
    store: Arc<dyn SessionStore>,
    auto_checkpoint: bool,
}

impl SessionsPlugin {
    /// Creates a new sessions plugin backed by the given store.
    ///
    /// Auto-checkpoint is enabled by default.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsPlugin;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let plugin = SessionsPlugin::new(Arc::new(InMemoryStore::new()));
    /// ```
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            auto_checkpoint: true,
        }
    }

    /// Disables automatic checkpointing after each successful turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsPlugin;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let plugin = SessionsPlugin::new(Arc::new(InMemoryStore::new()))
    ///     .without_auto_checkpoint();
    /// ```
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

    async fn ready(&self, server: &mut Server) {
        let persistence = server
            .api::<PersistenceAPI>()
            .expect("SessionsPlugin requires PersistencePlugin");
        let sessions = server
            .api::<SessionsAPI>()
            .expect("SessionsAPI should be present after build");
        sessions.set_serializers(persistence.serializers());
        sessions.set_graph_apis(
            server.api::<HooksAPI>().cloned(),
            server.api::<MiddlewareAPI>().cloned(),
        );
        sessions.set_context_factory(server.context_factory());
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PersistencePlugin>()]
    }
}
