//! Integration tests for the AWS Bedrock provider.
//!
//! These tests are ignored by default because they require AWS credentials.
//!
//! To run these tests:
//! ```sh
//! cargo test -p polaris_model_providers --features bedrock --test bedrock_integration -- --ignored
//! ```

#![cfg(feature = "bedrock")]

mod common;

use common::{LlmTestExt, init_env};
use polaris_model_providers::bedrock::BedrockPlugin;
use polaris_models::llm::Llm;
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::server::Server;

const MODEL: &str = "aws.bedrock/global.anthropic.claude-sonnet-4-5-20250929-v1:0";

fn get_llm(model_id: &str) -> Llm {
    init_env();

    let mut server = Server::new();
    server.add_plugins(ModelsPlugin::default());
    server.add_plugins(BedrockPlugin::from_env());
    server.finish();

    let registry = server
        .get_global::<ModelRegistry>()
        .expect("ModelRegistry should be available");
    registry.llm(model_id).expect("model should be valid")
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_basic_generation() {
    get_llm(MODEL).test_basic_generation().await;
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_system_prompt() {
    get_llm(MODEL).test_system_prompt().await;
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_tool_calling() {
    get_llm(MODEL).test_tool_calling().await;
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_structured_output() {
    get_llm(MODEL).test_structured_output().await;
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_invalid_model_error() {
    get_llm("aws.bedrock/not-a-real-model")
        .test_invalid_model_error()
        .await;
}

#[tokio::test]
#[ignore = "requires AWS credentials"]
async fn test_image_input() {
    get_llm(MODEL).test_image_input().await;
}
