//! Tests of the stream adapter, fed with `data` payloads directly.

use super::*;
use futures_util::StreamExt;
use polaris_models::llm::StopReason;
use serde_json::json;

type Events = Vec<Result<StreamEvent, GenerationError>>;

async fn run(payloads: Vec<Value>, done: bool) -> Events {
    let mut items: Vec<Result<String, GenerationError>> = payloads
        .into_iter()
        .map(|payload| Ok(payload.to_string()))
        .collect();
    if done {
        items.push(Ok("[DONE]".to_owned()));
    }
    run_items(items).await
}

async fn run_items(items: Vec<Result<String, GenerationError>>) -> Events {
    ChatStreamAdapter::new(futures_util::stream::iter(items))
        .collect()
        .await
}

fn delta(delta: Value) -> Value {
    json!({ "id": "r1", "choices": [{ "index": 0, "delta": delta }] })
}

fn finish(reason: &str) -> Value {
    json!({ "id": "r1", "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] })
}

fn ok(events: Events) -> Vec<StreamEvent> {
    events
        .into_iter()
        .map(|event| event.expect("no error expected"))
        .collect()
}

fn starts(events: &[StreamEvent]) -> Vec<(u32, ContentBlockStartData)> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ContentBlockStart { index, block } => Some((*index, block.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_stream_with_only_the_done_marker_ends_as_an_empty_turn() {
    let events = ok(run(Vec::new(), true).await);
    assert_eq!(
        events,
        vec![StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            id: None,
            model: None,
        }]
    );
}

#[tokio::test]
async fn an_empty_stream_with_no_done_marker_fails() {
    let events = run(Vec::new(), false).await;
    assert!(
        matches!(
            events.as_slice(),
            [Err(GenerationError::InvalidResponse(_))]
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn text_before_a_tool_call_is_closed_before_the_call_starts() {
    let events = ok(run(
        vec![
            delta(json!({ "content": "Looking." })),
            delta(json!({
                "tool_calls": [{ "index": 0, "id": "c1", "function": { "name": "get_weather", "arguments": "{}" } }],
            })),
            finish("tool_calls"),
        ],
        true,
    )
    .await);
    assert_eq!(
        events,
        vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("Looking.".to_owned())
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::ToolCall {
                    id: "c1".to_owned(),
                    call_id: Some("c1".to_owned()),
                    name: "get_weather".to_owned(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{}".to_owned()
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                id: Some("r1".to_owned()),
                model: None,
            },
        ]
    );
}

#[tokio::test]
async fn reasoning_that_comes_back_after_text_opens_a_new_block() {
    let events = ok(run(
        vec![
            delta(json!({ "reasoning": "a" })),
            delta(json!({ "content": "b" })),
            delta(json!({ "reasoning": "c" })),
            finish("stop"),
        ],
        true,
    )
    .await);
    let kinds: Vec<ContentBlockStartData> = starts(&events)
        .into_iter()
        .map(|(_, block)| block)
        .collect();
    assert_eq!(
        kinds,
        [
            ContentBlockStartData::Reasoning,
            ContentBlockStartData::Text,
            ContentBlockStartData::Reasoning,
        ]
    );
}

#[tokio::test]
async fn calls_with_ids_but_no_index_are_told_apart_by_id() {
    let events = ok(run(
        vec![
            delta(json!({ "tool_calls": [{ "id": "a", "function": { "name": "f", "arguments": "{\"x\":" } }] })),
            // The server repeats the id on a continuation chunk.
            delta(json!({ "tool_calls": [{ "id": "a", "function": { "arguments": "1}" } }] })),
            delta(json!({ "tool_calls": [{ "id": "b", "function": { "name": "g", "arguments": "{}" } }] })),
            finish("tool_calls"),
        ],
        true,
    )
    .await);
    let names: Vec<String> = starts(&events)
        .into_iter()
        .filter_map(|(_, block)| match block {
            ContentBlockStartData::ToolCall { id, name, .. } => Some(format!("{id}:{name}")),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["a:f", "b:g"]);
    let argument_text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall { arguments },
            } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(argument_text, "{\"x\":1}");
}

#[tokio::test]
async fn the_last_usage_chunk_wins_and_each_one_is_reported() {
    let usage_chunk = |prompt: u64, completion: u64| json!({ "choices": [], "usage": { "prompt_tokens": prompt, "completion_tokens": completion } });
    let events = ok(run(
        vec![
            delta(json!({ "content": "x" })),
            usage_chunk(10, 1),
            finish("stop"),
            usage_chunk(10, 5),
        ],
        true,
    )
    .await);
    let reported: Vec<Option<u64>> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::MessageDelta { usage } => Some(usage.output_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(reported, [Some(1), Some(5)]);
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::MessageStop { usage, .. }) if usage.output_tokens == Some(5)
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn only_the_first_choice_is_read() {
    let events = ok(run(
        vec![
            json!({ "choices": [
                { "index": 1, "delta": { "content": "other" } },
                { "index": 0, "delta": { "content": "mine" } },
            ] }),
            finish("stop"),
        ],
        true,
    )
    .await);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::Text(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "mine");
}

#[tokio::test]
async fn a_chunk_that_carries_a_message_instead_of_a_delta_is_read() {
    let events = ok(run(
        vec![
            json!({ "choices": [{ "message": { "content": "whole" }, "finish_reason": "stop" }] }),
        ],
        true,
    )
    .await);
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Text(text), .. } if text == "whole"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_chunk_that_is_not_json_ends_the_stream_with_an_error() {
    let items: Vec<Result<String, GenerationError>> = vec![
        Ok(delta(json!({ "content": "x" })).to_string()),
        Ok("{not json".to_owned()),
        Ok(delta(json!({ "content": "never read" })).to_string()),
    ];
    let events = run_items(items).await;
    let last = events.last().expect("the stream has events");
    assert!(
        matches!(last, Err(GenerationError::InvalidResponse(message)) if message.contains("not valid JSON")),
        "{events:?}"
    );
    assert!(
        events.iter().flatten().all(|event| !matches!(
            event,
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Text(text), .. } if text == "never read"
        )),
        "nothing after the error is read: {events:?}"
    );
}

#[tokio::test]
async fn empty_payloads_are_ignored_and_a_transport_error_is_passed_on() {
    let items: Vec<Result<String, GenerationError>> = vec![
        Ok("  ".to_owned()),
        Ok(delta(json!({ "content": "x" })).to_string()),
        Err(GenerationError::Http("reset".to_owned())),
    ];
    let events = run_items(items).await;
    assert!(
        matches!(events.last(), Some(Err(GenerationError::Http(message))) if message == "reset"),
        "{events:?}"
    );
}

#[tokio::test]
async fn the_response_id_and_model_are_taken_from_the_first_chunks_that_have_them() {
    let events = ok(run(
        vec![
            json!({ "choices": [{ "delta": { "content": "x" } }] }),
            json!({ "id": "late", "model": "served", "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            json!({ "id": "later", "model": "other", "choices": [] }),
        ],
        true,
    )
    .await);
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::MessageStop { id: Some(id), model: Some(model), .. })
                if id == "late" && model == "served"
        ),
        "{events:?}"
    );
}

const MIB: usize = 1024 * 1024;

fn tool_call_chunk(call: Value) -> Value {
    delta(json!({ "tool_calls": [call] }))
}

fn named_call(index: usize) -> Value {
    tool_call_chunk(
        json!({ "index": index, "id": format!("c{index}"), "function": { "name": "f" } }),
    )
}

/// Asserts that the stream ended with an `InvalidResponse` error whose message
/// has `needle`, with no `MessageStop` and no later read content.
fn assert_fails_with(events: &Events, needle: &str) {
    let (last, before) = events.split_last().expect("the stream has items");
    assert!(
        matches!(last, Err(GenerationError::InvalidResponse(message)) if message.contains(needle)),
        "{last:?}"
    );
    assert!(
        before.iter().all(
            |event| matches!(event, Ok(event) if !matches!(event, StreamEvent::MessageStop { .. }))
        ),
        "only events come before the error"
    );
    assert!(
        events.iter().flatten().all(|event| !matches!(
            event,
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Text(text), .. } if text == "never read"
        )),
        "nothing after the error is read"
    );
}

#[tokio::test]
async fn arguments_kept_before_a_tool_name_are_capped_across_events_and_calls() {
    let fragment = "x".repeat(MIB);
    // Seventeen events of 1 MiB each, for two calls that never get a name.
    let mut payloads: Vec<Value> = (0..17)
        .map(|n| tool_call_chunk(json!({ "index": n % 2, "function": { "arguments": fragment } })))
        .collect();
    payloads.push(delta(json!({ "content": "never read" })));
    let events = run(payloads, true).await;
    assert_fails_with(&events, "before the function name");
    assert_eq!(events.len(), 1, "nothing was sent before the error");
}

#[tokio::test]
async fn arguments_kept_before_a_tool_name_are_released_when_the_call_starts() {
    let fragment = "x".repeat(MIB);
    let mut payloads: Vec<Value> = (0..16)
        .map(|_| tool_call_chunk(json!({ "index": 0, "function": { "arguments": fragment } })))
        .collect();
    // Exactly 16 MiB is held when the name arrives, and is sent after the start.
    payloads.push(tool_call_chunk(
        json!({ "index": 0, "id": "a", "function": { "name": "f" } }),
    ));
    // The start released it, so another call may hold more.
    payloads.push(tool_call_chunk(
        json!({ "index": 1, "function": { "arguments": fragment } }),
    ));
    payloads.push(tool_call_chunk(
        json!({ "index": 1, "id": "b", "function": { "name": "g" } }),
    ));
    payloads.push(finish("tool_calls"));
    let events = ok(run(payloads, true).await);
    let sent: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::ToolCall { arguments },
                ..
            } => Some(arguments.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [16 * MIB, MIB]);
    assert_eq!(starts(&events).len(), 2);
    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop {
            stop_reason: StopReason::ToolUse,
            ..
        })
    ));
}

#[tokio::test]
async fn refusal_text_is_capped_across_events() {
    let fragment = "x".repeat(MIB);
    let mut payloads: Vec<Value> = (0..17)
        .map(|_| delta(json!({ "refusal": fragment })))
        .collect();
    payloads.push(delta(json!({ "content": "never read" })));
    let events = run(payloads, true).await;
    assert_fails_with(&events, "refusal");
    assert_eq!(events.len(), 1, "nothing was sent before the error");
}

#[tokio::test]
async fn refusal_text_at_the_limit_is_still_reported_as_a_refusal() {
    let fragment = "x".repeat(MIB);
    let payloads: Vec<Value> = (0..16)
        .map(|_| delta(json!({ "refusal": fragment })))
        .collect();
    let events = run(payloads, true).await;
    assert!(
        matches!(events.as_slice(), [Err(GenerationError::Refusal(text))] if text.len() == 16 * MIB),
        "the refusal is the only item"
    );
}

#[tokio::test]
async fn a_stream_may_not_open_more_tool_calls_than_the_limit() {
    let mut payloads: Vec<Value> = (0..=MAX_TOOL_SLOTS).map(named_call).collect();
    payloads.push(delta(json!({ "content": "never read" })));
    let events = run(payloads, true).await;
    assert_fails_with(&events, "more than 1024 tool calls");
    let started = events
        .iter()
        .flatten()
        .filter(|event| matches!(event, StreamEvent::ContentBlockStart { .. }))
        .count();
    assert_eq!(started, MAX_TOOL_SLOTS);
}

#[tokio::test]
async fn tool_calls_up_to_the_limit_are_accepted_and_open_calls_still_continue() {
    let mut payloads: Vec<Value> = (0..MAX_TOOL_SLOTS).map(named_call).collect();
    // A chunk for a call that is already open does not need a new slot.
    payloads.push(tool_call_chunk(
        json!({ "index": 0, "function": { "arguments": "{}" } }),
    ));
    payloads.push(finish("tool_calls"));
    let events = ok(run(payloads, true).await);
    assert_eq!(starts(&events).len(), MAX_TOOL_SLOTS);
    assert!(
        events.contains(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::ToolCall {
                arguments: "{}".to_owned()
            },
        }),
        "the continuation is sent"
    );
    assert!(matches!(
        events.last(),
        Some(StreamEvent::MessageStop { .. })
    ));
}

#[tokio::test]
async fn a_call_after_the_largest_index_fails_instead_of_overflowing() {
    let events = run(
        vec![
            tool_call_chunk(json!({ "index": u64::MAX, "id": "a", "function": { "name": "f" } })),
            // No index and a new id: the next number would be `u64::MAX + 1`.
            tool_call_chunk(json!({ "id": "b", "function": { "name": "g" } })),
            delta(json!({ "content": "never read" })),
        ],
        true,
    )
    .await;
    assert_fails_with(&events, "too large");
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(
        matches!(
            &events[0],
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall { id, .. },
            }) if id == "a"
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn the_largest_index_works_when_no_further_call_needs_a_number() {
    let events = ok(run(
        vec![
            tool_call_chunk(
                json!({ "index": u64::MAX, "id": "a", "function": { "name": "f", "arguments": "{" } }),
            ),
            // A known id continues its call, so nothing is added to the index.
            tool_call_chunk(json!({ "id": "a", "function": { "arguments": "}" } })),
            finish("tool_calls"),
        ],
        true,
    )
    .await);
    assert_eq!(starts(&events).len(), 1);
    let argument_text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::ToolCall { arguments },
                ..
            } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(argument_text, "{}");
}
