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
cargo run -p examples --bin cli -- <working_dir> [--session <id>] [--otel <endpoint>] [--capture-content]

# Example
cargo run -p examples --bin cli -- ./sandbox
cargo run -p examples --bin cli -- ./sandbox --session my-session

# With OpenTelemetry trace export
cargo run -p examples --bin cli -- ./sandbox --otel http://localhost:4318/v1/traces
```

### OpenTelemetry Tracing

The `--otel <endpoint>` flag enables OpenTelemetry trace export. All LLM calls, tool executions, and graph node transitions are exported as spans.

[Arize Phoenix](https://github.com/Arize-ai/phoenix) is an open-source observability tool that works well for local development:

```bash
# Install Phoenix (see https://arize.com/docs/phoenix/quickstart)
pip install arize-phoenix

# Start the Phoenix server
phoenix serve

# Run the CLI with tracing pointed at Phoenix
cargo run -p examples --bin cli -- ./sandbox --otel "http://localhost:6006/v1/traces"

# Include full LLM request/response and tool call content in spans
cargo run -p examples --bin cli -- ./sandbox --otel "http://localhost:6006/v1/traces" --capture-content
```

Then open `http://localhost:6006` to view traces.

The `--capture-content` flag records `gen_ai.*` content attributes on LLM and tool spans (input messages, output messages, tool arguments/results). Omit it to export only structural spans without payload data.

### Commands

- `/help` — Show available commands
- `/history` — Show conversation history
- `/clear` — Clear conversation history
- `/exit` or `/quit` — Exit the REPL
- `/save` — Save session to disk
- `/info` — Show session info
- `/sessions` — List all saved sessions
- `/rollback <turn>` — Rollback to a checkpoint
