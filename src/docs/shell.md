Shell command execution with a layered permission model.

This module provides infrastructure for agents to execute shell commands
safely, with configurable permission boundaries.

# Permission Architecture

Permissions are evaluated in precedence order:

| Layer | Scope | Example |
|-------|-------|---------|
| **Deny list** | Commands always blocked | `rm -rf /`, `sudo` |
| **Allow list** | Commands always permitted | `ls`, `cat`, `grep` |
| **Policy** | Default for unlisted commands | `Allow`, `Confirm`, or `Deny` |
| **Sandbox** | Execution environment | Working directory, env vars, timeout |

# Setup

```no_run
use polaris_ai::shell::{ShellPlugin, ShellConfig};
use polaris_ai::system::server::Server;

let mut server = Server::new();
server.add_plugins(
    ShellPlugin::new(
        ShellConfig::new()
            .with_working_dir("/home/user/project")
            .with_allowed_commands(vec!["ls *".into(), "cat *".into(), "grep *".into()])
            .with_denied_commands(vec!["rm *".into(), "sudo *".into()])
            .with_timeout(30)
    )
);
```

# Related

- [Tools](crate::tools) -- shell commands integrate with the tool permission model
- [Plugins](crate::system) -- `ShellPlugin` lifecycle
