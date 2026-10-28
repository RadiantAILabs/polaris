//! Integration tests for the `polaris_shell` crate.

use polaris_shell::{
    ChannelSink, ShellCancel, ShellConfig, ShellEvent, ShellPlugin, ShellSinkHandle,
};
use polaris_system::param::{Res, SystemParam};
use polaris_system::server::Server;
use polaris_tools::{ToolContext, ToolError, ToolRegistry, ToolsPlugin};
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────
// Plugin registration
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_registers_run_command_tool() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).expect("ToolRegistry should be accessible");

    assert!(
        registry.has("run_command"),
        "run_command tool should be registered"
    );
    assert_eq!(registry.names().len(), 1, "should have exactly one tool");
}

// ─────────────────────────────────────────────────────────────────────
// Tool definition schema
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_definition_has_correct_schema() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();

    let defs = registry.definitions();
    assert_eq!(defs.len(), 1);

    let def = &defs[0];
    assert_eq!(def.name, "run_command");
    assert!(
        def.description.contains("shell command"),
        "description should mention shell command, got: {}",
        def.description
    );

    // Check parameters
    let props = def.parameters["properties"]
        .as_object()
        .expect("should have properties");
    assert!(
        props.contains_key("command"),
        "should have 'command' parameter"
    );
    assert!(
        props.contains_key("working_dir"),
        "should have 'working_dir' parameter"
    );
    assert!(
        props.contains_key("timeout_secs"),
        "should have 'timeout_secs' parameter"
    );

    // command should be required, others optional
    let required = def.parameters["required"]
        .as_array()
        .expect("should have required array");
    assert!(
        required.contains(&json!("command")),
        "command should be required"
    );
    assert!(
        !required.contains(&json!("working_dir")),
        "working_dir should not be required"
    );
    assert!(
        !required.contains(&json!("timeout_secs")),
        "timeout_secs should not be required"
    );

    // Host-only `#[context]` parameters must never be advertised to the model.
    for host_only in ["cancel", "sink"] {
        assert!(
            !props.contains_key(host_only),
            "`{host_only}` is a host-only context value and must stay out of \
             the LLM-facing schema, got: {props:?}"
        );
    }
    assert_eq!(
        props.len(),
        3,
        "the schema should expose exactly command/working_dir/timeout_secs, got: {props:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Tool execution via registry
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_execution_via_registry() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["echo *".into()])
            .disable_sandbox(),
    ));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();

    let result: Value = registry
        .execute("run_command", &json!({"command": "echo integration-test"}))
        .await
        .expect("should execute successfully");

    // ExecutionResult is serialized to JSON
    let stdout = result["stdout"].as_str().expect("should have stdout field");
    assert!(
        stdout.contains("integration-test"),
        "stdout should contain 'integration-test', got: {stdout}"
    );
    assert_eq!(result["exit_code"], json!(0));
    assert_eq!(result["timed_out"], json!(false));
}

#[tokio::test]
async fn tool_context_streams_without_changing_the_tool_schema() {
    let temp = tempfile::TempDir::new().unwrap();
    let release = temp.path().join("release");
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".into()])
            .disable_sandbox(),
    ));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();
    let (sink, mut events) = ChannelSink::new(NonZeroUsize::new(8).unwrap());
    let tool_context = ToolContext::new().with(ShellSinkHandle::new(sink));
    let mut execution = Box::pin(registry.execute_with(
        "run_command",
        &json!({"command": format!("printf streamed; while [ ! -e '{}' ]; do sleep 0.01; done", release.display())}),
        &tool_context,
    ));

    let started = tokio::select! {
        result = &mut execution => panic!("tool returned before Started: {result:?}"),
        event = events.recv() => event,
    };
    assert!(matches!(started, Some(ShellEvent::Started { .. })));
    let chunk = tokio::select! {
        result = &mut execution => panic!("tool returned before incremental output: {result:?}"),
        event = events.recv() => event,
    };
    assert!(matches!(chunk, Some(ShellEvent::Chunk { .. })));
    std::fs::write(release, b"go").unwrap();
    let result = execution.await.unwrap();
    assert_eq!(result["stdout"], "streamed");
}

#[tokio::test]
async fn tool_context_cancellation_stops_a_running_command() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".into()])
            .disable_sandbox()
            .with_terminate_grace(Duration::from_millis(30)),
    ));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();
    let (sink, mut events) = ChannelSink::new(NonZeroUsize::new(8).unwrap());
    let (stop, observer) = tokio::sync::watch::channel(false);
    let tool_context = ToolContext::new()
        .with(ShellSinkHandle::new(sink))
        .with(ShellCancel::from_watch(observer));

    // A command that would otherwise run for 30s under a 30s default timeout.
    let mut execution = Box::pin(registry.execute_with(
        "run_command",
        &json!({"command": "printf running; sleep 30"}),
        &tool_context,
    ));

    // Wait until the child is definitely up before cancelling it.
    loop {
        let event = tokio::select! {
            result = &mut execution => panic!("tool returned before its first chunk: {result:?}"),
            event = events.recv() => event,
        };
        if matches!(event, Some(ShellEvent::Chunk { .. })) {
            break;
        }
    }

    stop.send_replace(true);

    let result = tokio::time::timeout(Duration::from_secs(5), execution)
        .await
        .expect("cancellation should end the tool call well before the timeout")
        .expect("a cancelled command is not a tool error");

    assert_eq!(result["outcome"], "cancelled");
    assert_eq!(result["timed_out"], json!(false));
    assert_eq!(result["stdout"], "running");
    assert_eq!(result["cleanup_completed"], json!(true));
}

// ─────────────────────────────────────────────────────────────────────
// Permission errors propagate through tool layer
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_returns_error_on_denied_command() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new().with_denied_commands(vec!["rm *".into()]),
    ));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();

    let result: Result<Value, ToolError> = registry
        .execute("run_command", &json!({"command": "rm -rf /"}))
        .await;

    assert!(result.is_err(), "should fail for denied command");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("denied"),
        "error should mention denial, got: {err_msg}"
    );
}

#[tokio::test]
async fn tool_returns_confirmation_required_for_unconfirmed_command() {
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    // No allowed or denied patterns — everything requires confirmation
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let registry = Res::<ToolRegistry>::fetch(&ctx).unwrap();

    let result: Value = registry
        .execute("run_command", &json!({"command": "echo hello"}))
        .await
        .expect("should succeed");

    assert_eq!(
        result["status"], "confirmation_required",
        "should return confirmation_required status, got: {result}"
    );
    assert_eq!(result["command"], "echo hello");
}

// ─────────────────────────────────────────────────────────────────────
// ShellExecutor registered as global resource
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shell_executor_accessible_as_global_resource() {
    use polaris_shell::ShellExecutor;

    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new().with_timeout(60)));
    server.finish().await.unwrap();

    let ctx = server.create_context();
    let executor = Res::<ShellExecutor>::fetch(&ctx)
        .expect("ShellExecutor should be accessible as global resource");

    assert_eq!(executor.config().default_timeout_secs, 60);
}
