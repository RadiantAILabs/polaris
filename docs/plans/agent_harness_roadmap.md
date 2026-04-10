---
notion_page: https://www.notion.so/radiant-ai/Agent-Harness-Roadmap-327afe2e695d8018b229d1f08c9e0a95
title: Agent Harness Roadmap
---

# Agent Harness Roadmap

**Date:** 2026-03-17

**Goal:** Make Polaris competitive with viral agentic open-source projects by shipping graph composition primitives, streaming, memory, MCP, 6 real-world agent patterns, and viral surface products (CLI, MCP server, arena).

**References:**
- `docs/research/competitive-analysis.md` — market analysis and gaps
- `docs/research/agentic-landscape.md` — architecture deep dives of 11+ projects
- `docs/design/scope-and-dynamic-nodes.md` — Scope/Dynamic node design doc

---

## Phase 1A: Graph Primitives (Layer 2 — `polaris_graph` + `polaris_system`)

*Adds Scope and Dynamic nodes, enabling agent composition and runtime graph generation.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 1 | Add `clone_local_resource` and `with_globals` to SystemContext | feature | L1 | Primitive | 2350 | — | Add `clone_local_resource(TypeId)` method and `SystemContext::with_globals()` constructor to `polaris_system`. Required for resource forwarding across scope boundaries. |
| 2 | Add ContextPolicy and ScopeNode | feature | L2 | Primitive | 2352 | #1 | Add `ContextPolicy` (Shared/Inherit/Isolated + forward), `ScopeNode` struct, `Node::Scope` variant, `add_scope` builder method, execution logic for all three context modes, validation (recursive into embedded graph), hook schedules (OnScopeStart/OnScopeComplete), middleware target. Per design doc. |
| 3 | Add DynamicNode and DynamicValidation | feature | L2 | Primitive | 2352 | #2 | Add `BoxedGraphFactory`, `DynamicValidation` (max_nodes, max_depth, require_loop_limits, require_timeouts, allow_nested_dynamic), `DynamicNode` struct, `Node::Dynamic` variant, `add_dynamic`/`add_dynamic_with` builder methods, 7-step execution sequence (factory → patch → enforce → validate → context → execute → merge), error edge support for factory failures, new `ExecutionError` variants, hooks. Per design doc. |
| 4 | Add graph visualization (ASCII + Mermaid export) | feature | L2 | Resource/API | 2352 | #2 | Add `graph.to_mermaid()` and `graph.to_ascii()` methods to `polaris_graph`. Render all node types (System, Decision, Switch, Parallel, Loop, Scope, Dynamic) with edges. Scope nodes rendered as subgraphs. Dynamic nodes as dashed placeholders. |

## Phase 1B: LLM Streaming (Layer 3 — `polaris_models` + `polaris_model_providers`)

*Adds streaming generation to the LLM provider interface.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 5 | Add streaming to LlmProvider trait | feature | L3 | Trait | 2351 | — | Add `stream` method to `LlmProvider` returning `Pin<Box<dyn Stream<Item = StreamEvent>>>`. Add `StreamEvent` enum (TextDelta, ToolCallStart, ToolCallDelta, Done). Add `generate_stream` to `Llm` and `LlmRequestBuilder`. Default impl falls back to `generate`. |
| 6 | Implement streaming for Anthropic provider | feature | L3 | Plugin | 2351 | #5 | Implement `stream` on the Anthropic provider using SSE event parsing. Handle text_delta, content_block_start/stop, tool_use events. |
| 7 | Implement streaming for OpenAI provider | feature | L3 | Plugin | 2351 | #5 | Implement `stream` on the OpenAI provider using SSE event parsing for the Responses API streaming format. |

## Phase 1C: Memory (Layer 3 — new `polaris_memory` crate)

*Adds pluggable memory with vector + keyword hybrid recall.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 8 | Define Memory trait and MemoryPlugin | feature | L3 | Trait + Resource/API | 2351 | — | New `polaris_memory` crate. Define `MemoryStore` trait (`save`, `recall`, `forget`), `MemoryEntry` type, `RecallQuery` (text + optional filters), `RecallResult` (entries + scores). Add `MemoryPlugin` registering `MemoryStore` as a global resource. |
| 9 | Implement SQLite hybrid memory backend | feature | L3 | Plugin | 2351 | #8 | Add `SqliteMemory` implementing `MemoryStore`. Dual indexing: vector store (cosine similarity via embedded embeddings) + FTS5 full-text search with BM25 ranking. Weighted merge of scores. Configurable chunking. |

## Phase 1D: MCP Client (Layer 3 — new `polaris_mcp` crate)

*Connects Polaris agents to external MCP tool servers.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 10 | Implement MCP client protocol | feature | L3 | Trait + Resource/API | 2351 | — | New `polaris_mcp` crate. Implement MCP client (protocol version `2024-11-05`). Support stdio and SSE transports. `McpClient` connects to external MCP servers, discovers tools. `McpToolBridge` wraps MCP tools into Polaris `Tool` trait implementations with namespaced names (`server__tool`). `McpPlugin` registers bridges into `ToolRegistry`. |

## Phase 1E: Agent Pattern Crates (Layer 3 — `examples/` or new crates)

*Six real-world agent architectures as Polaris graph implementations.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 11 | Implement `claude_code` agent pattern | feature | L3 | App | 2393 | #2, #5 | While-loop with tool calling + parallel sub-agent spawning via Scope nodes (Isolated context). Demonstrates: Loop, Conditional Branch, Parallel of Scopes. |
| 12 | Implement `openclaw` agent pattern | feature | L3 | App | 2393 | #8 | Gateway-dispatched loop with layered memory recall. Demonstrates: Sequential context assembly, tool loop, memory integration, proactive trigger hook. |
| 13 | Implement `devin` agent pattern | feature | L3 | App | 2393 | #3 | Multi-model planner/coder/critic pipeline with dynamic re-planning. Demonstrates: Nested Loop wrapping Dynamic node, multi-model ModelRegistry usage, conditional critic gate. |
| 14 | Implement `cursor` agent pattern | feature | L3 | App | 2393 | #3 | Plan-execute-verify with codebase RAG. Demonstrates: Dynamic node (plan → graph), verification conditional branch, non-LLM tool gates (linter/test). |
| 15 | Implement `zeroclaw` agent pattern | feature | L3 | App | 2393 | — | Classify-route-execute with model routing for cost optimization. Demonstrates: Switch node for model selection, hybrid memory recall, security gate system. |
| 16 | Implement `openfang` agent pattern | feature | L3 | App | 2393 | #2 | Kernel-orchestrated workflow with RBAC/budget gating. Demonstrates: Sequential Scope nodes (Hand chaining), capability-check systems, workflow fan-out via Parallel. |

## Phase 1F: Documentation

*Updates docs to reflect all Phase 1 additions.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 17 | Document Scope and Dynamic nodes in reference docs | docs | L2 | — | 2449 | #3, #4 | Update `docs/reference/graph.md` with Scope/Dynamic node sections, builder API examples, context policy reference. Update `docs/taxonomy.md` if needed. Update `CLAUDE.md` quick reference tables. |
| 18 | Document streaming, memory, and MCP | docs | L3 | — | 2449 | #5, #8, #10 | Add streaming usage to models reference. Create `docs/reference/memory.md`. Create `docs/reference/mcp.md`. |

## Phase 2: First Viral Surface (Separate Repos)

*Delivers the first things people can install, run, and share.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 19 | Build `polaris-cli` binary | feature | App | App | 2393 | #4, #11-16 | Separate repo. CLI with `polaris run --pattern <name> "task"`, `polaris patterns` (list available), `polaris visualize <name>` (render graph as ASCII/Mermaid). Clap-based. Bundles all 6 agent patterns. The instant-gratification entry point. |
| 20 | Build `polaris-mcp` server | feature | App | App | 2393 | #10, #11-16 | Separate repo. MCP server (stdio + SSE) that exposes each agent pattern as an MCP tool. Connect Cursor or Claude Desktop → call `claude_code("task")` or `devin("task")`. One-line setup. |

## Phase 3: Amplifiers

*Creates the "wow" moments that drive sharing and virality.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 21 | Build `polaris-arena` TUI | feature | App | App | 2544 | #4, #5, #11-16 | Separate repo. Ratatui TUI that runs 2-4 agent patterns on the same task simultaneously. Side-by-side panels showing live execution (streaming), graph visualization, and metrics (LLM calls, tokens, latency). Produces shareable screenshots/GIFs. |
| 22 | Build `polaris-edge` minimal runtime | feature | App | App | 2393 | #15, #19 | Separate repo. Stripped Polaris binary for edge deployment (opt-level="z", LTO, strip). Targets ARM64/RISC-V. Runs against Ollama. Includes GPIO/sensor tool plugin. The "AI on a Raspberry Pi" demo. |

---

## Plugin Stack: Full Initiative

```
polaris-arena (TUI comparison app)
polaris-cli (CLI binary with --pattern flag)
polaris-mcp (MCP server for Cursor/Claude Desktop)
├── claude_code pattern
│   ├── polaris_graph (Scope node, Parallel)
│   │   └── polaris_system (clone_local_resource, with_globals)
│   └── polaris_models (streaming)
├── devin pattern
│   ├── polaris_graph (Dynamic node)
│   └── polaris_models (generate_structured — already exists)
├── cursor pattern
│   └── polaris_graph (Dynamic node)
├── openclaw pattern
│   └── polaris_memory (hybrid recall)
│       └── polaris_memory (MemoryStore trait)
├── zeroclaw pattern (no new deps — uses existing Switch, Loop)
├── openfang pattern
│   └── polaris_graph (Scope node)
└── polaris_mcp (MCP client for external tools)
    └── polaris_tools (Tool trait — already exists)
```

## Parallel Tracks

Several streams can be worked concurrently:

```
Track A (Graph):    #1 → #2 → #3 → #4 → #13, #14, #16
Track B (Stream):   #5 → #6, #7 → #11
Track C (Memory):   #8 → #9 → #12
Track D (MCP):      #10
Track E (Patterns): #15 (no blockers)
                    ──── converge on #17, #18, #19, #20 ────
                                 ──── then #21, #22 ────
```

## Notes

- **Structured output (`generate_structured<T>`) already exists** in `polaris_models`. No ticket needed.
- **Layer isolation**: #1 is the only L1 change. #2-#4 are L2. Everything else is L3 or App.
- **Separate repos**: #19-22 are separate repositories, not crates in polaris.
- **Agent patterns** (#11-16) can be in `examples/` initially and promoted to standalone crates later if warranted.
