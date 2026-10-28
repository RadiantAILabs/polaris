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

/// Bedrock's Converse API cannot currently take a `strict` tool
/// specification safely across models: some models reject the field outright
/// (`ValidationException: tools.N.custom.strict: Extra inputs are not
/// permitted`, observed live on Claude Sonnet 5 via `global.anthropic.*`),
/// and others accept it but overflow their constrained-decoding grammar
/// compiler once more than a handful of tools are marked strict (`"The
/// compiled grammar is too large ... Simplify your tool schemas or reduce
/// the number of strict tools."`, observed live on Claude Haiku 4.5 with
/// this crate's default toolset). Until Bedrock's strict tool support
/// stabilizes, every tool degrades to non-strict here — the same
/// overflow-degrades-to-non-strict mechanism
/// `anthropic::provider::MAX_STRICT_TOOLS` uses for its own (much higher)
/// per-request budget, just with the budget dialed to zero.
const MAX_STRICT_TOOLS: usize = 0;

/// Builds a Bedrock tool configuration from a generation request.
pub fn build_tool_config(
    request: &LlmRequest,
) -> Result<Option<bedrock::ToolConfiguration>, GenerationError> {
    let tools = match &request.tools {
        Some(tools) if !tools.is_empty() => tools,
        _ => return Ok(None),
    };

    let mut strict_budget = MAX_STRICT_TOOLS;
    let tool_specs: Vec<bedrock::Tool> = tools
        .iter()
        .map(|tool| convert_tool_spec(tool, &mut strict_budget))
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

/// Converts a Polaris tool definition to a Bedrock tool specification,
/// degrading to non-strict once `strict_budget` is exhausted (see
/// [`MAX_STRICT_TOOLS`]).
fn convert_tool_spec(
    tool: &polaris_llm::ToolDefinition,
    strict_budget: &mut usize,
) -> Result<bedrock::Tool, GenerationError> {
    let strict = tool.strict && *strict_budget > 0;
    if strict {
        *strict_budget -= 1;
    } else if tool.strict {
        tracing::debug!(
            tool = %tool.name,
            max_strict_tools = MAX_STRICT_TOOLS,
            "strict-tool budget exhausted; tool sent non-strict"
        );
    }

    let input_schema = if strict {
        normalize_schema_for_strict_mode(tool.parameters.clone())
    } else {
        tool.parameters.clone()
    };
    let document = json_to_document(&input_schema);

    let mut builder = bedrock::ToolSpecification::builder()
        .name(&tool.name)
        .description(&tool.description)
        .input_schema(bedrock::ToolInputSchema::Json(document));
    if strict {
        builder = builder.strict(true);
    }

    let spec = builder.build().map_err(|err| {
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

/// Builds the Bedrock inference configuration carrying the effective output
/// token ceiling (the provider's registered `max_tokens`, lowered but never
/// raised by `request.max_output_tokens` — see
/// `BedrockProvider::effective_max_tokens`). Always present, mirroring
/// Anthropic's `max_tokens`, which is a required field on every request.
pub fn build_inference_config(max_tokens: u32) -> bedrock::InferenceConfiguration {
    bedrock::InferenceConfiguration::builder()
        .max_tokens(max_tokens.min(i32::MAX as u32) as i32)
        .build()
}

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

// -----------------------------------------------------------------------------
// Prompt caching
// -----------------------------------------------------------------------------

/// Bedrock's Converse API honors the same cache-checkpoint budget as the
/// underlying Claude model: 4 per request, matching Anthropic's
/// `cache_control` budget (`anthropic::provider::MAX_CACHE_BREAKPOINTS`).
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// The single cache-point marker Bedrock defines. Infallible to build (its
/// only field is `type`, always set).
fn cache_point() -> bedrock::CachePointBlock {
    bedrock::CachePointBlock::builder()
        .r#type(bedrock::CachePointType::Default)
        .build()
        .expect("CachePointBlock requires only `type`, which is always set")
}

/// Applies `cache` to the already-converted `system`/`tool_config`/`messages`
/// by appending `CachePoint` sentinel blocks (everything before a sentinel in
/// its list is cached).
///
/// Mirrors `anthropic::provider::apply_cache_control`'s prefix-then-breakpoints
/// budget policy: the stable prefix (system + tools) is covered by one
/// checkpoint placed after the system block, or after the last tool spec when
/// there is no system prompt; each requested message breakpoint places a
/// checkpoint after that message's content. Markers are budgeted to
/// [`MAX_CACHE_BREAKPOINTS`]; extras are dropped in the order applied (prefix
/// first, then message breakpoints in the order given). The mechanism differs
/// from Anthropic's (a sentinel block appended after the cached region,
/// rather than a marker on the last existing block within it), but the
/// budgeting policy is identical.
pub fn apply_cache_control(
    cache: &polaris_llm::CacheControl,
    system: &mut Option<Vec<bedrock::SystemContentBlock>>,
    tool_config: Option<&mut bedrock::ToolConfiguration>,
    messages: &mut [bedrock::Message],
) {
    if cache.is_disabled() {
        return;
    }

    let mut budget = MAX_CACHE_BREAKPOINTS;

    match (cache.prefix, system.as_mut()) {
        (true, Some(blocks)) if budget > 0 => {
            blocks.push(bedrock::SystemContentBlock::CachePoint(cache_point()));
            budget -= 1;
        }
        (true, None) if budget > 0 => {
            if let Some(config) = tool_config {
                config.tools.push(bedrock::Tool::CachePoint(cache_point()));
                budget -= 1;
            }
        }
        _ => {}
    }

    for &idx in &cache.breakpoints {
        if budget == 0 {
            break;
        }
        let Some(message) = messages.get_mut(idx) else {
            // A stale breakpoint (index past the message list): silently
            // producing no marker in release degrades the cache hit rate
            // with no signal. Trip the assertion in debug to fail fast in
            // tests, and warn in release so the misconfiguration is
            // observable rather than invisible — mirrors the Anthropic
            // provider's handling of the same condition.
            debug_assert!(
                idx < messages.len(),
                "cache breakpoint index {idx} out of range for {} messages; \
                 a context strategy produced a stale breakpoint",
                messages.len()
            );
            tracing::warn!(
                breakpoint = idx,
                message_count = messages.len(),
                "dropping cache breakpoint with no markable message; \
                 a context strategy produced a stale breakpoint"
            );
            continue;
        };
        message
            .content
            .push(bedrock::ContentBlock::CachePoint(cache_point()));
        budget -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_llm::{CacheControl, Message, ToolDefinition};

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            format!("{name} tool"),
            serde_json::json!({"type": "object", "properties": {}}),
        )
    }

    fn is_cache_point_system(block: &bedrock::SystemContentBlock) -> bool {
        matches!(block, bedrock::SystemContentBlock::CachePoint(_))
    }

    fn is_cache_point_tool(tool: &bedrock::Tool) -> bool {
        matches!(tool, bedrock::Tool::CachePoint(_))
    }

    fn is_cache_point_content(block: &bedrock::ContentBlock) -> bool {
        matches!(block, bedrock::ContentBlock::CachePoint(_))
    }

    #[test]
    fn output_token_limit_builds_bedrock_inference_config() {
        let config = build_inference_config(512);
        assert_eq!(config.max_tokens(), Some(512));
    }

    #[test]
    fn output_token_limit_respects_bedrocks_i32_ceiling() {
        let config = build_inference_config(u32::MAX);
        assert_eq!(config.max_tokens(), Some(i32::MAX));
    }

    #[test]
    fn tool_config_never_marks_a_tool_strict() {
        // Bedrock's Converse API cannot reliably accept `strict` tool specs
        // today (see the doc comment on `MAX_STRICT_TOOLS`), so even a
        // caller-requested strict tool must degrade to non-strict.
        assert!(
            tool_def("search").strict,
            "test fixture should default to strict, matching ToolDefinition::default"
        );

        let tool_config = build_tool_config(&LlmRequest {
            tools: Some(vec![tool_def("search"), tool_def("fetch")]),
            ..Default::default()
        })
        .unwrap()
        .unwrap();

        assert_eq!(
            tool_config.tools.len(),
            2,
            "both strict fixtures must reach the Bedrock tool configuration"
        );
        for tool in &tool_config.tools {
            let bedrock::Tool::ToolSpec(spec) = tool else {
                continue;
            };
            assert_eq!(
                spec.strict(),
                None,
                "Bedrock tool specs must never set strict: true"
            );
        }
    }

    #[test]
    fn cache_prefix_appends_a_checkpoint_after_the_system_block() {
        let mut system = Some(vec![bedrock::SystemContentBlock::Text("sys".to_string())]);
        let mut tool_config = build_tool_config(&LlmRequest {
            tools: Some(vec![tool_def("search")]),
            ..Default::default()
        })
        .unwrap();
        let mut messages: Vec<bedrock::Message> = vec![];

        apply_cache_control(
            &CacheControl::prefix(),
            &mut system,
            tool_config.as_mut(),
            &mut messages,
        );

        let system = system.unwrap();
        assert_eq!(
            system.len(),
            2,
            "a checkpoint should be appended: {system:?}"
        );
        assert!(is_cache_point_system(&system[1]));
        // One checkpoint covers tools + system, so the tool list is untouched.
        assert!(
            !tool_config.unwrap().tools.iter().any(is_cache_point_tool),
            "tools should not also get a checkpoint when the system prompt already carries one"
        );
    }

    #[test]
    fn cache_prefix_without_system_appends_a_checkpoint_after_the_last_tool() {
        let mut system = None;
        let mut tool_config = build_tool_config(&LlmRequest {
            tools: Some(vec![tool_def("a"), tool_def("b")]),
            ..Default::default()
        })
        .unwrap();
        let mut messages: Vec<bedrock::Message> = vec![];

        apply_cache_control(
            &CacheControl::prefix(),
            &mut system,
            tool_config.as_mut(),
            &mut messages,
        );

        assert!(system.is_none());
        let tools = tool_config.unwrap().tools;
        assert_eq!(tools.len(), 3, "a checkpoint should be appended: {tools:?}");
        assert!(is_cache_point_tool(&tools[2]));
    }

    #[test]
    fn cache_prefix_without_system_or_tools_is_a_noop() {
        let mut system = None;
        let mut messages: Vec<bedrock::Message> = vec![];
        apply_cache_control(&CacheControl::prefix(), &mut system, None, &mut messages);
        assert!(system.is_none());
        assert!(messages.is_empty());
    }

    #[test]
    fn cache_breakpoints_append_checkpoints_to_message_content() {
        let mut system = None;
        let mut messages: Vec<bedrock::Message> = vec![
            convert_message(&Message::user("first")).unwrap(),
            convert_message(&Message::assistant("second")).unwrap(),
            convert_message(&Message::user("third")).unwrap(),
        ];

        apply_cache_control(
            &CacheControl::default().with_breakpoints([0, 2]),
            &mut system,
            None,
            &mut messages,
        );

        assert!(messages[0].content.iter().any(is_cache_point_content));
        assert!(!messages[1].content.iter().any(is_cache_point_content));
        assert!(messages[2].content.iter().any(is_cache_point_content));
    }

    #[test]
    fn cache_markers_are_budget_capped_at_four() {
        let mut system = Some(vec![bedrock::SystemContentBlock::Text("sys".to_string())]);
        let mut messages: Vec<bedrock::Message> = (0..5)
            .map(|i| convert_message(&Message::user(format!("m{i}"))).unwrap())
            .collect();

        apply_cache_control(
            &CacheControl::prefix().with_breakpoints([0, 1, 2, 3, 4]),
            &mut system,
            None,
            &mut messages,
        );

        // Prefix takes one marker; the remaining three go to the lowest
        // breakpoint indices in the order given (0, 1, 2); 3 & 4 are dropped.
        let marked: Vec<usize> = (0..5)
            .filter(|&i| messages[i].content.iter().any(is_cache_point_content))
            .collect();
        assert_eq!(marked, vec![0, 1, 2]);
    }

    #[test]
    fn no_cache_control_appends_no_checkpoints() {
        let mut system = Some(vec![bedrock::SystemContentBlock::Text("sys".to_string())]);
        let mut messages = vec![convert_message(&Message::user("hi")).unwrap()];
        apply_cache_control(&CacheControl::default(), &mut system, None, &mut messages);
        assert_eq!(system.unwrap().len(), 1);
        assert_eq!(messages[0].content.len(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "out of range")]
    fn out_of_range_breakpoint_trips_debug_assert() {
        let mut system = None;
        let mut messages = vec![convert_message(&Message::user("only one")).unwrap()];
        apply_cache_control(
            &CacheControl::default().with_breakpoints([5]),
            &mut system,
            None,
            &mut messages,
        );
    }
}
