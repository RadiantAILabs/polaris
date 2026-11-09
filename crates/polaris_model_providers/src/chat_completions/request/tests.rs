//! Tests of the request body builder.

use super::*;
use polaris_models::llm::{AudioMediaType, DocumentMediaType, ReasoningBlock};

fn body(request: &LlmRequest) -> Value {
    build_body("m", request, false, &BodySettings::default()).expect("the body should build")
}

fn messages(request: &LlmRequest) -> Vec<Value> {
    body(request)["messages"]
        .as_array()
        .expect("messages is an array")
        .clone()
}

fn call(id: &str, call_id: Option<&str>) -> ToolCall {
    let mut call = ToolCall::new(id, "get_weather", json!({ "city": "Oslo" }));
    call.call_id = call_id.map(str::to_owned);
    call
}

fn assistant(content: Vec<AssistantBlock>) -> Message {
    Message::Assistant { id: None, content }
}

#[test]
fn an_empty_tool_list_sends_no_tools_key() {
    let request = LlmRequest {
        messages: vec![Message::user("hi")],
        tools: Some(Vec::new()),
        ..Default::default()
    };
    assert!(body(&request).get("tools").is_none());
}

#[test]
fn tool_choices_map_to_their_wire_values() {
    let cases = [
        (ToolChoice::Auto, json!("auto")),
        (ToolChoice::Required, json!("required")),
        (ToolChoice::None, json!("none")),
        (
            ToolChoice::Specific("get_weather".to_owned()),
            json!({ "type": "function", "function": { "name": "get_weather" } }),
        ),
    ];
    for (choice, wire) in cases {
        let request = LlmRequest {
            messages: vec![Message::user("hi")],
            tool_choice: Some(choice.clone()),
            ..Default::default()
        };
        assert_eq!(body(&request)["tool_choice"], wire, "{choice:?}");
    }
}

#[test]
fn an_output_schema_becomes_a_strict_json_schema_response_format() {
    let request = LlmRequest {
        messages: vec![Message::user("hi")],
        output_schema: Some(json!({
            "type": "object",
            "properties": { "score": { "type": "number" } },
        })),
        ..Default::default()
    };
    let format = &body(&request)["response_format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["json_schema"]["name"], "structured_output");
    assert_eq!(format["json_schema"]["strict"], true);
    assert_eq!(
        format["json_schema"]["schema"]["additionalProperties"], false,
        "the schema is normalized for strict mode: {format}"
    );
}

#[test]
fn a_non_strict_tool_keeps_its_schema_and_says_so_when_strict_tools_are_on() {
    let tool = ToolDefinition::new("t", "d", json!({ "type": "object", "properties": {} }))
        .with_strict(false);
    let wire = convert_tool(&tool, true);
    assert_eq!(wire["function"]["strict"], false);
    assert!(
        wire["function"]["parameters"]
            .get("additionalProperties")
            .is_none(),
        "{wire}"
    );
    assert!(
        convert_tool(&tool, false)["function"]
            .get("strict")
            .is_none()
    );
}

#[test]
fn the_system_prompt_comes_first() {
    let request = LlmRequest {
        system: Some("Be brief.".to_owned()),
        messages: vec![Message::user("hi")],
        ..Default::default()
    };
    assert_eq!(
        messages(&request),
        vec![
            json!({ "role": "system", "content": "Be brief." }),
            json!({ "role": "user", "content": "hi" }),
        ]
    );
}

#[test]
fn several_text_blocks_in_one_user_message_are_joined_with_newlines() {
    let request = LlmRequest {
        messages: vec![Message::User {
            content: vec![UserBlock::text("one"), UserBlock::text("two")],
        }],
        ..Default::default()
    };
    assert_eq!(
        messages(&request),
        vec![json!({ "role": "user", "content": "one\ntwo" })]
    );
}

#[test]
fn tool_results_come_before_the_text_that_shared_their_message() {
    let call = call("c1", None);
    let request = LlmRequest {
        messages: vec![
            assistant(vec![AssistantBlock::ToolCall(call.clone())]),
            Message::User {
                content: vec![
                    UserBlock::text("Also, be quick."),
                    call.result(ToolResultContent::Text("Sunny".to_owned())),
                ],
            },
        ],
        ..Default::default()
    };
    let sent = messages(&request);
    assert_eq!(sent[1]["role"], "tool");
    assert_eq!(sent[1]["tool_call_id"], "c1");
    assert_eq!(
        sent[2],
        json!({ "role": "user", "content": "Also, be quick." })
    );
}

#[test]
fn each_tool_result_is_its_own_message_in_order() {
    let (first, second) = (call("c1", None), call("c2", None));
    let request = LlmRequest {
        messages: vec![
            assistant(vec![
                AssistantBlock::ToolCall(first.clone()),
                AssistantBlock::ToolCall(second.clone()),
            ]),
            Message::User {
                content: vec![
                    first.result(ToolResultContent::Text("one".to_owned())),
                    second.error(ToolResultContent::Text("two".to_owned())),
                ],
            },
        ],
        ..Default::default()
    };
    let sent = messages(&request);
    assert_eq!(sent[0]["tool_calls"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        sent[1],
        json!({ "role": "tool", "tool_call_id": "c1", "content": "one" })
    );
    assert_eq!(
        sent[2],
        json!({ "role": "tool", "tool_call_id": "c2", "content": "Error: two" })
    );
}

fn png() -> ToolResultContent {
    ToolResultContent::Image(ImageBlock {
        data: DocumentSource::Base64("QUJD".to_owned()),
        media_type: ImageMediaType::PNG,
        additional_params: None,
    })
}

/// The user content an image result from `png` is sent as.
fn labeled_png(id: &str) -> Value {
    json!([
        { "type": "text", "text": format!("Image returned by tool call `{id}` (tool output, not user input):") },
        { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
    ])
}

#[test]
fn results_in_separate_messages_keep_their_tool_messages_together() {
    let (first, second) = (call("c1", None), call("c2", None));
    let request = LlmRequest {
        messages: vec![
            assistant(vec![
                AssistantBlock::ToolCall(first.clone()),
                AssistantBlock::ToolCall(second.clone()),
            ]),
            Message::tool_result_for(&first, png()),
            Message::tool_result_for(&second, ToolResultContent::Text("a.png".to_owned())),
            Message::user("next"),
        ],
        ..Default::default()
    };
    let sent = messages(&request);
    let roles: Vec<&str> = sent.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(
        roles,
        ["assistant", "tool", "tool", "user", "user"],
        "{sent:?}"
    );
    assert_eq!(sent[1]["tool_call_id"], "c1");
    assert_eq!(sent[2]["tool_call_id"], "c2");
    assert_eq!(sent[3]["content"], labeled_png("c1"));
    assert_eq!(sent[4], json!({ "role": "user", "content": "next" }));
}

#[test]
fn a_failed_image_result_is_marked_and_keeps_its_image() {
    let call = call("c1", None);
    let request = LlmRequest {
        messages: vec![
            assistant(vec![AssistantBlock::ToolCall(call.clone())]),
            Message::User {
                content: vec![call.error(png())],
            },
        ],
        ..Default::default()
    };
    let sent = messages(&request);
    assert_eq!(
        sent[1],
        json!({ "role": "tool", "tool_call_id": "c1", "content": format!("Error: {IMAGE_TOOL_RESULT_NOTE}") })
    );
    assert_eq!(sent[2]["content"], labeled_png("c1"));
}

#[test]
fn a_call_from_the_responses_api_goes_by_its_call_id_in_both_places() {
    let call = call("fc_item", Some("call_link"));
    let request = LlmRequest {
        messages: vec![
            assistant(vec![AssistantBlock::ToolCall(call.clone())]),
            Message::tool_result_for(&call, ToolResultContent::Text("Sunny".to_owned())),
        ],
        ..Default::default()
    };
    let sent = messages(&request);
    assert_eq!(sent[0]["tool_calls"][0]["id"], "call_link");
    assert_eq!(sent[1]["tool_call_id"], "call_link");
}

#[test]
fn a_call_or_result_with_no_id_is_refused() {
    let nameless = call("", None);
    let request = LlmRequest {
        messages: vec![assistant(vec![AssistantBlock::ToolCall(nameless.clone())])],
        ..Default::default()
    };
    let error = build_body("m", &request, false, &BodySettings::default())
        .expect_err("a call with no id cannot be answered");
    assert!(
        matches!(error, GenerationError::InvalidRequest(_)),
        "{error:?}"
    );

    let request = LlmRequest {
        messages: vec![Message::tool_result_for(
            &nameless,
            ToolResultContent::Text("x".to_owned()),
        )],
        ..Default::default()
    };
    let error = build_body("m", &request, false, &BodySettings::default())
        .expect_err("a result with no id cannot be linked");
    assert!(
        matches!(error, GenerationError::InvalidRequest(_)),
        "{error:?}"
    );
}

#[test]
fn a_call_with_null_arguments_is_sent_as_an_empty_object() {
    let mut no_arguments = call("c1", None);
    no_arguments.function.arguments = Value::Null;
    let request = LlmRequest {
        messages: vec![assistant(vec![AssistantBlock::ToolCall(no_arguments)])],
        ..Default::default()
    };
    assert_eq!(
        messages(&request)[0]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
}

#[test]
fn an_assistant_message_joins_its_text_and_reasoning_blocks_in_order_with_no_separator() {
    let request = LlmRequest {
        messages: vec![assistant(vec![
            AssistantBlock::Reasoning(ReasoningBlock {
                id: None,
                reasoning: vec!["first".to_owned(), "second".to_owned()],
                signature: Some("sig".to_owned()),
            }),
            AssistantBlock::reasoning("third"),
            AssistantBlock::text("Hello, "),
            AssistantBlock::text("world"),
        ])],
        ..Default::default()
    };
    assert_eq!(
        messages(&request),
        vec![json!({
            "role": "assistant",
            "content": "Hello, world",
            "reasoning_content": "firstsecondthird",
        })]
    );
}

#[test]
fn a_message_with_nothing_to_send_is_left_out() {
    let reasoning_only = assistant(vec![AssistantBlock::reasoning("thinking")]);
    let empty = assistant(Vec::new());
    let request = LlmRequest {
        messages: vec![Message::user("hi"), empty, reasoning_only],
        ..Default::default()
    };
    // Reasoning alone is still sent, with empty content.
    assert_eq!(
        messages(&request),
        vec![
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "assistant", "content": "", "reasoning_content": "thinking" }),
        ]
    );

    let omit = BodySettings {
        reasoning_replay: ReasoningReplay::Omit,
        ..Default::default()
    };
    let sent = build_body("m", &request, false, &omit).unwrap();
    assert_eq!(sent["messages"].as_array().map(Vec::len), Some(1), "{sent}");
}

#[test]
fn audio_documents_and_unsupported_images_are_refused() {
    let blocks = [
        UserBlock::audio_base64("QQ==", AudioMediaType::WAV),
        UserBlock::document_base64("a.txt", "QQ==", DocumentMediaType::TXT),
        UserBlock::image_base64("QQ==", ImageMediaType::HEIC),
        call("c1", None).result(ToolResultContent::Image(ImageBlock {
            data: DocumentSource::Base64("QQ==".to_owned()),
            media_type: ImageMediaType::HEIC,
            additional_params: None,
        })),
    ];
    for block in blocks {
        let request = LlmRequest {
            messages: vec![Message::User {
                content: vec![block],
            }],
            ..Default::default()
        };
        let error = build_body("m", &request, false, &BodySettings::default())
            .expect_err("the block cannot be sent");
        assert!(
            matches!(error, GenerationError::UnsupportedContent(_)),
            "{error:?}"
        );
    }
}

#[test]
fn a_plain_request_has_no_optional_keys() {
    let request = LlmRequest {
        messages: vec![Message::user("hi")],
        ..Default::default()
    };
    assert_eq!(
        body(&request),
        json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] })
    );
}
