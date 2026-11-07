//! Lenient parsing for Responses API servers other than `OpenAI`.
//!
//! `async_openai` parses events into types that require every field `OpenAI`
//! sends. Other servers that implement the Responses API leave some of those
//! fields out, or send event types `async_openai` does not know, and strict
//! parsing then fails the whole request.
//!
//! This module receives each event as raw JSON, skips the event types the
//! stream adapter does not use, fills in the missing fields the adapter does
//! not rely on (or can recover), and only then parses the typed event. A field
//! the adapter needs and cannot recover still fails with a parse error.

use async_openai::error::OpenAIError;
use async_openai::types::responses::{Response, ResponseStreamEvent};
use futures_core::Stream;
use polaris_models::llm::GenerationError;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Event types the stream adapter converts. Every other event type is
/// skipped before parsing, so its shape cannot fail the stream.
const HANDLED_EVENT_TYPES: &[&str] = &[
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_item.added",
    "response.output_item.done",
    "response.function_call_arguments.delta",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.reasoning_summary_text.delta",
    "response.refusal.delta",
    "response.refusal.done",
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
];

/// Parses a non-streaming Responses API result, filling in fields that
/// servers other than `OpenAI` may leave out.
pub(crate) fn parse_response(mut raw: Value) -> Result<Response, GenerationError> {
    fill_response(&mut raw, None);
    Ok(serde_json::from_value(raw)?)
}

/// Tracks which output item each event belongs to, for events that leave out
/// `output_index`.
#[derive(Debug, Default)]
struct OutputIndexes {
    /// `output_index` of each item, by item ID.
    by_item_id: HashMap<String, u64>,
    /// Number of items added so far.
    items_added: u64,
    /// `output_index` of the most recent item.
    last: u64,
}

impl OutputIndexes {
    /// Returns the `output_index` to use for `event`, which has none.
    fn recover(&mut self, event_type: &str, event: &Map<String, Value>) -> u64 {
        if event_type == "response.output_item.added" {
            return self.items_added;
        }
        let item_id = event
            .get("item_id")
            .or_else(|| event.get("item").and_then(|item| item.get("id")))
            .and_then(Value::as_str);
        item_id
            .and_then(|id| self.by_item_id.get(id).copied())
            .unwrap_or(self.last)
    }

    /// Records the `output_index` an event was given.
    fn record(&mut self, event_type: &str, event: &Map<String, Value>, index: u64) {
        if event_type == "response.output_item.added" {
            self.items_added = self.items_added.max(index + 1);
            if let Some(id) = event
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
            {
                self.by_item_id.insert(id.to_owned(), index);
            }
        }
        self.last = index;
    }
}

/// Converts one raw stream event into a typed event.
///
/// Returns `Ok(None)` for event types the adapter does not use.
fn parse_event(
    mut raw: Value,
    indexes: &mut OutputIndexes,
) -> Result<Option<ResponseStreamEvent>, GenerationError> {
    let Some(event) = raw.as_object_mut() else {
        return Err(GenerationError::InvalidResponse(format!(
            "stream event is not a JSON object: {raw}"
        )));
    };
    let Some(event_type) = event.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(GenerationError::InvalidResponse(
            "stream event has no `type` field".to_owned(),
        ));
    };
    if !HANDLED_EVENT_TYPES.contains(&event_type.as_str()) {
        tracing::debug!(event_type, "skipping unused Responses stream event");
        return Ok(None);
    }

    // The adapter does not read `sequence_number` or `item_id`.
    event.entry("sequence_number").or_insert(json!(0));
    if needs_item_id(&event_type) {
        event.entry("item_id").or_insert(json!(""));
    }
    if needs_output_index(&event_type) {
        let index = match event.get("output_index").and_then(Value::as_u64) {
            Some(index) => index,
            None => {
                let index = indexes.recover(&event_type, event);
                event.insert("output_index".to_owned(), json!(index));
                index
            }
        };
        indexes.record(&event_type, event, index);
    }
    if event_type.starts_with("response.content_part")
        || event_type == "response.output_text.delta"
        || event_type.starts_with("response.refusal")
    {
        event.entry("content_index").or_insert(json!(0));
    }
    if event_type.starts_with("response.reasoning_summary") {
        event.entry("summary_index").or_insert(json!(0));
    }
    if event_type.starts_with("response.refusal") {
        let field = if event_type.ends_with("delta") {
            "delta"
        } else {
            "refusal"
        };
        event.entry(field).or_insert(json!(""));
    }
    if event_type == "error" {
        event.entry("message").or_insert(json!("unknown error"));
    }
    if let Some(item) = event.get_mut("item") {
        fill_item(item);
    }
    if let Some(part) = event.get_mut("part") {
        fill_content(part);
    }
    if let Some(response) = event.get_mut("response") {
        fill_response(response, status_for_event(&event_type));
    }

    Ok(Some(serde_json::from_value(raw)?))
}

/// Whether events of `event_type` carry `item_id`.
fn needs_item_id(event_type: &str) -> bool {
    event_type.starts_with("response.content_part")
        || event_type.starts_with("response.output_text")
        || event_type.starts_with("response.function_call_arguments")
        || event_type.starts_with("response.reasoning_summary")
        || event_type.starts_with("response.refusal")
}

/// Whether events of `event_type` carry `output_index`.
fn needs_output_index(event_type: &str) -> bool {
    needs_item_id(event_type) || event_type.starts_with("response.output_item")
}

/// The response status implied by a terminal event, used when the server
/// leaves `status` out.
fn status_for_event(event_type: &str) -> Option<&'static str> {
    match event_type {
        "response.completed" => Some("completed"),
        "response.incomplete" => Some("incomplete"),
        "response.failed" => Some("failed"),
        _ => None,
    }
}

/// Fills in missing fields of a response object.
fn fill_response(response: &mut Value, status: Option<&str>) {
    let Some(fields) = response.as_object_mut() else {
        return;
    };
    fields.entry("id").or_insert(json!(""));
    fields.entry("object").or_insert(json!("response"));
    fields.entry("created_at").or_insert(json!(0));
    fields.entry("model").or_insert(json!(""));
    fields
        .entry("status")
        .or_insert(json!(status.unwrap_or("completed")));
    let output = fields.entry("output").or_insert(json!([]));
    if let Some(items) = output.as_array_mut() {
        items.iter_mut().for_each(fill_item);
    }
    if let Some(usage) = fields.get_mut("usage") {
        fill_usage(usage);
    }
}

/// Fills in missing fields of an output item.
fn fill_item(item: &mut Value) {
    let Some(fields) = item.as_object_mut() else {
        return;
    };
    match fields.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            // An empty `name` or `call_id` is resolved, or rejected, by the
            // stream adapter when the item is done.
            fields.entry("name").or_insert(json!(""));
            fields.entry("call_id").or_insert(json!(""));
            fields.entry("arguments").or_insert(json!(""));
        }
        Some("message") => {
            fields.entry("id").or_insert(json!(""));
            fields.entry("role").or_insert(json!("assistant"));
            fields.entry("status").or_insert(json!("completed"));
            let content = fields.entry("content").or_insert(json!([]));
            if let Some(parts) = content.as_array_mut() {
                parts.iter_mut().for_each(fill_content);
            }
        }
        Some("reasoning") => {
            fields.entry("id").or_insert(json!(""));
            fields.entry("summary").or_insert(json!([]));
        }
        _ => {}
    }
}

/// Fills in missing fields of a content part.
fn fill_content(part: &mut Value) {
    let Some(fields) = part.as_object_mut() else {
        return;
    };
    match fields.get("type").and_then(Value::as_str) {
        Some("output_text") => {
            fields.entry("text").or_insert(json!(""));
            fields.entry("annotations").or_insert(json!([]));
        }
        Some("refusal") => {
            fields.entry("refusal").or_insert(json!(""));
        }
        _ => {}
    }
}

/// Fills in missing token counts. A missing count is reported as zero.
fn fill_usage(usage: &mut Value) {
    let Some(fields) = usage.as_object_mut() else {
        return;
    };
    let input = fields.get("input_tokens").and_then(Value::as_u64);
    let output = fields.get("output_tokens").and_then(Value::as_u64);
    fields.entry("input_tokens").or_insert(json!(0));
    fields.entry("output_tokens").or_insert(json!(0));
    fields
        .entry("total_tokens")
        .or_insert(json!(input.unwrap_or(0) + output.unwrap_or(0)));
    for (details, count) in [
        ("input_tokens_details", "cached_tokens"),
        ("output_tokens_details", "reasoning_tokens"),
    ] {
        let entry = fields.entry(details).or_insert(json!({}));
        if entry.is_null() {
            *entry = json!({});
        }
        if let Some(detail_fields) = entry.as_object_mut() {
            detail_fields.entry(count).or_insert(json!(0));
        }
    }
}

/// Wraps a stream of raw JSON events and yields the typed events the stream
/// adapter uses. See the [module documentation](self).
pub(crate) struct LenientEvents<S> {
    inner: S,
    indexes: OutputIndexes,
}

impl<S> LenientEvents<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            indexes: OutputIndexes::default(),
        }
    }
}

impl<S> Stream for LenientEvents<S>
where
    S: Stream<Item = Result<Value, OpenAIError>> + Unpin,
{
    type Item = Result<ResponseStreamEvent, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(raw))) => match parse_event(raw, &mut this.indexes) {
                    Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                    Ok(None) => {}
                    Err(err) => return Poll::Ready(Some(Err(err))),
                },
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Some(Err(super::provider::convert_error(err))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses::{OutputContent, OutputItem};

    fn parse(events: Vec<Value>) -> Vec<Option<ResponseStreamEvent>> {
        let mut indexes = OutputIndexes::default();
        events
            .into_iter()
            .map(|raw| parse_event(raw, &mut indexes).expect("event should parse"))
            .collect()
    }

    #[test]
    fn skips_unknown_and_unused_event_types() {
        let parsed = parse(vec![
            json!({ "type": "response.venice.custom", "anything": [1, 2] }),
            json!({ "type": "response.created", "response": { "bad": true } }),
            json!({ "type": "response.output_text.done" }),
        ]);
        assert!(parsed.iter().all(Option::is_none));
    }

    #[test]
    fn fills_missing_annotations_and_indexes() {
        let parsed = parse(vec![
            json!({
                "type": "response.output_item.added",
                "item": { "type": "message", "id": "msg_1", "content": [] },
            }),
            json!({
                "type": "response.content_part.added",
                "item_id": "msg_1",
                "part": { "type": "output_text", "text": "" },
            }),
            json!({ "type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hi" }),
        ]);

        let Some(ResponseStreamEvent::ResponseContentPartAdded(part)) = &parsed[1] else {
            panic!("expected a content part event, got {:?}", parsed[1]);
        };
        assert_eq!(part.output_index, 0);
        assert_eq!(part.content_index, 0);
        let OutputContent::OutputText(text) = &part.part else {
            panic!("expected output text, got {:?}", part.part);
        };
        assert!(text.annotations.is_empty());

        let Some(ResponseStreamEvent::ResponseOutputTextDelta(delta)) = &parsed[2] else {
            panic!("expected a text delta, got {:?}", parsed[2]);
        };
        assert_eq!(delta.delta, "Hi");
        assert_eq!(delta.output_index, 0);
    }

    #[test]
    fn recovers_output_index_from_item_id() {
        let parsed = parse(vec![
            json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "fc_a", "name": "a", "call_id": "call_a" },
            }),
            json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "id": "fc_b", "name": "b", "call_id": "call_b" },
            }),
            json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "{}" }),
            json!({ "type": "response.function_call_arguments.delta", "delta": "{}" }),
        ]);

        let indexes: Vec<u32> = parsed
            .iter()
            .map(|event| match event {
                Some(ResponseStreamEvent::ResponseOutputItemAdded(added)) => added.output_index,
                Some(ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(delta)) => {
                    delta.output_index
                }
                other => panic!("unexpected event {other:?}"),
            })
            .collect();
        // The event without an item ID belongs to the most recent item.
        assert_eq!(indexes, vec![0, 1, 0, 0]);
    }

    #[test]
    fn keeps_server_output_index() {
        let parsed = parse(vec![json!({
            "type": "response.output_item.added",
            "output_index": 3,
            "item": { "type": "function_call", "name": "a", "call_id": "call_a" },
        })]);
        let Some(ResponseStreamEvent::ResponseOutputItemAdded(added)) = &parsed[0] else {
            panic!("expected an added item, got {:?}", parsed[0]);
        };
        assert_eq!(added.output_index, 3);
        let OutputItem::FunctionCall(call) = &added.item else {
            panic!("expected a function call, got {:?}", added.item);
        };
        assert_eq!(call.arguments, "");
    }

    #[test]
    fn fills_completed_response_and_usage_details() {
        let parsed = parse(vec![json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "venice-model",
                "output": [],
                "usage": { "input_tokens": 10, "output_tokens": 5, "input_tokens_details": null },
            },
        })]);
        let Some(ResponseStreamEvent::ResponseCompleted(done)) = &parsed[0] else {
            panic!("expected a completed event, got {:?}", parsed[0]);
        };
        assert_eq!(done.response.id, "resp_1");
        let usage = done.response.usage.as_ref().expect("usage is kept");
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.input_tokens_details.cached_tokens, 0);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 0);
    }

    #[test]
    fn rejects_event_without_type() {
        let mut indexes = OutputIndexes::default();
        let err = parse_event(json!({ "delta": "x" }), &mut indexes)
            .expect_err("an event without a type is rejected");
        assert!(
            matches!(err, GenerationError::InvalidResponse(_)),
            "{err:?}"
        );
    }

    #[test]
    fn still_rejects_missing_required_field() {
        // A text delta without its text cannot be recovered.
        let mut indexes = OutputIndexes::default();
        let err = parse_event(
            json!({ "type": "response.output_text.delta", "item_id": "msg_1" }),
            &mut indexes,
        )
        .expect_err("a delta without text is rejected");
        assert!(matches!(err, GenerationError::Json(_)), "{err:?}");
    }

    #[test]
    fn parse_response_fills_missing_fields() {
        let response = parse_response(json!({
            "id": "resp_1",
            "model": "m",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "Hello" }],
            }],
        }))
        .expect("response should parse");
        assert_eq!(response.output.len(), 1);
    }
}
