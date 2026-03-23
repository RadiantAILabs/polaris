//! File tools plugin.

use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};
use std::path::PathBuf;

/// Configuration for file tools with sandboxed working directory.
#[derive(Debug, Clone)]
pub struct FileToolsConfig {
    /// Working directory for sandboxed file operations.
    pub working_dir: PathBuf,
}

impl FileToolsConfig {
    /// Creates a new configuration with the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
        }
    }

    /// Resolves a path relative to the working directory.
    /// Returns `None` if the path escapes the sandbox.
    pub fn resolve_path(&self, path: &str) -> Option<PathBuf> {
        let resolved = self.working_dir.join(path);

        match resolved.canonicalize() {
            Ok(canonical) => {
                let working_dir = self.working_dir.canonicalize().ok()?;
                canonical.starts_with(&working_dir).then_some(canonical)
            }
            Err(_) => {
                // If path does not exist, walk up parents
                let mut current = resolved.as_path();
                while let Some(parent) = current.parent() {
                    if let Ok(parent_canonical) = parent.canonicalize() {
                        // Verify parent is within sandbox
                        let working_dir = self.working_dir.canonicalize().ok()?;
                        return parent_canonical
                            .starts_with(&working_dir)
                            .then_some(resolved);
                    }
                    current = parent;
                }
                None
            }
        }
    }
}

/// File operation tools for the sandboxed working directory.
pub struct FileTools {
    /// Configuration for path resolution.
    config: FileToolsConfig,
}

impl FileTools {
    /// Creates a new `FileTools` with the given configuration.
    pub fn new(config: FileToolsConfig) -> Self {
        Self { config }
    }
}

#[toolset]
impl FileTools {
    /// List files in a directory.
    #[tool]
    async fn list_files(
        &self,
        /// Directory path (relative to working directory).
        path: String,
    ) -> Result<String, ToolError> {
        let resolved = self
            .config
            .resolve_path(&path)
            .ok_or_else(|| ToolError::execution_error(format!("path '{path}' escapes sandbox")))?;

        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|err| ToolError::execution_error(err.to_string()))?;

        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await.transpose() {
            if let Ok(entry) = entry {
                files.push(entry.file_name().to_string_lossy().to_string());
            }
        }

        Ok(if files.is_empty() {
            "(empty directory)".to_string()
        } else {
            files.join("\n")
        })
    }

    /// Read the contents of a file.
    #[tool]
    async fn read_file(
        &self,
        /// File path (relative to working directory).
        path: String,
    ) -> Result<String, ToolError> {
        let resolved = self
            .config
            .resolve_path(&path)
            .ok_or_else(|| ToolError::execution_error(format!("path '{path}' escapes sandbox")))?;

        tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| ToolError::execution_error(err.to_string()))
    }

    /// Write content to a file.
    #[tool]
    async fn write_file(
        &self,
        /// File path (relative to working directory).
        path: String,
        /// Content to write.
        content: String,
    ) -> Result<String, ToolError> {
        let resolved = self
            .config
            .resolve_path(&path)
            .ok_or_else(|| ToolError::execution_error(format!("path '{path}' escapes sandbox")))?;

        tokio::fs::write(&resolved, &content)
            .await
            .map_err(|err| ToolError::execution_error(err.to_string()))?;

        Ok(format!("Wrote to {}", resolved.display()))
    }
}

/// Plugin that registers [`FileTools`] with the [`ToolRegistry`].
pub struct FileToolsPlugin {
    /// Configuration for the file tools.
    config: FileToolsConfig,
}

impl FileToolsPlugin {
    /// Creates a new `FileToolsPlugin` with the given configuration.
    pub fn new(config: FileToolsConfig) -> Self {
        Self { config }
    }
}

impl Plugin for FileToolsPlugin {
    const ID: &'static str = "examples::file_tools";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before FileToolsPlugin");
        registry.register_toolset(FileTools::new(self.config.clone()));
        registry
            .set_permission("write_file", ToolPermission::Confirm)
            .expect("write_file should be registered by FileTools toolset");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn resolve_path_within_sandbox() {
        let config = FileToolsConfig::new(env::temp_dir());
        assert!(config.resolve_path("test.txt").is_some());
    }

    #[test]
    fn resolve_path_nested_nonexistent() {
        let temp_dir = env::temp_dir();
        let config = FileToolsConfig::new(&temp_dir);
        // Should work even if neither new_dir nor new_file.txt exist
        let resolved = config.resolve_path("new_dir/new_file.txt");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().starts_with(&temp_dir));
    }

    #[test]
    fn resolve_path_escape_blocked() {
        let config = FileToolsConfig::new(env::current_dir().unwrap());

        assert!(config.resolve_path("../../../etc/passwd").is_none());
        assert!(config.resolve_path("../../..").is_none());
        assert!(config.resolve_path("/etc/passwd").is_none());
    }
}
