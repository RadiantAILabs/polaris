//! `OpenAI` [`LlmProvider`] implementation using the Responses API.

use crate::schema::normalize_schema_for_strict_mode;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::{
    CreateResponseArgs, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputContent, InputImageContent,
    InputItem, InputParam, InputTextContent, Item, OutputContent, OutputItem, OutputMessageContent,
    ReasoningItem, Response, ResponseFormatJsonSchema, ResponseStreamEvent, ResponseTextParam,
    ResponseUsage, Role, SummaryPart, SummaryTextContent, TextResponseFormatConfiguration, Tool,
    ToolChoiceFunction, ToolChoiceOptions, ToolChoiceParam,
};
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, ImageMediaType,
    LlmProvider, LlmRequest, LlmResponse, LlmStream, Message, ReasoningBlock, StopReason,
    StreamEvent, TextBlock, ToolCall, ToolChoice, ToolFunction,
    ToolResultContent as PolarisToolResult, ToolResultStatus, Usage, UserBlock,
};
use std::collections::{HashMap, VecDeque};

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

    /// Streams a response from the `OpenAI` Responses API.
    ///
    /// Uses `async_openai`'s built-in streaming support which handles SSE parsing
    /// and deserialization. An [`OpenAiStreamAdapter`] converts the library's
    /// [`ResponseStreamEvent`]s into Polaris [`StreamEvent`]s.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError::Provider`] if the API returns an error event
    /// or a non-2xx status code.
    /// Returns [`GenerationError::Refusal`] if the model refuses to respond.
    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let create_response = convert_request(model, &request)?;
        let raw_stream = self
            .client
            .responses()
            .create_stream(create_response)
            .await
            .map_err(convert_error)?;

        Ok(Box::pin(OpenAiStreamAdapter::new(raw_stream)))
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
// Stream Adapter
// ---------------------------------------------------------------------------

/// Sentinel `content_index` used for output items that carry no content index
/// (i.e. function calls). These are tracked by `output_index` alone.
const FUNCTION_CALL_CONTENT_IDX: u32 = u32::MAX;

/// Converts a stream of `async_openai` [`ResponseStreamEvent`]s into Polaris
/// [`StreamEvent`]s.
///
/// The `OpenAI` Responses API uses a hierarchical event model with output items
/// (messages, function calls, reasoning) containing content parts. This adapter
/// flattens that hierarchy into Polaris's sequential content block model,
/// assigning monotonically increasing block indices.
///
/// Usage is captured from the terminal `response.completed` or
/// `response.incomplete` events and emitted as [`StreamEvent::MessageDelta`]
/// followed by [`StreamEvent::MessageStop`].
struct OpenAiStreamAdapter<S> {
    inner: S,
    /// Next block index to assign.
    next_block_index: u32,
    /// Maps `(output_index, content_index)` to the assigned Polaris block index.
    /// Content-less output items (function calls) use [`FUNCTION_CALL_CONTENT_IDX`].
    block_indices: HashMap<(u32, u32), u32>,
    /// Events buffered for delivery (e.g. `MessageDelta` + `MessageStop` from a
    /// single `response.completed`).
    pending: VecDeque<Result<StreamEvent, GenerationError>>,
    /// Accumulated refusal text.
    refusal: Option<String>,
}

impl<S> OpenAiStreamAdapter<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            next_block_index: 0,
            block_indices: HashMap::new(),
            pending: VecDeque::new(),
            refusal: None,
        }
    }

    /// Assigns the next block index for the given `(output_index, content_index)`.
    fn assign_block_index(&mut self, output_index: u32, content_index: u32) -> u32 {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.block_indices
            .insert((output_index, content_index), idx);
        idx
    }

    /// Looks up a previously assigned block index.
    fn get_block_index(&self, output_index: u32, content_index: u32) -> Option<u32> {
        self.block_indices
            .get(&(output_index, content_index))
            .copied()
    }

    /// Converts a single raw event, potentially pushing multiple results into
    /// `self.pending`. Returns `true` if at least one event was produced.
    fn convert_event(&mut self, raw: ResponseStreamEvent) -> bool {
        match raw {
            // ── Text content ────────────────────────────────────────────
            ResponseStreamEvent::ResponseContentPartAdded(event) => {
                let block = match event.part {
                    OutputContent::OutputText(_) => ContentBlockStartData::Text,
                    // Refusals are accumulated via RefusalDelta/RefusalDone and
                    // emitted as GenerationError::Refusal. Skipping here means no
                    // block_indices entry is created, so any subsequent
                    // ResponseOutputTextDelta on this content_index will silently
                    // no-op (get_block_index returns None).
                    OutputContent::Refusal(_) => return false,
                    OutputContent::ReasoningText(_) => ContentBlockStartData::Reasoning,
                };
                let index = self.assign_block_index(event.output_index, event.content_index);
                self.pending
                    .push_back(Ok(StreamEvent::ContentBlockStart { index, block }));
                true
            }
            ResponseStreamEvent::ResponseOutputTextDelta(event) => {
                if let Some(index) = self.get_block_index(event.output_index, event.content_index) {
                    self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::Text(event.delta),
                    }));
                    true
                } else {
                    false
                }
            }
            ResponseStreamEvent::ResponseContentPartDone(event) => {
                if let Some(index) = self.get_block_index(event.output_index, event.content_index) {
                    self.pending
                        .push_back(Ok(StreamEvent::ContentBlockStop { index }));
                    true
                } else {
                    false
                }
            }

            // ── Function calls ──────────────────────────────────────────
            ResponseStreamEvent::ResponseOutputItemAdded(event) => {
                match event.item {
                    OutputItem::FunctionCall(ref call) => {
                        let index =
                            self.assign_block_index(event.output_index, FUNCTION_CALL_CONTENT_IDX);
                        self.pending.push_back(Ok(StreamEvent::ContentBlockStart {
                            index,
                            block: ContentBlockStartData::ToolCall {
                                id: call.id.clone().unwrap_or_default(),
                                call_id: Some(call.call_id.clone()),
                                name: call.name.clone(),
                            },
                        }));
                        true
                    }
                    // Reasoning summary parts handle their own block starts via
                    // `ResponseReasoningSummaryPartAdded`, so we skip the item-level
                    // event. All other output items (file search, web search, etc.)
                    // are also not mapped.
                    _ => false,
                }
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => {
                if let Some(index) =
                    self.get_block_index(event.output_index, FUNCTION_CALL_CONTENT_IDX)
                {
                    self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::ToolCall {
                            arguments: event.delta,
                        },
                    }));
                    true
                } else {
                    false
                }
            }
            ResponseStreamEvent::ResponseOutputItemDone(event) => {
                // Emit ContentBlockStop for function calls (which use FUNCTION_CALL_CONTENT_IDX).
                if matches!(event.item, OutputItem::FunctionCall(_))
                    && let Some(index) =
                        self.get_block_index(event.output_index, FUNCTION_CALL_CONTENT_IDX)
                {
                    self.pending
                        .push_back(Ok(StreamEvent::ContentBlockStop { index }));
                    return true;
                }
                false
            }

            // ── Reasoning (summary) ─────────────────────────────────────
            ResponseStreamEvent::ResponseReasoningSummaryPartAdded(event) => {
                let index = self.assign_block_index(event.output_index, event.summary_index);
                self.pending.push_back(Ok(StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockStartData::Reasoning,
                }));
                true
            }
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => {
                if let Some(index) = self.get_block_index(event.output_index, event.summary_index) {
                    self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::Reasoning(event.delta),
                    }));
                    true
                } else {
                    false
                }
            }
            ResponseStreamEvent::ResponseReasoningSummaryPartDone(event) => {
                if let Some(index) = self.get_block_index(event.output_index, event.summary_index) {
                    self.pending
                        .push_back(Ok(StreamEvent::ContentBlockStop { index }));
                    true
                } else {
                    false
                }
            }

            // ── Refusal ─────────────────────────────────────────────────
            ResponseStreamEvent::ResponseRefusalDelta(event) => {
                self.refusal
                    .get_or_insert_with(String::new)
                    .push_str(&event.delta);
                false
            }
            ResponseStreamEvent::ResponseRefusalDone(_) => {
                if let Some(refusal) = self.refusal.take() {
                    self.pending
                        .push_back(Err(GenerationError::Refusal(refusal)));
                    true
                } else {
                    false
                }
            }

            // ── Terminal events ──────────────────────────────────────────
            ResponseStreamEvent::ResponseCompleted(event) => {
                let stop_reason = infer_stop_reason(&event.response);
                let usage = event.response.usage.map(convert_usage).unwrap_or_default();

                // OpenAI reports usage only once, at completion — there is no
                // intermediate usage to report. `MessageDelta` is emitted for
                // protocol consistency; consumers that want the final value
                // should use `MessageStop`.
                self.pending.push_back(Ok(StreamEvent::MessageDelta {
                    usage: usage.clone(),
                }));
                self.pending
                    .push_back(Ok(StreamEvent::MessageStop { stop_reason, usage }));
                true
            }
            ResponseStreamEvent::ResponseIncomplete(event) => {
                let usage = event.response.usage.map(convert_usage).unwrap_or_default();
                let stop_reason = event
                    .response
                    .incomplete_details
                    .as_ref()
                    .map(|d| match d.reason.as_str() {
                        "max_output_tokens" => StopReason::MaxOutputTokens,
                        "content_filter" => StopReason::ContentFilter,
                        other => StopReason::Other(other.to_string()),
                    })
                    .unwrap_or(StopReason::MaxOutputTokens);

                self.pending.push_back(Ok(StreamEvent::MessageDelta {
                    usage: usage.clone(),
                }));
                self.pending
                    .push_back(Ok(StreamEvent::MessageStop { stop_reason, usage }));
                true
            }
            ResponseStreamEvent::ResponseFailed(event) => {
                let message = event
                    .response
                    .error
                    .map(|err| format!("{}: {}", err.code, err.message))
                    .unwrap_or_else(|| "Response failed".to_string());
                self.pending.push_back(Err(GenerationError::Provider {
                    status: None,
                    message,
                    source: None,
                }));
                true
            }
            ResponseStreamEvent::ResponseError(event) => {
                self.pending.push_back(Err(GenerationError::Provider {
                    status: None,
                    message: event.message,
                    source: None,
                }));
                true
            }

            // ── Filtered events ─────────────────────────────────────────
            // Created, in-progress, queued, text done, arguments done, reasoning
            // text deltas (raw, not summary), file/web/image/code/MCP search
            // events, annotations, and custom tool calls are not mapped.
            _ => false,
        }
    }
}

/// Infers the [`StopReason`] from a completed [`Response`].
///
/// The `incomplete_details` guard is a defensive fallback; in practice this
/// function is only called for `ResponseCompleted` where `incomplete_details`
/// should always be `None`. `ResponseIncomplete` uses its own dedicated handler.
fn infer_stop_reason(response: &Response) -> StopReason {
    if let Some(details) = &response.incomplete_details {
        return match details.reason.as_str() {
            "max_output_tokens" => StopReason::MaxOutputTokens,
            "content_filter" => StopReason::ContentFilter,
            other => StopReason::Other(other.to_string()),
        };
    }
    if response
        .output
        .iter()
        .any(|item| matches!(item, OutputItem::FunctionCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    }
}

impl<S> Stream for OpenAiStreamAdapter<S>
where
    S: Stream<Item = Result<ResponseStreamEvent, OpenAIError>> + Unpin,
{
    type Item = Result<StreamEvent, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Drain any buffered events first.
        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(raw))) => {
                    if this.convert_event(raw) {
                        return Poll::Ready(this.pending.pop_front());
                    }
                    // Event was filtered — poll again.
                }
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Some(Err(convert_error(err))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses::{
        AssistantRole, ErrorObject, OutputContent, OutputMessage, OutputStatus, OutputTextContent,
        ResponseCompletedEvent, ResponseContentPartAddedEvent, ResponseContentPartDoneEvent,
        ResponseCreatedEvent, ResponseErrorEvent, ResponseFailedEvent,
        ResponseFunctionCallArgumentsDeltaEvent, ResponseFunctionCallArgumentsDoneEvent,
        ResponseInProgressEvent, ResponseIncompleteEvent, ResponseOutputItemAddedEvent,
        ResponseOutputItemDoneEvent, ResponseReasoningSummaryPartAddedEvent,
        ResponseReasoningSummaryPartDoneEvent, ResponseReasoningSummaryTextDeltaEvent,
        ResponseRefusalDeltaEvent, ResponseRefusalDoneEvent, ResponseTextDeltaEvent,
        ResponseTextDoneEvent, ResponseUsage, Status,
    };

    /// Builds a minimal [`Response`] for use in terminal events.
    fn stub_response(status: Status, usage: Option<ResponseUsage>) -> Response {
        // Response has many fields; we deserialize from minimal JSON.
        let usage_json = match usage {
            Some(u) => serde_json::to_value(u).unwrap(),
            None => serde_json::Value::Null,
        };
        serde_json::from_value(serde_json::json!({
            "id": "resp_test",
            "created_at": 0,
            "model": "gpt-4o",
            "object": "response",
            "output": [],
            "parallel_tool_calls": true,
            "status": status,
            "usage": usage_json,
        }))
        .unwrap()
    }

    fn stub_usage() -> ResponseUsage {
        serde_json::from_value(serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens_details": { "reasoning_tokens": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn converts_text_stream() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let events: Vec<ResponseStreamEvent> = vec![
            ResponseStreamEvent::ResponseCreated(ResponseCreatedEvent {
                sequence_number: 0,
                response: stub_response(Status::InProgress, None),
            }),
            ResponseStreamEvent::ResponseInProgress(ResponseInProgressEvent {
                sequence_number: 1,
                response: stub_response(Status::InProgress, None),
            }),
            ResponseStreamEvent::ResponseOutputItemAdded(ResponseOutputItemAddedEvent {
                sequence_number: 2,
                output_index: 0,
                item: OutputItem::Message(OutputMessage {
                    id: "msg_1".to_string(),
                    content: vec![],
                    role: AssistantRole::Assistant,
                    status: OutputStatus::InProgress,
                }),
            }),
            ResponseStreamEvent::ResponseContentPartAdded(ResponseContentPartAddedEvent {
                sequence_number: 3,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                part: OutputContent::OutputText(OutputTextContent {
                    text: String::new(),
                    annotations: vec![],
                    logprobs: None,
                }),
            }),
            ResponseStreamEvent::ResponseOutputTextDelta(ResponseTextDeltaEvent {
                sequence_number: 4,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                delta: "Hello".to_string(),
                logprobs: None,
            }),
            ResponseStreamEvent::ResponseOutputTextDelta(ResponseTextDeltaEvent {
                sequence_number: 5,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                delta: " world".to_string(),
                logprobs: None,
            }),
            ResponseStreamEvent::ResponseOutputTextDone(ResponseTextDoneEvent {
                sequence_number: 6,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                text: "Hello world".to_string(),
                logprobs: None,
            }),
            ResponseStreamEvent::ResponseContentPartDone(ResponseContentPartDoneEvent {
                sequence_number: 7,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                part: OutputContent::OutputText(OutputTextContent {
                    text: "Hello world".to_string(),
                    annotations: vec![],
                    logprobs: None,
                }),
            }),
            ResponseStreamEvent::ResponseCompleted(ResponseCompletedEvent {
                sequence_number: 8,
                response: stub_response(Status::Completed, Some(stub_usage())),
            }),
        ];

        let converted: Vec<_> = events
            .into_iter()
            .flat_map(|raw| {
                adapter.convert_event(raw);
                adapter.pending.drain(..).collect::<Vec<_>>()
            })
            .collect();

        // Expected: ContentBlockStart, 2x delta, ContentBlockStop, MessageDelta, MessageStop
        assert_eq!(converted.len(), 6);

        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text
            }
        ));
        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == "Hello"
        ));
        assert!(matches!(
            converted[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == " world"
        ));
        assert!(matches!(
            converted[3].as_ref().unwrap(),
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        assert!(matches!(
            converted[4].as_ref().unwrap(),
            StreamEvent::MessageDelta { usage } if usage.input_tokens == Some(10)
        ));
        assert!(matches!(
            converted[5].as_ref().unwrap(),
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage
            } if usage.output_tokens == Some(5)
        ));
    }

    #[test]
    fn converts_tool_call_stream() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let events: Vec<ResponseStreamEvent> = vec![
            ResponseStreamEvent::ResponseOutputItemAdded(ResponseOutputItemAddedEvent {
                sequence_number: 0,
                output_index: 0,
                item: OutputItem::FunctionCall(FunctionToolCall {
                    arguments: String::new(),
                    call_id: "call_abc123".to_string(),
                    name: "get_weather".to_string(),
                    id: Some("fc_1".to_string()),
                    status: None,
                }),
            }),
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
                ResponseFunctionCallArgumentsDeltaEvent {
                    sequence_number: 1,
                    item_id: "fc_1".to_string(),
                    output_index: 0,
                    delta: r#"{"city""#.to_string(),
                },
            ),
            ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
                ResponseFunctionCallArgumentsDeltaEvent {
                    sequence_number: 2,
                    item_id: "fc_1".to_string(),
                    output_index: 0,
                    delta: r#":"London"}"#.to_string(),
                },
            ),
            ResponseStreamEvent::ResponseFunctionCallArgumentsDone(
                ResponseFunctionCallArgumentsDoneEvent {
                    name: Some("get_weather".to_string()),
                    sequence_number: 3,
                    item_id: "fc_1".to_string(),
                    output_index: 0,
                    arguments: r#"{"city":"London"}"#.to_string(),
                },
            ),
            ResponseStreamEvent::ResponseOutputItemDone(ResponseOutputItemDoneEvent {
                sequence_number: 4,
                output_index: 0,
                item: OutputItem::FunctionCall(FunctionToolCall {
                    arguments: r#"{"city":"London"}"#.to_string(),
                    call_id: "call_abc123".to_string(),
                    name: "get_weather".to_string(),
                    id: Some("fc_1".to_string()),
                    status: None,
                }),
            }),
        ];

        let converted: Vec<_> = events
            .into_iter()
            .flat_map(|raw| {
                adapter.convert_event(raw);
                adapter.pending.drain(..).collect::<Vec<_>>()
            })
            .collect();

        // Expected: ContentBlockStart, 2x delta, ContentBlockStop
        assert_eq!(converted.len(), 4);

        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall { id, call_id, name }
            } if id == "fc_1" && call_id.as_deref() == Some("call_abc123") && name == "get_weather"
        ));
        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall { arguments }
            } if arguments == r#"{"city""#
        ));
        assert!(matches!(
            converted[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall { arguments }
            } if arguments == r#":"London"}"#
        ));
        assert!(matches!(
            converted[3].as_ref().unwrap(),
            StreamEvent::ContentBlockStop { index: 0 }
        ));
    }

    #[test]
    fn converts_reasoning_stream() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let events: Vec<ResponseStreamEvent> = vec![
            ResponseStreamEvent::ResponseReasoningSummaryPartAdded(
                ResponseReasoningSummaryPartAddedEvent {
                    sequence_number: 0,
                    item_id: "rs_1".to_string(),
                    output_index: 0,
                    summary_index: 0,
                    part: SummaryPart::SummaryText(SummaryTextContent {
                        text: String::new(),
                    }),
                },
            ),
            ResponseStreamEvent::ResponseReasoningSummaryTextDelta(
                ResponseReasoningSummaryTextDeltaEvent {
                    sequence_number: 1,
                    item_id: "rs_1".to_string(),
                    output_index: 0,
                    summary_index: 0,
                    delta: "Let me think...".to_string(),
                },
            ),
            ResponseStreamEvent::ResponseReasoningSummaryPartDone(
                ResponseReasoningSummaryPartDoneEvent {
                    sequence_number: 2,
                    item_id: "rs_1".to_string(),
                    output_index: 0,
                    summary_index: 0,
                    part: SummaryPart::SummaryText(SummaryTextContent {
                        text: "Let me think...".to_string(),
                    }),
                },
            ),
        ];

        let converted: Vec<_> = events
            .into_iter()
            .flat_map(|raw| {
                adapter.convert_event(raw);
                adapter.pending.drain(..).collect::<Vec<_>>()
            })
            .collect();

        assert_eq!(converted.len(), 3);
        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning
            }
        ));
        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Reasoning(t) } if t == "Let me think..."
        ));
        assert!(matches!(
            converted[2].as_ref().unwrap(),
            StreamEvent::ContentBlockStop { index: 0 }
        ));
    }

    #[test]
    fn mixed_content_assigns_sequential_indices() {
        let mut adapter = OpenAiStreamAdapter::new(());

        // Reasoning block at output_index 0
        adapter.convert_event(ResponseStreamEvent::ResponseReasoningSummaryPartAdded(
            ResponseReasoningSummaryPartAddedEvent {
                sequence_number: 0,
                item_id: "rs_1".to_string(),
                output_index: 0,
                summary_index: 0,
                part: SummaryPart::SummaryText(SummaryTextContent {
                    text: String::new(),
                }),
            },
        ));
        // Text block at output_index 1, content_index 0
        adapter.convert_event(ResponseStreamEvent::ResponseContentPartAdded(
            ResponseContentPartAddedEvent {
                sequence_number: 1,
                item_id: "msg_1".to_string(),
                output_index: 1,
                content_index: 0,
                part: OutputContent::OutputText(OutputTextContent {
                    text: String::new(),
                    annotations: vec![],
                    logprobs: None,
                }),
            },
        ));
        // Function call at output_index 2
        adapter.convert_event(ResponseStreamEvent::ResponseOutputItemAdded(
            ResponseOutputItemAddedEvent {
                sequence_number: 2,
                output_index: 2,
                item: OutputItem::FunctionCall(FunctionToolCall {
                    arguments: String::new(),
                    call_id: "call_1".to_string(),
                    name: "search".to_string(),
                    id: Some("fc_1".to_string()),
                    status: None,
                }),
            },
        ));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 3);

        // Reasoning gets index 0
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning
            }
        ));
        // Text gets index 1
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::Text
            }
        ));
        // Function call gets index 2
        assert!(matches!(
            events[2].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 2,
                block: ContentBlockStartData::ToolCall { .. }
            }
        ));
    }

    #[test]
    fn usage_from_completed() {
        let mut adapter = OpenAiStreamAdapter::new(());

        adapter.convert_event(ResponseStreamEvent::ResponseCompleted(
            ResponseCompletedEvent {
                sequence_number: 0,
                response: stub_response(Status::Completed, Some(stub_usage())),
            },
        ));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 2);

        let delta = events[0].as_ref().unwrap();
        assert!(matches!(
            delta,
            StreamEvent::MessageDelta { usage }
                if usage.input_tokens == Some(10)
                && usage.output_tokens == Some(5)
                && usage.total_tokens == Some(15)
        ));

        let stop = events[1].as_ref().unwrap();
        assert!(matches!(
            stop,
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    #[test]
    fn incomplete_response_maps_to_max_output_tokens() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let mut response = stub_response(Status::Incomplete, Some(stub_usage()));
        response.incomplete_details = Some(async_openai::types::responses::IncompleteDetails {
            reason: "max_output_tokens".to_string(),
        });

        adapter.convert_event(ResponseStreamEvent::ResponseIncomplete(
            ResponseIncompleteEvent {
                sequence_number: 0,
                response,
            },
        ));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::MessageStop {
                stop_reason: StopReason::MaxOutputTokens,
                ..
            }
        ));
    }

    #[test]
    fn failed_response_maps_to_error() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let mut response = stub_response(Status::Failed, None);
        response.error = Some(ErrorObject {
            code: "server_error".to_string(),
            message: "Internal error".to_string(),
        });

        adapter.convert_event(ResponseStreamEvent::ResponseFailed(ResponseFailedEvent {
            sequence_number: 0,
            response,
        }));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
    }

    #[test]
    fn error_event_maps_to_generation_error() {
        let mut adapter = OpenAiStreamAdapter::new(());

        adapter.convert_event(ResponseStreamEvent::ResponseError(ResponseErrorEvent {
            sequence_number: 0,
            code: Some("rate_limit_exceeded".to_string()),
            message: "Rate limit exceeded".to_string(),
            param: None,
        }));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
    }

    #[test]
    fn refusal_accumulates_and_emits_error() {
        let mut adapter = OpenAiStreamAdapter::new(());

        adapter.convert_event(ResponseStreamEvent::ResponseRefusalDelta(
            ResponseRefusalDeltaEvent {
                sequence_number: 0,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                delta: "I cannot ".to_string(),
            },
        ));
        assert!(adapter.pending.is_empty(), "delta should not emit events");

        adapter.convert_event(ResponseStreamEvent::ResponseRefusalDelta(
            ResponseRefusalDeltaEvent {
                sequence_number: 1,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                delta: "help with that".to_string(),
            },
        ));

        adapter.convert_event(ResponseStreamEvent::ResponseRefusalDone(
            ResponseRefusalDoneEvent {
                sequence_number: 2,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                refusal: "I cannot help with that".to_string(),
            },
        ));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(GenerationError::Refusal(msg)) => {
                assert_eq!(msg, "I cannot help with that");
            }
            other => panic!("expected Refusal error, got {other:?}"),
        }
    }

    #[test]
    fn filtered_events_produce_no_output() {
        let mut adapter = OpenAiStreamAdapter::new(());

        // Created
        let produced =
            adapter.convert_event(ResponseStreamEvent::ResponseCreated(ResponseCreatedEvent {
                sequence_number: 0,
                response: stub_response(Status::InProgress, None),
            }));
        assert!(!produced);

        // InProgress
        let produced = adapter.convert_event(ResponseStreamEvent::ResponseInProgress(
            ResponseInProgressEvent {
                sequence_number: 1,
                response: stub_response(Status::InProgress, None),
            },
        ));
        assert!(!produced);

        // OutputTextDone (we only use deltas for text, done is informational)
        let produced = adapter.convert_event(ResponseStreamEvent::ResponseOutputTextDone(
            ResponseTextDoneEvent {
                sequence_number: 2,
                item_id: "msg_1".to_string(),
                output_index: 0,
                content_index: 0,
                text: "Hello".to_string(),
                logprobs: None,
            },
        ));
        assert!(!produced);

        // FunctionCallArgumentsDone (we only use deltas, done is informational)
        let produced =
            adapter.convert_event(ResponseStreamEvent::ResponseFunctionCallArgumentsDone(
                ResponseFunctionCallArgumentsDoneEvent {
                    name: Some("test".to_string()),
                    sequence_number: 3,
                    item_id: "fc_1".to_string(),
                    output_index: 0,
                    arguments: "{}".to_string(),
                },
            ));
        assert!(!produced);

        assert!(adapter.pending.is_empty());
    }

    #[tokio::test]
    async fn poll_next_sequences_buffered_events() {
        use futures_util::StreamExt;

        let events = vec![
            Ok(ResponseStreamEvent::ResponseContentPartAdded(
                ResponseContentPartAddedEvent {
                    sequence_number: 0,
                    item_id: "msg_1".to_string(),
                    output_index: 0,
                    content_index: 0,
                    part: OutputContent::OutputText(OutputTextContent {
                        text: String::new(),
                        annotations: vec![],
                        logprobs: None,
                    }),
                },
            )),
            Ok(ResponseStreamEvent::ResponseOutputTextDelta(
                ResponseTextDeltaEvent {
                    sequence_number: 1,
                    item_id: "msg_1".to_string(),
                    output_index: 0,
                    content_index: 0,
                    delta: "Hi".to_string(),
                    logprobs: None,
                },
            )),
            Ok(ResponseStreamEvent::ResponseContentPartDone(
                ResponseContentPartDoneEvent {
                    sequence_number: 2,
                    item_id: "msg_1".to_string(),
                    output_index: 0,
                    content_index: 0,
                    part: OutputContent::OutputText(OutputTextContent {
                        text: "Hi".to_string(),
                        annotations: vec![],
                        logprobs: None,
                    }),
                },
            )),
            Ok(ResponseStreamEvent::ResponseCompleted(
                ResponseCompletedEvent {
                    sequence_number: 3,
                    response: stub_response(Status::Completed, Some(stub_usage())),
                },
            )),
        ];

        let inner = futures_util::stream::iter(events);
        let mut adapter = std::pin::pin!(OpenAiStreamAdapter::new(inner));

        // ContentBlockStart
        let event = adapter.as_mut().next().await.unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text
            }
        ));

        // ContentBlockDelta
        let event = adapter.as_mut().next().await.unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ref t)
            } if t == "Hi"
        ));

        // ContentBlockStop
        let event = adapter.as_mut().next().await.unwrap().unwrap();
        assert!(matches!(event, StreamEvent::ContentBlockStop { index: 0 }));

        // MessageDelta (buffered from ResponseCompleted)
        let event = adapter.as_mut().next().await.unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::MessageDelta { usage } if usage.input_tokens == Some(10)
        ));

        // MessageStop (also buffered from the same ResponseCompleted)
        let event = adapter.as_mut().next().await.unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));

        // Stream exhausted
        assert!(adapter.as_mut().next().await.is_none());
    }

    #[test]
    fn tool_use_stop_reason_inferred_from_completed() {
        let mut adapter = OpenAiStreamAdapter::new(());

        let mut response = stub_response(Status::Completed, Some(stub_usage()));
        response.output = vec![OutputItem::FunctionCall(FunctionToolCall {
            arguments: "{}".to_string(),
            call_id: "call_1".to_string(),
            name: "search".to_string(),
            id: Some("fc_1".to_string()),
            status: None,
        })];

        adapter.convert_event(ResponseStreamEvent::ResponseCompleted(
            ResponseCompletedEvent {
                sequence_number: 0,
                response,
            },
        ));

        let events: Vec<_> = adapter.pending.drain(..).collect();
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                ..
            }
        ));
    }
}
