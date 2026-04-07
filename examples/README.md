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

---

## ReAct Agent HTTP Server

Serves the same ReAct agent over HTTP using `polaris_app`. Demonstrates `AppPlugin` route registration, Tower middleware, and the plugin-based HTTP architecture.

### Features

- Shared HTTP server runtime via `AppPlugin`
- Plugin-based route registration via `HttpRouter` API
- Tower middleware (CORS, request tracing, `x-request-id`)
- Health check and agent info endpoints
- Pre-configured demo session with ReAct agent

### Running

```bash
cargo run -p examples --bin http -- <working_dir> [--port <port>]

# Example
cargo run -p examples --bin http -- ./sandbox
cargo run -p examples --bin http -- ./sandbox --port 8080
```

### Endpoints

```bash
# Health check
curl http://localhost:3000/healthz

# Agent info
curl http://localhost:3000/v1/info


# Create a session
curl -X POST http://localhost:3000/v1/sessions \
  -H 'Content-Type: application/json' \
  -d '{"agent_type": "react"}'

# List sessions
curl http://localhost:3000/v1/sessions

# Get session info
curl http://localhost:3000/v1/sessions/{id}

# Send a turn (using the pre-loaded "demo" session)
curl -X POST http://localhost:3000/v1/sessions/demo/turns \
  -H 'Content-Type: application/json' \
  -d '{"message": "What files are in the current directory?"}'

# Delete a session
curl -X DELETE http://localhost:3000/v1/sessions/{id}
```
