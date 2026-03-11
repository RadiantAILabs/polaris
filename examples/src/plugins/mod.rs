//! Plugins for the example agent.
//!
//! - [`TerminalIOPlugin`] — Terminal I/O provider for CLI interaction
//! - [`FileToolsPlugin`] — Sandboxed file operation tools

mod file_tools;
mod terminal_io;

pub use file_tools::{FileToolsConfig, FileToolsPlugin};
pub use terminal_io::TerminalIOPlugin;
