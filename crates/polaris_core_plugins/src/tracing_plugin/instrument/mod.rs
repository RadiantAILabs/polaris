//! Span-emitting decorators and middleware.
//!
//! Each submodule wraps a Polaris primitive — an `LlmProvider`, a `Tool`, or
//! a graph node — to emit `tracing` spans following the OpenTelemetry
//! `GenAI` and Polaris graph conventions. The wrappers are independent:
//! [`llm`] (and the shared [`genai_content`] serializer it depends on)
//! instruments LLM providers, [`tool`] instruments tool execution, and
//! [`graph`] instruments graph node execution.

pub(super) mod genai_content;
pub(super) mod graph;
pub(super) mod llm;
pub(super) mod tool;
