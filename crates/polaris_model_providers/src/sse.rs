//! Server-Sent Events framing for streaming responses.
//!
//! Turns a stream of raw response bytes into the `data` payload of each event.
//! Follows the SSE line rules: lines end in `\n` or `\r\n` (a lone `\r` as a
//! line ending is not supported), a blank line ends an event, lines that start
//! with `:` are comments, and several `data` lines in one event are joined with
//! `\n`. The `event`, `id` and `retry` fields are ignored: the JSON payload says
//! what an event is.

use bytes::Bytes;
use core::fmt::Display;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::GenerationError;
use simdutf8::basic::from_utf8;
use std::collections::VecDeque;

/// The most bytes one event (or one unfinished line) may hold before the
/// stream fails. Bounds memory when a server never sends a line break.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// A stream of SSE event payloads read from a byte stream.
///
/// Bytes are buffered until a whole line is available, so multibyte UTF-8
/// characters split across network chunks are read correctly: UTF-8 is checked
/// per line, not per chunk.
pub(crate) struct SseDataStream<S> {
    inner: S,
    /// Bytes of the line being read.
    line: Vec<u8>,
    /// `data` lines of the event being read, joined with `\n`.
    event_data: Option<String>,
    /// Payloads and errors ready to hand out.
    pending: VecDeque<Result<String, GenerationError>>,
    /// The byte stream ended or failed; nothing more is read from it.
    finished: bool,
}

impl<S> SseDataStream<S> {
    /// Wraps a byte stream.
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            line: Vec::new(),
            event_data: None,
            pending: VecDeque::new(),
            finished: false,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            // After a failure nothing more is read, so no event follows the
            // error in the output.
            if self.finished {
                return;
            }
            if byte == b'\n' {
                let line = core::mem::take(&mut self.line);
                self.handle_line(&line);
            } else {
                self.line.push(byte);
                if self.line.len() > MAX_EVENT_BYTES {
                    self.line.clear();
                    self.fail("an SSE line is larger than the 16 MiB limit");
                    return;
                }
            }
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.dispatch();
            return;
        }
        if line.starts_with(b":") {
            return;
        }
        let text = match from_utf8(line) {
            Ok(text) => text,
            Err(err) => {
                self.fail(&format!("invalid UTF-8 in an SSE line: {err}"));
                return;
            }
        };
        let (field, value) = text.split_once(':').unwrap_or((text, ""));
        if field != "data" {
            return;
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        match &mut self.event_data {
            Some(data) => {
                if data.len() + value.len() >= MAX_EVENT_BYTES {
                    self.fail("an SSE event is larger than the 16 MiB limit");
                    return;
                }
                data.push('\n');
                data.push_str(value);
            }
            None => self.event_data = Some(value.to_owned()),
        }
    }

    fn dispatch(&mut self) {
        if let Some(data) = self.event_data.take() {
            self.pending.push_back(Ok(data));
        }
    }

    fn fail(&mut self, message: &str) {
        self.event_data = None;
        self.finished = true;
        self.pending
            .push_back(Err(GenerationError::InvalidResponse(message.to_owned())));
    }

    /// Reads what is left when the byte stream ends: an unterminated last line
    /// and an event with no closing blank line still count.
    fn finish(&mut self) {
        if !self.line.is_empty() {
            let line = core::mem::take(&mut self.line);
            self.handle_line(&line);
        }
        self.dispatch();
        self.finished = true;
    }
}

impl<S, E> Stream for SseDataStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Display,
{
    type Item = Result<String, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(item) = this.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => this.push_bytes(&bytes),
                Poll::Ready(Some(Err(err))) => {
                    // Events completed before the failure are handed out first.
                    this.finished = true;
                    this.pending
                        .push_back(Err(GenerationError::Http(err.to_string())));
                }
                Poll::Ready(None) => this.finish(),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    type Chunk = Result<Bytes, String>;

    async fn read(chunks: Vec<Chunk>) -> Vec<Result<String, GenerationError>> {
        SseDataStream::new(futures_util::stream::iter(chunks))
            .collect()
            .await
    }

    fn ok(text: &str) -> Chunk {
        Ok(Bytes::copy_from_slice(text.as_bytes()))
    }

    fn payloads(items: Vec<Result<String, GenerationError>>) -> Vec<String> {
        items
            .into_iter()
            .map(|item| item.expect("no error expected"))
            .collect()
    }

    #[tokio::test]
    async fn reads_lf_and_crlf_events_and_ignores_other_fields_and_comments() {
        let items = read(vec![ok(
            ": keep-alive\n\nevent: message\ndata: {\"a\":1}\n\nid: 7\r\ndata:{\"b\":2}\r\n\r\n",
        )])
        .await;
        assert_eq!(payloads(items), ["{\"a\":1}", "{\"b\":2}"]);
    }

    #[tokio::test]
    async fn joins_several_data_lines_with_a_newline() {
        let items = read(vec![ok("data: one\ndata: two\n\n")]).await;
        assert_eq!(payloads(items), ["one\ntwo"]);
    }

    #[tokio::test]
    async fn reads_an_event_split_across_chunks_and_inside_a_character() {
        let bytes = "data: h\u{e9}llo\n\n".as_bytes();
        // Split in the middle of the two-byte `é`.
        let (first, second) = bytes.split_at(8);
        let items = read(vec![
            Ok(Bytes::copy_from_slice(first)),
            Ok(Bytes::copy_from_slice(second)),
        ])
        .await;
        assert_eq!(payloads(items), ["h\u{e9}llo"]);
    }

    #[tokio::test]
    async fn dispatches_an_unterminated_last_event() {
        let items = read(vec![ok("data: first\n\ndata: last")]).await;
        assert_eq!(payloads(items), ["first", "last"]);
    }

    #[tokio::test]
    async fn invalid_utf8_fails_the_stream() {
        let items = read(vec![
            ok("data: fine\n\n"),
            Ok(Bytes::from_static(b"data: \xff\xfe\n\n")),
            ok("data: after\n\n"),
        ])
        .await;
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].as_ref().unwrap(), "fine");
        assert!(
            matches!(&items[1], Err(GenerationError::InvalidResponse(message)) if message.contains("UTF-8")),
            "{items:?}"
        );
    }

    #[tokio::test]
    async fn nothing_follows_an_error_in_the_same_chunk() {
        let items = read(vec![Ok(Bytes::from_static(
            b"data: one\n\ndata: \xff\n\ndata: two\n\n",
        ))])
        .await;
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].as_ref().unwrap(), "one");
        assert!(matches!(
            &items[1],
            Err(GenerationError::InvalidResponse(_))
        ));
    }

    #[tokio::test]
    async fn a_transport_error_comes_after_the_events_before_it() {
        let items = read(vec![
            ok("data: one\n\ndata: two\n"),
            Err("reset".to_owned()),
        ])
        .await;
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].as_ref().unwrap(), "one");
        assert!(
            matches!(&items[1], Err(GenerationError::Http(message)) if message == "reset"),
            "{items:?}"
        );
    }
}
