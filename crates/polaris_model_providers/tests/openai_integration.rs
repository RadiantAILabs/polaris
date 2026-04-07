//! Integration tests for the `OpenAI` provider.
//!
//! These tests are ignored by default because they require:
//! - `OPENAI_API_KEY` environment variable (or in `.env` file)
//! - Network access to the `OpenAI` API
//! - May incur API costs
//!
//! To run these tests:
//! ```sh
//! cargo test -p polaris_model_providers --features openai --test openai_integration -- --ignored
//! ```

#![cfg(feature = "openai")]

mod common;

use common::{LlmTestExt, init_env};
use polaris_model_providers::openai::OpenAiPlugin;
use polaris_models::llm::Llm;
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::server::Server;

const MODEL: &str = "openai/gpt-4o";

async fn get_llm(model_id: &str) -> Llm {
    init_env();

    let mut server = Server::new();
    server.add_plugins(ModelsPlugin::default());
    server.add_plugins(OpenAiPlugin::from_env("OPENAI_API_KEY"));
    server.finish().await;

    let registry = server
        .get_global::<ModelRegistry>()
        .expect("ModelRegistry should be available");
    registry.llm(model_id).expect("model should be valid")
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_basic_generation() {
    get_llm(MODEL).await.test_basic_generation().await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_system_prompt() {
    get_llm(MODEL).await.test_system_prompt().await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_tool_calling() {
    get_llm(MODEL).await.test_tool_calling().await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_structured_output() {
    get_llm(MODEL).await.test_structured_output().await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_invalid_model_error() {
    get_llm("openai/not-a-real-model")
        .await
        .test_invalid_model_error()
        .await;
}

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY"]
async fn test_image_input() {
    get_llm(MODEL).await.test_image_input().await;
}
