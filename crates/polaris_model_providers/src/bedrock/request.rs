//! Polaris to Bedrock request conversions.

use super::types::{
    convert_document_format, convert_image_format, convert_tool_result_status,
    decode_base64_source, json_to_document,
};
use crate::schema::normalize_schema_for_strict_mode;
use aws_sdk_bedrockruntime::types as bedrock;
use polaris_models::llm::{self as polaris_llm, GenerationError, LlmRequest};

// -----------------------------------------------------------------------------
// Message conversions
// -----------------------------------------------------------------------------

/// Converts a Polaris message to a Bedrock message.
pub fn convert_message(
    message: &polaris_llm::Message,
) -> Result<bedrock::Message, GenerationError> {
    let (role, blocks) = match message {
        polaris_llm::Message::User { content } => {
            let blocks = content
                .iter()
                .map(convert_user_block)
                .collect::<Result<Vec<_>, _>>()?;
            (bedrock::ConversationRole::User, blocks)
        }
        polaris_llm::Message::Assistant { content, .. } => {
            let blocks = content
                .iter()
                .map(convert_assistant_block)
                .collect::<Result<Vec<_>, _>>()?;
            (bedrock::ConversationRole::Assistant, blocks)
        }
    };

    bedrock::Message::builder()
        .role(role)
        .set_content(Some(blocks))
        .build()
        .map_err(|err| GenerationError::InvalidRequest(format!("failed to build message: {err}")))
}

/// Converts a Polaris user block to a Bedrock content block.
fn convert_user_block(
    block: &polaris_llm::UserBlock,
) -> Result<bedrock::ContentBlock, GenerationError> {
    match block {
        polaris_llm::UserBlock::Text(block) => Ok(bedrock::ContentBlock::Text(block.text.clone())),
        polaris_llm::UserBlock::Image(image) => {
            Ok(bedrock::ContentBlock::Image(convert_image_to_block(image)?))
        }
        polaris_llm::UserBlock::Audio(_) => Err(GenerationError::UnsupportedContent(
            "audio content is not yet implemented for the Bedrock Converse API".to_string(),
        )),
        polaris_llm::UserBlock::Document(doc) => Ok(bedrock::ContentBlock::Document(
            convert_document_to_block(doc)?,
        )),
        polaris_llm::UserBlock::ToolResult(result) => Ok(bedrock::ContentBlock::ToolResult(
            convert_tool_result(result)?,
        )),
    }
}

/// Converts a Polaris assistant block to a Bedrock content block.
fn convert_assistant_block(
    block: &polaris_llm::AssistantBlock,
) -> Result<bedrock::ContentBlock, GenerationError> {
    match block {
        polaris_llm::AssistantBlock::Text(block) => {
            Ok(bedrock::ContentBlock::Text(block.text.clone()))
        }
        polaris_llm::AssistantBlock::ToolCall(call) => {
            Ok(bedrock::ContentBlock::ToolUse(convert_tool_call(call)?))
        }
        polaris_llm::AssistantBlock::Reasoning(block) => {
            let text = block.reasoning.join("\n");
            let reasoning_text = bedrock::ReasoningTextBlock::builder()
                .text(text)
                .set_signature(block.signature.clone())
                .build()
                .map_err(|err| {
                    GenerationError::InvalidRequest(format!(
                        "failed to build reasoning block: {err}"
                    ))
                })?;
            Ok(bedrock::ContentBlock::ReasoningContent(
                bedrock::ReasoningContentBlock::ReasoningText(reasoning_text),
            ))
        }
    }
}

// -----------------------------------------------------------------------------
// Content block conversions
// -----------------------------------------------------------------------------

/// Converts a Polaris image to a Bedrock image block.
fn convert_image_to_block(
    image: &polaris_llm::ImageBlock,
) -> Result<bedrock::ImageBlock, GenerationError> {
    let format = convert_image_format(&image.media_type)?;
    let bytes = decode_base64_source(&image.data)?;

    bedrock::ImageBlock::builder()
        .format(format)
        .source(bedrock::ImageSource::Bytes(aws_smithy_types::Blob::new(
            bytes,
        )))
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build image block: {err}"))
        })
}

/// Converts a Polaris document to a Bedrock document block.
fn convert_document_to_block(
    doc: &polaris_llm::DocumentBlock,
) -> Result<bedrock::DocumentBlock, GenerationError> {
    let format = convert_document_format(&doc.media_type);
    let bytes = decode_base64_source(&doc.data)?;

    bedrock::DocumentBlock::builder()
        .format(format)
        .name(doc.name.clone())
        .source(bedrock::DocumentSource::Bytes(aws_smithy_types::Blob::new(
            bytes,
        )))
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build document block: {err}"))
        })
}

/// Converts a Polaris tool result to a Bedrock tool result block.
fn convert_tool_result(
    result: &polaris_llm::ToolResult,
) -> Result<bedrock::ToolResultBlock, GenerationError> {
    let content_blocks = match &result.content {
        polaris_llm::ToolResultContent::Text(text) => {
            vec![bedrock::ToolResultContentBlock::Text(text.clone())]
        }
        polaris_llm::ToolResultContent::Image(image) => {
            vec![bedrock::ToolResultContentBlock::Image(
                convert_image_to_block(image)?,
            )]
        }
    };

    let status = convert_tool_result_status(&result.status);

    bedrock::ToolResultBlock::builder()
        .tool_use_id(&result.id)
        .set_content(Some(content_blocks))
        .status(status)
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build tool result block: {err}"))
        })
}

/// Converts a Polaris tool call to a Bedrock tool use block.
fn convert_tool_call(
    call: &polaris_llm::ToolCall,
) -> Result<bedrock::ToolUseBlock, GenerationError> {
    let input = json_to_document(&call.function.arguments);

    bedrock::ToolUseBlock::builder()
        .tool_use_id(&call.id)
        .name(&call.function.name)
        .input(input)
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build tool use block: {err}"))
        })
}

// -----------------------------------------------------------------------------
// Tool configuration
// -----------------------------------------------------------------------------

/// Builds a Bedrock tool configuration from a generation request.
pub fn build_tool_config(
    request: &LlmRequest,
) -> Result<Option<bedrock::ToolConfiguration>, GenerationError> {
    let tools = match &request.tools {
        Some(tools) if !tools.is_empty() => tools,
        _ => return Ok(None),
    };

    let tool_specs: Vec<bedrock::Tool> = tools
        .iter()
        .map(convert_tool_spec)
        .collect::<Result<Vec<_>, _>>()?;

    let mut config_builder = bedrock::ToolConfiguration::builder().set_tools(Some(tool_specs));

    if let Some(choice) = &request.tool_choice
        && let Some(bedrock_choice) = convert_tool_choice(choice)?
    {
        config_builder = config_builder.tool_choice(bedrock_choice);
    }

    config_builder.build().map(Some).map_err(|err| {
        GenerationError::InvalidRequest(format!("failed to build tool configuration: {err}"))
    })
}

/// Converts a Polaris tool definition to a Bedrock tool specification.
fn convert_tool_spec(tool: &polaris_llm::ToolDefinition) -> Result<bedrock::Tool, GenerationError> {
    let normalized = normalize_schema_for_strict_mode(tool.parameters.clone());
    let input_schema = json_to_document(&normalized);

    let spec = bedrock::ToolSpecification::builder()
        .name(&tool.name)
        .description(&tool.description)
        .input_schema(bedrock::ToolInputSchema::Json(input_schema))
        .strict(true)
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build tool specification: {err}"))
        })?;

    Ok(bedrock::Tool::ToolSpec(spec))
}

/// Converts a Polaris tool choice to a Bedrock tool choice.
///
/// Returns `Ok(None)` for [`ToolChoice::None`](polaris_llm::ToolChoice::None)
/// because Bedrock has no equivalent — the caller should omit `tool_choice`
/// from the configuration, letting the model default to auto.
fn convert_tool_choice(
    choice: &polaris_llm::ToolChoice,
) -> Result<Option<bedrock::ToolChoice>, GenerationError> {
    match choice {
        polaris_llm::ToolChoice::Auto => Ok(Some(bedrock::ToolChoice::Auto(
            bedrock::AutoToolChoice::builder().build(),
        ))),
        polaris_llm::ToolChoice::Required => Ok(Some(bedrock::ToolChoice::Any(
            bedrock::AnyToolChoice::builder().build(),
        ))),
        // Only supported by Anthropic Claude 3 and Amazon Nova models.
        polaris_llm::ToolChoice::Specific(name) => {
            let specific = bedrock::SpecificToolChoice::builder()
                .name(name)
                .build()
                .map_err(|err| {
                    GenerationError::InvalidRequest(format!(
                        "failed to build specific tool choice: {err}"
                    ))
                })?;
            Ok(Some(bedrock::ToolChoice::Tool(specific)))
        }
        polaris_llm::ToolChoice::None => Ok(None),
    }
}

// -----------------------------------------------------------------------------
// Output configuration
// -----------------------------------------------------------------------------

/// Builds a Bedrock output configuration from a generation request.
pub fn build_output_config(
    request: &LlmRequest,
) -> Result<Option<bedrock::OutputConfig>, GenerationError> {
    let Some(schema) = &request.output_schema else {
        return Ok(None);
    };

    // Normalize the schema to comply with Bedrock's strict mode requirements
    let normalized_schema = normalize_schema_for_strict_mode(schema.clone());

    let schema_string = serde_json::to_string(&normalized_schema).map_err(|err| {
        GenerationError::InvalidRequest(format!("failed to serialize output schema: {err}"))
    })?;

    let json_schema_def = bedrock::JsonSchemaDefinition::builder()
        .schema(schema_string)
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!(
                "failed to build JSON schema definition: {err}"
            ))
        })?;

    let output_format = bedrock::OutputFormat::builder()
        .r#type(bedrock::OutputFormatType::JsonSchema)
        .structure(bedrock::OutputFormatStructure::JsonSchema(json_schema_def))
        .build()
        .map_err(|err| {
            GenerationError::InvalidRequest(format!("failed to build output format: {err}"))
        })?;

    let output_config = bedrock::OutputConfig::builder()
        .text_format(output_format)
        .build();

    Ok(Some(output_config))
}
