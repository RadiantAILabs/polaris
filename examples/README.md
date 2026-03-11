# Examples

## ReAct Agent CLI

Interactive REPL demonstrating the ReAct (Reasoning + Acting) pattern. See [agents.md](../docs/reference/agents.md) for the pattern specification.

### Features

- Multi-turn conversations with history
- Session persistence across runs
- File system tools (sandboxed to working directory)

**Available tools:** `list_files`, `read_file`, `write_file`

### Running

Run the following commands from the `examples/` directory:

```bash
cargo run -p examples --bin cli -- <working_dir> [--session <id>]

# Example
cargo run -p examples --bin cli -- ./sandbox
cargo run -p examples --bin cli -- ./sandbox --session my-session
```

### Commands

- `/help` — Show available commands
- `/history` — Show conversation history
- `/clear` — Clear conversation history
- `/exit` or `/quit` — Exit the REPL
- `/save` — Save session to disk
- `/info` — Show session info
- `/sessions` — List all saved sessions
- `/rollback <turn>` — Rollback to a checkpoint
