//! Shared test helpers for provider integration tests.

use polaris_models::llm::{
    AssistantBlock, ImageMediaType, Llm, LlmRequest, LlmResponse, Message, ToolCall, ToolChoice,
    ToolDefinition, UserBlock,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::future::Future;
use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize environment variables from `.env` file (once).
pub fn init_env() {
    INIT.call_once(|| {
        let _ = dotenvy::dotenv();
    });
}

/// A small 10x10 red PNG image encoded as base64.
const RED_SQUARE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAoAAAAKCAIAAAACUFjqAAAAEklEQVR4nGP4z8CAB+GTG8HSALfKY52fTcuYAAAAAElFTkSuQmCC";

/// A simple person struct for testing structured output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Person {
    pub name: String,
    pub age: u32,
    pub occupation: Option<String>,
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather in a location".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "The city and state, e.g. San Francisco, CA"
                }
            },
            "required": ["location"],
            "additionalProperties": false
        }),
    }
}

fn extract_tool_calls(response: &LlmResponse) -> Vec<&ToolCall> {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

/// Extension trait for testing LLM providers.
pub trait LlmTestExt {
    /// Tests basic generation - expects the model to say "hello".
    fn test_basic_generation(&self) -> impl Future<Output = ()> + Send;

    /// Tests generation with a system prompt.
    fn test_system_prompt(&self) -> impl Future<Output = ()> + Send;

    /// Tests tool calling - expects the model to call the weather tool.
    fn test_tool_calling(&self) -> impl Future<Output = ()> + Send;

    /// Tests structured output - expects the model to extract person info.
    fn test_structured_output(&self) -> impl Future<Output = ()> + Send;

    /// Tests image input - expects the model to identify a red image.
    fn test_image_input(&self) -> impl Future<Output = ()> + Send;

    /// Tests that an invalid model returns an error.
    fn test_invalid_model_error(&self) -> impl Future<Output = ()> + Send;
}

impl LlmTestExt for Llm {
    async fn test_basic_generation(&self) {
        let request = LlmRequest {
            messages: vec![Message::user("Say 'hello' and nothing else.")],
            ..Default::default()
        };

        let response = self
            .generate(request)
            .await
            .expect("generation should succeed");

        let text = response.text().to_lowercase();
        assert!(
            text.contains("hello"),
            "response should contain 'hello': {text}"
        );
    }

    async fn test_system_prompt(&self) {
        let request = LlmRequest {
            system: Some("You are a pirate. Always respond in pirate speak.".to_string()),
            messages: vec![Message::user("Say hello")],
            ..Default::default()
        };

        let response = self
            .generate(request)
            .await
            .expect("generation should succeed");

        assert!(!response.text().is_empty(), "response should not be empty");
    }

    async fn test_tool_calling(&self) {
        let request = LlmRequest {
            messages: vec![Message::user("What's the weather like in Tokyo?")],
            tools: Some(vec![weather_tool()]),
            tool_choice: Some(ToolChoice::Required),
            ..Default::default()
        };

        let response = self
            .generate(request)
            .await
            .expect("generation should succeed");

        let tool_calls = extract_tool_calls(&response);

        assert!(!tool_calls.is_empty(), "should have at least one tool call");
        assert_eq!(
            tool_calls[0].function.name, "get_weather",
            "response should call get_weather function"
        );

        let args = &tool_calls[0].function.arguments;
        assert!(
            args.get("location").is_some(),
            "Tool call should have location argument: {args:?}"
        );
    }

    async fn test_structured_output(&self) {
        let request = LlmRequest {
            messages: vec![Message::user(
                "Extract the person information: John Smith is a 35 year old software engineer.",
            )],
            ..Default::default()
        };

        let person: Person = self
            .generate_structured(request)
            .await
            .expect("structured generation should succeed");

        assert_eq!(person.name, "John Smith");
        assert_eq!(person.age, 35);
        assert_eq!(
            person.occupation.as_deref().map(str::to_lowercase),
            Some("software engineer".to_string()),
            "occupation should be software engineer: {:?}",
            person.occupation
        );
    }

    async fn test_image_input(&self) {
        let message = Message::User {
            content: vec![
                UserBlock::image_base64(RED_SQUARE_PNG_BASE64, ImageMediaType::PNG),
                UserBlock::text(
                    "What color is this image? Reply with just the color name, nothing else.",
                ),
            ],
        };

        let request = LlmRequest {
            messages: vec![message],
            ..Default::default()
        };

        let response = self
            .generate(request)
            .await
            .expect("generation should succeed");

        let text = response.text().to_lowercase();
        assert!(
            text.contains("red"),
            "model should identify the red color in the image: {text}"
        );
    }

    async fn test_invalid_model_error(&self) {
        let request = LlmRequest {
            messages: vec![Message::user("Hello")],
            ..Default::default()
        };
        let result = self.generate(request).await;

        assert!(result.is_err(), "should fail with invalid model");
    }
}
