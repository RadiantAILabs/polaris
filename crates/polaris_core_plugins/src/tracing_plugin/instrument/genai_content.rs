//! Serialization of Polaris types to `OTel` `GenAI` semantic convention JSON schemas.
//!
//! This module converts Polaris message and tool types into the JSON format
//! defined by the [`OTel` `GenAI` events specification](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-events/).
//!
//! All functions return JSON strings. They are only called when `capture_genai_content`
//! is enabled, so there is zero compute cost when content tracing is disabled.

use polaris_models::llm::{
    AssistantBlock, AudioMediaType, DocumentMediaType, ImageMediaType, Message, StopReason,
    ToolDefinition, ToolResultContent, UserBlock,
};
use serde_json::json;

/// Serializes input messages to the `OTel` `GenAI` input messages JSON format.
///
/// Each message is serialized as an object with `role` and `parts`.
pub(super) fn serialize_input_messages(messages: &[Message]) -> String {
    let msgs: Vec<serde_json::Value> = messages.iter().map(serialize_message).collect();
    serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".to_string())
}

/// Serializes output content blocks to the `OTel` `GenAI` output messages JSON format.
///
/// The output is a single-element array containing an assistant message with
/// `role`, `parts`, and `finish_reason` as required by `OutputMessage`.
pub(super) fn serialize_output_messages(
    content: &[AssistantBlock],
    finish_reason: &StopReason,
) -> String {
    let parts: Vec<serde_json::Value> = content.iter().map(serialize_assistant_block).collect();
    let msg = json!({
        "role": "assistant",
        "parts": parts,
        "finish_reason": finish_reason,
    });
    serde_json::to_string(&[msg]).unwrap_or_else(|_| "[]".to_string())
}

/// Serializes a system instruction string to the `OTel` `GenAI` format.
///
/// Returns a JSON array with a single text part.
pub(super) fn serialize_system_instructions(system: &str) -> String {
    let parts = [json!({ "type": "text", "content": system })];
    serde_json::to_string(&parts).unwrap_or_else(|_| "[]".to_string())
}

/// Serializes tool definitions to the `OTel` `GenAI` tool definitions JSON format.
///
/// Each tool is emitted as `{"type": "function", "name", "description", "parameters"}`,
/// matching the format shown in the `gen_ai.tool.definitions` semantic convention example.
pub(super) fn serialize_tool_definitions(tools: &[ToolDefinition]) -> String {
    let defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect();
    serde_json::to_string(&defs).unwrap_or_else(|_| "[]".to_string())
}

fn serialize_message(message: &Message) -> serde_json::Value {
    match message {
        Message::User { content } => {
            let parts: Vec<serde_json::Value> = content.iter().map(serialize_user_block).collect();
            json!({
                "role": "user",
                "parts": parts,
            })
        }
        Message::Assistant { content, .. } => {
            let parts: Vec<serde_json::Value> =
                content.iter().map(serialize_assistant_block).collect();
            json!({
                "role": "assistant",
                "parts": parts,
            })
        }
    }
}

fn serialize_user_block(block: &UserBlock) -> serde_json::Value {
    match block {
        UserBlock::Text(text) => {
            json!({ "type": "text", "content": text.text })
        }
        UserBlock::Image(img) => {
            // `BlobPart.content` contains raw binary and is intentionally omitted — embedding
            // base64 data in trace attributes would produce unbounded payload sizes.
            json!({
                "type": "blob",
                "modality": "image",
                "mime_type": image_mime_type(&img.media_type),
            })
        }
        UserBlock::Audio(audio) => {
            json!({
                "type": "blob",
                "modality": "audio",
                "mime_type": audio_mime_type(&audio.media_type),
            })
        }
        UserBlock::Document(doc) => {
            json!({
                "type": "blob",
                "modality": "document",
                "mime_type": document_mime_type(&doc.media_type),
            })
        }
        UserBlock::ToolResult(result) => {
            let response = match &result.content {
                ToolResultContent::Text(text) => text.clone(),
                ToolResultContent::Image(_) => "[image]".to_string(),
            };
            json!({
                "type": "tool_call_response",
                "id": result.id,
                "response": response,
            })
        }
    }
}

fn serialize_assistant_block(block: &AssistantBlock) -> serde_json::Value {
    match block {
        AssistantBlock::Text(text) => {
            json!({ "type": "text", "content": text.text })
        }
        AssistantBlock::ToolCall(call) => {
            json!({
                "type": "tool_call",
                "id": call.id,
                "name": call.function.name,
                "arguments": call.function.arguments,
            })
        }
        AssistantBlock::Reasoning(reasoning) => {
            json!({
                "type": "reasoning",
                "content": reasoning.reasoning.join(""),
            })
        }
    }
}

fn image_mime_type(media_type: &ImageMediaType) -> &'static str {
    match media_type {
        ImageMediaType::JPEG => "image/jpeg",
        ImageMediaType::PNG => "image/png",
        ImageMediaType::GIF => "image/gif",
        ImageMediaType::WEBP => "image/webp",
        ImageMediaType::HEIC => "image/heic",
        ImageMediaType::HEIF => "image/heif",
        ImageMediaType::SVG => "image/svg+xml",
    }
}

fn audio_mime_type(media_type: &AudioMediaType) -> &'static str {
    match media_type {
        AudioMediaType::WAV => "audio/wav",
        AudioMediaType::MP3 => "audio/mpeg",
        AudioMediaType::AIFF => "audio/aiff",
        AudioMediaType::AAC => "audio/aac",
        AudioMediaType::OGG => "audio/ogg",
        AudioMediaType::FLAC => "audio/flac",
    }
}

fn document_mime_type(media_type: &DocumentMediaType) -> &'static str {
    match media_type {
        DocumentMediaType::PDF => "application/pdf",
        DocumentMediaType::TXT => "text/plain",
        DocumentMediaType::HTML => "text/html",
        DocumentMediaType::MARKDOWN => "text/markdown",
        DocumentMediaType::CSV => "text/csv",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_models::llm::{
        DocumentSource, ImageBlock, ImageMediaType, ReasoningBlock, StopReason, TextBlock,
        ToolCall, ToolFunction, ToolResult, ToolResultContent, ToolResultStatus,
    };
    use serde_json::json;

    #[test]
    fn text_only_messages() {
        let messages = vec![Message::user("Hello"), Message::assistant("Hi there!")];
        let result = serialize_input_messages(&messages);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["parts"][0]["type"], "text");
        assert_eq!(parsed[0]["parts"][0]["content"], "Hello");
        assert_eq!(parsed[1]["role"], "assistant");
        assert_eq!(parsed[1]["parts"][0]["content"], "Hi there!");
    }

    #[test]
    fn tool_call_output() {
        let content = vec![AssistantBlock::ToolCall(ToolCall {
            id: "call_1".to_string(),
            call_id: None,
            function: ToolFunction {
                name: "get_weather".to_string(),
                arguments: json!({"city": "London"}),
            },
            signature: None,
            additional_params: None,
        })];
        let result = serialize_output_messages(&content, &StopReason::ToolUse);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["finish_reason"], "tool_use");
        assert_eq!(parsed[0]["parts"][0]["type"], "tool_call");
        assert_eq!(parsed[0]["parts"][0]["id"], "call_1");
        assert_eq!(parsed[0]["parts"][0]["name"], "get_weather");
        assert_eq!(parsed[0]["parts"][0]["arguments"]["city"], "London");
    }

    #[test]
    fn system_instructions() {
        let result = serialize_system_instructions("You are a helpful assistant");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["type"], "text");
        assert_eq!(parsed[0]["content"], "You are a helpful assistant");
    }

    #[test]
    fn image_blocks_omit_base64_data() {
        let messages = vec![Message::User {
            content: vec![UserBlock::Image(ImageBlock {
                data: DocumentSource::Base64("aGVsbG8=".to_string()),
                media_type: ImageMediaType::PNG,
                additional_params: None,
            })],
        }];
        let result = serialize_input_messages(&messages);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["parts"][0]["type"], "blob");
        assert_eq!(parsed[0]["parts"][0]["modality"], "image");
        assert_eq!(parsed[0]["parts"][0]["mime_type"], "image/png");
        assert!(parsed[0]["parts"][0].get("data").is_none());
    }

    #[test]
    fn tool_result_response_content() {
        let messages = vec![Message::User {
            content: vec![UserBlock::ToolResult(ToolResult {
                id: "tool_1".to_string(),
                call_id: None,
                content: ToolResultContent::Text("42 degrees".to_string()),
                status: ToolResultStatus::Success,
            })],
        }];
        let result = serialize_input_messages(&messages);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["parts"][0]["type"], "tool_call_response");
        assert_eq!(parsed[0]["parts"][0]["id"], "tool_1");
        assert_eq!(parsed[0]["parts"][0]["response"], "42 degrees");
    }

    #[test]
    fn reasoning_blocks_joined() {
        let content = vec![AssistantBlock::Reasoning(ReasoningBlock {
            id: None,
            reasoning: vec![
                "First, I need to ".to_string(),
                "think about this.".to_string(),
            ],
            signature: None,
        })];
        let result = serialize_output_messages(&content, &StopReason::EndTurn);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["finish_reason"], "end_turn");
        assert_eq!(parsed[0]["parts"][0]["type"], "reasoning");
        assert_eq!(
            parsed[0]["parts"][0]["content"],
            "First, I need to think about this."
        );
    }

    #[test]
    fn tool_definitions() {
        let tools = vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the weather for a city".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string"}
                }
            }),
        }];
        let result = serialize_tool_definitions(&tools);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["type"], "function");
        assert_eq!(parsed[0]["name"], "get_weather");
        assert_eq!(parsed[0]["description"], "Get the weather for a city");
        assert!(parsed[0]["parameters"]["properties"]["city"].is_object());
    }

    #[test]
    fn mixed_output_text_and_tool_call() {
        let content = vec![
            AssistantBlock::Text(TextBlock {
                text: "Let me check that.".to_string(),
            }),
            AssistantBlock::ToolCall(ToolCall {
                id: "call_2".to_string(),
                call_id: None,
                function: ToolFunction {
                    name: "search".to_string(),
                    arguments: json!({"query": "test"}),
                },
                signature: None,
                additional_params: None,
            }),
        ];
        let result = serialize_output_messages(&content, &StopReason::ToolUse);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed[0]["finish_reason"], "tool_use");
        assert_eq!(parsed[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["parts"][0]["type"], "text");
        assert_eq!(parsed[0]["parts"][1]["type"], "tool_call");
    }
}
