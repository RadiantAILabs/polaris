---
notion_page: https://www.notion.so/radiant-ai/Taxonomy-327afe2e695d80f3b502ca5118f5316a
title: Polaris Taxonomy
---

# Polaris Taxonomy

This document defines the conceptual layers and core abstractions in Polaris. For the design philosophy behind these decisions, see [philosophy.md](./philosophy.md).

## Architecture

Polaris is organized into three layers. Each layer has a clear scope: lower layers are fixed primitives, upper layers are swappable.

| Layer | Name | Crates |
|-------|------|--------|
| **3** | Plugin-Provided Abstractions | `polaris_core` · `polaris_models` · `polaris_model_providers` |
| **2** | Graph Execution & Agent Patterns | `polaris_graph` · `polaris_agent` |
| **1** | System Framework | `polaris_system` |

## Layer 1: System Framework

**Crate:** `polaris_system`

The foundation of Polaris. This layer provides the ECS-inspired primitives that all other code builds on: systems as pure async functions, resources as shared state, dependency injection via typed parameters (`Res<T>`, `ResMut<T>`, `Out<T>`), plugins as the unit of composition, and the `Server` runtime that orchestrates them.

These primitives define *how code is organized and executed*. They are fixed and changing them would mean using a different framework.

**Scope.** Layer 1 owns the core traits (`System`, `Resource`, `SystemParam`, `Plugin`, `API`, `IntoSystem`), the `SystemContext` hierarchy, resource containers, access descriptors and conflict detection, schedule identifiers, and the `Server` implementation. It does not contain anything agent-specific (graphs, LLMs, tools), domain-specific (memory backends, model providers), or optional.

See [system.md](./reference/system.md) for system primitives and parameters, [plugins.md](./reference/plugins.md) for the plugin system, and [api.md](./reference/api.md) for the API primitive.

## Layer 2: Graph Execution and Agent Patterns

**Crates:** `polaris_graph`, `polaris_agent`

This layer defines how agent behavior is structured. `polaris_graph` provides the directed graph model — nodes for computation and control flow, edges for connections between them, and an executor for running graphs against a `SystemContext`. `polaris_agent` provides the `Agent` trait, a minimal interface for packaging a behavior pattern (ReAct, ReWOO, or any custom design) as a reusable unit that knows how to build a graph.

These primitives define *how agents are structured*. They are fixed building blocks for expressing any agent topology.

**Scope.** Layer 2 owns the `Graph` structure and builder API, node types (system, decision, switch, parallel, loop), edge types (sequential, conditional, parallel, loop-back, error, timeout), control flow primitives (`Predicate`, `Discriminator`), the `GraphExecutor`, the hook system (`HooksAPI`, `GraphEvent`, schedule markers), the middleware system (`MiddlewareAPI`, targets, `Next`), the `Agent` trait, and concrete API definitions for agent orchestration. It does not contain specific agent implementations, tool or LLM abstractions, memory backends, or I/O mechanisms.

See [graph.md](./reference/graph.md) for graph construction, execution, hooks, and middleware, and [agents.md](./reference/agents.md) for the agent trait.

## Layer 3: Plugin-Provided Abstractions

**Crates:** `polaris_core`, `polaris_models`, `polaris_model_providers`, etc.

Everything above the fixed primitives is delivered through plugins. This includes core infrastructure (tracing, time, server info), the model registry and LLM provider integrations and concrete agent implementations. Users can mix and match, replace implementations, or omit anything entirely.

**Scope.** Layer 3 is where all optional, swappable functionality lives: LLM provider plugins, concrete agent implementations, utility plugins (tracing, time, metrics), domain-specific resources and traits, and integration plugins for external services. `polaris_core` provides plugin groups (`DefaultPlugins`, `MinimalPlugins`) for common configurations.

See [plugins.md](./reference/plugins.md) for plugin structure and lifecycle.

## Boundary Guide

When adding new functionality, the following process should be followed to determine the appropriate level:

| Question | If yes | Examples |
|----------|--------|----------|
| Is it fundamental to how code executes? | **Layer 1** | New `SystemParam` type, new resource access pattern, new conflict detection rule |
| Is it fundamental to how agents are structured? | **Layer 2** | New node type, new edge type, new hook schedule, new middleware target, graph validation rule |
| Neither of the above? | **Layer 3** (plugin) | New LLM provider, new tool, new agent pattern, new observability feature |
