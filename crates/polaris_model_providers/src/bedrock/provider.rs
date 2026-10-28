//! AWS Bedrock [`LlmProvider`] implementation.

use super::request::{
    apply_cache_control, build_inference_config, build_output_config, build_tool_config,
    convert_message,
};
use super::response::{convert_response, convert_stream};
use super::types::{
    accepts_cache_points, accepts_reasoning_history, json_to_document, normalize_bedrock_model_id,
};
use crate::pricing::PriceStore;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_bedrockruntime::operation::converse::builders::ConverseFluentBuilder;
use aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamFluentBuilder;
use aws_sdk_bedrockruntime::types as bedrock;
use core::num::NonZeroU32;
use polaris_models::llm::{
    GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream, ModelPricing,
};
use std::sync::Arc;

/// Default maximum tokens for generation requests.
///
/// Mirrors [`crate::anthropic::DEFAULT_MAX_TOKENS`]: Bedrock's Converse API
/// has the same "send `max_tokens` on every request" contract via
/// `InferenceConfiguration`, so an existing consumer that registers this
/// provider with no limit of its own keeps the value it has always sent.
pub const DEFAULT_MAX_TOKENS: NonZeroU32 = NonZeroU32::new(4096).unwrap();

/// AWS Bedrock [`LlmProvider`] implementation.
#[derive(Clone)]
pub struct BedrockProvider {
    client: Arc<Client>,
    sdk_config: Option<aws_config::SdkConfig>,
    http: reqwest::Client,
    max_tokens: NonZeroU32,
    pricing: PriceStore,
    additional_fields: Option<serde_json::Value>,
}

impl std::fmt::Debug for BedrockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockProvider")
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

impl BedrockProvider {
    /// Creates a new provider with an already-initialized client.
    ///
    /// Sends [`DEFAULT_MAX_TOKENS`] on every request until
    /// [`with_max_tokens`](Self::with_max_tokens) says otherwise. Pricing
    /// defaults to [`PriceStore::builtin`]: the built-in price table, with no
    /// network access and no files. Use
    /// [`with_pricing_store`](Self::with_pricing_store) to opt into live
    /// prices, for example with [`PriceStore::live`].
    ///
    /// `openai.*` models (for example `global.openai.gpt-6-luna`) do not go
    /// through Converse: they are sent to Bedrock's `OpenAI` Responses
    /// endpoint, which needs the region, endpoint and credentials from the
    /// SDK configuration. A provider built with `new` alone has no SDK
    /// configuration, so every call to such a model fails with
    /// [`GenerationError::InvalidRequest`]. Call
    /// [`with_sdk_config`](Self::with_sdk_config) with the configuration the
    /// client was built from to serve them. [`BedrockPlugin`](super::BedrockPlugin)
    /// does this for you.
    ///
    /// The Responses HTTP client always has finite timeouts. Until an SDK
    /// configuration is supplied it uses a 3.1-second connect timeout and a
    /// 60-second request timeout. [`with_sdk_config`](Self::with_sdk_config)
    /// replaces those values with the SDK connect and operation timeouts when
    /// they are configured.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_model_providers::bedrock::BedrockProvider;
    /// # use std::sync::Arc;
    /// # async fn example() {
    /// let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
    ///     .region("us-west-2")
    ///     .load()
    ///     .await;
    /// let client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
    /// let provider = BedrockProvider::new(Arc::new(client)).with_sdk_config(sdk_config);
    /// # let _ = provider;
    /// # }
    /// ```
    #[must_use]
    pub fn new(client: Arc<Client>) -> Self {
        Self {
            client,
            sdk_config: None,
            http: super::responses::http_client(None),
            max_tokens: DEFAULT_MAX_TOKENS,
            pricing: PriceStore::builtin(),
            additional_fields: None,
        }
    }

    /// Sets the `max_tokens` this provider sends on every request.
    ///
    /// The *upper* bound is passed through unchanged, for the same reason
    /// [`AnthropicProvider::with_max_tokens`](crate::anthropic::AnthropicProvider::with_max_tokens)
    /// does: it is per-model, changes as models ship, and silently clamping
    /// would lower a configured limit without telling anyone. A request that
    /// sets [`LlmRequest::max_output_tokens`] may lower this ceiling for
    /// that one call, never raise it.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: NonZeroU32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Supplies the SDK configuration used for signed `OpenAI` Responses calls.
    ///
    /// Required for `openai.*` models, which are sent to Bedrock's `OpenAI`
    /// Responses endpoint instead of Converse. The call takes its region,
    /// endpoint override, credentials, retry settings, and timeouts from this
    /// configuration. The SDK connect timeout configures the HTTP connect
    /// timeout. The operation-attempt timeout (or operation timeout when no
    /// attempt timeout is set) configures each HTTP request; finite defaults
    /// of 3.1 and 60 seconds apply when those settings are absent. Pass the
    /// same configuration the client was built from, so both paths reach the
    /// same account and region.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_model_providers::bedrock::BedrockProvider;
    /// # use std::sync::Arc;
    /// # async fn example() {
    /// let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
    ///     .region("us-east-1")
    ///     .load()
    ///     .await;
    /// let client = aws_sdk_bedrockruntime::Client::new(&sdk_config);
    /// // `openai.*` models now resolve their Responses endpoint from `sdk_config`.
    /// let provider = BedrockProvider::new(Arc::new(client)).with_sdk_config(sdk_config);
    /// # let _ = provider;
    /// # }
    /// ```
    #[must_use]
    pub fn with_sdk_config(mut self, config: aws_config::SdkConfig) -> Self {
        self.http = super::responses::http_client(Some(&config));
        self.sdk_config = Some(config);
        self
    }

    /// Sets model-specific request fields sent on every request, as the
    /// Converse API's `additionalModelRequestFields`.
    ///
    /// This is the Converse API's own escape hatch for parameters it does not
    /// model, such as `{"reasoning": {"effort": "high"}}` for `OpenAI` models.
    /// The fields are passed through unchanged; a model that does not know a
    /// field rejects the request, so set only what the registered models take.
    ///
    /// `openai.*` models are sent to the `OpenAI` Responses endpoint instead.
    /// There, each top-level key of `fields` is set on the request body,
    /// replacing any value the provider set for that key. A value that is not
    /// a JSON object is ignored on that path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_model_providers::bedrock::BedrockProvider;
    /// # use std::sync::Arc;
    /// # fn example(client: Arc<aws_sdk_bedrockruntime::Client>) {
    /// let provider = BedrockProvider::new(client).with_additional_model_request_fields(
    ///     serde_json::json!({"reasoning": {"effort": "high"}}),
    /// );
    /// # let _ = provider;
    /// # }
    /// ```
    #[must_use]
    pub fn with_additional_model_request_fields(mut self, fields: serde_json::Value) -> Self {
        self.additional_fields = Some(fields);
        self
    }

    /// The `max_tokens` this provider sends on every request.
    #[must_use]
    pub fn max_tokens(&self) -> NonZeroU32 {
        self.max_tokens
    }

    /// Replaces this provider's price store (default: [`PriceStore::builtin`]).
    ///
    /// Pass [`PriceStore::live`] to fetch current prices from the web and
    /// cache them on disk, or build your own store to choose the source,
    /// cache path, and TTL. A live store fetches and writes files; see
    /// [`crate::pricing`] for exactly when.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[cfg(feature = "bedrock")]
    /// # {
    /// use polaris_model_providers::bedrock::BedrockProvider;
    /// use polaris_model_providers::pricing::PriceStore;
    ///
    /// # fn wrap(provider: BedrockProvider) -> BedrockProvider {
    /// provider.with_pricing_store(PriceStore::live())
    /// # }
    /// # }
    /// ```
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }

    /// Adds a per-model pricing override for this provider only, beating
    /// whatever its price store says for models starting with `prefix`.
    ///
    /// Other providers are not affected, even when they share the same
    /// [`PriceStore`] (see [`PriceStore::with_override`]). Call it after
    /// [`with_pricing_store`](Self::with_pricing_store), which replaces the
    /// store together with its overrides. [`pricing`](LlmProvider::pricing)
    /// tries the full Bedrock id first, then the first-party id, so an
    /// override can target either.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[cfg(feature = "bedrock")]
    /// # {
    /// use polaris_model_providers::bedrock::BedrockProvider;
    /// use polaris_models::llm::ModelPricing;
    ///
    /// # fn wrap(provider: BedrockProvider) -> BedrockProvider {
    /// provider.with_pricing_override("us.anthropic.claude-opus-5-5", ModelPricing::new(4.4, 22.0))
    /// # }
    /// # }
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

    /// The effective output-token ceiling for `request`: the registered
    /// [`max_tokens`](Self::max_tokens), lowered but never raised by
    /// [`LlmRequest::max_output_tokens`].
    fn effective_max_tokens(&self, request: &LlmRequest) -> NonZeroU32 {
        request
            .max_output_tokens
            .map_or(self.max_tokens, |limit| limit.min(self.max_tokens))
    }

    /// The Bedrock Runtime endpoint this provider's client targets, derived
    /// from the client's configured region (e.g.
    /// `https://bedrock-runtime.us-west-2.amazonaws.com`).
    fn endpoint_url(&self) -> Option<String> {
        self.client
            .config()
            .region()
            .map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"))
    }

    /// Builds the Bedrock-shaped messages/system/tool-config/inference-config
    /// common to both `generate()` and `stream()`, including cache-point
    /// placement and the no-tool-definitions-but-tool-blocks-in-history
    /// guard.
    #[expect(
        clippy::type_complexity,
        reason = "internal helper, not part of the public API"
    )]
    fn build_common(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<
        (
            Vec<bedrock::Message>,
            Option<Vec<bedrock::SystemContentBlock>>,
            Option<bedrock::ToolConfiguration>,
            Option<bedrock::OutputConfig>,
            bedrock::InferenceConfiguration,
        ),
        GenerationError,
    > {
        let mut tool_config = build_tool_config(request)?;
        let output_config = build_output_config(request)?;
        let inference_config = build_inference_config(self.effective_max_tokens(request).get());

        // Bedrock requires tool definitions whenever tool blocks appear in
        // the message history. Surface a clear error instead of letting a
        // cryptic Bedrock validation error propagate.
        if tool_config.is_none() && request.contains_tool_blocks() {
            return Err(GenerationError::InvalidRequest(
                "messages contain tool use blocks but no tool definitions were provided; \
                 include the tool definitions in the request when the conversation history \
                 contains tool calls or tool results"
                    .to_string(),
            ));
        }

        let mut messages = if accepts_reasoning_history(model) {
            request
                .messages
                .iter()
                .map(convert_message)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            request
                .messages
                .iter()
                .filter_map(without_reasoning)
                .map(|m| convert_message(&m))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut system = request
            .system
            .as_ref()
            .map(|s| vec![bedrock::SystemContentBlock::Text(s.clone())]);

        // Only Claude and Nova take explicit cache points; other Bedrock
        // models (OpenAI, Kimi, Grok) cache on their own and reject the
        // block outright.
        if accepts_cache_points(model) {
            apply_cache_control(
                &request.cache,
                &mut system,
                tool_config.as_mut(),
                &mut messages,
            );
        }

        Ok((
            messages,
            system,
            tool_config,
            output_config,
            inference_config,
        ))
    }

    /// The Converse call for `model` and `request`, ready to send.
    pub(super) fn converse_request(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<ConverseFluentBuilder, GenerationError> {
        let (messages, system, tool_config, output_config, inference_config) =
            self.build_common(model, request)?;
        Ok(self
            .client
            .converse()
            .model_id(model)
            .set_messages(Some(messages))
            .set_system(system)
            .inference_config(inference_config)
            .set_tool_config(tool_config)
            .set_output_config(output_config)
            .set_additional_model_request_fields(
                self.additional_fields.as_ref().map(json_to_document),
            ))
    }

    /// The `ConverseStream` call for `model` and `request`, ready to send.
    pub(super) fn converse_stream_request(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<ConverseStreamFluentBuilder, GenerationError> {
        let (messages, system, tool_config, output_config, inference_config) =
            self.build_common(model, request)?;
        Ok(self
            .client
            .converse_stream()
            .model_id(model)
            .set_messages(Some(messages))
            .set_system(system)
            .inference_config(inference_config)
            .set_tool_config(tool_config)
            .set_output_config(output_config)
            .set_additional_model_request_fields(
                self.additional_fields.as_ref().map(json_to_document),
            ))
    }
}

/// Whether `model` is served through Bedrock's `OpenAI` Responses endpoint
/// rather than Converse. Every `openai.*` model id takes this path, including
/// cross-region ids such as `global.openai.gpt-6-luna`.
fn uses_responses_api(model: &str) -> bool {
    model.contains("openai.")
}

impl LlmProvider for BedrockProvider {
    fn name(&self) -> &'static str {
        "aws.bedrock"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        if uses_responses_api(model) {
            let mut request = request;
            request.max_output_tokens = Some(self.effective_max_tokens(&request));
            return super::responses::generate(
                self.sdk_config.as_ref().ok_or_else(|| {
                    GenerationError::InvalidRequest("Bedrock SDK configuration is missing".into())
                })?,
                &self.http,
                model,
                &request,
                self.additional_fields.as_ref(),
            )
            .await;
        }
        let response = self
            .converse_request(model, &request)?
            .send()
            .await
            .map_err(map_sdk_error)?;

        convert_response(response)
    }

    /// # Errors
    ///
    /// Returns [`GenerationError::InvalidRequest`] if the request contains
    /// tool-use history but no tool definitions, or if a content block
    /// cannot be represented on the wire.
    /// Returns [`GenerationError::Http`] if the request fails to send
    /// (a transport-level failure with no modeled AWS exception).
    /// Returns [`GenerationError::RateLimited`], [`GenerationError::Auth`],
    /// or [`GenerationError::Provider`] for a modeled AWS exception,
    /// depending on its exception code — see `map_sdk_error` in this module.
    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        if uses_responses_api(model) {
            let response = self.generate(model, request).await?;
            return Ok(super::responses::as_stream(response));
        }
        let output = self
            .converse_stream_request(model, &request)?
            .send()
            .await
            .map_err(map_sdk_error)?;

        Ok(convert_stream(output))
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Bedrock regional rows can price differently from first-party (e.g.
        // cross-region-inference premiums) — try the id verbatim first
        // (matches a regional row like `us.anthropic.claude-opus-5-5`), then
        // fall back to the normalized first-party id.
        self.pricing
            .get(model)
            .or_else(|| self.pricing.get(normalize_bedrock_model_id(model)))
    }

    fn endpoint(&self) -> Option<String> {
        self.endpoint_url()
    }
}

/// Maps an AWS SDK error to a [`GenerationError`], and — critically — never
/// panics.
///
/// [`SdkError::ServiceError`] carries a modeled AWS exception, mapped by
/// exception code to the `GenerationError` variant that best fits it:
/// `ThrottlingException` -> [`GenerationError::RateLimited`],
/// `AccessDeniedException` -> [`GenerationError::Auth`],
/// `ValidationException` -> [`GenerationError::InvalidRequest`] (this is also
/// how Bedrock reports a context-window overflow), and every other exception
/// (`ModelErrorException`, `InternalServerException`, `ModelNotReadyException`,
/// `ModelTimeoutException`, `ServiceUnavailableException`, an unrecognized
/// code, ...) -> [`GenerationError::Provider`], carrying the AWS message.
///
/// Every other [`SdkError`] variant (`DispatchFailure`, `TimeoutError`,
/// `ConstructionFailure`, `ResponseError`) is a transport-level failure with
/// *no* modeled service error at all — a connection refused, a DNS failure, a
/// TLS error, a timeout before any response arrived — and maps to
/// [`GenerationError::Http`], mirroring how the Anthropic client's `reqwest`
/// call reports the same failures. This is also the load-bearing fix for a
/// real bug: the previous implementation called `err.into_service_error()`
/// unconditionally, which *panics* for exactly these transport-level
/// variants instead of returning an error.
///
/// Generic over the operation's error type so it serves `converse()`,
/// `converse_stream()`, and the per-event errors `converse_stream`'s
/// `EventReceiver` can yield mid-stream, without duplicating this match per
/// call site.
pub(super) fn map_sdk_error<E, R>(err: SdkError<E, R>) -> GenerationError
where
    E: std::error::Error + Send + Sync + ProvideErrorMetadata + 'static,
    R: std::fmt::Debug,
{
    match err {
        SdkError::ServiceError(context) => {
            let service_err = context.into_err();
            let message = service_err.to_string();
            match service_err.code() {
                Some("ThrottlingException") => GenerationError::RateLimited { retry_after: None },
                Some("AccessDeniedException") => GenerationError::Auth(message),
                Some("ValidationException") => GenerationError::InvalidRequest(message),
                _ => GenerationError::Provider {
                    status: None,
                    message,
                    source: Some(Box::new(service_err)),
                },
            }
        }
        other => GenerationError::Http(other.to_string()),
    }
}

/// Drops reasoning blocks from an assistant message, for models that reject
/// them in history. An assistant message left empty is dropped entirely.
pub(super) fn without_reasoning(
    message: &polaris_models::llm::Message,
) -> Option<polaris_models::llm::Message> {
    let mut message = message.clone();
    if let polaris_models::llm::Message::Assistant { content, .. } = &mut message {
        content.retain(|b| !matches!(b, polaris_models::llm::AssistantBlock::Reasoning(_)));
        if content.is_empty() {
            return None;
        }
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reasoning_is_dropped_from_history_for_models_that_reject_it() {
        use polaris_models::llm::{AssistantBlock, Message, ReasoningBlock};
        let reasoning = AssistantBlock::Reasoning(ReasoningBlock {
            id: None,
            reasoning: vec!["thinking".to_string()],
            signature: None,
        });
        let only = Message::Assistant {
            content: vec![reasoning.clone()],
            id: None,
        };
        assert!(without_reasoning(&only).is_none());
        let mixed = Message::Assistant {
            content: vec![reasoning, AssistantBlock::text("hi")],
            id: None,
        };
        let Some(Message::Assistant { content, .. }) = without_reasoning(&mixed) else {
            panic!("assistant message kept");
        };
        assert!(matches!(
            content.as_slice(),
            [AssistantBlock::Text(text)] if text.text == "hi"
        ));
    }

    use super::*;
    use aws_config::SdkConfig;
    use aws_sdk_bedrockruntime::config::Region;

    fn provider_with_region(region: &'static str) -> BedrockProvider {
        let sdk_config = SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(Region::new(region))
            .build();
        BedrockProvider::new(Arc::new(Client::new(&sdk_config)))
    }

    #[test]
    fn endpoint_derives_from_the_configured_region() {
        let provider = provider_with_region("us-west-2");
        assert_eq!(
            provider.endpoint(),
            Some("https://bedrock-runtime.us-west-2.amazonaws.com".to_string())
        );
    }

    #[test]
    fn endpoint_is_none_without_a_configured_region() {
        let sdk_config = SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build();
        let provider = BedrockProvider::new(Arc::new(Client::new(&sdk_config)));
        assert_eq!(provider.endpoint(), None);
    }

    fn hello() -> LlmRequest {
        LlmRequest {
            messages: vec![polaris_models::llm::Message::user("Hello")],
            ..Default::default()
        }
    }

    const CONVERSE_MODEL: &str = "us.anthropic.claude-sonnet-4-6-v1:0";
    const RESPONSES_MODEL: &str = "global.openai.gpt-6-luna";

    #[test]
    fn additional_model_request_fields_reach_both_converse_calls() {
        let fields = serde_json::json!({"reasoning": {"effort": "high"}});
        let provider =
            provider_with_region("us-east-1").with_additional_model_request_fields(fields.clone());
        let expected = Some(json_to_document(&fields));

        let converse = provider.converse_request(CONVERSE_MODEL, &hello()).unwrap();
        assert_eq!(
            converse.as_input().get_additional_model_request_fields(),
            &expected
        );
        let stream = provider
            .converse_stream_request(CONVERSE_MODEL, &hello())
            .unwrap();
        assert_eq!(
            stream.as_input().get_additional_model_request_fields(),
            &expected
        );
    }

    #[test]
    fn converse_calls_carry_no_additional_fields_until_set() {
        let provider = provider_with_region("us-east-1");
        let converse = provider.converse_request(CONVERSE_MODEL, &hello()).unwrap();
        assert_eq!(
            converse.as_input().get_additional_model_request_fields(),
            &None
        );
        let stream = provider
            .converse_stream_request(CONVERSE_MODEL, &hello())
            .unwrap();
        assert_eq!(
            stream.as_input().get_additional_model_request_fields(),
            &None
        );
    }

    #[test]
    fn converse_calls_carry_the_effective_output_limit() {
        let provider =
            provider_with_region("us-east-1").with_max_tokens(NonZeroU32::new(2_048).unwrap());
        let converse = provider.converse_request(CONVERSE_MODEL, &hello()).unwrap();
        let limit = converse
            .as_input()
            .get_inference_config()
            .as_ref()
            .and_then(bedrock::InferenceConfiguration::max_tokens);
        assert_eq!(limit, Some(2_048));
    }

    fn request_with_reasoning_and_cache() -> LlmRequest {
        use polaris_models::llm::{AssistantBlock, CacheControl, Message, ReasoningBlock};

        LlmRequest {
            system: Some("system prompt".into()),
            messages: vec![Message::Assistant {
                content: vec![
                    AssistantBlock::Reasoning(ReasoningBlock {
                        id: None,
                        reasoning: vec!["thinking".into()],
                        signature: Some("signature".into()),
                    }),
                    AssistantBlock::text("answer"),
                ],
                id: None,
            }],
            cache: CacheControl::prefix().with_breakpoints([0]),
            ..Default::default()
        }
    }

    #[test]
    fn build_common_omits_reasoning_and_cache_points_for_other_models() {
        let provider = provider_with_region("us-east-1");
        let (messages, system, ..) = provider
            .build_common(
                "moonshotai.kimi-k2-thinking",
                &request_with_reasoning_and_cache(),
            )
            .unwrap();

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0].content.as_slice(),
            [bedrock::ContentBlock::Text(text)] if text == "answer"
        ));
        assert!(matches!(
            system.as_deref(),
            Some([bedrock::SystemContentBlock::Text(text)]) if text == "system prompt"
        ));
    }

    #[test]
    fn build_common_keeps_reasoning_and_cache_points_for_claude() {
        let provider = provider_with_region("us-east-1");
        let (messages, system, ..) = provider
            .build_common(CONVERSE_MODEL, &request_with_reasoning_and_cache())
            .unwrap();

        assert_eq!(messages.len(), 1);
        assert!(matches!(
            messages[0].content.as_slice(),
            [
                bedrock::ContentBlock::ReasoningContent(_),
                bedrock::ContentBlock::Text(text),
                bedrock::ContentBlock::CachePoint(_),
            ] if text == "answer"
        ));
        assert!(matches!(
            system.as_deref(),
            Some([
                bedrock::SystemContentBlock::Text(text),
                bedrock::SystemContentBlock::CachePoint(_),
            ]) if text == "system prompt"
        ));
    }

    #[test]
    fn only_openai_models_take_the_responses_path() {
        for model in [
            "openai.gpt-oss-120b-1:0",
            "us.openai.gpt-oss-20b-1:0",
            RESPONSES_MODEL,
        ] {
            assert!(uses_responses_api(model), "{model}");
        }
        for model in [
            CONVERSE_MODEL,
            "anthropic.claude-opus-5-5",
            "amazon.nova-pro-v1:0",
            "moonshot.kimi-k2-thinking",
        ] {
            assert!(!uses_responses_api(model), "{model}");
        }
    }

    /// A request with a tool result but no tool definitions. Converse rejects
    /// it before sending, and the Responses path, given no SDK config, fails
    /// before it looks at the request at all, so each path's error names the
    /// path that ran, with no network access.
    fn request_that_converse_rejects() -> LlmRequest {
        LlmRequest {
            messages: vec![polaris_models::llm::Message::tool_result_with_call_id(
                "item-1",
                "call-1",
                polaris_models::llm::ToolResultContent::Text("result".into()),
            )],
            ..Default::default()
        }
    }

    fn assert_took_responses_path(outcome: Result<(), GenerationError>) {
        assert!(
            matches!(
                &outcome,
                Err(GenerationError::InvalidRequest(message))
                    if message.contains("SDK configuration is missing")
            ),
            "expected the Responses path: {outcome:?}"
        );
    }

    fn assert_took_converse_path(outcome: Result<(), GenerationError>) {
        assert!(
            matches!(
                &outcome,
                Err(GenerationError::InvalidRequest(message))
                    if message.contains("no tool definitions")
            ),
            "expected the Converse path: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn generate_routes_openai_models_to_responses_and_others_to_converse() {
        let provider = provider_with_region("us-east-1");
        let request = request_that_converse_rejects();

        let outcome = provider.generate(RESPONSES_MODEL, request.clone()).await;
        assert_took_responses_path(outcome.map(drop));
        let outcome = provider.generate(CONVERSE_MODEL, request).await;
        assert_took_converse_path(outcome.map(drop));
    }

    #[tokio::test]
    async fn stream_routes_openai_models_to_responses_and_others_to_converse() {
        let provider = provider_with_region("us-east-1");
        let request = request_that_converse_rejects();

        let outcome = provider.stream(RESPONSES_MODEL, request.clone()).await;
        assert_took_responses_path(outcome.map(drop));
        let outcome = provider.stream(CONVERSE_MODEL, request).await;
        assert_took_converse_path(outcome.map(drop));
    }

    #[test]
    fn unconfigured_provider_uses_the_default_max_tokens() {
        let provider = provider_with_region("us-east-1");
        assert_eq!(provider.max_tokens(), DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn with_max_tokens_configures_the_ceiling() {
        let provider =
            provider_with_region("us-east-1").with_max_tokens(NonZeroU32::new(64_000).unwrap());
        assert_eq!(provider.max_tokens().get(), 64_000);
    }

    #[test]
    fn effective_max_tokens_is_capped_by_the_provider_ceiling() {
        let provider =
            provider_with_region("us-east-1").with_max_tokens(NonZeroU32::new(512).unwrap());
        let request = LlmRequest {
            max_output_tokens: NonZeroU32::new(4_096),
            ..Default::default()
        };
        assert_eq!(provider.effective_max_tokens(&request).get(), 512);
    }

    #[test]
    fn effective_max_tokens_can_be_lowered_by_the_request() {
        let provider =
            provider_with_region("us-east-1").with_max_tokens(NonZeroU32::new(4_096).unwrap());
        let request = LlmRequest {
            max_output_tokens: NonZeroU32::new(512),
            ..Default::default()
        };
        assert_eq!(provider.effective_max_tokens(&request).get(), 512);
    }

    // ── pricing: two-step lookup (raw Bedrock id, then normalized first-party id) ──

    fn isolated_pricing_store() -> PriceStore {
        PriceStore::builtin()
    }

    #[test]
    fn provider_pricing_override_wins_over_the_builtin_regional_row() {
        let override_rate = ModelPricing::new(123.0, 456.0);
        let provider = provider_with_region("us-east-1")
            .with_pricing_override("us.anthropic.claude-opus-5-5", override_rate);

        assert_eq!(
            provider.pricing("us.anthropic.claude-opus-5-5"),
            Some(override_rate),
            "the provider override must replace the different built-in rate"
        );
    }

    #[test]
    fn pricing_falls_back_to_the_normalized_first_party_id() {
        // No regional row for this model at all.
        let store = isolated_pricing_store()
            .with_override("claude-sonnet-4-6", ModelPricing::new(3.0, 15.0));
        let provider = provider_with_region("us-east-1").with_pricing_store(store);

        assert_eq!(
            provider.pricing("us.anthropic.claude-sonnet-4-6-v1:0"),
            Some(ModelPricing::new(3.0, 15.0)),
            "with no regional row, the normalized first-party id should still resolve"
        );
    }

    #[test]
    fn pricing_returns_none_for_an_unknown_model() {
        let provider =
            provider_with_region("us-east-1").with_pricing_store(isolated_pricing_store());
        assert_eq!(
            provider.pricing("us.anthropic.claude-nonexistent-1-0"),
            None
        );
    }

    // ── map_sdk_error ──

    #[test]
    fn transport_level_sdk_errors_map_to_http_without_panicking() {
        // Regression test: the code this replaced called
        // `err.into_service_error()` unconditionally, which panics for any
        // `SdkError` variant that isn't `ServiceError` (timeout, dispatch
        // failure, construction failure, response error).
        let err: SdkError<aws_sdk_bedrockruntime::operation::converse::ConverseError, ()> =
            SdkError::timeout_error("request timed out");
        assert!(matches!(map_sdk_error(err), GenerationError::Http(_)));

        let err: SdkError<aws_sdk_bedrockruntime::operation::converse::ConverseError, ()> =
            SdkError::construction_failure("bad config");
        assert!(matches!(map_sdk_error(err), GenerationError::Http(_)));
    }

    fn service_error_with_code(
        code: &str,
    ) -> SdkError<aws_sdk_bedrockruntime::types::error::ThrottlingException, ()> {
        let exception = aws_sdk_bedrockruntime::types::error::ThrottlingException::builder()
            .message("thrown for a test")
            .meta(
                aws_smithy_types::error::ErrorMetadata::builder()
                    .code(code)
                    .message("thrown for a test")
                    .build(),
            )
            .build();
        SdkError::service_error(exception, ())
    }

    #[test]
    fn throttling_exception_maps_to_rate_limited() {
        assert!(matches!(
            map_sdk_error(service_error_with_code("ThrottlingException")),
            GenerationError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn access_denied_exception_maps_to_auth() {
        assert!(matches!(
            map_sdk_error(service_error_with_code("AccessDeniedException")),
            GenerationError::Auth(_)
        ));
    }

    #[test]
    fn validation_exception_maps_to_invalid_request() {
        assert!(matches!(
            map_sdk_error(service_error_with_code("ValidationException")),
            GenerationError::InvalidRequest(_)
        ));
    }

    #[test]
    fn unrecognized_exception_code_maps_to_provider() {
        assert!(matches!(
            map_sdk_error(service_error_with_code("SomeNewException")),
            GenerationError::Provider { .. }
        ));
    }
}
