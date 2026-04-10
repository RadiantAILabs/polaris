---
notion_page: https://www.notion.so/radiant-ai/Design-Pattern-Requirements-and-Agent-Patterns-32fafe2e695d80ebbccadd1a0d135910
title: "Design: Pattern Requirements and Agent Patterns"
---

# Design: Pattern Requirements and Agent Patterns

**Status:** Draft
**Layer:** 3 (Plugin-Provided Abstractions)
**Data struct location:** `polaris_agent` (Layer 2, `PatternRequirements` is pure data with no L3 dependencies)
**Pattern implementations:** separate L3 crates grouped by similarity (see [Crate Organization](#crate-organization))
**Dependencies:** `polaris_agent`, `polaris_graph`, `polaris_tools`, `polaris_models`, `polaris_context`, optional `polaris_memory`, optional `polaris_mcp`
**Date:** 2026-03-25

## Motivation

The Polaris roadmap is intentionally pattern-first: the product value is not "yet another ReAct loop", it is the ability to swap between the architectures behind real systems such as Claude Code, Devin, Cursor, OpenClaw, ZeroClaw, and OpenFang.

To make that work, Polaris needs two things:

1. **A stable contract for what a pattern requires.** The CLI must fail fast if the selected pattern depends on scope, dynamic graphs, memory, or tools that are not available.
2. **A concrete topology spec per pattern.** Each built-in pattern should be implementable from a design doc without re-deciding where scopes, loops, and context boundaries live.

This design proposes:

- Adding `PatternRequirements` as a data struct in `polaris_agent` (Layer 2) with a default method on `Agent`
- Validation against the current runtime environment
- Six built-in pattern implementations as concrete `Agent` impls in dedicated L3 crates

## Crate Organization

`PatternRequirements` lives in `polaris_agent` (Layer 2) because it is a plain data struct with no Layer 3 dependencies. Concrete pattern implementations are Layer 3 agents that live in separate crates, grouped by architectural similarity:

| Crate | Patterns | Rationale |
|-------|----------|-----------|
| `polaris_pattern_claude_code` | `claude_code` | Unique architecture: scope-based sub-agents + parallel fan-out |
| `polaris_pattern_plan_execute` | `devin`, `cursor` | Both use dynamic graph construction driven by a planning phase |
| `polaris_pattern_routing_loop` | `openclaw`, `zeroclaw` | Both are routing loops (one memory-backed, one model-routed) |
| `polaris_pattern_openfang` | `openfang` | Unique architecture: kernel-orchestrated scopes with RBAC gating |

Related patterns share a crate because they reuse the same graph primitives and supporting systems. If a pattern diverges significantly during implementation, it can be split into its own crate.

---

## Pattern Contract

### Extension to `Agent`

Rather than introducing a separate `Pattern` trait, patterns are just `Agent` implementations that declare their requirements. A default method is added to the existing `Agent` trait in `polaris_agent`:

```rust
// in polaris_agent
pub trait Agent {
    // ... existing methods ...

    /// Returns the pattern requirements for this agent, if any.
    /// Agents that represent a pattern override this to declare
    /// their infrastructure needs for preflight validation.
    fn requirements(&self) -> Option<PatternRequirements> {
        None
    }
}
```

This avoids a new trait and keeps the type hierarchy flat. Any `Agent` can optionally declare requirements; the CLI validates them at startup if present.

### `PatternRequirements`

`PatternRequirements` lives in `polaris_agent` (Layer 2). It is a plain data struct with no Layer 3 dependencies, so placing it here does not violate layer boundaries.

```rust
pub struct PatternRequirements {
    pub needs_llm: bool,
    pub needs_streaming: bool,
    pub needs_memory: bool,
    pub needs_workspace: bool,
    pub needs_scope: bool,
    pub needs_dynamic: bool,
    pub required_tools: Vec<&'static str>,
    pub optional_tools: Vec<&'static str>,
}
```

Model role resolution (e.g., "planner", "coder", "critic") is an agent-level concern. Each agent's `setup()` resolves roles from its own config and registers the appropriate provider references into local context. The framework does not need to validate role assignments at the requirements level.

### `PatternEnvironment`

```rust
pub struct PatternEnvironment {
    pub has_model_registry: bool,
    pub has_memory: bool,
    pub has_workspace: bool,
    pub available_tools: BTreeSet<String>,
    pub streaming_models_available: bool,
    pub scope_supported: bool,
    pub dynamic_supported: bool,
}
```

### Validation

```rust
impl PatternRequirements {
    pub fn validate(&self, env: &PatternEnvironment) -> Result<(), PatternValidationError>;
}
```

Validation is a fast preflight check. It is not a substitute for runtime system errors, but it should catch obvious misconfiguration before the first turn starts.

### Failure mode

Validation returns one aggregated error with all missing capabilities, not one error at a time. The CLI should be able to print:

```text
pattern `cursor` cannot start:
- missing tool: read
- missing tool: edit
- workspace plugin not available
- dynamic graph support not available
```

---

## Shared Pattern Conventions

All built-in patterns follow the same conventions:

- `ContextManager` owns conversation history and request shaping
- tool execution results flow back into the LLM loop as normal tool result messages
- request construction is explicit; patterns decide when to call the model
- non-LLM verification steps such as tests and linters remain normal systems, not tool magic

Patterns may differ in topology, but they should not differ in these runtime conventions.

---

## Built-In Patterns

> **Note:** The topologies below are reference implementations based on analysis of the open-source agents they model. They are starting points, not specifications. The actual implementations will closely match the topology of the real open-source agents and may diverge from these descriptions as implementation proceeds.

## 1. `claude_code`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: true,
    needs_memory: false,
    needs_workspace: false,
    needs_scope: true,
    needs_dynamic: false,
    required_tools: vec!["read", "write", "edit", "multi_edit", "grep", "glob", "ls", "shell"],
    optional_tools: vec!["git_status", "git_diff", "git_log", "git_commit"],
}
```

### Topology

```text
receive_input
  -> update_context
  -> loop "main_loop"
       -> reason
       -> switch "next_action"
            -> tool_path
            -> sub_agent_path
            -> respond_path
```

### Details

- `tool_path`
  - execute one or more tool calls
  - append tool results to `ContextManager`
  - loop back to `reason`
- `sub_agent_path`
  - fan out into a `Parallel` node of `Scope` nodes
  - each scope uses `ContextPolicy::isolated()`
  - forward only task-specific input resources such as `SubAgentTask`
  - merge `SubAgentResult` outputs back into the parent
- `respond_path`
  - stream the assistant response
  - append assistant message to `ContextManager`
  - exit loop

### Context policy

- main loop: shared local context
- sub-agents: isolated context, no parent `ContextManager` forwarding by default

This keeps sub-agents sandboxed and prevents them from polluting the parent conversation state directly.

## 2. `devin`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: false,
    needs_memory: false,
    needs_workspace: false,
    needs_scope: false,
    needs_dynamic: true,
    required_tools: vec!["read", "write", "edit", "multi_edit", "grep", "glob", "ls", "shell"],
    optional_tools: vec!["web_fetch", "web_search"],
}
```

### Topology

```text
receive_input
  -> update_context
  -> loop "plan_loop"
       -> plan_steps
       -> dynamic "execution_graph"
       -> assess_progress
       -> conditional "needs_replan"
```

### Dynamic graph shape

The dynamic factory builds one subgraph per plan:

```text
step_1: coder -> critic -> conditional retry_or_continue
step_2: coder -> critic -> conditional retry_or_continue
...
```

Optional browser or web research branches can run in parallel inside a step when the planner requests them.

### Context policy

- the dynamic execution graph uses `ContextPolicy::inherit().forward::<ContextManager>()`

This gives dynamically created step systems a branch-local clone of the context manager while still letting them read parent state through inherited resources.

## 3. `cursor`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: false,
    needs_memory: false,
    needs_workspace: true,
    needs_scope: false,
    needs_dynamic: true,
    required_tools: vec!["read", "write", "edit", "multi_edit", "grep", "glob", "ls", "shell"],
    optional_tools: vec!["git_diff", "git_status"],
}
```

### Topology

```text
receive_input
  -> analyze_workspace
  -> build_plan
  -> dynamic "step_graph"
  -> summarize_result
```

### Dynamic graph shape

For each planned step:

```text
execute_step
  -> verify_step
  -> conditional "verification_passed"
       -> next_step
       -> fix_step -> verify_step
```

Verification systems are normal graph nodes and typically run linting or tests through shell tools.

### Context policy

- dynamic step graph uses `ContextPolicy::inherit()`
- verification failures are modeled as outputs or conditional branches, not graph-level infrastructure errors

## 4. `openclaw`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: true,
    needs_memory: true,
    needs_workspace: false,
    needs_scope: false,
    needs_dynamic: false,
    required_tools: vec![],
    optional_tools: vec!["web_fetch"],
}
```

### Topology

```text
receive_input
  -> assemble_context
  -> loop "tool_loop"
       -> reason
       -> conditional "has_tool_call"
            -> execute_tool -> persist_tool_findings -> loop
            -> finalize -> persist_memory
```

### Details

- `assemble_context` performs hybrid memory recall and injects recalled snippets into the prompt-building path
- `persist_memory` stores durable decisions, not the entire raw transcript

### Future extension

OpenClaw's proactive trigger model is out of scope for the first implementation and should be added later as schedule-driven entry systems, not forced into the v1 graph.

## 5. `zeroclaw`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: true,
    needs_memory: false,
    needs_workspace: false,
    needs_scope: false,
    needs_dynamic: false,
    required_tools: vec![],
    optional_tools: vec![],
}
```

### Topology

```text
receive_input
  -> classify_request
  -> switch "route_model"
       -> fast_loop
       -> reasoning_loop
```

Each branch is a standard tool-using loop. The difference is the model role and tool allowance, not a different graph primitive set.

### Context policy

All work stays inline in one shared graph. There is no scope or dynamic graph boundary in v1.

## 6. `openfang`

### Requirements

```rust
PatternRequirements {
    needs_llm: true,
    needs_streaming: true,
    needs_memory: false,
    needs_workspace: false,
    needs_scope: true,
    needs_dynamic: false,
    required_tools: vec![],
    optional_tools: vec![],
}
```

### Topology

```text
receive_trigger
  -> select_hand
  -> check_capabilities
  -> switch "workflow"
       -> scope research_hand
       -> scope execution_hand
       -> parallel broadcast_hands
```

### Details

- each Hand is a `Scope` node
- scopes usually use `ContextPolicy::inherit()`
- RBAC, budget checks, and safety gating happen outside scopes as normal systems before entering a Hand

This preserves auditability and avoids depending on shared mutable writes across scope boundaries.

---

## Pattern Selection

Since patterns are just `Agent` implementations that happen to return `Some(PatternRequirements)` from `requirements()`, pattern selection is simply selecting which `Agent` implementation to instantiate. The CLI can maintain a registry of agent constructors keyed by stable name:

```rust
pub type AgentFactory = fn(config: serde_json::Value) -> Result<Box<dyn Agent>, PatternConfigError>;

pub struct PatternRegistry {
    factories: BTreeMap<&'static str, AgentFactory>,
}
```

No `PatternFactory` trait is needed. The registry is a simple map from pattern key (e.g., `"claude_code"`, `"devin"`) to a constructor function that returns a `Box<dyn Agent>`. Validation happens after construction by calling `agent.requirements()` and checking against the runtime environment.

This is intentionally separate from `ToolRegistry`. Pattern selection is a startup concern, not an LLM-facing runtime tool.

---

## Implementation Plan

### Phase 1: Contract

1. Add `PatternRequirements`, `PatternEnvironment`, `PatternValidationError` to `polaris_agent`.
2. Add `fn requirements(&self) -> Option<PatternRequirements> { None }` as a default method on `Agent`.
3. Add CLI-facing validation helpers.
4. Add tests:
   - missing capabilities are aggregated
   - optional tools do not fail validation

### Phase 2: Built-in patterns

1. Create `polaris_pattern_claude_code` crate, implement `claude_code`.
2. Create `polaris_pattern_plan_execute` crate, implement `devin` and `cursor`.
3. Create `polaris_pattern_routing_loop` crate, implement `openclaw` and `zeroclaw`.
4. Create `polaris_pattern_openfang` crate, implement `openfang`.
5. Add one integration-style graph-shape test per pattern verifying the expected primitive set:
   - `claude_code` contains loop + parallel scopes
   - `devin` contains loop + dynamic node
   - `cursor` contains dynamic node + verification branch
   - `openclaw` contains loop + memory integration point
   - `zeroclaw` contains switch-based routing
   - `openfang` contains sequential scopes and parallel fan-out

## Open Questions

1. `openfang` capability gating will likely want a dedicated resource for budgets and permissions. That should be specified when the pattern is implemented, not guessed here.
2. `zeroclaw` can benefit from memory, but the first contract keeps memory optional because the core differentiator is routing, not persistence.
