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
