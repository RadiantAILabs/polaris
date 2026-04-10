---
notion_page: https://www.notion.so/radiant-ai/Polaris-Tutorial-Series-Wider-Roadmap-33bafe2e695d8047bf3fdb14e4a45e14
title: Polaris Tutorial Series Roadmap
---

# Polaris Tutorial Series Roadmap

**Goal:** Ship a comprehensive tutorial series that teaches users how to build AI agents with Polaris, progressing from fundamentals to advanced patterns. Tutorials are writable as soon as their framework dependencies are complete, and published alongside the features they teach.

**Date:** 2026-04-07

**References:**
- [`docs/plans/unified_polaris_roadmap.md`](unified_polaris_roadmap.md) (feature delivery timeline)
- [`docs/reference/graph.md`](../reference/graph.md)
- [`docs/reference/agents.md`](../reference/agents.md)
- [`docs/reference/plugins.md`](../reference/plugins.md)
- [`docs/reference/system.md`](../reference/system.md)

---

## Motivation

Every major AI lab and framework vendor has converged on the same set of agent-building tutorials: start simple, build up to orchestration patterns, add production concerns. The industry resources surveyed include:

- [OpenAI — A Practical Guide to Building Agents](https://cdn.openai.com/business-guides-and-resources/a-practical-guide-to-building-agents.pdf)
- [Anthropic — Building Effective Agents](https://www.anthropic.com/research/building-effective-agents)
- [Anthropic — Multi-Agent Research System](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Anthropic — Writing Tools for Agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
- [Anthropic — Effective Context Engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Anthropic — Effective Harnesses for Long-Running Agents](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)
- [OpenAI — Building Agents Developer Track](https://developers.openai.com/tracks/building-agents/)
- [Google — Agents Overview (Gemini API)](https://ai.google.dev/gemini-api/docs/agents)
- [Google — ReAct Agent from Scratch with LangGraph](https://ai.google.dev/gemini-api/docs/langgraph-example)
- [LangChain — How to Build an Agent](https://blog.langchain.com/how-to-build-an-agent/)
- [LangChain — Building LangGraph](https://blog.langchain.com/building-langgraph/)
- [HuggingFace — Agents Course](https://huggingface.co/learn/agents-course/unit1/tutorial)
- [Microsoft — AI Agents for Beginners](https://learn.microsoft.com/en-us/shows/ai-agents-for-beginners/)
- [Cloudflare — Agents Getting Started](https://developers.cloudflare.com/agents/getting-started/)

### Cross-Cutting Themes

These resources converge on eight themes that the tutorial series must cover:

1. **Start simple, add complexity only when measured improvements justify it** (Anthropic, OpenAI, LangChain)
2. **Tool design is as important as prompt design** — tools are the agent-computer interface (Anthropic, OpenAI, HuggingFace)
3. **The agent loop** (think -> act -> observe -> repeat) is the universal pattern (ReAct)
4. **Orchestration patterns converge**: single-agent -> orchestrator-worker -> decentralized handoff -> parallel subagents
5. **Guardrails and human-in-the-loop** are production requirements (OpenAI, LangGraph)
6. **Context management** is an emerging discipline: compaction, note-taking, just-in-time retrieval (Anthropic)
7. **Evaluation** remains the hardest problem — outcome-based over step-based (Anthropic, LangChain)
8. **Long-running agents** need special infrastructure: checkpointing, incremental progress, recovery (Anthropic, LangGraph)

### Polaris Differentiators

Polaris tutorials can stand apart by emphasizing capabilities other frameworks lack:

- **ECS-inspired separation** of state (resources) and behavior (systems) — enables testability and composability that framework-coupled agents cannot match
- **Graph-based execution with typed control flow** — graphs are inspectable, validatable, and restructurable; not opaque while-loops
- **Plugin architecture** — every capability is swappable, testable in isolation, and optional
- **Hooks and middleware** — first-class cross-cutting concerns, not afterthoughts
- **Compile-time verification** — typed resource injection (`Res<T>`, `ResMut<T>`, `Out<T>`) catches errors before runtime

---

## Design Philosophy

Tutorials are the primary interface between Polaris and its users — both human developers and LLMs generating Polaris code on their behalf. A tutorial that only works for one audience fails the other. The structure below ensures a single document serves both.

### Tutorial Structure

Every tutorial follows the same section order:

| Section | Purpose |
|---------|---------|
| **When to Use** | Which problem this pattern solves and when to reach for it |
| **Prerequisites** | Which tutorials to read first |
| **Concepts** | Mental model — *why* this works, design rationale |
| **Step-by-Step Build** | Progressive construction with explanation |
| **Complete Example** | Full compilable implementation — no elisions, every import shown |
| **Graph Topology** | ASCII or Mermaid diagram of the agent's structure |
| **Adapting This Pattern** | Substitution points for customization |
| **Constraints** | Polaris lint rules and project conventions |
| **Common Mistakes** | Explicit anti-patterns and "DON'T" rules |

### Code Rules

- **No elisions in Complete Example.** Every code block in that section must compile as-is. `// ...` is allowed only in the progressive "Step-by-Step" section.
- **Every import shown.** Complete examples start with `use` statements. No guessing which crate provides a type.
- **Lint-clean.** All code passes `cargo clippy --all-targets --all-features -- -D warnings` and `cargo fmt -- --check`.
- **Self-contained.** Each complete example works independently. Only conceptual prerequisites, not code dependencies across tutorials.
- **Consistent naming.** Across all tutorials, same concepts use same names (e.g., `reason`, `execute_tools`, `finalize`).

### Constraints Block

Every tutorial ends with a Constraints section encoding project conventions:

```markdown
## Constraints
- Use `#[expect(..., reason = "...")]` not `#[allow(...)]`
- Use `tracing` macros, never `println!` / `eprintln!`
- Error bindings must be descriptive — `e` is disallowed
- All public items must have doc comments
- Unsafe blocks require `// SAFETY:` comments
```

This is intentionally repetitive across tutorials so each one is self-contained.

---

## What Ships

Each tutorial includes:

| Artifact | Location | Description |
|----------|----------|-------------|
| Tutorial document | `docs/tutorials/NN-title.md` | Markdown following the structure above |
| Working example | `examples/tutorials/NN-title/` | Complete Rust code that compiles and runs |
| Tests | In example `#[cfg(test)]` blocks | Unit and/or integration tests for the example |

---

## Tutorial List

The tutorials are split into two tracks:

- **Core Track** (#1-6) — the minimum path to "an LLM can read these and build a Polaris agent with tools." Prioritized, compressed, all unblocked now.
- **Advanced Track** (#7-18) — deeper patterns unlocked progressively as framework features ship.

### Critical Path: Core Track

The core track gets users (and LLMs) to a working agent with custom tools in 6 tutorials. The original 24-tutorial plan had 12 tutorials before tools were even introduced. The core track compresses this by merging tutorials that teach variations of the same concept.

| # | Title | Industry Pattern |
|---|-------|-----------------|
| 1 | Quickstart | Augmented LLM |
| 2 | Control Flow | Prompt Chaining, Routing, Parallelization |
| 3 | The ReAct Loop | ReAct |
| 4 | Error Handling | Error Recovery |
| 5 | Building Tools | Tool Design (ACI) |
| 6 | Testing Agents | Testing & Evaluation |

**Why this ordering:** After Tutorial 5, an LLM reading from GitHub or the docs website has everything it needs to build a Polaris agent with custom tools. Tutorial 6 gives it the ability to verify its own output. This is the milestone that makes Polaris self-reinforcing — agents can build agents.

**Key insight: Tools are unblocked now.** The tool *framework* (`#[tool]`, `#[toolset]`, `ToolRegistry`, `ToolPermission`) is already shipped. Phase 1E (sc-3194-3197) adds *built-in code tools* (grep, read, edit), but a tools tutorial using custom tools needs no framework changes.

### Advanced Track

| # | Title | Industry Pattern | Polaris Feature |
|---|-------|-----------------|-----------------|
| 7 | Hooks | Observability | `HooksAPI`, observer/provider hooks, schedules |
| 8 | Middleware | Observability | `MiddlewareAPI`, targets, chaining |
| 9 | Custom Schedules | Observability | Custom `Schedule` impls, targeted hooks |
| 10 | Guardrails | Guardrails | Hooks + validation systems |
| 11 | Context Engineering | Context Engineering | Memory primitives, context management |
| 12 | Orchestrator-Workers | Orchestrator-Workers | Scope node (Isolated), parallel workers |
| 13 | Evaluator-Optimizer | Evaluator-Optimizer | Loop with quality predicate |
| 14 | Agent Handoffs | Decentralized Handoff | Scope node, context chains |
| 15 | Streaming | Streaming | `IOContent::TextDelta`, SSE |
| 16 | Human-in-the-Loop | Human-in-the-Loop | AppPlugin, approval endpoints |
| 17 | Checkpointing and Recovery | Checkpointing / Durable Execution | Sessions, rollback API |
| 18 | Building a ReWOO Agent | ReWOO / Plan-then-Execute | Dynamic node, parallel tools |

### Tutorial Descriptions

#### Core Track

**1. Quickstart — Build Your First Agent**
Build a complete agent from scratch in one tutorial: set up a `Server` with plugins, define systems that read `Res<Config>` and write `ResMut<Memory>`, build a 3-node graph (receive input -> call LLM -> respond), and run it with `GraphExecutor`. Covers `Server`, `Plugin` (with `build()`, `ready()`, `cleanup()`, dependency declaration), `Graph`, `SystemContext`, global vs. local resources, `Res<T>`, `ResMut<T>`, and how to swap plugin implementations without changing agent code. The example is a standalone binary.

**2. Control Flow — Chaining, Routing, and Parallel Execution**
Build three variations of the same agent to show how graph topology shapes behavior. Start with a sequential pipeline using `add_system` chaining and `Out<T>` for typed data flow between steps (prompt chaining). Add a `add_switch` node for intent classification and routing to specialized handlers (routing). Add `add_parallel` for concurrent subtask execution with aggregation (parallelization). All three patterns share the same `Server` setup and plugin scaffold established in Tutorial 1.

**3. The ReAct Loop — Reasoning and Acting**
Build a full ReAct agent using `add_loop` with `add_conditional_branch` inside. Reason -> decide if tool needed -> execute tool -> observe -> loop. The canonical agent pattern. This tutorial introduces the `Agent` trait — packaging the graph as a reusable `impl Agent` with `build()` and `name()`. The example agent uses mock tools (real tools come in Tutorial 5).

**4. Error Handling and Retry — Building Resilient Agents**
Add error handlers, timeout handlers, and retry policies to the ReAct agent from Tutorial 3. Distinguish agentic errors (LLM refusal, tool returning invalid result) from infrastructure errors (missing resource, network failure). Use `with_timeout`, `with_retry`, `on_error`, and `add_error_handler`. Show how error edges route to recovery subgraphs while infrastructure errors propagate to the caller.

**5. Building Tools — The Agent-Computer Interface**
Define custom tools using the `#[tool]` macro with typed inputs and outputs. Follow Anthropic's ACI principles: consolidate multi-step operations into single tools, namespace by service, return token-efficient responses. Register tools via `ToolRegistry` in a plugin. Wire tools into the ReAct agent from Tutorial 3, replacing mock tools with real ones. The example builds a small toolset (e.g., calculator, weather lookup, text transformer) to demonstrate the pattern without requiring external APIs.

**6. Testing Agents — From Unit Tests to End-to-End Evaluation**
Test systems in isolation with mock resources. Test complete graphs with a `MockLLMPlugin` that returns deterministic responses. Build outcome-based evaluations that check *what the agent produced*, not *which steps it took*. Cover the test pyramid: unit tests for individual systems, integration tests for graph execution, evaluation tests for agent behavior. The example tests the ReAct agent from Tutorial 3.

#### Advanced Track

**7. Hooks — Observing Graph Execution**
Register observer hooks for logging and metrics (`OnGraphStart`, `OnSystemComplete`, `OnLoopIteration`). Register provider hooks that inject `SystemInfo` before each system executes. Build a plugin that tracks execution metrics (node count, duration, error rate) via hooks.

**8. Middleware — Wrapping Execution with Cross-Cutting Logic**
Build middleware for timing, token counting, and rate limiting. Show how middleware targets (`System`, `Loop`, `GraphExecution`) control scope. Chain multiple middleware layers. Contrast with hooks: middleware wraps execution (can short-circuit), hooks observe it.

**9. Custom Schedules — Fine-Grained Hook Targeting**
Define custom schedules (`OnToolCall`, `OnLlmCall`) and attach them to specific system nodes. Subscribe hooks to custom schedules for targeted observability without noise from unrelated systems.

**10. Guardrails — Input Validation, Output Filtering, and Safety**
Build guardrail systems that run before/after agent logic using hooks on `OnSystemStart`/`OnSystemComplete`. Implement content filtering, PII detection, and tool-call safety checks. Package as a `GuardrailsPlugin` that can be added to any agent.

**11. Context Engineering — Managing What the Agent Knows**
Implement progressive context loading: start with minimal context, use tool calls to fetch more. Build a resource that summarizes conversation history (compaction). Show how `LocalResource` keeps per-agent state isolated.

**12. Orchestrator-Workers — Delegating to Specialized Agents**
Build a lead agent that decomposes a task and spawns worker agents. Each worker is a separate `Agent` impl with its own graph, running in a Scope (Isolated) context. The orchestrator collects results and synthesizes.

**13. Evaluator-Optimizer — Generate and Critique Loops**
Build a two-agent loop: generator produces output, evaluator critiques it, generator refines. Use `add_loop` with a quality threshold predicate.

**14. Agent Handoffs — Decentralized Multi-Agent Flows**
Implement peer-to-peer agent transitions: triage agent -> specialist agent -> resolution agent. Each agent is a plugin. Show how `SystemContext` parent-child chains enable shared state across agents.

**15. Streaming — Delivering Partial Results**
Implement SSE streaming for LLM token output. Build an `IOStream` resource that pushes chunks to clients as they arrive, not just complete messages.

**16. Human-in-the-Loop — Approval Gates and Intervention Points**
Add decision nodes that pause execution and wait for human input. Build an approval system for high-risk tool calls. Use the `AppPlugin` HTTP layer to expose approval endpoints.

**17. Checkpointing and Recovery — Long-Running Agent State**
Serialize agent state at checkpoints. Resume from the last checkpoint after failure. Use sessions and the rollback API to implement durable execution for multi-turn agents.

**18. Building a ReWOO Agent — Plan-Then-Execute**
Implement the ReWOO pattern: planner generates a full tool-use plan upfront, executor runs all tool calls (potentially in parallel), then the agent synthesizes. Contrast with ReAct's interleaved approach.

---

## Roadmap

### Alignment with Unified Polaris Roadmap

```
Unified Roadmap Phase          Tutorial Track               Tutorials
─────────────────────          ──────────────               ─────────
Already Complete          ───► Core Track (NOW)              #1-6

Already Complete          ───► Adv: Observability (NOW)      #7-9
Already Complete          ───► Adv: Safety (NOW)             #10

Phase 1F: Memory          ───► Adv: Context & Memory         #11

Phase 1B: Scope/Dynamic   ─┬─► Adv: Multi-Agent              #12-14
Phase 1F: Memory          ─┘

Phase 1J/1K: HTTP         ───► Adv: Production               #15-17

Phase 1B + 2A             ───► Adv: Advanced Patterns        #18
```

**All 6 core track tutorials are unblocked right now.** After the core track ships, LLMs reading from GitHub or the website can build Polaris agents with custom tools. The advanced track unlocks progressively as framework features ship.

### Core Track (priority — all unblocked now)

*The minimum path to "an LLM can build a Polaris agent with tools." All framework dependencies already complete.*

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 1 | Write "Quickstart" tutorial | docs | 2449 | — | Server, plugins (build/ready/cleanup, dependencies, swapping), systems, resources (global/local, Res/ResMut), first 3-node graph, GraphExecutor. Standalone binary. |
| 2 | Write "Control Flow" tutorial | docs | 2449 | #1 | Three graph variations: sequential chaining with `Out<T>`, routing with `add_switch`, parallel with `add_parallel`. Same Server scaffold as #1. |
| 3 | Write "The ReAct Loop" tutorial | docs | 2449 | #2 | `add_loop` + `add_conditional_branch`. Introduces `Agent` trait (`build`, `name`). Uses mock tools (real tools in #5). |
| 4 | Write "Error Handling" tutorial | docs | 2449 | #3 | Error edges, timeout handlers, retry policies on the ReAct agent. Agentic vs. infrastructure errors. |
| 5 | Write "Building Tools" tutorial | docs | 2449 | #3 | `#[tool]` macro, `ToolRegistry`, custom tools, ACI principles. Replaces mock tools in the ReAct agent with real ones. |
| 6 | Write "Testing Agents" tutorial | docs | 2449 | #3 | Mock resources, `MockLLMPlugin`, outcome-based evals. Tests the ReAct agent. Unit -> integration -> eval pyramid. |

**Milestone:** After #5, agents (human or LLM) can build Polaris agents with custom tools. After #6, they can verify their own output.

### Advanced Track

*Deeper patterns, unlocked progressively as framework features ship.*

#### Observability (unblocked now)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 7 | Write "Hooks" tutorial | docs | 2449 | #3 | Observer hooks, provider hooks, lifecycle schedules. Metrics plugin via hooks. |
| 8 | Write "Middleware" tutorial | docs | 2449 | #7 | Timing, token counting, rate limiting. Targets, chaining. Contrast with hooks. |
| 9 | Write "Custom Schedules" tutorial | docs | 2449 | #7 | Custom `Schedule` impls (`OnToolCall`, `OnLlmCall`). Targeted hook subscription. |

#### Safety (unblocked now — hooks + tool framework are done)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 10 | Write "Guardrails" tutorial | docs | 2449 | #5, #7 | Input validation, output filtering, PII detection via hooks. `GuardrailsPlugin`. |

#### Context & Memory (blocked on Phase 1F)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 11 | Write "Context Engineering" tutorial | docs | 2449 | #2, sc-3199, sc-3200 | Progressive context loading, compaction, `LocalResource` isolation. |

#### Multi-Agent (blocked on Phase 1B + 1F)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 12 | Write "Orchestrator-Workers" tutorial | docs | 2449 | #2, #5, sc-3186, sc-3199 | Lead agent + Scope (Isolated) workers. Parallel sub-agents. |
| 13 | Write "Evaluator-Optimizer" tutorial | docs | 2449 | #3, sc-3199 | Generate-critique loop. Quality threshold predicate. |
| 14 | Write "Agent Handoffs" tutorial | docs | 2449 | #2, sc-3186 | Triage -> specialist -> resolution. Context chains. |

#### Production (blocked on Phase 1J/1K)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 15 | Write "Streaming" tutorial | docs | 2449 | #1, sc-3223, sc-3232 | SSE streaming, `IOContent::TextDelta`, `HttpIOProvider`. |
| 16 | Write "Human-in-the-Loop" tutorial | docs | 2449 | #4, sc-3223, sc-3224, sc-3225 | Approval gates, HTTP endpoints for high-risk tool calls. |
| 17 | Write "Checkpointing and Recovery" tutorial | docs | 2449 | sc-3226 | Session persistence, rollback API, durable execution. |

#### Advanced Patterns (blocked on Phase 1B + 2A)

| # | Title | Type | Epic | Depends On | Scope |
|---|-------|------|------|------------|-------|
| 18 | Write "Building a ReWOO Agent" tutorial | docs | 2449 | #2, #5, sc-3187, sc-3199 | Plan-then-execute. Dynamic node. Contrast with ReAct. |

---

## Dependency Stack

```
Advanced: ReWOO (#18)
  |-- depends on Phase 1B Dynamic node (sc-3187)
  '-- depends on Phase 1F memory (sc-3199)

Advanced: Production (#15-17)
  '-- depends on Phase 1J/1K HTTP infrastructure (sc-3223-3227, sc-3232)

Advanced: Multi-Agent (#12-14)
  |-- depends on Phase 1B Scope node (sc-3186)
  |-- depends on Phase 1F memory (sc-3199)
  '-- depends on Core Track

Advanced: Context & Memory (#11)
  '-- depends on Phase 1F memory (sc-3199, sc-3200)

Advanced: Guardrails (#10)        --+
Advanced: Observability (#7-9)      |-- all writable NOW
Core Track (#1-6)                 --+
```

---

## Summary

| Track | Tutorials | Blocked On | When Writable |
|-------|-----------|------------|---------------|
| **Core Track** | #1-6 | Nothing | **Now** |
| Observability | #7-9 | Nothing | **Now** |
| Safety | #10 | Nothing | **Now** |
| Context & Memory | #11 | Phase 1F | After memory ships |
| Multi-Agent | #12-14 | Phase 1B + 1F | After Scope + memory ship |
| Production | #15-17 | Phase 1J/1K | After HTTP infra ships |
| Advanced Patterns | #18 | Phase 1B + 2A | After Dynamic node + patterns ship |

**10 of 18 tutorials** (Core + Observability + Safety) can start immediately. The remaining 8 unlock as the framework matures.

---

## Enhancements

These cross-cutting improvements apply across tutorials and can be added incrementally:

**Hosted sandbox.** Compile and run user-written agent code on the website. Tutorial 1 (Quickstart) is the ideal first candidate — pre-built agent with configurable graph/systems, compiles in browser. Graph visualization (sc-3188) would let users see topology update live as they modify code.

**Graph visualization.** Once sc-3188 ships, add a "Visualize" section to each tutorial showing `graph.to_mermaid()` output. Particularly valuable for Tutorials 2 (Control Flow), 3 (ReAct Loop), and all multi-agent tutorials.

**CLI integration.** Each tutorial's complete example runs as a standalone binary — which is itself a CLI. Once `polaris-code` ships (Phase 3A), tutorials can show how to plug custom agents into it via the `--pattern` flag.

**Fixed scaffold.** Tutorial 1 establishes the standard interaction model (Server + Plugins + Graph + Executor). All subsequent tutorials reuse the same setup, only varying the graph topology and systems. The "Complete Example" in each tutorial always shows the full `main()` function with the same scaffold.

---

## Open Questions

1. **Hosted sandbox**: Should tutorials be runnable in a browser-based sandbox? Option A (pre-built agents with configurable knobs) is lowest effort for the core track. Option B (embedded Rust editor with container compilation) is higher effort but covers all tutorials.
2. **Ticket granularity**: One ticket per tutorial (18 tickets) or one ticket per track (core + advanced groups)?
3. **Epic**: New "Tutorials" epic, or under existing Documentation (2449)?
