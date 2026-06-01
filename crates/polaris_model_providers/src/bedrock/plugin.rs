//! AWS Bedrock provider plugin.

use super::provider::BedrockProvider;
use aws_sdk_bedrockruntime::Client;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};
use std::sync::Arc;

/// Plugin providing support for AWS Bedrock models.
///
/// Registers a [`BedrockProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`] freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::resolve`](polaris_models::ModelRegistry::resolve) under
/// the `bedrock/<model>` identifier.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only registers a provider with the [`ModelRegistry`] owned by [`ModelsPlugin`]. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin contributes a provider to [`ModelRegistry`] rather than installing its own API. |
///
/// # Dependencies
///
/// - [`ModelsPlugin`] — owns the [`ModelRegistry`] that this plugin
///   registers itself with. Must be added before `BedrockPlugin`.
///
/// # Lifecycle
///
/// - The plugin is gated behind the `bedrock` feature.
/// - **`build()`** — resolves the AWS SDK config (either the explicit
///   config from [`from_sdk_config`](Self::from_sdk_config), or, for
///   [`from_env`](Self::from_env), the default credential chain loaded on
///   a dedicated thread with its own tokio runtime), builds a Bedrock
///   client, and registers a `BedrockProvider` with the [`ModelRegistry`].
///   Panics if [`ModelsPlugin`] was not added first, if AWS config
///   loading fails, or if the config-loading thread panics.
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`]) — registers a
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
}

impl BedrockPlugin {
    /// Initialises [`BedrockPlugin`] using the default AWS credential chain.
    #[must_use]
    pub fn from_env() -> Self {
        Self { sdk_config: None }
    }

    /// Initialises [`BedrockPlugin`] from a pre-configured AWS SDK config.
    #[must_use]
    pub fn from_sdk_config(sdk_config: aws_config::SdkConfig) -> Self {
        Self {
            sdk_config: Some(sdk_config),
        }
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

        let client = Client::new(&sdk_config);
        registry.register_llm_provider(BedrockProvider::new(Arc::new(client)));
    }
}
