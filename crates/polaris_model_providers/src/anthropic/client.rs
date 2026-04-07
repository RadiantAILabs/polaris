//! Anthropic API client.

use super::types::{CreateMessageRequest, MessageResponse, RawStreamEvent};
use bytes::Bytes;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::GenerationError;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use simdutf8::basic::from_utf8;
use std::collections::VecDeque;

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

    /// Builds common headers for the Anthropic API.
    fn build_headers(&self, request: &CreateMessageRequest) -> Result<HeaderMap, GenerationError> {
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

        Ok(headers)
    }

    /// Sends a create message request to the Anthropic API.
    pub async fn create_message(
        &self,
        request: &CreateMessageRequest,
    ) -> Result<MessageResponse, GenerationError> {
        let url = format!("{}/v1/messages", self.base_url);
        let headers = self.build_headers(request)?;

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

    /// Sends a streaming create message request and returns a receiver of raw SSE events.
    ///
    /// `stream` is set to `true` on the request before sending.
    /// SSE frames are parsed and deserialized into [`RawStreamEvent`]s.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError::Auth`] if the API key is not valid UTF-8.
    /// Returns [`GenerationError::Http`] if the HTTP request fails to send.
    /// Returns [`GenerationError::Provider`] if the server responds with a non-2xx
    /// status code.
    ///
    /// # Examples
    ///
    /// ```ignore (uses crate-private types)
    /// use futures_core::Stream;
    ///
    /// let client = AnthropicClient::new("sk-ant-...");
    /// let request = CreateMessageRequest { /* ... */ };
    /// let stream = client.create_message_stream(request).await?;
    /// // Poll the stream for RawStreamEvents...
    /// ```
    pub async fn create_message_stream(
        &self,
        mut request: CreateMessageRequest,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<RawStreamEvent, GenerationError>> + Send>>,
        GenerationError,
    > {
        let url = format!("{}/v1/messages", self.base_url);
        let headers = self.build_headers(&request)?;

        request.stream = Some(true);

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&request)
            .send()
            .await
            .map_err(|err| GenerationError::Http(err.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .map_err(|err| GenerationError::Http(err.to_string()))?;
            return Err(GenerationError::Provider {
                status: Some(status.as_u16()),
                message: error_body,
                source: None,
            });
        }

        let byte_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
            Box::pin(response.bytes_stream());
        Ok(Box::pin(SseStream::new(byte_stream)))
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

// ─────────────────────────────────────────────────────────────────────────────
// SSE Stream Parser
// ─────────────────────────────────────────────────────────────────────────────

/// Wraps a raw byte stream and parses SSE frames into [`RawStreamEvent`]s.
///
/// The Anthropic streaming API uses the Server-Sent Events format where each
/// frame consists of `event: <type>\ndata: <json>\n\n`. This parser buffers
/// incoming bytes at the byte level, splits on double-newline boundaries, and
/// validates UTF-8 on complete frames before deserializing the `data` line as
/// JSON.
///
/// Byte-level buffering ensures that multibyte UTF-8 characters split across
/// network chunks are handled correctly — validation occurs per-frame, not
/// per-chunk.
///
/// # Platform note
///
/// Frame boundaries are detected using bare LF (`\n\n`) only, matching what the
/// Anthropic Messages API emits. CRLF (`\r\n\r\n`) boundaries are **not**
/// handled. This means the parser will not work correctly if the byte stream
/// uses Windows-style line endings (e.g. from a proxy that re-encodes line
/// endings).
struct SseStream<S> {
    inner: S,
    buffer: Vec<u8>,
    pending: VecDeque<Result<RawStreamEvent, GenerationError>>,
    /// A transport error stashed until all buffered events have been drained.
    deferred_error: Option<GenerationError>,
    done: bool,
}

impl<S> SseStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            deferred_error: None,
            done: false,
        }
    }

    /// Parse complete SSE frames from the buffer, pushing results into `pending`.
    fn parse_frames(&mut self) {
        // SSE frames are separated by blank lines (\n\n). Anthropic's streaming
        // API uses bare LF; the SSE spec also permits \r\n\r\n but that is not
        // emitted by the Messages API. We search at the byte level so that
        // multibyte UTF-8 characters split across network chunks don't cause
        // spurious errors.
        while let Some(boundary) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            // Validate UTF-8 on the complete frame — not on individual chunks.
            let frame = match from_utf8(&self.buffer[..boundary]) {
                Ok(text) => text,
                Err(err) => {
                    self.buffer.drain(..boundary + 2);
                    self.pending
                        .push_back(Err(GenerationError::InvalidResponse(format!(
                            "invalid UTF-8 in SSE frame: {err}"
                        ))));
                    continue;
                }
            };

            let mut data_line = None;

            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("data: ") {
                    data_line = Some(value);
                } else if let Some(value) = line.strip_prefix("data:") {
                    data_line = Some(value);
                }
                // We ignore `event:`, `id:`, and `retry:` lines — the `type`
                // field inside the JSON `data` payload identifies the event.
            }

            if let Some(data) = data_line
                && !data.trim().is_empty()
            {
                match serde_json::from_str::<RawStreamEvent>(data) {
                    Ok(event) => self.pending.push_back(Ok(event)),
                    Err(err) => {
                        self.pending
                            .push_back(Err(GenerationError::InvalidResponse(format!(
                                "failed to parse SSE event: {err}\nData: {data}"
                            ))));
                    }
                }
            }

            self.buffer.drain(..boundary + 2);
        }
    }
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<RawStreamEvent, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Drain any already-parsed events first.
        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(event));
        }

        // Surface a deferred transport error after all buffered events have been delivered.
        if let Some(err) = this.deferred_error.take() {
            return Poll::Ready(Some(Err(err)));
        }

        if this.done {
            return Poll::Ready(None);
        }

        // Pull bytes from the inner stream and parse frames.
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buffer.extend_from_slice(&bytes);

                    this.parse_frames();

                    if let Some(event) = this.pending.pop_front() {
                        return Poll::Ready(Some(event));
                    }
                    // No complete frame yet — continue polling for more bytes.
                }
                Poll::Ready(Some(Err(err))) => {
                    this.done = true;
                    this.deferred_error = Some(GenerationError::Http(err.to_string()));
                    // Drain any buffered events before surfacing the error.
                    return Poll::Ready(
                        this.pending
                            .pop_front()
                            .or(this.deferred_error.take().map(Err)),
                    );
                }
                Poll::Ready(None) => {
                    this.done = true;
                    // Parse any remaining data in the buffer.
                    if this.buffer.iter().any(|b| !b.is_ascii_whitespace()) {
                        // Append a trailing double-newline to flush partial frames.
                        this.buffer.extend_from_slice(b"\n\n");
                        this.parse_frames();
                    }
                    return Poll::Ready(this.pending.pop_front());
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock byte stream from a list of chunks.
    struct MockByteStream(VecDeque<Result<Bytes, reqwest::Error>>);

    impl Stream for MockByteStream {
        type Item = Result<Bytes, reqwest::Error>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn mock_stream(chunks: Vec<&str>) -> MockByteStream {
        MockByteStream(
            chunks
                .into_iter()
                .map(|s| Ok(Bytes::from(s.to_string())))
                .collect(),
        )
    }

    fn mock_byte_stream(chunks: Vec<Vec<u8>>) -> MockByteStream {
        MockByteStream(chunks.into_iter().map(|b| Ok(Bytes::from(b))).collect())
    }

    /// Collect all events from an SSE stream.
    async fn collect_events(
        stream: SseStream<MockByteStream>,
    ) -> Vec<Result<RawStreamEvent, GenerationError>> {
        let mut stream = std::pin::pin!(stream);
        let mut events = Vec::new();
        loop {
            match std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                Some(event) => events.push(event),
                None => break,
            }
        }
        events
    }

    #[tokio::test]
    async fn parses_text_delta() {
        let raw = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n";
        let stream = SseStream::new(mock_stream(vec![raw]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            RawStreamEvent::ContentBlockDelta { index: 0, .. }
        ));
    }

    #[tokio::test]
    async fn parses_complete_sequence() {
        let data = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let stream = SseStream::new(mock_stream(vec![data]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 6);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            RawStreamEvent::MessageStart { .. }
        ));
        assert!(matches!(
            events[1].as_ref().unwrap(),
            RawStreamEvent::ContentBlockStart { .. }
        ));
        assert!(matches!(
            events[2].as_ref().unwrap(),
            RawStreamEvent::ContentBlockDelta { .. }
        ));
        assert!(matches!(
            events[3].as_ref().unwrap(),
            RawStreamEvent::ContentBlockStop { .. }
        ));
        assert!(matches!(
            events[4].as_ref().unwrap(),
            RawStreamEvent::MessageDelta { .. }
        ));
        assert!(matches!(
            events[5].as_ref().unwrap(),
            RawStreamEvent::MessageStop
        ));
    }

    #[tokio::test]
    async fn handles_chunked_bytes() {
        // SSE frame split across two byte chunks.
        let chunk1 = "event: ping\ndata: {\"type\"";
        let chunk2 = ":\"ping\"}\n\n";
        let stream = SseStream::new(mock_stream(vec![chunk1, chunk2]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].as_ref().unwrap(), RawStreamEvent::Ping));
    }

    #[tokio::test]
    async fn skips_empty_data() {
        let raw =
            "event: ping\ndata: \n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let stream = SseStream::new(mock_stream(vec![raw]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            RawStreamEvent::MessageStop
        ));
    }

    #[tokio::test]
    async fn parses_tool_use_stream() {
        let data = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"London\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let stream = SseStream::new(mock_stream(vec![data]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: super::super::types::StreamContentBlock::ToolUse { .. }
            }
        ));
    }

    #[tokio::test]
    async fn parses_error_event() {
        let data = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let stream = SseStream::new(mock_stream(vec![data]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        match events[0].as_ref().unwrap() {
            RawStreamEvent::Error { error } => {
                assert_eq!(error.error_type, "overloaded_error");
                assert_eq!(error.message, "Overloaded");
            }
            other => panic!("expected Error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_thinking_stream() {
        let data = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think...\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let stream = SseStream::new(mock_stream(vec![data]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            RawStreamEvent::ContentBlockStart {
                content_block: super::super::types::StreamContentBlock::Thinking { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn invalid_json_returns_error() {
        let data = "event: content_block_delta\ndata: {not valid json}\n\n";
        let stream = SseStream::new(mock_stream(vec![data]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
    }

    #[tokio::test]
    async fn handles_split_multibyte_utf8() {
        // 'é' (U+00E9) is 0xC3 0xA9 in UTF-8. Split it across two chunks to
        // verify that byte-level buffering handles incomplete multibyte sequences.
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"café"}}"#;
        let full = format!("event: content_block_delta\ndata: {json}\n\n");
        let bytes = full.as_bytes();

        // Find the 'é' bytes and split between them.
        let split_at = bytes
            .windows(2)
            .position(|w| w == [0xC3, 0xA9])
            .expect("should find é bytes")
            + 1;

        let chunk1 = bytes[..split_at].to_vec();
        let chunk2 = bytes[split_at..].to_vec();

        let stream = SseStream::new(mock_byte_stream(vec![chunk1, chunk2]));
        let events = collect_events(stream).await;

        assert_eq!(events.len(), 1);
        assert!(
            events[0].is_ok(),
            "should not error on split multibyte UTF-8"
        );
    }
}
