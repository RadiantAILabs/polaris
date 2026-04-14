//! Agent configuration.

use polaris::system::resource::LocalResource;

/// Configuration for the `ReAct` agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model identifier (e.g., "anthropic/claude-sonnet-4-6").
    pub model_id: String,
}

impl AgentConfig {
    /// Creates a new agent configuration.
    #[must_use]
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
        }
    }
}

impl LocalResource for AgentConfig {}
