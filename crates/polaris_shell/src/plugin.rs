//! Plugin for registering shell execution tools and resources.

use crate::executor::{ShellConfig, ShellExecutor};
use crate::tools::ShellTools;
use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris_system::server::Server;
use polaris_tools::ToolRegistry;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Plugin that provides shell command execution capabilities.
///
/// Registers [`ShellExecutor`] as a global resource and a [`ShellTools`]
/// toolset against [`ToolRegistry`] so the underlying executor is reachable
/// both as a typed resource (`Res<ShellExecutor>`) and as LLM-invokable tools.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ShellExecutor`] | Global | Permission-gated shell command executor |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Shell access is exposed through the [`ShellExecutor`] global resource and the [`ShellTools`] toolset registered with [`ToolRegistry`]. |
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — owns the [`ToolRegistry`] this plugin registers
///   its toolset with. Must be added before `ShellPlugin`.
///
/// # Tools Provided
///
/// Registered as the [`ShellTools`] toolset with [`ToolRegistry`].
///
/// | Tool | Description |
/// |------|-------------|
/// | `run_command` | Runs a shell command via `sh -c` (pipes, redirects, globbing supported) and returns its output, or a confirmation-required response when the command is permission-gated. |
///
/// # Lifecycle
///
/// - **`build()`** — inserts [`ShellExecutor`] as a global resource and
///   registers the [`ShellTools`] toolset with [`ToolRegistry`].
/// - **`cleanup()`** — when a cache directory is configured, garbage-
///   collects expired shell overflow files (`shell_stdout_*` /
///   `shell_stderr_*`) older than the configured overflow TTL. A no-op
///   when no cache directory is set.
/// - No `ready()` override; registers no tick schedules.
///
/// # Extends
///
/// - [`ToolRegistry`] (from [`ToolsPlugin`]) — registers the [`ShellTools`]
///   toolset so an LLM agent can invoke shell commands as the
///   `run_command` tool.
///
/// # Example
///
/// ```no_run
/// # async fn example() {
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
/// server.finish().await;
/// # }
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

    fn access(&self) -> PluginAccess {
        // Declares that this plugin extends the `ToolRegistry` capability rather than
        // naming `ToolsPlugin`: the resolver orders this plugin after whichever plugin
        // provides `ToolRegistry`, verifies the contract version, and guarantees it is
        // present — so the `get_resource_mut` below cannot actually fail in a resolved
        // server. `build` keeps a `&mut Server` parameter (rather than an `Extends`
        // build-param) because it also inserts the `ShellExecutor` global, which needs
        // mutable server access alongside the registry.
        PluginAccess::new()
            .extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        let executor = ShellExecutor::new(self.config.clone());

        server.insert_global(executor.clone());

        // Register tools with the ToolRegistry. The capability resolver guarantees a
        // provider built first, so this lookup is infallible in a resolved server.
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolRegistry capability must be provided before ShellPlugin");
        registry.register_toolset(ShellTools::new(executor));
    }

    async fn cleanup(&self, _server: &mut Server) {
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
