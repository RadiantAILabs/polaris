//! Tests that run `OpenAiProvider` against a local HTTP server.

use super::*;
use core::time::Duration;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BYTES: usize = 64 * 1024;

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out waiting on the loopback server or provider")
}

/// Serves one canned response and records the complete JSON request body.
struct Server {
    base_url: String,
    task: JoinHandle<Value>,
}

impl Server {
    async fn start(content_type: &str, body: String) -> Self {
        let listener = within(TcpListener::bind("127.0.0.1:0"))
            .await
            .expect("should bind a local port");
        let addr = listener.local_addr().expect("listener should have an addr");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(within(async move {
            let (mut socket, _) = listener.accept().await.expect("should accept a connection");
            let request = read_request_body(&mut socket).await;
            socket
                .write_all(response.as_bytes())
                .await
                .expect("should write the canned response");
            socket.shutdown().await.expect("should close the socket");
            request
        }));
        Self {
            base_url: format!("http://{addr}"),
            task,
        }
    }

    fn provider(&self) -> OpenAiProvider {
        OpenAiProvider {
            client: async_openai::Client::with_config(
                OpenAIConfig::new()
                    .with_api_key("test")
                    .with_api_base(&self.base_url),
            ),
            pricing: PriceStore::builtin(),
        }
    }

    async fn request_body(&mut self) -> Value {
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

async fn read_request_body(socket: &mut TcpStream) -> Value {
    let mut received = Vec::new();
    let body_start = loop {
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        read_more(socket, &mut received).await;
    };
    let headers = core::str::from_utf8(&received[..body_start]).expect("headers should be UTF-8");
    assert!(
        headers.starts_with("POST /responses HTTP/1.1\r\n"),
        "{headers}"
    );
    let content_length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .expect("request should carry a Content-Length header")
        .1
        .trim()
        .parse()
        .expect("Content-Length should be a number");
    assert!(content_length <= MAX_REQUEST_BYTES - body_start);
    let body_end = body_start + content_length;
    while received.len() < body_end {
        read_more(socket, &mut received).await;
    }
    serde_json::from_slice(&received[body_start..body_end]).expect("request body should be JSON")
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

fn sparse_text_response() -> Value {
    // No annotations, created_at, object, total_tokens, or usage details.
    json!({
        "id": "resp_sparse",
        "model": "compatible-model",
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "Sunny" }],
        }],
        "usage": { "input_tokens": 7, "output_tokens": 3 },
    })
}

async fn assert_tool_result_round_trip(status: ToolResultStatus) {
    let mut server = Server::start("application/json", sparse_text_response().to_string()).await;
    let provider = server.provider();
    let mut call = ToolCall::new("fc_weather", "get_weather", json!({ "city": "Oslo" }));
    call.call_id = Some("call_weather".to_owned());
    let (result_message, payload, expected_output) = match status {
        ToolResultStatus::Success => (
            Message::tool_result_for(&call, PolarisToolResult::Text("Sunny".to_owned())),
            "Sunny",
            "Sunny",
        ),
        ToolResultStatus::Error => (
            Message::tool_error_for(
                &call,
                PolarisToolResult::Text("Weather unavailable".to_owned()),
            ),
            "Weather unavailable",
            "Error: Weather unavailable",
        ),
    };
    let Message::User { content } = &result_message else {
        panic!("expected a user tool result, got {result_message:?}");
    };
    let [UserBlock::ToolResult(result)] = content.as_slice() else {
        panic!("expected one tool result, got {content:?}");
    };
    assert_eq!(result.id, call.id);
    assert_eq!(result.call_id, call.call_id);
    assert_eq!(result.status, status);
    assert!(
        matches!(&result.content, PolarisToolResult::Text(text) if text == payload),
        "{result:?}"
    );

    let request = LlmRequest {
        messages: vec![
            Message::user("What is the weather in Oslo?"),
            Message::assistant_tool_call(call),
            result_message,
        ],
        ..Default::default()
    };
    let response = within(provider.generate("compatible-model", request))
        .await
        .expect("tool result conversation should generate a response");
    assert_eq!(response.text(), "Sunny");

    let body = server.request_body().await;
    let outputs: Vec<_> = body["input"]
        .as_array()
        .expect("input should be an array")
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(outputs.len(), 1, "{body}");
    assert_eq!(outputs[0]["call_id"], "call_weather", "{body}");
    assert_eq!(outputs[0]["output"], expected_output, "{body}");
}

#[tokio::test]
async fn generate_sends_tool_result_with_provider_call_id() {
    assert_tool_result_round_trip(ToolResultStatus::Success).await;
}

#[tokio::test]
async fn generate_sends_tool_error_with_provider_call_id() {
    assert_tool_result_round_trip(ToolResultStatus::Error).await;
}

#[tokio::test]
async fn generate_sends_every_reasoning_effort_as_written() {
    for (effort, wire) in [
        (ReasoningEffort::Minimal, "minimal"),
        (ReasoningEffort::XHigh, "xhigh"),
        // `async_openai`'s own effort type has neither of these.
        (ReasoningEffort::Max, "max"),
        (ReasoningEffort::Other("ultra".to_owned()), "ultra"),
    ] {
        let mut server =
            Server::start("application/json", sparse_text_response().to_string()).await;
        let provider = server.provider();
        within(provider.generate(
            "compatible-model",
            LlmRequest {
                messages: vec![Message::user("Think hard.")],
                reasoning_effort: Some(effort.clone()),
                ..Default::default()
            },
        ))
        .await
        .expect("the request should succeed");

        let body = server.request_body().await;
        assert_eq!(
            body["reasoning"],
            json!({ "effort": wire }),
            "{effort:?}: {body}"
        );
    }
}

#[tokio::test]
async fn generate_without_reasoning_effort_sends_no_reasoning_object() {
    let mut server = Server::start("application/json", sparse_text_response().to_string()).await;
    let provider = server.provider();
    within(provider.generate(
        "compatible-model",
        LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        },
    ))
    .await
    .expect("the request should succeed");

    let body = server.request_body().await;
    assert!(body.get("reasoning").is_none(), "{body}");
}

#[tokio::test]
async fn generate_accepts_sparse_response_over_http() {
    let mut server = Server::start("application/json", sparse_text_response().to_string()).await;
    let provider = server.provider();
    let response = within(provider.generate(
        "compatible-model",
        LlmRequest {
            messages: vec![Message::user("What is the weather?")],
            ..Default::default()
        },
    ))
    .await
    .expect("sparse response should generate successfully");

    assert_eq!(response.text(), "Sunny");
    assert_eq!(response.content.len(), 1, "{response:?}");
    assert_eq!(response.id.as_deref(), Some("resp_sparse"));
    assert_eq!(response.model.as_deref(), Some("compatible-model"));
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: Some(7),
            output_tokens: Some(3),
            total_tokens: Some(10),
            cache_read_tokens: Some(0),
            reasoning_output_tokens: Some(0),
            ..Default::default()
        }
    );
    let body = server.request_body().await;
    assert_eq!(body["model"], "compatible-model", "{body}");
}

#[tokio::test]
async fn stream_accepts_sparse_sse_and_requests_streaming() {
    // Every event omits sequence_number and output_index. Tool identity is
    // supplied only in output_item.done, after two argument deltas.
    let events = [
        json!({ "type": "response.created", "response": {} }),
        json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "id": "fc_weather" },
        }),
        json!({ "type": "response.vendor.heartbeat" }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_weather",
            "delta": "{\"city\":",
        }),
        json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_weather",
            "delta": "\"Oslo\"}",
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "fc_weather",
                "name": "get_weather",
                "call_id": "call_weather",
                "arguments": "{\"city\":\"Oslo\"}",
            },
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_stream",
                "model": "compatible-model",
                "output": [{
                    "type": "function_call",
                    "name": "get_weather",
                    "call_id": "call_weather",
                    "arguments": "{\"city\":\"Oslo\"}",
                }],
                "usage": { "input_tokens": 7, "output_tokens": 3 },
            },
        }),
    ];
    let mut sse: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    sse.push_str("data: [DONE]\n\n");
    let mut server = Server::start("text/event-stream", sse).await;
    let provider = server.provider();
    let stream = within(provider.stream(
        "compatible-model",
        LlmRequest {
            messages: vec![Message::user("What is the weather in Oslo?")],
            ..Default::default()
        },
    ))
    .await
    .expect("stream request should succeed");
    let received: Vec<_> = within(
        stream
            .map(|event| event.expect("sparse SSE event should convert"))
            .collect(),
    )
    .await;

    let usage = Usage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        total_tokens: Some(10),
        cache_read_tokens: Some(0),
        reasoning_output_tokens: Some(0),
        ..Default::default()
    };
    assert_eq!(
        received,
        vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall {
                    id: "fc_weather".to_owned(),
                    call_id: Some("call_weather".to_owned()),
                    name: "get_weather".to_owned(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{\"city\":".to_owned()
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "\"Oslo\"}".to_owned()
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                usage: usage.clone()
            },
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage,
                id: Some("resp_stream".to_owned()),
                model: Some("compatible-model".to_owned()),
            },
        ]
    );
    let body = server.request_body().await;
    assert_eq!(body["stream"], true, "{body}");
    assert_eq!(body["model"], "compatible-model", "{body}");
}
