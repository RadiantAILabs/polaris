//! AWS Bedrock provider plugin.

use super::provider::{BedrockProvider, DEFAULT_MAX_TOKENS};
use crate::pricing::PriceStore;
use aws_sdk_bedrockruntime::Client;
use core::num::NonZeroU32;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};
use std::sync::Arc;

/// Plugin providing support for AWS Bedrock models.
///
/// Registers a [`BedrockProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`](polaris_models::ModelsPlugin::ready) freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::llm`](polaris_models::ModelRegistry::llm) under
/// the `bedrock/<model>` identifier.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only registers a provider with the [`ModelRegistry`] owned by [`ModelsPlugin`](polaris_models::ModelsPlugin). |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin contributes a provider to [`ModelRegistry`] rather than installing its own API. |
///
/// # Dependencies
///
/// - [`ModelsPlugin`](polaris_models::ModelsPlugin) — owns the [`ModelRegistry`] that this plugin
///   registers itself with. The capability resolver orders `ModelsPlugin`
///   (the provider) before this plugin regardless of `add_plugins` order.
///
/// # Lifecycle
///
/// - The plugin is gated behind the `bedrock` feature.
/// - **`build()`** — resolves the AWS SDK config (either the explicit
///   config from [`from_sdk_config`](Self::from_sdk_config), or, for
///   [`from_env`](Self::from_env), the default credential chain loaded on
///   a dedicated thread with its own tokio runtime), builds a Bedrock
///   client, and registers a `BedrockProvider` with the [`ModelRegistry`].
///   For `openai.*` Responses calls, SDK connect and operation timeouts are
///   applied to the HTTP client; finite 3.1-second connect and 60-second
///   request defaults apply when the SDK configuration omits them.
///   The `Extends<ModelRegistry>` build param has the resolver guarantee
///   the registry is present and provider-ordered. Panics if AWS config
///   loading fails, or if the config-loading thread panics.
/// - **Pricing:** the registered provider uses [`PriceStore::builtin`] by
///   default — the built-in price table, with no network access and no file
///   reads or writes. [`with_pricing_store`](Self::with_pricing_store) opts
///   into live prices. With [`PriceStore::live`], the first price lookup
///   (and each one after the prices are a day old) starts a background
///   thread that fetches [`DEFAULT_SOURCE_URL`](crate::pricing::DEFAULT_SOURCE_URL)
///   and writes the cache file at
///   [`default_cache_path`](crate::pricing::default_cache_path); the store
///   reads that file when it is first created. To turn this off again, pass
///   [`PriceStore::builtin`], or a store built with
///   [`PriceStore::with_fetcher`] and no cache path. See [`crate::pricing`].
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`](polaris_models::ModelsPlugin)) — registers a
///   `BedrockProvider` so that `bedrock/<model>` identifiers become
///   resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # #[cfg(feature = "bedrock")]
/// # {
/// # use polaris_model_providers::BedrockPlugin;
/// # use polaris_models::ModelsPlugin;
/// # let mut server = polaris_system::server::Server::new();
/// server.add_plugins(ModelsPlugin);
///
/// // Using default AWS credential chain
/// server.add_plugins(BedrockPlugin::from_env());
///
/// // Or with an explicit SDK config
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
///     .region("us-west-2")
///     .load()
///     .await;
/// server.add_plugins(BedrockPlugin::from_sdk_config(sdk_config));
/// # });
/// # }
/// ```
pub struct BedrockPlugin {
    sdk_config: Option<aws_config::SdkConfig>,
    max_tokens: NonZeroU32,
    additional_fields: Option<serde_json::Value>,
    pricing: PriceStore,
}

impl BedrockPlugin {
    /// Initialises [`BedrockPlugin`] using the default AWS credential chain.
    ///
    /// The registered provider sends [`DEFAULT_MAX_TOKENS`] until
    /// [`with_max_tokens`](Self::with_max_tokens) says otherwise. Its
    /// `openai.*` Responses client uses the SDK connect and operation
    /// timeouts, with finite defaults when they are absent.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            sdk_config: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            additional_fields: None,
            pricing: PriceStore::builtin(),
        }
    }

    /// Initialises [`BedrockPlugin`] from a pre-configured AWS SDK config.
    ///
    /// The registered provider sends [`DEFAULT_MAX_TOKENS`] until
    /// [`with_max_tokens`](Self::with_max_tokens) says otherwise.
    #[must_use]
    pub fn from_sdk_config(sdk_config: aws_config::SdkConfig) -> Self {
        Self {
            sdk_config: Some(sdk_config),
            max_tokens: DEFAULT_MAX_TOKENS,
            additional_fields: None,
            pricing: PriceStore::builtin(),
        }
    }

    /// Sets the output token ceiling for every request from this provider.
    ///
    /// A request that sets `LlmRequest::max_output_tokens` may lower this
    /// ceiling for that one call, never raise it. The upper bound is passed
    /// through to the API — see [`BedrockProvider::with_max_tokens`].
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: NonZeroU32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Sets model-specific request fields the registered provider sends on
    /// every request — see
    /// [`BedrockProvider::with_additional_model_request_fields`].
    #[must_use]
    pub fn with_additional_model_request_fields(mut self, fields: serde_json::Value) -> Self {
        self.additional_fields = Some(fields);
        self
    }

    /// The provider this plugin registers, built from the resolved
    /// `sdk_config` and this plugin's settings.
    fn provider(&self, sdk_config: aws_config::SdkConfig) -> BedrockProvider {
        let client = Client::new(&sdk_config);
        let mut provider = BedrockProvider::new(Arc::new(client))
            .with_sdk_config(sdk_config)
            .with_max_tokens(self.max_tokens)
            .with_pricing_store(self.pricing.clone());
        if let Some(fields) = &self.additional_fields {
            provider = provider.with_additional_model_request_fields(fields.clone());
        }
        provider
    }
}

impl BedrockPlugin {
    /// Sets the price store the registered provider reports costs from
    /// (default: [`PriceStore::builtin`], which never touches the network or
    /// the disk).
    ///
    /// Passing a live store such as [`PriceStore::live`] lets the provider
    /// fetch prices from the web and cache them on disk; see the
    /// `# Lifecycle` section above.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[cfg(feature = "bedrock")]
    /// # {
    /// # use polaris_model_providers::BedrockPlugin;
    /// # use polaris_model_providers::pricing::PriceStore;
    /// # use polaris_models::ModelsPlugin;
    /// # let mut server = polaris_system::server::Server::new();
    /// server.add_plugins(ModelsPlugin);
    /// server.add_plugins(BedrockPlugin::from_env().with_pricing_store(PriceStore::live()));
    /// # }
    /// ```
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }
}

impl Default for BedrockPlugin {
    fn default() -> Self {
        Self::from_env()
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present — so the parameter is an infallible
// `&mut ModelRegistry` and the old "add ModelsPlugin first" panic is gone.
#[plugin(id = "polaris::provider::bedrock", version = "0.0.1")]
impl Plugin for BedrockPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        let sdk_config = match &self.sdk_config {
            Some(config) => config.clone(),
            None => std::thread::scope(|s| {
                s.spawn(|| {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime for AWS config loading");
                    rt.block_on(aws_config::from_env().load())
                })
                .join()
                .expect("AWS config loading thread panicked")
            }),
        };

        registry.register_llm_provider(self.provider(sdk_config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_models::llm::{GenerationError, LlmProvider, LlmRequest, Message, ModelPricing};
    use polaris_models::{ModelRegistry, ModelsPlugin};
    use polaris_system::server::Server;

    /// An SDK configuration with a region and no credentials, so a Responses
    /// call fails at the credentials step, before anything is sent.
    fn sdk_config() -> aws_config::SdkConfig {
        aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_bedrockruntime::config::Region::new("us-west-2"))
            .build()
    }

    #[test]
    fn registered_provider_takes_the_plugin_settings() {
        let fields = serde_json::json!({"reasoning": {"effort": "high"}});
        let plugin = BedrockPlugin::from_sdk_config(sdk_config())
            .with_max_tokens(NonZeroU32::new(2_048).unwrap())
            .with_additional_model_request_fields(fields.clone());
        let provider = plugin.provider(sdk_config());

        assert_eq!(provider.max_tokens().get(), 2_048);
        assert_eq!(
            provider.endpoint(),
            Some("https://bedrock-runtime.us-west-2.amazonaws.com".to_string())
        );

        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        };
        let converse = provider
            .converse_request("us.anthropic.claude-sonnet-4-6-v1:0", &request)
            .unwrap();
        let input = converse.as_input();
        assert_eq!(
            input.get_additional_model_request_fields(),
            &Some(super::super::types::json_to_document(&fields))
        );
        assert_eq!(
            input
                .get_inference_config()
                .as_ref()
                .and_then(aws_sdk_bedrockruntime::types::InferenceConfiguration::max_tokens),
            Some(2_048)
        );
    }

    #[test]
    fn registered_provider_defaults_match_the_plugin_defaults() {
        let provider = BedrockPlugin::from_sdk_config(sdk_config()).provider(sdk_config());
        assert_eq!(provider.max_tokens(), DEFAULT_MAX_TOKENS);
    }

    async fn server_with(plugin: BedrockPlugin) -> Server {
        let mut server = Server::new();
        server.add_plugins(plugin);
        server.add_plugins(ModelsPlugin);
        #[cfg(feature = "dashboard")]
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral port");
            server.add_plugins(
                polaris_app::AppPlugin::new(polaris_app::AppConfig::new()).with_listener(listener),
            );
        }
        server.finish().await.expect("server should build");
        server
    }

    #[tokio::test]
    async fn build_registers_a_provider_with_the_configured_pricing_store() {
        let rate = ModelPricing::new(71.0, 113.0);
        let store = PriceStore::builtin().with_override("test-bedrock-model", rate);
        let server =
            server_with(BedrockPlugin::from_sdk_config(sdk_config()).with_pricing_store(store))
                .await;
        let provider = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available")
            .llm_provider("aws.bedrock")
            .expect("Bedrock provider should be registered");

        assert_eq!(provider.pricing("test-bedrock-model"), Some(rate));
    }

    #[tokio::test]
    async fn registered_provider_receives_the_sdk_config() {
        let provider = BedrockPlugin::from_sdk_config(sdk_config()).provider(sdk_config());
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        };

        // Without the SDK config the Responses path fails with "Bedrock SDK
        // configuration is missing". With it, the call gets as far as loading
        // credentials, which this configuration does not have.
        let outcome = provider
            .generate("global.openai.gpt-6-luna", request)
            .await
            .map(drop);
        assert!(
            matches!(&outcome, Err(GenerationError::InvalidRequest(_))),
            "{outcome:?}"
        );
    }
}
