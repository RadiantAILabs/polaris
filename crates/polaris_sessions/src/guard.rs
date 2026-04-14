//! RAII guard for automatic session cleanup.

use crate::api::SessionsAPI;
use crate::error::SessionError;
use crate::store::SessionId;
use polaris_graph::ExecutionResult;
use polaris_system::param::SystemContext;

/// RAII guard that deletes its session on drop.
///
/// Use [`SessionsAPI::scoped_session`] to create a guard. When the guard
/// is dropped, the session is deleted asynchronously via [`tokio::spawn`].
///
/// # Example
///
/// ```no_run
/// # use polaris_sessions::{SessionsAPI, AgentTypeId};
/// # use polaris_system::resource::LocalResource;
/// # #[derive(Clone)] struct MyInput(String);
/// # impl MyInput { fn new(s: &str) -> Self { Self(s.into()) } }
/// # impl LocalResource for MyInput {}
/// # async fn example(sessions: &SessionsAPI) -> Result<(), Box<dyn std::error::Error>> {
/// let agent_type = AgentTypeId::from_name("MyAgent");
/// let guard = sessions.scoped_session(&agent_type, |ctx| {
///     ctx.insert(MyInput::new("initial input"));
/// })?;
///
/// // Run multiple turns
/// guard.process_turn().await?;
/// guard.process_turn_with(|ctx| {
///     ctx.insert(MyInput::new("second input"));
/// }).await?;
///
/// // Session is automatically deleted when `guard` is dropped.
/// # Ok(())
/// # }
/// ```
pub struct SessionGuard {
    sessions: SessionsAPI,
    id: SessionId,
}

impl std::fmt::Debug for SessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGuard")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SessionGuard {
    /// Creates a new guard. Called by [`SessionsAPI::scoped_session`].
    pub(crate) fn new(sessions: SessionsAPI, id: SessionId) -> Self {
        Self { sessions, id }
    }

    /// Returns the session ID managed by this guard.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Executes a single turn for this session.
    ///
    /// See [`SessionsAPI::process_turn`] for details.
    pub async fn process_turn(&self) -> Result<ExecutionResult, SessionError> {
        self.sessions.process_turn(&self.id).await
    }

    /// Executes a single turn with a setup closure.
    ///
    /// See [`SessionsAPI::process_turn_with`] for details.
    pub async fn process_turn_with(
        &self,
        setup: impl FnOnce(&mut SystemContext<'static>),
    ) -> Result<ExecutionResult, SessionError> {
        self.sessions.process_turn_with(&self.id, setup).await
    }

    /// Provides mutable access to the session's context.
    ///
    /// See [`SessionsAPI::with_context`] for details.
    pub async fn with_context<R>(
        &self,
        f: impl FnOnce(&mut SystemContext<'static>) -> R,
    ) -> Result<R, SessionError> {
        self.sessions.with_context(&self.id, f).await
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let sessions = self.sessions.clone();
        let id = self.id.clone();
        handle.spawn(async move {
            if let Err(err) = sessions.delete_session(&id).await {
                tracing::warn!(session = %id, "session guard cleanup failed: {err}");
            }
        });
    }
}
