//! Behavioural tests for the environment managed children are spawned with.
//!
//! These read a variable that genuinely exists in this test process rather
//! than exporting one, so nothing here mutates the process environment — which
//! would be unsound while other tests run on sibling threads.

use polaris_shell::{ShellConfig, ShellExecutor, ShellRequest};
use std::collections::BTreeMap;

/// Names the executor always provides to a non-inheriting child, plus the ones
/// `sh` sets for itself. A variable outside this set can only reach the child
/// by inheritance, an allowlist, or the explicit map.
const PROVIDED_OR_SHELL_SET: [&str; 11] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR", "PWD", "SHLVL", "_",
];

fn executor(config: ShellConfig) -> ShellExecutor {
    ShellExecutor::new(
        config
            .with_allowed_commands(vec!["env".to_owned()])
            .disable_sandbox()
            .disable_cache(),
    )
}

/// Runs `env` and returns the child's environment as name/value pairs.
async fn child_env(config: ShellConfig) -> BTreeMap<String, String> {
    let result = executor(config)
        .execute_confirmed(ShellRequest::new("env"))
        .await
        .expect("env should run");
    assert_eq!(result.exit_code, Some(0), "stderr: {}", result.stderr);
    result
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Picks a variable that this test process has and that the executor would not
/// provide on its own — the thing that must not leak into a clean child.
fn host_only_variable() -> (String, String) {
    std::env::vars()
        .find(|(name, value)| {
            !PROVIDED_OR_SHELL_SET.contains(&name.as_str())
                && !name.is_empty()
                && !value.is_empty()
                && !value.contains('\n')
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .expect("the test process should have at least one non-trusted variable")
}

#[tokio::test]
async fn host_variables_do_not_reach_the_child_by_default() {
    let (name, _) = host_only_variable();
    let child = child_env(ShellConfig::new()).await;

    assert!(
        !child.contains_key(&name),
        "`{name}` leaked into a clean child: {child:?}"
    );
    for trusted in ["HOME", "LANG", "TERM"] {
        let host = std::env::var(trusted).ok();
        assert_eq!(
            child.get(trusted),
            host.as_ref(),
            "{trusted} must retain its host value"
        );
    }
    let unexpected: Vec<_> = child
        .keys()
        .filter(|key| !PROVIDED_OR_SHELL_SET.contains(&key.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "a clean child should only see the trusted base, got extras: {unexpected:?}"
    );
}

#[tokio::test]
async fn host_variables_reach_the_child_when_inheritance_is_enabled() {
    let (name, value) = host_only_variable();
    let child = child_env(ShellConfig::new().with_inherit_env(true)).await;

    assert_eq!(
        child.get(&name),
        Some(&value),
        "`{name}` should be inherited when inherit_env is on"
    );
}

#[tokio::test]
async fn an_allowlisted_variable_reaches_the_child() {
    let (name, value) = host_only_variable();
    let child = child_env(ShellConfig::new().with_env_allowlist(vec![name.clone()])).await;

    assert_eq!(
        child.get(&name),
        Some(&value),
        "`{name}` was allowlisted and should be copied"
    );
}

#[tokio::test]
async fn explicit_env_overrides_everything_else() {
    let (name, inherited) = host_only_variable();
    let explicit = format!("explicit-not-{inherited}");
    let child = child_env(
        ShellConfig::new()
            .with_inherit_env(true)
            .with_env_var(name.clone(), explicit.clone())
            .with_env_var("POLARIS_SHELL_TEST", "explicit"),
    )
    .await;

    assert_eq!(child.get(&name), Some(&explicit));
    assert_eq!(
        child.get("POLARIS_SHELL_TEST"),
        Some(&"explicit".to_owned())
    );
}

#[tokio::test]
async fn the_default_path_resolves_ordinary_binaries() {
    let child = child_env(ShellConfig::new()).await;
    assert_eq!(
        child.get("PATH"),
        Some(&"/usr/local/bin:/usr/bin:/bin".to_owned())
    );

    // The point of a trusted default: allowed commands still resolve.
    let result = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["sh *".to_owned()])
            .disable_sandbox()
            .disable_cache(),
    )
    .execute_confirmed(ShellRequest::new("sh -c 'command -v env'"))
    .await
    .expect("a binary on the default PATH should resolve");
    assert_eq!(result.exit_code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stdout.trim().ends_with("/env"),
        "unexpected resolution: {}",
        result.stdout
    );
}

#[tokio::test]
async fn a_configured_path_is_used_verbatim() {
    let child = child_env(ShellConfig::new().with_path("/opt/tools:/bin")).await;
    assert_eq!(child.get("PATH"), Some(&"/opt/tools:/bin".to_owned()));
}

#[tokio::test]
async fn explicit_env_overrides_trusted_names_and_configured_path() {
    let child = child_env(
        ShellConfig::new()
            .with_path("/unused-configured-path")
            .with_env_var("HOME", "/explicit-home")
            .with_env_var("LANG", "C")
            .with_env_var("TERM", "explicit-term")
            .with_env_var("PATH", "/usr/bin:/bin"),
    )
    .await;
    for (name, expected) in [
        ("HOME", "/explicit-home"),
        ("LANG", "C"),
        ("TERM", "explicit-term"),
        ("PATH", "/usr/bin:/bin"),
    ] {
        assert_eq!(child.get(name).map(String::as_str), Some(expected));
    }
}
