# polaris_model_providers

LLM provider plugins for Polaris.

## Available Providers

| Provider | Plugin | Feature Flag | Model ID Format |
|----------|--------|--------------|-----------------|
| Anthropic | `AnthropicPlugin` | `anthropic` (default) | `anthropic/*` |
| OpenAI | `OpenAiPlugin` | `openai` | `openai/*` |
| AWS Bedrock | `BedrockPlugin` | `bedrock` (also enables `openai`) | `bedrock/*` |

## Feature Flags

Each provider is gated behind a feature flag to avoid pulling in unnecessary dependencies.

```toml
# Enable only Anthropic (default)
polaris_model_providers = { path = "../polaris_model_providers" }

# Enable only Bedrock
polaris_model_providers = { path = "../polaris_model_providers", default-features = false, features = ["bedrock"] }

# Enable Anthropic and Bedrock (Bedrock also enables OpenAI)
polaris_model_providers = { path = "../polaris_model_providers", features = ["bedrock"] }
```

## Usage

All provider plugins depend on `ModelsPlugin`, which must be added first to register the `ModelRegistry` resource.

```rust
use core::num::NonZeroU32;

use polaris_model_providers::AnthropicPlugin;
use polaris_models::ModelsPlugin;
use polaris_system::server::Server;

let mut server = Server::new();
server.add_plugins(ModelsPlugin);
server.add_plugins(
    AnthropicPlugin::from_env("ANTHROPIC_API_KEY")
        .with_max_tokens(NonZeroU32::new(32_000).unwrap()),
);
```

The Anthropic output token limit is configured when the plugin is registered.
It defaults to 4096 when `with_max_tokens` is omitted. An `LlmRequest` may set
a lower per-request `max_output_tokens` limit, but cannot raise the registered
ceiling.

For AWS Bedrock, credentials are loaded from the default AWS credential chain:

```rust
use polaris_model_providers::BedrockPlugin;
use polaris_models::ModelsPlugin;
use polaris_system::server::Server;

let mut server = Server::new();
server.add_plugins(ModelsPlugin);
server.add_plugins(BedrockPlugin::from_env());
```

`openai.*` Bedrock models are sent to Bedrock's OpenAI Responses endpoint
instead of Converse. `BedrockPlugin` passes the AWS SDK config that path needs.
Use `with_max_tokens` for the output ceiling (default 4096) and
`with_additional_model_request_fields` for model-specific fields.

## Pricing

Providers report per-model token prices for tracing from a
`pricing::PriceStore`. The default, `PriceStore::builtin()`, uses only built-in
prices and never touches the network or disk. For prices fetched from the web
and cached on disk, opt in with `.with_pricing_store(PriceStore::live())` on a
provider plugin. See `docs/reference/model-providers.md` (Pricing) for details.

See the [polaris_models README](../polaris_models/README.md) for more usage examples.
