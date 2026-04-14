# Polaris Documentation

## Overview

- [**Philosophy**](./philosophy.md) — Design rationale behind Polaris
- [**Taxonomy**](./taxonomy.md) — Overview of the layered architecture

## Reference

- [**Systems**](./reference/system.md) — Systems, parameters, and the `#[system]` macro
- [**Execution Context**](./reference/context.md) — SystemContext lifecycle, hierarchy, resource resolution, graph context flow
- [**Graphs**](./reference/graph.md) — Graph execution, control flow, error handling, hooks, middleware
- [**Agents**](./reference/agents.md) — Agent trait and pattern implementations
- [**Plugins**](./reference/plugins.md) — Plugin system and compositional architecture
- [**APIs**](./reference/api.md) — Capability registration for shared behaviours across plugins
- [**Sessions**](./reference/sessions.md) — SessionsAPI, turn execution, checkpoints, persistence
- [**HTTP Integration**](./reference/http.md) — HTTP handlers, DeferredState, HttpIOProvider, route registration
- [**Scheduling**](./reference/scheduling.md) — Server lifecycle, tick scheduling, plugin update ordering
- [**Data Flow Patterns**](./reference/data-flow.md) — Choosing between `Res<T>`, `ResMut<T>`, and `Out<T>`
- [**Tools**](./reference/tools.md) — Tool definitions, `ToolRegistry`, permission model
- [**Model Providers**](./reference/model-providers.md) — `LlmProvider` trait, adding a custom provider
- [**DevTools**](./reference/devtools.md) — `SystemInfo`, event tracing, debugging graph execution
- [**Testing**](./reference/testing.md) — Testing strategy per architecture layer
