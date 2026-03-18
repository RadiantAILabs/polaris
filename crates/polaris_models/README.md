# polaris_models

Model provider interface and registry for Polaris. Provides a unified, provider-agnostic API for interacting with different LLM providers.

## Setup

`ModelsPlugin` must be added alongside at least one provider plugin:

```rust
use polaris_models::ModelsPlugin;
use polaris_model_providers::AnthropicPlugin;
use polaris_system::server::Server;

let mut server = Server::new();
server.add_plugins(ModelsPlugin);
server.add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"));
```

Other providers (OpenAI, AWS Bedrock) are available via feature flags in `polaris_model_providers`.

## Usage

### Basic Generation

Use the builder API for ergonomic single-shot LLM calls:

```rust
let registry = server.get_global::<ModelRegistry>().unwrap();
let llm = registry.llm("anthropic/claude-sonnet-4-5-20250929")?;

let response = llm.builder()
    .system("You are a helpful assistant")
    .user("Hello!")
    .generate()
    .await?;

println!("{}", response.text());
```

Or construct an `LlmRequest` directly via struct literal:

```rust
use polaris_models::llm::{LlmRequest, Message};

let request = LlmRequest {
    system: Some("You are a helpful assistant".into()),
    messages: vec![Message::user("Hello!")],
    ..Default::default()
};

let response = llm.generate(request).await?;
```

### Structured Output

Extract typed data from LLM responses using JSON schema:

```rust
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Person {
    name: String,
    age: u32,
}

let person: Person = llm.builder()
    .user("Extract: John is 30 years old")
    .generate_structured()
    .await?;
```

### Tool Calling

```rust
let response = llm.builder()
    .with_definitions(vec![weather_tool])
    .require_tool()
    .user("What's the weather in Tokyo?")
    .generate()
    .await?;

let tool_calls = response.tool_calls();
```

### Multi-turn Conversations

Build conversations with alternating user/assistant messages:

```rust
let response = llm.builder()
    .system("You are a helpful assistant")
    .user("What is Rust?")
    .assistant("Rust is a systems programming language focused on safety and performance.")
    .user("What makes it unique?")
    .generate()
    .await?;
```

Or pass a full message history:

```rust
let response = llm.builder()
    .system("You are a helpful assistant")
    .messages(conversation_history)
    .generate()
    .await?;
```

### Using in Systems

Access the `ModelRegistry` as a resource in Polaris systems:

```rust
use polaris_models::ModelRegistry;
use polaris_system::param::Res;
use polaris_system::system::SystemError;
use polaris_system::system;

#[system]
async fn my_agent(registry: Res<ModelRegistry>) -> Result<String, SystemError> {
    let llm = registry
        .llm("anthropic/claude-sonnet-4-5-20250929")
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

## Creating a Provider Plugin

Custom providers implement the `LlmProvider` trait and register with the `ModelRegistry` during the plugin build phase:

```rust
use polaris_models::llm::{LlmProvider, LlmRequest, LlmResponse, GenerationError};
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

pub struct MyProvider { /* ... */ }

impl MyProvider {
    pub fn new() -> Self {
        MyProvider { /* ... */ }
    }
}

impl LlmProvider for MyProvider {
    fn name(&self) -> &'static str {
        "my_provider"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        // Call your provider's API
        todo!()
    }
}

pub struct MyProviderPlugin { /* ... */ }

impl Plugin for MyProviderPlugin {
    const ID: &'static str = "my_provider";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.add_plugins(ModelsPlugin);

        let mut registry = server.get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must be added first");
        registry.register_llm_provider(MyProvider::new());
    }

    fn ready(&self, _server: &mut Server) {}
}
```

The registry is available as a mutable resource during the `build()` phase, allowing providers to register themselves. After the `ready()` phase, it becomes an immutable global for thread-safe access at runtime.

Models are then accessible via `"myprovider/model-name"`.
