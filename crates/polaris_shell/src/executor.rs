//! Shell command execution engine.
//!
//! Provides [`ShellExecutor`] for running shell commands with permission checking,
//! directory sandboxing, timeout enforcement, and output truncation.

use crate::error::ShellError;
use crate::permission::ShellPermission;
use polaris_system::resource::GlobalResource;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default maximum output bytes (1 MB).
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;

/// Default maximum command string length in bytes (100 KB).
///
/// Prevents excessive CPU time in permission evaluation (wildcard matching)
/// on adversarially long inputs. Generous enough for inline scripts and
/// heredoc-style commands.
const DEFAULT_MAX_COMMAND_LENGTH: usize = 102_400;

/// Default overflow file TTL in seconds (1 hour).
const DEFAULT_OVERFLOW_TTL_SECS: u64 = 3600;

/// Returns `~/.polaris`, or `None` if the home directory cannot be determined.
fn default_cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".polaris"))
}

/// Configuration for shell command execution.
///
/// Controls defaults, permission rules, and directory sandboxing.
///
/// # Example
///
/// ```
/// use polaris_shell::ShellConfig;
///
/// let config = ShellConfig::new()
///     .with_working_dir("/home/user/project")
///     .with_timeout(60)
///     .with_allowed_commands(vec!["cargo *".into(), "git *".into()])
///     .with_denied_commands(vec!["rm -rf *".into(), "sudo *".into()])
///     .with_allowed_dirs(vec!["/home/user/project".into()]);
/// ```
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Default working directory for commands.
    /// Falls back to the current process working directory if `None`.
    pub default_working_dir: Option<PathBuf>,

    /// Default timeout for commands in seconds.
    pub default_timeout_secs: u64,

    /// Maximum bytes to capture from stdout/stderr before truncation.
    pub max_output_bytes: usize,

    /// Glob patterns for commands that are auto-allowed (no confirmation needed).
    pub allowed_commands: Vec<String>,

    /// Glob patterns for commands that are always denied.
    /// Evaluated before `allowed_commands` — deny always takes precedence.
    pub denied_commands: Vec<String>,

    /// Whether directory sandboxing is enabled (default: `true`).
    ///
    /// When `true`, only directories listed in `allowed_dirs` (and their
    /// children) are permitted as working directories. An empty `allowed_dirs`
    /// with sandboxing enabled means **no** directory is allowed — the caller
    /// must explicitly grant access before any command can run.
    ///
    /// When `false`, `allowed_dirs` is ignored and any directory is permitted.
    /// This is an explicit opt-out of the security sandbox.
    pub sandbox: bool,

    /// Directories the shell is allowed to operate in when `sandbox` is `true`.
    ///
    /// Each entry permits the directory itself and all of its descendants.
    /// Ignored when `sandbox` is `false`.
    pub allowed_dirs: Vec<PathBuf>,

    /// Maximum command string length in bytes (default: 100 KB).
    ///
    /// Commands exceeding this limit are rejected before permission evaluation,
    /// preventing excessive CPU time in wildcard matching on adversarial inputs.
    pub max_command_length: usize,

    /// Directory for storing overflow output files.
    ///
    /// When stdout or stderr exceeds `max_output_bytes`, the full content is
    /// written to a file in this directory so the agent can read it later.
    /// Defaults to `~/.polaris` (resolved via [`dirs::home_dir`] at runtime).
    /// Set to `None` to disable overflow file creation.
    pub cache_dir: Option<PathBuf>,

    /// Maximum age (in seconds) of overflow files before cleanup removes them.
    ///
    /// [`ShellPlugin::cleanup`](crate::ShellPlugin) removes overflow files
    /// older than this threshold on server shutdown. Defaults to 3600 (1 hour).
    pub overflow_ttl_secs: u64,

    /// Whether to panic on invalid `allowed_dirs` entries at construction time.
    ///
    /// Only relevant when `sandbox` is `true`.
    ///
    /// - `true` — panics if any entry cannot be canonicalized (catches config
    ///   errors at startup).
    /// - `false` (default) — logs a warning and skips invalid entries.
    pub strict_dir_validation: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_working_dir: None,
            default_timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            allowed_commands: Vec::new(),
            denied_commands: Vec::new(),
            sandbox: true,
            allowed_dirs: Vec::new(),
            max_command_length: DEFAULT_MAX_COMMAND_LENGTH,
            cache_dir: default_cache_dir(),
            overflow_ttl_secs: DEFAULT_OVERFLOW_TTL_SECS,
            strict_dir_validation: false,
        }
    }
}

impl ShellConfig {
    /// Creates a new default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the default working directory.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.default_working_dir = Some(dir.into());
        self
    }

    /// Sets the default timeout in seconds.
    #[must_use]
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.default_timeout_secs = secs;
        self
    }

    /// Sets the maximum output bytes before truncation.
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Sets the allowed command patterns.
    #[must_use]
    pub fn with_allowed_commands(mut self, patterns: Vec<String>) -> Self {
        self.allowed_commands = patterns;
        self
    }

    /// Sets the denied command patterns.
    #[must_use]
    pub fn with_denied_commands(mut self, patterns: Vec<String>) -> Self {
        self.denied_commands = patterns;
        self
    }

    /// Sets the allowed directories for sandboxing.
    ///
    /// Only effective when sandboxing is enabled (the default).
    #[must_use]
    pub fn with_allowed_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.allowed_dirs = dirs;
        self
    }

    /// Sets the maximum command string length in bytes.
    ///
    /// Commands exceeding this limit are rejected before permission evaluation.
    /// Defaults to 100 KB.
    #[must_use]
    pub fn with_max_command_length(mut self, bytes: usize) -> Self {
        self.max_command_length = bytes;
        self
    }

    /// Sets the directory for overflow output files.
    ///
    /// When command output exceeds `max_output_bytes`, the full content is
    /// saved to this directory. Defaults to `~/.polaris`.
    #[must_use]
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// Disables overflow file creation for truncated output.
    #[must_use]
    pub fn disable_cache(mut self) -> Self {
        self.cache_dir = None;
        self
    }

    /// Disables directory sandboxing.
    ///
    /// When disabled, `allowed_dirs` is ignored and any directory is permitted.
    /// Sandboxing is enabled by default — call this only when unrestricted
    /// directory access is intentionally required.
    #[must_use]
    pub fn disable_sandbox(mut self) -> Self {
        self.sandbox = false;
        self
    }

    /// Enables or disables strict directory validation.
    ///
    /// When `true`, [`ShellExecutor::new`] panics if any `allowed_dirs` entry
    /// is invalid. When `false` (default), invalid entries are logged as
    /// warnings and skipped.
    #[must_use]
    pub fn with_strict_dir_validation(mut self, strict: bool) -> Self {
        self.strict_dir_validation = strict;
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request / Response
// ─────────────────────────────────────────────────────────────────────────────

/// A shell command execution request.
#[derive(Debug, Clone)]
pub struct ShellRequest {
    /// The command string, interpreted by the system shell (`sh -c`).
    pub command: String,

    /// Working directory. If `None`, uses [`ShellConfig::default_working_dir`].
    pub working_dir: Option<PathBuf>,

    /// Timeout in seconds. If `None`, uses [`ShellConfig::default_timeout_secs`].
    pub timeout_secs: Option<u64>,
}

impl ShellRequest {
    /// Creates a new request with the given command.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            working_dir: None,
            timeout_secs: None,
        }
    }

    /// Sets the working directory for this request.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Sets the timeout in seconds for this request.
    #[must_use]
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }
}

/// Result of a shell command execution.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionResult {
    /// Process exit code. `None` if the process was killed (e.g., timeout).
    pub exit_code: Option<i32>,

    /// Captured standard output.
    pub stdout: String,

    /// Captured standard error.
    pub stderr: String,

    /// Whether the command was killed due to timeout.
    pub timed_out: bool,

    /// Whether stdout was truncated due to exceeding `max_output_bytes`.
    pub stdout_truncated: bool,

    /// Whether stderr was truncated due to exceeding `max_output_bytes`.
    pub stderr_truncated: bool,

    /// Path to the full stdout when truncation occurred.
    ///
    /// Reading this file directly is not advised as the full output may
    /// exhaust context, but it is available for agents to access as
    /// needed. The `cache_dir` is automatically added to `allowed_dirs`
    /// when sandboxing is enabled.
    ///
    /// `None` when stdout was not truncated or overflow file creation is
    /// disabled.
    pub stdout_overflow_path: Option<PathBuf>,

    /// Path to the full stderr when truncation occurred.
    ///
    /// `None` when stderr was not truncated or overflow file creation is
    /// disabled.
    pub stderr_overflow_path: Option<PathBuf>,
}

/// Response from a shell execution attempt.
///
/// Distinguishes between a command that ran (or timed out) and a command
/// that needs user confirmation before it can run.
#[derive(Debug, Clone)]
pub enum ShellResponse {
    /// The command executed (may have timed out or exited non-zero).
    Executed(ExecutionResult),

    /// The command requires user confirmation before execution.
    ///
    /// Contains the original [`ShellRequest`] so the caller can pass it
    /// to [`ShellExecutor::execute_confirmed`] without losing the
    /// `timeout_secs` or `working_dir`.
    ConfirmationRequired(ShellRequest),
}

// ─────────────────────────────────────────────────────────────────────────────
// Shell Executor
// ─────────────────────────────────────────────────────────────────────────────

/// Private inner executor holding configuration and validated state.
struct ExecutorInner {
    config: ShellConfig,
    /// Pre-canonicalized allowed directories (validated at construction time).
    canonical_allowed_dirs: Vec<PathBuf>,
}

/// Shell command execution engine.
///
/// Provides async shell command execution with permission checking,
/// directory sandboxing, timeout enforcement, and output truncation.
///
/// Internally wraps an [`Arc`], so cloning is cheap and all clones share
/// the same underlying executor. Registered as a [`GlobalResource`] by
/// [`ShellPlugin`](crate::ShellPlugin).
///
/// See the [crate-level documentation](crate) for the full permission
/// model and 4-layer architecture.
///
/// # Example
///
/// ```
/// use polaris_shell::{ShellConfig, ShellExecutor, ShellRequest, ShellResponse};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let executor = ShellExecutor::new(ShellConfig::new()
///     .with_allowed_commands(vec!["echo *".into(), "ls *".into()]));
///
/// match executor.execute(ShellRequest::new("echo hello")).await? {
///     ShellResponse::Executed(result) => {
///         assert_eq!(result.exit_code, Some(0));
///         assert!(result.stdout.contains("hello"));
///     }
///     ShellResponse::ConfirmationRequired(request) => {
///         // Prompt user, then: executor.execute_confirmed(request).await?;
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ShellExecutor(Arc<ExecutorInner>);

impl GlobalResource for ShellExecutor {}

impl std::fmt::Debug for ShellExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecutor")
            .field("config", &self.0.config)
            .field("canonical_allowed_dirs", &self.0.canonical_allowed_dirs)
            .finish()
    }
}

impl ShellExecutor {
    /// Creates a new executor with the given configuration.
    ///
    /// Validates `allowed_dirs` entries by canonicalizing them. Invalid entries
    /// are handled according to [`ShellConfig::strict_dir_validation`]:
    /// - `true` — panics with a descriptive message.
    /// - `false` (default) — logs a warning and skips the entry.
    ///
    /// # Panics
    ///
    /// Panics if any entry in [`ShellConfig::allowed_dirs`] cannot be
    /// canonicalized and [`ShellConfig::strict_dir_validation`] is `true`.
    #[must_use]
    pub fn new(config: ShellConfig) -> Self {
        let mut canonical_allowed_dirs: Vec<PathBuf> = config
            .allowed_dirs
            .iter()
            .filter_map(|dir| match dir.canonicalize() {
                Ok(canonical) => Some(canonical),
                Err(err) => {
                    if config.strict_dir_validation {
                        panic!(
                            "ShellConfig: allowed_dirs entry '{}' is invalid: {err}",
                            dir.display()
                        );
                    }
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %err,
                        "skipping invalid allowed_dirs entry"
                    );
                    None
                }
            })
            .collect();

        // Auto-add cache_dir to sandbox so the agent can read overflow files.
        if config.sandbox
            && let Some(cache_dir) = &config.cache_dir
        {
            if let Err(err) = std::fs::create_dir_all(cache_dir) {
                tracing::warn!(
                    dir = %cache_dir.display(),
                    error = %err,
                    "failed to create cache directory"
                );
            } else if let Ok(canonical) = cache_dir.canonicalize()
                && !canonical_allowed_dirs
                    .iter()
                    .any(|d| canonical.starts_with(d))
            {
                canonical_allowed_dirs.push(canonical);
            }
        }

        Self(Arc::new(ExecutorInner {
            config,
            canonical_allowed_dirs,
        }))
    }

    /// Returns a reference to the executor's configuration.
    #[must_use]
    pub fn config(&self) -> &ShellConfig {
        &self.0.config
    }

    /// Checks the permission level for a command.
    ///
    /// For compound commands using shell operators (`&&`, `||`, `;`, `|`, `&`),
    /// each subcommand is evaluated individually and the **strictest**
    /// permission wins: `Deny` > `Confirm` > `Allow`.
    ///
    /// Evaluation order per subcommand (deny always wins):
    /// 1. `denied_commands` match → [`ShellPermission::Deny`]
    /// 2. `allowed_commands` match → [`ShellPermission::Allow`]
    /// 3. Otherwise → [`ShellPermission::Confirm`]
    #[must_use]
    pub fn check_permission(&self, command: &str) -> ShellPermission {
        let subcommands = split_command_chain(command);

        if subcommands.is_empty() {
            // Empty or whitespace-only command
            return self.check_single_permission(command);
        }

        let mut result = ShellPermission::Allow;

        for subcmd in &subcommands {
            match self.check_single_permission(subcmd) {
                ShellPermission::Deny => return ShellPermission::Deny,
                ShellPermission::Confirm => result = ShellPermission::Confirm,
                ShellPermission::Allow => {}
            }
        }

        result
    }

    /// Checks the permission level for a single command (no operator splitting).
    fn check_single_permission(&self, command: &str) -> ShellPermission {
        // Deny always wins
        for pattern in &self.0.config.denied_commands {
            if wildcard_match(pattern, command) {
                return ShellPermission::Deny;
            }
        }

        // Then check allow
        for pattern in &self.0.config.allowed_commands {
            if wildcard_match(pattern, command) {
                return ShellPermission::Allow;
            }
        }

        // Default: requires confirmation
        ShellPermission::Confirm
    }

    /// Executes a shell command with permission checking.
    ///
    /// Returns [`ShellResponse::ConfirmationRequired`] when the command needs
    /// user approval, and [`ShellResponse::Executed`] when the command ran.
    /// Use [`execute_confirmed`](Self::execute_confirmed) after obtaining
    /// user approval for commands that require confirmation.
    ///
    /// Timeouts are not errors — a timed-out command returns
    /// [`ShellResponse::Executed`] with [`ExecutionResult::timed_out`] set
    /// to `true`.
    ///
    /// # Errors
    ///
    /// Returns [`ShellError`] on permission denial, invalid working directory,
    /// spawn failure, or I/O error.
    pub async fn execute(&self, request: ShellRequest) -> Result<ShellResponse, ShellError> {
        self.validate_command_length(&request.command)?;

        match self.check_permission(&request.command) {
            ShellPermission::Deny => {
                return Err(ShellError::PermissionDenied(request.command));
            }
            ShellPermission::Confirm => {
                return Ok(ShellResponse::ConfirmationRequired(request));
            }
            ShellPermission::Allow => {}
        }

        self.execute_inner(request)
            .await
            .map(ShellResponse::Executed)
    }

    /// Executes a shell command after caller has obtained confirmation.
    ///
    /// Bypasses the `Confirm` check but still enforces `Deny` — denied
    /// commands can never be executed.
    ///
    /// # Errors
    ///
    /// Returns [`ShellError`] on permission denial, invalid working directory,
    /// spawn failure, or I/O error.
    pub async fn execute_confirmed(
        &self,
        request: ShellRequest,
    ) -> Result<ExecutionResult, ShellError> {
        self.validate_command_length(&request.command)?;

        if self.check_permission(&request.command) == ShellPermission::Deny {
            return Err(ShellError::PermissionDenied(request.command));
        }

        self.execute_inner(request).await
    }

    /// Inner execution logic — spawns the process, handles timeout and output.
    async fn execute_inner(&self, request: ShellRequest) -> Result<ExecutionResult, ShellError> {
        let working_dir = self.resolve_working_dir(&request)?;
        self.validate_working_dir(&working_dir)?;

        let timeout_secs = request
            .timeout_secs
            .unwrap_or(self.0.config.default_timeout_secs);

        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&request.command)
            .current_dir(&working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|spawn_err| ShellError::SpawnFailed(spawn_err.to_string()))?;

        let result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                let max = self.0.config.max_output_bytes;
                let (stdout, stdout_truncated) = truncate_output(&output.stdout, max);
                let (stderr, stderr_truncated) = truncate_output(&output.stderr, max);

                let stdout_overflow_path = if stdout_truncated {
                    write_overflow_file(&self.0.config.cache_dir, "stdout", &output.stdout)
                } else {
                    None
                };
                let stderr_overflow_path = if stderr_truncated {
                    write_overflow_file(&self.0.config.cache_dir, "stderr", &output.stderr)
                } else {
                    None
                };

                Ok(ExecutionResult {
                    exit_code: output.status.code(),
                    stdout,
                    stderr,
                    timed_out: false,
                    stdout_truncated,
                    stderr_truncated,
                    stdout_overflow_path,
                    stderr_overflow_path,
                })
            }
            Ok(Err(io_err)) => Err(ShellError::IoError(io_err.to_string())),
            Err(_timeout) => {
                // kill_on_drop(true) ensures the child process is killed
                // when the future is dropped on timeout.
                Ok(ExecutionResult {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("Command timed out after {timeout_secs}s"),
                    timed_out: true,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_overflow_path: None,
                    stderr_overflow_path: None,
                })
            }
        }
    }

    /// Resolves the working directory from the request and config.
    fn resolve_working_dir(&self, request: &ShellRequest) -> Result<PathBuf, ShellError> {
        if let Some(dir) = &request.working_dir {
            return Ok(dir.clone());
        }

        if let Some(dir) = &self.0.config.default_working_dir {
            return Ok(dir.clone());
        }

        std::env::current_dir().map_err(|io_err| {
            ShellError::InvalidWorkingDir(format!("cannot determine current directory: {io_err}"))
        })
    }

    /// Rejects commands that exceed the configured maximum length.
    fn validate_command_length(&self, command: &str) -> Result<(), ShellError> {
        let length = command.len();
        let max = self.0.config.max_command_length;
        if length > max {
            return Err(ShellError::CommandTooLong { length, max });
        }
        Ok(())
    }

    /// Validates the working directory against sandbox rules.
    ///
    /// Uses pre-canonicalized `allowed_dirs` from construction time for
    /// efficient comparison without repeated filesystem calls.
    fn validate_working_dir(&self, dir: &Path) -> Result<(), ShellError> {
        if !dir.is_dir() {
            return Err(ShellError::InvalidWorkingDir(format!(
                "not a directory: {}",
                dir.display()
            )));
        }

        if !self.0.config.sandbox {
            return Ok(());
        }

        let canonical = dir.canonicalize().map_err(|io_err| {
            ShellError::InvalidWorkingDir(format!(
                "cannot canonicalize {}: {io_err}",
                dir.display()
            ))
        })?;

        for allowed in &self.0.canonical_allowed_dirs {
            if canonical.starts_with(allowed) {
                return Ok(());
            }
        }

        if self.0.canonical_allowed_dirs.is_empty() {
            Err(ShellError::InvalidWorkingDir(format!(
                "directory {} denied: sandbox is enabled but no allowed_dirs are configured",
                dir.display()
            )))
        } else {
            Err(ShellError::InvalidWorkingDir(format!(
                "directory {} is outside allowed directories",
                dir.display()
            )))
        }
    }
}

/// Simple wildcard pattern matching with O(1) memory.
///
/// - `*` matches any sequence of bytes (including empty).
/// - `?` matches exactly one byte.
///
/// Matching operates on raw bytes, not Unicode characters. This is
/// appropriate for shell command strings which are effectively ASCII.
///
/// Unlike file-path globs, `*` matches everything including `/` and spaces.
///
/// Uses a greedy two-pointer algorithm that requires no heap allocation,
/// making it safe for arbitrarily long command strings.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Saved positions for backtracking on `*`
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            // Exact or single-char wildcard match — advance both
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            // Star: record position and try matching zero chars first
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            // Mismatch: backtrack to last star, consume one more text char
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    // Consume trailing stars in the pattern
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

/// Splits a command string on shell operators (`&&`, `||`, `;`, `|`, `&`),
/// respecting single-quoted, double-quoted, and backslash-escaped characters.
///
/// Returns individual subcommands with surrounding whitespace trimmed.
/// The operator tokens themselves are discarded — only the operands are returned.
///
/// Two-character operators (`&&`, `||`) are checked before single-character
/// operators (`|`, `;`, `&`), so `||` is never misinterpreted as two pipes
/// and `&&` is never misinterpreted as two background operators.
///
/// The `&` character is only treated as a command separator when it is NOT
/// part of a redirect expression (`>&`, `<&`, `&>`).
///
/// This is used for **permission evaluation only**; the full command string is
/// still passed to `sh -c` for execution.
fn split_command_chain(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < len {
        let b = bytes[i];

        // Backslash escapes the next character (outside single quotes)
        if b == b'\\' && !in_single_quote && i + 1 < len {
            i += 2;
            continue;
        }

        // Toggle quote state
        if b == b'\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        // Only split outside quotes
        if !in_single_quote && !in_double_quote {
            // Two-character operators first: && and ||
            // Checked before single-char to avoid misinterpreting || as two pipes
            // or && as two background operators.
            if i + 1 < len
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                let segment = command[start..i].trim();
                if !segment.is_empty() {
                    segments.push(segment);
                }
                i += 2;
                start = i;
                continue;
            }

            // Single-character operators: ; | &
            // For &: skip when part of a redirect (>& <& &>)
            let is_separator =
                b == b';' || b == b'|' || (b == b'&' && !is_redirect_ampersand(bytes, i, len));

            if is_separator {
                let segment = command[start..i].trim();
                if !segment.is_empty() {
                    segments.push(segment);
                }
                i += 1;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    // Final segment
    let segment = command[start..].trim();
    if !segment.is_empty() {
        segments.push(segment);
    }

    segments
}

/// Returns `true` if the `&` at position `i` is part of a shell redirect
/// rather than a command separator.
///
/// Detects three patterns:
/// - `>&` — redirect stdout to file descriptor
/// - `<&` — duplicate input file descriptor
/// - `&>` — redirect both stdout and stderr (bash extension)
fn is_redirect_ampersand(bytes: &[u8], i: usize, len: usize) -> bool {
    // >& or <& — ampersand preceded by redirect operator
    if i > 0 && (bytes[i - 1] == b'>' || bytes[i - 1] == b'<') {
        return true;
    }
    // &> — ampersand followed by redirect operator (bash extension)
    if i + 1 < len && bytes[i + 1] == b'>' {
        return true;
    }
    false
}

/// Writes the full output to a named file in `cache_dir`.
///
/// Filenames use a human-readable adjective-noun pair plus a Unix
/// timestamp for uniqueness (e.g., `shell_stdout_misty-river_1710864022.txt`).
///
/// Returns the file path, or `None` if the cache dir is not configured
/// or the write fails. Best-effort — the truncated output is always
/// returned in the response regardless.
fn write_overflow_file(cache_dir: &Option<PathBuf>, label: &str, bytes: &[u8]) -> Option<PathBuf> {
    let dir = cache_dir.as_ref()?;

    if let Err(err) = std::fs::create_dir_all(dir) {
        tracing::warn!(
            dir = %dir.display(),
            error = %err,
            "failed to create cache directory for overflow output"
        );
        return None;
    }

    let name = names::Generator::default()
        .next()
        .unwrap_or_else(|| "output".into());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format!("shell_{label}_{name}_{timestamp}.txt");
    let path = dir.join(filename);

    if let Err(err) = std::fs::write(&path, bytes) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "failed to write overflow output"
        );
        return None;
    }

    Some(path)
}

/// Converts raw bytes to a UTF-8 string, truncating if necessary.
///
/// Returns the string and whether truncation occurred.
fn truncate_output(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    if bytes.len() <= max_bytes {
        (String::from_utf8_lossy(bytes).into_owned(), false)
    } else {
        let truncated = String::from_utf8_lossy(&bytes[..max_bytes]).into_owned();
        (
            format!("{truncated}\n...[truncated, {max_bytes} byte limit]"),
            true,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates an executor that auto-allows all commands with no sandbox.
    fn auto_allow_executor() -> ShellExecutor {
        ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        )
    }

    /// Helper: unwraps a `ShellResponse::Executed`, panicking on `ConfirmationRequired`.
    fn expect_executed(response: ShellResponse) -> ExecutionResult {
        match response {
            ShellResponse::Executed(result) => result,
            ShellResponse::ConfirmationRequired(req) => {
                panic!(
                    "expected Executed, got ConfirmationRequired for: {}",
                    req.command
                )
            }
        }
    }

    // -- Execution tests --

    #[tokio::test]
    async fn execute_echo() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo hello"))
                .await
                .expect("should succeed"),
        );

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello");
        assert!(result.stderr.is_empty());
        assert!(!result.timed_out);
        assert!(!result.stdout_truncated);
        assert!(!result.stderr_truncated);
    }

    #[tokio::test]
    async fn execute_nonzero_exit() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("exit 42"))
                .await
                .expect("should succeed even with non-zero exit"),
        );

        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn execute_stderr() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo error >&2"))
                .await
                .expect("should succeed"),
        );

        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.is_empty());
        assert_eq!(result.stderr.trim(), "error");
    }

    #[tokio::test]
    async fn execute_timeout() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("sleep 10").with_timeout(1))
                .await
                .expect("should return output with timed_out flag"),
        );

        assert!(result.timed_out);
        assert!(result.exit_code.is_none());
    }

    #[tokio::test]
    async fn execute_working_dir() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("pwd").with_working_dir("/tmp"))
                .await
                .expect("should succeed"),
        );

        // /tmp may resolve to /private/tmp on macOS
        let stdout = result.stdout.trim();
        assert!(
            stdout == "/tmp" || stdout == "/private/tmp",
            "unexpected pwd output: {stdout}"
        );
    }

    #[tokio::test]
    async fn execute_invalid_working_dir() {
        let executor = auto_allow_executor();
        let result = executor
            .execute(ShellRequest::new("echo hi").with_working_dir("/nonexistent/path/xyz"))
            .await;

        assert!(
            matches!(result, Err(ShellError::InvalidWorkingDir(_))),
            "expected InvalidWorkingDir, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn execute_output_truncation() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_max_output_bytes(50)
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("printf '%0.s='; seq 1 200"))
                .await
                .expect("should succeed"),
        );

        assert!(result.stdout_truncated);
        assert!(result.stdout.contains("...[truncated"));
    }

    #[tokio::test]
    async fn truncation_writes_overflow_file() {
        let cache_dir = tempfile::tempdir().expect("should create temp dir");
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_max_output_bytes(50)
                .with_cache_dir(cache_dir.path())
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("seq 1 200"))
                .await
                .expect("should succeed"),
        );

        assert!(result.stdout_truncated);
        let overflow_path = result
            .stdout_overflow_path
            .expect("should have overflow path");
        assert!(overflow_path.exists(), "overflow file should exist");

        let contents = std::fs::read_to_string(&overflow_path).expect("should read overflow file");
        assert!(
            contents.len() > 50,
            "overflow file should contain the full output"
        );
        assert!(
            contents.contains("200"),
            "overflow file should contain last line"
        );
    }

    #[tokio::test]
    async fn no_overflow_file_when_cache_disabled() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_max_output_bytes(50)
                .disable_cache()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("seq 1 200"))
                .await
                .expect("should succeed"),
        );

        assert!(result.stdout_truncated);
        assert!(result.stdout_overflow_path.is_none());
    }

    #[tokio::test]
    async fn no_overflow_file_when_output_fits() {
        let cache_dir = tempfile::tempdir().expect("should create temp dir");
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_cache_dir(cache_dir.path())
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo short"))
                .await
                .expect("should succeed"),
        );

        assert!(!result.stdout_truncated);
        assert!(result.stdout_overflow_path.is_none());
    }

    #[tokio::test]
    async fn execute_pipe() {
        let executor = auto_allow_executor();
        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo 'hello world' | tr ' ' '\\n'"))
                .await
                .expect("should succeed with pipes"),
        );

        assert_eq!(result.exit_code, Some(0));
        let lines: Vec<&str> = result.stdout.trim().lines().collect();
        assert_eq!(lines, vec!["hello", "world"]);
    }

    // -- Command length tests --

    #[tokio::test]
    async fn rejects_command_exceeding_max_length() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_max_command_length(10)
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = executor
            .execute(ShellRequest::new("echo this is way too long"))
            .await;

        assert!(
            matches!(result, Err(ShellError::CommandTooLong { .. })),
            "expected CommandTooLong, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn accepts_command_within_max_length() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_max_command_length(100)
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo ok"))
                .await
                .expect("should succeed within limit"),
        );

        assert_eq!(result.exit_code, Some(0));
    }

    // -- Permission tests --

    #[tokio::test]
    async fn permission_denied_blocks() {
        let executor =
            ShellExecutor::new(ShellConfig::new().with_denied_commands(vec!["rm *".into()]));

        let result = executor.execute(ShellRequest::new("rm -rf /")).await;
        assert!(
            matches!(result, Err(ShellError::PermissionDenied(_))),
            "expected PermissionDenied, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn permission_auto_executes() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo hello"))
                .await
                .expect("should auto-execute"),
        );

        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn permission_confirm_returns_confirmation() {
        // No allowed or denied patterns — everything requires confirmation
        let executor = ShellExecutor::new(ShellConfig::new());

        let response = executor
            .execute(ShellRequest::new("echo hello"))
            .await
            .expect("should succeed");

        assert!(
            matches!(response, ShellResponse::ConfirmationRequired(_)),
            "expected ConfirmationRequired, got: {response:?}"
        );
    }

    #[tokio::test]
    async fn permission_deny_wins_over_allow() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .with_denied_commands(vec!["sudo *".into()]),
        );

        // "sudo ls" matches both * (allow) and "sudo *" (deny) — deny wins
        let result = executor.execute(ShellRequest::new("sudo ls")).await;
        assert!(
            matches!(result, Err(ShellError::PermissionDenied(_))),
            "expected PermissionDenied, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn execute_confirmed_bypasses_confirm() {
        // No allowed patterns — would normally require confirmation
        let executor = ShellExecutor::new(ShellConfig::new().disable_sandbox());

        // execute() returns ConfirmationRequired...
        let response = executor
            .execute(ShellRequest::new("echo hello"))
            .await
            .expect("should succeed");

        let request = match response {
            ShellResponse::ConfirmationRequired(req) => req,
            ShellResponse::Executed(_) => panic!("expected ConfirmationRequired"),
        };

        // ...then execute_confirmed() proceeds with the original request
        let result = executor
            .execute_confirmed(request)
            .await
            .expect("should succeed after confirmation");

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn execute_confirmed_still_denies() {
        let executor =
            ShellExecutor::new(ShellConfig::new().with_denied_commands(vec!["rm *".into()]));

        let result = executor
            .execute_confirmed(ShellRequest::new("rm -rf /"))
            .await;

        assert!(
            matches!(result, Err(ShellError::PermissionDenied(_))),
            "expected PermissionDenied even with execute_confirmed, got: {result:?}"
        );
    }

    // -- Sandboxing tests --

    #[tokio::test]
    async fn sandbox_allows_valid_dir() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .with_allowed_dirs(vec![PathBuf::from("/tmp")]),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo ok").with_working_dir("/tmp"))
                .await
                .expect("should allow /tmp"),
        );

        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn sandbox_blocks_escape() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .with_allowed_dirs(vec![PathBuf::from("/tmp")]),
        );

        let result = executor
            .execute(ShellRequest::new("echo hi").with_working_dir("/usr"))
            .await;

        assert!(
            matches!(result, Err(ShellError::InvalidWorkingDir(_))),
            "expected InvalidWorkingDir for /usr, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn sandbox_disabled() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("echo ok").with_working_dir("/tmp"))
                .await
                .expect("should allow any dir when sandbox is disabled"),
        );

        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn sandbox_blocks_all_when_no_dirs() {
        // Default: sandbox=true, allowed_dirs=[] → no directory is allowed
        let executor =
            ShellExecutor::new(ShellConfig::new().with_allowed_commands(vec!["*".into()]));

        let result = executor
            .execute(ShellRequest::new("echo hi").with_working_dir("/tmp"))
            .await;

        assert!(
            matches!(result, Err(ShellError::InvalidWorkingDir(_))),
            "expected InvalidWorkingDir with empty allowed_dirs, got: {result:?}"
        );
    }

    // -- check_permission unit tests --

    #[test]
    fn check_permission_deny_first() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .with_denied_commands(vec!["sudo *".into()]),
        );

        assert_eq!(
            executor.check_permission("sudo apt install"),
            ShellPermission::Deny
        );
        assert_eq!(executor.check_permission("ls -la"), ShellPermission::Allow);
    }

    #[test]
    fn check_permission_confirm_default() {
        let executor = ShellExecutor::new(ShellConfig::new());
        assert_eq!(
            executor.check_permission("any command"),
            ShellPermission::Confirm
        );
    }

    // -- split_command_chain tests --

    #[test]
    fn split_on_and() {
        assert_eq!(
            split_command_chain("echo foo && echo bar"),
            vec!["echo foo", "echo bar"]
        );
    }

    #[test]
    fn split_on_or() {
        assert_eq!(
            split_command_chain("echo foo || echo bar"),
            vec!["echo foo", "echo bar"]
        );
    }

    #[test]
    fn split_on_semicolon() {
        assert_eq!(
            split_command_chain("echo foo; echo bar"),
            vec!["echo foo", "echo bar"]
        );
    }

    #[test]
    fn split_on_pipe() {
        assert_eq!(
            split_command_chain("echo foo | grep foo"),
            vec!["echo foo", "grep foo"]
        );
    }

    #[test]
    fn split_on_background() {
        assert_eq!(
            split_command_chain("echo foo & echo bar"),
            vec!["echo foo", "echo bar"]
        );
    }

    #[test]
    fn split_mixed_operators() {
        assert_eq!(
            split_command_chain("echo a && echo b | grep b; echo c || echo d & echo e"),
            vec!["echo a", "echo b", "grep b", "echo c", "echo d", "echo e"]
        );
    }

    #[test]
    fn split_preserves_single_command() {
        assert_eq!(
            split_command_chain("echo hello world"),
            vec!["echo hello world"]
        );
    }

    #[test]
    fn split_respects_single_quotes() {
        assert_eq!(
            split_command_chain("echo 'foo && bar'"),
            vec!["echo 'foo && bar'"]
        );
    }

    #[test]
    fn split_respects_double_quotes() {
        assert_eq!(
            split_command_chain("echo \"foo | bar\" && echo baz"),
            vec!["echo \"foo | bar\"", "echo baz"]
        );
    }

    #[test]
    fn split_respects_backslash_escape() {
        // \; escapes the semicolon — not treated as a separator
        assert_eq!(
            split_command_chain("echo foo\\; echo bar"),
            vec!["echo foo\\; echo bar"]
        );
    }

    #[test]
    fn split_ignores_redirect_ampersand_gt() {
        // >& is a redirect, not a separator
        assert_eq!(split_command_chain("echo foo 2>&1"), vec!["echo foo 2>&1"]);
    }

    #[test]
    fn split_ignores_redirect_ampersand_lt() {
        // <& is a redirect, not a separator
        assert_eq!(split_command_chain("cat 0<&3"), vec!["cat 0<&3"]);
    }

    #[test]
    fn split_ignores_redirect_ampersand_bash_ext() {
        // &> is a bash redirect extension, not a separator
        assert_eq!(
            split_command_chain("echo foo &>/dev/null"),
            vec!["echo foo &>/dev/null"]
        );
    }

    #[test]
    fn split_redirect_with_chain() {
        // 2>&1 is a redirect, but the && is still a separator
        assert_eq!(
            split_command_chain("cmd1 2>&1 && cmd2"),
            vec!["cmd1 2>&1", "cmd2"]
        );
    }

    #[test]
    fn split_empty_string() {
        let result: Vec<&str> = split_command_chain("");
        assert!(result.is_empty());
    }

    #[test]
    fn split_whitespace_only() {
        let result: Vec<&str> = split_command_chain("   ");
        assert!(result.is_empty());
    }

    // -- Chain permission tests --

    #[test]
    fn permission_chain_deny_catches_hidden_command() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into()])
                .with_denied_commands(vec!["rm *".into()]),
        );

        // Without chain splitting, "echo foo && rm -rf /" would match "echo *" and auto-allow.
        // With splitting, "rm -rf /" is checked individually and denied.
        assert_eq!(
            executor.check_permission("echo foo && rm -rf /"),
            ShellPermission::Deny
        );
    }

    #[test]
    fn permission_chain_confirm_propagates() {
        let executor =
            ShellExecutor::new(ShellConfig::new().with_allowed_commands(vec!["echo *".into()]));

        // "echo foo" is allowed, but "unknown_cmd" requires confirmation → Confirm wins
        assert_eq!(
            executor.check_permission("echo foo && unknown_cmd"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn permission_chain_all_allow() {
        let executor = ShellExecutor::new(
            ShellConfig::new().with_allowed_commands(vec!["echo *".into(), "grep *".into()]),
        );

        assert_eq!(
            executor.check_permission("echo foo | grep foo"),
            ShellPermission::Allow
        );
    }

    #[test]
    fn permission_chain_background_deny() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into()])
                .with_denied_commands(vec!["rm *".into()]),
        );

        // Background operator: echo runs, then rm runs concurrently — both need checking
        assert_eq!(
            executor.check_permission("echo foo & rm -rf /"),
            ShellPermission::Deny
        );
    }

    #[test]
    fn permission_redirect_not_false_split() {
        let executor =
            ShellExecutor::new(ShellConfig::new().with_allowed_commands(vec!["echo *".into()]));

        // 2>&1 should NOT cause a split — this is a single allowed command
        assert_eq!(
            executor.check_permission("echo foo 2>&1"),
            ShellPermission::Allow
        );
    }

    // -- truncate_output tests --

    #[test]
    fn truncate_output_within_limit() {
        let (output, truncated) = truncate_output(b"hello", 100);
        assert_eq!(output, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_output_exceeds_limit() {
        let (output, truncated) = truncate_output(b"hello world", 5);
        assert!(output.starts_with("hello"));
        assert!(output.contains("...[truncated"));
        assert!(truncated);
    }
}
