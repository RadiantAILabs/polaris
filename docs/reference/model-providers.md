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

The `name()` must be stable and lowercase — it becomes the prefix in `"<name>/<model>"`. `stream()` has a default implementation that returns `UnsupportedOperation`; override it to support streaming. `pricing()` is optional — return `Some(ModelPricing::new(input_per_million_usd, output_per_million_usd))` to publish per-million-token rates for a given model; the tracing decorator multiplies token counts by the rate and records `polaris.gen_ai.cost_usd` on chat spans, so cost surfaces in any console subscriber or OTel exporter. The defaulted `None` means cost stays unset end-to-end. `endpoint()` is optional — return the base URL the provider sends requests to (or the proxy address, if one is used); the tracing decorator records it as the `server.address` attribute on chat spans. The defaulted `None` leaves the attribute unset.

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

Report cache usage back on `LlmResponse::usage` so cost accounting stays accurate: set `Usage::cache_read_tokens` (input served from cache, billed at a steep discount) and `Usage::cache_creation_tokens` (input written to the cache the first time, billed at a small premium). The tracing decorator prices these against the cache tiers on `ModelPricing` — `cache_read_per_million_usd` / `cache_write_per_million_usd`, derived from the input rate by default and overridable via `ModelPricing::with_cache_rates` — and records the result in `polaris.gen_ai.cost_usd`. A provider that does not report cache tokens leaves both fields `None`, and the rollup neither counts nor prices them.

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

## See Also

- [`polaris_models` README](../../crates/polaris_models/README.md) — usage-side docs (builder API, structured output, tool calling)
- [Plugins](plugins.md) — plugin lifecycle, dependencies, the two-phase init pattern
- [Tools](tools.md) — LLM tool calling via `ToolRegistry`
