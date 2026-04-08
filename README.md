# Polaris

[![Crates.io](https://img.shields.io/crates/v/polaris-ai.svg)](https://crates.io/crates/polaris-ai)
[![docs.rs](https://docs.rs/polaris-ai/badge.svg)](https://docs.rs/polaris-ai)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Rust](https://img.shields.io/badge/rust-1.93.0%2B-orange.svg)](https://www.rust-lang.org/)

> **Alpha** — Core APIs are stabilizing but may change between releases. See [Project Status](#project-status).

A Rust framework for building AI agents as composable directed graphs.

## Why Polaris

Building performant AI agents is a design problem, not a technical problem. The right control flow, tool strategy, and memory architecture for a given use case aren't known upfront — they're discovered through experimentation.

Polaris is designed around that workflow. Agent behavior is composed from small, independent parts that can be rearranged, swapped, and tested without touching the rest of the pipeline. There is no fixed execution loop and no prescribed agent pattern. See [philosophy.md](docs/philosophy.md) for the full rationale.

## Features

- **Design agents as graphs** — Wire together reasoning, tool use, and memory as nodes in a directed graph with sequential, conditional, parallel, and looping control flow.
- **Composition over inheritance** — Swap your LLM provider, tool set, or memory backend without restructuring your agent. Reasoning, tools, and memory are independent components that can be rearranged and replaced in isolation.
- **Everything is a plugin** — No built-in behavior. LLM providers, tools, memory, logging, and tracing are all plugins with managed lifecycles and automatic dependency resolution. Every component is replaceable.
- **Type-safe at compile time** — Graph structure, state types, and resource dependencies are validated by the Rust type system. Invalid designs fail fast, not in production.
- **Inspect and observe everything** — The full agent topology is a data structure you can traverse, and lifecycle hooks give you visibility into every node execution without touching agent logic.
- **Scale to multi-agent** — Each agent runs in an isolated context with its own state, while sharing global resources through a managed context hierarchy.

## Example

An agent is a type that builds a graph. This ReAct agent loops through reasoning, tool selection, and observation until the task is complete:

```rust
struct ReActAgent;

impl Agent for ReActAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(init);

        graph.add_loop::<ReactState, _, _>(
            "react_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(reason);
                g.add_conditional_branch::<ReasoningResult, _, _, _>(
                    "action",
                    |result| result.action == Action::UseTool,
                    |tool_branch| {
                        tool_branch.add_system(select_tool);
                        tool_branch.add_system(execute_tool);
                        tool_branch.add_system(observe);
                    },
                    |respond_branch| {
                        respond_branch.add_system(respond);
                    },
                );
            },
        );
    }
}
```

See [`examples`](examples/README.md) for a full implementation of a file assistant that reasons, selects tools, and acts within a sandboxed directory.

## Getting Started

```toml
[dependencies]
polaris-ai = "0.0.3"
```

Requires **Rust 1.93.0+** (Edition 2024). Run `cargo make test` for the full test suite.

## Documentation

Architecture, design patterns, and API reference are available in the [docs](docs/README.md) directory.

## Project Status

| Component | Status |
|-----------|--------|
| System Framework (Systems, Resources, Plugins, Server) | **Implemented** |
| Graph Execution (Nodes, Edges, Executor, Hooks) | **Implemented** |
| Agent Trait | **Implemented** |
| Model Registry and Providers | **Implemented** |
| Tool Registry | **Implemented** |
| IO Plugin | **Implemented** |
| Agent Plugins | **Implemented** |
| Sessions, Groups, CLI/HTTP Interfaces | **Implemented** |

## License

Apache-2.0 — [Repository](https://github.com/RadiantAILabs/polaris)
