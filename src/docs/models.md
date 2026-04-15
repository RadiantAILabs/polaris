Model registry and LLM provider implementations.

This module provides the provider-agnostic LLM interface (`polaris_models`)
and concrete vendor implementations (`polaris_model_providers`). The
architecture separates the consumer-facing API from provider-specific wiring.

# Architecture

```text
Systems                              Res<ModelRegistry>
  registry.llm("anthropic/...")?         |
                                         v
ModelRegistry (GlobalResource)       indexed by provider name
  +-- "anthropic" -> AnthropicProvider
  +-- "openai"    -> OpenAiProvider
  +-- "bedrock"   -> BedrockProvider
                |
                v  LlmProvider::generate(model, request)
Provider impl      raw HTTP client, vendor SDK, etc.
```

A model identifier is `"<provider>/<model>"` (e.g., `"anthropic/claude-sonnet-4-6"`).
`registry.llm(id)` splits the identifier, looks up the provider, and returns
an `LlmClient` handle bound to the model.

# Key Traits

| Trait | Role |
|-------|------|
| `LlmProvider` | Provider-facing: `(model, LlmRequest) -> LlmResponse` |
| `LlmClient` | Consumer-facing: wraps `(Arc<Provider>, model)`, exposes `.generate()`, `.stream()`, `.builder()` |
| `TokenCounter` | Optional: count tokens for a request |

# Usage

```no_run
# use polaris_ai::polaris_system;
use polaris_ai::system::{system, system::SystemError};
use polaris_ai::system::param::Res;
use polaris_ai::models::ModelRegistry;

#[system]
async fn chat(registry: Res<ModelRegistry>) -> Result<String, SystemError> {
    let llm = registry.llm("anthropic/claude-sonnet-4-6")
        .map_err(|e| SystemError::ExecutionError(e.to_string()))?;
    let response = llm.builder()
        .system("You are helpful")
        .user("Hello!")
        .generate()
        .await
        .map_err(|e| SystemError::ExecutionError(e.to_string()))?;
    Ok(response.text())
}
```

# Adding a Custom Provider

1. Implement `LlmProvider` (the `name()` becomes the prefix in model identifiers)
2. Create a plugin that registers it via `ModelRegistry::register_llm_provider()`
3. Declare `ModelsPlugin` as a dependency so the registry exists during `build()`

```no_run
use polaris_ai::models::llm::{LlmProvider, LlmRequest, LlmResponse, GenerationError};

struct MyProvider { api_key: String }

impl LlmProvider for MyProvider {
    fn name(&self) -> &'static str { "myprovider" }

    async fn generate(&self, model: &str, request: LlmRequest)
        -> Result<LlmResponse, GenerationError> {
        // Translate LlmRequest -> vendor request, call API, translate response
        todo!()
    }
}
```

# Built-in Providers

| Provider | Feature flag | Public items added | Runtime effect |
|----------|-------------|--------------------|----------------|
| Anthropic Claude | `anthropic` | [`anthropic`](crate::models::anthropic), [`AnthropicPlugin`](crate::models::AnthropicPlugin) | Registers the `anthropic/...` provider family in [`ModelRegistry`](crate::models::ModelRegistry) |
| `OpenAI` | `openai` | [`openai`](crate::models::openai), [`OpenAiPlugin`](crate::models::OpenAiPlugin) | Registers the `openai/...` provider family in [`ModelRegistry`](crate::models::ModelRegistry) |
| AWS Bedrock | `bedrock` | [`bedrock`](crate::models::bedrock), [`BedrockPlugin`](crate::models::BedrockPlugin) | Registers the `bedrock/...` provider family in [`ModelRegistry`](crate::models::ModelRegistry) |

# Feature-Gated Tokenization

| Feature | Adds public items | Existing public items affected | Runtime effect |
|---------|-------------------|--------------------------------|----------------|
| `tiktoken` | [`tokenizer::TiktokenCounter`](crate::models::tokenizer::TiktokenCounter), [`tokenizer::EncodingFamily`](crate::models::tokenizer::EncodingFamily) | Enables [`TokenizerPlugin::default`](crate::models::TokenizerPlugin::default) and defines what that default constructs | [`TokenizerPlugin::default`](crate::models::TokenizerPlugin::default) registers a global [`Tokenizer`](crate::models::Tokenizer) backed by [`tokenizer::TiktokenCounter`](crate::models::tokenizer::TiktokenCounter) |

# Related

- [Tools](crate::tools) -- LLM tool calling via `ToolRegistry`
- [Systems](crate::system) -- accessing `ModelRegistry` via `Res<ModelRegistry>`
- [Feature flags](crate#model-providers) -- enabling provider features
