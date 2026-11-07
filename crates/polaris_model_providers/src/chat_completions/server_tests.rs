//! Tests that run `ChatCompletionsProvider` against a local HTTP server.

use super::*;
use core::time::Duration;
use futures_util::StreamExt;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, ImageMediaType,
    LlmProvider, LlmRequest, Message, ReasoningEffort, StopReason, StreamEvent, StreamEventExt,
    ToolChoice, ToolDefinition, ToolResultContent, Usage, UserBlock,
};
use serde_json::{Value, json};
use std::num::NonZeroU32;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BYTES: usize = 256 * 1024;

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out waiting on the loopback server or provider")
}

/// What the loopback server saw.
struct Recorded {
    /// The request line and headers, lower-cased header names.
    head: String,
    /// The JSON request body.
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }
}

/// How the loopback server answers.
struct Reply {
    status: &'static str,
    content_type: &'static str,
    extra_headers: String,
    /// Written in order, with a pause and a flush between pieces.
    pieces: Vec<Vec<u8>>,
}

impl Reply {
    fn json(body: &Value) -> Self {
        Self::with_status("200 OK", "application/json", body.to_string())
    }

    fn with_status(status: &'static str, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            extra_headers: String::new(),
            pieces: vec![body.into_bytes()],
        }
    }

    fn sse(chunks: &[Value], done: bool) -> Self {
        let mut body: String = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect();
        if done {
            body.push_str("data: [DONE]\n\n");
        }
        Self::with_status("200 OK", "text/event-stream", body)
    }

    fn header(mut self, line: &str) -> Self {
        self.extra_headers.push_str(line);
        self.extra_headers.push_str("\r\n");
        self
    }
}

/// Serves one canned reply and records the request.
struct Server {
    base_url: String,
    task: JoinHandle<Recorded>,
}

impl Server {
    async fn start(reply: Reply) -> Self {
        let listener = within(TcpListener::bind("127.0.0.1:0"))
            .await
            .expect("should bind a local port");
        let addr = listener.local_addr().expect("listener should have an addr");
        let task = tokio::spawn(within(async move {
            let (mut socket, _) = listener.accept().await.expect("should accept a connection");
            let recorded = read_request(&mut socket).await;
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}Connection: close\r\n\r\n",
                reply.status, reply.content_type, reply.extra_headers
            );
            socket
                .write_all(head.as_bytes())
                .await
                .expect("should write the response head");
            for piece in reply.pieces {
                socket
                    .write_all(&piece)
                    .await
                    .expect("should write a response piece");
                socket.flush().await.expect("should flush");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            socket.shutdown().await.expect("should close the socket");
            recorded
        }));
        Self {
            base_url: format!("http://{addr}/v1"),
            task,
        }
    }

    fn provider(&self) -> ChatCompletionsProvider {
        ChatCompletionsProvider::new("test-key")
            .with_base_url(&self.base_url)
            .with_http_client(no_proxy_client())
    }

    async fn recorded(&mut self) -> Recorded {
        within(&mut self.task)
            .await
            .expect("loopback server task should not panic")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A client that ignores proxies named in the environment, which would
/// otherwise carry a loopback request away from the test server.
fn no_proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client without proxies should build")
}

async fn read_request(socket: &mut TcpStream) -> Recorded {
    let mut received = Vec::new();
    let body_start = loop {
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        read_more(socket, &mut received).await;
    };
    let head = core::str::from_utf8(&received[..body_start])
        .expect("headers should be UTF-8")
        .to_owned();
    assert!(
        head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
        "{head}"
    );
    let content_length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .expect("request should carry a Content-Length header")
        .1
        .trim()
        .parse()
        .expect("Content-Length should be a number");
    let body_end = body_start + content_length;
    while received.len() < body_end {
        read_more(socket, &mut received).await;
    }
    Recorded {
        head,
        body: serde_json::from_slice(&received[body_start..body_end])
            .expect("request body should be JSON"),
    }
}

async fn read_more(socket: &mut TcpStream, received: &mut Vec<u8>) {
    let mut chunk = [0u8; 4096];
    let read = socket
        .read(&mut chunk)
        .await
        .expect("should read the request");
    assert!(read > 0, "client closed the connection mid-request");
    assert!(
        received.len() + read <= MAX_REQUEST_BYTES,
        "request is too large"
    );
    received.extend_from_slice(&chunk[..read]);
}

fn hello() -> LlmRequest {
    LlmRequest {
        messages: vec![Message::user("Hello")],
        ..Default::default()
    }
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition::new(
        "get_weather",
        "Looks up the weather",
        json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
    )
}

async fn collect(
    stream: polaris_models::llm::LlmStream,
) -> Vec<Result<StreamEvent, GenerationError>> {
    within(stream.collect()).await
}

// ── generate: the request ─────────────────────────────────────────────

#[tokio::test]
async fn generate_sends_the_request_in_chat_completions_form() {
    let mut server = Server::start(Reply::json(&json!({
        "id": "chatcmpl-1",
        "model": "served-model",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Hi!" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 },
    })))
    .await;
    let provider = server.provider();
    let request = LlmRequest {
        system: Some("Be brief.".to_owned()),
        messages: vec![Message::user("Hello")],
        tools: Some(vec![weather_tool()]),
        tool_choice: Some(ToolChoice::Specific("get_weather".to_owned())),
        max_output_tokens: NonZeroU32::new(512),
        reasoning_effort: Some(ReasoningEffort::High),
        ..Default::default()
    };
    let response = within(provider.generate("zai-org/GLM-5.3-Flash:zai", request))
        .await
        .expect("the request should succeed");

    assert_eq!(response.text(), "Hi!");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(response.model.as_deref(), Some("served-model"));
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            total_tokens: Some(15),
            ..Default::default()
        }
    );

    let seen = server.recorded().await;
    assert_eq!(seen.header("authorization"), Some("Bearer test-key"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(
        seen.body,
        json!({
            "model": "zai-org/GLM-5.3-Flash:zai",
            "messages": [
                { "role": "system", "content": "Be brief." },
                { "role": "user", "content": "Hello" },
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Looks up the weather",
                    "parameters": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } },
                        "required": ["city"],
                    },
                },
            }],
            "tool_choice": { "type": "function", "function": { "name": "get_weather" } },
            "max_completion_tokens": 512,
            "reasoning_effort": "high",
        })
    );
}

#[tokio::test]
async fn a_keyless_provider_sends_no_authorization_header() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let provider = ChatCompletionsProvider::without_api_key()
        .with_base_url(&server.base_url)
        .with_http_client(no_proxy_client());
    within(provider.generate("local-model", hello()))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    assert_eq!(seen.header("authorization"), None, "{}", seen.head);
}

#[tokio::test]
async fn the_output_limit_goes_in_the_key_the_provider_was_told_to_use() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let provider = server
        .provider()
        .with_max_tokens_field(MaxTokensField::MaxTokens);
    let mut request = hello();
    request.max_output_tokens = NonZeroU32::new(64);
    within(provider.generate("m", request))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    assert_eq!(seen.body["max_tokens"], 64, "{}", seen.body);
    assert!(seen.body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn extra_body_fields_replace_the_provider_fields_with_the_same_key() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let provider = server.provider().with_extra_body(json!({
        "thinking": { "type": "enabled", "clear_thinking": false },
        "reasoning_effort": "max",
    }));
    let mut request = hello();
    request.reasoning_effort = Some(ReasoningEffort::Low);
    within(provider.generate("m", request))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    assert_eq!(
        seen.body["thinking"],
        json!({ "type": "enabled", "clear_thinking": false })
    );
    assert_eq!(
        seen.body["reasoning_effort"], "max",
        "the extra field replaces the request's own"
    );
    assert_eq!(seen.body["model"], "m");
}

#[tokio::test]
async fn an_extra_body_that_is_not_an_object_fails_before_any_request() {
    // Nothing listens here: reaching the network would be a different error.
    let provider = ChatCompletionsProvider::without_api_key()
        .with_base_url("http://127.0.0.1:9/v1")
        .with_extra_body(json!(["not", "an", "object"]))
        .with_http_client(no_proxy_client());
    let error = within(provider.generate("m", hello()))
        .await
        .expect_err("a non-object extra body must fail");
    assert!(
        matches!(&error, GenerationError::InvalidRequest(message) if message.contains("an array")),
        "{error:?}"
    );
}

#[tokio::test]
async fn strict_tools_are_sent_only_when_asked_for() {
    let strict_response =
        || Reply::json(&json!({ "choices": [{ "message": { "content": "ok" } }] }));
    let request = || LlmRequest {
        messages: vec![Message::user("hi")],
        tools: Some(vec![weather_tool()]),
        ..Default::default()
    };

    let mut server = Server::start(strict_response()).await;
    within(server.provider().generate("m", request()))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    let function = &seen.body["tools"][0]["function"];
    assert!(function.get("strict").is_none(), "{function}");
    assert!(
        function["parameters"].get("additionalProperties").is_none(),
        "the schema is sent as given: {function}"
    );

    let mut server = Server::start(strict_response()).await;
    within(
        server
            .provider()
            .with_strict_tools(true)
            .generate("m", request()),
    )
    .await
    .expect("the request should succeed");
    let seen = server.recorded().await;
    let function = &seen.body["tools"][0]["function"];
    assert_eq!(function["strict"], true, "{function}");
    assert_eq!(function["parameters"]["additionalProperties"], false);
}

#[tokio::test]
async fn images_are_sent_as_data_urls_in_content_parts() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "A cat." } }],
    })))
    .await;
    let provider = server.provider();
    let request = LlmRequest {
        messages: vec![Message::User {
            content: vec![
                UserBlock::text("What is this?"),
                UserBlock::image_base64("QUJD", ImageMediaType::PNG),
            ],
        }],
        ..Default::default()
    };
    let response = within(provider.generate("vision-model", request))
        .await
        .expect("the request should succeed");
    assert_eq!(response.text(), "A cat.");
    let seen = server.recorded().await;
    assert_eq!(
        seen.body["messages"],
        json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "What is this?" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
            ],
        }])
    );
}

// ── generate: the response ────────────────────────────────────────────

#[tokio::test]
async fn generate_returns_reasoning_text_and_tool_calls_in_order() {
    let server = Server::start(Reply::json(&json!({
        "id": "chatcmpl-2",
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Checking.",
                "reasoning_content": "The user wants Oslo.",
                "tool_calls": [{
                    "id": "call_a",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}" },
                }, {
                    "id": "call_b",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Rome\"}" },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "total_tokens": 140,
            "prompt_tokens_details": { "cached_tokens": 30 },
            "completion_tokens_details": { "reasoning_tokens": 25 },
        },
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert!(
        matches!(
            response.content.as_slice(),
            [
                AssistantBlock::Reasoning(reasoning),
                AssistantBlock::Text(text),
                AssistantBlock::ToolCall(first),
                AssistantBlock::ToolCall(second),
            ] if reasoning.reasoning == ["The user wants Oslo."]
                && text.text == "Checking."
                && first.id == "call_a"
                && first.call_id.as_deref() == Some("call_a")
                && first.function.arguments == json!({ "city": "Oslo" })
                && second.id == "call_b"
                && second.function.arguments == json!({ "city": "Rome" })
        ),
        "{:?}",
        response.content
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: Some(70),
            output_tokens: Some(40),
            total_tokens: Some(140),
            cache_read_tokens: Some(30),
            cache_creation_tokens: None,
            reasoning_output_tokens: Some(25),
        }
    );
}

#[tokio::test]
async fn generate_reads_reasoning_from_the_field_newer_servers_use() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "42", "reasoning": "Six times seven." }, "finish_reason": "stop" }],
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");
    assert!(
        matches!(
            response.content.as_slice(),
            [AssistantBlock::Reasoning(reasoning), AssistantBlock::Text(_)]
                if reasoning.reasoning == ["Six times seven."]
        ),
        "{:?}",
        response.content
    );
}

#[tokio::test]
async fn generate_accepts_a_response_that_leaves_most_fields_out() {
    // No id, model, usage, finish_reason or tool call id; the arguments are an
    // object; the server says `stop` although the model called a tool.
    let server = Server::start(Reply::json(&json!({
        "choices": [{
            "message": {
                "tool_calls": [{ "function": { "name": "get_weather", "arguments": { "city": "Oslo" } } }],
            },
            "finish_reason": "stop",
        }],
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("a sparse response should still be read");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.id, None);
    assert_eq!(response.model, None);
    assert_eq!(response.usage, Usage::default());
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 1, "{:?}", response.content);
    assert_eq!(calls[0].id, "chatcmpl_call_0");
    assert_eq!(calls[0].call_id.as_deref(), Some("chatcmpl_call_0"));
    assert_eq!(calls[0].function.arguments, json!({ "city": "Oslo" }));
}

#[tokio::test]
async fn generate_reads_text_from_a_list_of_content_parts() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": [{ "type": "text", "text": "Hello, " }, { "type": "text", "text": "world" }] } }],
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");
    assert_eq!(response.text(), "Hello, world");
}

#[tokio::test]
async fn generate_maps_the_output_limit_stop_even_when_a_tool_call_was_cut_off() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{
            "message": { "content": "Partial", "tool_calls": [{ "id": "c", "function": { "name": "t", "arguments": "{}" } }] },
            "finish_reason": "length",
        }],
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");
    assert_eq!(response.stop_reason, StopReason::MaxOutputTokens);
}

#[tokio::test]
async fn generate_keeps_a_finish_reason_it_does_not_know() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "x" }, "finish_reason": "insufficient_system_resource" }],
    })))
    .await;
    let response = within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");
    assert_eq!(
        response.stop_reason,
        StopReason::Other("insufficient_system_resource".to_owned())
    );
}

#[tokio::test]
async fn generate_fails_on_tool_arguments_that_are_not_json() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{
            "message": { "tool_calls": [{ "id": "c", "function": { "name": "get_weather", "arguments": "{\"city\": " } }] },
            "finish_reason": "tool_calls",
        }],
    })))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("unparseable arguments must fail");
    assert!(
        matches!(&error, GenerationError::InvalidResponse(message) if message.contains("get_weather")),
        "{error:?}"
    );
}

#[tokio::test]
async fn generate_fails_on_a_tool_call_without_a_name() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "tool_calls": [{ "id": "c", "function": { "arguments": "{}" } }] } }],
    })))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("a nameless call cannot be answered");
    assert!(
        matches!(&error, GenerationError::InvalidResponse(message) if message.contains("no function name")),
        "{error:?}"
    );
}

#[tokio::test]
async fn generate_reports_a_refusal_that_came_with_no_content() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": null, "refusal": "I can't help with that." }, "finish_reason": "stop" }],
    })))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("a refusal is an error");
    assert!(
        matches!(&error, GenerationError::Refusal(message) if message == "I can't help with that."),
        "{error:?}"
    );
}

// ── generate: errors ──────────────────────────────────────────────────

#[tokio::test]
async fn a_server_that_rejects_the_effort_reports_its_own_message() {
    // The shape Z.ai uses when a level is not one the model has.
    let server = Server::start(Reply::with_status(
        "400 Bad Request",
        "application/json",
        json!({ "error": { "code": "1210", "message": "Invalid reasoning_effort: medium" } })
            .to_string(),
    ))
    .await;
    let mut request = hello();
    request.reasoning_effort = Some(ReasoningEffort::Medium);
    let error = within(server.provider().generate("m", request))
        .await
        .expect_err("the server rejected the request");
    assert!(
        matches!(&error, GenerationError::Provider { status: Some(400), message, .. }
            if message == "Invalid reasoning_effort: medium"),
        "{error:?}"
    );
}

#[tokio::test]
async fn unauthorized_and_rate_limited_replies_map_to_their_own_errors() {
    let server = Server::start(Reply::with_status(
        "401 Unauthorized",
        "application/json",
        json!({ "error": { "message": "Invalid API key" } }).to_string(),
    ))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("401 is an error");
    assert!(
        matches!(&error, GenerationError::Auth(message) if message == "Invalid API key"),
        "{error:?}"
    );

    let server = Server::start(
        Reply::with_status("429 Too Many Requests", "application/json", "{}".to_owned())
            .header("Retry-After: 3"),
    )
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("429 is an error");
    assert!(
        matches!(error, GenerationError::RateLimited { retry_after: Some(wait) } if wait == Duration::from_secs(3)),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_error_body_sent_with_a_success_status_is_an_error() {
    let server = Server::start(Reply::json(&json!({
        "error": { "message": "model is overloaded", "type": "server_error" },
    })))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("the body is an error");
    assert!(
        matches!(&error, GenerationError::Provider { status: None, message, .. }
            if message == "model is overloaded"),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_body_that_is_not_json_is_an_invalid_response_that_shows_its_start() {
    let server = Server::start(Reply::with_status(
        "200 OK",
        "text/html",
        "<html>login required</html>".to_owned(),
    ))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("HTML is not a response");
    assert!(
        matches!(&error, GenerationError::InvalidResponse(message) if message.contains("login required")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_server_that_cannot_be_reached_is_an_http_error() {
    let provider = ChatCompletionsProvider::without_api_key()
        .with_base_url("http://127.0.0.1:9/v1")
        .with_http_client(no_proxy_client());
    let error = within(provider.generate("m", hello()))
        .await
        .expect_err("nothing listens on port 9");
    assert!(matches!(error, GenerationError::Http(_)), "{error:?}");
}

// ── tool results and reasoning, across turns ──────────────────────────

/// A tool-calling turn as the server sent it, read back through the provider.
async fn first_turn() -> Vec<AssistantBlock> {
    let server = Server::start(Reply::json(&json!({
        "id": "chatcmpl-turn1",
        "choices": [{
            "message": {
                "content": null,
                "reasoning_content": "I should look the weather up.",
                "tool_calls": [{
                    "id": "call_weather",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}" },
                }],
            },
            "finish_reason": "tool_calls",
        }],
    })))
    .await;
    within(server.provider().generate("m", hello()))
        .await
        .expect("the first turn should succeed")
        .content
}

fn second_turn_request(content: Vec<AssistantBlock>) -> LlmRequest {
    let call = content
        .iter()
        .find_map(|block| match block {
            AssistantBlock::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .expect("the first turn has a tool call");
    LlmRequest {
        messages: vec![
            Message::user("What is the weather in Oslo?"),
            Message::Assistant { id: None, content },
            Message::tool_result_for(&call, ToolResultContent::Text("Sunny".to_owned())),
        ],
        tools: Some(vec![weather_tool()]),
        ..Default::default()
    }
}

async fn send_second_turn(provider_for: impl FnOnce(&Server) -> ChatCompletionsProvider) -> Value {
    let content = first_turn().await;
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "Sunny in Oslo." }, "finish_reason": "stop" }],
    })))
    .await;
    let provider = provider_for(&server);
    let response = within(provider.generate("m", second_turn_request(content)))
        .await
        .expect("the second turn should succeed");
    assert_eq!(response.text(), "Sunny in Oslo.");
    server.recorded().await.body
}

#[tokio::test]
async fn reasoning_and_tool_results_are_sent_back_the_way_the_server_expects() {
    let body = send_second_turn(Server::provider).await;
    assert_eq!(
        body["messages"],
        json!([
            { "role": "user", "content": "What is the weather in Oslo?" },
            {
                "role": "assistant",
                "content": null,
                "reasoning_content": "I should look the weather up.",
                "tool_calls": [{
                    "id": "call_weather",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}" },
                }],
            },
            { "role": "tool", "tool_call_id": "call_weather", "content": "Sunny" },
        ])
    );
}

#[tokio::test]
async fn reasoning_can_be_sent_back_as_reasoning_or_left_out() {
    let body = send_second_turn(|server| {
        server
            .provider()
            .with_reasoning_replay(ReasoningReplay::Reasoning)
    })
    .await;
    let assistant = &body["messages"][1];
    assert_eq!(assistant["reasoning"], "I should look the weather up.");
    assert!(assistant.get("reasoning_content").is_none(), "{assistant}");

    let body = send_second_turn(|server| {
        server
            .provider()
            .with_reasoning_replay(ReasoningReplay::Omit)
    })
    .await;
    let assistant = &body["messages"][1];
    assert!(assistant.get("reasoning").is_none(), "{assistant}");
    assert!(assistant.get("reasoning_content").is_none(), "{assistant}");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_weather");
}

#[tokio::test]
async fn a_failed_tool_result_is_marked_and_images_in_results_are_refused() {
    let mut call = polaris_models::llm::ToolCall::new("c1", "get_weather", json!({}));
    call.call_id = Some("c1".to_owned());
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let request = LlmRequest {
        messages: vec![
            Message::user("go"),
            Message::assistant_tool_call(call.clone()),
            Message::tool_error_for(&call, ToolResultContent::Text("timed out".to_owned())),
        ],
        ..Default::default()
    };
    within(server.provider().generate("m", request))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    assert_eq!(
        seen.body["messages"][2],
        json!({ "role": "tool", "tool_call_id": "c1", "content": "Error: timed out" })
    );

    let provider = ChatCompletionsProvider::without_api_key().with_http_client(no_proxy_client());
    let image = polaris_models::llm::ImageBlock {
        data: polaris_models::llm::DocumentSource::Base64("QUJD".to_owned()),
        media_type: ImageMediaType::PNG,
        additional_params: None,
    };
    let request = LlmRequest {
        messages: vec![
            Message::user("go"),
            Message::assistant_tool_call(call.clone()),
            Message::tool_result_for(&call, ToolResultContent::Image(image)),
        ],
        ..Default::default()
    };
    let error = within(provider.generate("m", request))
        .await
        .expect_err("image results cannot be sent");
    assert!(
        matches!(error, GenerationError::UnsupportedContent(_)),
        "{error:?}"
    );
}

// ── streaming ─────────────────────────────────────────────────────────

fn chunk(delta: Value) -> Value {
    json!({ "id": "chatcmpl-s", "model": "served-model", "choices": [{ "index": 0, "delta": delta }] })
}

fn finish_chunk(reason: &str) -> Value {
    json!({
        "id": "chatcmpl-s",
        "model": "served-model",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }],
    })
}

#[tokio::test]
async fn stream_sends_the_stream_flags_and_reads_reasoning_text_and_late_usage() {
    let mut server = Server::start(Reply::sse(
        &[
            chunk(json!({ "role": "assistant", "content": "" })),
            chunk(json!({ "reasoning_content": "Let me " })),
            chunk(json!({ "reasoning_content": "think." })),
            chunk(json!({ "content": "The answer" })),
            chunk(json!({ "content": " is 42." })),
            finish_chunk("stop"),
            // The usage arrives last, in a chunk with no choices.
            json!({
                "id": "chatcmpl-s",
                "choices": [],
                "usage": {
                    "prompt_tokens": 20, "completion_tokens": 9, "total_tokens": 29,
                    "prompt_tokens_details": { "cached_tokens": 5 },
                    "completion_tokens_details": { "reasoning_tokens": 4 },
                },
            }),
        ],
        true,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events: Vec<_> = collect(stream)
        .await
        .into_iter()
        .map(|event| event.expect("no stream error expected"))
        .collect();

    let usage = Usage {
        input_tokens: Some(15),
        output_tokens: Some(9),
        total_tokens: Some(29),
        cache_read_tokens: Some(5),
        cache_creation_tokens: None,
        reasoning_output_tokens: Some(4),
    };
    assert_eq!(
        events,
        vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("Let me ".to_owned())
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("think.".to_owned())
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::Text
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::Text("The answer".to_owned())
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::Text(" is 42.".to_owned())
            },
            StreamEvent::MessageDelta {
                usage: usage.clone()
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage,
                id: Some("chatcmpl-s".to_owned()),
                model: Some("served-model".to_owned()),
            },
        ]
    );
    let seen = server.recorded().await;
    assert_eq!(seen.body["stream"], true, "{}", seen.body);
    assert_eq!(
        seen.body["stream_options"],
        json!({ "include_usage": true })
    );
}

#[tokio::test]
async fn stream_assembles_parallel_tool_calls_whose_fragments_interleave() {
    let server = Server::start(Reply::sse(
        &[
            chunk(json!({ "reasoning_content": "Two lookups." })),
            chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "type": "function", "function": { "name": "get_weather", "arguments": "" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"city\":" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 1, "id": "call_b", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Rome\"}" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "\"Oslo\"}" } }] })),
            finish_chunk("tool_calls"),
        ],
        true,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let response = within(stream.collect_response())
        .await
        .expect("the stream should assemble into a response");

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 2, "{:?}", response.content);
    assert_eq!(calls[0].id, "call_a");
    assert_eq!(calls[0].call_id.as_deref(), Some("call_a"));
    assert_eq!(calls[0].function.arguments, json!({ "city": "Oslo" }));
    assert_eq!(calls[1].id, "call_b");
    assert_eq!(calls[1].function.arguments, json!({ "city": "Rome" }));
    assert!(
        matches!(&response.content[0], AssistantBlock::Reasoning(r) if r.reasoning == ["Two lookups."]),
        "{:?}",
        response.content
    );
    assert_eq!(response.id.as_deref(), Some("chatcmpl-s"));
}

#[tokio::test]
async fn stream_accepts_chunks_that_leave_out_ids_indexes_usage_and_the_done_marker() {
    // No tool call id or index, arguments as an object in one chunk, the
    // server says `stop`, no usage, and the connection just closes.
    let server = Server::sse_without_done(&[
        chunk(json!({ "tool_calls": [{ "function": { "name": "get_weather" } }] })),
        chunk(json!({ "tool_calls": [{ "function": { "arguments": { "city": "Oslo" } } }] })),
        finish_chunk("stop"),
    ])
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let response = within(stream.collect_response())
        .await
        .expect("a sparse stream should still assemble");
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage, Usage::default());
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 1, "{:?}", response.content);
    assert_eq!(calls[0].id, "chatcmpl-s_call_0");
    assert_eq!(calls[0].function.arguments, json!({ "city": "Oslo" }));
}

#[tokio::test]
async fn stream_holds_arguments_that_arrive_before_the_function_name() {
    let server = Server::start(Reply::sse(
        &[
            chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "function": { "arguments": "{\"city\":" } }] })),
            chunk(json!({ "tool_calls": [{ "index": 0, "function": { "name": "get_weather", "arguments": "\"Oslo\"}" } }] })),
            finish_chunk("tool_calls"),
        ],
        true,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let response = within(stream.collect_response())
        .await
        .expect("the stream should assemble");
    let calls = response.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "get_weather");
    assert_eq!(calls[0].function.arguments, json!({ "city": "Oslo" }));
}

#[tokio::test]
async fn stream_fails_when_a_tool_call_never_gets_a_name() {
    let server = Server::start(Reply::sse(
        &[
            chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "function": { "arguments": "{}" } }] })),
            finish_chunk("tool_calls"),
        ],
        true,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events = collect(stream).await;
    let error = events
        .into_iter()
        .find_map(Result::err)
        .expect("the stream should fail");
    assert!(
        matches!(&error, GenerationError::InvalidResponse(message) if message.contains("no function name")),
        "{error:?}"
    );
}

#[tokio::test]
async fn stream_reports_an_error_the_server_sends_in_the_middle() {
    let server = Server::start(Reply::sse(
        &[
            chunk(json!({ "content": "Partial" })),
            json!({ "error": { "message": "upstream timeout", "code": 504 } }),
        ],
        false,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events = collect(stream).await;
    let last = events.last().expect("the stream has events");
    assert!(
        matches!(last, Err(GenerationError::Provider { message, .. }) if message == "upstream timeout"),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_fails_when_the_connection_closes_before_the_server_finished() {
    let server = Server::sse_without_done(&[chunk(json!({ "content": "Partial" }))]).await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events = collect(stream).await;
    let last = events.last().expect("the stream has events");
    assert!(
        matches!(last, Err(GenerationError::InvalidResponse(message)) if message.contains("before the server finished")),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_reports_a_refusal_that_came_with_no_content() {
    let server = Server::start(Reply::sse(
        &[
            chunk(json!({ "refusal": "I can't " })),
            chunk(json!({ "refusal": "help." })),
            finish_chunk("stop"),
        ],
        true,
    ))
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events = collect(stream).await;
    assert!(
        matches!(events.last(), Some(Err(GenerationError::Refusal(message))) if message == "I can't help."),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_reads_events_split_across_writes_and_inside_a_character() {
    let first = format!("data: {}\n\n", chunk(json!({ "content": "caf\u{e9} " })));
    let second = format!(
        "data: {}\r\n\r\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({ "content": "cr\u{e8}me" })),
        finish_chunk("stop")
    );
    let bytes: Vec<u8> = [first.as_bytes(), second.as_bytes()].concat();
    // Cut inside the two-byte `é`, then in the middle of the next event.
    let cut_in_character = first.find('\u{e9}').expect("the text has an accent") + 1;
    let cut_in_event = first.len() + 9;
    let pieces = vec![
        bytes[..cut_in_character].to_vec(),
        bytes[cut_in_character..cut_in_event].to_vec(),
        bytes[cut_in_event..].to_vec(),
    ];
    let server = Server::start(Reply {
        status: "200 OK",
        content_type: "text/event-stream",
        extra_headers: String::new(),
        pieces,
    })
    .await;
    let stream = within(server.provider().stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let response = within(stream.collect_response())
        .await
        .expect("split events should still assemble");
    assert_eq!(response.text(), "caf\u{e9} cr\u{e8}me");
}

#[tokio::test]
async fn stream_reports_http_errors_before_any_event() {
    let server = Server::start(Reply::with_status(
        "401 Unauthorized",
        "application/json",
        json!({ "error": { "message": "no key" } }).to_string(),
    ))
    .await;
    let error = within(server.provider().stream("m", hello()))
        .await
        .err()
        .expect("the stream request should fail");
    assert!(matches!(error, GenerationError::Auth(_)), "{error:?}");
}

impl Server {
    /// Serves server-sent events and closes the connection with no `[DONE]`.
    async fn sse_without_done(chunks: &[Value]) -> Self {
        Self::start(Reply::sse(chunks, false)).await
    }
}
