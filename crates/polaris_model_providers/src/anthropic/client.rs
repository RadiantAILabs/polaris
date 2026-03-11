//! Anthropic API client.

use super::types::{CreateMessageRequest, MessageResponse};
use polaris_models::llm::GenerationError;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};

/// HTTP client for the Anthropic Messages API.
#[derive(Clone)]
pub struct AnthropicClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicClient {
    /// Creates a new client.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    /// Sends a create message request to the Anthropic API.
    pub async fn create_message(
        &self,
        request: &CreateMessageRequest,
    ) -> Result<MessageResponse, GenerationError> {
        let url = format!("{}/v1/messages", self.base_url);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "X-Api-Key",
            HeaderValue::from_str(&self.api_key)
                .map_err(|err| GenerationError::Auth(format!("Invalid API key header: {err}")))?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));

        let has_strict_tools = request
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|t| t.strict == Some(true)));

        // Structured outputs and tools with strict schemas require a beta header.
        if request.output_format.is_some() || has_strict_tools {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("structured-outputs-2025-11-13"),
            );
        }

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(request)
            .send()
            .await
            .map_err(|err| GenerationError::Http(err.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| GenerationError::Http(err.to_string()))?;

        if !status.is_success() {
            return Err(GenerationError::Provider {
                status: Some(status.as_u16()),
                message: body,
                source: None,
            });
        }

        serde_json::from_str(&body).map_err(|err| {
            GenerationError::InvalidResponse(format!(
                "Failed to parse response: {err}\nBody: {body}"
            ))
        })
    }
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}
