//! RAII guard for automatic session cleanup.

use crate::api::SessionsAPI;
use crate::error::SessionError;
use crate::store::SessionId;
use polaris_graph::ExecutionResult;
use polaris_system::param::SystemContext;

/// RAII guard that deletes its session on drop.
///
/// Use [`SessionsAPI::scoped_session`] or
/// [`SessionsAPI::scoped_session_with_executor`] to create a guard. When the
/// guard is dropped, the session is removed from the live set at once, and its
/// stored copy is deleted asynchronously via [`tokio::spawn`]. Removal from the
/// live set happens even when the guard is dropped mid-turn — for example when
/// the future holding it is cancelled — or outside a tokio runtime. Outside a
/// runtime the stored copy is not deleted.
///
/// Call [`delete`](Self::delete) instead of dropping when the caller needs to
/// wait for the stored copy to be deleted and see any store error.
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
    /// `false` once the session has been deleted or handed back to the
    /// caller; the `Drop` impl then does nothing.
    armed: bool,
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
        Self {
            sessions,
            id,
            armed: true,
        }
    }

    /// Releases the session without deleting it and returns its ID.
    ///
    /// Used by [`SessionsAPI::run_oneshot_preserved`] to keep a finished
    /// session alive.
    pub(crate) fn keep(mut self) -> SessionId {
        self.armed = false;
        self.id.clone()
    }

    /// Deletes the session now and waits for the store deletion to finish.
    ///
    /// Equivalent to [`SessionsAPI::delete_session`] on this guard's ID. The
    /// guard is consumed, so the `Drop` cleanup does not run a second time.
    ///
    /// # Errors
    ///
    /// Returns a store error if deleting the stored copy fails. The session
    /// is removed from the live set either way.
    pub async fn delete(mut self) -> Result<(), SessionError> {
        let result = self.sessions.delete_session(&self.id).await;
        // Disarm only once the deletion finished, so a cancelled `delete`
        // still falls back to the `Drop` cleanup.
        self.armed = false;
        result
    }

    /// Returns the session ID managed by this guard.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Executes a single turn for this session.
    ///
    /// See [`SessionsAPI::process_turn`] for details.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Interrupted`] on interruption or
    /// [`SessionError::RestoreFailed`] while a failed restore leaves the
    /// context incoherent. Other errors match the corresponding API method.
    pub async fn process_turn(&self) -> Result<ExecutionResult, SessionError> {
        self.sessions.process_turn(&self.id).await
    }

    /// Executes a single turn with a setup closure.
    ///
    /// See [`SessionsAPI::process_turn_with`] for details.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Interrupted`] on interruption or
    /// [`SessionError::RestoreFailed`] while a failed restore leaves the
    /// context incoherent. Other errors match the corresponding API method.
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
        if !self.armed {
            return;
        }
        // Remove the live entry synchronously, so the session is gone as
        // soon as the guard is, even if no runtime is available to run the
        // store deletion below.
        self.sessions.remove_live(&self.id);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(session = %self.id, "no tokio runtime; stored copy not deleted");
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
