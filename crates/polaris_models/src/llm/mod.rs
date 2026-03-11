//! LLM (Large Language Model) generation capabilities.
//!
//! This module provides the core traits and types for text generation
//! with LLMs, including support for:
//!
//! - Text generation with tool calling
//! - Structured outputs
//! - Multi-modal inputs (images, audio, documents)

mod builder;
mod error;
mod model;
mod provider;
mod types;

pub use builder::{Empty, LlmRequestBuilder, Ready};
pub use error::{ExtractionError, GenerationError};
pub use model::Llm;
pub use provider::LlmProvider;
pub use types::{
    AssistantBlock, AudioBlock, AudioMediaType, DocumentBlock, DocumentMediaType, DocumentSource,
    ImageBlock, ImageMediaType, LlmRequest, LlmResponse, Message, ReasoningBlock, TextBlock,
    ToolCall, ToolChoice, ToolDefinition, ToolFunction, ToolResult, ToolResultContent,
    ToolResultStatus, Usage, UserBlock,
};
