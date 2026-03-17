//! `OpenAI` [`LlmProvider`] implementation using the Responses API.

use crate::schema::normalize_schema_for_strict_mode;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::{
    CreateResponseArgs, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputContent, InputImageContent,
    InputItem, InputParam, InputTextContent, Item, OutputItem, OutputMessageContent, ReasoningItem,
    Response, ResponseFormatJsonSchema, ResponseTextParam, ResponseUsage, Role, SummaryPart,
    SummaryTextContent, TextResponseFormatConfiguration, Tool, ToolChoiceFunction,
    ToolChoiceOptions, ToolChoiceParam,
};
use async_trait::async_trait;
use polaris_models::llm::{
    AssistantBlock, GenerationError, ImageMediaType, LlmProvider, LlmRequest, LlmResponse, Message,
    ReasoningBlock, StopReason, TextBlock, ToolCall, ToolChoice, ToolFunction,
    ToolResultContent as PolarisToolResult, ToolResultStatus, Usage, UserBlock,
};

/// `OpenAI` [`LlmProvider`] implementation using the Responses API.
pub struct OpenAiProvider {
    client: async_openai::Client<OpenAIConfig>,
}

impl OpenAiProvider {
    /// Creates a new provider with the given API key.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        let config = OpenAIConfig::new().with_api_key(api_key);
        Self {
            client: async_openai::Client::with_config(config),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        let create_response = convert_request(model, &request)?;
        let response = self
            .client
            .responses()
            .create(create_response)
            .await
            .map_err(convert_error)?;
        convert_response(response)
    }
}

// ---------------------------------------------------------------------------
// Request conversion (Polaris -> OpenAI)
// ---------------------------------------------------------------------------

fn convert_request(
    model: &str,
    request: &LlmRequest,
) -> Result<async_openai::types::responses::CreateResponse, GenerationError> {
    let input_items = convert_messages(&request.messages)?;

    let tools: Option<Vec<Tool>> = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|tool| {
                let normalized_parameters =
                    normalize_schema_for_strict_mode(tool.parameters.clone());
                Tool::Function(FunctionTool {
                    name: tool.name.clone(),
                    description: Some(tool.description.clone()),
                    parameters: Some(normalized_parameters),
                    strict: Some(true),
                })
            })
            .collect()
    });

    let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

    let text = request.output_schema.as_ref().map(|schema| {
        let normalized = normalize_schema_for_strict_mode(schema.clone());
        ResponseTextParam {
            format: TextResponseFormatConfiguration::JsonSchema(ResponseFormatJsonSchema {
                name: "structured_output".to_string(),
                description: None,
                schema: Some(normalized),
                strict: Some(true),
            }),
            verbosity: None,
        }
    });

    let mut builder = CreateResponseArgs::default();
    builder.model(model).input(InputParam::Items(input_items));

    if let Some(system) = &request.system {
        builder.instructions(system.clone());
    }
    if let Some(tools) = tools {
        builder.tools(tools);
    }
    if let Some(tool_choice) = tool_choice {
        builder.tool_choice(tool_choice);
    }
    if let Some(text) = text {
        builder.text(text);
    }

    builder.build().map_err(|build_err| {
        GenerationError::InvalidRequest(format!("Failed to build CreateResponse: {build_err}"))
    })
}

fn convert_messages(messages: &[Message]) -> Result<Vec<InputItem>, GenerationError> {
    let mut items = Vec::new();

    for message in messages {
        match message {
            Message::User { content } => {
                convert_user_message(content, &mut items)?;
            }
            Message::Assistant { content, .. } => {
                convert_assistant_message(content, &mut items)?;
            }
        }
    }

    Ok(items)
}

fn convert_user_message(
    blocks: &[UserBlock],
    items: &mut Vec<InputItem>,
) -> Result<(), GenerationError> {
    // Separate tool results from regular content blocks.
    // Tool results become top-level InputItem entries, while text/image
    // blocks get grouped into a single EasyInputMessage.
    let mut content_parts: Vec<InputContent> = Vec::new();

    for block in blocks {
        match block {
            UserBlock::Text(block) => {
                content_parts.push(InputContent::InputText(InputTextContent {
                    text: block.text.clone(),
                }));
            }
            UserBlock::Image(image) => {
                let data_url = build_image_data_url(image)?;
                content_parts.push(InputContent::InputImage(InputImageContent {
                    image_url: Some(data_url),
                    file_id: None,
                    detail: Default::default(),
                }));
            }
            UserBlock::ToolResult(result) => {
                // Each tool result is a separate top-level item.
                // Flush any accumulated content first.
                flush_content_parts(&mut content_parts, Role::User, items);

                let output_text = match &result.content {
                    PolarisToolResult::Text(text) => text.clone(),
                    PolarisToolResult::Image(_) => {
                        return Err(GenerationError::UnsupportedContent(
                            "Image tool results are not supported by OpenAI".to_string(),
                        ));
                    }
                };

                let output_text = match result.status {
                    ToolResultStatus::Success => output_text,
                    ToolResultStatus::Error => format!("Error: {output_text}"),
                };

                // OpenAI uses call_id to link function outputs back to function calls.
                let call_id = result.call_id.clone().ok_or_else(|| {
                    GenerationError::InvalidRequest(
                        "Tool result is missing a call_id, which is required by OpenAI to link function outputs back to function calls".to_string(),
                    )
                })?;

                items.push(InputItem::Item(Item::FunctionCallOutput(
                    FunctionCallOutputItemParam {
                        call_id,
                        output: FunctionCallOutput::Text(output_text),
                        id: None,
                        status: None,
                    },
                )));
            }
            UserBlock::Audio(_) => {
                return Err(GenerationError::UnsupportedContent(
                    "Audio content is not yet supported by the OpenAI Responses provider"
                        .to_string(),
                ));
            }
            UserBlock::Document(_) => {
                return Err(GenerationError::UnsupportedContent(
                    "Document content is not yet supported by the OpenAI Responses provider"
                        .to_string(),
                ));
            }
        }
    }

    // Flush any remaining content.
    flush_content_parts(&mut content_parts, Role::User, items);

    Ok(())
}

fn convert_assistant_message(
    blocks: &[AssistantBlock],
    items: &mut Vec<InputItem>,
) -> Result<(), GenerationError> {
    // Text blocks get grouped into a single EasyInputMessage with role assistant.
    // Tool calls and reasoning blocks become individual top-level Item entries.
    let mut text_parts: Vec<InputContent> = Vec::new();

    for block in blocks {
        match block {
            AssistantBlock::Text(block) => {
                text_parts.push(InputContent::InputText(InputTextContent {
                    text: block.text.clone(),
                }));
            }
            AssistantBlock::ToolCall(call) => {
                flush_content_parts(&mut text_parts, Role::Assistant, items);

                let arguments =
                    serde_json::to_string(&call.function.arguments).map_err(|json_err| {
                        GenerationError::InvalidRequest(format!(
                            "Failed to serialize tool call arguments: {json_err}"
                        ))
                    })?;

                items.push(InputItem::Item(Item::FunctionCall(FunctionToolCall {
                    call_id: call.call_id.clone().ok_or_else(|| {
                        GenerationError::InvalidRequest(
                            "Tool call is missing a call_id, which is required by OpenAI to link function calls to their outputs".to_string(),
                        )
                    })?,
                    name: call.function.name.clone(),
                    arguments,
                    id: Some(call.id.clone()),
                    status: None,
                })));
            }
            AssistantBlock::Reasoning(reasoning) => {
                flush_content_parts(&mut text_parts, Role::Assistant, items);

                let summary = reasoning
                    .reasoning
                    .iter()
                    .map(|text| SummaryPart::SummaryText(SummaryTextContent { text: text.clone() }))
                    .collect();

                if reasoning.id.is_none() {
                    tracing::warn!(
                        "Reasoning block is missing an ID; using empty string as fallback"
                    );
                }

                items.push(InputItem::Item(Item::Reasoning(ReasoningItem {
                    id: reasoning.id.clone().unwrap_or_default(),
                    summary,
                    content: None,
                    encrypted_content: None,
                    status: None,
                })));
            }
        }
    }

    flush_content_parts(&mut text_parts, Role::Assistant, items);

    Ok(())
}

/// Flushes accumulated content parts into an [`EasyInputMessage`] and appends
/// it to the items list. Does nothing if `parts` is empty.
fn flush_content_parts(parts: &mut Vec<InputContent>, role: Role, items: &mut Vec<InputItem>) {
    if parts.is_empty() {
        return;
    }

    let content = if parts.len() == 1 {
        // Single text block can use the simpler Text variant.
        if let InputContent::InputText(ref text_content) = parts[0] {
            EasyInputContent::Text(text_content.text.clone())
        } else {
            EasyInputContent::ContentList(core::mem::take(parts))
        }
    } else {
        EasyInputContent::ContentList(core::mem::take(parts))
    };

    items.push(InputItem::EasyMessage(EasyInputMessage {
        content,
        role,
        r#type: Default::default(),
    }));

    parts.clear();
}

fn build_image_data_url(
    image: &polaris_models::llm::ImageBlock,
) -> Result<String, GenerationError> {
    let mime = match image.media_type {
        ImageMediaType::JPEG => "image/jpeg",
        ImageMediaType::PNG => "image/png",
        ImageMediaType::GIF => "image/gif",
        ImageMediaType::WEBP => "image/webp",
        ref other => {
            return Err(GenerationError::UnsupportedContent(format!(
                "Unsupported image media type for OpenAI: {other:?}"
            )));
        }
    };

    let polaris_models::llm::DocumentSource::Base64(data) = &image.data;
    Ok(format!("data:{mime};base64,{data}"))
}

fn convert_tool_choice(choice: &ToolChoice) -> ToolChoiceParam {
    match choice {
        ToolChoice::Auto => ToolChoiceParam::Mode(ToolChoiceOptions::Auto),
        ToolChoice::Required => ToolChoiceParam::Mode(ToolChoiceOptions::Required),
        ToolChoice::None => ToolChoiceParam::Mode(ToolChoiceOptions::None),
        ToolChoice::Specific(name) => {
            ToolChoiceParam::Function(ToolChoiceFunction { name: name.clone() })
        }
    }
}

// ---------------------------------------------------------------------------
// Response conversion (OpenAI -> Polaris)
// ---------------------------------------------------------------------------

fn convert_response(response: Response) -> Result<LlmResponse, GenerationError> {
    let content = response
        .output
        .into_iter()
        .map(convert_output_item)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    let usage = response.usage.map(convert_usage).unwrap_or_default();

    // The Responses API has no top-level finish_reason. Use
    // `incomplete_details.reason` when present, then fall back to
    // content-based inference.
    let finish_reason = if let Some(details) = response.incomplete_details {
        match details.reason.as_str() {
            "max_output_tokens" => StopReason::MaxOutputTokens,
            "content_filter" => StopReason::ContentFilter,
            other => StopReason::Other(other.to_string()),
        }
    } else if content
        .iter()
        .any(|b| matches!(b, AssistantBlock::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };

    Ok(LlmResponse {
        content,
        usage,
        stop_reason: finish_reason,
    })
}

fn convert_output_item(item: OutputItem) -> Result<Vec<AssistantBlock>, GenerationError> {
    match item {
        OutputItem::Message(msg) => msg
            .content
            .into_iter()
            .map(convert_output_message_content)
            .collect::<Result<Vec<_>, _>>(),
        OutputItem::FunctionCall(call) => {
            let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        error = %err,
                        raw_arguments = call.arguments,
                        "Failed to parse tool call arguments as JSON, falling back to Null"
                    );
                    serde_json::Value::Null
                });

            if call.id.is_none() {
                tracing::warn!(
                    call_id = call.call_id,
                    function = call.name,
                    "OpenAI function call is missing an item ID"
                );
            }

            Ok(vec![AssistantBlock::ToolCall(ToolCall {
                id: call.id.unwrap_or_default(),
                call_id: Some(call.call_id),
                function: ToolFunction {
                    name: call.name,
                    arguments,
                },
                signature: None,
                additional_params: None,
            })])
        }
        OutputItem::Reasoning(reasoning) => {
            let texts: Vec<String> = reasoning
                .summary
                .into_iter()
                .map(|part| {
                    let SummaryPart::SummaryText(text_content) = part;
                    text_content.text
                })
                .collect();

            if texts.is_empty() {
                Ok(vec![])
            } else {
                Ok(vec![AssistantBlock::Reasoning(ReasoningBlock {
                    id: Some(reasoning.id),
                    reasoning: texts,
                    signature: None,
                })])
            }
        }
        // Other output item types (file search, web search, computer use, etc.)
        // are not mapped to Polaris types yet.
        other => {
            tracing::warn!(
                item = ?other,
                "Dropping unsupported OpenAI output item type during response conversion"
            );
            Ok(vec![])
        }
    }
}

fn convert_output_message_content(
    content: OutputMessageContent,
) -> Result<AssistantBlock, GenerationError> {
    match content {
        OutputMessageContent::OutputText(text) => {
            Ok(AssistantBlock::Text(TextBlock { text: text.text }))
        }
        OutputMessageContent::Refusal(refusal) => Err(GenerationError::Refusal(refusal.refusal)),
    }
}

fn convert_usage(usage: ResponseUsage) -> Usage {
    Usage {
        input_tokens: Some(u64::from(usage.input_tokens)),
        output_tokens: Some(u64::from(usage.output_tokens)),
        total_tokens: Some(u64::from(usage.total_tokens)),
    }
}

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

fn convert_error(err: OpenAIError) -> GenerationError {
    match err {
        OpenAIError::ApiError(api_err) => GenerationError::Provider {
            status: None,
            message: api_err.message.clone(),
            source: Some(Box::new(OpenAIError::ApiError(api_err))),
        },
        OpenAIError::Reqwest(ref reqwest_err) => {
            if reqwest_err
                .status()
                .is_some_and(|s| s == reqwest::StatusCode::UNAUTHORIZED)
            {
                GenerationError::Auth(err.to_string())
            } else if reqwest_err
                .status()
                .is_some_and(|s| s == reqwest::StatusCode::TOO_MANY_REQUESTS)
            {
                GenerationError::RateLimited { retry_after: None }
            } else {
                GenerationError::Http(err.to_string())
            }
        }
        OpenAIError::JSONDeserialize(serde_err, ref _body) => GenerationError::Json(serde_err),
        OpenAIError::InvalidArgument(msg) => GenerationError::InvalidRequest(msg),
        _ => GenerationError::Provider {
            status: None,
            message: err.to_string(),
            source: Some(Box::new(err)),
        },
    }
}
