Shell command execution with a layered permission model.

This module provides infrastructure for agents to execute shell commands
safely, with configurable permission boundaries and a managed process lifecycle.

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

# Managed execution and streaming

[`ShellExecutor`](crate::shell::ShellExecutor) exposes both foreground
`execute` methods and managed `start` methods over one lifecycle. Permission
and working-directory checks happen before a [`ShellProcessId`](crate::shell::ShellProcessId)
is allocated or an operating-system child is created. A
[`ShellProcess`](crate::shell::ShellProcess) can be waited, stopped, or detached;
the executor table supports status snapshots and late, exactly-once result
consumption.

Use [`ShellSinkHandle`](crate::shell::ShellSinkHandle) for incremental raw-byte
events. [`ChannelSink`](crate::shell::ChannelSink) bounds data events while
retaining `Started` and `Finished`. Each channel and all its clones accept exactly
one run; reuse is rejected before spawning. A receiver whose producers disappear
drains buffered events and ends without inventing `Finished`. Stdout and stderr remain distinct, with
ordering guaranteed within each stream. Capture is bounded independently per
stream. An overflow file contains the complete bytes from the start of the
stream when the per-stream spill quota is not reached; the result flags an
incomplete spill file.

Timeout and cancellation preserve partial output. Unix cleanup signals the
spawned process group; descendants that deliberately leave it are outside the
guarantee. Normal leader exit forcibly stops remaining group members before the
leader is reaped. An escaped descendant retaining a pipe causes bounded draining
to report `cleanup_completed = false`; escaped processes without inherited pipes
cannot be detected this way. Windows cleanup is direct-child-only. This surface uses pipes rather
than a PTY.

`run_command` keeps the same LLM-visible schema. Hosts opt into streaming and
cancellation through [`ToolContext`](crate::tools::ToolContext) values when
calling [`ToolRegistry::execute_with`](crate::tools::ToolRegistry::execute_with).

# Related

- [Tools](crate::tools) -- shell commands integrate with the tool permission model
- [Plugins](crate::system) -- `ShellPlugin` lifecycle
