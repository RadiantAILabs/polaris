//! Turning a Chat Completions SSE stream into Polaris stream events.
//!
//! Each `data:` payload is a JSON chunk whose first choice carries a `delta`.
//! The adapter keeps one open reasoning or text block at a time (a new kind of
//! content closes the old block and opens another) and one block per tool call,
//! keyed by the call's `index`. The message ends only at `data: [DONE]` or when
//! the connection ends, never at `finish_reason`, because servers send the
//! final `usage` in a chunk after the one that carries `finish_reason`.

use super::response::{
    error_message, first_choice, parse_usage, reasoning_text, stop_reason, synthesized_call_id,
};
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, StreamEvent, Usage,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

/// Text, as opposed to tool calls, that can fill an open block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Reasoning,
    Text,
}

/// The reasoning or text block that is open.
#[derive(Debug, Clone, Copy)]
struct OpenBlock {
    kind: BlockKind,
    index: u32,
}

/// One tool call being streamed.
#[derive(Debug, Default)]
struct ToolSlot {
    /// The call's id (empty until the server sends one).
    id: String,
    /// The function name (empty until the server sends it).
    name: String,
    /// The block index, once the call has started.
    block: Option<u32>,
    /// Argument text received before the call could start.
    buffered: String,
}

/// Converts the `data` payloads of a Chat Completions stream into
/// [`StreamEvent`]s.
pub(super) struct ChatStreamAdapter<S> {
    inner: S,
    pending: VecDeque<Result<StreamEvent, GenerationError>>,
    next_block: u32,
    open: Option<OpenBlock>,
    tools: BTreeMap<u64, ToolSlot>,
    last_tool: Option<u64>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    response_id: Option<String>,
    model: Option<String>,
    refusal: String,
    /// Some content block has been emitted.
    emitted: bool,
    /// `data: [DONE]` was received.
    done_marker: bool,
    /// The stream is over: `MessageStop` or an error has been queued.
    stopped: bool,
}

impl<S> ChatStreamAdapter<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            pending: VecDeque::new(),
            next_block: 0,
            open: None,
            tools: BTreeMap::new(),
            last_tool: None,
            finish_reason: None,
            usage: None,
            response_id: None,
            model: None,
            refusal: String::new(),
            emitted: false,
            done_marker: false,
            stopped: false,
        }
    }

    fn fail(&mut self, error: GenerationError) {
        self.pending.push_back(Err(error));
        self.stopped = true;
    }

    fn take_block_index(&mut self) -> u32 {
        let index = self.next_block;
        self.next_block += 1;
        index
    }

    fn close_open(&mut self) {
        if let Some(open) = self.open.take() {
            self.pending
                .push_back(Ok(StreamEvent::ContentBlockStop { index: open.index }));
        }
    }

    /// Adds text to the open block of `kind`, opening one if needed.
    fn push_text(&mut self, kind: BlockKind, text: &str) {
        let index = match self.open {
            Some(open) if open.kind == kind => open.index,
            _ => {
                self.close_open();
                let index = self.take_block_index();
                let block = match kind {
                    BlockKind::Reasoning => ContentBlockStartData::Reasoning,
                    BlockKind::Text => ContentBlockStartData::Text,
                };
                self.pending
                    .push_back(Ok(StreamEvent::ContentBlockStart { index, block }));
                self.open = Some(OpenBlock { kind, index });
                index
            }
        };
        let delta = match kind {
            BlockKind::Reasoning => ContentBlockDelta::Reasoning(text.to_owned()),
            BlockKind::Text => ContentBlockDelta::Text(text.to_owned()),
        };
        self.pending
            .push_back(Ok(StreamEvent::ContentBlockDelta { index, delta }));
        self.emitted = true;
    }

    /// The slot index a tool call chunk belongs to.
    ///
    /// Servers number calls with `index`. One that does not is read as: a chunk
    /// with an id we know continues that call, a chunk with a new id starts the
    /// next call, and a chunk with no id continues the latest call.
    fn tool_index(&self, position: usize, call: &Value) -> u64 {
        if let Some(index) = call.get("index").and_then(Value::as_u64) {
            return index;
        }
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        match id {
            Some(id) => self
                .tools
                .iter()
                .find(|(_, slot)| slot.id == id)
                .map(|(index, _)| *index)
                .unwrap_or_else(|| self.tools.keys().next_back().map_or(0, |last| last + 1)),
            None => self.last_tool.unwrap_or(position as u64),
        }
    }

    fn handle_tool_call(&mut self, position: usize, call: &Value) {
        let index = self.tool_index(position, call);
        self.last_tool = Some(index);
        let function = call.get("function").unwrap_or(&Value::Null);
        let arguments = match function.get("arguments") {
            Some(Value::String(text)) => text.clone(),
            Some(arguments @ (Value::Object(_) | Value::Array(_))) => arguments.to_string(),
            _ => String::new(),
        };

        let slot = self.tools.entry(index).or_default();
        if slot.id.is_empty()
            && let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            slot.id = id.to_owned();
        }
        if slot.name.is_empty()
            && let Some(name) = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        {
            slot.name = name.to_owned();
        }

        if let Some(block) = slot.block {
            if !arguments.is_empty() {
                self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index: block,
                    delta: ContentBlockDelta::ToolCall { arguments },
                }));
            }
            return;
        }

        // The call cannot start before its name is known. Arguments that
        // arrive first are kept and sent right after the start.
        slot.buffered.push_str(&arguments);
        if slot.name.is_empty() {
            return;
        }
        let (mut id, name) = (slot.id.clone(), slot.name.clone());
        let buffered = core::mem::take(&mut slot.buffered);

        self.close_open();
        let block = self.take_block_index();
        if id.is_empty() {
            id = synthesized_call_id(self.response_id.as_deref(), index);
        }
        if let Some(slot) = self.tools.get_mut(&index) {
            slot.block = Some(block);
            slot.id.clone_from(&id);
        }
        self.pending.push_back(Ok(StreamEvent::ContentBlockStart {
            index: block,
            block: ContentBlockStartData::ToolCall {
                call_id: Some(id.clone()),
                id,
                name,
            },
        }));
        if !buffered.is_empty() {
            self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                index: block,
                delta: ContentBlockDelta::ToolCall {
                    arguments: buffered,
                },
            }));
        }
        self.emitted = true;
    }

    fn handle_delta(&mut self, delta: &Value) {
        if let Some(text) = reasoning_text(delta) {
            self.push_text(BlockKind::Reasoning, text);
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.push_text(BlockKind::Text, text);
        }
        if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
            self.refusal.push_str(refusal);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                self.handle_tool_call(position, call);
            }
        }
    }

    fn handle_payload(&mut self, payload: &str) {
        let payload = payload.trim();
        if payload.is_empty() {
            return;
        }
        if payload == "[DONE]" {
            self.done_marker = true;
            self.finish();
            return;
        }
        let chunk: Value = match serde_json::from_str(payload) {
            Ok(chunk) => chunk,
            Err(err) => {
                self.fail(GenerationError::InvalidResponse(format!(
                    "a stream chunk is not valid JSON: {err}"
                )));
                return;
            }
        };

        let choices = chunk
            .get("choices")
            .and_then(Value::as_array)
            .filter(|choices| !choices.is_empty());
        // A server can report a failure in the middle of a stream.
        if choices.is_none()
            && let Some(message) = error_message(&chunk)
        {
            self.fail(GenerationError::Provider {
                status: None,
                message,
                source: None,
            });
            return;
        }

        if self.response_id.is_none() {
            self.response_id = chunk
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
        }
        if self.model.is_none() {
            self.model = chunk
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_owned);
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            let usage = parse_usage(usage);
            self.usage = Some(usage.clone());
            self.pending
                .push_back(Ok(StreamEvent::MessageDelta { usage }));
        }

        if let Some(choices) = choices {
            let choice = first_choice(choices);
            let delta = ["delta", "message"]
                .into_iter()
                .filter_map(|key| choice.get(key))
                .find(|delta| delta.is_object());
            if let Some(delta) = delta {
                self.handle_delta(delta);
            }
            if let Some(reason) = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                self.finish_reason = Some(reason.to_owned());
            }
        }
    }

    /// Ends the message: closes open blocks and queues `MessageStop`, or the
    /// error that says why the stream cannot end cleanly.
    fn finish(&mut self) {
        if self.stopped {
            return;
        }
        self.close_open();
        let blocks: Vec<u32> = self.tools.values().filter_map(|slot| slot.block).collect();
        for index in blocks.iter().copied() {
            self.pending
                .push_back(Ok(StreamEvent::ContentBlockStop { index }));
        }
        if let Some((index, _)) = self.tools.iter().find(|(_, slot)| {
            slot.block.is_none() && (!slot.id.is_empty() || !slot.buffered.is_empty())
        }) {
            let message = format!("tool call {index} has no function name");
            self.fail(GenerationError::InvalidResponse(message));
            return;
        }
        if !self.emitted && !self.refusal.is_empty() {
            let refusal = core::mem::take(&mut self.refusal);
            self.fail(GenerationError::Refusal(refusal));
            return;
        }
        if self.finish_reason.is_none() && !self.done_marker {
            self.fail(GenerationError::InvalidResponse(
                "the stream ended before the server finished the response".to_owned(),
            ));
            return;
        }

        let stop_reason = stop_reason(self.finish_reason.as_deref(), !blocks.is_empty());
        self.pending.push_back(Ok(StreamEvent::MessageStop {
            stop_reason,
            usage: self.usage.clone().unwrap_or_default(),
            id: self.response_id.clone(),
            model: self.model.clone(),
        }));
        self.stopped = true;
    }
}

impl<S> Stream for ChatStreamAdapter<S>
where
    S: Stream<Item = Result<String, GenerationError>> + Unpin,
{
    type Item = Result<StreamEvent, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(item) = this.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
            if this.stopped {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(payload))) => this.handle_payload(&payload),
                Poll::Ready(Some(Err(err))) => this.fail(err),
                Poll::Ready(None) => this.finish(),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
