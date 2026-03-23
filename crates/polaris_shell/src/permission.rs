//! Permission model for shell command execution.

/// Permission level for a shell command.
///
/// Determined by evaluating the command against the configured
/// `allowed_commands` and `denied_commands` patterns in
/// [`ShellConfig`](crate::ShellConfig).
///
/// Evaluation order (deny always wins):
/// 1. `denied_commands` match → [`Deny`](Self::Deny)
/// 2. `allowed_commands` match → [`Allow`](Self::Allow)
/// 3. Otherwise → [`Confirm`](Self::Confirm)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellPermission {
    /// Execute without asking — the command matches an allowed pattern.
    Allow,

    /// Requires caller to obtain user confirmation before execution.
    /// This is the default when no pattern matches.
    Confirm,

    /// Reject execution entirely — the command matches a denied pattern.
    Deny,
}
