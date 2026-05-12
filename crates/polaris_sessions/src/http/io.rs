//! Channel-based [`IOProvider`] for sessions HTTP request/response bridging.

use parking_lot::Mutex;
use polaris_core_plugins::{IOError, IOMessage, IOProvider};
use tokio::sync::mpsc;

/// Channel-based [`IOProvider`] for bridging HTTP requests to agent I/O.
///
/// Created per-request by sessions HTTP handlers. The handler holds the
/// channel endpoints while the agent graph interacts through the
/// [`IOProvider`] trait.
///
/// Both input and output channels are bounded so a misbehaving agent (or a
/// disconnected SSE consumer) cannot drive unbounded memory growth — when
/// the receiver lags the agent, [`HttpIOProvider::send`] applies
/// backpressure via `await`.
pub struct HttpIOProvider {
    input_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    output_tx: Mutex<Option<mpsc::Sender<IOMessage>>>,
}

impl std::fmt::Debug for HttpIOProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpIOProvider")
            .field("closed", &self.output_tx.lock().is_none())
            .finish_non_exhaustive()
    }
}

impl HttpIOProvider {
    /// Creates a new provider with the given input/output channel buffer
    /// sizes.
    ///
    /// Returns the provider plus the input/output channel halves used by the
    /// HTTP handler around it. Both channels are bounded — pick a sensible
    /// `output_buffer` (e.g. 32–64) to balance smoothing short bursts
    /// against bounding memory if the consumer stalls.
    #[must_use]
    pub fn new(
        input_buffer: usize,
        output_buffer: usize,
    ) -> (Self, mpsc::Sender<IOMessage>, mpsc::Receiver<IOMessage>) {
        let (input_tx, input_rx) = mpsc::channel(input_buffer);
        let (output_tx, output_rx) = mpsc::channel(output_buffer);

        let provider = Self {
            input_rx: tokio::sync::Mutex::new(input_rx),
            output_tx: Mutex::new(Some(output_tx)),
        };

        (provider, input_tx, output_rx)
    }
}

impl IOProvider for HttpIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        // Clone the sender out of the mutex so the parking_lot guard is
        // never held across the bounded `send().await` (which would block
        // a blocking lock under backpressure).
        let tx = {
            let guard = self.output_tx.lock();
            guard.as_ref().ok_or(IOError::Closed)?.clone()
        };
        tx.send(message).await.map_err(|_| IOError::Closed)
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.input_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(IOError::Closed)
    }

    async fn close(&self) {
        self.output_tx.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core_plugins::IOContent;

    #[tokio::test]
    async fn send_and_receive() {
        let (provider, input_tx, mut output_rx) = HttpIOProvider::new(8, 8);

        input_tx.send(IOMessage::user_text("hello")).await.unwrap();

        let msg = provider.receive().await.unwrap();
        assert!(matches!(msg.content, IOContent::Text(ref t) if t == "hello"));

        provider
            .send(IOMessage::system_text("response"))
            .await
            .unwrap();

        let resp = output_rx.recv().await.unwrap();
        assert!(matches!(resp.content, IOContent::Text(ref t) if t == "response"));
    }

    #[tokio::test]
    async fn receive_returns_closed_when_sender_dropped() {
        let (provider, input_tx, _output_rx) = HttpIOProvider::new(8, 8);
        drop(input_tx);

        let result = provider.receive().await;
        assert!(matches!(result, Err(IOError::Closed)));
    }

    #[tokio::test]
    async fn send_returns_closed_when_receiver_dropped() {
        let (provider, _input_tx, output_rx) = HttpIOProvider::new(8, 8);
        drop(output_rx);

        let result = provider.send(IOMessage::system_text("msg")).await;
        assert!(matches!(result, Err(IOError::Closed)));
    }

    /// `close()` drops the output sender so subsequent `send()` calls
    /// observe `IOError::Closed` and the receiver sees the stream end.
    #[tokio::test]
    async fn close_terminates_output_stream() {
        let (provider, _input_tx, mut output_rx) = HttpIOProvider::new(8, 8);

        provider
            .send(IOMessage::system_text("first"))
            .await
            .expect("first send should succeed before close");

        provider.close().await;

        // Subsequent sends fail — sender slot is taken.
        let after_close = provider.send(IOMessage::system_text("after")).await;
        assert!(matches!(after_close, Err(IOError::Closed)));

        // Receiver drains the buffered message, then sees end-of-stream.
        let buffered = output_rx.recv().await.expect("buffered message");
        assert!(matches!(buffered.content, IOContent::Text(ref t) if t == "first"));
        assert!(
            output_rx.recv().await.is_none(),
            "receiver should observe end-of-stream after close()"
        );
    }

    /// `close()` is idempotent — calling it twice is a no-op.
    #[tokio::test]
    async fn close_is_idempotent() {
        let (provider, _input_tx, _output_rx) = HttpIOProvider::new(8, 8);
        provider.close().await;
        provider.close().await;
        let result = provider.send(IOMessage::system_text("x")).await;
        assert!(matches!(result, Err(IOError::Closed)));
    }
}
