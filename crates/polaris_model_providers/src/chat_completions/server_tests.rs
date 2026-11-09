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
    /// Send nothing at all after reading the request.
    silent: bool,
    /// The pause after each piece.
    pause: Duration,
    /// Keep the connection open and silent after the last piece.
    stall: bool,
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
            silent: false,
            pause: Duration::from_millis(5),
            stall: false,
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
            if reply.silent {
                tokio::time::sleep(TIMEOUT).await;
                return recorded;
            }
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
                tokio::time::sleep(reply.pause).await;
            }
            if reply.stall {
                tokio::time::sleep(TIMEOUT).await;
                return recorded;
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

    fn provider_with(&self, timeouts: Timeouts) -> ChatCompletionsProvider {
        ChatCompletionsProvider::new("test-key")
            .with_base_url(&self.base_url)
            .with_timeouts(timeouts)
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
    no_proxy_client_with(Timeouts::default())
}

fn no_proxy_client_with(timeouts: Timeouts) -> reqwest::Client {
    provider::client_builder(timeouts)
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
async fn requests_name_the_library_in_the_user_agent() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    within(server.provider().generate("m", hello()))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    let agent = seen.header("user-agent").expect("a user agent is sent");
    assert!(agent.starts_with("polaris-ai/"), "{agent}");
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
async fn extra_body_calls_merge_with_later_values_winning() {
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let provider = server
        .provider()
        .with_extra_body(
            json!({
                "thinking": { "type": "enabled", "clear_thinking": false },
                "reasoning_effort": "max",
                "top_k": 10,
                "max_completion_tokens": 64,
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .with_extra_body(json!({ "top_k": 20 }).as_object().unwrap().clone());
    let mut request = hello();
    request.max_output_tokens = NonZeroU32::new(12);
    within(provider.generate("m", request))
        .await
        .expect("the request should succeed");
    let seen = server.recorded().await;
    assert_eq!(
        seen.body["thinking"],
        json!({ "type": "enabled", "clear_thinking": false })
    );
    assert_eq!(seen.body["reasoning_effort"], "max");
    assert_eq!(seen.body["top_k"], 20);
    assert_eq!(seen.body["max_completion_tokens"], 64);
    assert_eq!(seen.body["model"], "m");
}

#[tokio::test]
async fn reserved_extra_body_keys_fail_before_any_request() {
    for key in [
        "model",
        "messages",
        "stream",
        "stream_options",
        "reasoning_effort",
    ] {
        // Nothing listens here: reaching the network would be a different error.
        let provider = ChatCompletionsProvider::without_api_key()
            .with_base_url("http://127.0.0.1:9/v1")
            .with_extra_body(serde_json::Map::from_iter([(
                key.to_owned(),
                json!("override"),
            )]))
            .with_http_client(no_proxy_client());
        let mut request = hello();
        request.reasoning_effort = Some(ReasoningEffort::Low);
        let generate_error = within(provider.generate("m", request.clone()))
            .await
            .expect_err("a reserved key must fail");
        let stream_error = within(provider.stream("m", request))
            .await
            .err()
            .expect("a reserved key must fail for streams too");
        for error in [generate_error, stream_error] {
            assert!(
                matches!(&error, GenerationError::InvalidRequest(message) if message.contains(key)),
                "{key}: {error:?}"
            );
        }
    }
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
    assert!(
        calls[0].id.starts_with("chatcmpl-") && calls[0].id.ends_with("_call_0"),
        "{}",
        calls[0].id
    );
    assert_eq!(calls[0].call_id.as_deref(), Some(calls[0].id.as_str()));
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

/// The first, tool-calling turn as a server sends it, with its reasoning in
/// `field` (`reasoning_content` or `reasoning`).
fn first_turn_reply(field: &str) -> Reply {
    Reply::json(&json!({
        "id": "chatcmpl-turn1",
        "choices": [{
            "message": {
                "content": null,
                field: "I should look the weather up.",
                "tool_calls": [{
                    "id": "call_weather",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}" },
                }],
            },
            "finish_reason": "tool_calls",
        }],
    }))
}

/// The same turn as a stream.
fn first_turn_stream_reply(field: &str) -> Reply {
    Reply::sse(
        &[
            chunk(json!({ field: "I should look the weather up." })),
            chunk(
                json!({ "tool_calls": [{ "index": 0, "id": "call_weather", "function": { "name": "get_weather", "arguments": "{\"city\":\"Oslo\"}" } }] }),
            ),
            finish_chunk("tool_calls"),
        ],
        true,
    )
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

/// How the first turn is fetched.
#[derive(Clone, Copy)]
enum FirstTurn {
    Generate,
    Stream,
}

/// Runs a tool turn against one loopback server whose reasoning is in `field`,
/// then the next turn against a second server through the *same* provider
/// (reconfigured by `configure`), and returns the second request's body.
async fn two_turns(
    field: &str,
    how: FirstTurn,
    configure: impl FnOnce(ChatCompletionsProvider) -> ChatCompletionsProvider,
) -> Value {
    let first = match how {
        FirstTurn::Generate => Server::start(first_turn_reply(field)).await,
        FirstTurn::Stream => Server::start(first_turn_stream_reply(field)).await,
    };
    let provider = configure(first.provider());
    let content = match how {
        FirstTurn::Generate => {
            within(provider.generate("m", hello()))
                .await
                .expect("the first turn should succeed")
                .content
        }
        FirstTurn::Stream => {
            let stream = within(provider.stream("m", hello()))
                .await
                .expect("the first stream should start");
            within(stream.collect_response())
                .await
                .expect("the first stream should assemble")
                .content
        }
    };

    let mut second = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "Sunny in Oslo." }, "finish_reason": "stop" }],
    })))
    .await;
    // A clone shares what the provider has learned about the server.
    let provider = provider.clone().with_base_url(&second.base_url);
    let response = within(provider.generate("m", second_turn_request(content)))
        .await
        .expect("the second turn should succeed");
    assert_eq!(response.text(), "Sunny in Oslo.");
    second.recorded().await.body
}

fn assistant_message(body: &Value) -> &Value {
    &body["messages"][1]
}

#[tokio::test]
async fn reasoning_and_tool_results_are_sent_back_the_way_the_server_expects() {
    let body = two_turns("reasoning_content", FirstTurn::Generate, |p| p).await;
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
async fn by_default_reasoning_goes_back_under_the_field_the_server_used() {
    for how in [FirstTurn::Generate, FirstTurn::Stream] {
        // A server that answers under `reasoning`, like the Hugging Face router.
        let body = two_turns("reasoning", how, |p| p).await;
        let assistant = assistant_message(&body);
        assert_eq!(assistant["reasoning"], "I should look the weather up.");
        assert!(assistant.get("reasoning_content").is_none(), "{assistant}");

        // A server that answers under `reasoning_content`, like `DeepSeek`.
        let body = two_turns("reasoning_content", how, |p| p).await;
        let assistant = assistant_message(&body);
        assert_eq!(
            assistant["reasoning_content"],
            "I should look the weather up."
        );
        assert!(assistant.get("reasoning").is_none(), "{assistant}");
    }
}

#[tokio::test]
async fn reasoning_replay_is_learned_separately_for_each_model() {
    let first = Server::start(first_turn_reply("reasoning_content")).await;
    let mut provider = first.provider();
    let content_a = within(provider.generate("model-a", hello()))
        .await
        .expect("model-a should answer")
        .content;

    let second = Server::start(first_turn_stream_reply("reasoning")).await;
    provider = provider.with_base_url(&second.base_url);
    let stream = within(provider.stream("model-b", hello()))
        .await
        .expect("model-b should stream");
    let content_b = within(stream.collect_response())
        .await
        .expect("model-b should answer")
        .content;

    for (model, field, other, content) in [
        ("model-a", "reasoning_content", "reasoning", content_a),
        ("model-b", "reasoning", "reasoning_content", content_b),
    ] {
        let mut next = Server::start(Reply::json(&json!({
            "choices": [{ "message": { "content": "Sunny" } }],
        })))
        .await;
        provider = provider.with_base_url(&next.base_url);
        within(provider.generate(model, second_turn_request(content)))
            .await
            .expect("the next turn should succeed");
        let body = next.recorded().await.body;
        let assistant = assistant_message(&body);
        assert_eq!(
            assistant[field], "I should look the weather up.",
            "{model}: {body}"
        );
        assert!(assistant.get(other).is_none(), "{model}: {body}");
        assert_eq!(body["model"], model);
    }
}

#[tokio::test]
async fn reasoning_replay_tracks_field_changes_through_clones_and_keeps_them_without_reasoning() {
    for how in [FirstTurn::Generate, FirstTurn::Stream] {
        let provider =
            ChatCompletionsProvider::without_api_key().with_http_client(no_proxy_client());
        // Clone before learning so updates must be shared in both directions.
        let cloned = provider.clone();
        for (index, field) in ["reasoning", "reasoning_content", "reasoning"]
            .into_iter()
            .enumerate()
        {
            let reply = match how {
                FirstTurn::Generate => first_turn_reply(field),
                FirstTurn::Stream => first_turn_stream_reply(field),
            };
            let server = Server::start(reply).await;
            let active = if index % 2 == 0 { &cloned } else { &provider };
            let active = active.clone().with_base_url(&server.base_url);
            let content = match how {
                FirstTurn::Generate => {
                    within(active.generate("m", hello()))
                        .await
                        .expect("the reply should succeed")
                        .content
                }
                FirstTurn::Stream => {
                    let stream = within(active.stream("m", hello()))
                        .await
                        .expect("the stream should start");
                    within(stream.collect_response())
                        .await
                        .expect("the reply should succeed")
                        .content
                }
            };
            // Both requests receive no reasoning; neither may clear the learned field.
            for shared in [&provider, &cloned] {
                let mut next = Server::start(Reply::json(&json!({
                    "choices": [{ "message": { "content": "Sunny" } }],
                })))
                .await;
                within(
                    shared
                        .clone()
                        .with_base_url(&next.base_url)
                        .generate("m", second_turn_request(content.clone())),
                )
                .await
                .expect("the next turn should succeed");
                let body = next.recorded().await.body;
                let assistant = assistant_message(&body);
                assert_eq!(assistant[field], "I should look the weather up.", "{body}");
                let other = if field == "reasoning" {
                    "reasoning_content"
                } else {
                    "reasoning"
                };
                assert!(assistant.get(other).is_none(), "{body}");
            }
        }
    }
}

#[tokio::test]
async fn without_any_reply_to_learn_from_reasoning_goes_back_as_reasoning_content() {
    // History that did not come from this provider, such as a restored session.
    let content = vec![
        AssistantBlock::reasoning("I should look the weather up."),
        AssistantBlock::ToolCall({
            let mut call = polaris_models::llm::ToolCall::new(
                "call_weather",
                "get_weather",
                json!({ "city": "Oslo" }),
            );
            call.call_id = Some("call_weather".to_owned());
            call
        }),
    ];
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "Sunny in Oslo." } }],
    })))
    .await;
    within(
        server
            .provider()
            .generate("m", second_turn_request(content)),
    )
    .await
    .expect("the request should succeed");
    let body = server.recorded().await.body;
    assert_eq!(
        assistant_message(&body)["reasoning_content"],
        "I should look the weather up."
    );
}

#[tokio::test]
async fn an_explicit_replay_field_overrides_what_the_server_used() {
    // The server answers under `reasoning` but is told to read `reasoning_content`.
    let body = two_turns("reasoning", FirstTurn::Generate, |p| {
        p.with_reasoning_replay(ReasoningReplay::ReasoningContent)
    })
    .await;
    let assistant = assistant_message(&body);
    assert_eq!(
        assistant["reasoning_content"],
        "I should look the weather up."
    );
    assert!(assistant.get("reasoning").is_none(), "{assistant}");

    // And the other way round.
    let body = two_turns("reasoning_content", FirstTurn::Generate, |p| {
        p.with_reasoning_replay(ReasoningReplay::Reasoning)
    })
    .await;
    let assistant = assistant_message(&body);
    assert_eq!(assistant["reasoning"], "I should look the weather up.");
    assert!(assistant.get("reasoning_content").is_none(), "{assistant}");
}

#[tokio::test]
async fn reasoning_can_be_left_out_of_the_replay() {
    let body = two_turns("reasoning_content", FirstTurn::Generate, |p| {
        p.with_reasoning_replay(ReasoningReplay::Omit)
    })
    .await;
    let assistant = assistant_message(&body);
    assert!(assistant.get("reasoning").is_none(), "{assistant}");
    assert!(assistant.get("reasoning_content").is_none(), "{assistant}");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_weather");
}

// ── timeouts and size limits ──────────────────────────────────────────

fn short_timeouts() -> Timeouts {
    Timeouts::new(Duration::from_secs(5), Duration::from_millis(300))
}

#[tokio::test]
async fn a_server_that_never_answers_fails_after_the_idle_timeout() {
    let server = Server::start(Reply {
        silent: true,
        ..Reply::json(&json!({}))
    })
    .await;
    let started = std::time::Instant::now();
    let error = within(
        server
            .provider_with(short_timeouts())
            .generate("m", hello()),
    )
    .await
    .expect_err("a silent server must not hang the call");
    assert!(matches!(error, GenerationError::Http(_)), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_stream_that_stalls_fails_after_the_idle_timeout_keeping_what_arrived() {
    let server = Server::start(Reply {
        stall: true,
        ..Reply::sse(&[chunk(json!({ "content": "Partial" }))], false)
    })
    .await;
    let stream = within(server.provider_with(short_timeouts()).stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let events = collect(stream).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Text(text), .. }) if text == "Partial"
        )),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(Err(GenerationError::Http(_)))),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_long_stream_is_not_cut_off_while_data_keeps_arriving() {
    // Six events 150 ms apart take longer than the 300 ms idle timeout in
    // total, but no single wait does.
    let mut chunks: Vec<Value> = (0..6)
        .map(|i| chunk(json!({ "content": format!("{i}") })))
        .collect();
    chunks.push(finish_chunk("stop"));
    let mut reply = Reply::sse(&chunks, true);
    let body = reply.pieces.remove(0);
    reply.pieces = String::from_utf8(body)
        .expect("SSE text is UTF-8")
        .split_inclusive("\n\n")
        .map(|event| event.as_bytes().to_vec())
        .collect();
    reply.pause = Duration::from_millis(150);
    let server = Server::start(reply).await;
    let stream = within(server.provider_with(short_timeouts()).stream("m", hello()))
        .await
        .expect("the stream request should succeed");
    let response = within(stream.collect_response())
        .await
        .expect("a slow but live stream should finish");
    assert_eq!(response.text(), "012345");
}

#[tokio::test]
async fn a_response_body_over_the_size_limit_is_refused() {
    let server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "x".repeat(500) } }],
    })))
    .await;
    let error = within(
        server
            .provider()
            .with_max_body_bytes(100)
            .generate("m", hello()),
    )
    .await
    .expect_err("the body is over the limit");
    assert!(
        matches!(&error, GenerationError::InvalidResponse(message) if message.contains("100 byte limit")),
        "{error:?}"
    );
}

#[tokio::test]
async fn error_statuses_survive_an_error_body_that_stalls() {
    for status in [
        "401 Unauthorized",
        "403 Forbidden",
        "429 Too Many Requests",
        "502 Bad Gateway",
    ] {
        for streaming in [false, true] {
            let server = Server::start(Reply {
                stall: true,
                ..Reply::with_status(status, "application/json", "{\"error\":".to_owned())
                    .header("Retry-After: 7")
            })
            .await;
            let provider = server.provider_with(short_timeouts());
            let error = if streaming {
                within(provider.stream("m", hello()))
                    .await
                    .err()
                    .expect("the status must fail")
            } else {
                within(provider.generate("m", hello()))
                    .await
                    .expect_err("the status must fail")
            };
            match status {
                "401 Unauthorized" | "403 Forbidden" => assert!(
                    matches!(&error, GenerationError::Auth(message) if message == status),
                    "{error:?}"
                ),
                "429 Too Many Requests" => assert!(
                    matches!(error, GenerationError::RateLimited { retry_after: Some(wait) }
                        if wait == Duration::from_secs(7)),
                    "{error:?}"
                ),
                _ => assert!(
                    matches!(&error, GenerationError::Provider { status: Some(502), message, .. }
                        if message == status),
                    "{error:?}"
                ),
            }
        }
    }
}

#[tokio::test]
async fn a_long_error_body_is_cut_off_but_its_status_is_kept() {
    let server = Server::start(Reply::with_status(
        "502 Bad Gateway",
        "text/html",
        "y".repeat(3 * 1024 * 1024),
    ))
    .await;
    let error = within(server.provider().generate("m", hello()))
        .await
        .expect_err("502 is an error");
    assert!(
        matches!(&error, GenerationError::Provider { status: Some(502), message, .. } if message.len() < 4_000),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_failed_tool_result_is_marked() {
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
}

#[tokio::test]
async fn an_image_tool_result_is_a_note_then_the_image_in_a_user_message() {
    let mut call = polaris_models::llm::ToolCall::new("c1", "read_file", json!({}));
    call.call_id = Some("c1".to_owned());
    let mut other = polaris_models::llm::ToolCall::new("c2", "list_files", json!({}));
    other.call_id = Some("c2".to_owned());
    let mut server = Server::start(Reply::json(&json!({
        "choices": [{ "message": { "content": "ok" } }],
    })))
    .await;
    let image = polaris_models::llm::ImageBlock {
        data: polaris_models::llm::DocumentSource::Base64("QUJD".to_owned()),
        media_type: ImageMediaType::PNG,
        additional_params: None,
    };
    let request = LlmRequest {
        messages: vec![
            Message::user("go"),
            Message::Assistant {
                id: None,
                content: vec![
                    polaris_models::llm::AssistantBlock::ToolCall(call.clone()),
                    polaris_models::llm::AssistantBlock::ToolCall(other.clone()),
                ],
            },
            Message::User {
                content: vec![
                    call.result(ToolResultContent::Image(image)),
                    other.result(ToolResultContent::Text("a.png".to_owned())),
                ],
            },
        ],
        ..Default::default()
    };
    within(server.provider().generate("m", request))
        .await
        .expect("an image result is sent");
    let seen = server.recorded().await;
    let messages = seen.body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 5, "{messages:?}");
    // Both tool messages come straight after the calls, the image one as a note.
    assert_eq!(
        messages[2],
        json!({ "role": "tool", "tool_call_id": "c1", "content": "The result is an image. It is attached in the next message." })
    );
    assert_eq!(
        messages[3],
        json!({ "role": "tool", "tool_call_id": "c2", "content": "a.png" })
    );
    // The image follows as a user message in the parts form.
    assert_eq!(
        messages[4],
        json!({
            "role": "user",
            "content": [{ "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }],
        })
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
        silent: false,
        pause: Duration::from_millis(5),
        stall: false,
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
