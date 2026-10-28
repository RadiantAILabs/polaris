# polaris_shell

Shell command execution for Polaris agents.

## Overview

Provides shell command execution with a safety-first permission model,
directory sandboxing, bounded incremental output, stable managed-run identity,
and process cleanup.

- **`ShellExecutor`** - Execution engine with permission checking (global resource)
- **`ShellTools`** - LLM-facing tool wrapper for the tool registry
- **`ShellPlugin`** - Plugin that registers both of the above
- **`ShellConfig`** - Configuration for allowed/denied commands and directories
- **`ShellProcess`** - Handle for waiting, stopping, or detaching one accepted run
- **`ChannelSink`** - Bounded incremental stdout/stderr delivery with retained lifecycle events

## Permission Model

Every command is matched against glob patterns, evaluated in precedence order:

| Priority | Matches | Result |
|----------|---------|--------|
| 1 (highest) | `denied_commands` | **Deny** — command is rejected |
| 2 | `allowed_commands` | **Allow** — command runs immediately |
| 3 (default) | neither list | **Confirm** — requires user approval |

Compound commands (`&&`, `||`, `;`, `|`) are split and each subcommand is evaluated separately. The most restrictive result applies.

## Managed processes

`ShellExecutor::start` and `start_confirmed` expose the same permission-gated
lifecycle used by `execute` and `execute_confirmed`. Accepted runs receive a
stable `ShellProcessId`; callers can observe status, stop a run, detach it, or
consume its result exactly once. The executor retains up to 64 unconsumed
completed results for late waiters by default.

Attach a `ShellSinkHandle` to stream distinct raw stdout and stderr chunks. The
provided `ChannelSink` bounds data events and reserves constant-size storage for
`Started` and `Finished`. Each channel accepts one run across all its clones;
reuse is rejected before spawning. If every producer disappears before
completion, the receiver drains buffered events and returns `None` without
inventing a terminal outcome. In-memory capture is independently bounded per stream;
when output crosses the limit, a lazy overflow file contains the retained prefix
and later bytes until the configured per-stream spill quota is reached.
`stdout_overflow_truncated` and `stderr_overflow_truncated` report incomplete
spill files.

Timeout, cancellation, and plugin shutdown terminate the spawned Unix process
group with `SIGTERM`, a configurable grace period, then `SIGKILL`. This covers
descendants that remain in the group; deliberately daemonized processes that
leave it are outside the guarantee. Normal leader exit forcibly stops remaining
group members before the leader is reaped. An escaped descendant holding an
output pipe makes `cleanup_completed` false after the bounded drain deadline;
escaped processes with no inherited pipes cannot be detected this way. Windows version one terminates only the
direct child. Execution uses pipes, not a PTY, so terminal-only programs and
terminal rendering are out of scope.

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
