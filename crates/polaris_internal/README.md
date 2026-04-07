# polaris_internal

Umbrella re-export crate for Polaris internals.

## Overview

`polaris_internal` re-exports all core Polaris crates under a single dependency, providing convenience modules and a `prelude` for common types.

| Module | Re-exports |
|--------|------------|
| `system` | `polaris_system` |
| `graph` | `polaris_graph` |
| `agent` | `polaris_agent` |
| `tools` | `polaris_tools` |
| `models` | `polaris_models`, `polaris_model_providers` |
| `plugins` | `polaris_core_plugins` |
| `sessions` | `polaris_sessions` |
| `shell` | `polaris_shell` |

## Feature Flags

| Feature | Description |
|---------|-------------|
| `anthropic` | Anthropic model provider |
| `openai` | OpenAI model provider |
| `bedrock` | AWS Bedrock model provider |
| `graph_tracing` | Graph execution tracing |
| `models_tracing` | Model call tracing |
| `tools_tracing` | Tool execution tracing |
| `otel` | OpenTelemetry integration |

## License

Apache-2.0
