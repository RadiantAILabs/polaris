//! Procedural macros for the Polaris tool framework.
//!
//! Provides `#[tool]` for standalone tool functions and `#[toolset]` for
//! grouped tools on impl blocks.

mod common;
mod tool_fn;
mod toolset;

use proc_macro::TokenStream;

/// Defines a standalone tool from an async function.
///
/// Generates a `Tool` impl struct with automatic JSON schema generation
/// and parameter extraction.
///
/// # Parameter Attributes
///
/// - `/// doc comment` — becomes the parameter's description in JSON schema
/// - `#[default(value)]` — makes the parameter optional with a default value
/// - `#[context]` — extracts the parameter from [`ToolContext`] instead of LLM
///   arguments; the parameter does not appear in the JSON schema and requires
///   `T: Clone`. Use `Option<T>` for optional context values.
///
/// # Example
///
/// ```
/// use polaris_tools::{tool, ToolError};
///
/// #[tool]
/// /// Search for documents.
/// async fn search(
///     /// The search query.
///     query: String,
///     /// Max results.
///     #[default(10)]
///     limit: usize,
/// ) -> Result<String, ToolError> {
///     Ok(format!("Results for: {query}"))
/// }
/// ```
#[proc_macro_attribute]
pub fn tool(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);
    tool_fn::generate_tool_fn(&input).into()
}

/// Defines a toolset from an impl block containing `#[tool]` methods.
///
/// Generates a `Toolset` impl that provides all `#[tool]` methods as
/// individual `Tool` instances for bulk registration.
///
/// # Example
///
/// ```
/// use polaris_tools::{toolset, tool, ToolError};
///
/// struct FileTools;
///
/// #[toolset]
/// impl FileTools {
///     #[tool]
///     /// List files.
///     async fn list_files(&self, path: String) -> Result<String, ToolError> {
///         Ok("files".to_string())
///     }
///
///     #[tool]
///     /// Read a file.
///     async fn read_file(&self, path: String) -> Result<String, ToolError> {
///         Ok("contents".to_string())
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn toolset(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemImpl);
    toolset::generate_toolset(&input).into()
}
