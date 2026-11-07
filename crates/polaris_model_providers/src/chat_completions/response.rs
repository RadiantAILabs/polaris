//! Reading Chat Completions responses (server -> Polaris).
//!
//! Everything is read from raw JSON. Servers differ in which fields they send:
//! a tool call may have no `id`, `arguments` may be an object instead of a
//! string, `usage` may be missing or use another field name, and reasoning
//! arrives as `reasoning_content` or `reasoning`. Fields the provider does not
//! need are never required.

use polaris_models::llm::{
    AssistantBlock, GenerationError, LlmResponse, ReasoningBlock, StopReason, TextBlock, ToolCall,
    ToolFunction, Usage,
};
use serde_json::Value;

/// Reads a non-streaming response body.
pub(super) fn parse_response(raw: &Value) -> Result<LlmResponse, GenerationError> {
    let choices = raw.get("choices").and_then(Value::as_array);
    let Some(choices) = choices.filter(|choices| !choices.is_empty()) else {
        // Some servers answer `200` with an error body.
        if let Some(message) = error_message(raw) {
            return Err(GenerationError::Provider {
                status: None,
                message,
                source: None,
            });
        }
        return Err(GenerationError::InvalidResponse(
            "the response has no choices".to_owned(),
        ));
    };
    let choice = first_choice(choices);
    let message = choice
        .get("message")
        .filter(|message| message.is_object())
        .ok_or_else(|| {
            GenerationError::InvalidResponse("the first choice has no message".to_owned())
        })?;

    let mut content = Vec::new();
    if let Some(reasoning) = reasoning_text(message) {
        content.push(AssistantBlock::Reasoning(ReasoningBlock {
            id: None,
            reasoning: vec![reasoning.to_owned()],
            signature: None,
        }));
    }
    let text = content_text(message.get("content"));
    if !text.is_empty() {
        content.push(AssistantBlock::Text(TextBlock { text }));
    }
    let response_id = raw
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let mut has_tool_calls = false;
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (position, call) in calls.iter().enumerate() {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            content.push(AssistantBlock::ToolCall(parse_tool_call(
                call,
                index,
                response_id,
            )?));
            has_tool_calls = true;
        }
    }

    if content.is_empty()
        && let Some(refusal) = message
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|refusal| !refusal.is_empty())
    {
        return Err(GenerationError::Refusal(refusal.to_owned()));
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    Ok(LlmResponse {
        content,
        usage: raw.get("usage").map(parse_usage).unwrap_or_default(),
        stop_reason: stop_reason(finish_reason, has_tool_calls),
        id: response_id.map(str::to_owned),
        model: raw
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
    })
}

/// The choice to read: the one with index 0, else the first.
pub(super) fn first_choice(choices: &[Value]) -> &Value {
    choices
        .iter()
        .find(|choice| choice.get("index").and_then(Value::as_u64) == Some(0))
        .unwrap_or(&choices[0])
}

/// The reasoning text of a message or delta: `reasoning_content`, else
/// `reasoning` (the name newer servers use). Empty text counts as none.
pub(super) fn reasoning_text(message: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"]
        .into_iter()
        .find_map(|key| {
            message
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
}

/// The text of `content`, which is a string, `null`, or a list of parts.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.as_str()),
                other => other.get("text").and_then(Value::as_str),
            })
            .collect(),
        _ => String::new(),
    }
}

fn parse_tool_call(
    call: &Value,
    index: u64,
    response_id: Option<&str>,
) -> Result<ToolCall, GenerationError> {
    let function = call.get("function").unwrap_or(&Value::Null);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            GenerationError::InvalidResponse(format!("tool call {index} has no function name"))
        })?;
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map_or_else(|| synthesized_call_id(response_id, index), str::to_owned);
    let arguments = match function.get("arguments") {
        Some(Value::String(text)) => parse_arguments(text).map_err(|err| {
            GenerationError::InvalidResponse(format!(
                "the arguments of tool call `{name}` are not valid JSON: {err}"
            ))
        })?,
        Some(arguments @ (Value::Object(_) | Value::Array(_))) => arguments.clone(),
        _ => Value::Object(serde_json::Map::new()),
    };
    Ok(ToolCall {
        call_id: Some(id.clone()),
        id,
        function: ToolFunction {
            name: name.to_owned(),
            arguments,
        },
        signature: None,
        additional_params: None,
    })
}

/// Parses tool call arguments. Empty text means no arguments.
fn parse_arguments(text: &str) -> Result<Value, serde_json::Error> {
    if text.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(text)
}

/// An id for a tool call whose server sent none. Built from the response id
/// and the call's position, so it is stable and unique within a conversation.
pub(super) fn synthesized_call_id(response_id: Option<&str>, index: u64) -> String {
    format!("{}_call_{index}", response_id.unwrap_or("chatcmpl"))
}

/// Maps a `finish_reason` to a [`StopReason`].
///
/// `length` is kept even when tool calls are present: the call may be cut off.
/// `stop` with tool calls present is treated as a tool use, because some
/// servers report `stop` when the model called a tool. A missing reason is
/// inferred from the content.
pub(super) fn stop_reason(finish_reason: Option<&str>, has_tool_calls: bool) -> StopReason {
    match finish_reason {
        Some("length" | "max_tokens") => StopReason::MaxOutputTokens,
        Some("content_filter") => StopReason::ContentFilter,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("stop" | "end_turn") | None => {
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        Some(other) => StopReason::Other(other.to_owned()),
    }
}

/// Reads a `usage` object.
///
/// `input_tokens` is the uncached count: the cached part (reported as
/// `prompt_tokens_details.cached_tokens`, or by `DeepSeek` as
/// `prompt_cache_hit_tokens`) is taken out of `prompt_tokens`, so cached tokens
/// are not billed at the full rate twice. Counts a server leaves out stay
/// `None`.
pub(super) fn parse_usage(usage: &Value) -> Usage {
    let count = |value: Option<&Value>| value.and_then(Value::as_u64);
    let prompt = count(usage.get("prompt_tokens")).or_else(|| count(usage.get("input_tokens")));
    let output =
        count(usage.get("completion_tokens")).or_else(|| count(usage.get("output_tokens")));
    let cached = count(usage.pointer("/prompt_tokens_details/cached_tokens"))
        .or_else(|| count(usage.get("prompt_cache_hit_tokens")));
    let total = count(usage.get("total_tokens")).or_else(|| match (prompt, output) {
        (Some(prompt), Some(output)) => Some(prompt.saturating_add(output)),
        _ => None,
    });
    Usage {
        input_tokens: prompt.map(|prompt| prompt.saturating_sub(cached.unwrap_or(0))),
        output_tokens: output,
        total_tokens: total,
        cache_read_tokens: cached,
        cache_creation_tokens: None,
        reasoning_output_tokens: count(
            usage.pointer("/completion_tokens_details/reasoning_tokens"),
        ),
    }
}

/// The message of an error body, in the shapes servers use:
/// `{"error": {"message": ..}}`, `{"error": ".."}`, `{"message": ..}` and
/// `{"detail": ..}`.
pub(super) fn error_message(body: &Value) -> Option<String> {
    let error = body.get("error").filter(|error| !error.is_null());
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    if let Some(error) = error {
        if let Some(message) = text(error) {
            return Some(message);
        }
        if let Some(message) = error.get("message").and_then(text) {
            return Some(message);
        }
        // An error object without a message: show it whole.
        return Some(error.to_string());
    }
    body.get("message")
        .and_then(text)
        .or_else(|| body.get("detail").and_then(text))
}

#[cfg(test)]
mod tests;
