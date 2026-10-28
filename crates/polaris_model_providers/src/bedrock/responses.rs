//! Signed Bedrock Responses API calls for `OpenAI` models.
//!
//! Bedrock's Converse endpoint accepts these models but does not expose their
//! Responses API prompt cache. Keep the same Polaris request and response types
//! while using the endpoint on which that cache is available.

use crate::openai::provider::{convert_request, convert_response};
use async_openai::types::responses::Response;
use aws_config::retry::RetryConfig;
use aws_sdk_bedrockruntime::config::ProvideCredentials;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SigningParams, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmRequest,
    LlmResponse, LlmStream, StreamEvent,
};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(3_100);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Builds the HTTP client used by the Responses path. The SDK's operation
/// timeout is applied per HTTP attempt because retries are driven in this
/// module rather than by the SDK client.
pub(super) fn http_client(config: Option<&aws_config::SdkConfig>) -> reqwest::Client {
    let timeouts = config.and_then(aws_config::SdkConfig::timeout_config);
    let connect_timeout = timeouts
        .and_then(aws_smithy_types::timeout::TimeoutConfig::connect_timeout)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    let request_timeout = timeouts
        .and_then(|timeouts| {
            timeouts
                .operation_attempt_timeout()
                .or_else(|| timeouts.operation_timeout())
        })
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
    let builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout);
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
        .build()
        .expect("Bedrock Responses HTTP client configuration should be valid")
}

fn invalid(message: impl Into<String>) -> GenerationError {
    GenerationError::InvalidRequest(message.into())
}

/// Whether a failed call is worth sending again: a transport failure, or a
/// status the service uses for a condition that clears by itself.
fn is_transient(error: &GenerationError) -> bool {
    match error {
        GenerationError::Http(_) | GenerationError::RateLimited { .. } => true,
        GenerationError::Provider {
            status: Some(status),
            ..
        } => matches!(status, 429 | 500 | 502 | 503 | 504),
        _ => false,
    }
}

/// The wait before retry number `retry` (1 is the first): the initial backoff
/// doubled for each earlier retry, never above the maximum.
fn backoff(policy: &RetryConfig, retry: u32) -> Duration {
    let doublings = retry.saturating_sub(1).min(16);
    policy
        .initial_backoff()
        .saturating_mul(1 << doublings)
        .min(policy.max_backoff())
}

/// Runs `call`, sending it again after a transient failure until `policy`'s
/// attempts are used. The Converse path gets the same budget from the SDK
/// client; this call is made without that client, so it applies the same
/// configuration itself (`AWS_MAX_ATTEMPTS` and the SDK defaults).
async fn with_retries<T, F, Fut>(policy: &RetryConfig, mut call: F) -> Result<T, GenerationError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, GenerationError>>,
{
    let attempts = policy.max_attempts().max(1);
    let mut attempt = 1;
    loop {
        match call().await {
            Err(error) if attempt < attempts && is_transient(&error) => {
                let wait = backoff(policy, attempt);
                tracing::warn!(attempt, attempts, ?wait, %error, "retrying Bedrock Responses call");
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            outcome => return outcome,
        }
    }
}

fn responses_url(config: &aws_config::SdkConfig) -> Result<String, GenerationError> {
    let base = match config.endpoint_url() {
        Some(endpoint) => endpoint.trim_end_matches('/').to_owned(),
        None => {
            let region = config
                .region()
                .ok_or_else(|| invalid("Bedrock region is not configured"))?;
            format!("https://bedrock-runtime.{region}.amazonaws.com")
        }
    };
    if base.ends_with("/openai/v1") {
        Ok(format!("{base}/responses"))
    } else {
        Ok(format!("{base}/openai/v1/responses"))
    }
}

/// Build the exact Responses payload used for a request. The cache key follows
/// the stable task prefix across tool round trips; the explicit marker anchors
/// that prefix while implicit caching can also use later message boundaries.
fn payload(model: &str, request: &LlmRequest) -> Result<serde_json::Value, GenerationError> {
    let mut compatible = request.clone();
    compatible.messages = request
        .messages
        .iter()
        .filter_map(super::provider::without_reasoning)
        .collect();
    if let Some(tools) = compatible.tools.as_mut() {
        // Converse sends every tool non-strict. Preserve that behavior on this
        // endpoint, including each tool's original schema.
        for tool in tools {
            tool.strict = false;
        }
    }
    let mut body = serde_json::to_value(convert_request(model, &compatible)?)
        .map_err(|err| invalid(format!("failed to encode Responses request: {err}")))?;
    body["store"] = serde_json::Value::Bool(false);

    if request.cache.is_disabled() {
        body["prompt_cache_options"] = serde_json::json!({"mode": "explicit"});
    } else {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        model.hash(&mut hasher);
        request.system.hash(&mut hasher);
        request.tools.as_ref().map(Vec::len).hash(&mut hasher);
        request
            .messages
            .first()
            .and_then(|message| serde_json::to_string(message).ok())
            .hash(&mut hasher);
        body["prompt_cache_key"] = format!("polaris:{:016x}", hasher.finish()).into();
        body["prompt_cache_options"] = serde_json::json!({"mode": "implicit"});

        // OpenAI's explicit marker belongs on a content block. The earliest
        // user text remains unchanged through the task and is a useful anchor
        // even when later tool outputs have no markable input_text block.
        if request.cache.prefix
            && let Some(items) = body["input"].as_array_mut()
        {
            for item in items {
                if item["role"] == "user" && item["content"].is_string() {
                    let text = item["content"].take();
                    item["content"] = serde_json::json!([{
                        "type": "input_text",
                        "text": text,
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    }]);
                    break;
                }
                if let Some(parts) = item["content"].as_array_mut()
                    && let Some(part) = parts.iter_mut().find(|part| part["type"] == "input_text")
                {
                    part["prompt_cache_breakpoint"] = serde_json::json!({"mode": "explicit"});
                    break;
                }
            }
        }
    }
    Ok(body)
}

/// Signs and sends one attempt, returning the status and body of a successful
/// call. Credentials are loaded and the request signed on every attempt, so a
/// retry never reuses a stale signature.
async fn send_once(
    config: &aws_config::SdkConfig,
    http: &reqwest::Client,
    url: &str,
    body: &bytes::Bytes,
) -> Result<(u16, bytes::Bytes), GenerationError> {
    let region = config
        .region()
        .ok_or_else(|| invalid("Bedrock region is not configured"))?;
    let credentials = config
        .credentials_provider()
        .ok_or_else(|| invalid("Bedrock credentials provider is not configured"))?
        .provide_credentials()
        .await
        .map_err(|err| invalid(format!("failed to load Bedrock credentials: {err}")))?;
    let identity = credentials.into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region.as_ref())
        .name("bedrock")
        .time(SystemTime::now())
        .settings(SigningSettings::default())
        .build()
        .map_err(|err| invalid(format!("failed to configure Bedrock signing: {err}")))?;
    let signing = SigningParams::from(params);
    let signable = SignableRequest::new(
        "POST",
        url,
        [("content-type", "application/json")].into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|err| invalid(format!("failed to prepare Bedrock request: {err}")))?;
    let (instructions, _) = sign(signable, &signing)
        .map_err(|err| invalid(format!("failed to sign Bedrock request: {err}")))?
        .into_parts();
    let mut outbound = http.post(url).header("content-type", "application/json");
    for (name, value) in instructions.headers() {
        outbound = outbound.header(name, value);
    }
    let response = outbound
        .body(body.clone())
        .send()
        .await
        .map_err(|err| GenerationError::Http(err.to_string()))?;
    let status = response.status();
    let data = response
        .bytes()
        .await
        .map_err(|err| GenerationError::Http(err.to_string()))?;
    if !status.is_success() {
        return Err(GenerationError::Provider {
            status: Some(status.as_u16()),
            message: String::from_utf8_lossy(&data).into_owned(),
            source: None,
        });
    }
    Ok((status.as_u16(), data))
}

/// The full Responses request body: [`payload`], with the provider's
/// `additional_fields` laid over its top-level keys. A field the caller sets
/// replaces the payload's own value for that key.
fn request_body(
    model: &str,
    request: &LlmRequest,
    additional_fields: Option<&serde_json::Value>,
) -> Result<serde_json::Value, GenerationError> {
    let mut body = payload(model, request)?;
    if let Some(fields) = additional_fields.and_then(serde_json::Value::as_object) {
        for (key, value) in fields {
            body[key] = value.clone();
        }
    }
    Ok(body)
}

/// Converts the JSON body of a successful Responses call into a Polaris
/// response.
///
/// Bedrock reports prompt-cache writes as `cache_write_tokens`, either under
/// `usage.input_tokens_details` or directly under `usage`. Those tokens are
/// part of `input_tokens`, so they are taken out of it and reported as
/// `cache_creation_tokens`, which keeps each token billed at one tier only.
fn convert_result(status: u16, raw: serde_json::Value) -> Result<LlmResponse, GenerationError> {
    let cache_write = raw["usage"]["input_tokens_details"]["cache_write_tokens"]
        .as_u64()
        .or_else(|| raw["usage"]["cache_write_tokens"].as_u64())
        .unwrap_or_default();
    let parsed: Response =
        serde_json::from_value(raw).map_err(|err| GenerationError::Provider {
            status: Some(status),
            message: format!("invalid Bedrock Responses result: {err}"),
            source: None,
        })?;
    let mut converted = convert_response(parsed)?;
    if cache_write > 0 {
        converted.usage.input_tokens = converted
            .usage
            .input_tokens
            .map(|input| input.saturating_sub(cache_write));
        converted.usage.cache_creation_tokens = Some(cache_write);
    }
    Ok(converted)
}

pub(super) async fn generate(
    config: &aws_config::SdkConfig,
    http: &reqwest::Client,
    model: &str,
    request: &LlmRequest,
    additional_fields: Option<&serde_json::Value>,
) -> Result<LlmResponse, GenerationError> {
    let body = request_body(model, request, additional_fields)?;
    let body = bytes::Bytes::from(
        serde_json::to_vec(&body)
            .map_err(|err| invalid(format!("failed to encode Responses request: {err}")))?,
    );
    let url = responses_url(config)?;
    let policy = config
        .retry_config()
        .cloned()
        .unwrap_or_else(RetryConfig::standard);
    let (status, data) = with_retries(&policy, || send_once(config, http, &url, &body)).await?;
    let raw: serde_json::Value =
        serde_json::from_slice(&data).map_err(|err| GenerationError::Provider {
            status: Some(status),
            message: format!("invalid Bedrock Responses result: {err}"),
            source: None,
        })?;
    convert_result(status, raw)
}

/// Adapt a complete Responses result to Polaris's stream contract. This keeps
/// the agent's existing stream collector and usage accounting unchanged.
pub(super) fn as_stream(response: LlmResponse) -> LlmStream {
    let mut events = Vec::new();
    for (index, block) in response.content.iter().enumerate() {
        let index = index as u32;
        match block {
            AssistantBlock::Text(text) => {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::Text,
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::Text(text.text.clone()),
                }));
            }
            AssistantBlock::ToolCall(call) => {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::ToolCall {
                        id: call.id.clone(),
                        call_id: call.call_id.clone(),
                        name: call.function.name.clone(),
                    },
                }));
                events.push(Ok(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: call.function.arguments.to_string(),
                    },
                }));
            }
            AssistantBlock::Reasoning(reasoning) => {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::Reasoning,
                }));
                for text in &reasoning.reasoning {
                    events.push(Ok(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::Reasoning(text.clone()),
                    }));
                }
            }
        }
        events.push(Ok(StreamEvent::ContentBlockStop { index }));
    }
    events.push(Ok(StreamEvent::MessageDelta {
        usage: response.usage.clone(),
    }));
    events.push(Ok(StreamEvent::MessageStop {
        stop_reason: response.stop_reason,
        usage: response.usage,
        id: response.id,
        model: response.model,
    }));
    Box::pin(futures_util::stream::iter(events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_models::llm::{CacheControl, Message, ToolDefinition, ToolResultContent};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn read_request(socket: &mut TcpStream) {
        let mut received = Vec::new();
        let body_start = loop {
            if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
            let mut chunk = [0; 4096];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before sending request headers");
            received.extend_from_slice(&chunk[..read]);
        };
        let headers = str::from_utf8(&received[..body_start]).unwrap();
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.trim().parse().unwrap());
        while received.len() < body_start + content_length {
            let mut chunk = [0; 4096];
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "client closed before sending the request body");
            received.extend_from_slice(&chunk[..read]);
        }
    }

    struct CannedServer {
        endpoint: String,
        task: tokio::task::JoinHandle<usize>,
    }

    impl CannedServer {
        async fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let mut requests = 0;
                for response in responses {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    read_request(&mut socket).await;
                    requests += 1;
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
                requests
            });
            Self {
                endpoint: format!("http://{address}"),
                task,
            }
        }

        async fn request_count(self) -> usize {
            tokio::time::timeout(Duration::from_secs(2), self.task)
                .await
                .expect("timed out waiting for loopback requests")
                .expect("loopback server task should not panic")
        }
    }

    struct HangingServer {
        endpoint: String,
        accepted: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl HangingServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accepted = Arc::new(AtomicUsize::new(0));
            let accepted_by_task = Arc::clone(&accepted);
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                accepted_by_task.fetch_add(1, Ordering::SeqCst);
                read_request(&mut socket).await;
                std::future::pending::<()>().await;
            });
            Self {
                endpoint: format!("http://{address}"),
                accepted,
                task,
            }
        }

        fn stop(self) -> usize {
            self.task.abort();
            self.accepted.load(Ordering::SeqCst)
        }
    }

    fn request_config(
        endpoint: &str,
        attempts: u32,
        request_timeout: Duration,
    ) -> aws_config::SdkConfig {
        let credentials = aws_sdk_bedrockruntime::config::Credentials::new(
            "test-access-key",
            "test-secret-key",
            None,
            None,
            "bedrock responses test",
        );
        aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_bedrockruntime::config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .credentials_provider(
                aws_sdk_bedrockruntime::config::SharedCredentialsProvider::new(credentials),
            )
            .retry_config(quick_policy(attempts))
            .timeout_config(
                aws_config::timeout::TimeoutConfig::builder()
                    .connect_timeout(Duration::from_millis(100))
                    .operation_attempt_timeout(request_timeout)
                    .build(),
            )
            .build()
    }

    fn successful_result_body() -> String {
        result_json(serde_json::json!({
            "input_tokens": 1,
            "output_tokens": 1,
            "total_tokens": 2,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }))
        .to_string()
    }

    fn hello() -> LlmRequest {
        LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn responses_request_retries_503_then_succeeds() {
        let server = CannedServer::start(vec![
            json_response("503 Service Unavailable", "try again"),
            json_response("200 OK", &successful_result_body()),
        ])
        .await;
        let config = request_config(&server.endpoint, 2, Duration::from_secs(1));
        let client = http_client(Some(&config));

        let response = generate(&config, &client, "openai.gpt-test", &hello(), None)
            .await
            .expect("the second response should succeed");

        assert!(response.content.is_empty());
        assert_eq!(server.request_count().await, 2);
    }

    #[tokio::test]
    async fn responses_request_does_not_retry_400() {
        let server = CannedServer::start(vec![json_response("400 Bad Request", "invalid")]).await;
        let config = request_config(&server.endpoint, 3, Duration::from_secs(1));
        let client = http_client(Some(&config));

        let outcome = generate(&config, &client, "openai.gpt-test", &hello(), None).await;

        assert!(matches!(
            outcome,
            Err(GenerationError::Provider {
                status: Some(400),
                ..
            })
        ));
        assert_eq!(server.request_count().await, 1);
    }

    #[tokio::test]
    async fn responses_request_stops_when_the_retry_budget_is_exhausted() {
        let unavailable = || json_response("503 Service Unavailable", "try again");
        let server = CannedServer::start(vec![unavailable(), unavailable(), unavailable()]).await;
        let config = request_config(&server.endpoint, 3, Duration::from_secs(1));
        let client = http_client(Some(&config));

        let outcome = generate(&config, &client, "openai.gpt-test", &hello(), None).await;

        assert!(matches!(
            outcome,
            Err(GenerationError::Provider {
                status: Some(503),
                ..
            })
        ));
        assert_eq!(server.request_count().await, 3);
    }

    #[tokio::test]
    async fn sdk_request_timeout_stops_a_server_that_never_answers() {
        let server = HangingServer::start().await;
        let config = request_config(&server.endpoint, 1, Duration::from_millis(25));
        let client = http_client(Some(&config));

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            generate(&config, &client, "openai.gpt-test", &hello(), None),
        )
        .await
        .expect("the configured request timeout should fire");

        assert!(matches!(outcome, Err(GenerationError::Http(_))));
        assert_eq!(
            server.stop(),
            1,
            "the request must reach the hanging server"
        );
    }

    #[test]
    fn tool_round_trip_keeps_cache_prefix_byte_stable() {
        let model = "global.openai.gpt-6-luna";
        let first = LlmRequest {
            system: Some("Fixed instructions".into()),
            messages: vec![Message::user("Fix the example")],
            tools: Some(vec![ToolDefinition::new(
                "read_file",
                "Read a file",
                serde_json::json!({"type": "object"}),
            )]),
            cache: CacheControl::prefix().with_breakpoints([0]),
            ..Default::default()
        };
        let mut later = first.clone();
        later.messages.push(Message::tool_result_with_call_id(
            "item-1",
            "call-1",
            ToolResultContent::Text("file contents".into()),
        ));
        later.cache = CacheControl::prefix().with_breakpoints([0, 1]);

        let original = payload(model, &first).unwrap();
        let next = payload(model, &later).unwrap();
        assert_eq!(original["instructions"], next["instructions"]);
        assert_eq!(original["tools"], next["tools"]);
        assert_eq!(original["input"][0], next["input"][0]);
        assert_eq!(original["prompt_cache_key"], next["prompt_cache_key"]);
        assert_eq!(
            original["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"], "explicit",
            "{original}"
        );
        assert_eq!(original["prompt_cache_options"]["mode"], "implicit");
        assert_eq!(original["tools"][0]["strict"], false);
    }

    fn provider_error(status: u16) -> GenerationError {
        GenerationError::Provider {
            status: Some(status),
            message: "failed".into(),
            source: None,
        }
    }

    fn quick_policy(attempts: u32) -> RetryConfig {
        RetryConfig::standard()
            .with_max_attempts(attempts)
            .with_initial_backoff(Duration::from_millis(1))
            .with_max_backoff(Duration::from_millis(4))
    }

    #[test]
    fn only_conditions_that_clear_by_themselves_are_transient() {
        for status in [429, 500, 502, 503, 504] {
            assert!(is_transient(&provider_error(status)), "{status}");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!is_transient(&provider_error(status)), "{status}");
        }
        assert!(is_transient(&GenerationError::Http("reset".into())));
        assert!(is_transient(&GenerationError::RateLimited {
            retry_after: None
        }));
        assert!(!is_transient(&invalid("bad request")));
    }

    #[test]
    fn backoff_doubles_up_to_the_maximum() {
        let policy = RetryConfig::standard()
            .with_initial_backoff(Duration::from_secs(1))
            .with_max_backoff(Duration::from_secs(5));
        let waits: Vec<_> = (1..=5).map(|retry| backoff(&policy, retry)).collect();
        let expected = [1, 2, 4, 5, 5].map(Duration::from_secs);
        assert_eq!(waits, expected);
        assert_eq!(backoff(&policy, u32::MAX), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_transient_failure_is_sent_again_until_it_succeeds() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let outcome = with_retries(&quick_policy(3), || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 | 1 => Err(provider_error(500)),
                _ => Ok("done"),
            }
        })
        .await;
        assert_eq!(outcome.unwrap(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_stop_at_the_attempt_budget() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let outcome: Result<(), _> = with_retries(&quick_policy(3), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(provider_error(503))
        })
        .await;
        assert!(matches!(
            outcome,
            Err(GenerationError::Provider {
                status: Some(503),
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failure_that_will_not_clear_is_returned_at_once() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let outcome: Result<(), _> = with_retries(&quick_policy(3), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(provider_error(400))
        })
        .await;
        assert!(
            matches!(
                &outcome,
                Err(GenerationError::Provider {
                    status: Some(400),
                    ..
                })
            ),
            "the original 400 must be returned unchanged: {outcome:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a failure that will not clear must not be sent again"
        );
    }

    // ── responses_url ──

    fn sdk_config(region: Option<&'static str>, endpoint: Option<&str>) -> aws_config::SdkConfig {
        let mut builder = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(region.map(aws_sdk_bedrockruntime::config::Region::new));
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        builder.build()
    }

    #[test]
    fn url_is_derived_from_the_region() {
        let config = sdk_config(Some("us-west-2"), None);
        assert_eq!(
            responses_url(&config).unwrap(),
            "https://bedrock-runtime.us-west-2.amazonaws.com/openai/v1/responses"
        );
    }

    #[test]
    fn endpoint_override_replaces_the_region_host() {
        let config = sdk_config(Some("us-west-2"), Some("https://bedrock.example.test/"));
        assert_eq!(
            responses_url(&config).unwrap(),
            "https://bedrock.example.test/openai/v1/responses",
            "the trailing slash is dropped and the Responses path appended"
        );
    }

    #[test]
    fn endpoint_that_already_names_the_openai_path_is_not_extended_twice() {
        let config = sdk_config(None, Some("https://bedrock.example.test/openai/v1/"));
        assert_eq!(
            responses_url(&config).unwrap(),
            "https://bedrock.example.test/openai/v1/responses"
        );
    }

    #[test]
    fn missing_region_and_endpoint_is_an_invalid_request() {
        let outcome = responses_url(&sdk_config(None, None));
        assert!(
            matches!(
                &outcome,
                Err(GenerationError::InvalidRequest(message)) if message.contains("region")
            ),
            "{outcome:?}"
        );
    }

    // ── payload and request_body ──

    #[test]
    fn disabled_cache_sends_explicit_mode_and_no_cache_key() {
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        };
        assert!(request.cache.is_disabled());

        let body = payload("openai.gpt-oss-120b", &request).unwrap();
        assert_eq!(body["prompt_cache_options"]["mode"], "explicit", "{body}");
        assert!(body.get("prompt_cache_key").is_none(), "{body}");
        assert_eq!(body["store"], false);
        assert!(
            !body.to_string().contains("prompt_cache_breakpoint"),
            "no content block is marked when caching is off: {body}"
        );
    }

    #[test]
    fn additional_fields_are_laid_over_the_payload() {
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        };
        let fields = serde_json::json!({
            "reasoning": {"effort": "high"},
            "store": true,
        });

        let body = request_body("openai.gpt-oss-120b", &request, Some(&fields)).unwrap();
        assert_eq!(
            body["reasoning"],
            serde_json::json!({"effort": "high"}),
            "{body}"
        );
        assert_eq!(
            body["store"], true,
            "a caller field replaces the payload's value"
        );
        assert_eq!(body["model"], "openai.gpt-oss-120b", "other keys are kept");

        let plain = request_body("openai.gpt-oss-120b", &request, None).unwrap();
        assert_eq!(plain, payload("openai.gpt-oss-120b", &request).unwrap());
    }

    // ── convert_result ──

    fn result_json(usage: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "resp_test",
            "created_at": 0,
            "model": "openai.gpt-oss-120b",
            "object": "response",
            "output": [],
            "parallel_tool_calls": true,
            "status": "completed",
            "usage": usage,
        })
    }

    #[test]
    fn cache_writes_are_taken_out_of_input_tokens() {
        let raw = result_json(serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "total_tokens": 105,
            "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 30},
            "output_tokens_details": {"reasoning_tokens": 0}
        }));

        let usage = convert_result(200, raw).unwrap().usage;
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.cache_creation_tokens, Some(30));
        assert_eq!(
            usage.input_tokens,
            Some(50),
            "100 input tokens, less 20 cache reads and 30 cache writes"
        );
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn cache_writes_reported_directly_under_usage_are_counted() {
        let raw = result_json(serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "total_tokens": 105,
            "cache_write_tokens": 40,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }));

        let usage = convert_result(200, raw).unwrap().usage;
        assert_eq!(usage.cache_creation_tokens, Some(40));
        assert_eq!(usage.input_tokens, Some(60));
    }

    #[test]
    fn no_cache_writes_leave_usage_as_converted() {
        let raw = result_json(serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "total_tokens": 105,
            "input_tokens_details": {"cached_tokens": 20},
            "output_tokens_details": {"reasoning_tokens": 0}
        }));

        let usage = convert_result(200, raw).unwrap().usage;
        assert_eq!(usage.cache_creation_tokens, None);
        assert_eq!(usage.input_tokens, Some(80));
    }

    #[test]
    fn a_result_that_is_not_a_response_is_a_provider_error() {
        let outcome = convert_result(200, serde_json::json!({"unexpected": true}));
        assert!(
            matches!(
                &outcome,
                Err(GenerationError::Provider {
                    status: Some(200),
                    message,
                    ..
                }) if message.contains("invalid Bedrock Responses result")
            ),
            "{outcome:?}"
        );
    }

    // ── as_stream ──

    #[tokio::test]
    async fn stream_replays_each_block_then_usage_and_stop() {
        use futures_util::StreamExt as _;
        use polaris_models::llm::{ReasoningBlock, StopReason, ToolCall, Usage};

        let usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            ..Default::default()
        };
        let response = LlmResponse {
            content: vec![
                AssistantBlock::text("Looking"),
                AssistantBlock::ToolCall(ToolCall {
                    call_id: Some("call-1".into()),
                    ..ToolCall::new("item-1", "read_file", serde_json::json!({"path": "a.rs"}))
                }),
                AssistantBlock::Reasoning(ReasoningBlock {
                    id: None,
                    reasoning: vec!["first".into(), "second".into()],
                    signature: None,
                }),
            ],
            usage: usage.clone(),
            stop_reason: StopReason::ToolUse,
            id: Some("resp-1".into()),
            model: Some("openai.gpt-oss-120b".into()),
        };

        let events: Vec<StreamEvent> = as_stream(response)
            .map(|event| event.expect("every replayed event is Ok"))
            .collect()
            .await;

        let expected = vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("Looking".into()),
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::ToolCall {
                    id: "item-1".into(),
                    call_id: Some("call-1".into()),
                    name: "read_file".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::ToolCall {
                    arguments: serde_json::json!({"path": "a.rs"}).to_string(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::ContentBlockStart {
                index: 2,
                block: ContentBlockStartData::Reasoning,
            },
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::Reasoning("first".into()),
            },
            StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::Reasoning("second".into()),
            },
            StreamEvent::ContentBlockStop { index: 2 },
            StreamEvent::MessageDelta {
                usage: usage.clone(),
            },
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage,
                id: Some("resp-1".into()),
                model: Some("openai.gpt-oss-120b".into()),
            },
        ];
        assert_eq!(events, expected);
    }
}
