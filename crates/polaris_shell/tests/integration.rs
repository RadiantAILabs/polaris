//! Integration tests for the `polaris_shell` crate.

use polaris_shell::{ShellConfig, ShellPlugin};
use polaris_system::param::{Res, SystemParam};
use polaris_system::server::Server;
use polaris_tools::{ToolError, ToolRegistry, ToolsPlugin};
use serde_json::{Value, json};

// ─────────────────────────────────────────────────────────────────────
// Plugin registration
// ─────────────────────────────────────────────────────────────────────

#[test]
fn plugin_registers_run_command_tool() {
    let mut server = Server::new();
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish();

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

#[test]
fn tool_definition_has_correct_schema() {
    let mut server = Server::new();
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish();

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
}

// ─────────────────────────────────────────────────────────────────────
// Tool execution via registry
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_execution_via_registry() {
    let mut server = Server::new();
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["echo *".into()])
            .disable_sandbox(),
    ));
    server.finish();

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

// ─────────────────────────────────────────────────────────────────────
// Permission errors propagate through tool layer
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_returns_error_on_denied_command() {
    let mut server = Server::new();
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new().with_denied_commands(vec!["rm *".into()]),
    ));
    server.finish();

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
    server.add_plugins(ToolsPlugin);
    // No allowed or denied patterns — everything requires confirmation
    server.add_plugins(ShellPlugin::new(ShellConfig::new()));
    server.finish();

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

#[test]
fn shell_executor_accessible_as_global_resource() {
    use polaris_shell::ShellExecutor;

    let mut server = Server::new();
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(ShellConfig::new().with_timeout(60)));
    server.finish();

    let ctx = server.create_context();
    let executor = Res::<ShellExecutor>::fetch(&ctx)
        .expect("ShellExecutor should be accessible as global resource");

    assert_eq!(executor.config().default_timeout_secs, 60);
}
