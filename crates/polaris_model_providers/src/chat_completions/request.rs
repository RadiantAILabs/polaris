//! Building the Chat Completions request body (Polaris -> server).

use super::{MaxTokensField, ReasoningReplay, SeenReasoningField};
use crate::schema::normalize_schema_for_strict_mode;
use polaris_models::llm::{
    AssistantBlock, DocumentSource, GenerationError, ImageBlock, ImageMediaType, LlmRequest,
    Message, ToolCall, ToolChoice, ToolDefinition, ToolResult, ToolResultContent, ToolResultStatus,
    UserBlock,
};
use serde_json::{Map, Value, json};

/// Provider settings that shape every request body.
#[derive(Debug, Clone, Default)]
pub(super) struct BodySettings {
    /// The key that carries the output token limit.
    pub(super) max_tokens_field: MaxTokensField,
    /// How earlier reasoning is sent back.
    pub(super) reasoning_replay: ReasoningReplay,
    /// The reasoning field the server used in its latest reply.
    pub(super) seen_reasoning: SeenReasoningField,
    /// Whether tool definitions carry `strict` and a normalized schema.
    pub(super) strict_tools: bool,
    /// Fields laid over the top level of every body.
    pub(super) extra_body: Option<Value>,
}

/// Builds the JSON body of one `/chat/completions` call.
pub(super) fn build_body(
    model: &str,
    request: &LlmRequest,
    stream: bool,
    settings: &BodySettings,
) -> Result<Value, GenerationError> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    let reasoning_field = settings.reasoning_replay.field(&settings.seen_reasoning);
    convert_messages(&request.messages, reasoning_field, &mut messages)?;

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(model));
    body.insert("messages".to_owned(), Value::Array(messages));

    // An empty `tools` array is rejected by servers, so none is sent.
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        let tools = tools
            .iter()
            .map(|tool| convert_tool(tool, settings.strict_tools))
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice {
        body.insert("tool_choice".to_owned(), convert_tool_choice(choice));
    }
    if let Some(schema) = &request.output_schema {
        body.insert(
            "response_format".to_owned(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "structured_output",
                    "schema": normalize_schema_for_strict_mode(schema.clone()),
                    "strict": true,
                },
            }),
        );
    }
    if let Some(limit) = request.max_output_tokens {
        body.insert(
            settings.max_tokens_field.key().to_owned(),
            json!(limit.get()),
        );
    }
    if let Some(effort) = &request.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), json!(effort.as_str()));
    }
    if stream {
        body.insert("stream".to_owned(), json!(true));
        // Without this, OpenAI-style servers send no usage in a stream.
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
    }

    match &settings.extra_body {
        None => {}
        Some(Value::Object(fields)) => {
            for (key, value) in fields {
                body.insert(key.clone(), value.clone());
            }
        }
        Some(other) => {
            return Err(GenerationError::InvalidRequest(format!(
                "the extra request body must be a JSON object, not {}",
                json_kind(other)
            )));
        }
    }
    Ok(Value::Object(body))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn convert_tool(tool: &ToolDefinition, strict_tools: bool) -> Value {
    let mut function = Map::new();
    function.insert("name".to_owned(), json!(tool.name));
    function.insert("description".to_owned(), json!(tool.description));
    if strict_tools && tool.strict {
        function.insert(
            "parameters".to_owned(),
            normalize_schema_for_strict_mode(tool.parameters.clone()),
        );
        function.insert("strict".to_owned(), json!(true));
    } else {
        function.insert("parameters".to_owned(), tool.parameters.clone());
        if strict_tools {
            function.insert("strict".to_owned(), json!(false));
        }
    }
    json!({ "type": "function", "function": Value::Object(function) })
}

fn convert_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Required => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Specific(name) => json!({ "type": "function", "function": { "name": name } }),
    }
}

fn convert_messages(
    messages: &[Message],
    reasoning_field: Option<&'static str>,
    out: &mut Vec<Value>,
) -> Result<(), GenerationError> {
    for message in messages {
        match message {
            Message::User { content } => convert_user_message(content, out)?,
            Message::Assistant { content, .. } => {
                convert_assistant_message(content, reasoning_field, out)?;
            }
        }
    }
    Ok(())
}

/// Converts a user message. Tool results become `tool` messages, which must
/// directly follow the assistant message that made the calls, so they come
/// first; any text and images that came with them follow as one user message.
fn convert_user_message(blocks: &[UserBlock], out: &mut Vec<Value>) -> Result<(), GenerationError> {
    let mut parts = Vec::new();
    let mut has_image = false;
    let mut tool_messages = Vec::new();

    for block in blocks {
        match block {
            UserBlock::Text(text) => parts.push(json!({ "type": "text", "text": text.text })),
            UserBlock::Image(image) => {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": image_data_url(image)? },
                }));
                has_image = true;
            }
            UserBlock::ToolResult(result) => tool_messages.push(convert_tool_result(result)?),
            UserBlock::Audio(_) => {
                return Err(GenerationError::UnsupportedContent(
                    "Audio content is not supported by the Chat Completions provider".to_owned(),
                ));
            }
            UserBlock::Document(_) => {
                return Err(GenerationError::UnsupportedContent(
                    "Document content is not supported by the Chat Completions provider".to_owned(),
                ));
            }
        }
    }

    out.extend(tool_messages);
    if parts.is_empty() {
        return Ok(());
    }
    // Text only is sent as a plain string, which every server reads. Images
    // need the parts form.
    let content = if has_image {
        Value::Array(parts)
    } else {
        let text: Vec<&str> = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        Value::String(text.join("\n"))
    };
    out.push(json!({ "role": "user", "content": content }));
    Ok(())
}

fn convert_tool_result(result: &ToolResult) -> Result<Value, GenerationError> {
    let text = match &result.content {
        ToolResultContent::Text(text) => text.clone(),
        ToolResultContent::Image(_) => {
            return Err(GenerationError::UnsupportedContent(
                "Image tool results are not supported by the Chat Completions provider".to_owned(),
            ));
        }
    };
    let text = match result.status {
        ToolResultStatus::Success => text,
        ToolResultStatus::Error => format!("Error: {text}"),
    };
    let id = tool_call_id(result.call_id.as_deref(), &result.id).ok_or_else(|| {
        GenerationError::InvalidRequest(
            "a tool result has no id to link it to its tool call".to_owned(),
        )
    })?;
    Ok(json!({ "role": "tool", "tool_call_id": id, "content": text }))
}

/// The id a tool call goes by on the wire: the provider call id when there is
/// one (calls that came from the Responses API have both), else the id.
fn tool_call_id<'a>(call_id: Option<&'a str>, id: &'a str) -> Option<&'a str> {
    call_id
        .filter(|call_id| !call_id.is_empty())
        .or(Some(id).filter(|id| !id.is_empty()))
}

fn convert_assistant_message(
    blocks: &[AssistantBlock],
    reasoning_field: Option<&'static str>,
    out: &mut Vec<Value>,
) -> Result<(), GenerationError> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning = String::new();

    for block in blocks {
        match block {
            AssistantBlock::Text(block) => text.push_str(&block.text),
            AssistantBlock::ToolCall(call) => tool_calls.push(convert_tool_call(call)?),
            AssistantBlock::Reasoning(block) => {
                reasoning.extend(block.reasoning.iter().map(String::as_str));
            }
        }
    }

    let reasoning_field = reasoning_field.filter(|_| !reasoning.is_empty());
    if text.is_empty() && tool_calls.is_empty() && reasoning_field.is_none() {
        return Ok(());
    }

    let mut message = Map::new();
    message.insert("role".to_owned(), json!("assistant"));
    // `content` may be null when the message carries tool calls.
    let content = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    message.insert("content".to_owned(), content);
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    if let Some(field) = reasoning_field {
        message.insert(field.to_owned(), Value::String(reasoning));
    }
    out.push(Value::Object(message));
    Ok(())
}

fn convert_tool_call(call: &ToolCall) -> Result<Value, GenerationError> {
    let id = tool_call_id(call.call_id.as_deref(), &call.id).ok_or_else(|| {
        GenerationError::InvalidRequest(format!(
            "tool call `{}` has no id for its result to refer to",
            call.function.name
        ))
    })?;
    // A call with no arguments is sent as an empty object, the form servers
    // read as "no arguments".
    let arguments = if call.function.arguments.is_null() {
        "{}".to_owned()
    } else {
        serde_json::to_string(&call.function.arguments)?
    };
    Ok(json!({
        "id": id,
        "type": "function",
        "function": { "name": call.function.name, "arguments": arguments },
    }))
}

fn image_data_url(image: &ImageBlock) -> Result<String, GenerationError> {
    let mime = match image.media_type {
        ImageMediaType::JPEG => "image/jpeg",
        ImageMediaType::PNG => "image/png",
        ImageMediaType::GIF => "image/gif",
        ImageMediaType::WEBP => "image/webp",
        ref other => {
            return Err(GenerationError::UnsupportedContent(format!(
                "Unsupported image media type for Chat Completions: {other:?}"
            )));
        }
    };
    let DocumentSource::Base64(data) = &image.data;
    Ok(format!("data:{mime};base64,{data}"))
}

#[cfg(test)]
mod tests;
