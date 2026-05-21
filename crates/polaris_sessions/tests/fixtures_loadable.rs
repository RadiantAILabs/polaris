//! Verifies the JSON session fixtures under `tests/fixtures/sessions/` are
//! loadable through [`FileStore`] and parse into [`SessionData`].
//!
//! These fixtures are intentionally hand-rolled (not produced by a running
//! server) so they double as copy-pasteable starting points for tests that
//! want pre-seeded session state without having to drive a full agent.

#![cfg(feature = "file-store")]

use polaris_sessions::store::{SessionId, SessionStore};
use polaris_sessions::{FileStore, SessionData};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sessions")
}

#[tokio::test]
async fn list_returns_all_fixture_sessions() {
    let store = FileStore::new(fixtures_dir());
    let mut ids: Vec<String> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    ids.sort();

    assert_eq!(
        ids,
        vec![
            "counter-midrun".to_owned(),
            "fresh".to_owned(),
            "long-running".to_owned(),
            "react-with-tools".to_owned(),
        ],
    );
}

#[tokio::test]
async fn fresh_fixture_has_no_resources() {
    let data = load("fresh").await;
    assert_eq!(data.agent_type, "ReActAgent");
    assert_eq!(data.turn_number, 0);
    assert!(data.resources.is_empty());
}

#[tokio::test]
async fn counter_fixture_carries_counter_resource() {
    let data = load("counter-midrun").await;
    assert_eq!(data.agent_type, "CounterAgent");
    assert_eq!(data.turn_number, 7);
    let entry = data
        .resources
        .iter()
        .find(|r| r.storage_key == "Counter")
        .expect("Counter entry must be present");
    assert_eq!(entry.plugin_id, "examples::counter");
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.data["value"].as_u64(), Some(7));
}

#[tokio::test]
async fn react_fixture_has_tool_call_history() {
    let data = load("react-with-tools").await;
    assert_eq!(data.agent_type, "ReActAgent");
    let context = data
        .resources
        .iter()
        .find(|r| r.storage_key == "ContextManager")
        .expect("ContextManager entry must be present");
    let messages = context.data["messages"]
        .as_array()
        .expect("messages must be an array");
    assert!(messages.len() >= 4, "expected a multi-turn conversation");

    // At least one assistant tool_call and one user tool_result should appear.
    let saw_tool_call = messages.iter().any(|m| {
        m["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_call"))
    });
    let saw_tool_result = messages.iter().any(|m| {
        m["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
    });
    assert!(saw_tool_call, "fixture should include a tool_call block");
    assert!(
        saw_tool_result,
        "fixture should include a tool_result block"
    );
}

#[tokio::test]
async fn long_running_fixture_has_multiple_resources() {
    let data = load("long-running").await;
    assert_eq!(data.turn_number, 42);
    assert!(
        data.resources.len() >= 2,
        "long-running fixture should exercise multiple resources"
    );
    let keys: Vec<&str> = data
        .resources
        .iter()
        .map(|r| r.storage_key.as_str())
        .collect();
    assert!(keys.contains(&"ContextManager"));
    assert!(keys.contains(&"AgentNotes"));
}

async fn load(name: &str) -> SessionData {
    let store = FileStore::new(fixtures_dir());
    store
        .load(&SessionId::from_string(name))
        .await
        .unwrap_or_else(|err| panic!("loading fixture '{name}' failed: {err}"))
        .unwrap_or_else(|| panic!("fixture '{name}' did not exist"))
}
