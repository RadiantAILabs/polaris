//! `OpenAI` provider plugin.

use super::provider::OpenAiProvider;
use crate::pricing::PriceStore;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

/// Plugin providing support for `OpenAI` models via the Responses API.
///
/// Registers an [`OpenAiProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`](polaris_models::ModelsPlugin::ready) freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::llm`](polaris_models::ModelRegistry::llm) under
/// the `openai/<model>` identifier.
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
/// - The plugin is gated behind the `openai` feature.
/// - **`build()`** — constructs an `OpenAiProvider` and registers it
///   with the [`ModelRegistry`], which is a mutable resource during the
///   `build()` phase. The `Extends<ModelRegistry>` build param has the
///   resolver guarantee the registry is present and provider-ordered.
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
///   `OpenAiProvider` so that `openai/<model>` identifiers become
///   resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # use polaris_model_providers::OpenAiPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(OpenAiPlugin::from_env("OPENAI_API_KEY"));
/// ```
pub struct OpenAiPlugin {
    api_key: String,
    pricing: PriceStore,
}

impl OpenAiPlugin {
    /// Creates a plugin with the given API key. Private, so the one
    /// initializer that sets the defaults can be used without an environment
    /// variable in tests.
    fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            pricing: PriceStore::builtin(),
        }
    }

    /// Creates a plugin that reads the API key from the specified environment variable.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable is not set.
    #[must_use]
    pub fn from_env(env_var: &str) -> Self {
        let api_key = std::env::var(env_var).unwrap_or_else(|_| {
            panic!("Environment variable {env_var} for OpenAiPlugin not set. Please set it to your OpenAI API key.");
        });
        Self::new(api_key)
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
    /// # use polaris_model_providers::OpenAiPlugin;
    /// # use polaris_model_providers::pricing::PriceStore;
    /// # use polaris_models::ModelsPlugin;
    /// # use polaris_system::server::Server;
    /// let mut server = Server::new();
    /// server.add_plugins(ModelsPlugin);
    /// server.add_plugins(OpenAiPlugin::from_env("OPENAI_API_KEY").with_pricing_store(PriceStore::live()));
    /// ```
    #[must_use]
    pub fn with_pricing_store(mut self, pricing: PriceStore) -> Self {
        self.pricing = pricing;
        self
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present — so the parameter is an infallible
// `&mut ModelRegistry` and the old "add ModelsPlugin first" panic is gone.
#[plugin(id = "polaris::provider::openai", version = "0.0.1")]
impl Plugin for OpenAiPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(
            OpenAiProvider::new(self.api_key.clone()).with_pricing_store(self.pricing.clone()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiPlugin;
    use crate::pricing::PriceStore;
    use polaris_models::llm::ModelPricing;
    use polaris_models::{ModelRegistry, ModelsPlugin};
    use polaris_system::server::Server;

    /// A finished server with `plugin` registered.
    ///
    /// `plugin` is added *before* `ModelsPlugin`, so building successfully
    /// proves the resolver orders them rather than `add_plugins` order doing it.
    async fn server_with(plugin: OpenAiPlugin) -> Server {
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

    /// The price `plugin`'s registered provider reports for `model`, read
    /// through the `ModelRegistry` of a built server.
    async fn registered_pricing(plugin: OpenAiPlugin, model: &str) -> Option<ModelPricing> {
        let server = server_with(plugin).await;
        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available");
        registry
            .llm_provider("openai")
            .expect("the openai provider should be registered")
            .pricing(model)
    }

    /// `build()` registers the provider with the `ModelRegistry`, and the
    /// registry resolves `openai/<model>` once the server is finished.
    #[tokio::test]
    async fn build_registers_the_provider_with_the_model_registry() {
        let server = server_with(OpenAiPlugin::new("test-key")).await;

        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelRegistry should be available");
        assert!(registry.has_llm_provider("openai"));
        assert_eq!(registry.llm_provider_names(), ["openai"]);
        registry
            .llm("openai/gpt-5.5")
            .expect("openai/<model> should resolve through the registry");
    }

    /// With no `with_pricing_store`, the registered provider prices from the
    /// built-in table only.
    #[tokio::test]
    async fn default_plugin_registers_a_provider_priced_from_the_builtin_table() {
        assert_eq!(
            registered_pricing(OpenAiPlugin::new("test-key"), "gpt-5.5").await,
            Some(ModelPricing::new(5.0, 30.0)),
            "the default store serves the built-in row"
        );
        assert_eq!(
            registered_pricing(OpenAiPlugin::new("test-key"), "gpt-test-model").await,
            None,
            "a model the built-in table does not know has no price"
        );
    }

    /// The store given to `with_pricing_store` replaces the default one in the
    /// provider that `build()` registers.
    #[tokio::test]
    async fn with_pricing_store_configures_the_registered_provider() {
        let store = PriceStore::builtin()
            .with_override("gpt-test-model", ModelPricing::new(1.0, 2.0))
            .with_override("gpt-5.5", ModelPricing::new(9.0, 90.0));

        assert_eq!(
            registered_pricing(
                OpenAiPlugin::new("test-key").with_pricing_store(store.clone()),
                "gpt-test-model"
            )
            .await,
            Some(ModelPricing::new(1.0, 2.0)),
            "the provider must price from the store the plugin was given"
        );
        assert_eq!(
            registered_pricing(
                OpenAiPlugin::new("test-key").with_pricing_store(store),
                "gpt-5.5"
            )
            .await,
            Some(ModelPricing::new(9.0, 90.0)),
            "the given store's override beats the built-in row"
        );
    }

    /// The last store given wins, so `with_pricing_store` can replace an
    /// earlier one.
    #[tokio::test]
    async fn the_last_pricing_store_given_wins() {
        let plugin = OpenAiPlugin::new("test-key")
            .with_pricing_store(
                PriceStore::builtin().with_override("gpt-test-model", ModelPricing::new(1.0, 2.0)),
            )
            .with_pricing_store(
                PriceStore::builtin().with_override("gpt-test-model", ModelPricing::new(3.0, 4.0)),
            );

        assert_eq!(
            registered_pricing(plugin, "gpt-test-model").await,
            Some(ModelPricing::new(3.0, 4.0))
        );
    }
}
