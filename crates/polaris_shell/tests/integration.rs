//! Integration tests for the `polaris_shell` crate.

use polaris_shell::{
    ChannelSink, ShellAllowedDirs, ShellCancel, ShellConfig, ShellError, ShellEvent, ShellExecutor,
    ShellPlugin, ShellRequest, ShellResponse, ShellSinkHandle, ShellTools,
};
use polaris_system::param::{Res, SystemParam};
use polaris_system::server::Server;
use polaris_tools::{ToolContext, ToolError, ToolRegistry, ToolsPlugin};
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
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
    for host_only in ["cancel", "sink", "allowed_dirs"] {
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

// ─────────────────────────────────────────────────────────────────────
// Per-request folder limits
// ─────────────────────────────────────────────────────────────────────

/// A repository folder with a helper worktree inside and a sibling folder
/// the executor allows but the helper must not start in.
fn request_roots_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("temporary directory");
    let repo = root.path().canonicalize().unwrap().join("repo");
    let helper = repo.join("worktrees").join("helper");
    std::fs::create_dir_all(&helper).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    (root, repo, helper)
}

fn pwd_executor(allowed: &Path) -> ShellExecutor {
    ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["pwd".into()])
            .with_allowed_dirs(vec![allowed.to_path_buf()])
            .disable_cache(),
    )
}

async fn pwd_in(
    executor: &ShellExecutor,
    dir: &Path,
    limit: Vec<PathBuf>,
) -> Result<String, ShellError> {
    let request = ShellRequest::new("pwd")
        .with_working_dir(dir)
        .with_allowed_dirs(limit);
    match executor.execute(request).await? {
        ShellResponse::Executed(result) => Ok(result.stdout.trim().to_owned()),
        other => panic!("pwd should run without confirmation, got {other:?}"),
    }
}

#[tokio::test]
async fn request_dirs_allow_a_folder_inside_the_limit() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = pwd_executor(&repo);
    let nested = helper.join("deep");
    std::fs::create_dir(&nested).unwrap();

    let out = pwd_in(&executor, &nested, vec![helper.clone()])
        .await
        .unwrap();
    assert_eq!(out, nested.to_string_lossy());
}

#[tokio::test]
async fn request_dirs_refuse_a_folder_the_executor_allows() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = pwd_executor(&repo);

    let err = pwd_in(&executor, &repo.join("src"), vec![helper])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ShellError::InvalidWorkingDir(text) if text.contains("this request")),
        "a sibling of the helper folder must be refused, got {err:?}"
    );
}

#[tokio::test]
async fn request_dirs_cannot_widen_the_executor() {
    let (_root, repo, helper) = request_roots_tree();
    // The executor only allows the helper folder; the request names the repo.
    let executor = pwd_executor(&helper);

    let err = pwd_in(&executor, &repo.join("src"), vec![repo.clone()])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ShellError::InvalidWorkingDir(text) if text.contains("outside allowed directories")),
        "the executor's own limit must still apply, got {err:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn request_dirs_resolve_symlinks_before_checking() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = pwd_executor(&repo);
    let link = helper.join("escape");
    std::os::unix::fs::symlink(repo.join("src"), &link).unwrap();

    let err = pwd_in(&executor, &link, vec![helper]).await.unwrap_err();
    assert!(
        matches!(err, ShellError::InvalidWorkingDir(_)),
        "a symlink out of the limit must be refused, got {err:?}"
    );
}

#[tokio::test]
async fn empty_or_missing_request_dirs_refuse_every_folder() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = pwd_executor(&repo);

    let err = pwd_in(&executor, &helper, Vec::new()).await.unwrap_err();
    assert!(
        matches!(err, ShellError::InvalidWorkingDir(_)),
        "got {err:?}"
    );

    let missing = repo.join("gone");
    // The order of the list must not matter: an unresolvable entry refuses
    // the request even after a folder that matches.
    for limit in [
        vec![missing.clone(), helper.clone()],
        vec![helper.clone(), missing.clone()],
    ] {
        let err = pwd_in(&executor, &helper, limit.clone()).await.unwrap_err();
        assert!(
            matches!(&err, ShellError::InvalidWorkingDir(text) if text.contains("is invalid")),
            "an unresolvable entry must refuse, not be skipped, for {limit:?}, got {err:?}"
        );
    }
}

#[tokio::test]
async fn a_later_request_dir_can_match() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = pwd_executor(&repo);

    let out = pwd_in(&executor, &helper, vec![repo.join("src"), helper.clone()])
        .await
        .unwrap();
    assert_eq!(out, helper.to_string_lossy());
}

#[tokio::test]
async fn request_dirs_apply_when_the_executor_sandbox_is_off() {
    let (_root, repo, helper) = request_roots_tree();
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["pwd".into()])
            .disable_sandbox()
            .disable_cache(),
    );

    let err = pwd_in(&executor, &repo.join("src"), vec![helper.clone()])
        .await
        .unwrap_err();
    assert!(
        matches!(err, ShellError::InvalidWorkingDir(_)),
        "got {err:?}"
    );
    let out = pwd_in(&executor, &helper, vec![helper.clone()])
        .await
        .unwrap();
    assert_eq!(out, helper.to_string_lossy());
}

#[tokio::test]
async fn tool_context_allowed_dirs_narrow_run_command() {
    let (_root, repo, helper) = request_roots_tree();
    // A local registry: no server, so no HTTP listener on a fixed port.
    let registry = pwd_registry(&repo, None);
    let context = ToolContext::new().with(ShellAllowedDirs::new(vec![helper.clone()]));

    let inside = registry
        .execute_with(
            "run_command",
            &json!({"command": "pwd", "working_dir": helper}),
            &context,
        )
        .await
        .expect("the helper folder is inside the limit");
    assert_eq!(
        inside["stdout"].as_str().unwrap().trim(),
        helper.to_string_lossy()
    );

    let outside = registry
        .execute_with(
            "run_command",
            &json!({"command": "pwd", "working_dir": repo.join("src")}),
            &context,
        )
        .await;
    assert!(
        matches!(&outside, Err(ToolError::ExecutionError(text)) if text.contains("this request")),
        "a folder outside the context limit must be refused, got {outside:?}"
    );

    // Without the context value the executor's own limit is the only one.
    let unlimited = registry
        .execute_with(
            "run_command",
            &json!({"command": "pwd", "working_dir": repo.join("src")}),
            &ToolContext::new(),
        )
        .await
        .expect("the executor allows the repo");
    assert_eq!(unlimited["exit_code"], json!(0));
}

fn pwd_registry(allowed: &Path, default_dir: Option<&Path>) -> ToolRegistry {
    let mut config = ShellConfig::new()
        .with_allowed_commands(vec!["pwd".into()])
        .with_allowed_dirs(vec![allowed.to_path_buf()])
        .disable_cache();
    if let Some(dir) = default_dir {
        config = config.with_working_dir(dir);
    }
    let mut registry = ToolRegistry::new();
    registry.register_toolset(ShellTools::new(ShellExecutor::new(config)));
    registry
}

#[tokio::test]
async fn tool_context_allowed_dirs_apply_to_the_default_working_dir() {
    let (_root, repo, helper) = request_roots_tree();
    let context = ToolContext::new().with(ShellAllowedDirs::new(vec![helper.clone()]));

    // The configured default is inside the limit: an absent or null
    // `working_dir` runs there.
    let registry = pwd_registry(&repo, Some(&helper));
    for args in [
        json!({"command": "pwd"}),
        json!({"command": "pwd", "working_dir": null}),
    ] {
        let out = registry
            .execute_with("run_command", &args, &context)
            .await
            .unwrap_or_else(|err| panic!("default inside the limit must run for {args}: {err:?}"));
        assert_eq!(
            out["stdout"].as_str().unwrap().trim(),
            helper.to_string_lossy()
        );
    }

    // The configured default is outside the limit: the call is refused.
    let registry = pwd_registry(&repo, Some(&repo.join("src")));
    for args in [
        json!({"command": "pwd"}),
        json!({"command": "pwd", "working_dir": null}),
    ] {
        let result = registry.execute_with("run_command", &args, &context).await;
        assert!(
            matches!(&result, Err(ToolError::ExecutionError(text)) if text.contains("this request")),
            "a default outside the limit must be refused for {args}, got {result:?}"
        );
    }
}
