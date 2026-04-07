//! Terminal I/O provider for CLI-based agent interaction.
//!
//! Provides [`TerminalIOProvider`] which implements [`IOProvider`] for stdin/stdout
//! communication, and [`TerminalIOPlugin`] which registers it as the [`UserIO`] local resource.
//!
//! Supports the `io_type` metadata convention: when a message is sent with
//! `io_type: confirm`, the next `receive()` appends `[y/N]` to the prompt,
//! reads a line from stdin, and sets `confirmed: "true"` or `"false"` in
//! the response metadata.

use polaris::plugins::{
    CONFIRMED, CONFIRMED_FALSE, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOError,
    IOMessage, IOProvider, IOSource, UserIO,
};
use polaris::system::api::API;
use polaris::system::plugin::{Plugin, Version};
use polaris::system::server::Server;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ANSI style constants
const STYLE_DIM: &str = "\x1b[2m";
const STYLE_RED: &str = "\x1b[31m";
const STYLE_YELLOW: &str = "\x1b[33m";
const STYLE_RESET: &str = "\x1b[0m";

/// I/O provider that reads from stdin and writes to stdout/stderr.
///
/// Routes messages based on type and source:
/// - stderr for `"type": "trace"`, `"type": "error"` and system messages
/// - stdout for agent messages
///
/// Handles `io_type` metadata to provide interaction-appropriate rendering:
/// - `io_type: confirm` — renders prompt with `[y/N]`, parses response into
///   `confirmed` metadata
///
/// # Warning
///
/// **Not safe for concurrent use.** Interleaved `send()`/`receive()` from
/// multiple agents sharing the same provider will produce incorrect results.
/// Use a per-agent provider or a queue-based implementation for multi-agent
/// scenarios.
#[derive(Debug)]
pub struct TerminalIOProvider {
    /// Tracks the `io_type` from the last `send()` so `receive()` can
    /// format the response appropriately.
    last_io_type: Mutex<Option<String>>,
    /// Pre-loaded messages returned before falling back to stdin.
    queue: Mutex<VecDeque<IOMessage>>,
}

impl TerminalIOProvider {
    /// Creates a new terminal I/O provider.
    pub fn new() -> Self {
        Self {
            last_io_type: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Enqueues a message to be returned by the next [`IOProvider::receive`] call.
    ///
    /// Messages in the queue are drained first, before falling back to stdin.
    /// This allows external code (e.g., the CLI REPL) to inject user messages
    /// through the same [`UserIO`] protocol the agent uses.
    pub fn push(&self, message: IOMessage) {
        self.queue
            .lock()
            .expect("TerminalIOProvider lock poisoned")
            .push_back(message);
    }
}

impl IOProvider for TerminalIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        // Track io_type for the next receive() call
        let io_type = message.metadata.get(IO_TYPE).cloned();
        *self
            .last_io_type
            .lock()
            .expect("TerminalIOProvider lock poisoned") = io_type;

        let is_trace = message.metadata.get("type").is_some_and(|v| v == "trace");
        let is_error = message.metadata.get("type").is_some_and(|v| v == "error");
        let is_confirm = message
            .metadata
            .get(IO_TYPE)
            .is_some_and(|v| v == IO_TYPE_CONFIRM);

        let text = match &message.content {
            IOContent::Text(s) => s.clone(),
            IOContent::Structured(v) => v.to_string(),
            IOContent::Binary { mime_type, data } => {
                format!("[binary: {mime_type}, {} bytes]", data.len())
            }
        };

        if is_confirm {
            // Confirm prompts on stderr with [y/N] suffix, yellow for visibility
            eprint!("{STYLE_YELLOW}{text} [y/N]: {STYLE_RESET}");
        } else if is_trace {
            // Trace messages on stderr, indented and dimmed
            eprintln!("  {STYLE_DIM}{text}{STYLE_RESET}");
        } else if is_error {
            // Error messages on stderr, indented and red
            eprintln!("  {STYLE_RED}{text}{STYLE_RESET}");
        } else if matches!(message.source, IOSource::System) {
            // System messages on stderr
            eprintln!("{text}");
        } else {
            // Agent messages on stdout
            println!("{text}");
        }

        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        // Drain queued messages first (e.g., user input injected by the CLI).
        {
            let mut queue = self.queue.lock().expect("TerminalIOProvider lock poisoned");
            if let Some(message) = queue.pop_front() {
                return Ok(message);
            }
        }

        // No queued messages — fall back to stdin (for confirmations, etc.).
        let io_type = self
            .last_io_type
            .lock()
            .expect("TerminalIOProvider lock poisoned")
            .take();

        tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|err| IOError::Provider(err.to_string()))?;

            if line.is_empty() {
                return Err(IOError::Closed);
            }

            let trimmed = line.trim_end().to_string();
            let mut message = IOMessage::user_text(trimmed.clone());

            // Interpret response based on the interaction type
            if io_type.as_deref() == Some(IO_TYPE_CONFIRM) {
                let is_yes = matches!(
                    trimmed.trim().to_lowercase().as_str(),
                    "y" | "yes" | "true" | "1"
                );
                message = message.with_metadata(
                    CONFIRMED,
                    if is_yes {
                        CONFIRMED_TRUE
                    } else {
                        CONFIRMED_FALSE
                    },
                );
            }

            Ok(message)
        })
        .await
        .map_err(|err| IOError::Provider(err.to_string()))?
    }
}

/// API handle for pushing messages into the [`TerminalIOProvider`] queue.
///
/// Registered by [`TerminalIOPlugin`] and accessed via
/// [`Server::api::<TerminalIO>()`](polaris::system::server::Server::api).
#[derive(Debug)]
pub struct TerminalIO {
    provider: Arc<TerminalIOProvider>,
}

impl API for TerminalIO {}

impl TerminalIO {
    /// Push a message into the provider's queue for the next
    /// [`IOProvider::receive`] call.
    pub fn push(&self, message: IOMessage) {
        self.provider.push(message);
    }
}

/// Plugin that registers [`TerminalIOProvider`] as the [`UserIO`] local
/// resource and exposes a [`TerminalIO`] API for injecting messages.
pub struct TerminalIOPlugin;

impl Plugin for TerminalIOPlugin {
    const ID: &'static str = "examples::terminal_io";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let provider = Arc::new(TerminalIOProvider::new());
        let provider_for_factory = provider.clone();
        server.register_local(move || UserIO::new(provider_for_factory.clone()));
        server.insert_api(TerminalIO { provider });
    }
}
