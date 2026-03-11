//! `ReAct` (Reasoning + Acting) agent definition.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │  ReAct Loop                                             │
//! │                                                         │
//! │  ┌─────┐   ┌──────────────┐   ┌──────────────┐          │
//! │  │ Act │──▶│ Has tools?   │──▶│ ExecuteTools │──┐       │
//! │  └─────┘   └──────┬───────┘   └──────────────┘  │       │
//! │                   │ no                          │       │
//! │                   ▼                             │       │
//! │              ┌──────────┐                       │       │
//! │              │ Finalize │                       │       │
//! │              └──────────┘                       │       │
//! │                                                 │       │
//! │  ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐          │       │
//! │    Error Handler (all fallible nodes)           │       │
//! │  │ ┌─────────┐                       │          │       │
//! │    │ Recover │   (log + respond)                │       │
//! │  │ └─────────┘                       │          │       │
//! │  └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘          │       │
//! │                     ◀───────────────────────────┘       │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Each iteration makes a single LLM call with tools available. The model
//! decides whether to call tools or respond with text. Tool results are
//! user-role messages, so the conversation always alternates correctly:
//!
//! ```text
//! User → Assistant (text + tool_calls) → User (tool_results) → Assistant → …
//! ```

use super::config::AgentConfig;
use super::context::ContextManager;
use super::state::ReactState;

use polaris::agent::{Agent, SetupError};
use polaris::graph::{CaughtError, Graph};
use polaris::models::ModelRegistry;
use polaris::models::llm::LlmResponse;
use polaris::models::llm::{Llm, Message, ToolResultContent, UserBlock};
use polaris::plugins::{IOContent, IOMessage, IOSource, InputBuffer, PersistenceAPI, UserIO};
use polaris::prelude::Out;
use polaris::system::param::{ErrOut, Res, ResMut, SystemContext};
use polaris::system::plugin::{Plugin, Version};
use polaris::system::prelude::SystemError;
use polaris::system::resource::LocalResource;
use polaris::system::server::Server;
use polaris::system::system;
use polaris::tools::{LlmReasonExt, LlmRequestBuilderExt, ToolRegistry};
use std::ops::Deref;

/// Wrapper for the current LLM instance used by the agent.
#[derive(Clone)]
pub struct AgentLlm(Llm);

impl Deref for AgentLlm {
    type Target = Llm;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl LocalResource for AgentLlm {}

/// Plugin that registers the `ReAct` agent's local resources.
pub struct ReActPlugin;

impl Plugin for ReActPlugin {
    const ID: &'static str = "examples::react";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.register_local(ContextManager::default);
        server.register_local(ReactState::default);
    }

    fn ready(&self, server: &mut Server) {
        // If a PersistenceAPI is available, register ContextManager for persistence.
        if let Some(api) = server.api::<PersistenceAPI>() {
            api.register::<ContextManager>(Self::ID);
        }
    }
}

const SYSTEM_PROMPT: &str = "\
You are a helpful assistant that follows the ReAct pattern (Reason + Act).

Based on the conversation, you MUST:
1. REASON: Write your reasoning as text BEFORE calling any tools.
   Explain what you know, what you still need, and why you are choosing your next action.
2. ACTION: Either call a tool to gather information, or respond with your final answer.
3. OBSERVATION: After receiving tool results, start your next turn with a new REASON.

Never call a tool without first explaining your reasoning in text.";

/// Helper to send a trace message via `UserIO`.
async fn send_trace(user_io: &UserIO, text: impl Into<String>) {
    let msg =
        IOMessage::from_agent("react", IOContent::Text(text.into())).with_metadata("type", "trace");
    let _ = user_io.send(msg).await;
}

/// Helper to send an error message via `UserIO`.
async fn send_error(user_io: &UserIO, text: impl Into<String>) {
    let msg = IOMessage::new(IOContent::Text(text.into()), IOSource::System)
        .with_metadata("type", "error");
    let _ = user_io.send(msg).await;
}

/// Receive user input from the input buffer and add to conversation history.
#[system]
async fn receive_user_input(
    mut input_buffer: ResMut<InputBuffer>,
    mut context: ResMut<ContextManager>,
) {
    for message in input_buffer.drain() {
        if let IOContent::Text(text) = message.content {
            context.push(Message::user(text));
        }
    }
}

/// Initialize the agent loop.
async fn init_loop() -> ReactState {
    ReactState { is_complete: false }
}

/// Single LLM call with tools available. The model decides whether to call
/// tools or respond with text — no separate reasoning step needed.
#[system]
async fn act(
    mut context: ResMut<ContextManager>,
    llm: Res<AgentLlm>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
) -> Result<LlmResponse, SystemError> {
    let messages = context.messages.clone();

    let response = llm
        .builder()
        .with_registry(&tool_registry)
        .system(SYSTEM_PROMPT)
        .messages(messages)
        .reason()
        .await
        .map_err(|err| SystemError::ExecutionError(err.to_string()))?;

    context.push(Message::Assistant {
        id: None,
        content: response.content.clone(),
    });

    let reasoning = response.text();
    if !reasoning.is_empty() {
        send_trace(&user_io, format!("\n[REASON] {reasoning}")).await;
    }
    for call in response.tool_calls() {
        send_trace(
            &user_io,
            format!(
                "\n[Action] {}({})",
                call.function.name, call.function.arguments
            ),
        )
        .await;
    }

    Ok(response)
}

/// Execute tool calls from the LLM response and add results to history.
///
/// All tool results are collected into a single user message with multiple
/// content blocks. The API requires every tool_use block in an assistant
/// message to have a corresponding toolResult in the next user message.
#[system]
async fn execute_tools(
    decision: Out<LlmResponse>,
    mut context: ResMut<ContextManager>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
) -> Result<(), SystemError> {
    let mut result_blocks = Vec::new();

    for tool_call in decision.tool_calls() {
        let block = match tool_registry
            .execute(&tool_call.function.name, &tool_call.function.arguments)
            .await
        {
            Ok(value) => {
                let output = value
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| value.to_string());
                send_trace(&user_io, format!("\n[Observation] {output}")).await;
                UserBlock::tool_result(&tool_call.id, ToolResultContent::Text(output))
            }
            Err(err) => {
                let output = err.to_string();
                send_error(&user_io, format!("\n[Tool Error] {output}")).await;
                UserBlock::tool_error(&tool_call.id, ToolResultContent::Text(output))
            }
        };
        result_blocks.push(block);
    }

    // Single user message with all tool results — maintains alternation
    // and satisfies the API requirement that all tool_use IDs are answered.
    context.push(Message::User {
        content: result_blocks,
    });

    Ok(())
}

/// Output the final text response to the user.
#[system]
async fn finalize(decision: Out<LlmResponse>, user_io: Res<UserIO>) -> ReactState {
    let text = decision.text();
    let msg = IOMessage::from_agent("react", IOContent::Text(format!("\n{text}")));
    let _ = user_io.send(msg).await;

    ReactState { is_complete: true }
}

/// Handle a caught error: log it, add to context as a user message so the
/// model can see what went wrong, then generate a graceful response.
#[system]
async fn recover(
    error: ErrOut<CaughtError>,
    mut context: ResMut<ContextManager>,
    llm: Res<AgentLlm>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
) -> ReactState {
    let error_text = format!(
        "System '{}' failed with error: {}",
        error.system_name, error.message,
    );
    let error_text = format!(
        "{} (node ID: {}, duration: {:?}, kind: {})",
        error_text, error.node_id, error.duration, error.kind
    );
    send_error(&user_io, format!("\n[System Error] {error_text}")).await;
    context.push(Message::user(format!("[System Error] {error_text}")));

    let messages = context.messages.clone();
    let builder = llm
        .builder()
        .system(SYSTEM_PROMPT)
        .messages(messages)
        .with_registry(&tool_registry);

    match builder.generate().await {
        Ok(response) => {
            let text = response.text();
            let msg = IOMessage::from_agent("react", IOContent::Text(format!("\n{text}")));
            let _ = user_io.send(msg).await;
            context.push(Message::assistant(text));
        }
        Err(err) => {
            send_error(&user_io, format!("\nLLM error: {err}")).await;
            let msg = IOMessage::from_agent(
                "react",
                IOContent::Text("I encountered an error processing your request.".to_string()),
            );
            let _ = user_io.send(msg).await;
        }
    }

    ReactState { is_complete: true }
}

/// `ReAct` agent implementing the Reasoning + Acting pattern.
///
/// Uses a single LLM call per iteration — the model's own output structure
/// (tool calls present or absent) drives the control flow.
#[derive(Debug, Clone, Default)]
pub struct ReActAgent;

impl ReActAgent {
    /// Stable agent name, accessible without an instance.
    pub const NAME: &'static str = "ReActAgent";
}

impl Agent for ReActAgent {
    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        let model_id = ctx
            .get_resource::<AgentConfig>()
            .map_err(SetupError::new)?
            .model_id
            .clone();
        let llm = ctx
            .get_resource::<ModelRegistry>()
            .map_err(SetupError::new)?
            .llm(&model_id)
            .map_err(SetupError::new)?;
        ctx.insert(AgentLlm(llm));
        Ok(())
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(receive_user_input);
        graph.add_system(init_loop);

        graph.add_loop::<ReactState, _, _>(
            "react_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(act);
                g.add_conditional_branch::<LlmResponse, _, _, _>(
                    "has_tool_calls",
                    |response| response.has_tool_calls(),
                    |tool_branch| {
                        tool_branch.add_system(execute_tools);
                    },
                    |done_branch| {
                        done_branch.add_system(finalize);
                    },
                );
                g.add_error_handler(|h| {
                    h.add_system(recover);
                });
            },
        );
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }
}
