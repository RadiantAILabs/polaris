//! Interactive CLI REPL for the ReAct agent.
//!
//! Multi-turn conversational interface that maintains conversation history
//! across turns, with session persistence via [`FileStore`].
//!
//! # Usage
//!
//! ```bash
//! cargo run -p examples --bin cli -- <working_dir> [--session <id>]
//! ```
//!
//! # Commands
//!
//! - `/help` — Show available commands
//! - `/history` — Show conversation history
//! - `/clear` — Clear conversation history
//! - `/save` — Save session to disk
//! - `/info` — Show session info
//! - `/sessions` — List all saved sessions
//! - `/rollback <turn>` — Rollback to a checkpoint
//! - `/exit` or `/quit` — Exit the REPL

use examples::plugins::{FileToolsConfig, FileToolsPlugin, TerminalIOPlugin};
use examples::react_agent::{AgentConfig, ContextManager, ReActAgent, ReActPlugin, ReactState};
use polaris::models::AnthropicPlugin;
use polaris::models::llm::{AssistantBlock, Message, UserBlock};
use polaris::plugins::{IOMessage, InputBuffer, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{
    AgentTypeId, FileStore, SessionId, SessionInfo, SessionsAPI, SessionsPlugin,
};
use polaris::{
    graph::{DevToolsPlugin, GraphExecutor},
    models::ModelsPlugin,
    plugins::{IOPlugin, ServerInfoPlugin, TracingPlugin},
    system::server::Server,
    tools::ToolsPlugin,
};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

// ANSI style constants
const STYLE_DIM: &str = "\x1b[2m";
const STYLE_BOLD: &str = "\x1b[1m";
const STYLE_RED: &str = "\x1b[31m";
const STYLE_GREEN: &str = "\x1b[32m";
const STYLE_RESET: &str = "\x1b[0m";

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spawns a spinner on stderr that animates until the returned sender is dropped or signalled.
fn spawn_spinner() -> oneshot::Sender<()> {
    let (tx, mut rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut i = 0usize;
        loop {
            let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
            // Write frame, flush, then wait or break
            eprint!("\r  {STYLE_DIM}{frame} Thinking...{STYLE_RESET}");
            let _ = std::io::stderr().flush();

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(80)) => {}
                _ = &mut rx => break,
            }
            i += 1;
        }
        // Clear the spinner line
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });
    tx
}

fn print_help() {
    eprintln!("{STYLE_DIM}Commands:");
    eprintln!("  /help           — Show this help message");
    eprintln!("  /history        — Show conversation history");
    eprintln!("  /clear          — Clear conversation history");
    eprintln!("  /save           — Save session to disk");
    eprintln!("  /info           — Show session info");
    eprintln!("  /sessions       — List all saved sessions");
    eprintln!("  /rollback <n>   — Rollback to checkpoint at turn <n>");
    eprintln!("  /exit           — Exit the REPL{STYLE_RESET}");
}

/// Extracts text content from user message blocks.
fn extract_user_text(content: &[UserBlock]) -> String {
    content
        .iter()
        .filter_map(|b| {
            if let UserBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extracts text content from assistant message blocks.
fn extract_assistant_text(content: &[AssistantBlock]) -> String {
    content
        .iter()
        .filter_map(|b| {
            if let AssistantBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_history(ctx: &polaris::system::param::SystemContext<'_>) {
    let cm = ctx
        .get_resource::<ContextManager>()
        .expect("ContextManager missing");
    if cm.messages.is_empty() {
        eprintln!("{STYLE_DIM}  (no messages){STYLE_RESET}");
        return;
    }
    for (i, msg) in cm.messages.iter().enumerate() {
        match msg {
            Message::User { content } => {
                let text = extract_user_text(content);
                eprintln!("{STYLE_DIM}  [{i}] User: {text}{STYLE_RESET}");
            }
            Message::Assistant { content, .. } => {
                let text = extract_assistant_text(content);
                if !text.is_empty() {
                    eprintln!("{STYLE_DIM}  [{i}] Assistant: {text}{STYLE_RESET}");
                } else {
                    eprintln!("{STYLE_DIM}  [{i}] Assistant: (tool call){STYLE_RESET}");
                }
            }
        }
    }
}

/// Creates a [`GraphExecutor`] configured for the CLI agent.
fn make_executor() -> GraphExecutor {
    GraphExecutor::new().with_default_max_iterations(10)
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    // Parse arguments: <working_dir> [--session <id>]
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cli <working_dir> [--session <id>]");
        std::process::exit(1);
    }

    let working_dir = PathBuf::from(&args[1])
        .canonicalize()
        .unwrap_or_else(|err| {
            eprintln!("Error: {err}");
            std::process::exit(1);
        });

    let session_id_str = args
        .iter()
        .position(|a| a == "--session")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "default".to_string());

    let agent_config = AgentConfig::new("claude-sonnet-4-6");
    let file_tools_config = FileToolsConfig::new(&working_dir);

    // Build server
    let mut server = Server::new();
    server
        .add_plugins(TracingPlugin::default().with_env_filter("polaris=debug,rustyline=warn"))
        .add_plugins(ServerInfoPlugin)
        .add_plugins(IOPlugin)
        .add_plugins(TerminalIOPlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
        .add_plugins(ToolsPlugin)
        .add_plugins(FileToolsPlugin::new(file_tools_config))
        .add_plugins(PersistencePlugin)
        .add_plugins(ReActPlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new("data"))))
        .add_plugins(DevToolsPlugin::new().with_event_tracing());

    server.finish();

    // Set serializers after all plugins have registered their resources.
    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    if let Err(err) = sessions.register_agent(ReActAgent) {
        eprintln!("{STYLE_RED}{err}{STYLE_RESET}");
        std::process::exit(1);
    }

    // Try to resume an existing session, or create a new one.
    let session_id = SessionId::from_string(&session_id_str);
    let agent_type = AgentTypeId::from_name(ReActAgent::NAME);

    match sessions
        .resume_session_with_executor(&server, &session_id, make_executor(), |ctx| {
            ctx.insert(agent_config.clone());
        })
        .await
    {
        Ok(()) => {
            eprintln!("{STYLE_DIM}Resumed session: {session_id_str}{STYLE_RESET}");
        }
        Err(_) => {
            sessions
                .create_session_with_executor(
                    &server,
                    &session_id,
                    &agent_type,
                    make_executor(),
                    |ctx| {
                        ctx.insert(agent_config.clone());
                    },
                )
                .unwrap();
            eprintln!("{STYLE_DIM}Created new session: {session_id_str}{STYLE_RESET}");
        }
    }

    // Welcome banner
    eprintln!("{STYLE_BOLD}Polaris ReAct Agent{STYLE_RESET}");
    eprintln!(
        "{STYLE_DIM}Session: {session_id_str} | {}/{STYLE_RESET}",
        working_dir.display()
    );
    eprintln!("{STYLE_DIM}Type /help for commands, Ctrl+D or /exit to quit.{STYLE_RESET}");
    eprintln!();

    // Initialize rustyline editor
    let mut rl = DefaultEditor::new().expect("Failed to create readline editor");

    // REPL loop
    loop {
        let readline = rl.readline(&format!("{STYLE_BOLD}> {STYLE_RESET}"));

        match readline {
            Ok(line) => {
                let trimmed = line.trim();

                if trimmed.is_empty() {
                    continue;
                }

                let _ = rl.add_history_entry(trimmed);

                // Handle commands
                if trimmed.starts_with('/') {
                    match trimmed {
                        "/exit" | "/quit" => break,
                        "/help" => {
                            print_help();
                            continue;
                        }
                        "/history" => {
                            sessions
                                .with_context(&session_id, |ctx| print_history(ctx))
                                .await
                                .unwrap();
                            continue;
                        }
                        "/clear" => {
                            sessions
                                .with_context(&session_id, |ctx| {
                                    ctx.insert(ContextManager::default());
                                })
                                .await
                                .unwrap();
                            eprintln!("{STYLE_DIM}  Conversation cleared.{STYLE_RESET}");
                            continue;
                        }
                        "/save" => {
                            match sessions.save_session(&session_id).await {
                                Ok(()) => {
                                    eprintln!("{STYLE_GREEN}  Session saved.{STYLE_RESET}");
                                }
                                Err(err) => {
                                    eprintln!("{STYLE_RED}  Save failed: {err}{STYLE_RESET}");
                                }
                            }
                            continue;
                        }
                        "/info" => {
                            sessions
                                .with_context(&session_id, |ctx| {
                                    if let Ok(info) = ctx.get_resource::<SessionInfo>() {
                                        eprintln!(
                                            "{STYLE_DIM}  Session: {}",
                                            info.session_id.as_str()
                                        );
                                        eprintln!("  Turn:    {}{STYLE_RESET}", info.turn_number);
                                    } else {
                                        eprintln!("{STYLE_DIM}  Session: {session_id_str}");
                                        eprintln!("  Turn:    0 (no turns executed){STYLE_RESET}");
                                    }
                                })
                                .await
                                .unwrap();
                            continue;
                        }
                        "/sessions" => {
                            match sessions.list_sessions().await {
                                Ok(ids) => {
                                    if ids.is_empty() {
                                        eprintln!("{STYLE_DIM}  (no saved sessions){STYLE_RESET}");
                                    } else {
                                        for id in &ids {
                                            let marker =
                                                if id == &session_id { " (current)" } else { "" };
                                            eprintln!(
                                                "{STYLE_DIM}  {}{marker}{STYLE_RESET}",
                                                id.as_str()
                                            );
                                        }
                                    }
                                }
                                Err(err) => {
                                    eprintln!(
                                        "{STYLE_RED}  Failed to list sessions: {err}{STYLE_RESET}"
                                    );
                                }
                            }
                            continue;
                        }
                        _ if trimmed.starts_with("/rollback") => {
                            let turn: Option<u32> = trimmed
                                .strip_prefix("/rollback")
                                .and_then(|s| s.trim().parse().ok());

                            match turn {
                                Some(t) => match sessions.rollback(&session_id, t).await {
                                    Ok(()) => {
                                        // Re-inject config and re-run setup for non-persisted resources.
                                        sessions
                                            .with_context(&session_id, |ctx| {
                                                ctx.insert(agent_config.clone());
                                            })
                                            .await
                                            .unwrap();
                                        sessions.setup_session(&session_id).await.unwrap();
                                        eprintln!(
                                            "{STYLE_GREEN}  Rolled back to turn {t}.{STYLE_RESET}"
                                        );
                                    }
                                    Err(err) => {
                                        eprintln!(
                                            "{STYLE_RED}  Rollback failed: {err}{STYLE_RESET}"
                                        );
                                    }
                                },
                                None => {
                                    eprintln!("{STYLE_DIM}  Usage: /rollback <turn>{STYLE_RESET}");
                                }
                            }
                            continue;
                        }
                        _ => {
                            eprintln!(
                                "{STYLE_DIM}  Unknown command. Type /help for help.{STYLE_RESET}"
                            );
                            continue;
                        }
                    }
                }

                // Execute a turn with spinner
                let spinner = spawn_spinner();
                let result = sessions
                    .process_turn_with(&server, &session_id, |ctx| {
                        ctx.get_resource_mut::<InputBuffer>()
                            .expect("InputBuffer missing")
                            .push(IOMessage::user_text(trimmed));
                        ctx.insert(ReactState::default());
                        ctx.clear_outputs();
                    })
                    .await;
                drop(spinner);
                // Yield briefly so the spinner task clears the line
                tokio::task::yield_now().await;

                match result {
                    Ok(result) => {
                        eprintln!(
                            "{STYLE_DIM}  ({} nodes, {:.2}s){STYLE_RESET}\n",
                            result.nodes_executed,
                            result.duration.as_secs_f64()
                        );
                    }
                    Err(err) => {
                        eprintln!("{STYLE_RED}Execution error: {err}{STYLE_RESET}\n");
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                eprintln!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("{STYLE_RED}Error: {err}{STYLE_RESET}");
                break;
            }
        }
    }

    // Save session on exit.
    if let Err(err) = sessions.save_session(&session_id).await {
        eprintln!("{STYLE_RED}Failed to save session on exit: {err}{STYLE_RESET}");
    }
}
