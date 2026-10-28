//! Anthropic provider plugin.

use super::provider::{AnthropicProvider, DEFAULT_MAX_TOKENS};
use crate::pricing::PriceStore;
use core::num::NonZeroU32;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

/// Plugin providing support for Anthropic models.
///
/// Registers an [`AnthropicProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`](polaris_models::ModelsPlugin::ready) freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::llm`](polaris_models::ModelRegistry::llm) under
/// the `anthropic/<model>` identifier.
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
/// - The plugin is gated behind the `anthropic` feature.
/// - **`build()`** — constructs an [`AnthropicProvider`] carrying this
///   plugin's configured `max_tokens` and registers it with the
///   [`ModelRegistry`], which is a mutable resource during the `build()`
///   phase. The `Extends<ModelRegistry>` build param has the resolver
///   guarantee the registry is present and provider-ordered.
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
/// - [`ModelRegistry`] (from [`ModelsPlugin`](polaris_models::ModelsPlugin)) — registers an
///   [`AnthropicProvider`] so that `anthropic/<model>` identifiers become
///   resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # use polaris_model_providers::anthropic::AnthropicPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"));
/// ```
///
/// The per-request output token limit is part of *this* registration, not of
/// any single request — see [`AnthropicPlugin::with_max_tokens`]:
///
/// ```no_run
/// # use polaris_model_providers::anthropic::AnthropicPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// # use core::num::NonZeroU32;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(
///     AnthropicPlugin::from_env("ANTHROPIC_API_KEY").with_max_tokens(NonZeroU32::new(32_000).unwrap()),
/// );
/// ```
pub struct AnthropicPlugin {
    api_key: String,
    max_tokens: NonZeroU32,
    pricing: PriceStore,
    /// Where tests point the registered provider; production always targets
    /// the real Anthropic API.
    #[cfg(test)]
    base_url: Option<String>,
}

impl AnthropicPlugin {
    /// Creates a plugin with the given API key directly.
    ///
    /// The registered provider sends [`DEFAULT_MAX_TOKENS`] until
    /// [`with_max_tokens`](Self::with_max_tokens) says otherwise.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            pricing: PriceStore::builtin(),
            #[cfg(test)]
            base_url: None,
        }
    }

    /// Creates a plugin that reads the API key from the specified environment variable.
    ///
    /// The registered provider sends [`DEFAULT_MAX_TOKENS`] until
    /// [`with_max_tokens`](Self::with_max_tokens) says otherwise.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable is not set.
    #[must_use]
    pub fn from_env(env_var: &str) -> Self {
        let api_key = std::env::var(env_var).unwrap_or_else(|_| {
            panic!("Environment variable {env_var} for AnthropicPlugin not set. Please set it to your Anthropic API key.");
        });
        Self::new(api_key)
    }

    /// Sets the `max_tokens` every request from this provider carries.
    ///
    /// This is the configuration point: the limit belongs to the registration,
    /// so one server has one answer for it and no caller can quietly ask for a
    /// different one. The upper bound is passed through to the API —
    /// [`AnthropicProvider::with_max_tokens`] explains why.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: NonZeroU32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Points the registered provider at a local server instead of the real
    /// Anthropic API, so tests can read the requests it sends.
    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

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
    /// # use polaris_model_providers::anthropic::AnthropicPlugin;
    /// # use polaris_model_providers::pricing::PriceStore;
    /// # use polaris_models::ModelsPlugin;
    /// # use polaris_system::server::Server;
    /// let mut server = Server::new();
    /// server.add_plugins(ModelsPlugin);
    /// server.add_plugins(
    ///     AnthropicPlugin::from_env("ANTHROPIC_API_KEY").with_pricing_store(PriceStore::live()),
    /// );
    /// ```
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }

    /// The provider this plugin registers. Separate from `build` so the wiring
    /// from plugin configuration to provider configuration is testable without
    /// a server.
    fn provider(&self) -> AnthropicProvider {
        let provider = AnthropicProvider::new(self.api_key.clone())
            .with_max_tokens(self.max_tokens)
            .with_pricing_store(self.pricing.clone());
        #[cfg(test)]
        if let Some(base_url) = &self.base_url {
            return provider.with_base_url(base_url);
        }
        provider
    }
}

impl std::fmt::Debug for AnthropicPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicPlugin")
            .field("api_key", &"[REDACTED]")
            .field("max_tokens", &self.max_tokens)
            .field("pricing", &self.pricing)
            .finish()
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present — so the parameter is an infallible
// `&mut ModelRegistry` and the old "add ModelsPlugin first" panic is gone.
#[plugin(id = "polaris::provider::anthropic", version = "0.0.1")]
impl Plugin for AnthropicPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(self.provider());
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::loopback::{self, Server as LoopbackServer};
    use super::{AnthropicPlugin, DEFAULT_MAX_TOKENS, NonZeroU32, PriceStore};
    use futures_util::StreamExt;
    use polaris_models::llm::{LlmRequest, Message, ModelPricing};
    use polaris_models::{ModelRegistry, ModelsPlugin};
    use polaris_system::server::Server;

    /// A finished server with `plugin` registered.
    ///
    /// `plugin` is added *before* `ModelsPlugin`, so building successfully
    /// proves the resolver orders them rather than `add_plugins` order doing it.
    async fn server_with(plugin: AnthropicPlugin) -> Server {
        let mut server = Server::new();
        server.add_plugins(plugin);
        server.add_plugins(ModelsPlugin);
        // Under `--all-features`, `dashboard` makes `ModelsPlugin` require
        // `AppPlugin`. Bind an ephemeral listener so `finish()` does not
        // claim a fixed port.
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

    /// `AnthropicPlugin::new` goes through the same initializer `from_env`
    /// uses, so this actually breaks if that initializer's default changes —
    /// unlike a struct literal that hardcodes `DEFAULT_MAX_TOKENS` itself.
    #[test]
    fn new_plugin_keeps_the_default_max_tokens() {
        let plugin = AnthropicPlugin::new("test-key");
        assert_eq!(plugin.provider().max_tokens(), DEFAULT_MAX_TOKENS);
    }

    /// The configured limit reaches the provider the plugin registers.
    #[test]
    fn registration_with_a_limit_configures_the_provider() {
        let plugin =
            AnthropicPlugin::new("test-key").with_max_tokens(NonZeroU32::new(32_000).unwrap());
        assert_eq!(plugin.provider().max_tokens().get(), 32_000);
    }

    /// `Debug` shows the configured limit and never the API key.
    #[test]
    fn debug_output_hides_the_api_key_and_shows_max_tokens() {
        let debug = format!("{:?}", AnthropicPlugin::new("sk-secret"));
        assert!(
            !debug.contains("sk-secret"),
            "the API key should not appear in Debug output: {debug}"
        );
        assert!(
            debug.contains("max_tokens: 4096"),
            "the limit should appear in Debug output: {debug}"
        );
    }

    /// The configured price store reaches the provider that `build()`
    /// registers with the `ModelRegistry`, not just the one `provider()`
    /// returns.
    #[tokio::test]
    async fn registration_with_a_price_store_configures_the_provider() {
        let plugin = AnthropicPlugin::new("test-key").with_pricing_store(
            PriceStore::builtin().with_override("claude-test-model", ModelPricing::new(1.0, 2.0)),
        );
        let server = server_with(plugin).await;

        let provider = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available")
            .llm_provider("anthropic")
            .expect("the anthropic provider should be registered");
        assert_eq!(
            provider.pricing("claude-test-model"),
            Some(ModelPricing::new(1.0, 2.0)),
            "the registered provider must price from the store the plugin was given"
        );
        assert_eq!(
            provider.pricing("claude-sonnet-4-6"),
            Some(ModelPricing::new(3.0, 15.0)),
            "the given store still falls back to the built-in rows"
        );
    }

    /// `build()` registers the provider with the `ModelRegistry` the way a
    /// real server does, rather than reaching past it into the private
    /// `provider()` helper. The plugin is added before `ModelsPlugin`, so a
    /// successful build also proves the resolver orders the two.
    #[tokio::test]
    async fn build_registers_the_provider_with_the_model_registry() {
        let server = server_with(AnthropicPlugin::new("test-key")).await;

        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available");
        assert!(registry.has_llm_provider("anthropic"));
    }

    /// The limit set on the plugin reaches the HTTP request, for both
    /// `generate` and `stream`. Goes through a real `Server`, the registry,
    /// and a loopback server, so it fails if `build()` drops the limit or
    /// either method stops building its request from the configured value.
    #[tokio::test]
    async fn configured_max_tokens_reaches_the_request_body_of_generate_and_stream() {
        let message = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-sonnet-4-6","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let events = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let loopback = LoopbackServer::start(vec![
            loopback::json_response("200 OK", message),
            loopback::sse_response(events),
        ])
        .await;

        let server = server_with(
            AnthropicPlugin::new("test-key")
                .with_max_tokens(NonZeroU32::new(32_000).unwrap())
                .with_base_url(loopback.base_url()),
        )
        .await;
        let llm = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available")
            .llm("anthropic/claude-sonnet-4-6")
            .expect("the anthropic provider should be registered");
        let request = LlmRequest {
            messages: vec![Message::user("hi")],
            ..Default::default()
        };

        loopback::within(llm.generate(request.clone()))
            .await
            .expect("generate should succeed against the loopback server");
        let mut stream = loopback::within(llm.stream(request))
            .await
            .expect("stream should start against the loopback server");
        while let Some(event) = loopback::within(stream.next()).await {
            event.expect("every stream event should parse");
        }

        let bodies = loopback.request_bodies().await;
        assert_eq!(bodies.len(), 2, "generate and stream send one request each");
        for (method, body) in ["generate", "stream"].into_iter().zip(&bodies) {
            assert!(
                body.contains(r#""max_tokens":32000"#),
                "{method} should send the configured limit: {body}"
            );
        }
    }
}
