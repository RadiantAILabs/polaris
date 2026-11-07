//! Chat Completions provider plugin.

use super::provider::ChatCompletionsProvider;
use super::{MaxTokensField, ReasoningReplay};
use crate::pricing::PriceStore;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

/// Plugin providing support for servers that implement the `OpenAI` Chat
/// Completions API (`POST {base_url}/chat/completions`).
///
/// Registers a [`ChatCompletionsProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`](polaris_models::ModelsPlugin::ready)
/// freezes the registry, models become resolvable through
/// [`ModelRegistry::llm`](polaris_models::ModelRegistry::llm) under the
/// `chat_completions/<model>` identifier, where `<model>` is the exact string
/// the server expects in its `model` field (for example
/// `chat_completions/zai-org/GLM-5.3-Flash:zai`).
///
/// Use it for the Hugging Face inference router, vLLM, `SGLang`, Ollama,
/// `DeepInfra`, Z.ai, `DeepSeek`, `OpenAI`'s own Chat Completions endpoint and other
/// servers of this family. It is separate from
/// [`OpenAiPlugin`](crate::openai::OpenAiPlugin), which speaks the Responses
/// API; both can be added to one server, because their model ids start with
/// different provider names.
///
/// Only one `ChatCompletionsPlugin` can be registered, because the provider name
/// is the registry key. To reach two Chat Completions servers, wrap one in a
/// provider of your own that holds a [`ChatCompletionsProvider`] for each.
///
/// What the provider sends and reads, and how it maps errors, is described on
/// [`ChatCompletionsProvider`]. A reasoning effort set on the request is sent as
/// `reasoning_effort`.
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
/// - The plugin is gated behind the `chat-completions` feature, which adds no
///   dependency.
/// - **`build()`** — registers a copy of the configured provider with the
///   [`ModelRegistry`], which is a mutable resource during the `build()` phase.
///   The `Extends<ModelRegistry>` build param has the resolver guarantee the
///   registry is present and provider-ordered.
/// - **Pricing:** the provider uses [`PriceStore::builtin`] by default — the
///   built-in price table, with no network access and no file reads or writes.
///   Most models served this way are not in that table, so use
///   [`with_pricing_override`](Self::with_pricing_override) to price them. See
///   [`crate::pricing`].
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`](polaris_models::ModelsPlugin)) — registers a
///   `ChatCompletionsProvider` so that `chat_completions/<model>` identifiers
///   become resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # use polaris_model_providers::ChatCompletionsPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(
///     ChatCompletionsPlugin::from_env("HF_TOKEN")
///         .with_base_url("https://router.huggingface.co/v1"),
/// );
/// ```
pub struct ChatCompletionsPlugin {
    provider: ChatCompletionsProvider,
}

impl ChatCompletionsPlugin {
    /// Creates a plugin that sends `api_key` as a bearer token to
    /// `https://api.openai.com/v1`. Use [`with_base_url`](Self::with_base_url)
    /// for another server.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            provider: ChatCompletionsProvider::new(api_key),
        }
    }

    /// Creates a plugin that reads the API key from the specified environment
    /// variable.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable is not set.
    #[must_use]
    pub fn from_env(env_var: &str) -> Self {
        let api_key = std::env::var(env_var).unwrap_or_else(|_| {
            panic!(
                "Environment variable {env_var} for ChatCompletionsPlugin not set. \
                 Please set it to the API key of the server."
            );
        });
        Self::new(api_key)
    }

    /// Creates a plugin that sends no `Authorization` header, for local servers
    /// that need no key.
    #[must_use]
    pub fn without_api_key() -> Self {
        Self {
            provider: ChatCompletionsProvider::without_api_key(),
        }
    }

    /// Sets the server root that `/chat/completions` is appended to (default
    /// `https://api.openai.com/v1`). No environment variable is read. See
    /// [`ChatCompletionsProvider::with_base_url`].
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.provider = self.provider.with_base_url(base_url);
        self
    }

    /// Adds fields to the top level of every request body, replacing a field
    /// Polaris set for the same key. `fields` must be a JSON object. See
    /// [`ChatCompletionsProvider::with_extra_body`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_model_providers::ChatCompletionsPlugin;
    /// let plugin = ChatCompletionsPlugin::from_env("ZAI_API_KEY")
    ///     .with_base_url("https://api.z.ai/api/paas/v4")
    ///     .with_extra_body(serde_json::json!({
    ///         "thinking": { "type": "enabled", "clear_thinking": false }
    ///     }));
    /// # let _ = plugin;
    /// ```
    #[must_use]
    pub fn with_extra_body(mut self, fields: serde_json::Value) -> Self {
        self.provider = self.provider.with_extra_body(fields);
        self
    }

    /// Chooses the request key that carries the output token limit (default
    /// `max_completion_tokens`). See [`MaxTokensField`].
    #[must_use]
    pub fn with_max_tokens_field(mut self, field: MaxTokensField) -> Self {
        self.provider = self.provider.with_max_tokens_field(field);
        self
    }

    /// Chooses how reasoning from earlier turns is sent back (default
    /// `reasoning_content`). See [`ReasoningReplay`].
    #[must_use]
    pub fn with_reasoning_replay(mut self, replay: ReasoningReplay) -> Self {
        self.provider = self.provider.with_reasoning_replay(replay);
        self
    }

    /// Sends `strict` with each tool definition (default `false`). See
    /// [`ChatCompletionsProvider::with_strict_tools`].
    #[must_use]
    pub fn with_strict_tools(mut self, strict: bool) -> Self {
        self.provider = self.provider.with_strict_tools(strict);
        self
    }

    /// Sets the price store the registered provider reports costs from
    /// (default: [`PriceStore::builtin`], which never touches the network or
    /// the disk).
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.provider = self.provider.with_pricing_store(pricing);
        self
    }

    /// Adds a per-model pricing override for the registered provider, beating
    /// its price store for models starting with `prefix`. Call it after
    /// [`with_pricing_store`](Self::with_pricing_store), which replaces the
    /// store together with its overrides.
    #[must_use]
    pub fn with_pricing_override(
        mut self,
        prefix: impl Into<String>,
        pricing: polaris_models::llm::ModelPricing,
    ) -> Self {
        self.provider = self.provider.with_pricing_override(prefix, pricing);
        self
    }
}

impl core::fmt::Debug for ChatCompletionsPlugin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChatCompletionsPlugin")
            .field("provider", &self.provider)
            .finish()
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present.
#[plugin(id = "polaris::provider::chat_completions", version = "0.0.1")]
impl Plugin for ChatCompletionsPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(self.provider.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::ChatCompletionsPlugin;
    use polaris_models::llm::ModelPricing;
    use polaris_models::{ModelRegistry, ModelsPlugin};
    use polaris_system::server::Server;

    /// A finished server with `plugin` registered.
    ///
    /// `plugin` is added *before* `ModelsPlugin`, so building successfully
    /// proves the resolver orders them rather than `add_plugins` order doing it.
    async fn server_with(plugin: ChatCompletionsPlugin) -> Server {
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

    #[tokio::test]
    async fn build_registers_the_provider_and_model_ids_keep_their_slashes() {
        let server = server_with(
            ChatCompletionsPlugin::new("test-key").with_base_url("http://127.0.0.1:9/v1"),
        )
        .await;
        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available");
        assert!(registry.has_llm_provider("chat_completions"));
        assert_eq!(registry.llm_provider_names(), ["chat_completions"]);
        let llm = registry
            .llm("chat_completions/zai-org/GLM-5.3-Flash:zai")
            .expect("chat_completions/<model> should resolve through the registry");
        assert_eq!(llm.model_name(), "zai-org/GLM-5.3-Flash:zai");
        assert_eq!(
            registry
                .llm_provider("chat_completions")
                .expect("registered")
                .endpoint()
                .as_deref(),
            Some("http://127.0.0.1:9/v1")
        );
    }

    #[tokio::test]
    async fn pricing_overrides_reach_the_registered_provider() {
        let server = server_with(
            ChatCompletionsPlugin::without_api_key()
                .with_pricing_override("zai-org/GLM-5.3", ModelPricing::new(0.3, 1.2)),
        )
        .await;
        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available");
        let provider = registry
            .llm_provider("chat_completions")
            .expect("registered");
        assert_eq!(
            provider.pricing("zai-org/GLM-5.3-Flash:zai"),
            Some(ModelPricing::new(0.3, 1.2))
        );
        assert_eq!(provider.pricing("some/other-model"), None);
    }

    #[test]
    fn debug_hides_the_api_key() {
        let shown = format!("{:?}", ChatCompletionsPlugin::new("sk-secret-value"));
        assert!(!shown.contains("sk-secret-value"), "{shown}");
    }
}
