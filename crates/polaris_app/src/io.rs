//! Channel-based [`IOProvider`] for HTTP request/response bridging.
//!
//! [`HttpIOProvider`] connects an HTTP handler to the agent's [`UserIO`]
//! abstraction via tokio channels. The handler pre-loads user input into
//! the input channel and collects agent output from the output channel.
//!
//! # Example
//!
//! ```no_run
//! use polaris_app::HttpIOProvider;
//! use polaris_core_plugins::{IOMessage, IOProvider};
//!
//! # async fn example() {
//! let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1);
//!
//! // HTTP handler sends user input
//! input_tx.send(IOMessage::user_text("hello")).await.unwrap();
//!
//! // Agent receives it
//! let msg = provider.receive().await.unwrap();
//!
//! // Agent sends response
//! provider.send(IOMessage::system_text("world")).await.unwrap();
//!
//! // HTTP handler collects it
//! let resp = output_rx.recv().await.unwrap();
//! # }
//! ```

use polaris_core_plugins::{IOError, IOMessage, IOProvider};
use tokio::sync::mpsc;

/// Channel-based [`IOProvider`] for bridging HTTP requests to agent I/O.
///
/// Created per-request by HTTP handlers. The handler holds the channel
/// endpoints while the agent graph interacts through the [`IOProvider`] trait.
#[derive(Debug)]
pub struct HttpIOProvider {
    /// Agent reads user input from here.
    input_rx: tokio::sync::Mutex<mpsc::Receiver<IOMessage>>,
    /// Agent writes output here (unbounded to prevent deadlock).
    output_tx: mpsc::UnboundedSender<IOMessage>,
}

impl HttpIOProvider {
    /// Creates a new provider with the given input channel buffer size.
    ///
    /// The output channel is unbounded because the handler only drains it
    /// after the turn completes. A bounded output channel would deadlock if
    /// the agent produced more messages than the buffer capacity.
    ///
    /// Returns:
    /// - The provider (give to [`UserIO::new`](polaris_core_plugins::UserIO::new))
    /// - `input_tx` — the handler sends user messages here
    /// - `output_rx` — the handler collects agent responses here
    #[must_use]
    pub fn new(
        input_buffer: usize,
    ) -> (
        Self,
        mpsc::Sender<IOMessage>,
        mpsc::UnboundedReceiver<IOMessage>,
    ) {
        let (input_tx, input_rx) = mpsc::channel(input_buffer);
        let (output_tx, output_rx) = mpsc::unbounded_channel();

        let provider = Self {
            input_rx: tokio::sync::Mutex::new(input_rx),
            output_tx,
        };

        (provider, input_tx, output_rx)
    }
}

impl IOProvider for HttpIOProvider {
    /// Sends a message from the agent to the HTTP response channel.
    ///
    /// # Errors
    ///
    /// Returns [`IOError::Closed`] if the output channel receiver has been dropped.
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.output_tx.send(message).map_err(|_| IOError::Closed)
    }

    /// Receives a message from the HTTP request input channel.
    ///
    /// # Errors
    ///
    /// Returns [`IOError::Closed`] if the input channel sender has been dropped.
    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.input_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or(IOError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core_plugins::IOContent;

    #[tokio::test]
    async fn send_and_receive() {
        let (provider, input_tx, mut output_rx) = HttpIOProvider::new(8);

        // Simulate HTTP handler sending user input
        input_tx.send(IOMessage::user_text("hello")).await.unwrap();

        // Agent receives
        let msg = provider.receive().await.unwrap();
        assert!(matches!(msg.content, IOContent::Text(ref t) if t == "hello"));

        // Agent sends response
        provider
            .send(IOMessage::system_text("response"))
            .await
            .unwrap();

        // HTTP handler collects
        let resp = output_rx.recv().await.unwrap();
        assert!(matches!(resp.content, IOContent::Text(ref t) if t == "response"));
    }

    #[tokio::test]
    async fn receive_returns_closed_when_sender_dropped() {
        let (provider, input_tx, _output_rx) = HttpIOProvider::new(8);
        drop(input_tx);

        let result = provider.receive().await;
        assert!(matches!(result, Err(IOError::Closed)));
    }

    #[tokio::test]
    async fn send_returns_closed_when_receiver_dropped() {
        let (provider, _input_tx, output_rx) = HttpIOProvider::new(8);
        drop(output_rx);

        let result = provider.send(IOMessage::system_text("msg")).await;
        assert!(matches!(result, Err(IOError::Closed)));
    }

    #[tokio::test]
    async fn multiple_messages_in_order() {
        let (provider, input_tx, mut output_rx) = HttpIOProvider::new(8);

        // Send multiple inputs
        input_tx.send(IOMessage::user_text("a")).await.unwrap();
        input_tx.send(IOMessage::user_text("b")).await.unwrap();

        // Receive in order
        let a = provider.receive().await.unwrap();
        assert!(matches!(a.content, IOContent::Text(ref t) if t == "a"));
        let b = provider.receive().await.unwrap();
        assert!(matches!(b.content, IOContent::Text(ref t) if t == "b"));

        // Send multiple outputs
        provider.send(IOMessage::system_text("x")).await.unwrap();
        provider.send(IOMessage::system_text("y")).await.unwrap();

        let x = output_rx.recv().await.unwrap();
        assert!(matches!(x.content, IOContent::Text(ref t) if t == "x"));
        let y = output_rx.recv().await.unwrap();
        assert!(matches!(y.content, IOContent::Text(ref t) if t == "y"));
    }
}
