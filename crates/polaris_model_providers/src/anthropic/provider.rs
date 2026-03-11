//! Anthropic [`LlmProvider`] implementation.

use super::client::AnthropicClient;
use super::types::{
    ContentBlock, ContentBlockParam, CreateMessageRequest, ImageMediaType, ImageSource,
    MessageParam, OutputFormat, Role, ToolChoiceParam, ToolDef, ToolResultBlock, ToolResultContent,
};
use async_trait::async_trait;
use polaris_models::llm::{
    AssistantBlock, GenerationError, ImageBlock, ImageMediaType as PolarisImageMediaType,
    LlmProvider, LlmRequest, LlmResponse, Message, ToolCall, ToolChoice, ToolFunction,
    ToolResultContent as PolarisToolResult, ToolResultStatus, Usage, UserBlock,
};

/// Default maximum tokens for generation requests.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic [`LlmProvider`] implementation.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: AnthropicClient,
}

impl AnthropicProvider {
    /// Creates a new provider.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: AnthropicClient::new(api_key),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        let anthropic_request = convert_request(model, &request)?;

        let response = self.client.create_message(&anthropic_request).await?;

        Ok(convert_response(response))
    }
}

fn convert_request(
    model: &str,
    request: &LlmRequest,
) -> Result<CreateMessageRequest, GenerationError> {
    let messages = request
        .messages
        .iter()
        .map(convert_message)
        .collect::<Result<Vec<_>, _>>()?;

    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|tool| ToolDef {
                name: tool.name.clone(),
                description: Some(tool.description.clone()),
                input_schema: tool.parameters.clone(),
                strict: Some(true),
            })
            .collect()
    });

    let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

    let output_format = request
        .output_schema
        .as_ref()
        .map(|schema| OutputFormat::new(schema.clone()));

    Ok(CreateMessageRequest {
        model: model.to_string(),
        max_tokens: DEFAULT_MAX_TOKENS,
        messages,
        system: request.system.clone(),
        tools,
        tool_choice,
        temperature: None,
        stop_sequences: None,
        output_format,
    })
}

fn convert_message(message: &Message) -> Result<MessageParam, GenerationError> {
    match message {
        Message::User { content } => {
            let blocks = content
                .iter()
                .map(convert_user_block)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MessageParam {
                role: Role::User,
                content: blocks,
            })
        }
        Message::Assistant { content, .. } => {
            let blocks = content
                .iter()
                .map(convert_assistant_block)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MessageParam {
                role: Role::Assistant,
                content: blocks,
            })
        }
    }
}

fn convert_image_to_source(image: &ImageBlock) -> Result<ImageSource, GenerationError> {
    let media_type = match image.media_type {
        PolarisImageMediaType::JPEG => ImageMediaType::Jpeg,
        PolarisImageMediaType::PNG => ImageMediaType::Png,
        PolarisImageMediaType::GIF => ImageMediaType::Gif,
        PolarisImageMediaType::WEBP => ImageMediaType::Webp,
        ref other => {
            return Err(GenerationError::UnsupportedContent(format!(
                "Unsupported image media type for Anthropic: {other:?}"
            )));
        }
    };

    match &image.data {
        polaris_models::llm::DocumentSource::Base64(data) => Ok(ImageSource::Base64 {
            media_type,
            data: data.clone(),
        }),
    }
}

fn convert_user_block(block: &UserBlock) -> Result<ContentBlockParam, GenerationError> {
    match block {
        UserBlock::Text(block) => Ok(ContentBlockParam::Text {
            text: block.text.clone(),
        }),
        UserBlock::Image(image) => {
            let source = convert_image_to_source(image)?;
            Ok(ContentBlockParam::Image { source })
        }
        UserBlock::Audio(_) => Err(GenerationError::UnsupportedContent(
            "Audio content is not supported by Anthropic".to_string(),
        )),
        UserBlock::Document(_) => Err(GenerationError::UnsupportedContent(
            "Document content is not yet implemented for Anthropic".to_string(),
        )),
        UserBlock::ToolResult(result) => {
            let content = match &result.content {
                PolarisToolResult::Text(text) => Some(ToolResultContent::Text(text.clone())),
                PolarisToolResult::Image(image) => {
                    let source = convert_image_to_source(image)?;
                    Some(ToolResultContent::Blocks(vec![ToolResultBlock::Image {
                        source,
                    }]))
                }
            };
            let is_error = match result.status {
                ToolResultStatus::Success => None,
                ToolResultStatus::Error => Some(true),
            };
            Ok(ContentBlockParam::ToolResult {
                tool_use_id: result.id.clone(),
                content,
                is_error,
            })
        }
    }
}

fn convert_assistant_block(block: &AssistantBlock) -> Result<ContentBlockParam, GenerationError> {
    match block {
        AssistantBlock::Text(block) => Ok(ContentBlockParam::Text {
            text: block.text.clone(),
        }),
        AssistantBlock::ToolCall(call) => Ok(ContentBlockParam::ToolUse {
            id: call.id.clone(),
            name: call.function.name.clone(),
            input: call.function.arguments.clone(),
        }),
        AssistantBlock::Reasoning(reasoning) => {
            let signature = reasoning.signature.clone().unwrap_or_default();
            Ok(ContentBlockParam::Thinking {
                thinking: reasoning.reasoning.join("\n"),
                signature,
            })
        }
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> ToolChoiceParam {
    match choice {
        ToolChoice::Auto => ToolChoiceParam::Auto {
            disable_parallel_tool_use: None,
        },
        ToolChoice::Required => ToolChoiceParam::Any {
            disable_parallel_tool_use: None,
        },
        ToolChoice::Specific(name) => ToolChoiceParam::Tool {
            name: name.clone(),
            disable_parallel_tool_use: None,
        },
        ToolChoice::None => ToolChoiceParam::None,
    }
}

fn convert_response(response: super::types::MessageResponse) -> LlmResponse {
    let content = response
        .content
        .into_iter()
        .filter_map(convert_content_block)
        .collect();

    LlmResponse {
        content,
        usage: Usage {
            input_tokens: Some(response.usage.input_tokens),
            output_tokens: Some(response.usage.output_tokens),
            total_tokens: Some(response.usage.input_tokens + response.usage.output_tokens),
        },
    }
}

fn convert_content_block(block: ContentBlock) -> Option<AssistantBlock> {
    match block {
        ContentBlock::Text { text } => Some(AssistantBlock::Text(polaris_models::llm::TextBlock {
            text,
        })),
        ContentBlock::ToolUse { id, name, input } => Some(AssistantBlock::ToolCall(ToolCall {
            id: id.clone(),
            call_id: None,
            function: ToolFunction {
                name,
                arguments: input,
            },
            signature: None,
            additional_params: None,
        })),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => Some(AssistantBlock::Reasoning(
            polaris_models::llm::ReasoningBlock {
                id: None,
                reasoning: vec![thinking],
                signature: Some(signature),
            },
        )),
        ContentBlock::RedactedThinking { .. } => None,
    }
}
