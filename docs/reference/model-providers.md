---
notion_page: https://www.notion.so/radiant-ai/Model-Providers-342afe2e695d80de84ecf1316afb58b1
title: Model Providers
---

# Model Providers

`polaris_models` defines the provider-agnostic LLM interface, and `polaris_model_providers` implements it for specific vendors (Anthropic, OpenAI, AWS Bedrock) and for servers that speak the OpenAI Chat Completions API. This document covers how to *extend* the framework with a new provider. For end-user usage of the registry, see the `polaris_models` README.

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
| Chat Completions | none | none sent | sent as given, as `max_completion_tokens` (or `max_tokens`, see [Chat Completions servers](#chat-completions-servers)) |

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

## Reasoning effort

`LlmRequest::reasoning_effort` (an `Option<ReasoningEffort>`, also set with `LlmRequestBuilder::reasoning_effort`) asks the model to spend a given amount of reasoning on a request. `None` leaves the provider's and the model's own default in place. Set it on every request you build if a run must stay at one effort: there is no registration-level default, so a decorator that wraps a provider can set it on each call.

```rust
use polaris_models::llm::ReasoningEffort;

let response = llm
    .builder()
    .reasoning_effort(ReasoningEffort::High)
    .user("Why is the sky blue?")
    .generate()
    .await?;
```

`ReasoningEffort` is `#[non_exhaustive]`: `None`, `Minimal`, `Low`, `Medium`, `High`, `XHigh`, `Max`, and `Other(String)` for a level only some servers define. It writes and reads its lower-case name (`Display`, `FromStr`, serde), so `"high".parse::<ReasoningEffort>()` works for a setting read from an environment variable. `FromStr` knows the named levels in any case, turns other non-empty text into `Other`, and fails on empty text.

Each provider sends it in its own way:

| Provider | Sent as | Levels it honours |
|----------|---------|-------------------|
| Chat Completions | `reasoning_effort` | every level, as the level's name |
| OpenAI (Responses) | `reasoning.effort` | every level, as the level's name |
| Anthropic | `output_config.effort` | `low`, `medium`, `high`, `xhigh`, `max`, including matching `Other` text |
| Bedrock, `openai.*` models | `reasoning.effort` | every level, as the level's name |
| Bedrock, Converse | not supported | none |

**Polaris never drops the setting.** A provider that cannot send the requested level returns `GenerationError::UnsupportedOperation` **before it sends anything**: Anthropic for names outside `low`, `medium`, `high`, `xhigh` and `max` (including `None` and `Minimal`; it cannot turn thinking off through effort), Bedrock Converse for every level (the Converse API has no effort setting, and the model-specific field that carries one differs per model). Anthropic checks the level's text, so `Other("low".into())` is accepted. On Bedrock Converse, keep using `with_additional_model_request_fields` with the model's own field and leave the request field unset.

**The server decides which levels a model has.** Providers that take every level send the name as written (`xhigh`, `max`, or an `Other` string); a server may ignore an unknown level, so check `Usage::reasoning_output_tokens` when the effort matters. If the server rejects the level, its error message reaches the caller as `GenerationError::Provider`. OpenAI's own `reasoning` object keeps other settings such as `summary` when an effort is added. For Bedrock `openai.*` models, a provider-wide field from `with_additional_model_request_fields` replaces the request's `reasoning` key, like any other key it sets.

**Implementing a provider.** Read `request.reasoning_effort` in `generate` and `stream`, put it in the request the vendor expects, and return `GenerationError::UnsupportedOperation` (with the level in the message) when the vendor has no way to take it. Do not ignore it.

## Pricing

The built-in providers (Anthropic, OpenAI, Chat Completions, Bedrock) answer `LlmProvider::pricing()` from a `PriceStore` in `polaris_model_providers::pricing`. The tracing decorator uses it to record `gen_ai.usage.cost`.

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

## OpenAI and compatible servers

`OpenAiProvider` talks to the OpenAI Responses API. It also works with other servers that implement the same API, which often send slightly different events.

**Select another server.** Set the `OPENAI_BASE_URL` environment variable to the server's base URL (for example `http://localhost:8000/v1`) before the provider is created. `OpenAiProvider::new` and `OpenAiPlugin::from_env` read it, and the API key you pass is sent to that server. There is no builder method for the URL.

- **Missing fields are filled in.** Each event is read as raw JSON first. Fields the provider does not use, or can work out, get a default, such as `sequence_number`, `item_id`, `annotations`, `content_index`, `summary_index`, usage details (missing token counts become 0), response metadata, and `output_index` (taken from the item ID, or the most recent item). A field the provider needs and cannot work out, such as the text of a text delta, still fails with `GenerationError::Json`. The stream fails with `GenerationError::InvalidResponse` if an event is not a JSON object or has no `type`.
- **Event types the provider does not use are skipped** before they are parsed, so an unknown or unusual event cannot fail the stream.
- **Tool name and `call_id` may arrive late.** If `response.output_item.added` has no tool name or no `call_id`, the provider holds the tool call until `response.output_item.done` and takes them from there. If the name or the `call_id` is still missing then, the stream fails with `GenerationError::InvalidResponse`. The stream also fails with `InvalidResponse`, naming the call, if it ends (completed, incomplete or failed) while a tool call is still held.
- **Non-streaming `generate` uses the same parsing.** A function call with an empty name or an empty `call_id` fails with `GenerationError::InvalidResponse`.

### Tool results need the call's `call_id`

OpenAI links a tool result to its call by `call_id`, not by `id`. This applies to the whole OpenAI Responses path, including Bedrock `openai.*` models, which reuse the OpenAI request conversion. Anthropic and Bedrock Converse link results by `id`.

Build results from the call so both are copied: `ToolCall::result` / `ToolCall::error`, or `Message::tool_result_for` / `Message::tool_error_for`. The result then works on every provider.

```rust
use polaris_models::llm::{Message, ToolResultContent};

// `call` is a `ToolCall` taken from the model's response.
let reply = Message::tool_result_for(&call, ToolResultContent::Text("Sunny".into()));
```

The older `UserBlock::tool_result` / `tool_error` and `Message::tool_result` / `tool_error` set only `id`, so these providers reject them (a missing or empty `call_id` fails with "Tool result is missing a call_id"); they are deprecated. If you have only the two ids and not the `ToolCall`, use `UserBlock::tool_result_with_call_id` / `tool_error_with_call_id` or `Message::tool_result_with_call_id` / `tool_error_with_call_id`.

The Chat Completions provider is different: it links a result to its call by the call's `call_id` when that is set and by `id` otherwise, and it sets both to the server's id on the calls it returns. Results built from the call work there as well, and so do the older constructors.

## Chat Completions servers

`ChatCompletionsPlugin` (feature `chat-completions`, no extra dependency) reaches any server that implements `POST {base_url}/chat/completions`: the Hugging Face inference router, vLLM, SGLang, Ollama, DeepInfra, Z.ai, DeepSeek, OpenAI's own Chat Completions endpoint and others. Models are `chat_completions/<model>`, where `<model>` is the exact string the server expects in its `model` field. It may contain `/` and `:`, as in `chat_completions/zai-org/GLM-5.3-Flash:zai`; the registry splits only at the first `/`.

```rust
use polaris_model_providers::chat_completions::MaxTokensField;

server.add_plugins(ModelsPlugin);
server.add_plugins(
    ChatCompletionsPlugin::from_env("HF_TOKEN")
        .with_base_url("https://router.huggingface.co/v1")
        .with_max_tokens_field(MaxTokensField::MaxTokens),
);
let llm = registry.llm("chat_completions/zai-org/GLM-5.3-Flash:zai")?;
```

**A separate plugin, not a mode of `OpenAiPlugin`.** The two speak different protocols with different failure modes. A mode switch would make `openai/<model>` mean one protocol or the other depending on configuration, and a server could not use both. Separate plugins register under different provider names (`openai`, `chat_completions`), so both can be added and each model id says which protocol it uses. Only one `ChatCompletionsPlugin` can be registered, because the provider name is the registry key; registering a second panics during `build()`. To reach two servers, wrap two `ChatCompletionsProvider`s in a provider of your own.

**Configuration.** The key is sent as `Authorization: Bearer <key>`; `without_api_key()` sends no header for local servers. No environment variable is read except by `from_env(var)`, which names the variable that holds the key.

| Method | Meaning |
|--------|---------|
| `with_base_url(url)` | Server root; `/chat/completions` is appended. Default `https://api.openai.com/v1`. A trailing `/` is ignored. |
| `with_extra_body(fields)` | Takes `serde_json::Map<String, Value>` for top-level fields such as Z.ai's `thinking`, vLLM's `chat_template_kwargs`, or `top_k`. Repeated calls merge fields; later values replace earlier ones. Requests fail with `GenerationError::InvalidRequest` for reserved keys `model`, `messages`, `stream`, `stream_options`, or for `reasoning_effort` when the request also sets it. Other keys replace any value Polaris set. |
| `with_max_tokens_field(MaxTokensField)` | `MaxCompletionTokens` (default) or `MaxTokens`: the key that carries `LlmRequest::max_output_tokens`. OpenAI rejects `max_tokens` for reasoning models, so the default is right for OpenAI; many other servers read only `max_tokens`. **For the Hugging Face router use `MaxTokens`**: its request schema lists `max_tokens` only, and a backend that ignores `max_completion_tokens` would run without the cap you asked for. |
| `with_reasoning_replay(ReasoningReplay)` | How earlier reasoning is sent back: `AsReceived` (default), `ReasoningContent`, `Reasoning`, or `Omit`. See below. |
| `with_timeouts(Timeouts)` | Connect timeout (default 30 s) and idle timeout (default 5 min): the longest wait for more data. There is no limit on the whole request, so a long reasoning stream is never cut off while data keeps arriving. A server that accepts a request and goes silent, or a stream that stalls, fails with `GenerationError::Http` after the idle timeout. A non-streaming call receives nothing until the answer is complete, so raise the idle timeout above the longest answer you will wait for. |
| `with_strict_tools(bool)` | Default `false`: tools are sent without `strict` and with their schema as given, because `strict` is an OpenAI field that other servers may reject. `true` sends each `ToolDefinition::strict` and normalizes strict schemas like the Responses provider does. |
| `with_pricing_store`, `with_pricing_override` | As on the other providers. Most models served this way are not in the built-in price table, so set a rate with `with_pricing_override(prefix, ModelPricing)` to get `gen_ai.usage.cost`. |

**What is sent.** The system prompt becomes a first `system` message. User text goes as a string (several text blocks are joined with newlines); with images, as `text` and `image_url` parts with a base64 data URL (JPEG, PNG, GIF and WebP). Each tool result becomes its own `tool` message, placed before any text that shared its user message, because `tool` messages must directly follow the assistant message that made the calls. A failed result's text gets an `Error:` prefix and a space. A `tool` message takes text only, so an image result is sent as a `tool` message saying the image is in the next message, and the image goes into the user message that follows the tool messages, as an `image_url` part. Tools go as `tools`, `tool_choice` as `auto`, `required`, `none` or a named function, and an output schema as `response_format` of type `json_schema`. Streaming adds `stream: true` and `stream_options.include_usage: true`; without the second, OpenAI-style servers send no usage in a stream. Audio and document blocks fail with `GenerationError::UnsupportedContent`. Prompt-cache markers on the request are ignored (servers cache on their own, and cached tokens show up in the usage). Requests carry a `User-Agent` of `polaris-ai/<version>`. A tool call and its result go by the call's `call_id` when it has one (calls that came from the Responses API have both) and by `id` otherwise, so a conversation can move between providers.

**Reasoning from the model.** Servers return it next to the answer, as `reasoning_content` or, on newer servers, `reasoning`. The provider reads either (`reasoning_content` first) into an `AssistantBlock::Reasoning` placed before the text and tool calls, and streams it as a reasoning block. Sending it back is where servers differ, so `ReasoningReplay` chooses:

- **`AsReceived` (default)** learns the field from the latest reply for the same model that carries reasoning through this provider (a router that answers with `reasoning` gets `reasoning` back; one that answers with `reasoning_content` gets that). This state is kept per model; before any such reply, including after a process restart, earlier reasoning is sent as `reasoning_content`. Set an explicit value below when a server answers under one name and reads another.
- **`ReasoningContent`** always sends it on the assistant message as `reasoning_content`. DeepSeek's thinking mode needs this on every later request that carries tools: without tools the field is ignored, with tools a request that omits the earlier `reasoning_content` is answered with `400` ([DeepSeek thinking mode](https://api-docs.deepseek.com/guides/thinking_mode)). Z.ai's Preserved Thinking needs the complete, unmodified and correctly ordered `reasoning_content` of earlier turns, with `thinking.clear_thinking` set to `false` ([Z.ai thinking mode](https://docs.z.ai/guides/capabilities/thinking-mode), [chat completion reference](https://docs.z.ai/api-reference/llm/chat-completion)); set that with `with_extra_body`.
- **`Reasoning`** always sends it as `reasoning`, the name newer vLLM versions use in responses ([vLLM reasoning outputs](https://docs.vllm.ai/en/latest/features/reasoning_outputs/)). Use it only for a server that is known to read that name on input.
- **`Omit`** sends no earlier reasoning, for servers that reject the unknown field.

Reasoning blocks from other providers are sent the same way (their signatures are dropped). Several reasoning blocks in one assistant message are joined in their original order, with nothing added between them, because servers that check replayed reasoning expect it exactly as they produced it. Reasoning written inline in `content` between `<think>` tags, by a server that has no reasoning parser, is not split out.

**What is read.** Responses and stream chunks are read as raw JSON, so a server that leaves fields out still works:

- `content` may be a string, `null` or a list of parts; the first choice (index 0) is read; a `refusal` with no other content is a `GenerationError::Refusal`.
- A tool call without an `id` gets `<response id>_call_<position>`, or, when the response has no id either, `chatcmpl-<n>_call_<position>` with `<n>` from a process-wide counter, so ids do not repeat from one turn to the next. `arguments` may be a JSON string, an object or empty (no arguments). Arguments that are not valid JSON fail the response with `GenerationError::InvalidResponse`, as the stream collector does. A call with no function name fails the same way. In a stream, argument fragments that arrive before the name are held until it does; calls are told apart by `index`, or by `id` when a server sends no `index`.
- Stop reasons: `length` is `MaxOutputTokens` (even when a tool call was cut off), `tool_calls` is `ToolUse`, `content_filter` is `ContentFilter`, `stop` is `EndTurn` (or `ToolUse` when the message has tool calls, because some servers say `stop` after a tool call), a missing reason is inferred the same way, and any other value is `StopReason::Other`.
- Usage: `input_tokens` is `prompt_tokens` without the cached part (`prompt_tokens_details.cached_tokens`, or DeepSeek's `prompt_cache_hit_tokens`), `cache_read_tokens` is the cached part, `reasoning_output_tokens` is `completion_tokens_details.reasoning_tokens`, `output_tokens` is `completion_tokens` as the server counts it. A count a server leaves out stays `None`.
- A non-streaming body larger than 16 MiB fails with `InvalidResponse`; an error body is read up to 1 MiB and cut there.
- Exceeding a limit on buffered tool arguments, refusal text or tool-slot count, or overflowing a tool index, terminates the stream with `InvalidResponse`.
- A stream ends at `data: [DONE]`, or when the connection closes after a finish reason. The usage chunk that servers send after the finish chunk is included in the final `MessageStop`. A stream that closes before any finish reason fails with `InvalidResponse`, and an `error` object in a chunk fails it with `Provider`.

**Errors.** `401` and `403` are `Auth`, `429` is `RateLimited` (with `Retry-After` when it is a number of seconds), anything else is `Provider` with the status and the server's message, read from `{"error": {"message": ..}}`, `{"error": ".."}`, `{"message": ..}` or `{"detail": ..}`. An unreadable error body preserves the status classification, including `Auth` and `RateLimited`. A `200` response whose body is an error object is also `Provider`.

**Not supported:** `n` greater than 1 (only the first choice is read), the legacy `function_call` field, and audio and document input.

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
| `polaris_model_providers/src/chat_completions/` | Reference impl — Chat Completions servers (raw JSON, SSE) |
| `polaris_model_providers/src/sse.rs` | Server-sent event framing (lines end in LF or CRLF; a lone CR is not supported) used by the Chat Completions provider |
| `polaris_model_providers/src/bedrock/` | Reference impl — AWS Bedrock |
| `polaris_model_providers/src/pricing.rs` | `PriceStore`, `PricingTable`, live price fetching |

## See Also

- [`polaris_models` README](../../crates/polaris_models/README.md) — usage-side docs (builder API, structured output, tool calling)
- [Plugins](plugins.md) — plugin lifecycle, dependencies, the two-phase init pattern
- [Tools](tools.md) — LLM tool calling via `ToolRegistry`
