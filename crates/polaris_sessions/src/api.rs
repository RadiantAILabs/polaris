//! Sessions API, plugin, and internal session state.
//!
//! [`SessionsAPI`] is the primary interface for managing agent sessions.
//! It is registered as an [`API`] by [`SessionsPlugin`]
//! and accessed via `server.api::<SessionsAPI>()`.

use crate::checkpoint::{
    CheckpointHistory, CheckpointMeta, CheckpointOrigin, CheckpointRetention, CheckpointSet,
    ResourceRef, RevisionId, SessionCheckpoints,
};
use crate::error::{SessionError, WiringError};
use crate::guard::SessionGuard;
use crate::info::{SessionInfo, SessionMetadata, SessionStatus};
use crate::models::{BucketGranularity, SessionUptimeResponse};
use crate::models::{Turn, TurnStatus, TurnSummary};
use crate::store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore, TurnNumber};
use crate::uptime::{LifecycleKind, UptimeRecorder};
use chrono::DateTime;
use chrono::Utc;
use hashbrown::{HashMap, hash_map::Entry};
use parking_lot::RwLock;
use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, ResourceSerializer};
use polaris_core_plugins::{IOContent, IOMessage, IOSource};
use polaris_graph::MiddlewareAPI;
use polaris_graph::cancel::CancellationSource;
use polaris_graph::hooks::HooksAPI;
use polaris_graph::{
    ExecutionError, ExecutionResult, Graph, GraphExecutor, GraphSignature, RunLabels, SignatureDiff,
};
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use polaris_system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris_system::resource::Output;
use polaris_system::server::{ContextFactory, Server};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tracing::Instrument;

/// Soft cap on turn history retained per session.
///
/// Each completed turn produces one [`TurnHistoryEntry`]. Bounding the
/// deque prevents an unbounded-running session from leaking memory; the
/// oldest entries are evicted first when the cap is reached.
const MAX_TURN_HISTORY: usize = 256;

/// Maximum number of IO messages retained in one turn-history entry.
const MAX_RECORDED_MESSAGES_PER_TURN: usize = 256;

/// Maximum retained payload size across all messages in one turn.
const MAX_RECORDED_BYTES_PER_TURN: usize = 1024 * 1024;

/// Maximum retained payload size for one message.
const MAX_RECORDED_BYTES_PER_MESSAGE: usize = 64 * 1024;

/// Maximum metadata entries copied from one recorded message.
const MAX_RECORDED_METADATA_ENTRIES: usize = 64;

/// Max characters retained from the most recent IO message in a turn's
/// `last_message_preview`.
const PREVIEW_MAX_CHARS: usize = 120;

// ─────────────────────────────────────────────────────────────────────────────
// SessionState (internal)
// ─────────────────────────────────────────────────────────────────────────────

/// Live state for a single session.
///
/// The context is held in a `tokio::sync::Mutex` because it is held across
/// the async `execute()` call. Checkpoints use a `parking_lot::Mutex`
/// because they are only accessed synchronously.
pub(crate) struct SessionState {
    pub(crate) ctx: tokio::sync::Mutex<SystemContext<'static>>,
    graph: Graph,
    executor: GraphExecutor,
    agent_type: AgentTypeId,
    /// The registered agent's version, resolved at session creation and
    /// on resume. `None` when the agent reports no version.
    agent_version: Option<String>,
    /// The registered agent's description, resolved at session creation and
    /// on resume. `None` when the agent reports no description.
    agent_description: Option<String>,
    pub(crate) turn_number: AtomicU32,
    /// The in-flight turn's cancellation authority, or `None` when the
    /// session is idle.
    ///
    /// Filled immediately before the executor call and cleared immediately
    /// after, so a raise that lands outside that window is not latched into
    /// the next turn. A `parking_lot::Mutex` rather than the async context
    /// mutex is the whole point: a turn holds the context lock for its entire
    /// duration, so [`SessionsAPI::interrupt_turn`] would deadlock behind it.
    cancellation: parking_lot::Mutex<Option<CancellationSource>>,
    /// The session's retained checkpoints, their identity counter, their
    /// removal history and the pins holding them.
    ///
    /// One field behind one synchronous lock, as before: every
    /// [`CheckpointSet`] operation is synchronous, so the comment above still
    /// holds and the session gains no second lock.
    pub(crate) checkpoints: parking_lot::Mutex<CheckpointSet>,
    created_at: String,
    /// Read-only sessions reject all mutation paths but stay queryable.
    /// Set by [`SessionsAPI::run_oneshot_preserved`] after the terminal
    /// turn completes.
    pub(crate) read_only: AtomicBool,
    /// Pre-built `(session_id, agent_type)` labels shared across every turn.
    /// `RunLabels` is `Arc`-backed, so per-turn cost is one `Arc::clone`
    /// instead of allocating a fresh `HashMap`.
    labels: RunLabels,
    /// Per-session turn history used by the dashboard endpoints.
    ///
    /// Bounded ring; oldest entries are evicted once
    /// [`MAX_TURN_HISTORY`] entries accumulate.
    turns: parking_lot::Mutex<VecDeque<TurnHistoryEntry>>,
}

/// In-memory record for one turn, used as the source-of-truth for
/// [`SessionsAPI::turn_history`] and [`SessionsAPI::turn`].
#[derive(Debug, Clone)]
struct TurnHistoryEntry {
    turn: TurnNumber,
    started_at: String,
    finished_at: Option<String>,
    status: TurnStatus,
    messages: Vec<IOMessage>,
    io_message_count: u32,
    recorded_bytes: usize,
    messages_truncated: bool,
    last_message_preview: Option<String>,
}

/// Owns the accounting one turn opens — its history entry, its `Active`
/// lifecycle mark and its interrupt slot — and closes all three on **every**
/// exit path, including the caller dropping the turn future.
///
/// Bare stores around the executor `await` were correct only for `return`.
/// A client that `POST`s a turn and disconnects makes axum drop the handler,
/// which drops the turn future at that `await`: the context lock is released
/// so the session is genuinely idle, but the slot stayed `Some(dead_source)`,
/// so [`SessionsAPI::interrupt_turn`] answered `Ok(true)` for a turn that no
/// longer existed — and a host implementing the documented "one press
/// interrupts, a second quits" escalation on that boolean quits on the user's
/// first press. The same drop stranded the turn record at
/// [`TurnStatus::Running`] and the session at
/// [`LifecycleKind::Active`] forever.
struct TurnGuard<'a> {
    api: &'a SessionsAPI,
    state: &'a SessionState,
    id: &'a SessionId,
    turn: TurnNumber,
    /// Set once the turn reached a terminal status of its own, which is what
    /// tells `Drop` the only thing left to do is nothing.
    settled: bool,
}

impl<'a> TurnGuard<'a> {
    /// Opens the turn: history entry, `Active` lifecycle mark, interrupt slot.
    ///
    /// Recording before the executor runs is deliberate — a `Running` entry is
    /// visible from the dashboard the moment a turn is in flight.
    fn open(
        api: &'a SessionsAPI,
        state: &'a SessionState,
        id: &'a SessionId,
        turn: TurnNumber,
        cancellation: CancellationSource,
    ) -> Self {
        api.start_turn_record(state, turn);
        api.inner.uptime.record(id, LifecycleKind::Active);
        *state.cancellation.lock() = Some(cancellation);
        Self {
            api,
            state,
            id,
            turn,
            settled: false,
        }
    }

    /// Closes the turn with the status the executor actually produced.
    fn settle(&mut self, status: TurnStatus) {
        *self.state.cancellation.lock() = None;
        self.api.finish_turn_record(self.state, self.turn, status);
        self.api.inner.uptime.record(self.id, LifecycleKind::Idle);
        self.settled = true;
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Reached only when the turn future was dropped inside the executor
        // `await`. `Failed` rather than `Interrupted`: an abandoned turn
        // neither advanced the turn number nor attempted a checkpoint, and
        // `TurnStatus::Interrupted` records both. It is not `Completed`
        // either — nothing returned a result.
        self.settle(TurnStatus::Failed);
    }
}

fn build_session_labels(id: &SessionId, agent_type: &AgentTypeId) -> RunLabels {
    RunLabels::from([
        ("session_id", id.as_str().to_owned()),
        ("agent_type", agent_type.as_str().to_owned()),
    ])
}

fn status_for(state: &SessionState) -> SessionStatus {
    if state.read_only.load(Ordering::Acquire) {
        SessionStatus::ReadOnly
    } else {
        SessionStatus::Active
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionsAPI
// ─────────────────────────────────────────────────────────────────────────────

/// Stable name for a registered capability contract.
///
/// A contract name is the coordination key that lets producer and consumer
/// plugins agree on a graph-interface capability without comparing
/// process-local [`TypeId`](std::any::TypeId)s over the wire.
/// Names must be non-empty, have no leading or trailing whitespace, and
/// contain no control characters.
///
/// # Examples
///
/// ```
/// use polaris_sessions::ContractName;
///
/// let name = ContractName::new("self-learn")?;
/// assert_eq!(name.as_str(), "self-learn");
/// # Ok::<(), polaris_sessions::ContractNameError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContractName(String);

impl ContractName {
    /// Creates a validated contract name.
    ///
    /// # Errors
    ///
    /// Returns [`ContractNameError`] when the value is empty, has leading or
    /// trailing whitespace, or contains a control character.
    pub fn new(value: impl Into<String>) -> Result<Self, ContractNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ContractNameError::Empty);
        }
        if value.trim() != value {
            return Err(ContractNameError::SurroundingWhitespace);
        }
        if value.chars().any(char::is_control) {
            return Err(ContractNameError::ControlCharacter);
        }
        Ok(Self(value))
    }

    /// Returns this contract name as a string slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_sessions::ContractName;
    ///
    /// let name = ContractName::new("summarize")?;
    /// assert_eq!(name.as_str(), "summarize");
    /// # Ok::<(), polaris_sessions::ContractNameError>(())
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Converts this contract name into its owned string.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_sessions::ContractName;
    ///
    /// let name = ContractName::new("summarize")?;
    /// assert_eq!(name.into_string(), "summarize".to_owned());
    /// # Ok::<(), polaris_sessions::ContractNameError>(())
    /// ```
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for ContractName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for ContractName {
    type Error = ContractNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ContractName {
    type Error = ContractNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl std::fmt::Display for ContractName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error returned when constructing an invalid [`ContractName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ContractNameError {
    /// The supplied name was empty.
    #[error("contract name must not be empty")]
    Empty,
    /// The supplied name had leading or trailing whitespace.
    #[error("contract name must not have leading or trailing whitespace")]
    SurroundingWhitespace,
    /// The supplied name contained a control character.
    #[error("contract name must not contain control characters")]
    ControlCharacter,
}

/// Snapshot of one registered agent type, as returned by
/// [`SessionsAPI::agent_type_infos`].
///
/// Carries the typed [`GraphSignature`] — in-process consumers compare with
/// [`GraphSignature::satisfies`] / [`compatible_with`](GraphSignature::compatible_with);
/// wire surfaces render it human-readable via [`GraphSignature::to_rendered`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentTypeInfo {
    /// The agent's stable type name.
    pub name: &'static str,
    /// The agent graph's IO signature, cached at registration.
    pub signature: GraphSignature,
    /// Names of the registered capability contracts the signature
    /// [satisfies](GraphSignature::satisfies), sorted.
    pub contracts: Vec<ContractName>,
}

/// A registered agent pattern plus the [`GraphSignature`] cached from the
/// graph already built for validation at registration time — so signature
/// reads (contract satisfaction, the authenticated agent-type details
/// endpoint) never rebuild the graph.
struct RegisteredAgent {
    agent: Arc<dyn Agent>,
    signature: GraphSignature,
}

/// Internal state shared by all clones of a [`SessionsAPI`].
struct SessionsInner {
    store: Arc<dyn SessionStore>,
    serializers: RwLock<Arc<Vec<Arc<dyn ResourceSerializer>>>>,
    agents: RwLock<HashMap<AgentTypeId, RegisteredAgent>>,
    /// Named capability contracts (slot [`GraphSignature`]s), consumer-
    /// registered via [`SessionsAPI::register_contract`]. Satisfaction is
    /// computed at read time from the cached agent signatures, so
    /// registration order never matters.
    contracts: RwLock<HashMap<ContractName, GraphSignature>>,
    sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>,
    auto_checkpoint: AtomicBool,
    /// Bounds on one session's retained checkpoint set. Injected policy, set
    /// by [`SessionsPlugin::with_checkpoint_retention`], never a constant at
    /// a call site.
    retention: RwLock<CheckpointRetention>,
    hooks: OnceLock<HooksAPI>,
    middleware: OnceLock<MiddlewareAPI>,
    context_factory: OnceLock<ContextFactory>,
    /// Lifecycle recorder backing the per-session uptime endpoint.
    uptime: UptimeRecorder,
}

/// Server API for session lifecycle management.
///
/// Reach for `SessionsAPI` whenever application code — an HTTP handler, a
/// background job, a test — needs to drive agents: register agent
/// patterns, create or resume sessions, execute turns, checkpoint and
/// roll back state, and persist sessions to a [`SessionStore`].
///
/// # Provided by
///
/// [`SessionsPlugin`], which calls
/// [`Server::insert_api`](polaris_system::server::Server::insert_api) with
/// the configured [`SessionStore`] during its `build()` phase. The plugin
/// completes wiring (graph APIs, context factory, resource serializers) in
/// `ready()` — see [Lifecycle](#lifecycle).
///
/// # Surface
///
/// **Agent registration & lookup**
///
/// | Method | Description |
/// |--------|-------------|
/// | `register_agent` | Registers an agent pattern under its type id (caching its graph signature). |
/// | `registered_agents` | Lists the names of registered agent types. |
/// | `find_agent_type` | Resolves an [`AgentTypeId`] from an agent name. |
/// | `agent_signature` | Returns a registered agent's cached graph signature. |
///
/// **Capability contracts**
///
/// | Method | Description |
/// |--------|-------------|
/// | `register_contract` | Registers a named capability contract (a slot [`GraphSignature`]). |
/// | `contract_diff` | Satisfaction diff between a registered agent and a contract. |
/// | `agent_type_infos` | Snapshot of every agent type with signature and satisfied contracts. |
///
/// **Session lifecycle**
///
/// | Method | Description |
/// |--------|-------------|
/// | `create_context` | Creates a fresh system context for a session. |
/// | `create_session` | Creates a session bound to an agent type. |
/// | `create_session_with` | Creates a session, running a setup closure over its context. |
/// | `create_session_with_executor` | Creates a session with a caller-supplied graph executor. |
/// | `setup_session` | Re-runs the agent's setup phase (e.g. after a rollback). |
/// | `delete_session` | Removes a live session and frees its resources. |
/// | `scoped_session` | Creates a [`SessionGuard`] that deletes its session on drop. |
///
/// **Turn execution**
///
/// | Method | Description |
/// |--------|-------------|
/// | `process_turn` | Executes one agent turn. |
/// | `process_turn_with` | Executes one turn after a per-turn setup closure. |
/// | `try_process_turn` | Like `process_turn`, but returns `SessionBusy` instead of waiting when a turn is already running. |
/// | `try_process_turn_with` | `try_process_turn` plus a per-turn setup closure. |
/// | `run_oneshot` | Creates an ephemeral session, runs one turn, returns typed output, deletes the session. |
/// | `run_oneshot_preserved` | Like `run_oneshot`, but keeps the session alive read-only for later inspection. |
/// | `with_context` | Runs a closure against a live session's context to read or inject resources. |
/// | [`interrupt_turn`](Self::interrupt_turn) | Raises the cancellation signal for the turn currently in flight. Synchronous, idempotent, never takes the context lock; `Ok(true)` means a turn was in flight, `Ok(false)` that the session was idle. |
///
/// **Checkpoints & rollback**
///
/// | Method | Description |
/// |--------|-------------|
/// | `checkpoints` | The [`SessionCheckpoints`] handle: stable checkpoint identity, branch-safe restore, retention bounds and availability reporting. |
/// | `checkpoint` | Snapshots the session's resources at the current turn. Superseded by `checkpoints().capture`. |
/// | `list_checkpoints` | Lists the turn numbers that have checkpoints. Superseded by `checkpoints().list`. |
/// | `rollback` | Restores the session's resources to a checkpointed turn. Superseded by `checkpoints().restore`; see its rustdoc for the run-identity hazard on the automatic path. |
/// | `set_auto_checkpoint` | Enables or disables creation, resume, completed-turn and interrupted-turn checkpoints. |
///
/// **Persistence**
///
/// | Method | Description |
/// |--------|-------------|
/// | `save_session` | Persists a session's state to the backing [`SessionStore`]. |
/// | `resume_session` | Reconstructs a session from the store. |
/// | `resume_session_with` | `resume_session` plus a setup closure. |
/// | `resume_session_with_executor` | `resume_session` with a caller-supplied executor. |
///
/// **Introspection**
///
/// | Method | Description |
/// |--------|-------------|
/// | `list_sessions` | Lists session ids known to the backing store. |
/// | `list_live_sessions` | Lists ids of sessions currently held in memory. |
/// | `session_info` | Returns metadata for one live session. |
/// | `list_session_metadata` | Returns metadata for every live session. |
/// | `turn_history` | Returns recorded turn summaries. |
/// | `record_turn_messages` | Attaches IO messages observed during a turn to turn history. |
/// | `turn` | Returns the detail of one recorded turn. |
/// | `uptime_buckets` | Returns a bucketed session-lifecycle series. |
///
/// **Plugin wiring** — called by [`SessionsPlugin`] / the HTTP layer, not
/// by general consumers:
///
/// | Method | Description |
/// |--------|-------------|
/// | `new` | Constructs the API over a [`SessionStore`]. |
/// | `set_serializers` | Snapshots the resource serializers from [`PersistenceAPI`]. |
/// | `set_graph_apis` | Stores the hooks/middleware APIs used during turn execution. |
/// | `set_context_factory` | Stores the factory used to build fresh contexts. |
///
/// # Lifecycle
///
/// The wiring methods (`set_serializers`, `set_graph_apis`,
/// `set_context_factory`) are called once by [`SessionsPlugin`] during its
/// `ready()` phase; `set_graph_apis` and `set_context_factory` return
/// [`WiringError`] if invoked a second time. `set_serializers` has no such
/// guard: it **replaces** the current serializer snapshot wholesale, so a
/// second call drops any serializer the first one carried. Turns already in
/// flight hold the snapshot they resolved at their start and are unaffected.
///
/// `register_agent` and `register_contract` are intended to be called after
/// `ready()` and before the first turn. Calling either later is accepted
/// rather than rejected, and takes effect only for subsequent lookups and
/// turns: a live session keeps the graph it was created with, so
/// re-registering an agent name changes nothing for sessions already running
/// under it. `register_contract` is the exception that stays strict — a
/// second registration of the same name with a *different* signature returns
/// [`SessionError::ContractConflict`] whenever it happens.
///
/// Every other method — session creation, turn execution, checkpoints,
/// persistence, introspection — is a runtime operation, valid for the
/// lifetime of the server once the plugin is ready. All methods take `&self`.
///
/// # Composition
///
/// **Provider-scoped, with one open-extension point.** Only
/// [`SessionsPlugin`] inserts this API and performs the build-time wiring;
/// consumers invoke its session and turn operations (create a session, run
/// a turn, checkpoint) but do not contribute to them. The exception is the
/// capability-contract registry: any plugin may add named contracts via
/// [`register_contract`](Self::register_contract) after `ready()`, so that
/// one axis composes openly across plugins. The API is cheaply cloneable;
/// see [Interior Mutability](#interior-mutability).
///
/// # Example consumers
///
/// - `HttpPlugin` (feature `sessions-http`) — resolves `SessionsAPI` during
///   app `ready()` and uses it as handler state for the sessions REST
///   endpoints.
/// - Consumer plugins defining agent capabilities — call
///   [`register_contract`](Self::register_contract) to publish named
///   graph-interface contracts that registered agents may satisfy.
/// - Application code and tests — create sessions, run turns, checkpoint,
///   resume, and inspect session state through this single handle.
///
/// # Cloning
///
/// `SessionsAPI` is cheaply cloneable (backed by `Arc`). Clones share
/// the same underlying state, making it suitable for use as shared state
/// in HTTP handlers.
///
/// # Auto-Checkpoint
///
/// Enabled by default. A checkpoint is captured at four points: session
/// creation, [`resume_session`](Self::resume_session), and the return path of
/// every **completed** and every **interrupted**
/// [`process_turn`](Self::process_turn). A turn that failed for any other
/// reason takes no checkpoint — its context is not a state anyone can reason
/// about.
///
/// Capture is **synchronous**, taken while the turn still holds the session's
/// context lock, so the snapshot is of exactly the state the turn produced and
/// its serialization cost is on the turn's latency. Failures — including a
/// retention refusal — are logged at `warn` and never propagate: an automatic
/// checkpoint must not take a user's completed turn down with it.
///
/// The retained set is **bounded** by the [`CheckpointRetention`] the host
/// injects through [`SessionsPlugin::with_checkpoint_retention`], 64
/// checkpoints / 16 MiB of serialized payload by default;
/// [`CheckpointRetention::unbounded`] opts out. Retained checkpoints are
/// **durable**: they ride [`SessionData`] through the configured
/// [`SessionStore`], so [`save_session`](Self::save_session) writes them and
/// [`resume_session`](Self::resume_session) brings them back with their
/// identities intact.
///
/// Disable the whole mechanism — turn captures and boundary captures alike —
/// via [`SessionsPlugin::without_auto_checkpoint`].
///
/// # Interior Mutability
///
/// All methods take `&self` and use internal locks for thread safety.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris_core_plugins::PersistencePlugin;
/// use polaris_sessions::store::SessionStore;
/// use polaris_sessions::{SessionsAPI, SessionsPlugin};
/// use polaris_sessions::store::memory::InMemoryStore;
/// use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version};
/// use polaris_system::server::Server;
///
/// // Provider side — the shape of `SessionsPlugin::build`: declare the
/// // capability, then construct the API over the configured store and publish
/// // it so consumers can resolve it.
/// struct MySessionsPlugin {
///     store: Arc<dyn SessionStore>,
/// }
///
/// impl Plugin for MySessionsPlugin {
///     const ID: &'static str = "example::sessions";
///     const VERSION: Version = Version::new(0, 1, 0);
///
///     fn access(&self) -> PluginAccess {
///         PluginAccess::new().provides::<SessionsAPI>(SessionsAPI::CONTRACT_VERSION)
///     }
///
///     fn build(&self, server: &mut Server) {
///         server.insert_api(SessionsAPI::new(Arc::clone(&self.store)));
///     }
/// }
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // In practice a host adds the shipped plugin rather than writing its own.
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
/// server.finish().await?;
///
/// // Consumer side: resolve the API after plugin setup.
/// let sessions = server
///     .api::<SessionsAPI>()
///     .expect("SessionsPlugin provides SessionsAPI");
/// let live_sessions = sessions.list_live_sessions();
/// assert!(live_sessions.is_empty());
///
/// server.cleanup().await;
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

/// The contract version at which [`SessionsAPI`] is exposed as a capability. Plugins that
/// consume it (e.g. the session `HttpPlugin`) declare a requirement against this version;
/// bump it when the API's public surface changes incompatibly.
///
/// 0.1.1 added the capability-contract registry (`register_contract`,
/// `contract_diff`, `agent_signature`, `agent_type_infos`) — additive.
///
/// 0.1.2 added the checkpoint surface ([`checkpoints`](SessionsAPI::checkpoints)),
/// turn interruption ([`interrupt_turn`](SessionsAPI::interrupt_turn)), and made
/// the turn-history and lifecycle surface unconditional —
/// [`turn_history`](SessionsAPI::turn_history), [`turn`](SessionsAPI::turn),
/// [`record_turn_messages`](SessionsAPI::record_turn_messages) and
/// [`uptime_buckets`](SessionsAPI::uptime_buckets) were previously behind the
/// `sessions-http` feature. All additive; a caret requirement on 0.1.1 admits
/// it, so no consumer changes — but a consumer that needs any of these methods
/// must require 0.1.2, since 0.1.1 does not carry them.
impl Contract for SessionsAPI {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 2);
}

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
                contracts: RwLock::new(HashMap::new()),
                sessions: RwLock::new(HashMap::new()),
                auto_checkpoint: AtomicBool::new(true),
                retention: RwLock::new(CheckpointRetention::default()),
                hooks: OnceLock::new(),
                middleware: OnceLock::new(),
                context_factory: OnceLock::new(),
                uptime: UptimeRecorder::new(),
            }),
        }
    }

    /// Snapshots the current set of serializers from [`PersistenceAPI`].
    ///
    /// Called during the plugin ready phase.
    pub fn set_serializers(&self, serializers: Vec<Arc<dyn ResourceSerializer>>) {
        *self.inner.serializers.write() = Arc::new(serializers);
    }

    /// Returns the checkpoint surface for this API.
    ///
    /// Cheap: one `Arc` clone, the same cost as cloning `SessionsAPI` itself,
    /// so callers need not cache the handle.
    ///
    /// Everything beyond the legacy [`checkpoint`](Self::checkpoint) /
    /// [`list_checkpoints`](Self::list_checkpoints) /
    /// [`rollback`](Self::rollback) trio lives on
    /// [`SessionCheckpoints`]: stable checkpoint identity, branch-safe
    /// restore, retention bounds and availability reporting. See the
    /// [`checkpoint`](crate::checkpoint) module.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # use polaris_sessions::checkpoint::CheckpointOrigin;
    /// # async fn example(
    /// #     sessions: &SessionsAPI,
    /// #     id: &SessionId,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// let point = sessions.checkpoints().capture(id, CheckpointOrigin::Manual).await?;
    /// sessions.checkpoints().restore(id, point.id).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn checkpoints(&self) -> SessionCheckpoints {
        SessionCheckpoints::new(self.clone())
    }

    /// Sets the checkpoint retention policy.
    ///
    /// Crate-internal: a host configures this through
    /// [`SessionsPlugin::with_checkpoint_retention`], which keeps the policy a
    /// build-time composition choice rather than something a running consumer
    /// can change under another consumer's pins.
    pub(crate) fn set_checkpoint_retention(&self, retention: CheckpointRetention) {
        *self.inner.retention.write() = retention;
    }

    /// The configured checkpoint retention policy.
    pub(crate) fn checkpoint_retention(&self) -> CheckpointRetention {
        *self.inner.retention.read()
    }

    /// The current serializer snapshot.
    pub(crate) fn serializers(&self) -> Arc<Vec<Arc<dyn ResourceSerializer>>> {
        Arc::clone(&self.inner.serializers.read())
    }

    /// Looks up live session state, for the checkpoint surface.
    pub(crate) fn session_state(&self, id: &SessionId) -> Result<Arc<SessionState>, SessionError> {
        self.get_state(id)
    }

    /// Captures the context as a checkpoint while its lock is already held.
    ///
    /// The single capture path: [`SessionCheckpoints::capture`], the legacy
    /// [`checkpoint`](Self::checkpoint), `auto_checkpoint`, the session
    /// boundary captures and [`SessionCheckpoints::restore`]'s pre-restore
    /// snapshot all arrive here, so retention is enforced in exactly one
    /// place.
    ///
    /// `label` overrides the display turn label; `None` uses the resume turn.
    /// `resume_turn` is always the counter's current value, which is what
    /// makes an automatic capture resume as correctly as a manual one.
    pub(crate) fn capture_locked(
        &self,
        state: &SessionState,
        id: &SessionId,
        ctx: &SystemContext<'static>,
        origin: CheckpointOrigin,
        label: Option<TurnNumber>,
    ) -> Result<CheckpointMeta, SessionError> {
        if let Some(err) = state.checkpoints.lock().coherence_refusal(id) {
            return Err(err);
        }
        let serializers = self.serializers();
        let resources = serialize_resources(&serializers, ctx)?;
        let resume_turn = state.turn_number.load(Ordering::Acquire);
        let retention = self.checkpoint_retention();
        state.checkpoints.lock().admit_checkpoint(
            id,
            resources,
            origin,
            resume_turn,
            label,
            utc_now_iso8601(),
            retention,
        )
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
    /// # Errors
    ///
    /// Returns [`WiringError::HooksAlreadySet`] or
    /// [`WiringError::MiddlewareAlreadySet`] if the corresponding API has
    /// already been wired.
    pub fn set_graph_apis(
        &self,
        hooks: Option<HooksAPI>,
        middleware: Option<MiddlewareAPI>,
    ) -> Result<(), WiringError> {
        if let Some(hooks) = hooks {
            self.inner
                .hooks
                .set(hooks)
                .map_err(|_| WiringError::HooksAlreadySet)?;
        }
        if let Some(middleware) = middleware {
            self.inner
                .middleware
                .set(middleware)
                .map_err(|_| WiringError::MiddlewareAlreadySet)?;
        }
        Ok(())
    }

    /// Sets the context factory used to create fresh system contexts.
    ///
    /// Called automatically by [`SessionsPlugin::ready()`]. Only call this
    /// manually if you are not using `SessionsPlugin`.
    ///
    /// # Errors
    ///
    /// Returns [`WiringError::ContextFactoryAlreadySet`] if the factory has
    /// already been wired.
    pub fn set_context_factory(&self, factory: ContextFactory) -> Result<(), WiringError> {
        self.inner
            .context_factory
            .set(factory)
            .map_err(|_| WiringError::ContextFactoryAlreadySet)
    }

    /// Creates a fresh [`SystemContext`] using the stored
    /// [`ContextFactory`].
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
    /// server.finish().await.unwrap();
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
    /// The graph built for validation is also used to cache the agent's
    /// [`GraphSignature`], so [`agent_signature`](Self::agent_signature),
    /// [`agent_type_infos`](Self::agent_type_infos), and
    /// [`contract_diff`](Self::contract_diff) never rebuild it.
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
        let registered = RegisteredAgent {
            signature: graph.signature(),
            agent: Arc::new(agent),
        };
        self.inner.agents.write().insert(id, registered);
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
    // Capability contracts
    // ─────────────────────────────────────────────────────────────────────

    /// Registers a named capability contract.
    ///
    /// A capability contract is a slot [`GraphSignature`] under a well-known
    /// name (e.g. `"self-learn"`), registered by the consumer plugin that
    /// defines the capability — none is registered by default. Every
    /// registered agent whose cached signature
    /// [satisfies](GraphSignature::satisfies) the contract by subsumption
    /// advertises it through [`agent_type_infos`](Self::agent_type_infos)
    /// (and, over HTTP, the authenticated
    /// `GET /v1/sessions/agent-types/details` route), so trusted
    /// out-of-process callers discover capable agents by contract name
    /// instead of re-implementing the signature check — which cannot be
    /// sound across process boundaries, since `TypeId`s do not unify across
    /// binaries.
    ///
    /// Satisfaction is computed at read time from signatures cached at
    /// [`register_agent`](Self::register_agent), so agents and contracts may
    /// be registered in either order. Registering the same name with an
    /// identical signature is a no-op. A registrant that *expects* its own
    /// agent to satisfy its own contract should assert that explicitly via
    /// [`contract_diff`](Self::contract_diff).
    ///
    /// Not to be confused with [`polaris_system::plugin::Contract`], the
    /// plugin-capability version marker: a capability contract here is a
    /// graph-interface slot signature — the same sense in which dynamic
    /// slots call their signatures contracts.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::ContractConflict`] if the name is already
    /// registered with a different signature. Contracts are cross-plugin
    /// coordination points; silent last-write-wins would let one plugin
    /// redefine another's capability.
    /// Returns [`SessionError::InvalidContractName`] if the supplied name is
    /// not a normalized, non-empty coordination key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_graph::GraphSignature;
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// let contract = GraphSignature::new().produce::<String>();
    /// sessions.register_contract("summarize", contract.clone())?;
    /// // Identical re-registration is a no-op…
    /// sessions.register_contract("summarize", contract)?;
    /// // …but a different signature under the same name is refused.
    /// let other = GraphSignature::new().produce::<u32>();
    /// assert!(sessions.register_contract("summarize", other).is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_contract(
        &self,
        name: impl AsRef<str>,
        signature: GraphSignature,
    ) -> Result<(), SessionError> {
        let name = ContractName::new(name.as_ref())?;
        match self.inner.contracts.write().entry(name) {
            Entry::Occupied(entry) => {
                if *entry.get() == signature {
                    Ok(())
                } else {
                    Err(SessionError::ContractConflict {
                        name: entry.key().clone(),
                    })
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(signature);
                Ok(())
            }
        }
    }

    /// Computes the satisfaction diff between a registered agent and a
    /// registered contract.
    ///
    /// Returns the [`SignatureDiff`] from
    /// [`GraphSignature::satisfies_diff`] — empty exactly when the agent
    /// [satisfies](GraphSignature::satisfies) the contract. A contract
    /// registrant uses this at startup to assert that its own agent
    /// satisfies its own contract, logging the human-readable drift if not.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::InvalidContractName`] if the contract name is
    /// invalid, [`SessionError::ContractNotFound`] if no contract with that
    /// name is registered, or [`SessionError::AgentNotFound`] if no agent with
    /// that name is registered.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_agent::Agent;
    /// use polaris_graph::{Graph, GraphSignature};
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// async fn step() -> u32 { 7 }
    /// struct MyAgent;
    /// impl Agent for MyAgent {
    ///     fn name(&self) -> &'static str { "MyAgent" }
    ///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
    /// }
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// sessions.register_agent(MyAgent)?;
    /// sessions
    ///     .register_contract("produce-u32", GraphSignature::new().produce::<u32>())
    ///     ?;
    ///
    /// let diff = sessions.contract_diff("produce-u32", "MyAgent")?;
    /// assert!(diff.is_empty(), "agent drifted from its contract: {diff}");
    /// # Ok(())
    /// # }
    /// ```
    pub fn contract_diff(
        &self,
        contract: impl AsRef<str>,
        agent_name: &str,
    ) -> Result<SignatureDiff, SessionError> {
        let contract = ContractName::new(contract.as_ref())?;
        let contracts = self.inner.contracts.read();
        let slot = contracts
            .get(&contract)
            .ok_or_else(|| SessionError::ContractNotFound(contract.clone()))?;
        let agents = self.inner.agents.read();
        let registered = agents
            .iter()
            .find(|(k, _)| k.as_str() == agent_name)
            .map(|(_, v)| v)
            .ok_or_else(|| SessionError::AgentNotFound(agent_name.to_owned()))?;
        Ok(registered.signature.satisfies_diff(slot))
    }

    /// Returns the cached [`GraphSignature`] of a registered agent, or
    /// `None` if no agent with that name has been registered.
    ///
    /// The signature is cached at [`register_agent`](Self::register_agent)
    /// time, so this never rebuilds the agent's graph. For a bulk snapshot
    /// including contract satisfaction, use
    /// [`agent_type_infos`](Self::agent_type_infos).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_agent::Agent;
    /// use polaris_graph::{Graph, GraphSignature};
    /// use polaris_sessions::SessionsAPI;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// async fn step() -> u32 { 7 }
    /// struct MyAgent;
    /// impl Agent for MyAgent {
    ///     fn name(&self) -> &'static str { "MyAgent" }
    ///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
    /// }
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// sessions.register_agent(MyAgent)?;
    ///
    /// let signature = sessions.agent_signature("MyAgent").expect("agent was registered");
    /// assert!(signature.satisfies(&GraphSignature::new().produce::<u32>()));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn agent_signature(&self, name: &str) -> Option<GraphSignature> {
        self.inner
            .agents
            .read()
            .iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, registered)| registered.signature.clone())
    }

    /// Returns a snapshot of every registered agent type — name, cached
    /// signature, and the names of the registered contracts it satisfies —
    /// sorted by name.
    ///
    /// Satisfaction is joined at read time (one pass over both registries),
    /// so the result never depends on the order agents and contracts were
    /// registered in. This is the composite read behind
    /// the authenticated `GET /v1/sessions/agent-types/details` route; for
    /// a single agent's signature, prefer
    /// [`agent_signature`](Self::agent_signature).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_agent::Agent;
    /// use polaris_graph::{Graph, GraphSignature};
    /// use polaris_sessions::{ContractName, SessionsAPI};
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// async fn step() -> u32 { 7 }
    /// struct MyAgent;
    /// impl Agent for MyAgent {
    ///     fn name(&self) -> &'static str { "MyAgent" }
    ///     fn build(&self, graph: &mut Graph) { graph.add_system(step); }
    /// }
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    /// sessions.register_agent(MyAgent)?;
    /// sessions
    ///     .register_contract("produce-u32", GraphSignature::new().produce::<u32>())
    ///     ?;
    ///
    /// let infos = sessions.agent_type_infos();
    /// assert_eq!(infos[0].name, "MyAgent");
    /// assert_eq!(infos[0].contracts, vec![ContractName::new("produce-u32")?]);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn agent_type_infos(&self) -> Vec<AgentTypeInfo> {
        let contracts = self.inner.contracts.read();
        let agents = self.inner.agents.read();
        let mut infos: Vec<AgentTypeInfo> = agents
            .iter()
            .map(|(id, registered)| {
                let mut satisfied: Vec<ContractName> = contracts
                    .iter()
                    .filter(|(_, slot)| registered.signature.satisfies(slot))
                    .map(|(name, _)| name.clone())
                    .collect();
                satisfied.sort_unstable();
                AgentTypeInfo {
                    name: id.as_str(),
                    signature: registered.signature.clone(),
                    contracts: satisfied,
                }
            })
            .collect();
        infos.sort_unstable_by_key(|info| info.name);
        infos
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
            .map(|registered| Arc::clone(&registered.agent))
            .ok_or_else(|| SessionError::AgentNotFound(agent_type.to_string()))?;

        let graph = agent.to_graph();
        init(&mut ctx);

        agent
            .setup(&mut ctx)
            .map_err(|source| SessionError::Setup {
                agent_name: agent.name().to_owned(),
                source,
            })?;

        // A before-first-prompt boundary. Without it the earliest point a
        // rewind could reach would be "after turn 0", and "rewind to before
        // the first prompt" would be inexpressible.
        let checkpoints = self.boundary_checkpoint(&ctx, id, 0);

        let state = Arc::new(SessionState {
            ctx: tokio::sync::Mutex::new(ctx),
            graph,
            executor,
            agent_type: *agent_type,
            agent_version: agent.version().map(str::to_owned),
            agent_description: agent.description().map(str::to_owned),
            turn_number: AtomicU32::new(0),
            cancellation: parking_lot::Mutex::new(None),
            checkpoints: parking_lot::Mutex::new(checkpoints),
            created_at: utc_now_iso8601(),
            read_only: AtomicBool::new(false),
            labels: build_session_labels(id, agent_type),
            turns: parking_lot::Mutex::new(VecDeque::with_capacity(MAX_TURN_HISTORY)),
        });

        match self.inner.sessions.write().entry(id.clone()) {
            Entry::Occupied(_) => return Err(SessionError::SessionAlreadyExists(id.clone())),
            Entry::Vacant(entry) => entry.insert(state),
        };
        self.inner.uptime.record(id, LifecycleKind::Created);
        Ok(())
    }

    /// Executes a single turn for the session.
    ///
    /// [`SessionInfo`] is injected into the context before execution.
    /// Completed and interrupted turns advance the turn number; failed turns do not.
    ///
    /// When auto-checkpoint is enabled, the context state is serialized
    /// after a completed or interrupted turn, while holding the context lock.
    /// Serialization adds to turn latency; a capture failure is logged as a warning.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// [`SessionError::Interrupted`] if execution was interrupted,
    /// [`SessionError::RestoreFailed`] if a previous restore left the context
    /// incoherent, [`SessionError::ReadOnly`] if the session is read-only,
    /// or [`SessionError::Execution`] if the graph execution fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, SessionId};
    /// # async fn example(sessions: &SessionsAPI, id: &SessionId) {
    /// let result = sessions.process_turn(id).await.unwrap();
    /// assert!(result.nodes_executed() > 0);
    /// # }
    /// ```
    pub async fn process_turn(&self, id: &SessionId) -> Result<ExecutionResult, SessionError> {
        self.process_turn_with(id, |_| {}).await
    }

    /// Executes a single turn for the session with a setup closure.
    ///
    /// Before execution, [`SessionInfo`] is injected into the context and
    /// the `setup` closure is called to prepare turn-specific resources.
    /// Completed and interrupted turns advance the turn number; failed turns do not.
    ///
    /// When auto-checkpoint is enabled, the context state is serialized
    /// after a completed or interrupted turn, while holding the context lock.
    /// Serialization adds to turn latency; a capture failure is logged as a warning.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist,
    /// [`SessionError::Interrupted`] if execution was interrupted,
    /// [`SessionError::RestoreFailed`] if a previous restore left the context
    /// incoherent, [`SessionError::ReadOnly`] if the session is read-only,
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
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
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
    /// exist, [`SessionError::Interrupted`] if execution was interrupted,
    /// [`SessionError::RestoreFailed`] if a previous restore left the context
    /// incoherent, [`SessionError::ReadOnly`] if the session is read-only,
    /// or [`SessionError::Execution`] if graph execution fails.
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
    /// exist, [`SessionError::Interrupted`] if execution was interrupted,
    /// [`SessionError::RestoreFailed`] if a previous restore left the context
    /// incoherent, [`SessionError::ReadOnly`] if the session is read-only,
    /// or [`SessionError::Execution`] if graph execution fails.
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
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
        let mut ctx = state
            .ctx
            .try_lock()
            .map_err(|_| SessionError::SessionBusy(id.clone()))?;
        self.execute_turn(id, &state, &mut ctx, setup).await
    }

    /// Raises the cancellation signal for the turn currently executing in
    /// `id`, if any.
    ///
    /// Synchronous and idempotent, with short locking of the session registry
    /// and cancellation slot. It never takes the context mutex held by a
    /// running turn. Call from a normal task, UI callback, or HTTP handler;
    /// forward OS signals to a task first because this method is not
    /// async-signal-safe.
    ///
    /// The whole contract a caller may rely on:
    ///
    /// - `Ok(true)` means **a turn was in flight**, nothing finer. The session
    ///   cannot distinguish a turn that is working from one parked waiting for
    ///   user input; both hold the slot.
    /// - A raise while the session is idle returns `Ok(false)` and is **not**
    ///   latched into the next turn. A stray interrupt must not kill the next
    ///   request.
    /// - An unknown session returns [`SessionError::SessionNotFound`].
    /// - Escalation policy — "a second press quits" — belongs to the host and
    ///   its own timer, never to this method. A second call during the same
    ///   turn also returns `Ok(true)`.
    /// - A turn whose future was **dropped** rather than awaited to completion
    ///   — a client disconnecting mid-request, a `select!` arm losing, a
    ///   timeout firing — counts as not in flight: the slot is closed as the
    ///   future unwinds, so this returns `Ok(false)` and the host is told the
    ///   truth about a session it can start a new turn on.
    ///
    /// How promptly the turn ends is the agent's business: a system that races
    /// [`RunCancellation`](polaris_graph::RunCancellation) returns at once, and
    /// one that ignores it runs to completion while the executor stops at the
    /// next node boundary. Either way the turn ends as
    /// [`SessionError::Interrupted`], the turn record reaches a terminal
    /// status, and the session returns to idle.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionError, SessionsAPI, SessionId};
    /// # async fn example(
    /// #     sessions: &SessionsAPI,
    /// #     id: &SessionId,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// // The turn has to be genuinely in flight for a raise to reach it, so it
    /// // runs on its own task. Binding the future without polling it would
    /// // leave the session idle and every raise below would report `false`.
    /// let turn = tokio::spawn({
    ///     let sessions = sessions.clone();
    ///     let id = id.clone();
    ///     async move { sessions.process_turn(&id).await }
    /// });
    ///
    /// // Tokio forwards the OS signal to this normal async task.
    /// tokio::signal::ctrl_c().await?;
    /// let was_running = sessions.interrupt_turn(id)?;
    ///
    /// match turn.await? {
    ///     Err(SessionError::Interrupted { turn, nodes_executed, .. }) => {
    ///         assert!(was_running, "the raise found the turn it ended");
    ///         // The turn is recorded as `Interrupted`, the session is idle,
    ///         // and the next turn may proceed.
    ///     }
    ///     Ok(result) => {
    ///         // The turn finished before the signal reached a node boundary —
    ///         // `was_running` may be either, and a raise that found nothing is
    ///         // dropped rather than latched into the next turn.
    ///     }
    ///     Err(other) => { /* failed for an unrelated reason */ }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn interrupt_turn(&self, id: &SessionId) -> Result<bool, SessionError> {
        let state = self.get_state(id)?;
        // Clone the source out and release the slot before raising, so the
        // lock is held for exactly one pointer clone.
        let source = state.cancellation.lock().clone();
        match source {
            Some(source) => {
                source.cancel();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Shared turn execution logic used by both blocking and try-lock variants.
    async fn execute_turn(
        &self,
        id: &SessionId,
        state: &SessionState,
        ctx: &mut SystemContext<'static>,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        // A restore that failed partway leaves the context half-applied.
        // Running a turn on it would produce output nobody can reason about,
        // so refuse until a restore succeeds or the caller resumes from the
        // store. Unreachable for a caller that never restores.
        if let Some(refusal) = state.checkpoints.lock().coherence_refusal(id) {
            return Err(refusal);
        }

        let turn = state.turn_number.load(Ordering::Acquire);

        // Inject session metadata.
        ctx.insert(SessionInfo {
            session_id: id.clone(),
            turn_number: turn,
        });

        // A fresh signal per turn, inserted before the caller's setup closure
        // so a closure may still override it deliberately — the same idiom the
        // HTTP handlers use for `UserIO`.
        let cancellation = CancellationSource::new();
        ctx.insert(cancellation.token());

        setup(ctx);

        let hooks = self.inner.hooks.get();
        let middleware = self.inner.middleware.get();

        // Per-turn labels — extends the session-wide labels with the
        // current turn number. Threading `turn` here lets dashboards
        // join a tracing run back to a turn without a separate index,
        // since the same labels surface on both `GraphEvent`s and on
        // `polaris.label.*` tracing fields below.
        let turn_str = turn.to_string();
        let labels = RunLabels::from(
            state
                .labels
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .chain(std::iter::once(("turn".to_owned(), turn_str.clone()))),
        );

        // The `polaris.label.*` fields below piggyback on the tracing
        // span chain so child graph spans inherit the same labels,
        // letting downstream subscribers and OTel exporters correlate
        // recorded spans by `session_id`.
        // The `error.type`/`otel.status_code` placeholders stay `Empty`
        // and are recorded only on the failure path below.
        let turn_span = tracing::info_span!(
            "polaris.session.turn",
            otel.name = %format_args!("invoke_agent {}", state.agent_type),
            otel.kind = "Internal",
            gen_ai.operation.name = "invoke_agent",
            gen_ai.agent.name = %state.agent_type,
            gen_ai.agent.version = tracing::field::Empty,
            gen_ai.agent.description = tracing::field::Empty,
            gen_ai.conversation.id = %id,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            polaris.session.id = %id,
            polaris.session.turn_number = turn,
            polaris.session.agent_type = %state.agent_type,
            polaris.label.session_id = id.as_str(),
            polaris.label.agent_type = state.agent_type.as_str(),
            polaris.label.turn = %turn_str,
        );

        if let Some(version) = &state.agent_version {
            turn_span.record("gen_ai.agent.version", version.as_str());
        }
        if let Some(description) = &state.agent_description {
            turn_span.record("gen_ai.agent.description", description.as_str());
        }

        // Opens the turn's history entry, its `Active` lifecycle mark and its
        // interrupt slot together, and closes all three on every exit path —
        // including this future being dropped inside the `await` below. The
        // slot is open exactly for the duration of the executor call: a raise
        // landing before it opens or after it closes finds nothing and reports
        // `Ok(false)`; the token is latched and per-turn, so every interleaving
        // inside the window is benign.
        let mut turn_guard = TurnGuard::open(self, state, id, turn, cancellation);

        let exec_result = state
            .executor
            .execute_with_labels(&state.graph, ctx, hooks, middleware, labels)
            .instrument(turn_span.clone())
            .await;

        let interrupted = matches!(exec_result, Err(ExecutionError::Interrupted { .. }));

        // Close the slot and finalize the record + lifecycle regardless of
        // success or failure, so the dashboard reflects what actually happened.
        turn_guard.settle(match (&exec_result, interrupted) {
            (Ok(_), _) => TurnStatus::Completed,
            (Err(_), true) => TurnStatus::Interrupted,
            (Err(_), false) => TurnStatus::Failed,
        });

        if let Err(err) = &exec_result {
            // `error.type` is meant to be a low-cardinality discriminant, so
            // record the `ExecutionError` variant name rather than the full
            // `Display` (which embeds node ids and messages). The derived
            // `Debug` renders the variant name first, so take the leading
            // identifier before any payload delimiter (`(`, `{`, or space).
            let error_debug = format!("{err:?}");
            let error_type = error_debug
                .split(['(', '{', ' '])
                .next()
                .unwrap_or("ExecutionError");
            // A user's interrupt is not an error, so the status code stays
            // unset; `error.type` still carries the low-cardinality
            // discriminant, which the `Debug` split above already renders as
            // exactly `"Interrupted"`.
            if !interrupted {
                turn_span.record("otel.status_code", "ERROR");
            }
            turn_span.record("error.type", error_type);
        }

        // The sole conversion point from `ExecutionError` to `SessionError`.
        // `SessionError::Execution` carries `#[from] ExecutionError`, so a
        // bare `?` here would silently build
        // `Execution(Interrupted { .. })` — a second wire shape for one
        // outcome that every consumer would then have to match twice.
        let result = match exec_result {
            Ok(result) => result,
            Err(ExecutionError::Interrupted {
                run_id,
                nodes_executed,
                elapsed,
                ..
            }) => {
                // An interrupted turn *happened*: its user message and every
                // node that completed are in the context, and the run's
                // identity is `(session, turn)`, which two runs must never
                // share. So the number advances and the state — consistent by
                // construction, since every system either completed or
                // returned by its own choice — is a legitimate rollback
                // target. The `Failed` path keeps its own non-advancing,
                // non-checkpointing behaviour, unchanged here.
                state.turn_number.store(turn + 1, Ordering::Release);
                self.auto_checkpoint(state, id, turn, ctx);
                return Err(SessionError::Interrupted {
                    session: id.clone(),
                    turn,
                    run_id,
                    nodes_executed,
                    elapsed,
                });
            }
            Err(err) => return Err(SessionError::Execution(err)),
        };

        state.turn_number.store(turn + 1, Ordering::Release);
        self.auto_checkpoint(state, id, turn, ctx);

        Ok(result)
    }

    /// Captures the post-turn context as a checkpoint when auto-checkpoint is
    /// enabled, warning rather than failing the turn if serialization fails.
    ///
    /// Called while the context lock is still held, for both the completed and
    /// the interrupted turn.
    fn auto_checkpoint(
        &self,
        state: &SessionState,
        id: &SessionId,
        turn: TurnNumber,
        ctx: &SystemContext<'static>,
    ) {
        // TODO @localminimum: look into doing this in a background task
        // to avoid any potential latency impact on the turn result.
        // Get profiling data to see if this is actually a problem
        // worth optimizing.
        if !self.inner.auto_checkpoint.load(Ordering::Relaxed) {
            return;
        }
        // Both call sites run immediately after `turn_number.store(turn + 1)`,
        // so the counter — and therefore `resume_turn` — is already `turn + 1`
        // while the display label stays `turn`. That split is what keeps a
        // restore from re-running an already-used turn number.
        if let Err(err) = self.capture_locked(state, id, ctx, CheckpointOrigin::Auto, Some(turn)) {
            // Warn, never fail the turn: an automatic checkpoint that cannot
            // fit must not take a user's completed turn down with it. The
            // warning names the cause so a missing point is diagnosable
            // rather than inferred.
            tracing::warn!(
                session = %id,
                turn,
                "auto-checkpoint failed: {err}"
            );
        }
    }

    /// Builds a session's initial checkpoint set with a boundary capture.
    fn boundary_checkpoint(
        &self,
        ctx: &SystemContext<'static>,
        id: &SessionId,
        resume_turn: TurnNumber,
    ) -> CheckpointSet {
        let mut checkpoints = CheckpointSet::default();
        self.capture_boundary_into(&mut checkpoints, ctx, id, resume_turn);
        checkpoints
    }

    /// Captures a session-boundary checkpoint directly into `checkpoints`,
    /// before the session state exists to lock.
    ///
    /// Subject to auto-checkpoint being enabled and to the same
    /// warn-don't-fail rule as a post-turn capture: a host that disabled
    /// automatic checkpoints gets no boundary point either, and a boundary
    /// point that cannot be taken never fails session creation or resume.
    fn capture_boundary_into(
        &self,
        checkpoints: &mut CheckpointSet,
        ctx: &SystemContext<'static>,
        id: &SessionId,
        resume_turn: TurnNumber,
    ) {
        if !self.inner.auto_checkpoint.load(Ordering::Relaxed) {
            return;
        }
        let serializers = self.serializers();
        let retention = self.checkpoint_retention();
        let captured = serialize_resources(&serializers, ctx).and_then(|resources| {
            checkpoints.admit_checkpoint(
                id,
                resources,
                CheckpointOrigin::Auto,
                resume_turn,
                None,
                utc_now_iso8601(),
                retention,
            )
        });
        if let Err(err) = captured {
            tracing::warn!(
                session = %id,
                resume_turn,
                "session boundary checkpoint failed: {err}"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Checkpoint / rollback
    // ─────────────────────────────────────────────────────────────────────

    /// Creates a checkpoint of the session's current resource state.
    ///
    /// Returns the turn number at which the checkpoint was taken.
    ///
    /// Superseded by [`SessionCheckpoints::capture`], which returns a stable
    /// [`CheckpointId`](crate::checkpoint::CheckpointId) instead of a turn
    /// label. Two checkpoints can share a label, and
    /// [`rollback`](Self::rollback) then resolves to whichever was taken
    /// first, so a label cannot identify a snapshot. This method keeps its
    /// exact signature, return value and permissive behaviour — including on
    /// a read-only session, which `capture` refuses.
    ///
    /// Retained checkpoints are now bounded per session
    /// ([`CheckpointRetention`]), so
    /// in a long session the oldest evictable point is reclaimed rather than
    /// retained forever. [`SessionCheckpoints::status`] reports which.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`], a persistence error if
    /// serialization fails, [`SessionError::RestoreFailed`] if the context is
    /// incoherent after a failed restore, or
    /// [`SessionError::CheckpointCapacity`] if the retained set is at a bound
    /// and nothing is evictable.
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
        // Deliberately no read-only check: this method accepted a preserved
        // session before the checkpoint surface existed, and keeping that is
        // what makes the legacy trio's behaviour unchanged.
        let meta = self.capture_locked(&state, id, &ctx, CheckpointOrigin::Manual, None)?;
        Ok(meta.turn)
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
        Ok(checkpoints.turn_labels())
    }

    /// Rolls back the session to a previously checkpointed turn.
    ///
    /// Checkpoints newer than the target turn are discarded.
    ///
    /// # Run-identity hazard on the auto-checkpoint path
    ///
    /// **An automatic checkpoint's turn label is the turn that just ran, not
    /// the turn to resume at, so rolling back to one re-uses a run identity.**
    /// After turn *N* completes the counter is already `N + 1` while the
    /// label pushed over the post-turn state is `N`. `rollback(N)` sets the
    /// counter back to `N`, so the next turn runs as `N` again — and
    /// `(session_id, turn_number)` is a run's identity, which two runs must
    /// never share (see the sessions reference). A *manual* checkpoint reads
    /// the already-incremented counter and does not have the problem, so the
    /// two paths label byte-identical state differently.
    ///
    /// This behaviour is left exactly as it was rather than corrected here:
    /// changing what a shipped method does is a separate decision from adding
    /// a correct surface beside it. [`SessionCheckpoints::restore`] is that
    /// surface — it resumes at the target's
    /// [`resume_turn`](crate::checkpoint::CheckpointMeta::resume_turn), which
    /// is stored rather than inferred, so no identity is re-used on either
    /// path. Prefer it for anything that resumes execution after rewinding.
    ///
    /// `restore` also differs in what it keeps: it re-parents onto a new
    /// revision instead of discarding newer checkpoints, so a second rewind
    /// after a rewind is well defined. Points this method discards are
    /// reported as [`CheckpointStatus::Evicted`](crate::checkpoint::CheckpointStatus::Evicted)
    /// by [`SessionCheckpoints::status`].
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
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
        let mut ctx = state.ctx.lock().await;

        // First label match, as before — the set preserves insertion order.
        let (target, payload) = {
            let checkpoints = state.checkpoints.lock();
            let target = checkpoints
                .find_by_label(turn)
                .ok_or(SessionError::TurnNotFound(turn))?;
            let payload = checkpoints.payload(target).unwrap_or_default();
            (target, payload)
        };

        let serializers = self.serializers();
        deserialize_entries_into_context(&serializers, &payload, &mut ctx)?;

        // The label, not the resume turn: this is the documented behaviour and
        // the hazard above.
        state.turn_number.store(turn, Ordering::Release);

        // Discard checkpoints newer than the rollback target.
        let retention = self.checkpoint_retention();
        state
            .checkpoints
            .lock()
            .discard_above_label(turn, target, &utc_now_iso8601(), retention);

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────
    // Persistence (store)
    // ─────────────────────────────────────────────────────────────────────

    /// Serializes the session and persists it to the backing store.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::RestoreFailed`] while a failed restore leaves
    /// the context incoherent; the existing stored session remains untouched.
    /// Otherwise returns a persistence or store error on failure.
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
        if let Some(err) = state.checkpoints.lock().coherence_refusal(id) {
            return Err(err);
        }
        let turn = state.turn_number.load(Ordering::Acquire);

        let serializers = self.serializers();
        let mut data = serialize_context(
            &serializers,
            state.agent_type,
            turn,
            &state.created_at,
            &ctx,
        )?;
        // The retained checkpoint set rides `SessionData` through whatever
        // store the host configured, which is why durability needs no store
        // method and no second store.
        {
            let checkpoints = state.checkpoints.lock();
            data.checkpoints = checkpoints.stored().to_vec();
            data.next_checkpoint = checkpoints.next_checkpoint();
            data.revision = checkpoints.revision();
            data.checkpoint_history = checkpoints.history();
        }

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
                .map(|(k, v)| (*k, Arc::clone(&v.agent)))
                .ok_or_else(|| SessionError::AgentNotFound(data.agent_type.clone()))?
        };

        let graph = agent.to_graph();

        let serializers = Arc::clone(&self.inner.serializers.read());
        deserialize_entries_into_context(&serializers, &data.resources, &mut ctx)?;

        init(&mut ctx);

        agent
            .setup(&mut ctx)
            .map_err(|source| SessionError::Setup {
                agent_name: agent.name().to_owned(),
                source,
            })?;

        // Checkpoints ride `SessionData`, so resuming restores the retained
        // set, its identity counter, its removal history and the session's
        // position — the pre-durability behaviour of dropping all of it was
        // what made checkpoint identity meaningless across a restart.
        //
        // Moved, not cloned: `data` is owned here and nothing below reads
        // either field again, so cloning would duplicate the whole retained
        // payload — up to the 16 MiB byte bound — on every resume.
        let mut checkpoints = CheckpointSet::from_stored_with_retention(
            data.checkpoints,
            data.next_checkpoint,
            data.revision,
            data.checkpoint_history,
            self.checkpoint_retention(),
        );
        self.capture_boundary_into(&mut checkpoints, &ctx, id, data.turn_number);

        let state = Arc::new(SessionState {
            ctx: tokio::sync::Mutex::new(ctx),
            graph,
            executor,
            agent_type,
            agent_version: agent.version().map(str::to_owned),
            agent_description: agent.description().map(str::to_owned),
            turn_number: AtomicU32::new(data.turn_number),
            cancellation: parking_lot::Mutex::new(None),
            checkpoints: parking_lot::Mutex::new(checkpoints),
            created_at: data.created_at.clone(),
            read_only: AtomicBool::new(false),
            labels: build_session_labels(id, &agent_type),
            turns: parking_lot::Mutex::new(VecDeque::with_capacity(MAX_TURN_HISTORY)),
        });

        {
            let mut sessions = self.inner.sessions.write();
            if let Some(existing) = sessions.get(id)
                && existing.read_only.load(Ordering::Acquire)
            {
                return Err(SessionError::ReadOnly(id.clone()));
            }
            sessions.insert(id.clone(), state);
        }
        self.inner.uptime.record(id, LifecycleKind::Created);
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
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
        let agent = self
            .inner
            .agents
            .read()
            .get(&state.agent_type)
            .map(|registered| Arc::clone(&registered.agent))
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
        let removed = self.inner.sessions.write().remove(id).is_some();
        if removed {
            // Record the terminal transition before forgetting the
            // session so any in-flight uptime query sees the final state.
            self.inner.uptime.record(id, LifecycleKind::Terminated);
            self.inner.uptime.forget(id);
        }
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
            status: status_for(&state),
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
                status: status_for(state),
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
        if state.read_only.load(Ordering::Acquire) {
            return Err(SessionError::ReadOnly(id.clone()));
        }
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

    /// Executes a one-shot turn and preserves the finished session as
    /// read-only.
    ///
    /// Identical to [`run_oneshot`](Self::run_oneshot) up to the terminal
    /// turn: a transient session is created, one turn runs, and the typed
    /// output is extracted. Instead of deleting the session afterward,
    /// it is marked read-only and remains in the live map.
    ///
    /// Read-only sessions stay queryable through every read surface —
    /// [`session_info`](Self::session_info),
    /// [`list_live_sessions`](Self::list_live_sessions),
    /// [`turn_history`](Self::turn_history),
    /// [`list_checkpoints`](Self::list_checkpoints),
    /// [`uptime_buckets`](Self::uptime_buckets) — and accept
    /// [`save_session`](Self::save_session) and
    /// [`delete_session`](Self::delete_session). Any method that mutates
    /// session state returns [`SessionError::ReadOnly`].
    ///
    /// On execution failure, the session is deleted (matching
    /// [`run_oneshot`](Self::run_oneshot) cleanup semantics — there is no useful read-only
    /// state to preserve).
    ///
    /// Returns the generated [`SessionId`] alongside the output so the
    /// caller can address the preserved session later.
    ///
    /// # Type Parameters
    ///
    /// `T` — the output type produced by the agent's terminal system.
    ///
    /// # Errors
    ///
    /// Same as [`run_oneshot`](Self::run_oneshot).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_sessions::{SessionsAPI, AgentTypeId};
    /// # #[derive(Clone)] struct MyOutput;
    /// # async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
    /// let agent_type = AgentTypeId::from_name("MyAgent");
    /// let (session_id, output): (_, MyOutput) = sessions
    ///     .run_oneshot_preserved(&agent_type, |_| {})
    ///     .await?;
    /// // Session is still listed as read-only; query history via session_id.
    /// let meta = sessions.session_info(&session_id)?;
    /// # let _ = (output, meta);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run_oneshot_preserved<T: Output + Clone>(
        &self,
        agent_type: &AgentTypeId,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<(SessionId, T), SessionError> {
        let id = SessionId::default();
        let ctx = self.create_context();
        self.create_session_with(ctx, &id, agent_type, setup)?;

        let outcome = async {
            let exec_result = self.process_turn(&id).await?;
            exec_result
                .output::<T>()
                .cloned()
                .ok_or(SessionError::OutputNotFound(std::any::type_name::<T>()))
        }
        .await;

        match outcome {
            Ok(output) => {
                // Flip the session to read-only. Subsequent mutation
                // attempts return SessionError::ReadOnly.
                if let Ok(state) = self.get_state(&id) {
                    state.read_only.store(true, Ordering::Release);
                }
                Ok((id, output))
            }
            Err(err) => {
                // Match run_oneshot cleanup semantics on failure.
                let _ = self.delete_session(&id).await;
                Err(err)
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Dashboard queries (A9)
    // ─────────────────────────────────────────────────────────────────────

    /// Returns recorded turn summaries for a live session.
    ///
    /// Entries are returned oldest first. If `include_messages` is `true`,
    /// each [`TurnSummary`] embeds the retained [`IOMessage`] array recorded
    /// for that turn; otherwise [`TurnSummary::messages`] is omitted. Check
    /// [`TurnSummary::messages_truncated`] before treating it as complete.
    ///
    /// Messages are only captured for turns whose IO was driven through the
    /// HTTP handlers' recording provider. Programmatic callers that drive
    /// `process_turn` themselves will see `io_message_count = 0` unless they
    /// call [`record_turn_messages`].
    ///
    /// [`record_turn_messages`]: SessionsAPI::record_turn_messages
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not exist.
    pub fn turn_history(
        &self,
        id: &SessionId,
        include_messages: bool,
    ) -> Result<Vec<TurnSummary>, SessionError> {
        let state = self.get_state(id)?;
        let turns = state.turns.lock();
        Ok(turns
            .iter()
            .map(|entry| entry.to_summary(include_messages))
            .collect())
    }

    /// Returns the retained payload for a single recorded turn.
    ///
    /// If multiple records exist for the same turn number (e.g. a failed
    /// turn followed by a successful retry), the most recent record wins
    /// — matching what [`turn_history`](Self::turn_history) lists last.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist, or [`SessionError::TurnNotFound`] if no record exists for
    /// the requested turn number.
    pub fn turn(&self, id: &SessionId, turn: TurnNumber) -> Result<Turn, SessionError> {
        let state = self.get_state(id)?;
        let turns = state.turns.lock();
        turns
            .iter()
            .rev()
            .find(|entry| entry.turn == turn)
            .map(TurnHistoryEntry::to_turn)
            .ok_or(SessionError::TurnNotFound(turn))
    }

    /// Attaches IO messages observed during a turn to the turn history.
    ///
    /// Available to integrations that collect turn output outside the built-in
    /// HTTP recording provider. No-op if the session no longer exists or the
    /// turn record has aged out of the per-session ring. Retained history is
    /// capped by message count and payload bytes;
    /// [`TurnSummary::messages_truncated`] reports when the supplied stream
    /// exceeded either limit.
    pub fn record_turn_messages(&self, id: &SessionId, turn: TurnNumber, messages: Vec<IOMessage>) {
        let Ok(state) = self.get_state(id) else {
            return;
        };
        let mut turns = state.turns.lock();
        // Most recent first — the entry we want is typically near the back.
        if let Some(entry) = turns.iter_mut().rev().find(|entry| entry.turn == turn) {
            entry.messages.clear();
            entry.io_message_count = 0;
            entry.recorded_bytes = 0;
            entry.messages_truncated = false;
            entry.last_message_preview = None;
            for message in &messages {
                entry.append_message(message);
            }
        }
    }

    /// Records one outbound message without retaining an unbounded capture
    /// buffer in the HTTP streaming path.
    #[cfg(feature = "sessions-http")]
    pub(crate) fn record_turn_message(
        &self,
        id: &SessionId,
        turn: TurnNumber,
        message: &IOMessage,
    ) {
        let Ok(state) = self.get_state(id) else {
            return;
        };
        let mut turns = state.turns.lock();
        if let Some(entry) = turns.iter_mut().rev().find(|entry| entry.turn == turn) {
            entry.append_message(message);
        }
    }

    /// Returns the bucketed uptime time-series for a session.
    ///
    /// `since`/`until` are inclusive/exclusive; the response always
    /// contains `ceil((until − since) / bucket)` buckets. Returns an
    /// empty buckets vector when `until <= since`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::SessionNotFound`] if the session does not
    /// exist. (Lifecycle history is dropped on
    /// [`delete_session`](Self::delete_session), so terminated sessions
    /// are not queryable.)
    pub fn uptime_buckets(
        &self,
        id: &SessionId,
        bucket: BucketGranularity,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<SessionUptimeResponse, SessionError> {
        // Validate the session exists so the endpoint can 404 properly.
        let _ = self.get_state(id)?;
        Ok(self.inner.uptime.buckets_for(id, bucket, since, until))
    }

    fn start_turn_record(&self, state: &SessionState, turn: TurnNumber) {
        let entry = TurnHistoryEntry {
            turn,
            started_at: utc_now_iso8601(),
            finished_at: None,
            status: TurnStatus::Running,
            messages: Vec::new(),
            io_message_count: 0,
            recorded_bytes: 0,
            messages_truncated: false,
            last_message_preview: None,
        };
        let mut turns = state.turns.lock();
        if turns.len() >= MAX_TURN_HISTORY {
            turns.pop_front();
        }
        turns.push_back(entry);
    }

    fn finish_turn_record(&self, state: &SessionState, turn: TurnNumber, status: TurnStatus) {
        let mut turns = state.turns.lock();
        if let Some(entry) = turns.iter_mut().rev().find(|entry| entry.turn == turn) {
            entry.finished_at = Some(utc_now_iso8601());
            entry.status = status;
        }
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

impl TurnHistoryEntry {
    fn to_summary(&self, include_messages: bool) -> TurnSummary {
        TurnSummary {
            turn: self.turn,
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.clone(),
            status: self.status,
            io_message_count: self.io_message_count,
            messages_truncated: self.messages_truncated,
            last_message_preview: self.last_message_preview.clone(),
            messages: include_messages.then(|| self.messages.clone()),
        }
    }

    fn to_turn(&self) -> Turn {
        Turn {
            turn: self.turn,
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.clone(),
            status: self.status,
            messages_truncated: self.messages_truncated,
            messages: self.messages.clone(),
        }
    }

    fn append_message(&mut self, message: &IOMessage) {
        self.io_message_count = self.io_message_count.saturating_add(1);
        self.last_message_preview = preview_from_message(message);

        let remaining = MAX_RECORDED_BYTES_PER_TURN.saturating_sub(self.recorded_bytes);
        if self.messages.len() >= MAX_RECORDED_MESSAGES_PER_TURN || remaining == 0 {
            self.messages_truncated = true;
            return;
        }

        let limit = remaining.min(MAX_RECORDED_BYTES_PER_MESSAGE);
        let (recorded, retained_bytes, truncated) = bounded_message_copy(message, limit);
        self.recorded_bytes = self.recorded_bytes.saturating_add(retained_bytes);
        self.messages_truncated |= truncated;
        self.messages.push(recorded);
    }
}

fn bounded_message_copy(message: &IOMessage, limit: usize) -> (IOMessage, usize, bool) {
    let mut remaining = limit;
    let mut truncated = false;

    let source = match &message.source {
        IOSource::User => IOSource::User,
        IOSource::System => IOSource::System,
        IOSource::Agent(name) => {
            let (name, changed) = bounded_string(name, &mut remaining);
            truncated |= changed;
            IOSource::Agent(name)
        }
        IOSource::External(name) => {
            let (name, changed) = bounded_string(name, &mut remaining);
            truncated |= changed;
            IOSource::External(name)
        }
    };

    let content = match &message.content {
        IOContent::Text(text) => {
            let (text, changed) = bounded_string(text, &mut remaining);
            truncated |= changed;
            IOContent::Text(text)
        }
        IOContent::Binary { mime_type, data } => {
            let (mime_type, mime_changed) = bounded_string(mime_type, &mut remaining);
            let retained = data.len().min(remaining);
            let data_changed = retained < data.len();
            let data = data[..retained].to_vec();
            remaining -= retained;
            truncated |= mime_changed || data_changed;
            IOContent::Binary { mime_type, data }
        }
        IOContent::Structured(value) => {
            let mut counter = LimitedWriter::new(remaining);
            if serde_json::to_writer(&mut counter, value).is_ok() {
                remaining = remaining.saturating_sub(counter.written);
                IOContent::Structured(value.clone())
            } else {
                truncated = true;
                IOContent::Structured(serde_json::Value::Null)
            }
        }
    };

    let mut recorded = IOMessage::new(content, source);
    for (index, (key, value)) in message.metadata.iter().enumerate() {
        if index >= MAX_RECORDED_METADATA_ENTRIES || remaining == 0 {
            truncated = true;
            break;
        }
        let (key, key_changed) = bounded_string(key, &mut remaining);
        let (value, value_changed) = bounded_string(value, &mut remaining);
        truncated |= key_changed || value_changed;
        recorded.metadata.insert(key, value);
    }

    (recorded, limit - remaining, truncated)
}

fn bounded_string(value: &str, remaining: &mut usize) -> (String, bool) {
    let retained = value.len().min(*remaining);
    let mut end = retained;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    *remaining -= end;
    (value[..end].to_owned(), end < value.len())
}

struct LimitedWriter {
    written: usize,
    limit: usize,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self { written: 0, limit }
    }
}

impl std::io::Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::other(
                "structured message exceeds history limit",
            ));
        }
        self.written += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Renders a short, single-line preview of the last text payload in a
/// turn's message stream. Non-text content yields `None` so the
/// dashboard renders a placeholder instead of a misleading excerpt.
fn preview_from_message(msg: &IOMessage) -> Option<String> {
    let IOContent::Text(text) = &msg.content else {
        return None;
    };
    let trimmed: String = text.split('\n').next().unwrap_or("").trim().to_owned();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= PREVIEW_MAX_CHARS {
        return Some(trimmed);
    }
    // Truncate on a char boundary so we never split a multi-byte codepoint.
    let truncated: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    Some(format!("{truncated}…"))
}

/// Returns the current UTC time formatted as ISO 8601 (e.g. `2026-03-31T12:00:00Z`).
fn utc_now_iso8601() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Serialization helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Serializes all registered resources from a context.
///
/// The payload shape shared by a persisted session and a checkpoint: a
/// checkpoint holds exactly this, rather than a nested [`SessionData`] that
/// would repeat the session-constant fields in every snapshot.
pub(crate) fn serialize_resources(
    serializers: &[Arc<dyn ResourceSerializer>],
    ctx: &SystemContext<'_>,
) -> Result<Vec<ResourceEntry>, SessionError> {
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
    Ok(resources)
}

/// Serializes all registered resources from a context into [`SessionData`].
///
/// Leaves the checkpoint fields at their defaults; `save_session` fills them
/// from the session's live set.
fn serialize_context(
    serializers: &[Arc<dyn ResourceSerializer>],
    agent_type: AgentTypeId,
    turn_number: TurnNumber,
    created_at: &str,
    ctx: &SystemContext<'_>,
) -> Result<SessionData, SessionError> {
    let resources = serialize_resources(serializers, ctx)?;
    Ok(SessionData {
        agent_type: agent_type.as_str().to_owned(),
        turn_number,
        created_at: created_at.to_owned(),
        resources,
        checkpoints: Vec::new(),
        next_checkpoint: 0,
        revision: RevisionId::default(),
        checkpoint_history: CheckpointHistory::default(),
    })
}

/// What a restore actually loaded, and what it could not.
pub(crate) struct RestoreReport {
    /// Entries loaded into the context.
    pub(crate) restored: Vec<ResourceRef>,
    /// Entries no registered serializer matched, and which were skipped.
    pub(crate) unmatched: Vec<ResourceRef>,
}

/// Deserializes serialized resource entries back into a context by matching
/// each entry to a serializer via `(plugin_id, storage_key, schema_version)`.
///
/// Entries that match nothing are skipped, as they always have been — but they
/// are now *reported* rather than silently dropped, which is what lets a
/// caller tell a partial restore from a complete one.
pub(crate) fn deserialize_entries_into_context(
    serializers: &[Arc<dyn ResourceSerializer>],
    entries: &[ResourceEntry],
    ctx: &mut SystemContext<'static>,
) -> Result<RestoreReport, SessionError> {
    let mut report = RestoreReport {
        restored: Vec::new(),
        unmatched: Vec::new(),
    };
    for entry in entries {
        if let Some(ser) = matching_serializer(serializers, entry) {
            ser.load(entry.data.clone(), ctx)?;
            report.restored.push(entry_ref(entry));
        } else {
            report.unmatched.push(entry_ref(entry));
        }
    }
    Ok(report)
}

/// The entries in `payload` that no registered serializer can load.
///
/// Computed without touching the context, so a caller can refuse a partial
/// restore before anything is written.
pub(crate) fn unmatched_refs(
    serializers: &[Arc<dyn ResourceSerializer>],
    payload: &[ResourceEntry],
) -> Vec<ResourceRef> {
    payload
        .iter()
        .filter(|entry| matching_serializer(serializers, entry).is_none())
        .map(entry_ref)
        .collect()
}

fn matching_serializer<'a>(
    serializers: &'a [Arc<dyn ResourceSerializer>],
    entry: &ResourceEntry,
) -> Option<&'a Arc<dyn ResourceSerializer>> {
    serializers.iter().find(|ser| {
        ser.plugin_id() == entry.plugin_id
            && ser.storage_key() == entry.storage_key
            && ser.schema_version() == entry.version
    })
}

fn entry_ref(entry: &ResourceEntry) -> ResourceRef {
    ResourceRef {
        plugin_id: entry.plugin_id.clone(),
        storage_key: entry.storage_key.clone(),
        version: entry.version.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionsPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Plugin that provides session management via [`SessionsAPI`].
///
/// Use this whenever an application needs multi-turn agent execution with
/// persistence, checkpoints, or HTTP-driven turn handling. `SessionsAPI` is
/// the entry point for [`run_oneshot`](SessionsAPI::run_oneshot),
/// [`process_turn`](SessionsAPI::process_turn), checkpoint management, and
/// scoped sessions.
///
/// # Auto-Checkpoint
///
/// Enabled by default. A checkpoint is captured at session creation, at
/// [`resume_session`](SessionsAPI::resume_session), and on the return path of
/// every completed and every interrupted
/// [`process_turn`](SessionsAPI::process_turn). Capture is synchronous — it
/// runs while the turn still holds the session's context lock, so its
/// serialization cost lands on turn latency — and a capture that fails is
/// logged at `warn` rather than failing the turn.
///
/// Retained checkpoints are durable: they ride [`SessionData`] through the
/// configured [`SessionStore`], written by
/// [`save_session`](SessionsAPI::save_session) and reloaded by
/// [`resume_session`](SessionsAPI::resume_session).
///
/// Two knobs:
///
/// - [`with_checkpoint_retention`](Self::with_checkpoint_retention) bounds the
///   retained set. The default is 64 checkpoints / 16 MiB of serialized
///   payload; [`CheckpointRetention::unbounded`] opts out of both.
/// - [`without_auto_checkpoint`](Self::without_auto_checkpoint) disables the
///   mechanism entirely — turn captures and boundary captures alike.
///
/// # Lifecycle
///
/// - **`build()`** — constructs a [`SessionsAPI`] backed by the configured
///   [`SessionStore`] and inserts it as a build-time API.
/// - **`ready()`** — attaches required [`PersistenceAPI`] serializers and the
///   [`ContextFactory`]. It also captures [`HooksAPI`] and [`MiddlewareAPI`]
///   when present; missing optional graph instrumentation is logged by
///   `Server::expect_api` and execution continues without it.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Session state lives behind the [`SessionsAPI`] handle rather than as a context-scoped resource. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`SessionsAPI`] | Session lifecycle entry point — register agents, create sessions, run one-shot or multi-turn execution, manage checkpoints, persist or resume sessions, and register named capability contracts matched against cached agent signatures. |
///
/// # Dependencies
///
/// - [`PersistencePlugin`] — owns the [`PersistenceAPI`]
///   used during `ready()` to gather resource serializers for checkpointing.
///
/// [`HooksAPI`] and [`MiddlewareAPI`] are optional integrations discovered
/// during `ready()`; they are not plugin dependencies.
///
/// # Example
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
    retention: CheckpointRetention,
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
            retention: CheckpointRetention::default(),
        }
    }

    /// Disables automatic checkpoints at creation, resume, and after completed
    /// or interrupted turns.
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

    /// Sets how many checkpoints, and how many serialized bytes of them, one
    /// session retains.
    ///
    /// Retention is injected policy rather than a constant at a call site: the
    /// host decides, and a consumer reads the decision back with
    /// [`SessionCheckpoints::retention`] instead of restating a number that
    /// would then disagree with the framework.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use polaris_sessions::SessionsPlugin;
    /// use polaris_sessions::checkpoint::CheckpointRetention;
    /// use polaris_sessions::store::memory::InMemoryStore;
    ///
    /// let plugin = SessionsPlugin::new(Arc::new(InMemoryStore::new()))
    ///     .with_checkpoint_retention(CheckpointRetention::new(Some(8), Some(1 << 20)));
    /// ```
    #[must_use]
    pub fn with_checkpoint_retention(mut self, retention: CheckpointRetention) -> Self {
        self.retention = retention;
        self
    }
}

impl Plugin for SessionsPlugin {
    const ID: &'static str = "polaris::sessions";
    const VERSION: Version = Version::new(0, 0, 1);

    fn access(&self) -> PluginAccess {
        // Declares the `SessionsAPI` capability so consumers (e.g. the session
        // `HttpPlugin`) can depend on the API type rather than naming `SessionsPlugin`.
        // The API is inserted imperatively in `build()` below. The `PersistenceAPI` this
        // plugin reads in `ready()` remains expressed via `dependencies()`.
        PluginAccess::new().provides::<SessionsAPI>(SessionsAPI::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        let api = SessionsAPI::new(Arc::clone(&self.store));
        api.set_auto_checkpoint(self.auto_checkpoint);
        api.set_checkpoint_retention(self.retention);
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
        sessions
            .set_graph_apis(
                server.expect_api::<HooksAPI>("graph hook fan-out"),
                server.expect_api::<MiddlewareAPI>("graph span instrumentation"),
            )
            .expect("SessionsPlugin::ready called twice");
        sessions
            .set_context_factory(server.context_factory())
            .expect("SessionsPlugin::ready called twice");
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PersistencePlugin>()]
    }
}
