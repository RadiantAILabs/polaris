//! Permission model for tool execution.
//!
//! Provides [`ToolPermission`], the generic permission level that any tool can
//! declare via [`Tool::permission`](crate::Tool::permission).

/// Permission level for tool invocation.
///
/// Tools declare their default permission via [`Tool::permission`](crate::Tool::permission).
/// The executor or middleware uses this to determine whether to allow execution,
/// prompt for confirmation, or deny the invocation.
///
/// This is a tool-level default. Runtime systems may further adjust permissions
/// (e.g., per-agent config, command-level pattern matching in shell tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolPermission {
    /// Execute without user confirmation.
    ///
    /// This is the default for most tools.
    #[default]
    Allow,

    /// Requires caller to obtain user confirmation before execution.
    Confirm,

    /// Reject execution entirely.
    Deny,
}

impl std::fmt::Display for ToolPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow => write!(f, "allow"),
            Self::Confirm => write!(f, "confirm"),
            Self::Deny => write!(f, "deny"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_allow() {
        assert_eq!(ToolPermission::default(), ToolPermission::Allow);
    }

    #[test]
    fn display_variants() {
        assert_eq!(ToolPermission::Allow.to_string(), "allow");
        assert_eq!(ToolPermission::Confirm.to_string(), "confirm");
        assert_eq!(ToolPermission::Deny.to_string(), "deny");
    }
}
