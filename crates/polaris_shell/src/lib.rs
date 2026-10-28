//! Shell command execution for Polaris agents.
//!
//! This crate provides shell command execution with a safety-first permission
//! model, directory sandboxing, bounded incremental output, and managed
//! process-tree cleanup.
//!
//! # Architecture
//!
//! - [`ShellExecutor`] — execution engine with permission checking (global resource)
//! - [`ShellTools`] — LLM-facing tool wrapper (registered with [`ToolRegistry`](polaris_tools::ToolRegistry))
//! - [`ShellPlugin`] — plugin that registers both of the above
//!
//! # Permission Model
//!
//! Every command is matched against the glob patterns in [`ShellConfig`],
//! evaluated in precedence order:
//!
//! | Priority | Matches | Result |
//! |----------|---------|--------|
//! | 1 (highest) | `denied_commands` | [`ShellPermission::Deny`] — command is rejected |
//! | 2 | `allowed_commands` | [`ShellPermission::Allow`] — command runs immediately |
//! | 3 (default) | neither list | [`ShellPermission::Confirm`] — requires user approval |
//!
//! **Compound commands** (joined by `&&`, `||`, `;`, `|`, or `&`) are split
//! into individual subcommands, each evaluated separately. The most
//! restrictive result applies to the entire pipeline. This prevents bypassing
//! deny rules by chaining a denied command after an allowed one
//! (e.g., `echo foo && rm -rf /`).
//!
//! When a command resolves to `Confirm`, [`ShellExecutor::execute`] returns
//! [`ShellResponse::ConfirmationRequired`]. After obtaining user approval,
//! call [`ShellExecutor::execute_confirmed`] with the preserved
//! [`ShellRequest`] to run it.
//!
//! ## Auto-allow is fail-closed
//!
//! Permission evaluation operates on the **literal command string** — it
//! splits on shell operators (`&&`, `||`, `;`, `|`, `&`) but does **not**
//! parse shell expressions, while `sh -c` understands the whole grammar. A
//! construct that the split cannot account for can therefore hide a second
//! command inside one that matches an allow pattern.
//!
//! Rather than parse a shell grammar, the auto-allow path is fail-closed: a
//! command that would otherwise be [`ShellPermission::Allow`] is downgraded to
//! [`ShellPermission::Confirm`] when it contains a command substitution
//! (`$(…)`, backticks), an arithmetic expansion, a process substitution
//! (`<(…)` / `>(…)`), an indirect execution word (`eval`, `exec`, `source`,
//! `.`), a redirection displacing the command word, or an unbalanced quote.
//!
//! For example, with `allowed_commands: ["echo *"]`:
//! - `echo hello` → **allowed** (matches pattern)
//! - `echo hello && rm -rf /` → **denied** (splits; `rm -rf /` checked separately)
//! - `echo $(rm -rf /)` → **confirm** (a substitution is never auto-allowed)
//! - `echo '$(x)'` → **allowed** (single quotes are literal in `sh`)
//!
//! The downgrade only ever moves a command from `Allow` to `Confirm`; `Deny`
//! still wins over everything, and a command that decomposes cleanly is
//! unaffected. Pattern matching remains a **convenience for reducing
//! confirmation noise**, not a security boundary — the actual security
//! boundary is Layer 4, runtime user confirmation for unrecognized commands.
//!
//! ## Child environment
//!
//! Children do **not** inherit the server's environment by default. Each one
//! is spawned with a cleared environment holding a trusted `PATH`
//! ([`ShellConfig::path`]), whichever of `HOME`, `USER`, `LOGNAME`, `SHELL`,
//! `LANG`, `TERM`, and `TMPDIR` the host has set, then
//! [`ShellConfig::env_allowlist`], then [`ShellConfig::env`]. That keeps host
//! credentials out of the captured or streamed output of an allowed command
//! such as `env`. [`ShellConfig::with_inherit_env`]`(true)` restores full
//! inheritance when a command genuinely needs the server's environment.
//!
//! ## Streaming and process management
//!
//! [`ShellExecutor::start`] returns a stable [`ShellProcess`] handle while
//! preserving the same permission checks used by foreground execution. Output
//! is read from distinct stdout and stderr pipes, retained within configured
//! bounds, and optionally delivered through a non-blocking [`ShellSink`].
//! Each [`ChannelSink`] accepts one run, including across its clones. Reuse
//! fails before spawning. Dropping all producers drains an incomplete event
//! stream and then closes it without synthesizing `Finished`.
//! Overflow files contain the complete byte stream when creation succeeds and
//! the per-stream spill quota ([`ShellConfig::max_overflow_bytes`]) is not
//! reached.
//!
//! ## Who owns a running child
//!
//! A [`ShellProcess`] is the owner of its run's foreground lifetime:
//!
//! - **Dropping the handle requests a stop.** If the future awaiting a command
//!   is dropped — a graph node's `max_duration` elapsing, a cancelled request,
//!   an early `return` — the child is terminated rather than left running.
//!   That includes the handle held internally by [`ShellExecutor::execute`].
//! - **[`ShellProcess::detach`] is the explicit opt-out.** It returns the run's
//!   [`ShellProcessId`] and leaves the child running under executor ownership,
//!   reachable through [`ShellExecutor::process`], [`ShellExecutor::stop`], and
//!   [`ShellExecutor::wait_for`].
//! - **[`ShellPlugin`] cleanup is the backstop.** At server shutdown it stops
//!   and reaps whatever detached runs are still live.
//!
//! On Unix, cancellation, timeout, and plugin shutdown signal every descendant
//! that remains in the spawned process group. Normal leader exit forcibly
//! stops remaining group members before reaping the leader. A program that deliberately
//! leaves the group with `setsid` or `setpgid` is outside that guarantee. On
//! Windows, version one is direct-child-only and descendants may survive. This
//! crate uses pipes rather than a PTY, so interactive terminal behavior is not
//! provided.
//!
//! # Permission Architecture (4 Layers)
//!
//! Shell permissions are designed as **4 complementary layers** that compose
//! together. Each layer narrows the allowed surface — none can override a
//! restriction imposed by a layer above it.
//!
//! ## Layer 1: Plugin-Level Config ([`ShellConfig`])
//!
//! Set at build time when constructing [`ShellPlugin`]. Defines the baseline
//! allow/deny patterns and directory sandbox for the entire server. This is
//! the coarsest and most restrictive layer.
//!
//! ```
//! use polaris_shell::{ShellConfig, ShellPlugin};
//!
//! let plugin = ShellPlugin::new(
//!     ShellConfig::new()
//!         .with_allowed_commands(vec!["cargo *".into(), "git *".into()])
//!         .with_denied_commands(vec!["rm -rf *".into(), "sudo *".into()])
//!         .with_allowed_dirs(vec!["/home/user/project".into()])
//! );
//! ```
//!
//! ## Layer 2: Agent-Level Resource (per-agent permissions)
//!
//! Individual agents can have narrower permissions via agent-local resources.
//! For example, a code review agent might only allow read-only commands, while
//! a build agent allows `cargo` and `make`. This is implemented by accessing
//! [`ShellExecutor`] through `Res<ShellExecutor>` in systems and applying
//! additional permission checks at the agent level.
//!
//! ## Layer 3: External Config File
//!
//! An external config file can provide user-editable permission overrides without
//! recompiling. This layer would load at startup and merge with the plugin-level
//! config, following the same deny-wins precedence.
//!
//! ## Layer 4: Runtime Confirmation via `UserIO`
//!
//! When a command's permission is [`ShellPermission::Confirm`], the system
//! presents the command to the user for approval before execution. This uses
//! the `UserIO` resource from `polaris_core_plugins`:
//!
//! 1. System calls `executor.execute(request)` → gets [`ShellResponse::ConfirmationRequired`]`(request)`
//! 2. System calls `user_io.send("Allow command: `rm -rf`? (y/n)")` + `user_io.receive()`
//! 3. If approved, system calls `executor.execute_confirmed(request)`
//!
//! # See Also
//!
//! For the full framework guide and integration patterns,
//! see the [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

pub mod error;
pub mod executor;
pub mod permission;
pub mod plugin;
pub mod process;
pub mod tools;

// Re-export core types at crate root.
pub use error::ShellError;
pub use executor::{
    ExecutionResult, ShellConfig, ShellExecutor, ShellRequest, ShellResponse, ShellStartResponse,
};
pub use permission::ShellPermission;
pub use plugin::ShellPlugin;
pub use process::{
    ChannelSink, ProcessTreeGuarantee, ShellCancel, ShellEvent, ShellEventDelivery,
    ShellEventReceiver, ShellOutcome, ShellProcess, ShellProcessId, ShellProcessState,
    ShellProcessStatus, ShellSink, ShellSinkHandle, ShellStream,
};
pub use tools::{ShellToolResponse, ShellTools};
