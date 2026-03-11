//! Model provider interface and registry for Polaris.
//!
//! Provides a unified interface for AI model access, decoupling consumers from
//! provider implementations.
//!
//! # Overview
//!
//! - Provider-agnostic: Consumers depend only on this crate, not specific provider plugins.
//!
//! - Modular provider plugins: Provider crates register at runtime, allowing
//!   models to be swapped via configuration without code changes.
//!
//! - Minimal dependencies: Each provider lives in a separate crate.
//!
//! # Example
//!
//! ```
//! use polaris_models::{ModelRegistry, ModelsPlugin};
//! use polaris_models::llm::LlmRequest;
//! use polaris_system::param::Res;
//! use polaris_system::system::SystemError;
//! use polaris_system::system;
//!
//! #[system]
//! async fn my_agent(registry: Res<ModelRegistry>) -> Result<String, SystemError> {
//!     let llm = registry
//!         .llm("openai/gpt-4o")
//!         .map_err(|e| SystemError::ExecutionError(e.to_string()))?;
//!
//!     let response = llm.builder()
//!         .system("You are helpful")
//!         .user("Hello!")
//!         .generate()
//!         .await
//!         .map_err(|e| SystemError::ExecutionError(e.to_string()))?;
//!
//!     Ok(response.text())
//! }
//! ```

// Self-reference so tool macros can use `polaris_models::` paths within this crate.
extern crate self as polaris_models;

pub mod error;
pub mod llm;
mod plugin;
mod registry;

pub use plugin::ModelsPlugin;
pub use registry::ModelRegistry;
