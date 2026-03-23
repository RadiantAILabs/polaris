//! Shell command execution for Polaris agents.
//!
//! This crate provides shell command execution with a safety-first permission
//! model, directory sandboxing, timeout enforcement, and output truncation.
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
//! ## Known Limitation: Shell Expression Bypass
//!
//! Permission evaluation operates on the **literal command string** — it
//! splits on shell operators (`&&`, `||`, `;`, `|`, `&`) but does **not**
//! parse shell expressions. Command substitutions (`$(...)`, backticks),
//! `eval`, `bash -c`, and similar constructs are treated as opaque text.
//!
//! For example, with `allowed_commands: ["echo *"]`:
//! - `echo hello` → **allowed** (matches pattern)
//! - `echo hello && rm -rf /` → **denied** (splits; `rm -rf /` checked separately)
//! - `echo $(rm -rf /)` → **allowed** (matches `echo *` as literal text)
//!
//! This is a deliberate design trade-off: perfectly parsing shell syntax is
//! infeasible (nested substitutions, encoded strings, `printf` tricks, etc.),
//! and heuristic detection provides a false sense of security. The pattern
//! matching layer is a **convenience for reducing confirmation noise**, not a
//! security boundary. The actual security boundary is Layer 4 — runtime user
//! confirmation for unrecognized commands.
//!
//! ## Known Limitation: Background Process Timeout
//!
//! Commands using the `&` operator (e.g., `sleep 100 &`) may leave orphaned
//! processes after timeout. The shell exits immediately when backgrounding,
//! so `kill_on_drop` only terminates the shell process, not its children.
//! Process group isolation is planned as a future enhancement.
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
//! 1. System calls `executor.execute(request)` → gets [`ShellResponse::ConfirmationRequired(request)`]
//! 2. System calls `user_io.send("Allow command: `rm -rf`? (y/n)")` + `user_io.receive()`
//! 3. If approved, system calls `executor.execute_confirmed(request)`

pub mod error;
pub mod executor;
pub mod permission;
pub mod plugin;
pub mod tools;

// Re-export core types at crate root.
pub use error::ShellError;
pub use executor::{ExecutionResult, ShellConfig, ShellExecutor, ShellRequest, ShellResponse};
pub use permission::ShellPermission;
pub use plugin::ShellPlugin;
pub use tools::{ShellToolResponse, ShellTools};
