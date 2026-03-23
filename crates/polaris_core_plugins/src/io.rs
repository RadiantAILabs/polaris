//! I/O plugin for agent communication.
//!
//! Provides [`IOPlugin`] which registers per-agent I/O buffer resources, and defines
//! the [`IOProvider`] trait for concrete communication implementations.
//!
//! # Architecture
//!
//! [`IOPlugin`] is a **base abstraction layer**. It defines the contract (traits, message
//! types, buffer resources) that concrete communication plugins can be built on top of.
//! It does **not** implement any actual communication channels itself — that is the role of
//! concrete plugins (e.g., terminal, WebSocket, MCP, HTTP).
//!
//! # Resources Provided
//!
//! | Resource | Scope | Description |
//! |----------|-------|-------------|
//! | [`InputBuffer`] | Local | Per-agent buffer for incoming messages |
//! | [`OutputBuffer`] | Local | Per-agent buffer for outgoing messages |
//!
//! [`UserIO`] is **not** registered by [`IOPlugin`] — concrete communication plugins
//! register it via [`Server::register_local`] with a factory that captures their
//! [`IOProvider`] implementation:
//!
//! ```
//! # use std::sync::Arc;
//! # use polaris_system::server::Server;
//! # use polaris_system::plugin::{Plugin, PluginId, Version};
//! # use polaris_core_plugins::{ServerInfoPlugin, IOPlugin, IOProvider, IOMessage, IOError, UserIO};
//!
//! struct TerminalProvider;
//!
//! impl IOProvider for TerminalProvider {
//!     async fn send(&self, _message: IOMessage) -> Result<(), IOError> {
//!         Ok(()) // write to stdout
//!     }
//!     async fn receive(&self) -> Result<IOMessage, IOError> {
//!         Ok(IOMessage::user_text("input")) // read from stdin
//!     }
//! }
//!
//! struct TerminalIOPlugin;
//!
//! impl Plugin for TerminalIOPlugin {
//!     const ID: &'static str = "myapp::terminal_io";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn build(&self, server: &mut Server) {
//!         let provider = Arc::new(TerminalProvider);
//!         server.register_local(move || UserIO::new(provider.clone()));
//!     }
//! }
//! ```

use crate::ServerInfoPlugin;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use std::collections::HashMap;
#[cfg(any(test, feature = "test-utils"))]
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
#[cfg(any(test, feature = "test-utils"))]
use std::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// Interaction Metadata Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata key for the interaction type (set on request messages).
///
/// [`IOProvider`] implementations inspect this to determine how to render
/// the request and what response format to use.
pub const IO_TYPE: &str = "io_type";

/// [`IO_TYPE`] value for confirmation interactions.
///
/// Signals the provider that a yes/no answer is expected.
/// Used by [`UserIO::confirm`].
pub const IO_TYPE_CONFIRM: &str = "confirm";

/// Metadata key for the confirmation result (set on response messages).
///
/// Providers set this to [`CONFIRMED_TRUE`] or [`CONFIRMED_FALSE`]
/// after interpreting the user's response.
pub const CONFIRMED: &str = "confirmed";

/// Affirmative value for the [`CONFIRMED`] metadata key.
pub const CONFIRMED_TRUE: &str = "true";

/// Negative value for the [`CONFIRMED`] metadata key.
pub const CONFIRMED_FALSE: &str = "false";

/// A continuous stream of [`IOMessage`]s from an [`IOProvider`].
///
/// Returned by [`UserIO::stream`].
pub type IOStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<IOMessage, IOError>> + Send>>;

// ─────────────────────────────────────────────────────────────────────────────
// Message Types
// ─────────────────────────────────────────────────────────────────────────────

/// Source of an I/O message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IOSource {
    /// From a human user.
    User,
    /// From another agent, identified by agent ID.
    Agent(String),
    /// From an external service (MCP server, API, etc.).
    External(String),
    /// System-generated (timeouts, errors, internal events).
    System,
}

/// Content of an I/O message, supporting multiple modalities.
#[derive(Debug, Clone)]
pub enum IOContent {
    /// Plain text content.
    Text(String),
    /// Structured data (JSON-compatible).
    Structured(serde_json::Value),
    /// Binary data with MIME type (images, audio, files).
    Binary {
        /// MIME type of the binary data (e.g., `"image/png"`).
        mime_type: String,
        /// The raw binary data.
        data: Vec<u8>,
    },
}

/// A multi-modal message for communication between users, agents, and services.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::{IOMessage, IOContent, IOSource};
///
/// // Simple text message from user
/// let msg = IOMessage::user_text("Hello, agent!");
/// assert!(matches!(msg.source, IOSource::User));
///
/// // Structured message from another agent
/// let data = serde_json::json!({"result": 42});
/// let msg = IOMessage::from_agent("planner", IOContent::Structured(data));
/// ```
#[derive(Debug, Clone)]
pub struct IOMessage {
    /// Message content — supports multiple modalities.
    pub content: IOContent,
    /// Source of the message.
    pub source: IOSource,
    /// Optional metadata (headers, tool call IDs, session info, etc.).
    pub metadata: HashMap<String, String>,
}

impl IOMessage {
    /// Creates a new message with the given content and source.
    #[must_use]
    pub fn new(content: IOContent, source: IOSource) -> Self {
        Self {
            content,
            source,
            metadata: HashMap::new(),
        }
    }

    /// Creates a text message from a user.
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::new(IOContent::Text(text.into()), IOSource::User)
    }

    /// Creates a text message from the system.
    #[must_use]
    pub fn system_text(text: impl Into<String>) -> Self {
        Self::new(IOContent::Text(text.into()), IOSource::System)
    }

    /// Creates a message from an agent with the given content.
    #[must_use]
    pub fn from_agent(agent_id: impl Into<String>, content: IOContent) -> Self {
        Self::new(content, IOSource::Agent(agent_id.into()))
    }

    /// Creates a message from an external service with the given content.
    #[must_use]
    pub fn from_external(service: impl Into<String>, content: IOContent) -> Self {
        Self::new(content, IOSource::External(service.into()))
    }

    /// Adds a metadata entry to this message, returning self for chaining.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IOProvider Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for I/O communication providers.
///
/// Implement this for concrete communication channels (terminal, WebSocket,
/// MCP, HTTP, etc.). The provider handles the actual sending and receiving
/// of messages.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::{IOProvider, IOMessage, IOError};
///
/// struct TerminalProvider;
///
/// impl IOProvider for TerminalProvider {
///     async fn send(&self, _message: IOMessage) -> Result<(), IOError> {
///         // Write to stdout
///         Ok(())
///     }
///
///     async fn receive(&self) -> Result<IOMessage, IOError> {
///         // Read from stdin
///         Ok(IOMessage::user_text("user input"))
///     }
/// }
/// ```
pub trait IOProvider: Send + Sync + 'static {
    /// Sends a message through this provider.
    fn send(&self, message: IOMessage) -> impl Future<Output = Result<(), IOError>> + Send + '_;

    /// Receives a message from this provider.
    ///
    /// This will suspend until a message is available.
    /// Concrete implementations control the blocking behavior (polling,
    /// channel recv, stdin read, etc.).
    fn receive(&self) -> impl Future<Output = Result<IOMessage, IOError>> + Send + '_;

    /// Returns a continuous stream of messages from this provider.
    fn stream(&self) -> impl Future<Output = Result<IOStream, IOError>> + Send + '_ {
        async { Err(IOError::Unsupported("streaming not supported".into())) }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Type-Erased Provider (internal)
// ─────────────────────────────────────────────────────────────────────────────

/// Internal type-erased provider trait for object safety.
///
/// [`IOProvider`] uses RPITIT which isn't object-safe, so we erase through
/// boxed futures for dynamic dispatch in [`UserIO`].
trait ErasedProvider: Send + Sync + 'static {
    /// Sends a message through this provider (type-erased).
    fn send_erased(
        &self,
        message: IOMessage,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IOError>> + Send + '_>>;

    /// Receives a message from this provider (type-erased).
    fn receive_erased(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<IOMessage, IOError>> + Send + '_>>;

    /// Returns a continuous stream of messages (type-erased).
    fn stream_erased(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<IOStream, IOError>> + Send + '_>>;
}

/// Automatically implement object safe trait `ErasedProvider` for any `IOProvider`.
///
/// This allows us to use `Arc<dyn ErasedProvider>` in [`UserIO`] for dynamic dispatch,
impl<T: IOProvider> ErasedProvider for T {
    fn send_erased(
        &self,
        message: IOMessage,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IOError>> + Send + '_>> {
        Box::pin(self.send(message))
    }

    fn receive_erased(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<IOMessage, IOError>> + Send + '_>> {
        Box::pin(self.receive())
    }

    fn stream_erased(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<IOStream, IOError>> + Send + '_>> {
        Box::pin(self.stream())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UserIO Resource
// ─────────────────────────────────────────────────────────────────────────────

/// User I/O resource wrapping an [`IOProvider`] implementation.
///
/// Per-agent I/O resource wrapping an [`IOProvider`] implementation.
///
/// Local resource providing async send/receive for agent communication.
/// Each agent context gets its own `UserIO` instance, enabling per-agent
/// I/O isolation while optionally sharing the underlying provider via `Arc`.
///
/// Registered by concrete communication plugins (not by [`IOPlugin`] itself)
/// using [`Server::register_local`] with a factory that captures a shared
/// provider. See the [module-level documentation](self) for a full example.
///
/// # Example
///
/// ```
/// # use polaris_system::param::Res;
/// # use polaris_system::system;
/// # use polaris_core_plugins::{UserIO, IOMessage};
///
/// #[system]
/// async fn respond_to_user(user_io: Res<'_, UserIO>) {
///     let msg = IOMessage::system_text("Hello from the agent!");
///     user_io.send(msg).await.expect("send failed");
/// }
///
/// #[system]
/// async fn wait_for_input(user_io: Res<'_, UserIO>) -> IOMessage {
///     // Suspends graph execution until user responds
///     user_io.receive().await.expect("receive failed")
/// }
/// ```
pub struct UserIO {
    provider: Arc<dyn ErasedProvider>,
}

impl LocalResource for UserIO {}

impl UserIO {
    /// Creates a new `UserIO` wrapping the given provider.
    #[must_use]
    pub fn new<P: IOProvider>(provider: Arc<P>) -> Self {
        Self { provider }
    }

    /// Sends a message through the underlying provider.
    ///
    /// Takes ownership of the message. Clone before calling if you need
    /// to retain the message.
    ///
    /// # Errors
    ///
    /// Returns [`IOError`] if the send operation fails.
    pub async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.provider.send_erased(message).await
    }

    /// Receives a message from the underlying provider.
    ///
    /// This is async and will suspend until a message is available,
    /// naturally pausing graph execution.
    ///
    /// # Errors
    ///
    /// Returns [`IOError`] if the receive operation fails.
    pub async fn receive(&self) -> Result<IOMessage, IOError> {
        self.provider.receive_erased().await
    }

    /// Sends a confirmation prompt and returns whether the user accepted.
    ///
    /// This is the standard mechanism for enforcing [`ToolPermission::Confirm`] —
    /// when a tool's permission resolves to `Confirm`, the executor calls this
    /// method to obtain user approval before proceeding.
    ///
    /// Sends the `prompt` as a system text message with
    /// [`IO_TYPE`]: [`IO_TYPE_CONFIRM`] metadata, then awaits the provider's
    /// response. The `io_type` metadata convention lets [`IOProvider`]
    /// implementations render the appropriate UI (terminal `[y/N]` prompt,
    /// GUI dialog, etc.).
    ///
    /// [`ToolPermission::Confirm`]: polaris_tools::ToolPermission::Confirm
    ///
    /// Returns a [`ConfirmResponse`] containing both the parsed boolean result
    /// and the full response [`IOMessage`], so the caller can access any
    /// additional data the provider attached.
    ///
    /// # Provider Contract
    ///
    /// The [`IO_TYPE`]: [`IO_TYPE_CONFIRM`] metadata signals the [`IOProvider`]
    /// that a yes/no answer is expected. The provider is responsible for:
    ///
    /// 1. Rendering the prompt appropriately (terminal `[y/N]`, GUI dialog, etc.)
    /// 2. Interpreting the user's response (text, button click, voice, etc.)
    /// 3. Setting [`CONFIRMED`]: [`CONFIRMED_TRUE`] or [`CONFIRMED`]: [`CONFIRMED_FALSE`]
    ///    in the response message's [`metadata`](IOMessage::metadata)
    ///
    /// The response content ([`IOContent`]) is unconstrained — providers may
    /// include the raw user input, structured data, or anything else alongside
    /// the [`CONFIRMED`] metadata.
    ///
    /// If [`CONFIRMED`] metadata is absent, it is treated as denial (safe default).
    ///
    /// # Errors
    ///
    /// Returns [`IOError`] if sending the prompt or receiving the response fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use polaris_core_plugins::UserIO;
    /// # async fn example(user_io: &UserIO) {
    /// let response = user_io.confirm("Allow shell command `rm -rf build/`?").await.unwrap();
    /// if response.confirmed {
    ///     // proceed with execution
    ///     // optionally inspect response.message for extra context
    /// } else {
    ///     // abort
    /// }
    /// # }
    /// ```
    pub async fn confirm(&self, prompt: impl Into<String>) -> Result<ConfirmResponse, IOError> {
        let message = IOMessage::system_text(prompt).with_metadata(IO_TYPE, IO_TYPE_CONFIRM);
        self.send(message).await?;

        let response = self.receive().await?;
        let confirmed = response
            .metadata
            .get(CONFIRMED)
            .is_some_and(|v| v == CONFIRMED_TRUE);
        Ok(ConfirmResponse {
            confirmed,
            message: response,
        })
    }

    /// Returns a continuous stream of messages from the underlying provider.
    ///
    /// # Errors
    ///
    /// Returns [`IOError::Unsupported`] if the provider does not support streaming.
    pub async fn stream(&self) -> Result<IOStream, IOError> {
        self.provider.stream_erased().await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Confirm Response
// ─────────────────────────────────────────────────────────────────────────────

/// Response from a [`UserIO::confirm`] call.
///
/// Contains the parsed boolean result and the full response message,
/// allowing callers to access any additional data the provider attached.
#[derive(Debug, Clone)]
pub struct ConfirmResponse {
    /// Whether the user confirmed the action.
    pub confirmed: bool,
    /// The full response message from the user/provider.
    pub message: IOMessage,
}

// ─────────────────────────────────────────────────────────────────────────────
// InputBuffer Resource
// ─────────────────────────────────────────────────────────────────────────────

/// Per-agent buffer for incoming messages.
///
/// Local resource that systems use to buffer received messages.
/// Each agent context gets its own isolated buffer.
///
/// # Example
///
/// ```
/// use polaris_system::param::ResMut;
/// use polaris_system::system;
/// use polaris_core_plugins::{InputBuffer, IOMessage};
///
/// #[system]
/// async fn process_inputs(mut input: ResMut<'_, InputBuffer>) {
///     for msg in input.drain() {
///         // Process each buffered message
///         # let _ = msg;
///     }
/// }
/// ```
#[derive(Debug)]
pub struct InputBuffer {
    messages: Vec<IOMessage>,
}

impl LocalResource for InputBuffer {}

impl InputBuffer {
    /// Creates a new empty input buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Pushes a message into the buffer.
    pub fn push(&mut self, message: IOMessage) {
        self.messages.push(message);
    }

    /// Drains all messages from the buffer, returning them.
    pub fn drain(&mut self) -> Vec<IOMessage> {
        std::mem::take(&mut self.messages)
    }

    /// Clears the buffer, discarding all messages.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Returns the number of buffered messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns `true` if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// OutputBuffer Resource
// ─────────────────────────────────────────────────────────────────────────────

/// Per-agent buffer for outgoing messages.
///
/// Local resource that systems use to buffer messages for sending.
/// Each agent context gets its own isolated buffer.
///
/// # Example
///
/// ```
/// use polaris_system::param::ResMut;
/// use polaris_system::system;
/// use polaris_core_plugins::{OutputBuffer, IOMessage, IOContent};
///
/// #[system]
/// async fn prepare_response(mut output: ResMut<'_, OutputBuffer>) {
///     output.push(IOMessage::system_text("Processing complete."));
/// }
/// ```
#[derive(Debug)]
pub struct OutputBuffer {
    messages: Vec<IOMessage>,
}

impl LocalResource for OutputBuffer {}

impl OutputBuffer {
    /// Creates a new empty output buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Pushes a message into the buffer.
    pub fn push(&mut self, message: IOMessage) {
        self.messages.push(message);
    }

    /// Drains all messages from the buffer, returning them.
    pub fn drain(&mut self) -> Vec<IOMessage> {
        std::mem::take(&mut self.messages)
    }

    /// Clears the buffer, discarding all messages.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Returns the number of buffered messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns `true` if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IOPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// I/O plugin providing per-agent message buffers.
///
/// Registers [`InputBuffer`] and [`OutputBuffer`] as local resources.
/// Does **not** register [`UserIO`] — that is the responsibility of concrete
/// communication plugins that implement [`IOProvider`].
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`InputBuffer`] | Local | Per-agent incoming message buffer |
/// | [`OutputBuffer`] | Local | Per-agent outgoing message buffer |
///
/// # Dependencies
///
/// - [`ServerInfoPlugin`]
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{ServerInfoPlugin, IOPlugin};
///
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// server.add_plugins(IOPlugin);
/// server.finish();
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct IOPlugin;

impl Plugin for IOPlugin {
    const ID: &'static str = "polaris::io";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.register_local(InputBuffer::new);
        server.register_local(OutputBuffer::new);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ServerInfoPlugin>()]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error Type
// ─────────────────────────────────────────────────────────────────────────────

/// Returned by [`IOProvider::send`], [`IOProvider::receive`], and [`IOProvider::stream`].
#[derive(Debug)]
#[non_exhaustive]
pub enum IOError {
    /// The connection or channel is closed.
    Closed,
    /// The operation timed out.
    Timeout,
    /// A provider-specific error.
    Provider(String),
    /// The operation is not supported by this provider.
    Unsupported(String),
}

impl std::fmt::Display for IOError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "I/O channel closed"),
            Self::Timeout => write!(f, "I/O operation timed out"),
            Self::Provider(msg) => write!(f, "I/O provider error: {}", msg),
            Self::Unsupported(msg) => write!(f, "I/O operation unsupported: {}", msg),
        }
    }
}

impl std::error::Error for IOError {}

// ─────────────────────────────────────────────────────────────────────────────
// MockIOProvider for Testing
// ─────────────────────────────────────────────────────────────────────────────

/// Mock I/O provider for testing.
///
/// Records all sent messages and returns scripted responses for receives.
///
/// # Example
///
/// ```
/// # #[cfg(any(test, feature = "test-utils"))]
/// # {
/// use std::sync::Arc;
/// use polaris_core_plugins::{MockIOProvider, IOMessage, UserIO};
///
/// # tokio_test::block_on(async {
/// let mock = Arc::new(MockIOProvider::new());
/// mock.enqueue_receive(IOMessage::user_text("test input"));
///
/// let user_io = UserIO::new(mock.clone());
///
/// // Receive returns enqueued messages
/// let msg = user_io.receive().await.unwrap();
///
/// // Sent messages are recorded
/// user_io.send(IOMessage::system_text("response")).await.unwrap();
/// let sent = mock.take_sent();
/// assert_eq!(sent.len(), 1);
/// # });
/// # }
/// ```
#[cfg(any(test, feature = "test-utils"))]
pub struct MockIOProvider {
    sent: Mutex<Vec<IOMessage>>,
    receive_queue: Mutex<VecDeque<IOMessage>>,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockIOProvider {
    /// Creates a new mock provider with empty queues.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            receive_queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Enqueues a message to be returned by the next [`IOProvider::receive`] call.
    pub fn enqueue_receive(&self, message: IOMessage) {
        self.receive_queue
            .lock()
            .expect("MockIOProvider lock poisoned")
            .push_back(message);
    }

    /// Takes all sent messages, clearing the internal record.
    pub fn take_sent(&self) -> Vec<IOMessage> {
        std::mem::take(&mut *self.sent.lock().expect("MockIOProvider lock poisoned"))
    }

    /// Returns the number of sent messages.
    #[must_use]
    pub fn sent_count(&self) -> usize {
        self.sent
            .lock()
            .expect("MockIOProvider lock poisoned")
            .len()
    }

    /// Returns the number of remaining messages in the receive queue.
    #[must_use]
    pub fn receive_queue_len(&self) -> usize {
        self.receive_queue
            .lock()
            .expect("MockIOProvider lock poisoned")
            .len()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockIOProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl IOProvider for MockIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.sent
            .lock()
            .expect("MockIOProvider lock poisoned")
            .push(message);
        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.receive_queue
            .lock()
            .expect("MockIOProvider lock poisoned")
            .pop_front()
            .ok_or(IOError::Closed)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- IOMessage tests --

    #[test]
    fn message_user_text() {
        let msg = IOMessage::user_text("hello");
        assert!(matches!(msg.content, IOContent::Text(s) if s == "hello"));
        assert_eq!(msg.source, IOSource::User);
        assert!(msg.metadata.is_empty());
    }

    #[test]
    fn message_system_text() {
        let msg = IOMessage::system_text("status");
        assert!(matches!(msg.content, IOContent::Text(s) if s == "status"));
        assert_eq!(msg.source, IOSource::System);
    }

    #[test]
    fn message_from_agent() {
        let msg = IOMessage::from_agent("planner", IOContent::Text("plan".into()));
        assert_eq!(msg.source, IOSource::Agent("planner".into()));
    }

    #[test]
    fn message_from_external() {
        let msg = IOMessage::from_external("mcp-server", IOContent::Text("data".into()));
        assert_eq!(msg.source, IOSource::External("mcp-server".into()));
    }

    #[test]
    fn message_with_metadata() {
        let msg = IOMessage::user_text("hi")
            .with_metadata("session", "abc123")
            .with_metadata("tool_call_id", "tc_1");
        assert_eq!(msg.metadata.get("session").unwrap(), "abc123");
        assert_eq!(msg.metadata.get("tool_call_id").unwrap(), "tc_1");
    }

    #[test]
    fn message_structured_content() {
        let data = serde_json::json!({"key": "value", "count": 42});
        let msg = IOMessage::new(IOContent::Structured(data.clone()), IOSource::System);
        match &msg.content {
            IOContent::Structured(v) => assert_eq!(v, &data),
            _ => panic!("expected Structured content"),
        }
    }

    #[test]
    fn message_binary_content() {
        let msg = IOMessage::new(
            IOContent::Binary {
                mime_type: "image/png".into(),
                data: vec![0x89, 0x50, 0x4E, 0x47],
            },
            IOSource::User,
        );
        match &msg.content {
            IOContent::Binary { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data.len(), 4);
            }
            _ => panic!("expected Binary content"),
        }
    }

    // -- InputBuffer tests --

    #[test]
    fn input_buffer_push_and_drain() {
        let mut buf = InputBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);

        buf.push(IOMessage::user_text("msg1"));
        buf.push(IOMessage::user_text("msg2"));
        assert_eq!(buf.len(), 2);
        assert!(!buf.is_empty());

        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn input_buffer_clear() {
        let mut buf = InputBuffer::new();
        buf.push(IOMessage::user_text("msg"));
        buf.clear();
        assert!(buf.is_empty());
    }

    // -- OutputBuffer tests --

    #[test]
    fn output_buffer_push_and_drain() {
        let mut buf = OutputBuffer::new();
        assert!(buf.is_empty());

        buf.push(IOMessage::system_text("response1"));
        buf.push(IOMessage::system_text("response2"));
        assert_eq!(buf.len(), 2);

        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn output_buffer_clear() {
        let mut buf = OutputBuffer::new();
        buf.push(IOMessage::system_text("msg"));
        buf.clear();
        assert!(buf.is_empty());
    }

    // -- MockIOProvider tests --

    #[tokio::test]
    async fn mock_provider_send_records() {
        let mock = MockIOProvider::new();
        mock.send(IOMessage::system_text("hello")).await.unwrap();

        assert_eq!(mock.sent_count(), 1);
        let sent = mock.take_sent();
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0].content, IOContent::Text(s) if s == "hello"));

        // take_sent clears the record
        assert_eq!(mock.sent_count(), 0);
    }

    #[tokio::test]
    async fn mock_provider_receive_returns_enqueued() {
        let mock = MockIOProvider::new();
        mock.enqueue_receive(IOMessage::user_text("input1"));
        mock.enqueue_receive(IOMessage::user_text("input2"));

        assert_eq!(mock.receive_queue_len(), 2);

        let msg1 = mock.receive().await.unwrap();
        assert!(matches!(&msg1.content, IOContent::Text(s) if s == "input1"));

        let msg2 = mock.receive().await.unwrap();
        assert!(matches!(&msg2.content, IOContent::Text(s) if s == "input2"));

        // Empty queue returns Closed error
        let result = mock.receive().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn user_io_delegates_to_provider() {
        let mock = Arc::new(MockIOProvider::new());
        mock.enqueue_receive(IOMessage::user_text("test"));

        let user_io = UserIO::new(mock.clone());

        // Receive
        let msg = user_io.receive().await.unwrap();
        assert!(matches!(&msg.content, IOContent::Text(s) if s == "test"));

        // Send
        user_io.send(IOMessage::system_text("reply")).await.unwrap();
        assert_eq!(mock.sent_count(), 1);
    }

    // -- UserIO isolation tests --

    #[tokio::test]
    async fn user_io_per_agent_isolation() {
        let mock_a = Arc::new(MockIOProvider::new());
        let mock_b = Arc::new(MockIOProvider::new());
        mock_a.enqueue_receive(IOMessage::user_text("for A"));

        let io_a = UserIO::new(mock_a.clone());
        let io_b = UserIO::new(mock_b.clone());

        // A can receive its message
        let msg = io_a.receive().await.unwrap();
        assert!(matches!(&msg.content, IOContent::Text(s) if s == "for A"));

        // B has no messages — isolated provider
        let result = io_b.receive().await;
        assert!(matches!(result, Err(IOError::Closed)));

        // Sending on A doesn't appear on B's provider
        io_a.send(IOMessage::system_text("from A")).await.unwrap();
        assert_eq!(mock_a.sent_count(), 1);
        assert_eq!(mock_b.sent_count(), 0);
    }

    #[tokio::test]
    async fn user_io_shared_provider_via_arc() {
        let shared = Arc::new(MockIOProvider::new());
        shared.enqueue_receive(IOMessage::user_text("shared msg"));

        let io_a = UserIO::new(shared.clone());
        let io_b = UserIO::new(shared.clone());

        // First receive consumes from the shared queue
        let msg = io_a.receive().await.unwrap();
        assert!(matches!(&msg.content, IOContent::Text(s) if s == "shared msg"));

        // Queue is now empty for both — they share the same provider
        let result = io_b.receive().await;
        assert!(matches!(result, Err(IOError::Closed)));

        // Both sends go to the same provider
        io_a.send(IOMessage::system_text("from A")).await.unwrap();
        io_b.send(IOMessage::system_text("from B")).await.unwrap();
        assert_eq!(shared.sent_count(), 2);
    }

    // -- IOPlugin tests --

    #[test]
    fn io_plugin_registers_buffers() {
        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);
        server.add_plugins(IOPlugin);
        server.finish();

        let ctx = server.create_context();
        assert!(ctx.contains_resource::<InputBuffer>());
        assert!(ctx.contains_resource::<OutputBuffer>());
    }

    // -- UserIO::confirm tests --

    /// Helper to create a mock that responds to confirm with the given metadata.
    fn confirm_mock(confirmed: &str) -> (UserIO, Arc<MockIOProvider>) {
        let mock = Arc::new(MockIOProvider::new());
        mock.enqueue_receive(IOMessage::user_text("response").with_metadata(CONFIRMED, confirmed));
        (UserIO::new(mock.clone()), mock)
    }

    #[tokio::test]
    async fn confirm_returns_true_when_confirmed() {
        let (user_io, mock) = confirm_mock(CONFIRMED_TRUE);

        let response = user_io.confirm("Allow?").await.unwrap();
        assert!(response.confirmed);

        // Verify the sent message has correct metadata
        let sent = mock.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].metadata.get(IO_TYPE).unwrap(), IO_TYPE_CONFIRM);
    }

    #[tokio::test]
    async fn confirm_returns_false_when_denied() {
        let (user_io, _mock) = confirm_mock(CONFIRMED_FALSE);

        let response = user_io.confirm("Allow?").await.unwrap();
        assert!(!response.confirmed);
    }

    #[tokio::test]
    async fn confirm_returns_false_when_metadata_absent() {
        let mock = Arc::new(MockIOProvider::new());
        mock.enqueue_receive(IOMessage::user_text("whatever"));
        let user_io = UserIO::new(mock.clone());

        let response = user_io.confirm("Allow?").await.unwrap();
        assert!(!response.confirmed);
    }

    #[tokio::test]
    async fn confirm_preserves_response_message() {
        let mock = Arc::new(MockIOProvider::new());
        mock.enqueue_receive(
            IOMessage::user_text("I approve")
                .with_metadata(CONFIRMED, CONFIRMED_TRUE)
                .with_metadata("reason", "looks safe"),
        );
        let user_io = UserIO::new(mock.clone());

        let response = user_io.confirm("Allow?").await.unwrap();
        assert!(response.confirmed);
        assert_eq!(
            response.message.metadata.get("reason").unwrap(),
            "looks safe"
        );
    }

    #[tokio::test]
    async fn confirm_propagates_receive_error() {
        let mock = Arc::new(MockIOProvider::new());
        // No messages enqueued — receive returns IOError::Closed
        let user_io = UserIO::new(mock);

        let result = user_io.confirm("Allow?").await;
        assert!(result.is_err());
    }

    // -- IOError tests --

    #[test]
    fn io_error_display() {
        assert_eq!(IOError::Closed.to_string(), "I/O channel closed");
        assert_eq!(IOError::Timeout.to_string(), "I/O operation timed out");
        assert_eq!(
            IOError::Provider("custom".into()).to_string(),
            "I/O provider error: custom"
        );
        assert_eq!(
            IOError::Unsupported("feature".into()).to_string(),
            "I/O operation unsupported: feature"
        );
    }
}
