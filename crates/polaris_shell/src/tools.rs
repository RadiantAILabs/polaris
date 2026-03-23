//! LLM-facing tool wrappers for shell command execution.
//!
//! Provides [`ShellTools`], a [`Toolset`](polaris_tools::Toolset) that exposes
//! [`ShellExecutor`](crate::ShellExecutor) functionality as tools for LLM agents.

use crate::error::ShellError;
use crate::executor::{ExecutionResult, ShellExecutor, ShellRequest, ShellResponse};
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
    ) -> Result<ShellToolResponse, ToolError> {
        let mut request = ShellRequest::new(&command);

        if let Some(dir) = &working_dir {
            request = request.with_working_dir(PathBuf::from(dir));
        }

        if let Some(secs) = timeout_secs {
            request = request.with_timeout(secs);
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
