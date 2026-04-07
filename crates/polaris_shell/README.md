# polaris_shell

Shell command execution for Polaris agents.

## Overview

Provides shell command execution with a safety-first permission model, directory sandboxing, timeout enforcement, and output truncation.

- **`ShellExecutor`** - Execution engine with permission checking (global resource)
- **`ShellTools`** - LLM-facing tool wrapper for the tool registry
- **`ShellPlugin`** - Plugin that registers both of the above
- **`ShellConfig`** - Configuration for allowed/denied commands and directories

## Permission Model

Every command is matched against glob patterns, evaluated in precedence order:

| Priority | Matches | Result |
|----------|---------|--------|
| 1 (highest) | `denied_commands` | **Deny** — command is rejected |
| 2 | `allowed_commands` | **Allow** — command runs immediately |
| 3 (default) | neither list | **Confirm** — requires user approval |

Compound commands (`&&`, `||`, `;`, `|`) are split and each subcommand is evaluated separately. The most restrictive result applies.

## Example

```rust
use polaris_shell::{ShellConfig, ShellPlugin};

let plugin = ShellPlugin::new(
    ShellConfig::new()
        .with_allowed_commands(vec!["cargo *".into(), "git *".into()])
        .with_denied_commands(vec!["rm -rf *".into(), "sudo *".into()])
        .with_allowed_dirs(vec!["/home/user/project".into()])
);
```

## License

Apache-2.0
