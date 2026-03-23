//! Plugin for registering shell execution tools and resources.

use crate::executor::{ShellConfig, ShellExecutor};
use crate::tools::ShellTools;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use polaris_tools::{ToolRegistry, ToolsPlugin};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Plugin that provides shell command execution capabilities.
///
/// Registers [`ShellExecutor`] as a global resource and [`ShellTools`] with the
/// [`ToolRegistry`] for LLM tool invocation.
///
/// # Dependencies
///
/// - [`ToolsPlugin`] must be added before this plugin.
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_tools::ToolsPlugin;
/// use polaris_shell::{ShellPlugin, ShellConfig};
///
/// let mut server = Server::new();
/// server.add_plugins(ToolsPlugin);
/// server.add_plugins(ShellPlugin::new(
///     ShellConfig::new()
///         .with_working_dir("/home/user/project")
///         .with_allowed_commands(vec!["cargo *".into(), "git *".into()])
///         .with_denied_commands(vec!["rm -rf *".into()])
///         .with_allowed_dirs(vec!["/home/user/project".into()])
/// ));
/// server.finish();
/// ```
#[derive(Debug)]
pub struct ShellPlugin {
    config: ShellConfig,
}

impl ShellPlugin {
    /// Creates a new plugin with the given configuration.
    #[must_use]
    pub fn new(config: ShellConfig) -> Self {
        Self { config }
    }

    /// Creates a plugin with default configuration and the given working directory.
    #[must_use]
    pub fn with_working_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            config: ShellConfig::new().with_working_dir(dir),
        }
    }
}

impl Plugin for ShellPlugin {
    const ID: &'static str = "polaris::shell";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let executor = ShellExecutor::new(self.config.clone());

        server.insert_global(executor.clone());

        // Register tools with the ToolRegistry
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before ShellPlugin");
        registry.register_toolset(ShellTools::new(executor));
    }

    fn cleanup(&self, _server: &mut Server) {
        let Some(cache_dir) = &self.config.cache_dir else {
            return;
        };

        let ttl = Duration::from_secs(self.config.overflow_ttl_secs);
        let now = SystemTime::now();

        let Ok(entries) = std::fs::read_dir(cache_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };

            // Only touch files created by write_overflow_file.
            if !name_str.starts_with("shell_stdout_") && !name_str.starts_with("shell_stderr_") {
                continue;
            }

            let Ok(metadata) = entry.metadata() else {
                continue;
            };

            let expired = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > ttl);

            if expired && let Err(err) = std::fs::remove_file(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %err,
                    "failed to remove expired overflow file"
                );
            }
        }
    }
}
