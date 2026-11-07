//! The Chat Completions [`LlmProvider`].

use super::request::{BodySettings, build_body};
use super::response::{error_message, parse_response, response_reasoning_field};
use super::stream::ChatStreamAdapter;
use super::{MaxTokensField, ReasoningReplay};
use crate::pricing::PriceStore;
use crate::sse::SseDataStream;
use core::time::Duration;
use polaris_models::llm::{
    GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream, ModelPricing,
};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::Value;

/// The base URL used until [`ChatCompletionsProvider::with_base_url`] sets
/// another.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The largest non-streaming response body that is read. Streams have their
/// own limit per event.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Time limits for requests to a server.
///
/// There is deliberately no limit on the whole request: a model that reasons
/// for many minutes while it streams must not be cut off. Instead:
///
/// - `connect` bounds opening the connection;
/// - `idle` bounds every wait for more data, so a server that accepts the
///   request and then goes silent, or a stream that stalls, fails with
///   [`GenerationError::Http`] after `idle`. For a **non-streaming** call the
///   server sends nothing until the whole answer is ready, so `idle` must be
///   longer than the longest answer you will wait for. A streaming call sends
///   data as it goes and is not affected.
///
/// The defaults are 30 seconds to connect and 5 minutes of silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    connect: Duration,
    idle: Duration,
}

impl Timeouts {
    /// Creates limits from a connect timeout and an idle (per read) timeout.
    #[must_use]
    pub const fn new(connect: Duration, idle: Duration) -> Self {
        Self { connect, idle }
    }

    /// The connect timeout.
    #[must_use]
    pub const fn connect(&self) -> Duration {
        self.connect
    }

    /// The idle timeout.
    #[must_use]
    pub const fn idle(&self) -> Duration {
        self.idle
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self::new(Duration::from_secs(30), Duration::from_secs(300))
    }
}

/// The HTTP client settings every provider starts from.
///
/// Some servers sit behind gateways that refuse a request with no
/// `User-Agent`, so one that names the library is sent.
pub(super) fn client_builder(timeouts: Timeouts) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("polaris-ai/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.idle)
}

/// The longest part of an error body kept in an error message.
const MAX_ERROR_BODY_CHARS: usize = 2_000;

/// [`LlmProvider`] for servers that implement the `OpenAI` Chat Completions API.
///
/// Registered as `chat_completions`; models are addressed as
/// `chat_completions/<model>` with `<model>` sent verbatim.
///
/// # What is sent
///
/// `POST {base_url}/chat/completions` with `Authorization: Bearer <key>` (no
/// header when built [`without_api_key`](Self::without_api_key)) and a JSON body
/// built from the [`LlmRequest`]:
///
/// | Request | Body |
/// |---------|------|
/// | `system` | a first `system` message |
/// | messages | `user` messages (text as a string; with images, as `text` / `image_url` parts), `assistant` messages with `tool_calls`, and one `tool` message per tool result |
/// | `tools`, `tool_choice` | `tools`, `tool_choice`; no `strict` unless [`with_strict_tools`](Self::with_strict_tools) |
/// | `output_schema` | `response_format` of type `json_schema` |
/// | `max_output_tokens` | `max_completion_tokens`, or `max_tokens` with [`with_max_tokens_field`](Self::with_max_tokens_field) |
/// | `reasoning_effort` | `reasoning_effort`, as the level's name |
/// | streaming | `stream: true` and `stream_options.include_usage: true` |
///
/// A tool result's text gets an `Error: ` prefix when it reports a failure.
/// Earlier reasoning is sent back on assistant messages under the field the server used for it, `reasoning_content` or `reasoning`
/// (see [`ReasoningReplay`]). Audio and document blocks, and images in tool
/// results, fail with [`GenerationError::UnsupportedContent`].
///
/// [`with_extra_body`](Self::with_extra_body) adds fields that Polaris has no
/// request type for, such as `chat_template_kwargs`.
///
/// # What is read
///
/// Responses are read leniently, so servers that leave fields out still work:
///
/// - reasoning is read from `reasoning_content`, else `reasoning`, and returned
///   as a reasoning block ahead of the answer;
/// - `content` may be a string, `null` or a list of parts;
/// - a tool call without an `id` gets one made from the response id and its
///   position; `arguments` may be a JSON string, an object or empty (no
///   arguments); arguments that are not valid JSON fail the response with
///   [`GenerationError::InvalidResponse`], as the stream collector does;
/// - `finish_reason` `length` is [`StopReason::MaxOutputTokens`], `tool_calls`
///   is [`StopReason::ToolUse`], `content_filter` is
///   [`StopReason::ContentFilter`]; `stop` is
///   [`StopReason::EndTurn`], or [`StopReason::ToolUse`] when the message has
///   tool calls; a missing reason is inferred the same way; any other value is
///   [`StopReason::Other`];
/// - usage: `input_tokens` is `prompt_tokens` without the cached part
///   (`prompt_tokens_details.cached_tokens`, or `prompt_cache_hit_tokens`),
///   `cache_read_tokens` is the cached part, `reasoning_output_tokens` is
///   `completion_tokens_details.reasoning_tokens`; whatever a server leaves out
///   stays `None`.
///
/// A stream ends at `data: [DONE]` or when the connection closes after the
/// server reported a finish reason. The final usage chunk, which servers send
/// after the chunk with the finish reason, is included. A stream that closes
/// before any finish reason fails with [`GenerationError::InvalidResponse`].
///
/// # Errors
///
/// A non-success status becomes [`GenerationError::Auth`] for `401` and `403`,
/// [`GenerationError::RateLimited`] for `429` (reading `Retry-After` when it is
/// a number of seconds), and [`GenerationError::Provider`] with the status and
/// the server's message for anything else.
///
/// [`StopReason`]: polaris_models::llm::StopReason
/// [`StopReason::MaxOutputTokens`]: polaris_models::llm::StopReason::MaxOutputTokens
/// [`StopReason::ToolUse`]: polaris_models::llm::StopReason::ToolUse
/// [`StopReason::ContentFilter`]: polaris_models::llm::StopReason::ContentFilter
/// [`StopReason::EndTurn`]: polaris_models::llm::StopReason::EndTurn
/// [`StopReason::Other`]: polaris_models::llm::StopReason::Other
///
/// # Examples
///
/// ```no_run
/// use polaris_model_providers::chat_completions::ChatCompletionsProvider;
///
/// let provider = ChatCompletionsProvider::new("api-key")
///     .with_base_url("https://router.huggingface.co/v1");
/// # let _ = provider;
/// ```
#[derive(Clone)]
pub struct ChatCompletionsProvider {
    client: reqwest::Client,
    max_body_bytes: usize,
    api_key: Option<String>,
    base_url: String,
    settings: BodySettings,
    pricing: PriceStore,
}

impl ChatCompletionsProvider {
    /// Creates a provider that sends `api_key` as a bearer token to
    /// `https://api.openai.com/v1`. Use [`with_base_url`](Self::with_base_url)
    /// for another server.
    ///
    /// Pricing defaults to [`PriceStore::builtin`]: the built-in price table,
    /// with no network access and no files.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::build(Some(api_key.into()))
    }

    /// Creates a provider that sends no `Authorization` header, for local
    /// servers that need no key.
    #[must_use]
    pub fn without_api_key() -> Self {
        Self::build(None)
    }

    fn build(api_key: Option<String>) -> Self {
        let client = client_builder(Timeouts::default())
            .build()
            .expect("a client with only a user agent and timeouts set should build");
        Self {
            client,
            max_body_bytes: MAX_BODY_BYTES,
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            settings: BodySettings::default(),
            pricing: PriceStore::builtin(),
        }
    }

    /// Sets the server root that `/chat/completions` is appended to (default
    /// `https://api.openai.com/v1`). A trailing `/` is ignored. No environment
    /// variable is read.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::chat_completions::ChatCompletionsProvider;
    /// use polaris_models::llm::LlmProvider;
    ///
    /// let provider = ChatCompletionsProvider::without_api_key()
    ///     .with_base_url("http://127.0.0.1:8000/v1/");
    /// assert_eq!(provider.endpoint().as_deref(), Some("http://127.0.0.1:8000/v1"));
    /// ```
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Adds fields to the top level of every request body, replacing a field
    /// Polaris set for the same key.
    ///
    /// This is the place for settings that Polaris has no request type for:
    /// Z.ai's `thinking`, vLLM's `chat_template_kwargs`, `top_k`, and so on.
    /// `fields` must be a JSON object; any other value makes every request fail
    /// with [`GenerationError::InvalidRequest`].
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::chat_completions::ChatCompletionsProvider;
    ///
    /// let provider = ChatCompletionsProvider::new("api-key").with_extra_body(
    ///     serde_json::json!({ "thinking": { "type": "enabled", "clear_thinking": false } }),
    /// );
    /// # let _ = provider;
    /// ```
    #[must_use]
    pub fn with_extra_body(mut self, fields: Value) -> Self {
        self.settings.extra_body = Some(fields);
        self
    }

    /// Chooses the request key that carries the output token limit (default
    /// [`MaxTokensField::MaxCompletionTokens`]).
    #[must_use]
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.settings.max_tokens_field = field;
        self
    }

    /// Chooses how reasoning from earlier turns is sent back (default
    /// [`ReasoningReplay::AsReceived`]).
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: ReasoningReplay) -> Self {
        self.settings.reasoning_replay = replay;
        self
    }

    /// Sends `strict` with each tool definition, and normalizes the schema of
    /// a strict tool for strict mode (default `false`).
    ///
    /// Off by default: `strict` is an `OpenAI` field, and servers that do not
    /// implement it may ignore or reject it. With it on, each
    /// [`ToolDefinition::strict`](polaris_models::llm::ToolDefinition::strict)
    /// is sent as given.
    #[must_use]
    pub fn with_strict_tools(mut self, strict: bool) -> Self {
        self.settings.strict_tools = strict;
        self
    }

    /// Replaces this provider's price store (default: [`PriceStore::builtin`]).
    ///
    /// Pass [`PriceStore::live`] to fetch current prices from the web and
    /// cache them on disk; see [`crate::pricing`] for exactly when.
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }

    /// Adds a per-model pricing override for this provider only, beating
    /// whatever its price store says for models starting with `prefix`.
    ///
    /// Call it after [`with_pricing_store`](Self::with_pricing_store), which
    /// replaces the store together with its overrides.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_model_providers::chat_completions::ChatCompletionsProvider;
    /// use polaris_models::llm::{LlmProvider, ModelPricing};
    ///
    /// let provider = ChatCompletionsProvider::without_api_key()
    ///     .with_pricing_override("zai-org/GLM-5.3", ModelPricing::new(0.3, 1.2));
    /// assert_eq!(
    ///     provider.pricing("zai-org/GLM-5.3-Flash:zai"),
    ///     Some(ModelPricing::new(0.3, 1.2))
    /// );
    /// ```
    #[must_use]
    pub fn with_pricing_override(
        mut self,
        prefix: impl Into<String>,
        pricing: ModelPricing,
    ) -> Self {
        self.pricing = self.pricing.with_override(prefix, pricing);
        self
    }

    /// Sets how long a connection may take to open and how long a response may
    /// go silent (default: [`Timeouts::default`]). See [`Timeouts`].
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be built, which does not happen for
    /// any pair of durations.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::time::Duration;
    /// use polaris_model_providers::chat_completions::{ChatCompletionsProvider, Timeouts};
    ///
    /// let provider = ChatCompletionsProvider::without_api_key()
    ///     .with_timeouts(Timeouts::new(Duration::from_secs(10), Duration::from_secs(90)));
    /// # let _ = provider;
    /// ```
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.client = client_builder(timeouts)
            .build()
            .expect("a client with only a user agent and timeouts set should build");
        self
    }

    /// Replaces the HTTP client, so tests can keep a loopback request from
    /// going through a proxy named in the environment.
    #[cfg(test)]
    pub(crate) fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Lowers the size limit of a non-streaming response body, so a test does
    /// not need a 16 MiB body.
    #[cfg(test)]
    pub(crate) fn with_max_body_bytes(mut self, limit: usize) -> Self {
        self.max_body_bytes = limit;
        self
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn headers(&self) -> Result<HeaderMap, GenerationError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|err| GenerationError::Auth(format!("invalid API key header: {err}")))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }

    /// Sends `body` and returns the response if its status is a success.
    async fn post(&self, body: &Value) -> Result<reqwest::Response, GenerationError> {
        // Error bodies are only read for a message, so a long one is cut off.
        const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
        let response = self
            .client
            .post(self.url())
            .headers(self.headers()?)
            .json(body)
            .send()
            .await
            .map_err(|err| GenerationError::Http(err.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let headers = response.headers().clone();
        let bytes = read_capped(response, MAX_ERROR_BODY_BYTES, false).await?;
        Err(status_error(
            status,
            &headers,
            &String::from_utf8_lossy(&bytes),
        ))
    }
}

/// Reads a response body of at most `limit` bytes.
///
/// A longer body fails with [`GenerationError::InvalidResponse`] when
/// `fail_when_over` is set, and is cut off at the limit otherwise.
async fn read_capped(
    mut response: reqwest::Response,
    limit: usize,
    fail_when_over: bool,
) -> Result<Vec<u8>, GenerationError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| GenerationError::Http(err.to_string()))?
    {
        if body.len() + chunk.len() > limit {
            if fail_when_over {
                return Err(GenerationError::InvalidResponse(format!(
                    "the response body is larger than the {limit} byte limit"
                )));
            }
            body.extend_from_slice(&chunk[..limit - body.len()]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl core::fmt::Debug for ChatCompletionsProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChatCompletionsProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl LlmProvider for ChatCompletionsProvider {
    fn name(&self) -> &'static str {
        "chat_completions"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        let body = build_body(model, &request, false, &self.settings)?;
        let response = self.post(&body).await?;
        let bytes = read_capped(response, self.max_body_bytes, true).await?;
        let raw: Value = serde_json::from_slice(&bytes).map_err(|err| {
            GenerationError::InvalidResponse(format!(
                "the response is not valid JSON: {err}; it starts with: {}",
                excerpt(&String::from_utf8_lossy(&bytes))
            ))
        })?;
        if let Some(field) = response_reasoning_field(&raw) {
            self.settings.seen_reasoning.record(field);
        }
        parse_response(&raw)
    }

    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let body = build_body(model, &request, true, &self.settings)?;
        let response = self.post(&body).await?;
        Ok(Box::pin(
            ChatStreamAdapter::new(SseDataStream::new(response.bytes_stream()))
                .recording_reasoning_field(self.settings.seen_reasoning.clone()),
        ))
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.pricing.get(model)
    }

    fn endpoint(&self) -> Option<String> {
        Some(self.base_url.clone())
    }
}

/// Maps a non-success response to a [`GenerationError`].
fn status_error(status: StatusCode, headers: &HeaderMap, body: &str) -> GenerationError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| error_message(&value))
        .unwrap_or_else(|| excerpt(body));
    let message = if message.is_empty() {
        status.to_string()
    } else {
        message
    };
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => GenerationError::Auth(message),
        StatusCode::TOO_MANY_REQUESTS => GenerationError::RateLimited {
            retry_after: headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        },
        _ => GenerationError::Provider {
            status: Some(status.as_u16()),
            message,
            source: None,
        },
    }
}

/// The start of `text`, short enough for an error message.
fn excerpt(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_ERROR_BODY_CHARS) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn auth_statuses_become_auth_errors_with_the_server_message() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let error = status_error(
                status,
                &HeaderMap::new(),
                r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#,
            );
            assert!(
                matches!(&error, GenerationError::Auth(message) if message == "bad key"),
                "{status}: {error:?}"
            );
        }
    }

    #[test]
    fn rate_limits_read_retry_after_seconds_only() {
        let error = status_error(
            StatusCode::TOO_MANY_REQUESTS,
            &headers_with_retry_after("7"),
            "{}",
        );
        assert!(
            matches!(error, GenerationError::RateLimited { retry_after: Some(wait) } if wait == Duration::from_secs(7)),
            "{error:?}"
        );
        let error = status_error(
            StatusCode::TOO_MANY_REQUESTS,
            &headers_with_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"),
            "{}",
        );
        assert!(
            matches!(error, GenerationError::RateLimited { retry_after: None }),
            "{error:?}"
        );
    }

    #[test]
    fn other_statuses_keep_the_status_and_read_the_known_error_shapes() {
        let cases = [
            (
                r#"{"error":{"code":"1210","message":"invalid effort"}}"#,
                "invalid effort",
            ),
            (r#"{"error":"model not found"}"#, "model not found"),
            (r#"{"message":"overloaded"}"#, "overloaded"),
            (r#"{"detail":"bad request body"}"#, "bad request body"),
            (
                "<html>gateway timeout</html>",
                "<html>gateway timeout</html>",
            ),
        ];
        for (body, expected) in cases {
            let error = status_error(StatusCode::BAD_GATEWAY, &HeaderMap::new(), body);
            assert!(
                matches!(&error, GenerationError::Provider { status: Some(502), message, .. }
                    if message == expected),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn an_empty_error_body_falls_back_to_the_status() {
        let error = status_error(StatusCode::INTERNAL_SERVER_ERROR, &HeaderMap::new(), "");
        assert!(
            matches!(&error, GenerationError::Provider { status: Some(500), message, .. }
                if message.contains("500")),
            "{error:?}"
        );
    }

    #[test]
    fn excerpt_cuts_long_text_on_a_character_boundary() {
        let long = "\u{e9}".repeat(MAX_ERROR_BODY_CHARS + 10);
        let cut = excerpt(&long);
        assert!(cut.ends_with("..."));
        assert_eq!(cut.chars().count(), MAX_ERROR_BODY_CHARS + 3);
    }

    #[test]
    fn debug_hides_the_api_key() {
        let provider = ChatCompletionsProvider::new("sk-secret-value");
        let shown = format!("{provider:?}");
        assert!(!shown.contains("sk-secret-value"), "{shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        let keyless = format!("{:?}", ChatCompletionsProvider::without_api_key());
        assert!(!keyless.contains("[REDACTED]"), "{keyless}");
    }

    #[test]
    fn base_url_loses_trailing_slashes_and_gets_the_path_appended() {
        let provider = ChatCompletionsProvider::without_api_key()
            .with_base_url("https://router.example.com/v1//");
        assert_eq!(
            provider.url(),
            "https://router.example.com/v1/chat/completions"
        );
        assert_eq!(
            provider.endpoint().as_deref(),
            Some("https://router.example.com/v1")
        );
        assert_eq!(
            ChatCompletionsProvider::new("k").url(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn the_key_is_sent_as_a_sensitive_bearer_token_and_a_keyless_provider_sends_none() {
        let headers = ChatCompletionsProvider::new("abc").headers().unwrap();
        let value = headers.get(AUTHORIZATION).unwrap();
        assert_eq!(value, "Bearer abc");
        assert!(value.is_sensitive());
        let headers = ChatCompletionsProvider::without_api_key()
            .headers()
            .unwrap();
        assert!(headers.get(AUTHORIZATION).is_none());
        let headers = ChatCompletionsProvider::new("").headers().unwrap();
        assert!(
            headers.get(AUTHORIZATION).is_none(),
            "an empty key sends no header"
        );
    }

    #[test]
    fn a_key_that_is_not_a_valid_header_value_is_an_auth_error() {
        let error = ChatCompletionsProvider::new("bad\nkey")
            .headers()
            .expect_err("a newline cannot be in a header");
        assert!(matches!(error, GenerationError::Auth(_)), "{error:?}");
    }
}
