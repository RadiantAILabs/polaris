//! Model provider registry.

use crate::error::CreateModelError;
use crate::llm::{ErasedLlmProvider, Llm, LlmProvider};
use polaris_system::resource::GlobalResource;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry for model provider implementations.
///
/// # For Consumers
///
/// Access models using provider/model identifiers (e.g., `"openai/gpt-4o"`).
/// See [`llm()`](Self::llm) for details.
///
/// # For Provider Plugin Authors
///
/// Provider plugins must register themselves during their `build()` phase. The registry
/// is available as a mutable resource during this phase and becomes an immutable global
/// after the `ready()` phase.
///
/// ```
/// # use polaris_system::plugin::{Plugin, PluginId, Version};
/// # use polaris_system::server::Server;
/// # use polaris_models::{ModelRegistry, ModelsPlugin};
/// # use polaris_models::llm::{LlmProvider, LlmRequest, LlmResponse, GenerationError};
/// # struct MyProviderPlugin;
/// # struct MyProvider;
///
/// # impl MyProvider { fn new() -> Self { MyProvider } }
///
/// # impl LlmProvider for MyProvider {
/// #   fn name(&self) -> &'static str { "my_provider" }
/// #   async fn generate(&self, _model: &str, _request: LlmRequest) -> Result<LlmResponse, GenerationError> {
/// #     unimplemented!()
/// #   }
/// # }
///
/// impl Plugin for MyProviderPlugin {
///    const ID: &'static str = "my_provider";
///    const VERSION: Version = Version::new(0, 0, 1);
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<ModelsPlugin>()]
///     }
///
///     fn build(&self, server: &mut Server) {
///         let provider = MyProvider::new(/* ... */);
///
///         let mut registry = server
///             .get_resource_mut::<ModelRegistry>()
///             .expect("ModelsPlugin must be added before provider plugins");
///
///         registry.register_llm_provider(provider);
///     }
/// }
/// ```
#[derive(Default)]
pub struct ModelRegistry {
    // Maps provider names to implementations.
    llm_providers: HashMap<String, Arc<dyn ErasedLlmProvider>>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("llm_providers", &self.llm_provider_names())
            .finish()
    }
}

impl GlobalResource for ModelRegistry {}

impl ModelRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            llm_providers: HashMap::new(),
        }
    }

    /// Creates a handle to an [`Llm`].
    ///
    /// # Arguments
    ///
    /// * `model_id` - Identifier in `"provider/model"` format (e.g., `"openai/gpt-4o"`)
    ///
    /// # Errors
    ///
    /// Returns an error if the `model_id` structure is invalid or the provider is not registered.
    pub fn llm(&self, model_id: impl AsRef<str>) -> Result<Llm, CreateModelError> {
        let model_id = model_id.as_ref();

        let (provider_name, model_name) = model_id
            .split_once('/')
            .ok_or_else(|| CreateModelError::InvalidModelId(model_id.to_string()))?;

        let provider = self
            .get_llm_provider(provider_name)
            .ok_or_else(|| CreateModelError::UnknownProvider(provider_name.to_string()))?;

        Ok(Llm::new(provider, model_name.to_string()))
    }

    /// Registers an LLM provider.
    ///
    /// The provider's [`LlmProvider::name()`] is used as the registry key
    /// (e.g., `"openai"` for `"openai/gpt-4o"`).
    ///
    /// This method may only be called during the `build()` phase when the registry is
    /// available as a mutable resource via [`Server::get_resource_mut`]. After the
    /// `ready()` phase, the registry becomes an immutable global and registration is
    /// no longer possible.
    ///
    /// # Panics
    ///
    /// Panics if a provider with the same name is already registered.
    pub fn register_llm_provider(&mut self, provider: impl LlmProvider) {
        let name = provider.name().to_string();
        assert!(
            !self.llm_providers.contains_key(&name),
            "LLM provider '{name}' is already registered"
        );
        self.llm_providers.insert(name, Arc::new(provider));
    }

    /// Returns a provider by name.
    #[must_use]
    fn get_llm_provider(&self, name: impl AsRef<str>) -> Option<Arc<dyn ErasedLlmProvider>> {
        self.llm_providers.get(name.as_ref()).cloned()
    }

    /// Checks if a provider is registered.
    #[must_use]
    pub fn has_llm_provider(&self, name: impl AsRef<str>) -> bool {
        self.llm_providers.contains_key(name.as_ref())
    }

    /// Lists registered provider names.
    #[must_use]
    pub fn llm_provider_names(&self) -> Vec<String> {
        self.llm_providers.keys().cloned().collect()
    }
}
