---
notion_page: https://www.notion.so/radiant-ai/Model-Providers-342afe2e695d80de84ecf1316afb58b1
title: Model Providers
---

# Model Providers

`polaris_models` defines the provider-agnostic LLM interface, and `polaris_model_providers` implements it for specific vendors (Anthropic, OpenAI, AWS Bedrock). This document covers how to *extend* the framework with a new provider. For end-user usage of the registry, see the `polaris_models` README.

## Architecture

```text
┌──────────────────────────────────────┐
│  Systems                             │  Res<ModelRegistry>
│    registry.llm("anthropic/...")?    │      │
└──────────────────────────────────────┘      │
                                              ▼
┌──────────────────────────────────────┐
│  ModelRegistry (GlobalResource)      │  indexed by provider name
│   ├── "anthropic" → AnthropicProvider│
│   ├── "openai"    → OpenAiProvider   │
│   └── ...                            │
└──────────────────────────────────────┘
                │
                ▼  LlmProvider::generate(model, request)
┌──────────────────────────────────────┐
│  Provider impl                       │  raw HTTP client, vendor SDK, etc.
└──────────────────────────────────────┘
```

The registry indexes providers by `LlmProvider::name()`. A model identifier is `"<provider>/<model>"` (e.g., `"anthropic/claude-sonnet-4-6"`). `registry.llm(id)` splits the id, looks up the provider, and returns an `LlmClient` handle bound to the model.

## Trait Hierarchy

| Trait | Role |
|-------|------|
| `LlmProvider` | Provider-facing: receives a `(model, LlmRequest)`, returns `LlmResponse`. Implementors wire HTTP / SDK calls here. |
| `LlmClient` | Consumer-facing: returned by `ModelRegistry::llm(id)`. Wraps `(Arc<Provider>, model_name)`. Exposes `.generate(req)`, `.stream(req)`, and `.builder()`. |
| `TokenCounter` | Optional: count tokens for a request. Providers may implement via vendor SDKs. |

Consumers never instantiate `LlmClient` directly — the registry constructs one on demand from the registered provider.

### `LlmProvider`

```rust
pub trait LlmProvider: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> impl Future<Output = Result<LlmResponse, GenerationError>> + Send;

    fn stream(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> impl Future<Output = Result<LlmStream, GenerationError>> + Send {
        async { Err(GenerationError::UnsupportedOperation("stream")) }
    }

    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        None
    }

    fn endpoint(&self) -> Option<String> {
        None
    }
}
```

The `name()` must be stable and lowercase — it becomes the prefix in `"<name>/<model>"`. `stream()` has a default implementation that returns `UnsupportedOperation`; override it to support streaming. `pricing()` is optional — return `Some(ModelPricing::new(input_per_million_usd, output_per_million_usd))` to publish per-million-token rates for a given model (a vendored provider should hold a [`PriceStore`](#pricing) and return `self.pricing.get(model)` instead of hardcoding rates); the tracing decorator multiplies token counts by the rate and records `gen_ai.usage.cost` on chat spans, so cost surfaces in any console subscriber or OTel exporter. The defaulted `None` means cost stays unset end-to-end. `endpoint()` is optional — return the base URL the provider sends requests to (or the proxy address, if one is used); the tracing decorator records it as the `server.address` attribute on chat spans. The defaulted `None` leaves the attribute unset.

## Building a Provider

A provider is two pieces: a type implementing `LlmProvider`, and a plugin that registers it.

### 1. Implement `LlmProvider`

```rust
use polaris_models::llm::{LlmProvider, LlmRequest, LlmResponse, GenerationError};

pub struct MyProvider {
    api_key: String,
    http: reqwest::Client,
}

impl MyProvider {
    pub fn new(api_key: String) -> Self {
        Self { api_key, http: reqwest::Client::new() }
    }
}

impl LlmProvider for MyProvider {
    fn name(&self) -> &'static str {
        "myprovider"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        // Translate LlmRequest → vendor request, call HTTP, translate response.
        todo!()
    }
}
```

Map `LlmRequest` (provider-agnostic) to your vendor's request shape, call the API, and map the response back. Include token counts in `LlmResponse::usage` when the vendor reports them — downstream tooling uses this for cost accounting and rate limiting.

### 2. Plugin Wiring

```rust
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

pub struct MyProviderPlugin {
    api_key: String,
}

impl MyProviderPlugin {
    pub fn from_env(var: &str) -> Self {
        Self {
            api_key: std::env::var(var)
                .unwrap_or_else(|_| panic!("{var} not set")),
        }
    }
}

impl Plugin for MyProviderPlugin {
    const ID: &'static str = "my_crate::provider::myprovider";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = MyProvider::new(self.api_key.clone());
        let mut registry = server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must be registered before MyProviderPlugin");
        registry.register_llm_provider(provider);
    }
}
```

Key rules:

- **Declare `ModelsPlugin` as a dependency.** The dependency graph ensures `ModelsPlugin::build()` runs first, so `ModelRegistry` exists when `MyProviderPlugin::build()` runs.
- **Register in `build()`, not `ready()`.** `ModelsPlugin` freezes the registry into a `GlobalResource` during its own `ready()`, so any provider registration must complete during the build phase.
- **Do not register twice.** `register_llm_provider` panics on duplicate provider names.

### 3. Feature-Flag Wiring (if vendored under `polaris_model_providers`)

Providers living in `polaris_model_providers` are gated behind per-provider feature flags:

```toml
# crates/polaris_model_providers/Cargo.toml
[features]
default = ["anthropic"]
anthropic = ["dep:reqwest", /* ... */]
openai = ["dep:reqwest", /* ... */]
myprovider = ["dep:reqwest"]
```

```rust
// crates/polaris_model_providers/src/lib.rs
#[cfg(feature = "myprovider")]
pub mod myprovider;

#[cfg(feature = "myprovider")]
pub use myprovider::{MyProvider, MyProviderPlugin};
```

External providers in their own crate do not need this gating.

## Usage

Once registered, models are accessible via `"<provider_name>/<model>"`:

```rust
let registry = server.get_global::<ModelRegistry>().unwrap();
let llm = registry.llm("myprovider/my-model-v1")?;

let response = llm.builder()
    .system("You are helpful")
    .user("Hello!")
    .generate()
    .await?;
```

## Output token limits

There are two places to set an output token limit:

- **The registration** sets a ceiling for every request sent through that
  provider (Anthropic and Bedrock).
- **The request** sets `LlmRequest::max_output_tokens` (a `NonZeroU32`, also
  set with `LlmRequestBuilder::max_output_tokens`). A request limit can only
  **lower** the registration ceiling for that one request. It never raises it.

The limit that is sent is `min(request limit, registration ceiling)`.

| Provider | Registration ceiling | Default | Request limit |
|----------|----------------------|---------|---------------|
| Anthropic | `AnthropicPlugin::with_max_tokens` | 4096 | lowers the ceiling |
| Bedrock | `BedrockPlugin::with_max_tokens` / `BedrockProvider::with_max_tokens` | 4096 (`bedrock::DEFAULT_MAX_TOKENS`) | lowers the ceiling |
| OpenAI | none | none sent | sent as given |

To raise the ceiling for long responses or extended thinking, set it at
registration:

```rust
use core::num::NonZeroU32;

server.add_plugins(
    AnthropicPlugin::from_env("ANTHROPIC_API_KEY")
        .with_max_tokens(NonZeroU32::new(32_000).unwrap()),
);
```

To use fewer tokens for one call, set the limit on the request:

```rust
let response = llm
    .builder()
    .user("Summarize in one line.")
    .max_output_tokens(NonZeroU32::new(256).unwrap())
    .generate()
    .await?;
```

Thinking tokens count against the same output budget. Values above a model's
real output limit are sent unchanged, not clamped locally; the provider API
reports the error.

On Bedrock, model-specific settings that `LlmRequest` does not cover go in
`with_additional_model_request_fields` — see [AWS Bedrock](#aws-bedrock).

## Pricing

The built-in providers (Anthropic, OpenAI, Bedrock) answer `LlmProvider::pricing()` from a `PriceStore` in `polaris_model_providers::pricing`. The tracing decorator uses it to record `gen_ai.usage.cost`.

A store looks up a model in this order, each by longest model-id prefix:

1. its own overrides (`with_override`);
2. the fetched or cached price table (live stores only);
3. the built-in table (`fallback_table()`).

**Default: built-in prices only.** Every provider starts with `PriceStore::builtin()`. It never uses the network and never reads or writes a file. Built-in rates can go stale when a vendor changes prices.

**Opt in to live prices** with `with_pricing_store(PriceStore::live())` on the plugin or the provider:

```rust
use polaris_model_providers::pricing::PriceStore;

server.add_plugins(
    AnthropicPlugin::from_env("ANTHROPIC_API_KEY")
        .with_pricing_store(PriceStore::live()),
);
```

`PriceStore::live()` returns one store shared by the whole process. It:

- reads the cache file at `default_cache_path()` (`<cache dir>/polaris/model-prices.json`) when it is created;
- fetches `DEFAULT_SOURCE_URL` (LiteLLM's price JSON) on a background thread when prices are missing or older than `DEFAULT_TTL` (24 hours). `get` never waits for the network; it answers with the prices it already has;
- writes the cache file after a successful fetch;
- after a failed fetch, keeps the old prices and waits `FAILED_REFRESH_RETRY_DELAY` (15 minutes) before trying again.

Fetched data and the cache file are capped at `MAX_PRICE_DATA_BYTES` (32 MiB). A fetched row is dropped if a rate is not finite, is negative, is above 10,000 USD per million tokens, or if input and output are both zero. For another source use `PriceStore::new(source, cache_path, ttl)`; in tests use `PriceStore::with_fetcher` with a fake `PriceFetcher`, so no test touches the network.

**Per-provider overrides.** `with_pricing_override(prefix, ModelPricing)` on a provider (or `store.with_override(...)` before handing the store over) sets a rate that wins over every table. An override belongs to that handle only: other providers sharing the same live store do not see it, but they still share the fetched prices.

```rust
let provider = AnthropicProvider::new(api_key)
    .with_pricing_store(PriceStore::live())
    .with_pricing_override("claude-sonnet-4-6", ModelPricing::new(1.0, 5.0));
```

## AWS Bedrock

`BedrockPlugin::from_env()` loads credentials from the default AWS chain; `BedrockPlugin::from_sdk_config(config)` uses a config you built. Model ids are `bedrock/<bedrock model id>`, for example `bedrock/us.anthropic.claude-sonnet-4-6`.

**Two request paths.**

- Most models use the Converse API (`ConverseStream` for `stream`). Prompt-cache points are sent only to `anthropic.*` and `amazon.nova*` models.
- Models whose id contains `openai.` (for example `bedrock/global.openai.gpt-6-luna`) go to Bedrock's signed OpenAI Responses endpoint. That path needs the AWS SDK config to sign and route the call. `BedrockPlugin` passes it for you. A `BedrockProvider` you build yourself needs `.with_sdk_config(config)`; without it the call fails with `GenerationError::InvalidRequest("Bedrock SDK configuration is missing")`. Transient errors (HTTP errors, rate limits, 429/500/502/503/504) are retried with exponential backoff, up to the SDK retry config's max attempts. `stream` on this path makes one call and replays the result as a stream.

Because the Responses path reuses the OpenAI provider's types, the `bedrock` feature also enables the `openai` feature.

**Output limit.** `with_max_tokens(NonZeroU32)` sets the ceiling (default `bedrock::DEFAULT_MAX_TOKENS`, 4096). A request may lower it — see [Output token limits](#output-token-limits).

**Model-specific fields.** `with_additional_model_request_fields(serde_json::Value)` sends settings that `LlmRequest` does not cover:

- on Converse, as `additionalModelRequestFields`;
- on the Responses path, as top-level body keys, which replace any value the provider set for the same key. A value that is not a JSON object is ignored on this path.

```rust
// Fields depend on the model; this one is for an `openai.*` model.
server.add_plugins(
    BedrockPlugin::from_env()
        .with_max_tokens(NonZeroU32::new(16_000).unwrap())
        .with_additional_model_request_fields(serde_json::json!({
            "reasoning": { "effort": "high" }
        })),
);
```

`bedrock::normalize_bedrock_model_id` turns a Bedrock Anthropic id into the first-party Claude id by dropping everything up to `anthropic.` and a trailing version suffix (`us.anthropic.claude-opus-4-1-20250805-v1:0` → `claude-opus-4-1-20250805`). Other ids are returned unchanged.

## Prompt Caching

Prompt caching is a provider-agnostic request feature: the builder marks which parts of a request are stable enough to cache, and each provider decides how to honor them. A provider that supports caching translates the markers into its native cache directives; one that does not simply ignores them, so the same `LlmRequest` runs unchanged on every provider.

### Consumer side

The builder exposes cache breakpoints:

- `.cache_prefix()` — caches the stable prefix (system prompt + tool definitions). This is the dominant cost win, since that prefix is otherwise re-billed in full on every call.
- `.cache_breakpoint()` — marks "everything up to here is stable" as you assemble a context window, for incremental history caching.
- `.cache_breakpoints(indices)` — the bulk form, taking the message indices to mark in one call (handy when a context strategy computes them).

```rust
let response = llm.builder()
    .system("You are helpful")
    .cache_prefix()         // marks the system + tools prefix
    .user("Hello!")
    .generate()
    .await?;
```

These verbs populate `CacheControl` on the `LlmRequest`; the default (no verbs called) is an empty `CacheControl` that requests no caching.

### Provider side

When you implement `generate()` / `stream()`, read `request.cache` (a `CacheControl`) and translate it into your vendor's cache directives:

- **Honoring it.** The Anthropic provider maps the prefix marker onto the system block — or the last tool when there is no system prompt — and each message breakpoint onto the last block of the referenced message, emitting `cache_control: { "type": "ephemeral" }`. Anthropic honors at most four breakpoints per request, so the provider budgets markers and drops extras low-to-high (prefix first, then message breakpoints in order); a stale (out-of-range) breakpoint index is skipped and surfaced with a `tracing::warn!` rather than silently lost.
- **Ignoring it.** A provider with no cache support leaves `request.cache` untouched and sends the request as-is. Caching is therefore purely opt-in per provider, and adding a new provider never needs to think about it.

Report cache usage back on `LlmResponse::usage` so cost accounting stays accurate: set `Usage::cache_read_tokens` (input served from cache, billed at a steep discount) and `Usage::cache_creation_tokens` (input written to the cache the first time, billed at a small premium). The tracing decorator prices these against the cache tiers on `ModelPricing` — `cache_read_per_million_usd` / `cache_write_per_million_usd`, derived from the input rate by default and overridable via `ModelPricing::with_cache_rates` — and records the result in `gen_ai.usage.cost`. A provider that does not report cache tokens leaves both fields `None`, and the rollup neither counts nor prices them.

### Strict tool schemas

Each `ToolDefinition` carries a `strict` flag (see [Tools](tools.md#strict-mode-and-exposure)) requesting that the provider enforce the tool's JSON schema via constrained decoding. As with caching, honoring it is per-provider:

- **OpenAI** passes each tool's `strict` flag through verbatim — there is no per-request strict-tool cap. The schema is normalized (`additionalProperties: false`, all keys required, unsupported constructs stripped) only when the tool is strict; a non-strict tool keeps its full schema.
- **Anthropic** honors at most `MAX_STRICT_TOOLS` (20) strict tools per request. The provider budgets strictness in registration order — tools past the cap degrade to non-strict (and keep their full schema) so the request stays valid, surfacing each degradation with a `tracing::debug!`. A tool that opts out of strict does not consume a budget slot.

Strict mode is best-effort schema enforcement, not a correctness requirement: a degraded tool is still sent, just without constrained decoding.

## Testing

- **Unit-test** `LlmRequest → vendor` translation pure functions in isolation.
- **Integration-test** the full provider against a recorded HTTP fixture or a local mock server. See `crates/polaris_model_providers/tests/anthropic_integration.rs` for the pattern.
- Avoid hitting real APIs in CI — gate live tests behind an env var (e.g., `ANTHROPIC_INTEGRATION=1`).

## Key Files

| File | Purpose |
|------|---------|
| `polaris_models/src/llm/provider.rs` | `LlmProvider` trait |
| `polaris_models/src/llm/model.rs` | `LlmClient` wrapper |
| `polaris_models/src/llm/types.rs` | `LlmRequest`, `LlmResponse`, streaming types |
| `polaris_models/src/llm/error.rs` | `GenerationError` |
| `polaris_models/src/registry.rs` | `ModelRegistry` |
| `polaris_models/src/plugin.rs` | `ModelsPlugin` (two-phase init) |
| `polaris_model_providers/src/anthropic/` | Reference impl — Anthropic |
| `polaris_model_providers/src/openai/` | Reference impl — OpenAI |
| `polaris_model_providers/src/bedrock/` | Reference impl — AWS Bedrock |
| `polaris_model_providers/src/pricing.rs` | `PriceStore`, `PricingTable`, live price fetching |

## See Also

- [`polaris_models` README](../../crates/polaris_models/README.md) — usage-side docs (builder API, structured output, tool calling)
- [Plugins](plugins.md) — plugin lifecycle, dependencies, the two-phase init pattern
- [Tools](tools.md) — LLM tool calling via `ToolRegistry`
