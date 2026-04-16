use polaris_tools::{toolset, ToolError};
use std::path::PathBuf;

#[derive(Clone)]
struct WorkingDir(PathBuf);

struct FileTools {
    root: String,
}

#[toolset]
impl FileTools {
    #[tool]
    /// Toolset method with a context param.
    async fn resolve(
        &self,
        #[context] cwd: WorkingDir,
        /// The path to resolve.
        path: String,
    ) -> Result<String, ToolError> {
        Ok(format!("{}/{}/{}", self.root, cwd.0.display(), path))
    }
}

fn main() {}
