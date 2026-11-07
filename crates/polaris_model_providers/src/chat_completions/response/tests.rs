//! Tests of the response reader.

use super::*;
use serde_json::json;

#[test]
fn finish_reasons_map_to_stop_reasons() {
    let cases = [
        (Some("stop"), false, StopReason::EndTurn),
        (Some("stop"), true, StopReason::ToolUse),
        (Some("end_turn"), false, StopReason::EndTurn),
        (Some("tool_calls"), true, StopReason::ToolUse),
        (Some("function_call"), true, StopReason::ToolUse),
        (Some("length"), false, StopReason::MaxOutputTokens),
        (Some("length"), true, StopReason::MaxOutputTokens),
        (Some("max_tokens"), false, StopReason::MaxOutputTokens),
        (Some("content_filter"), false, StopReason::ContentFilter),
        (None, false, StopReason::EndTurn),
        (None, true, StopReason::ToolUse),
        (Some("eos"), false, StopReason::Other("eos".to_owned())),
    ];
    for (reason, tools, expected) in cases {
        assert_eq!(stop_reason(reason, tools), expected, "{reason:?} {tools}");
    }
}

#[test]
fn usage_takes_the_cached_part_out_of_the_prompt_count() {
    let usage = parse_usage(&json!({
        "prompt_tokens": 100,
        "completion_tokens": 20,
        "total_tokens": 120,
        "prompt_tokens_details": { "cached_tokens": 40 },
        "completion_tokens_details": { "reasoning_tokens": 7 },
    }));
    assert_eq!(
        usage,
        Usage {
            input_tokens: Some(60),
            output_tokens: Some(20),
            total_tokens: Some(120),
            cache_read_tokens: Some(40),
            cache_creation_tokens: None,
            reasoning_output_tokens: Some(7),
        }
    );
}

#[test]
fn usage_reads_the_cache_fields_deepseek_uses() {
    let usage = parse_usage(&json!({
        "prompt_tokens": 50,
        "completion_tokens": 10,
        "prompt_cache_hit_tokens": 32,
        "prompt_cache_miss_tokens": 18,
    }));
    assert_eq!(usage.input_tokens, Some(18));
    assert_eq!(usage.cache_read_tokens, Some(32));
    assert_eq!(
        usage.total_tokens,
        Some(60),
        "the total is the sum when absent"
    );
}

#[test]
fn usage_reads_the_input_output_names_and_leaves_out_what_is_missing() {
    let usage = parse_usage(&json!({ "input_tokens": 9, "output_tokens": 4 }));
    assert_eq!(usage.input_tokens, Some(9));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.cache_read_tokens, None);
    assert_eq!(usage.reasoning_output_tokens, None);

    assert_eq!(parse_usage(&json!({})), Usage::default());
    // A cached count larger than the prompt cannot go below zero.
    let odd = parse_usage(&json!({
        "prompt_tokens": 5,
        "prompt_tokens_details": { "cached_tokens": 9 },
    }));
    assert_eq!(odd.input_tokens, Some(0));
    assert_eq!(odd.total_tokens, None, "no total without both counts");
}

#[test]
fn error_messages_are_read_from_the_shapes_servers_use() {
    assert_eq!(
        error_message(&json!({ "error": { "message": "bad", "code": 1 } })).as_deref(),
        Some("bad")
    );
    assert_eq!(
        error_message(&json!({ "error": "plain" })).as_deref(),
        Some("plain")
    );
    assert_eq!(
        error_message(&json!({ "error": { "code": 7 } })).as_deref(),
        Some(r#"{"code":7}"#),
        "an error object with no message is shown whole"
    );
    assert_eq!(
        error_message(&json!({ "message": "m" })).as_deref(),
        Some("m")
    );
    assert_eq!(
        error_message(&json!({ "detail": "d" })).as_deref(),
        Some("d")
    );
    assert_eq!(error_message(&json!({ "error": null })), None);
    assert_eq!(error_message(&json!({ "choices": [] })), None);
}

#[test]
fn reasoning_prefers_reasoning_content_and_ignores_empty_text() {
    let both = json!({ "reasoning_content": "a", "reasoning": "b" });
    assert_eq!(reasoning_text(&both), Some("a"));
    let empty_first = json!({ "reasoning_content": "", "reasoning": "b" });
    assert_eq!(reasoning_text(&empty_first), Some("b"));
    assert_eq!(reasoning_text(&json!({ "reasoning_content": null })), None);
    assert_eq!(reasoning_text(&json!({ "content": "x" })), None);
}

#[test]
fn a_response_with_no_choices_is_invalid_unless_it_carries_an_error() {
    let error = parse_response(&json!({ "choices": [] })).expect_err("no choices");
    assert!(
        matches!(error, GenerationError::InvalidResponse(_)),
        "{error:?}"
    );
    let error = parse_response(&json!({ "id": "x" })).expect_err("no choices");
    assert!(
        matches!(error, GenerationError::InvalidResponse(_)),
        "{error:?}"
    );
    let error = parse_response(&json!({ "error": { "message": "down" } })).expect_err("error");
    assert!(
        matches!(&error, GenerationError::Provider { message, .. } if message == "down"),
        "{error:?}"
    );
}

#[test]
fn the_choice_with_index_zero_is_read_even_when_it_is_not_first() {
    let response = parse_response(&json!({
        "choices": [
            { "index": 1, "message": { "content": "second" } },
            { "index": 0, "message": { "content": "first" } },
        ],
    }))
    .expect("the response should parse");
    assert_eq!(response.text(), "first");
}

#[test]
fn a_choice_without_a_message_is_invalid() {
    let error = parse_response(&json!({ "choices": [{ "finish_reason": "stop" }] }))
        .expect_err("no message");
    assert!(
        matches!(error, GenerationError::InvalidResponse(_)),
        "{error:?}"
    );
}

#[test]
fn empty_arguments_mean_no_arguments() {
    for arguments in [json!(""), json!("  "), json!(null)] {
        let response = parse_response(&json!({
            "choices": [{
                "message": { "tool_calls": [{ "id": "c", "function": { "name": "ping", "arguments": arguments } }] },
                "finish_reason": "tool_calls",
            }],
        }))
        .expect("the response should parse");
        assert_eq!(response.tool_calls()[0].function.arguments, json!({}));
    }
}

#[test]
fn calls_without_ids_get_ids_made_from_the_response_id_and_their_position() {
    let response = parse_response(&json!({
        "id": "resp-9",
        "choices": [{
            "message": { "tool_calls": [
                { "function": { "name": "a", "arguments": "{}" } },
                { "function": { "name": "b", "arguments": "{}" } },
            ] },
        }],
    }))
    .expect("the response should parse");
    let ids: Vec<&str> = response
        .tool_calls()
        .into_iter()
        .map(|call| call.id.as_str())
        .collect();
    assert_eq!(ids, ["resp-9_call_0", "resp-9_call_1"]);
}
