//! Shell command execution engine.
//!
//! Provides [`ShellExecutor`] for running shell commands with permission
//! checking, directory sandboxing, timeout enforcement, and bounded capture.

use crate::error::ShellError;
use crate::permission::ShellPermission;
use crate::process::{
    ProcessTable, ProcessTreeGuarantee, ShellCancel, ShellOutcome, ShellProcess, ShellProcessId,
    ShellProcessStatus, ShellSinkHandle, start_managed,
};
use polaris_system::resource::GlobalResource;
use serde::Serialize;
use std::collections::BTreeMap;
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

/// Default per-stream overflow spill quota (64 MB).
const DEFAULT_MAX_OVERFLOW_BYTES: u64 = 64 * 1024 * 1024;

/// Trusted `PATH` used when the child does not inherit the host environment.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Default graceful termination period.
const DEFAULT_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Default time allowed for reader drain and final reaping.
const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_millis(250);

/// Default time allowed for plugin shutdown cleanup.
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Default number of unconsumed completed results retained by the executor.
const DEFAULT_RETAINED_COMPLETED_RESULTS: usize = 64;

/// Returns `~/.polaris`, or `None` if the home directory cannot be determined.
fn default_cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".polaris"))
}

/// Configuration for shell command execution.
///
/// Controls defaults, permission rules, directory sandboxing, and the
/// environment child processes see.
///
/// Construct with [`new`](Self::new) and the `with_*` builders rather than a
/// struct literal — the type is `#[non_exhaustive]`, and every field has a
/// builder.
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
///     .with_allowed_dirs(vec!["/home/user/project".into()])
///     .with_env_allowlist(vec!["CARGO_HOME".into()]);
/// ```
#[derive(Clone)]
#[non_exhaustive]
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
    ///
    /// On Linux, a sandboxed start validates an opened directory through
    /// `/proc/self/fd` and uses that handle for the child's initial working
    /// directory. Procfs must be available; validation fails closed otherwise.
    /// On other platforms, the host must prevent untrusted renames and symlink
    /// replacements of the directory and its ancestors between validation and
    /// spawn. This setting restricts the initial working directory, not the
    /// paths a command can access afterward.
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

    /// Maximum bytes spilled to one stream's overflow file (default: 64 MB).
    ///
    /// In-memory capture is bounded by `max_output_bytes`; without this
    /// second ceiling a permitted but noisy command could write unbounded
    /// output to disk before its timeout elapses. When the quota is reached
    /// the reader stops spilling, emits
    /// [`ShellEvent::OverflowLimitReached`](crate::ShellEvent::OverflowLimitReached),
    /// and sets
    /// [`ExecutionResult::stdout_overflow_truncated`] /
    /// [`ExecutionResult::stderr_overflow_truncated`]; the bytes already
    /// written stay readable. Use `u64::MAX` for an effectively unbounded
    /// spill, or [`disable_cache`](Self::disable_cache) to write none at all.
    pub max_overflow_bytes: u64,

    /// Whether to panic on invalid `allowed_dirs` entries at construction time.
    ///
    /// Only relevant when `sandbox` is `true`.
    ///
    /// - `true` — panics if any entry cannot be canonicalized (catches config
    ///   errors at startup).
    /// - `false` (default) — logs a warning and skips invalid entries.
    pub strict_dir_validation: bool,

    /// Grace period between graceful and forced process termination
    /// (default: 2 seconds).
    ///
    /// On Unix the whole window elapses between the group `SIGTERM` and the
    /// group `SIGKILL`, so it bounds how long a cancelled or timed-out
    /// command has to shut down cleanly.
    pub terminate_grace: Duration,

    /// Deadline for reader drain and final process reaping
    /// (default: 250 milliseconds).
    pub drain_deadline: Duration,

    /// Maximum time the shell plugin gives shutdown cleanup
    /// (default: 3 seconds).
    pub shutdown_grace: Duration,

    /// Number of unconsumed completed results retained for late waiters
    /// (default: 64).
    pub retained_completed_results: usize,

    /// Whether children inherit the host process environment (default: `false`).
    ///
    /// The default is a clean environment: the child sees `PATH` (from
    /// [`path`](Self::path)), whichever of `HOME`, `USER`, `LOGNAME`, `SHELL`,
    /// `LANG`, `TERM`, and `TMPDIR` the host has set, then
    /// [`env_allowlist`](Self::env_allowlist), then [`env`](Self::env). This
    /// keeps host credentials — `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`,
    /// `*_API_KEY` — out of the output of an otherwise harmless allowed
    /// command such as `env`.
    ///
    /// Setting this to `true` restores full inheritance, including every
    /// credential and loader variable in the server's environment. Prefer an
    /// [`env_allowlist`](Self::env_allowlist) naming exactly what the command
    /// needs.
    pub inherit_env: bool,

    /// Host environment variable names copied into the child (default: empty).
    ///
    /// Only consulted when [`inherit_env`](Self::inherit_env) is `false`; a
    /// name with no value in the host environment is skipped. The allowlist is
    /// honoured verbatim and is not filtered for credential-shaped names — an
    /// entry naming a secret passes that secret to the child, which is the
    /// point of naming it.
    pub env_allowlist: Vec<String>,

    /// Environment variables set explicitly on every child (default: empty).
    ///
    /// Applied last, so these override the trusted base, the allowlist, and —
    /// when [`inherit_env`](Self::inherit_env) is `true` — the inherited value.
    /// Debug formatting includes names but redacts every value.
    pub env: BTreeMap<String, String>,

    /// `PATH` given to children when [`inherit_env`](Self::inherit_env) is
    /// `false`.
    ///
    /// `None` (the default) uses `/usr/local/bin:/usr/bin:/bin`. An entry for
    /// `PATH` in [`env`](Self::env) overrides this.
    pub path: Option<String>,
}

impl std::fmt::Debug for ShellConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct RedactedEnv<'a>(&'a BTreeMap<String, String>);

        impl std::fmt::Debug for RedactedEnv<'_> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_map()
                    .entries(self.0.keys().map(|name| (name, "[REDACTED]")))
                    .finish()
            }
        }

        formatter
            .debug_struct("ShellConfig")
            .field("default_working_dir", &self.default_working_dir)
            .field("default_timeout_secs", &self.default_timeout_secs)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("allowed_commands", &self.allowed_commands)
            .field("denied_commands", &self.denied_commands)
            .field("sandbox", &self.sandbox)
            .field("allowed_dirs", &self.allowed_dirs)
            .field("max_command_length", &self.max_command_length)
            .field("cache_dir", &self.cache_dir)
            .field("overflow_ttl_secs", &self.overflow_ttl_secs)
            .field("max_overflow_bytes", &self.max_overflow_bytes)
            .field("strict_dir_validation", &self.strict_dir_validation)
            .field("terminate_grace", &self.terminate_grace)
            .field("drain_deadline", &self.drain_deadline)
            .field("shutdown_grace", &self.shutdown_grace)
            .field(
                "retained_completed_results",
                &self.retained_completed_results,
            )
            .field("inherit_env", &self.inherit_env)
            .field("env_allowlist", &self.env_allowlist)
            .field("env", &RedactedEnv(&self.env))
            .field("path", &self.path)
            .finish()
    }
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
            max_overflow_bytes: DEFAULT_MAX_OVERFLOW_BYTES,
            strict_dir_validation: false,
            terminate_grace: DEFAULT_TERMINATE_GRACE,
            drain_deadline: DEFAULT_DRAIN_DEADLINE,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            retained_completed_results: DEFAULT_RETAINED_COMPLETED_RESULTS,
            inherit_env: false,
            env_allowlist: Vec::new(),
            env: BTreeMap::new(),
            path: None,
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

    /// Sets the age at which shutdown cleanup deletes an overflow file.
    ///
    /// Defaults to 3600 seconds (1 hour).
    #[must_use]
    pub fn with_overflow_ttl_secs(mut self, secs: u64) -> Self {
        self.overflow_ttl_secs = secs;
        self
    }

    /// Sets the per-stream overflow spill quota in bytes.
    ///
    /// Defaults to 64 MB. See [`max_overflow_bytes`](Self::max_overflow_bytes).
    #[must_use]
    pub fn with_max_overflow_bytes(mut self, bytes: u64) -> Self {
        self.max_overflow_bytes = bytes;
        self
    }

    /// Enables or disables inheritance of the host process environment.
    ///
    /// Inheritance is **off** by default, which is a deliberate departure from
    /// what [`std::process::Command`] does. See
    /// [`inherit_env`](Self::inherit_env) for what the child sees instead, and
    /// prefer [`with_env_allowlist`](Self::with_env_allowlist) over turning
    /// this on.
    #[must_use]
    pub fn with_inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    /// Sets the host environment variable names copied into children.
    ///
    /// Only consulted when inheritance is off.
    #[must_use]
    pub fn with_env_allowlist(mut self, names: Vec<String>) -> Self {
        self.env_allowlist = names;
        self
    }

    /// Sets the environment variables applied explicitly to every child.
    ///
    /// These take precedence over the trusted base, the allowlist, and any
    /// inherited value.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Adds one explicit environment variable, replacing any existing entry.
    #[must_use]
    pub fn with_env_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Sets the `PATH` used when the child does not inherit the environment.
    ///
    /// Defaults to `/usr/local/bin:/usr/bin:/bin`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
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

    /// Sets the graceful termination period.
    #[must_use]
    pub fn with_terminate_grace(mut self, grace: Duration) -> Self {
        self.terminate_grace = grace;
        self
    }

    /// Sets the reader-drain and final-reaping deadline.
    #[must_use]
    pub fn with_drain_deadline(mut self, deadline: Duration) -> Self {
        self.drain_deadline = deadline;
        self
    }

    /// Sets the plugin shutdown cleanup deadline.
    #[must_use]
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Sets the number of unconsumed completed results retained for late waiters.
    #[must_use]
    pub fn with_retained_completed_results(mut self, retained: usize) -> Self {
        self.retained_completed_results = retained;
        self
    }

    /// Returns the `PATH` handed to children that do not inherit the host
    /// environment.
    pub(crate) fn resolved_path(&self) -> &str {
        self.path.as_deref().unwrap_or(DEFAULT_PATH)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request / Response
// ─────────────────────────────────────────────────────────────────────────────

/// A shell command execution request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ShellRequest {
    /// The command string, interpreted by the system shell (`sh -c`).
    pub command: String,

    /// Working directory. If `None`, uses [`ShellConfig::default_working_dir`].
    pub working_dir: Option<PathBuf>,

    /// Timeout in seconds. If `None`, uses [`ShellConfig::default_timeout_secs`].
    pub timeout_secs: Option<u64>,

    /// Optional externally owned cancellation observer.
    pub cancel: Option<ShellCancel>,

    /// Optional destination for incremental process events.
    pub sink: Option<ShellSinkHandle>,
}

impl ShellRequest {
    /// Creates a new request with the given command.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_shell::ShellRequest;
    ///
    /// let req = ShellRequest::new("cargo build")
    ///     .with_working_dir("/home/user/project")
    ///     .with_timeout(120);
    /// ```
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            working_dir: None,
            timeout_secs: None,
            cancel: None,
            sink: None,
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

    /// Attaches an externally owned cancellation observer.
    #[must_use]
    pub fn with_cancel(mut self, cancel: ShellCancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Attaches an incremental event sink.
    #[must_use]
    pub fn with_sink(mut self, sink: ShellSinkHandle) -> Self {
        self.sink = Some(sink);
        self
    }
}

/// Result of a shell command execution.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
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

    /// Path to spilled stdout when truncation occurred.
    ///
    /// Reading this file directly is not advised as the full output may
    /// exhaust context, but it is available for agents to access as
    /// needed. The `cache_dir` is automatically added to `allowed_dirs`
    /// when sandboxing is enabled.
    ///
    /// The file contains the complete stream unless
    /// [`stdout_overflow_truncated`](Self::stdout_overflow_truncated) is
    /// `true`, in which case it contains the first
    /// [`ShellConfig::max_overflow_bytes`] bytes. `None` when stdout was not
    /// truncated or overflow file creation is disabled.
    pub stdout_overflow_path: Option<PathBuf>,

    /// Path to spilled stderr when truncation occurred.
    ///
    /// The file contains the complete stream unless
    /// [`stderr_overflow_truncated`](Self::stderr_overflow_truncated) is
    /// `true`, in which case it contains the first
    /// [`ShellConfig::max_overflow_bytes`] bytes. `None` when stderr was not
    /// truncated or overflow file creation is disabled.
    pub stderr_overflow_path: Option<PathBuf>,

    /// Whether the stdout overflow file hit
    /// [`ShellConfig::max_overflow_bytes`] and is therefore incomplete.
    pub stdout_overflow_truncated: bool,

    /// Whether the stderr overflow file hit
    /// [`ShellConfig::max_overflow_bytes`] and is therefore incomplete.
    pub stderr_overflow_truncated: bool,

    /// Terminal lifecycle outcome.
    pub outcome: ShellOutcome,

    /// Executor-local managed-run identity.
    pub process: ShellProcessId,

    /// Elapsed wall-clock duration of the managed run.
    pub duration: Duration,

    /// Number of events that did not reach the configured sink.
    ///
    /// Counts events the sink truthfully reported dropping, plus any data
    /// event suppressed because the terminal event had already been delivered
    /// — the guarantee that `Finished` is last is worth more than a late
    /// chunk. Zero when no sink is attached.
    pub dropped_events: u64,

    /// Whether process reaping and output-reader cleanup completed by the deadline.
    pub cleanup_completed: bool,
}

impl ExecutionResult {
    pub(crate) fn cancelled_before_spawn(process: ShellProcessId, dropped_events: u64) -> Self {
        Self {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_overflow_path: None,
            stderr_overflow_path: None,
            stdout_overflow_truncated: false,
            stderr_overflow_truncated: false,
            outcome: ShellOutcome::Cancelled,
            process,
            duration: Duration::ZERO,
            dropped_events,
            cleanup_completed: true,
        }
    }
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

/// Response from a permission-gated managed process start.
///
/// Keep the original request when confirmation is needed: it carries the
/// working directory, timeout, cancellation observer, and event sink.
///
/// # Example
///
/// ```no_run
/// use polaris_shell::{
///     ExecutionResult, ShellError, ShellExecutor, ShellRequest, ShellStartResponse,
/// };
///
/// async fn run(
///     shell: &ShellExecutor,
///     user_approved: bool,
/// ) -> Result<Option<ExecutionResult>, ShellError> {
///     let process = match shell
///         .start(ShellRequest::new("cargo test").with_timeout(120))
///         .await?
///     {
///         ShellStartResponse::Started(process) => process,
///         ShellStartResponse::ConfirmationRequired(request) => {
///             if !user_approved {
///                 return Ok(None);
///             }
///             shell.start_confirmed(request).await?
///         }
///     };
///     process.wait().await.transpose()
/// }
/// ```
#[derive(Debug)]
pub enum ShellStartResponse {
    /// A managed process was accepted.
    Started(ShellProcess),
    /// The command requires user confirmation before a process can be created.
    ConfirmationRequired(ShellRequest),
}

// ─────────────────────────────────────────────────────────────────────────────
// Shell Executor
// ─────────────────────────────────────────────────────────────────────────────

/// Who owns the terminal result of a run the executor is about to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// `execute` / `execute_confirmed` will await the result themselves, so it
    /// is reserved and [`ShellExecutor::wait_for`] refuses to take it.
    Foreground,
    /// The caller receives a [`ShellProcess`] and decides who waits.
    Caller,
}

/// Converts a foreground wait into the flat result those two methods return.
///
/// Reservation means the `None` arm is unreachable today; it is mapped to an
/// error rather than a panic so `execute` and `execute_confirmed` have no
/// panicking failure mode at all.
fn foreground_result(
    id: ShellProcessId,
    waited: Option<Result<ExecutionResult, ShellError>>,
) -> Result<ExecutionResult, ShellError> {
    waited.unwrap_or(Err(ShellError::ResultConsumed(id)))
}

/// Private inner executor holding configuration and validated state.
pub(crate) struct ExecutorInner {
    pub(crate) config: ShellConfig,
    /// Pre-canonicalized allowed directories (validated at construction time).
    canonical_allowed_dirs: Vec<PathBuf>,
    pub(crate) processes: ProcessTable,
}

/// A validated directory object and the path used in process status.
pub(crate) struct ValidatedWorkingDir {
    pub(crate) path: PathBuf,
    #[cfg(target_os = "linux")]
    pub(crate) directory: std::fs::File,
}

/// Start, stream, inspect, stop, and await permission-gated shell processes.
///
/// Use this Global resource when a typed system needs to run a shell command
/// itself, or needs to own the lifecycle of a long-running one: `execute` for
/// a foreground command whose output you want as a value, `start` for a
/// managed process you will stream, poll, and stop. Use
/// [`ShellTools`](crate::ShellTools) instead when an *LLM* should invoke the
/// `run_command` tool.
///
/// Every entry point applies the same permission policy, directory sandbox,
/// command-length limit, timeout, and per-stream capture bound, so the choice
/// between foreground and managed execution is only about who owns the
/// lifetime.
///
/// # Scope and access
///
/// Global: permission configuration and the managed process table belong to
/// the server rather than to one graph context, and a process started by one
/// system must remain inspectable from another. Systems take `Res<ShellExecutor>`
/// only — `ResMut` is not available for globals, and is not needed because the
/// executor is internally synchronized. Cloning is cheap ([`Arc`] inside) and
/// every clone shares one process table, so a clone captured by a spawned task
/// still sees and can stop the same runs.
///
/// # Provided by
///
/// [`ShellPlugin`](crate::ShellPlugin) inserts the executor during `build()` and
/// stops its live processes during `cleanup()`.
///
/// # Alternatives
///
/// | Instead of | Use | When |
/// |---|---|---|
/// | `Res<ShellExecutor>` | [`ShellTools`](crate::ShellTools) | an LLM should choose the command |
/// | `execute` | [`start`](Self::start) + [`ShellProcess`] | the caller wants incremental output or an explicit stop |
/// | polling [`processes`](Self::processes) | [`ShellSinkHandle`] on the request | the caller wants pushed events rather than snapshots |
///
/// See the [crate-level documentation](crate) for the full permission
/// model and 4-layer architecture.
///
/// # Example
///
/// ```
/// use polaris_shell::{ShellExecutor, ShellRequest, ShellResponse};
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_system::system::SystemError;
///
/// /// Runs a build in the foreground and reports what the shell said.
/// #[system]
/// async fn run_build(shell: Res<ShellExecutor>) -> Result<String, SystemError> {
///     let response = shell
///         .execute(ShellRequest::new("cargo build").with_timeout(600))
///         .await
///         .map_err(|shell_err| SystemError::ExecutionError(shell_err.to_string()))?;
///
///     Ok(match response {
///         ShellResponse::Executed(result) => result.stdout,
///         ShellResponse::ConfirmationRequired(request) => {
///             format!("`{}` needs user approval", request.command)
///         }
///     })
/// }
///
/// /// Starts a long-running server and hands it to the executor to outlive
/// /// this system.
/// #[system]
/// async fn start_server(shell: Res<ShellExecutor>) -> Result<(), SystemError> {
///     let process = shell
///         .start_confirmed(ShellRequest::new("./serve"))
///         .await
///         .map_err(|shell_err| SystemError::ExecutionError(shell_err.to_string()))?;
///
///     // Without `detach`, dropping the handle here would stop the child.
///     let id = process.detach();
///     tracing::info!(?id, "server started");
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct ShellExecutor(pub(crate) Arc<ExecutorInner>);

impl GlobalResource for ShellExecutor {}

impl std::fmt::Debug for ShellExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecutor")
            .field("config", &self.0.config)
            .field("canonical_allowed_dirs", &self.0.canonical_allowed_dirs)
            // Counted rather than snapshotted: `Debug` runs per node under
            // `#[system(inspect)]`, and `processes()` clones every command
            // string and path just to be measured.
            .field("process_count", &self.0.processes.len())
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
            processes: ProcessTable::new(),
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
    ///
    /// # Auto-allow is fail-closed
    ///
    /// The command string is ultimately handed to `sh -c`, which understands
    /// far more than the operator split above. A command that would otherwise
    /// be auto-allowed is therefore downgraded to
    /// [`ShellPermission::Confirm`] when it contains a construct the split
    /// cannot account for, because that construct can hide a whole second
    /// command from the decomposition:
    ///
    /// | Construct | Example |
    /// |---|---|
    /// | Line continuation outside single quotes | A backslash followed by a newline |
    /// | Unquoted newline separator | Commands on separate lines |
    /// | Shell comment | `echo ok # comment` |
    /// | Command substitution | `echo $(rm -rf /)`, ``echo `rm -rf /` `` |
    /// | Arithmetic expansion | `echo $((1+1))` |
    /// | Process substitution | `diff <(a) <(b)` |
    /// | Indirect execution | `eval …`, `exec …`, `source …`, `. …` |
    /// | Displaced command word | `>out.txt echo hi` |
    /// | Unbalanced quote | `echo 'hi` |
    ///
    /// A deny match still wins over everything, and a command that decomposes
    /// cleanly is unaffected — `echo hi && ls` is still auto-allowed. Single
    /// quotes are literal in `sh`, so `echo '$(x)'` also stays auto-allowed.
    /// This is a fail-closed heuristic, not a shell parser: it can only move a
    /// command from `Allow` to `Confirm`, never the other way, and the
    /// security boundary remains the confirmation step.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_shell::{ShellConfig, ShellExecutor, ShellPermission};
    ///
    /// let config = ShellConfig::new()
    ///     .with_allowed_commands(vec!["cargo *".into(), "echo *".into()])
    ///     .with_denied_commands(vec!["rm -rf *".into()])
    ///     .disable_sandbox();
    /// let executor = ShellExecutor::new(config);
    ///
    /// assert_eq!(executor.check_permission("cargo build"), ShellPermission::Allow);
    /// assert_eq!(executor.check_permission("rm -rf /"), ShellPermission::Deny);
    /// assert_eq!(executor.check_permission("ls -la"), ShellPermission::Confirm);
    ///
    /// // A substitution could hide anything, so it is never auto-allowed.
    /// assert_eq!(
    ///     executor.check_permission("echo $(rm -rf /tmp/x)"),
    ///     ShellPermission::Confirm
    /// );
    /// ```
    #[must_use]
    pub fn check_permission(&self, command: &str) -> ShellPermission {
        match self.check_decomposed_permission(command) {
            ShellPermission::Allow => match opaque_construct(command) {
                Some(construct) => {
                    tracing::debug!(
                        construct,
                        "command matches an allow pattern but is not fully decomposable; \
                         requiring confirmation"
                    );
                    ShellPermission::Confirm
                }
                None => ShellPermission::Allow,
            },
            stricter => stricter,
        }
    }

    /// Applies the allow/deny patterns to each decomposed subcommand.
    fn check_decomposed_permission(&self, command: &str) -> ShellPermission {
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
    /// The run is reserved for this call: it is visible through
    /// [`processes`](Self::processes) and stoppable through
    /// [`stop`](Self::stop), but [`wait_for`](Self::wait_for) will not take its
    /// result. Dropping the returned future stops the child — see
    /// [`ShellProcess`](ShellProcess#dropping-requests-a-stop).
    ///
    /// # Errors
    ///
    /// - [`ShellError::PermissionDenied`] — a deny pattern matched.
    /// - [`ShellError::CommandTooLong`] — the command exceeds
    ///   [`ShellConfig::max_command_length`].
    /// - [`ShellError::InvalidWorkingDir`] — the directory does not exist or
    ///   is outside the sandbox.
    /// - [`ShellError::SpawnFailed`] / [`ShellError::IoError`] — the child
    ///   could not be started or its pipes could not be taken.
    /// - [`ShellError::ResultConsumed`] — the reservation was bypassed and the
    ///   terminal result was taken by another waiter. Not reachable through
    ///   the reserved path; reported rather than panicked so this surface has
    ///   no panicking failure mode.
    pub async fn execute(&self, request: ShellRequest) -> Result<ShellResponse, ShellError> {
        match self.start_gated(request, Ownership::Foreground).await? {
            ShellStartResponse::ConfirmationRequired(request) => {
                Ok(ShellResponse::ConfirmationRequired(request))
            }
            ShellStartResponse::Started(process) => {
                let id = process.id();
                foreground_result(id, process.wait().await).map(ShellResponse::Executed)
            }
        }
    }

    /// Executes a shell command after caller has obtained confirmation.
    ///
    /// Bypasses the `Confirm` check but still enforces `Deny` — denied
    /// commands can never be executed. Reserves its run exactly as
    /// [`execute`](Self::execute) does.
    ///
    /// # Errors
    ///
    /// The same set as [`execute`](Self::execute), except that a command which
    /// would merely need confirmation runs instead of being reported.
    pub async fn execute_confirmed(
        &self,
        request: ShellRequest,
    ) -> Result<ExecutionResult, ShellError> {
        let process = self
            .start_confirmed_gated(request, Ownership::Foreground)
            .await?;
        let id = process.id();
        foreground_result(id, process.wait().await)
    }

    /// Starts a permission-gated managed shell process.
    ///
    /// Permission denial, confirmation, and directory validation happen before
    /// a run identity or operating-system process is created. The caller owns
    /// the returned handle: dropping it stops the child, and
    /// [`ShellProcess::detach`] hands the run to the executor instead — see
    /// [`ShellProcess`](ShellProcess#dropping-requests-a-stop).
    ///
    /// # Errors
    ///
    /// [`ShellError::PermissionDenied`], [`ShellError::CommandTooLong`],
    /// [`ShellError::InvalidWorkingDir`], [`ShellError::SpawnFailed`], or
    /// [`ShellError::IoError`], as described on [`execute`](Self::execute).
    pub async fn start(&self, request: ShellRequest) -> Result<ShellStartResponse, ShellError> {
        self.start_gated(request, Ownership::Caller).await
    }

    /// Starts a managed shell process after caller confirmation.
    ///
    /// The deny policy remains authoritative.
    ///
    /// # Errors
    ///
    /// The same set as [`start`](Self::start), except that a command which
    /// would merely need confirmation is started instead of being reported.
    pub async fn start_confirmed(&self, request: ShellRequest) -> Result<ShellProcess, ShellError> {
        self.start_confirmed_gated(request, Ownership::Caller).await
    }

    async fn start_gated(
        &self,
        request: ShellRequest,
        ownership: Ownership,
    ) -> Result<ShellStartResponse, ShellError> {
        self.validate_command_length(&request.command)?;
        match self.check_permission(&request.command) {
            ShellPermission::Deny => Err(ShellError::PermissionDenied(request.command)),
            ShellPermission::Confirm => Ok(ShellStartResponse::ConfirmationRequired(request)),
            ShellPermission::Allow => self
                .start_inner(request, ownership)
                .await
                .map(ShellStartResponse::Started),
        }
    }

    async fn start_confirmed_gated(
        &self,
        request: ShellRequest,
        ownership: Ownership,
    ) -> Result<ShellProcess, ShellError> {
        self.validate_command_length(&request.command)?;
        if self.check_permission(&request.command) == ShellPermission::Deny {
            return Err(ShellError::PermissionDenied(request.command));
        }
        self.start_inner(request, ownership).await
    }

    async fn start_inner(
        &self,
        request: ShellRequest,
        ownership: Ownership,
    ) -> Result<ShellProcess, ShellError> {
        let working_dir = self.resolve_working_dir(&request)?;
        // Managed spawning validates and retains the directory object before
        // allocating a run identity or observing cancellation.
        let timeout_secs = request
            .timeout_secs
            .unwrap_or(self.0.config.default_timeout_secs);
        start_managed(
            self.clone(),
            request,
            working_dir,
            Duration::from_secs(timeout_secs),
            ownership == Ownership::Foreground,
        )
        .await
    }

    /// Returns snapshots of all discoverable live and retained processes.
    #[must_use]
    pub fn processes(&self) -> Vec<ShellProcessStatus> {
        crate::process::processes(&self.0)
    }

    /// Returns a snapshot of one discoverable process.
    #[must_use]
    pub fn process(&self, id: ShellProcessId) -> Option<ShellProcessStatus> {
        crate::process::process(&self.0, id)
    }

    /// Waits for and consumes one retained terminal result.
    ///
    /// Terminal results are consumed exactly once, so a second call for the
    /// same run reports `None`.
    ///
    /// # Errors
    ///
    /// The outer `Option` reports *availability*, the inner `Result` reports
    /// the run:
    ///
    /// - `None` — nothing to hand over. Either the id is unknown (never
    ///   started, or evicted after
    ///   [`ShellConfig::retained_completed_results`] newer results completed),
    ///   the result was already consumed, or the run is **reserved** for a
    ///   foreground [`execute`](Self::execute) / [`execute_confirmed`](Self::execute_confirmed)
    ///   caller. [`ShellProcessStatus::reserved`] distinguishes the last case
    ///   before waiting.
    /// - `Some(Err(_))` — the supervisor recorded a terminal management error.
    ///   The current implementation reports pipe/reaping incompleteness through
    ///   [`ExecutionResult::cleanup_completed`] instead, so no post-start error
    ///   is currently produced; the inner `Result` preserves that distinction
    ///   for future supervisors.
    /// - `Some(Ok(_))` — the run finished. A non-zero exit, a timeout, and a
    ///   cancellation are all `Ok`; read [`ExecutionResult::outcome`].
    pub async fn wait_for(
        &self,
        id: ShellProcessId,
    ) -> Option<Result<ExecutionResult, ShellError>> {
        crate::process::wait_for(self, id).await
    }

    /// Requests termination of one running process.
    ///
    /// Returns `true` only for the transition that wins — the call that moved
    /// the run out of [`ShellProcessState::Running`](crate::ShellProcessState).
    /// `false` means there was nothing to transition: the id is unknown, or
    /// the run is already terminating or finished. Either way the request is
    /// asynchronous: `true` does not mean the child is gone, only that the
    /// supervisor has been told to run the termination sequence. Await
    /// [`wait_for`](Self::wait_for) to observe the outcome.
    pub async fn stop(&self, id: ShellProcessId) -> bool {
        crate::process::stop(&self.0, id).await
    }

    /// Requests termination of all running processes and waits for cleanup.
    ///
    /// Returns how many runs this call transitioned out of
    /// [`ShellProcessState::Running`](crate::ShellProcessState) — not how many
    /// processes existed and not how many children were killed. A run already
    /// terminating when the call started is waited for but not counted.
    ///
    /// Unlike [`stop`](Self::stop) this does not return until every entry in
    /// the table has reached a terminal state, so it can take as long as the
    /// configured [`terminate_grace`](ShellConfig::terminate_grace) plus
    /// [`drain_deadline`](ShellConfig::drain_deadline).
    /// [`ShellPlugin::cleanup`](crate::ShellPlugin) bounds it with
    /// [`shutdown_grace`](ShellConfig::shutdown_grace).
    pub async fn stop_all(&self) -> usize {
        crate::process::stop_all(&self.0).await
    }

    /// Reports this platform's process-tree cleanup guarantee.
    ///
    /// This is an associated function because the guarantee is a compile-time
    /// property of the target platform, independent of executor configuration.
    #[must_use]
    pub const fn tree_guarantee() -> ProcessTreeGuarantee {
        #[cfg(unix)]
        {
            ProcessTreeGuarantee::Group
        }
        #[cfg(not(unix))]
        {
            ProcessTreeGuarantee::DirectChildOnly
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

    /// Opens and validates the directory that a child will use.
    ///
    /// On Linux the procfs descriptor path resolves the opened object, so a
    /// rename or symlink exchange cannot replace it between validation and
    /// spawn. The caller must retain the handle and use `fchdir` in the child.
    /// Other platforms require trusted directory ancestry through spawn.
    pub(crate) fn pin_working_dir(&self, dir: &Path) -> Result<ValidatedWorkingDir, ShellError> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(dir)
                .map_err(|io_err| {
                    ShellError::InvalidWorkingDir(format!(
                        "cannot open directory {}: {io_err}",
                        dir.display()
                    ))
                })?;
            let path = if self.0.config.sandbox {
                let descriptor_path =
                    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
                // Procfs reports the physical path of the opened object.
                // Canonicalizing its link would walk that pathname again and
                // allow a replacement symlink to authorize a different object.
                let observed = std::fs::read_link(&descriptor_path).map_err(|io_err| {
                    ShellError::InvalidWorkingDir(format!(
                        "cannot resolve opened directory {}: {io_err}",
                        dir.display()
                    ))
                })?;
                self.validate_resolved_working_dir(&observed)?
            } else {
                dir.to_path_buf()
            };
            Ok(ValidatedWorkingDir { path, directory })
        }
        #[cfg(not(target_os = "linux"))]
        {
            if !dir.is_dir() {
                return Err(ShellError::InvalidWorkingDir(format!(
                    "not a directory: {}",
                    dir.display()
                )));
            }
            let path = if self.0.config.sandbox {
                let canonical = dir.canonicalize().map_err(|io_err| {
                    ShellError::InvalidWorkingDir(format!(
                        "cannot canonicalize {}: {io_err}",
                        dir.display()
                    ))
                })?;
                self.validate_resolved_working_dir(&canonical)?
            } else {
                dir.to_path_buf()
            };
            Ok(ValidatedWorkingDir { path })
        }
    }

    /// Checks an already resolved directory path without walking it again.
    fn validate_resolved_working_dir(&self, dir: &Path) -> Result<PathBuf, ShellError> {
        for allowed in &self.0.canonical_allowed_dirs {
            if dir.starts_with(allowed) {
                return Ok(dir.to_path_buf());
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

/// Command words whose argument is itself a command the split cannot see.
const INDIRECT_EXECUTION_WORDS: [&str; 4] = ["eval", "exec", "source", "."];

/// Returns the name of the first construct that defeats
/// [`split_command_chain`], or `None` when the command decomposes cleanly.
///
/// Used only to downgrade an auto-allow to a confirmation. Deliberately
/// **not** a shell parser: it recognises the shapes
/// that can carry a whole second command past the operator split, and leaves
/// everything else alone.
fn opaque_construct(command: &str) -> Option<&'static str> {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < len {
        let byte = bytes[i];

        // The shell removes backslash-newline before parsing, including in
        // double quotes. That can assemble an otherwise hidden substitution
        // or command word, so it must not be auto-allowed.
        if byte == b'\\' && !in_single_quote && i + 1 < len {
            if bytes[i + 1] == b'\n' {
                return Some("line continuation");
            }
            i += 2;
            continue;
        }

        if byte == b'\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if byte == b'"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        // Single quotes are literal in `sh`; everything below expands inside
        // double quotes, except process substitution which needs to be bare.
        if in_single_quote {
            i += 1;
            continue;
        }

        // A comment can contain quote bytes that the shell never parses.
        // Reject it before those bytes can hide a later newline separator.
        if byte == b'#'
            && !in_double_quote
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b' ' | b'\t' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')'
                ))
        {
            return Some("shell comment");
        }

        // Newlines separate commands just like semicolons, but the operator
        // split does not decompose them. Quoted newlines remain literal text.
        if byte == b'\n' && !in_double_quote {
            return Some("newline separator");
        }

        if byte == b'$' && i + 1 < len && bytes[i + 1] == b'(' {
            return Some(if i + 2 < len && bytes[i + 2] == b'(' {
                "arithmetic expansion"
            } else {
                "command substitution"
            });
        }
        if byte == b'`' {
            return Some("backtick substitution");
        }
        if !in_double_quote && (byte == b'<' || byte == b'>') && i + 1 < len && bytes[i + 1] == b'('
        {
            return Some("process substitution");
        }

        i += 1;
    }

    if in_single_quote || in_double_quote {
        return Some("unbalanced quote");
    }

    for segment in split_command_chain(command) {
        // Conservatively reject an indirect-execution word anywhere in a
        // segment. That catches shell control forms and assignment prefixes
        // (`if ... eval`, `PAYLOAD=x eval ...`) that a first-word check misses.
        // Quoted occurrences retain their quote byte and do not match.
        if segment
            .split_whitespace()
            .any(|word| INDIRECT_EXECUTION_WORDS.contains(&word))
        {
            return Some("indirect execution");
        }
        // A leading redirection displaces the command word, so the allow
        // pattern was matched against the redirection rather than against the
        // program that will run.
        if first_word(segment).is_some_and(|word| word.starts_with(['<', '>'])) {
            return Some("displaced command word");
        }
    }

    None
}

/// Returns the first whitespace-delimited word of a command segment.
fn first_word(segment: &str) -> Option<&str> {
    segment.split_whitespace().next()
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

    fn managed_permission_executor() -> ShellExecutor {
        ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into()])
                .with_denied_commands(vec!["echo forbidden*".into()])
                .disable_sandbox()
                .disable_cache(),
        )
    }

    async fn assert_next_managed_run_is_first(executor: &ShellExecutor, request: ShellRequest) {
        assert!(executor.processes().is_empty());
        let ShellStartResponse::Started(process) = executor
            .start(request)
            .await
            .expect("allowed command should start")
        else {
            panic!("allowed command should not require confirmation");
        };
        assert_eq!(process.id(), ShellProcessId(1));
        let result = process.wait().await.unwrap().unwrap();
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn managed_start_denies_before_allocating_a_run() {
        let executor = managed_permission_executor();
        let response = executor.start(ShellRequest::new("echo forbidden")).await;
        assert!(matches!(
            response,
            Err(ShellError::PermissionDenied(command)) if command == "echo forbidden"
        ));
        assert_next_managed_run_is_first(&executor, ShellRequest::new("echo accepted")).await;
    }

    #[tokio::test]
    async fn managed_confirmed_start_denies_before_allocating_a_run() {
        let executor = managed_permission_executor();
        let response = executor
            .start_confirmed(ShellRequest::new("echo forbidden"))
            .await;
        assert!(matches!(
            response,
            Err(ShellError::PermissionDenied(command)) if command == "echo forbidden"
        ));
        assert_next_managed_run_is_first(&executor, ShellRequest::new("echo accepted")).await;
    }

    #[tokio::test]
    async fn managed_start_confirmation_preserves_request_without_allocating_a_run() {
        use crate::process::{ChannelSink, ShellEvent};
        use std::num::NonZeroUsize;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let executor = managed_permission_executor();
        let directory = tempfile::tempdir().unwrap();
        let observations = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&observations);
        let cancel = ShellCancel::from_fn(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>()
        });
        let (sink, mut events) = ChannelSink::new(NonZeroUsize::new(8).unwrap());
        let response = executor
            .start(
                ShellRequest::new("printf unlisted")
                    .with_timeout(77)
                    .with_working_dir(directory.path())
                    .with_cancel(cancel)
                    .with_sink(ShellSinkHandle::new(sink)),
            )
            .await
            .expect("confirmation is not an error");
        let ShellStartResponse::ConfirmationRequired(request) = response else {
            panic!("unlisted command should require confirmation");
        };
        assert_eq!(request.command, "printf unlisted");
        assert_eq!(request.timeout_secs, Some(77));
        assert_eq!(request.working_dir.as_deref(), Some(directory.path()));
        assert!(request.cancel.is_some());
        assert!(request.sink.is_some());
        assert!(executor.processes().is_empty());
        assert_eq!(observations.load(Ordering::SeqCst), 0);

        let process = executor.start_confirmed(request).await.unwrap();
        assert_eq!(process.id(), ShellProcessId(1));
        let result = process.wait().await.unwrap().unwrap();
        assert_eq!(result.stdout, "unlisted");
        assert!(observations.load(Ordering::SeqCst) > 0);
        assert!(matches!(
            events.recv().await,
            Some(ShellEvent::Started { process: ShellProcessId(1), command, .. })
                if command == "printf unlisted"
        ));
    }

    #[tokio::test]
    async fn managed_starts_reject_outside_directories_before_allocating_a_run() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for confirmed in [false, true] {
            let executor = ShellExecutor::new(
                ShellConfig::new()
                    .with_allowed_commands(vec!["echo *".into()])
                    .with_allowed_dirs(vec![allowed.path().into()])
                    .disable_cache(),
            );
            let request = ShellRequest::new("echo escaped").with_working_dir(outside.path());
            let response = if confirmed {
                executor.start_confirmed(request).await.map(|_| ())
            } else {
                executor.start(request).await.map(|_| ())
            };
            assert!(matches!(response, Err(ShellError::InvalidWorkingDir(_))));
            assert_next_managed_run_is_first(
                &executor,
                ShellRequest::new("echo accepted").with_working_dir(allowed.path()),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn execute_requires_confirmation_for_line_continuation() {
        assert_execute_requires_confirmation("echo $\\\n(printf HIDDEN_COMMAND_MARKER)").await;
    }

    #[tokio::test]
    async fn start_requires_confirmation_for_line_continuation() {
        assert_start_requires_confirmation("echo $\\\n(printf HIDDEN_COMMAND_MARKER)").await;
    }

    #[tokio::test]
    async fn execute_requires_confirmation_for_newline_separator() {
        assert_execute_requires_confirmation("echo ok\nprintf HIDDEN_COMMAND_MARKER").await;
    }

    #[tokio::test]
    async fn start_requires_confirmation_for_newline_separator() {
        assert_start_requires_confirmation("echo ok\nprintf HIDDEN_COMMAND_MARKER").await;
    }

    #[tokio::test]
    async fn comment_quotes_cannot_hide_newline_separators() {
        let command = "echo ok # '\nprintf HIDDEN_COMMAND_MARKER # '";
        assert_execute_requires_confirmation(command).await;
        assert_start_requires_confirmation(command).await;
    }

    #[test]
    fn literal_hashes_remain_auto_allowed() {
        for command in [
            "echo before#after",
            "echo '# comment'",
            "echo \"# comment\"",
            "echo \\#",
        ] {
            assert_eq!(
                echo_allow_executor().check_permission(command),
                ShellPermission::Allow,
                "{command:?} contains a literal hash"
            );
        }
    }

    #[test]
    fn double_quoted_line_continuation_requires_confirmation() {
        assert_eq!(
            echo_allow_executor().check_permission("echo \"$\\\n(printf hidden)\""),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn quoted_literal_newlines_remain_auto_allowed() {
        for command in [
            "echo 'first\nsecond'",
            "echo \"first\nsecond\"",
            "echo 'first\\\nsecond'",
        ] {
            assert_eq!(
                echo_allow_executor().check_permission(command),
                ShellPermission::Allow,
                "{command:?} contains only literal newlines"
            );
        }
    }

    async fn assert_execute_requires_confirmation(command: &str) {
        let executor = echo_allow_executor();
        let response = executor
            .execute(ShellRequest::new(command))
            .await
            .expect("permission evaluation should succeed");
        let ShellResponse::ConfirmationRequired(request) = response else {
            panic!("expected confirmation, got {response:?}");
        };
        assert_eq!(request.command, command);
        assert!(executor.processes().is_empty());
    }

    async fn assert_start_requires_confirmation(command: &str) {
        let executor = echo_allow_executor();
        let response = executor
            .start(ShellRequest::new(command))
            .await
            .expect("permission evaluation should succeed");
        let ShellStartResponse::ConfirmationRequired(request) = response else {
            if let ShellStartResponse::Started(process) = response {
                let result = process.wait().await;
                panic!("expected confirmation, got a process with result {result:?}");
            }
            unreachable!();
        };
        assert_eq!(request.command, command);
        assert!(executor.processes().is_empty());
    }

    #[test]
    fn environment_values_are_redacted_from_debug() {
        use polaris_system::param::inspect::Inspection;

        let secret = "test-credential-never-log-1873";
        let ordinary_value = "an-ordinary-value-still-private";
        let config = ShellConfig::new()
            .with_env_var("SERVICE_TOKEN", secret)
            .with_env_var("GREETING", ordinary_value)
            .disable_cache()
            .disable_sandbox();
        let executor = ShellExecutor::new(config.clone());
        let plugin = crate::ShellPlugin::new(config.clone());
        let Inspection::Text { value, .. } = Inspection::debug(&executor) else {
            panic!("executor inspection should render text");
        };

        for rendered in [
            format!("{config:?}"),
            format!("{config:#?}"),
            format!("{executor:?}"),
            format!("{executor:#?}"),
            format!("{plugin:?}"),
            format!("{plugin:#?}"),
            value,
        ] {
            assert!(
                !rendered.contains(secret),
                "credential leaked through Debug"
            );
            assert!(
                !rendered.contains(ordinary_value),
                "environment value leaked through Debug"
            );
            assert!(rendered.contains("SERVICE_TOKEN"));
            assert!(rendered.contains("GREETING"));
        }
    }

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

    #[cfg(target_os = "linux")]
    #[test]
    fn resolved_directory_paths_are_not_resolved_again() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        let moved = root.path().join("moved");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_dirs(vec![allowed.clone()])
                .disable_cache(),
        );

        // Replacing a name reported by procfs must not make the outside
        // directory object held by this handle look allowed.
        let opened = std::fs::File::open(&outside).unwrap();
        let observed = std::fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd())).unwrap();
        std::fs::rename(&outside, &moved).unwrap();
        symlink(&allowed, &outside).unwrap();
        assert!(matches!(
            executor.validate_resolved_working_dir(&observed),
            Err(ShellError::InvalidWorkingDir(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn replacing_validated_directory_does_not_redirect_the_child() {
        use std::os::unix::fs::symlink;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().expect("temporary directory");
        let allowed = root.path().join("allowed");
        let leaf = allowed.join("leaf");
        let retained = allowed.join("retained");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&leaf).expect("create allowed directory");
        std::fs::create_dir(&outside).expect("create outside directory");

        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["pwd *".into()])
                .with_allowed_dirs(vec![allowed])
                .disable_cache(),
        );
        let cancelled_leaf = leaf.clone();
        let cancelled_retained = retained.clone();
        let swapped = AtomicBool::new(false);
        let cancel = ShellCancel::from_fn(move || {
            // The cancellation factory runs after validation and before spawn.
            if !swapped.swap(true, Ordering::SeqCst) {
                std::fs::rename(&cancelled_leaf, &cancelled_retained)
                    .expect("move validated directory");
                symlink(&outside, &cancelled_leaf).expect("replace path with outside symlink");
            }
            std::future::pending::<()>()
        });

        let response = executor
            .execute(
                ShellRequest::new("pwd -P")
                    .with_working_dir(leaf)
                    .with_cancel(cancel),
            )
            .await
            .expect("start in retained directory");
        let result = expect_executed(response);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), retained.to_string_lossy());
    }

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

    // -- Fail-closed auto-allow tests --

    /// Allows literal echo and comparison commands while denying removal.
    fn echo_allow_executor() -> ShellExecutor {
        ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into(), "diff *".into(), "cat *".into()])
                .with_denied_commands(vec!["rm *".into()])
                .disable_sandbox(),
        )
    }

    #[test]
    fn command_substitution_is_not_auto_allowed() {
        // The outer `echo *` pattern must not authorize a nested command
        // substitution that the operator split cannot evaluate separately.
        assert_eq!(
            echo_allow_executor().check_permission("echo $(rm -rf \"$TARGET\")"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn backtick_substitution_is_not_auto_allowed() {
        assert_eq!(
            echo_allow_executor().check_permission("echo `rm -rf /tmp/x`"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn arithmetic_expansion_is_not_auto_allowed() {
        assert_eq!(
            echo_allow_executor().check_permission("echo $((1 + 1))"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn process_substitution_is_not_auto_allowed() {
        assert_eq!(
            echo_allow_executor().check_permission("diff <(echo a) <(echo b)"),
            ShellPermission::Confirm
        );
        assert_eq!(
            echo_allow_executor().check_permission("echo hi >(cat)"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn indirect_execution_words_are_not_auto_allowed() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );
        for command in [
            "eval rm -rf /",
            "exec rm -rf /",
            "source ./x.sh",
            ". ./x.sh",
        ] {
            assert_eq!(
                executor.check_permission(command),
                ShellPermission::Confirm,
                "{command} should require confirmation"
            );
        }
    }

    #[test]
    fn indirect_execution_word_inside_a_chain_is_caught() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );
        assert_eq!(
            executor.check_permission("echo hi && eval \"$PAYLOAD\""),
            ShellPermission::Confirm
        );
        assert_eq!(
            executor.check_permission("PAYLOAD=hi eval \"$PAYLOAD\""),
            ShellPermission::Confirm
        );
        assert_eq!(
            executor.check_permission("if true; then eval \"$PAYLOAD\"; fi"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn displaced_command_word_is_not_auto_allowed() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["*".into()])
                .disable_sandbox(),
        );
        assert_eq!(
            executor.check_permission(">out.txt echo hi"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn unbalanced_quote_is_not_auto_allowed() {
        assert_eq!(
            echo_allow_executor().check_permission("echo 'hi"),
            ShellPermission::Confirm
        );
        assert_eq!(
            echo_allow_executor().check_permission("echo \"hi"),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn deny_still_wins_over_the_fail_closed_downgrade() {
        // A denied component must stay `Deny`, not soften to `Confirm`.
        assert_eq!(
            echo_allow_executor().check_permission("echo $(date) && rm -rf /"),
            ShellPermission::Deny
        );
    }

    #[test]
    fn clean_decomposition_is_still_auto_allowed() {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(vec!["echo *".into(), "ls*".into()])
                .disable_sandbox(),
        );
        assert_eq!(
            executor.check_permission("echo hi && ls"),
            ShellPermission::Allow
        );
        assert_eq!(
            executor.check_permission("echo foo 2>&1"),
            ShellPermission::Allow
        );
    }

    #[test]
    fn single_quoted_substitution_is_inert_and_still_auto_allowed() {
        // `sh` does not expand inside single quotes, so this really is just
        // text and downgrading it would be a false positive.
        assert_eq!(
            echo_allow_executor().check_permission("echo '$(x)'"),
            ShellPermission::Allow
        );
        assert_eq!(
            echo_allow_executor().check_permission("echo '`x`'"),
            ShellPermission::Allow
        );
    }

    #[test]
    fn escaped_substitution_is_inert_and_still_auto_allowed() {
        assert_eq!(
            echo_allow_executor().check_permission("echo \\$(x)"),
            ShellPermission::Allow
        );
    }

    #[test]
    fn double_quoted_substitution_still_expands_and_is_downgraded() {
        assert_eq!(
            echo_allow_executor().check_permission("echo \"$(rm -rf /tmp/x)\""),
            ShellPermission::Confirm
        );
    }

    #[test]
    fn confirm_default_is_unchanged_by_the_downgrade() {
        // Nothing matched an allow pattern, so the downgrade never runs.
        let executor = ShellExecutor::new(ShellConfig::new());
        assert_eq!(
            executor.check_permission("echo $(rm -rf /)"),
            ShellPermission::Confirm
        );
    }

    #[tokio::test]
    async fn substitution_reaches_confirmation_instead_of_running() {
        let executor = echo_allow_executor();
        let response = executor
            .execute(ShellRequest::new("echo $(echo nested)"))
            .await
            .expect("should not error");
        assert!(
            matches!(response, ShellResponse::ConfirmationRequired(_)),
            "expected ConfirmationRequired, got: {response:?}"
        );
    }

    // -- Foreground result ownership --

    #[test]
    fn foreground_result_reports_a_consumed_result_as_an_error() {
        let id = ShellProcessId(7);
        let error = foreground_result(id, None).expect_err("a missing result must be an error");
        assert!(
            matches!(error, ShellError::ResultConsumed(consumed) if consumed == id),
            "expected ResultConsumed, got: {error:?}"
        );
    }

    #[test]
    fn foreground_result_passes_a_present_result_through() {
        let id = ShellProcessId(9);
        let result =
            foreground_result(id, Some(Ok(ExecutionResult::cancelled_before_spawn(id, 0))))
                .expect("a present Ok must pass through");
        assert_eq!(result.process, id);
    }

    #[tokio::test]
    async fn foreground_runs_are_reserved_against_outside_waiters() {
        let executor = auto_allow_executor();
        let observer = executor.clone();
        // Races `execute`: finds the run while it is live and tries to take
        // its terminal result out from under the foreground caller.
        let watcher = tokio::spawn(async move {
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(status) = observer.processes().into_iter().next() {
                        return status;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the foreground run should become discoverable");
            let refused = observer.wait_for(status.id).await.is_none();
            (status.reserved, refused)
        });

        let result = expect_executed(
            executor
                .execute(ShellRequest::new("sleep 0.2; printf foreground"))
                .await
                .expect("foreground execution should succeed"),
        );

        assert_eq!(result.stdout, "foreground");
        let (reserved, refused) = watcher.await.expect("watcher should not panic");
        assert!(reserved, "a foreground run must be marked reserved");
        assert!(
            refused,
            "wait_for must refuse a reserved run rather than consume it"
        );
    }

    #[tokio::test]
    async fn started_runs_are_not_reserved() {
        let executor = auto_allow_executor();
        let process = executor
            .start_confirmed(ShellRequest::new("true"))
            .await
            .expect("should start");
        let id = process.detach();
        let status = executor.process(id).expect("run should be discoverable");
        assert!(!status.reserved);
        let result = executor
            .wait_for(id)
            .await
            .expect("an unreserved result is available to the executor")
            .expect("run should succeed");
        assert_eq!(result.exit_code, Some(0));
    }
}
