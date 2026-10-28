//! LLM-facing tool wrappers for shell command execution.
//!
//! Provides [`ShellTools`], a [`Toolset`](polaris_tools::Toolset) that exposes
//! [`ShellExecutor`] functionality as tools for LLM agents.
//!
//! # Observing an invocation
//!
//! ```no_run
//! use polaris_shell::{ChannelSink, ShellCancel, ShellEvent, ShellSinkHandle};
//! use polaris_tools::{ToolContext, ToolError, ToolRegistry};
//! use serde_json::{Value, json};
//! use std::num::NonZeroUsize;
//!
//! async fn run_observed(
//!     registry: &ToolRegistry,
//!     stop_receiver: tokio::sync::watch::Receiver<bool>,
//!     mut on_event: impl FnMut(ShellEvent),
//! ) -> Result<Value, ToolError> {
//!     let (sink, mut events) = ChannelSink::new(NonZeroUsize::new(32).unwrap());
//!     let context = ToolContext::new()
//!         .with(ShellSinkHandle::new(sink))
//!         .with(ShellCancel::from_watch(stop_receiver));
//!     let arguments = json!({"command": "cargo test"});
//!     let execution = registry.execute_with("run_command", &arguments, &context);
//!     tokio::pin!(execution);
//!     let mut events_open = true;
//!     loop {
//!         tokio::select! {
//!             result = &mut execution => {
//!                 let response = result?; // Propagate permission and execution errors.
//!                 if response["status"] == "confirmation_required" {
//!                     // Return the prompt to the host for approval; no run started.
//!                     return Ok(response);
//!                 }
//!                 // Completion published Finished; drain any remaining events.
//!                 while let Some(event) = events.recv().await {
//!                     on_event(event);
//!                 }
//!                 return Ok(response);
//!             }
//!             event = events.recv(), if events_open => {
//!                 if let Some(event) = event {
//!                     on_event(event);
//!                 } else {
//!                     events_open = false;
//!                 }
//!             }
//!         }
//!     }
//! }
//! ```

use crate::error::ShellError;
use crate::executor::{ExecutionResult, ShellExecutor, ShellRequest, ShellResponse};
use crate::process::{ShellCancel, ShellSinkHandle};
use polaris_tools::ToolError;
use polaris_tools::toolset;
use serde::Serialize;
use std::path::PathBuf;

/// Response from the `run_command` tool.
///
/// Uses a tagged enum so the agent can pattern-match on `"status"` to
/// distinguish between a completed execution and a confirmation prompt.
///
/// ```json
/// {"status": "executed", "exit_code": 0, "stdout": "hello\n", ...}
/// {"status": "confirmation_required", "command": "rm -rf /", ...}
/// ```
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum ShellToolResponse {
    /// The command executed (may have timed out or exited non-zero).
    #[serde(rename = "executed")]
    Executed(ExecutionResult),

    /// The command requires user confirmation before execution.
    #[serde(rename = "confirmation_required")]
    ConfirmationRequired {
        /// The command that needs approval.
        command: String,
        /// Working directory, if specified.
        working_dir: Option<PathBuf>,
        /// Timeout in seconds, if specified.
        timeout_secs: Option<u64>,
    },
}

/// LLM-callable shell tools backed by a [`ShellExecutor`].
#[derive(Debug)]
pub struct ShellTools {
    executor: ShellExecutor,
}

impl ShellTools {
    /// Creates a new `ShellTools` wrapping the given executor.
    #[must_use]
    pub fn new(executor: ShellExecutor) -> Self {
        Self { executor }
    }
}

#[toolset]
impl ShellTools {
    /// Run a shell command and return its output.
    ///
    /// The command is interpreted by the system shell (`sh -c`), supporting
    /// pipes, redirects, and globbing.
    ///
    /// `cancel` and `sink` are `#[context]` parameters: they are supplied by
    /// the host through [`ToolContext`](polaris_tools::ToolContext) and
    /// [`ToolRegistry::execute_with`](polaris_tools::ToolRegistry::execute_with),
    /// and never appear in the LLM-facing JSON schema. A host driving a
    /// cancellable run should build a [`ShellCancel`] from its own
    /// cancellation signal and put it in the context to stop the child while
    /// keeping the invocation future alive. Dropping that future also requests
    /// a stop through the foreground process handle.
    #[tool]
    async fn run_command(
        &self,
        /// The shell command to execute.
        command: String,
        /// Working directory for the command. Defaults to the configured default.
        #[default(None::<String>)]
        working_dir: Option<String>,
        /// Timeout in seconds. Defaults to the configured default (30s).
        #[default(None::<u64>)]
        timeout_secs: Option<u64>,
        /// Optional externally owned cancellation observer.
        #[context]
        cancel: Option<ShellCancel>,
        /// Optional destination for incremental process events.
        #[context]
        sink: Option<ShellSinkHandle>,
    ) -> Result<ShellToolResponse, ToolError> {
        let mut request = ShellRequest::new(&command);

        if let Some(dir) = &working_dir {
            request = request.with_working_dir(PathBuf::from(dir));
        }

        if let Some(secs) = timeout_secs {
            request = request.with_timeout(secs);
        }

        if let Some(cancel) = cancel {
            request = request.with_cancel(cancel);
        }

        if let Some(sink) = sink {
            request = request.with_sink(sink);
        }

        let response =
            self.executor
                .execute(request)
                .await
                .map_err(|shell_err| match shell_err {
                    ShellError::PermissionDenied(cmd) => {
                        ToolError::permission_denied(format!("Command denied by policy: {cmd}"))
                    }
                    other => ToolError::execution_error(other.to_string()),
                })?;

        match response {
            ShellResponse::Executed(result) => Ok(ShellToolResponse::Executed(result)),
            ShellResponse::ConfirmationRequired(req) => {
                Ok(ShellToolResponse::ConfirmationRequired {
                    command: req.command,
                    working_dir: req.working_dir,
                    timeout_secs: req.timeout_secs,
                })
            }
        }
    }
}
