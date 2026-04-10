---
notion_page: https://www.notion.so/radiant-ai/Unified-Polaris-Roadmap-327afe2e695d80c09b31d77efc701b65
title: Unified Polaris Roadmap
---

# Unified Polaris Roadmap

**Goal:** Ship `polaris-code` — a terminal coding agent built on Polaris with swappable agent architectures — then use the same framework to power viral surface products (arena, MCP server, HTTP agent server, edge runtime).

**Date:** 2026-03-30 (rev 3)

**Supersedes:**
- [`docs/plans/terminal-based-coding-agent.md`](terminal-based-coding-agent.md) (2026-03-10)
- [`docs/plans/agent_harness_roadmap.md`](agent_harness_roadmap.md) (2026-03-17)

**References:**
- [`docs/research/competitive-analysis.md`](../research/competitive-analysis.md)
- [`docs/research/agentic-landscape.md`](../research/agentic-landscape.md)
- [`docs/design/scope-and-dynamic-nodes.md`](../design/scope-and-dynamic-nodes.md)
- [`docs/design/memory.md`](../design/memory.md)
- [`docs/design/http-mcp-infrastructure.md`](../design/http-mcp-infrastructure.md)
- [`docs/design/pattern-requirements-and-agent-patterns.md`](../design/pattern-requirements-and-agent-patterns.md) (planned)

---

## What Polaris Looks Like When It's Done

### Compositional picture

Every layer is independently useful. The framework does not force you to use any layer above the one you need.

```
┌───────────────────────────────────────────────────────────────────────────┐
│               END PRODUCTS                                                │
│  polaris-code   polaris-http   polaris-mcp   polaris-arena   polaris-edge │
└──────────────────────┬────────────────────────────────────────────────────┘
                       │ select and configure
┌──────────────────────▼────────────────────────────────────────────────────┐
│               AGENT PATTERNS                                              │
│  claude_code  devin  cursor  openclaw  zeroclaw  openfang                 │
│  (each is an Agent impl that builds a different graph)                    │
└──────────────────────┬────────────────────────────────────────────────────┘
                       │ compose from
┌──────────────────────▼────────────────────────────────────────────────────┐
│               CAPABILITY PLUGINS (L3)                                     │
│                                                                           │
│  Models           Tools            Memory                                 │
│  ├ LlmProvider    ├ 13 built-in    ├ Store                                │
│  ├ Embedding      ├ MCP bridge     ├ Retrieve                             │
│  ├ TokenCounter   ├ Permissions    ├ Update                               │
│  └ 3 providers    └ Shell+confirm  └ Remove                               │
│                                                                           │
│  IO               Sessions         Workspace    MCP Client                │
│  ├ Streaming      ├ FileStore      ├ Project     ├ stdio                  │
│  ├ Highlighting   └ InMemory       └ Ignores     └ SSE                    │
│  └ Confirm UX                                                             │
│                                                                           │
│  App (shared HTTP infrastructure)                                         │
│  ├ AppPlugin (axum runtime, graceful shutdown)                            │
│  ├ HttpRouter API (plugins register routes during build)                  │
│  ├ HttpIOProvider (channel-based IO bridging)                             │
│  └ Tower middleware (CORS, tracing, auth hook)                            │
└──────────────────────┬────────────────────────────────────────────────────┘
                       │ built on
┌──────────────────────▼────────────────────────────────────────────────────┐
│               GRAPH EXECUTION (L2)                                        │
│  7 Node Types (incl. Scope + Dynamic)                                     │
│  6 Edge Types · ContextPolicy · DynamicValidation                         │
│  Hooks · Middleware · Agent trait · Visualization                         │
└──────────────────────┬────────────────────────────────────────────────────┘
                       │ built on
┌──────────────────────▼────────────────────────────────────────────────────┐
│               SYSTEM FRAMEWORK (L1)                                       │
│  Systems · Resources · Plugins · Server                                   │
│  SystemContext hierarchy · Dependency injection                           │
└───────────────────────────────────────────────────────────────────────────┘
```

### Building blocks by layer

**Layer 1 — System Primitives** (foundation)

| Block | Capability |
|-------|-----------|
| `System` + `#[system]` | Any async function becomes a unit of work |
| `Resource` (Global/Local) | Typed shared state with hierarchical scoping |
| `SystemContext` | Dependency injection — systems declare what they need |
| `Plugin` | Unit of composition — register resources, wire systems |
| `Server` | Orchestrates plugin lifecycle, creates execution contexts |
| `clone_local_resource` | Move resources across scope boundaries |

**Layer 2 — Graph Primitives** (agent structure)

| Block | Capability |
|-------|-----------|
| 7 Node Types | System, Decision, Switch, Parallel, Loop, Scope, Dynamic |
| 6 Edge Types | Sequential, Conditional, Parallel, LoopBack, Error, Timeout |
| Scope + ContextPolicy | Embed sub-graph with configurable isolation (Shared/Inherit/Isolated) |
| Dynamic + DynamicValidation | LLM generates a graph at runtime with safety constraints |
| Hooks + Middleware | Observe and intercept execution at every level |
| Agent trait | Package a pattern as a reusable graph builder with optional `PatternRequirements` |
| Graph visualization | Inspect topology as ASCII or Mermaid (including nested scopes) |

**Layer 3 — Capability Plugins** (what agents can do)

| Block | Capability |
|-------|-----------|
| LlmProvider (generate + stream) | Talk to any model — Anthropic, OpenAI, Bedrock with streaming |
| EmbeddingProvider | Embed text for semantic search |
| TokenCounter | Count tokens for any model |
| 13 built-in tools | read, write, edit, multi_edit, grep, glob, ls, shell, git (4), web_fetch |
| MCP bridge | External MCP server tools appear as native Polaris tools |
| Memory primitives | Store, retrieve, update, remove — unified abstractions for conversation history, long-term recall, or any agent state. Policies (capacity, indexing, scoping) are pattern-level opinions built on these primitives. |
| WorkspacePlugin | Project type detection, ignore patterns, file tree |
| Sessions | Persist and resume agent state across runs |
| IO system | Streaming terminal renderer, syntax highlighting, confirm UX |
| App infrastructure | Shared HTTP runtime (axum), route registration API, Tower middleware, `HttpIOProvider` for bridging HTTP to agent IO. Used by `polaris-http`, `polaris-mcp` (SSE transport), and `polaris-arena` (web dashboard). |

### Agent patterns

| Pattern | Architecture | Key primitives used |
|---------|-------------|---------------------|
| **claude_code** | While-loop + tool calling + parallel isolated sub-agents | Loop, Scope (Isolated), Parallel, core tools, streaming |
| **devin** | Planner → dynamic execution graph → critic → re-plan | Dynamic, Loop, multi-model via setup() |
| **cursor** | Plan → dynamic step graph → verify → fix-or-continue | Dynamic, WorkspacePlugin, shell verification |
| **openclaw** | Routing loop + memory recall/persist per turn | Loop, Decision, Memory |
| **zeroclaw** | Classify → route to fast or reasoning model | Switch, multi-model routing |
| **openfang** | Kernel → sequential/parallel Hand scopes + RBAC | Scope (Inherit), Parallel |

### End products

| Product | Description |
|---------|-------------|
| **polaris-code** | Terminal coding agent with `--pattern` flag to swap architectures. REPL with session persistence. |
| **polaris-http** | HTTP server exposing agents via REST + SSE. Session CRUD, turn processing, checkpoint/rollback. Enables web UIs, mobile clients, and service-to-service integration. |
| **polaris-mcp** | MCP server exposing Polaris patterns as tools for Cursor/Claude Desktop. Uses shared `AppPlugin` for SSE transport. |
| **polaris-arena** | TUI running 2-4 patterns on the same task side-by-side with live metrics |
| **polaris-edge** | Stripped binary for ARM64/RISC-V with local models (Ollama) |

---

## What is `polaris-code`?

A terminal-based coding agent (like Claude Code) whose agent architecture is swappable. By default it runs a `claude_code`-style while-loop, but can switch to `devin`, `cursor`, or other patterns.

### Pattern Contract

Each agent pattern is an `Agent` implementation that:
1. Implements the `Agent` trait (`build()` → graph, `setup()` → initialize resources)
2. Optionally declares required capabilities via `requirements()` returning `PatternRequirements`
3. The CLI validates requirements against available plugins before execution

`PatternRequirements` lives in `polaris_agent` (Layer 2) as a plain data struct:

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

Model role resolution (e.g., "planner", "coder", "critic") is an agent-level concern handled in `setup()`, not a framework validation concern.

---

## Tool Inventory

### Core Tools (required for all coding patterns)

| Tool | Signature | Description |
|------|-----------|-------------|
| **Read** | `read(path, offset?, limit?)` | Read file with optional line range. Returns numbered lines. |
| **Write** | `write(path, content)` | Create or overwrite a file. |
| **Edit** | `edit(path, old_string, new_string)` | Targeted string replacement. Fails if not unique. |
| **MultiEdit** | `multi_edit(edits: Vec<{path, old, new}>)` | Batch edits across files. |
| **Glob** | `glob(pattern, path?)` | Find files matching a glob pattern. |
| **Grep** | `grep(pattern, path?, type?, context?)` | Search file contents with regex. |
| **Shell** | `shell(command, working_dir?, timeout?)` | Execute shell command. Returns stdout, stderr, exit code. |
| **LS** | `ls(path)` | List directory contents. |

### Extended Tools (optional, enhance specific patterns)

| Tool | Signature | Description | Used by |
|------|-----------|-------------|---------|
| **GitStatus** | `git_status()` | Staged/unstaged/untracked files | claude_code, cursor |
| **GitDiff** | `git_diff(staged?)` | Diff of changes | claude_code, cursor |
| **GitLog** | `git_log(count?, path?)` | Recent commit history | claude_code |
| **GitCommit** | `git_commit(message, files?)` | Stage and commit | claude_code |
| **WebFetch** | `web_fetch(url)` | Fetch and extract text from URL | devin, openclaw |
| **WebSearch** | `web_search(query)` | Search the web | devin |
| **MemoryStore** | `memory_store(key, content, category?)` | Save to long-term memory | openclaw, zeroclaw |
| **MemoryRecall** | `memory_recall(query, limit?)` | Recall via hybrid search | openclaw, zeroclaw |

### MCP Bridge Tools (dynamic, registered at build time)

MCP tools bridged from external servers appear as native tools with namespaced names (`mcp_github__create_issue`). `McpPlugin` registers bridge tools during `build()` from config; actual connections are established lazily on first tool execution.

---

## Memory

Memory is all information an agent encounters during operation. The framework provides unified primitives — **store, retrieve, update, remove** — that agents use for any stateful need: conversation history, long-term recall, working scratch state, or anything else.

How memory is used is a pattern-level decision:

- A coding agent might store conversation turns and retrieve by recency
- A memory-backed agent might store durable facts and retrieve by semantic similarity
- A planning agent might store intermediate plans and retrieve by key

**Policies are opinions, not primitives.** Capacity management, indexing strategy, turn segmentation, compaction, and scoping are all pattern-level decisions built on top of the memory primitives, not prescribed by the framework.

### Context management

Context management is a subset of memory: given everything the agent remembers, what goes into the next LLM request? It is a projection of memory onto a model's finite context window.

Context management reads from memory and produces a bounded message list shaped by the model's budget (input limit, output reservation, system prompt overhead). Strategies like sliding window, compaction, and semantic retrieval are pattern-level policies — different patterns may manage context differently, all reading from the same memory primitives.

### Supporting infrastructure

Memory and context implementations may depend on shared model infrastructure:

- **TokenCounter** (in `polaris_models`) — count tokens for budget-aware context policies
- **EmbeddingProvider** (in `polaris_models`) — embed text for semantic retrieval policies

---

## Current State

### Done

- Core framework (Layer 1 + Layer 2): Complete
- LLM providers — Anthropic, OpenAI, Bedrock (non-streaming): Complete
- LLM streaming trait (`LlmProvider::stream()`, `StreamEvent`, `LlmStream`): Complete
- Tool framework (`#[tool]`, `#[toolset]`, `ToolRegistry`): Complete
- Tool permissions (`ToolPermission`: Allow/Confirm/Deny): Complete
- User confirmation (`UserIO::confirm()`, per-tool permission config): Complete
- IO system (`UserIO`, `IOProvider`): Complete
- Persistence (`Storable`, `PersistenceAPI`): Complete
- Sessions (`SessionsAPI`, `FileStore`, `InMemoryStore`): Complete
- Agent trait (`to_graph()` on `Agent`): Complete
- Shell execution (`polaris_shell`): Complete
- Structured output (`generate_structured<T>`): Complete
- Tools+LLM ergonomic sugar (sc-3137): Merged
- Native error propagation with CaughtError (sc-3139): Merged
- Middleware for Graph Execution (sc-3166): Merged
- Tracing instrumentation (LLM, tools, graph spans): Complete
- Token usage tracking: Complete
- Anthropic streaming (sc-3189): Complete
- Streaming from main (sc-3168): Complete
- `clone_local_resource` on SystemContext (sc-3185): Complete
- Make Plugin `ready()` and `cleanup()` async (sc-3230): Complete
- Create `polaris_app` crate with `AppPlugin` (sc-3223): Complete
- Session management REST endpoints (sc-3224): Complete
- Turn processing endpoint with IO bridging (sc-3225): Complete
- Checkpoint and rollback endpoints (sc-3226): Complete

### In Progress

- sc-3141: JSON Schema normalization hardening (Ready for Review)
- sc-3186: ContextPolicy and ScopeNode (Ready for Review)
- sc-3192: TokenCounter trait and tiktoken implementation (Ready for Review)
- sc-3181: Consolidate and expand lm_schema test suite (WIP)

### Existing Backlog

- sc-3158–3163: Graph manipulation utilities
- sc-3094: MemoryPlugin with `MemoryBackend` trait
- sc-3129: Separate Server/Graph schedules

### Created Tickets

Original 36 roadmap items have Shortcut tickets (sc-3185 through sc-3220). HTTP infrastructure added in rev 3: sc-3223 through sc-3229 (Phase 1J, 1K, docs, and polaris-http binary). See the Ticket column in each phase table below.

---

## Roadmap

### Phase 1A: System Primitives (Layer 1)

*Extends SystemContext for resource forwarding across scope boundaries.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| ~~sc-3185~~ | ~~Add `clone_local_resource` to SystemContext~~ | feature | L1 | 2350 | — | **Done.** `clone_local_resource(TypeId)` method on `SystemContext`. Required for Scope node resource forwarding. (`with_globals` and `insert_boxed` already exist.) |

### Phase 1B: Graph Primitives (Layer 2)

*Adds Scope, Dynamic nodes and graph visualization.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3186 | Add ContextPolicy and ScopeNode | feature | L2 | 2352 | sc-3185 | `ContextPolicy` (Shared/Inherit/Isolated + forward), `ScopeNode`, `Node::Scope`, `add_scope` builder, execution for all three modes, recursive validation, hooks, middleware target. Per design doc. |
| sc-3187 | Add DynamicNode and DynamicValidation | feature | L2 | 2352 | sc-3186 | `BoxedGraphFactory`, `DynamicValidation`, `DynamicNode`, `Node::Dynamic`, `add_dynamic`/`add_dynamic_with` builder, 7-step execution, error edges for factory failures, new `ExecutionError` variants. Per design doc. |
| sc-3188 | Add graph visualization (ASCII + Mermaid) | feature | L2 | 2352 | sc-3186 | `graph.to_mermaid()` and `graph.to_ascii()`. All node types including Scope (subgraph) and Dynamic (dashed placeholder). Supersedes sc-3158. |

### Phase 1C: LLM Provider Streaming (Layer 3)

*Concrete streaming implementations. The `LlmProvider::stream()` trait and `StreamEvent` types are already merged.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| ~~sc-3189~~ | ~~Implement streaming for Anthropic provider~~ | feature | L3 | 2351 | — | **Done.** SSE parsing for Messages API. text_delta, content_block_start/stop, tool_use events. |
| sc-3190 | Implement streaming for OpenAI provider | feature | L3 | 2351 | — | SSE parsing for Responses API streaming format. |
| sc-3191 | Implement streaming for Bedrock provider | feature | L3 | 2351 | — | Bedrock `converseStream()` API support. |

### Phase 1D: Models Infrastructure (Layer 3)

*Shared tokenization and embedding infrastructure in `polaris_models`.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3192 | Add TokenCounter trait and tiktoken implementation | feature | L3 | 2351 | — | `TokenCounter` trait + `TiktokenCounter` impl in `polaris_models`. Model-family tokenizer mapping, heuristic fallback for unknown models. Shared by context management and memory chunking. |
| sc-3193 | Add EmbeddingProvider trait | feature | L3 | 2351 | — | `EmbeddingProvider` trait in `polaris_models` (sibling to `LlmProvider`). `embed(inputs: Vec<String>) -> Vec<Vec<f32>>`, `dimensions()`. Required by memory backend for vector indexing. |

### Phase 1E: Tools (Layer 3)

*Concrete tools for coding agent patterns. Builds on `polaris_shell`.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3194 | Add code search tools (grep, glob, ls) | feature | L3 | 2351 | — | `grep(pattern, path, context_lines, type_filter)`, `glob(pattern, path)`, `ls(path)`. Regex via `grep` crate, glob via `globset`. |
| sc-3195 | Add file read/write/edit tools | feature | L3 | 2351 | — | `read(path, offset, limit)` with line numbers, `write(path, content)`, `edit(path, old_string, new_string)` with uniqueness check, `multi_edit(edits)`. |
| sc-3196 | Add git integration tools | feature | L3 | 2351 | — | `git_status`, `git_diff(staged?)`, `git_log(count?)`, `git_commit(message, files)`. Wraps git CLI via `polaris_shell`. |
| sc-3197 | Add web fetch tool | feature | L3 | 2351 | — | `web_fetch(url)` — HTTP GET, extract text content, strip HTML. `reqwest` + HTML-to-text. |
| sc-3198 | Add workspace awareness plugin | feature | L3 | 2351 | sc-3194 | `WorkspacePlugin` — detect project type, provide project root, load ignore patterns, build file tree. |

### Phase 1F: Memory (Layer 3)

*Unified memory primitives (store, retrieve, update, remove) plus context management as a projection of memory onto LLM context windows. Per design doc `docs/design/memory.md` (planned). Supersedes sc-3094.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3199 | Define Memory primitives and plugin | feature | L3 | 2351 | sc-3192 | New `polaris_memory` crate. Unified memory abstractions (store, retrieve, update, remove). In-memory backend for conversation-scoped use. `MemoryPlugin`. Supersedes sc-3094. |
| sc-3200 | Add context management layer | feature | L3 | 2351 | sc-3199 | Context management as a projection of memory onto LLM context windows. Budget accounting (input limit, output reservation, system prompt overhead). Pluggable strategies for how to select/shape memory into a request. |
| sc-3201 | Add durable memory backend | feature | L3 | 2351 | sc-3199 | SQLite backend for cross-session memory. Durable persistence as a single file. |
| sc-3202 | Add semantic retrieval support | feature | L3 | 2351 | sc-3201, sc-3192, sc-3193 | Semantic retrieval via `EmbeddingProvider`. Keyword retrieval via FTS5. Hybrid scoring. Pluggable into any memory backend. |

### Phase 1H: MCP Client (Layer 3)

*Connects to external MCP tool servers. Per design doc `docs/design/http-mcp-infrastructure.md` (planned).*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3203 | Implement MCP client protocol | feature | L3 | 2351 | — | New `polaris_mcp` crate. MCP client (protocol `2024-11-05`), stdio + SSE transports. Lazy-init bridges: `build()` registers tools from config schemas, connections established on first `execute()`. `McpToolBridge` wraps as Polaris `Tool` impls. `McpPlugin` registers into `ToolRegistry`. |

### Phase 1I: Terminal UX (App)

*Streaming rendering and syntax highlighting.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3204 | Add streaming terminal renderer | feature | App | 2544 | — | `StreamingIOProvider` — progressive token rendering, markdown code blocks, spinners for tool calls, diff rendering for edits. |
| sc-3205 | Add syntax highlighting for code output | feature | App | 2544 | sc-3204 | `syntect`-based highlighting for code blocks. Language auto-detection. |

### Phase 1J: App Infrastructure (Layer 3)

*Shared HTTP server runtime. `polaris_app` provides an axum-based HTTP runtime with a route registration API that any plugin can use. This is the shared foundation for `polaris-http`, `polaris-mcp` (SSE transport), and any future product that needs an HTTP interface. Axum was chosen for native Tower middleware support (aligns with Polaris's composable middleware philosophy), `Router::merge()` for plugin-based route composition, built-in SSE support, and Tokio-native runtime alignment.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| ~~sc-3223~~ | ~~Create `polaris_app` crate with `AppPlugin`~~ | feature | L3 | 2351 | — | **Done.** New `crates/polaris_app`. `AppConfig` global resource, `HttpRouter` API, `AppPlugin` with axum server, graceful shutdown, Tower middleware (CORS, tracing, auth), `HttpIOProvider` bridging HTTP to agent `UserIO`. |
| sc-3232 | Support token-level streaming in IO types and `HttpIOProvider` | feature | L3 | 2351 | sc-3223 | Add `IOContent::TextDelta(String)` variant for incremental text chunks. Derive `Serialize` on `IOSource`, `IOContent`, `IOMessage` for SSE `Event::json_data()`. Implement `HttpIOProvider::stream()` returning an `IOStream` backed by the output channel. Enables sc-3227 and sc-3218 to build on token-level streaming from the start rather than retrofitting message-level streaming. |

### Phase 1K: HTTP Session Endpoints (Layer 3)

*REST + SSE endpoints for serving agents over HTTP. Lives in `polaris_sessions` behind `feature = "http"` — keeps transport and domain in one crate while remaining fully additive. No new crate; code in `polaris_sessions/src/http/`.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| ~~sc-3224~~ | ~~Add session management REST endpoints~~ | feature | L3 | 2351 | sc-3223 | **Done.** `http` feature flag on `polaris_sessions`. `HttpPlugin` with session CRUD endpoints (`POST/GET/DELETE /v1/sessions`). JSON DTOs, error responses. |
| ~~sc-3225~~ | ~~Add turn processing endpoint with IO bridging~~ | feature | L3 | 2351 | sc-3224 | **Done.** `POST /v1/sessions/:id/turns` with `HttpIOProvider` bridging. `process_turn_with()` setup closure, `OutputBuffer` JSON response. |
| ~~sc-3226~~ | ~~Add checkpoint and rollback endpoints~~ | feature | L3 | 2351 | sc-3224 | **Done.** Checkpoint create/list and rollback endpoints. Thin wrapper over `SessionsAPI`. |
| sc-3227 | Add SSE streaming for turn execution | feature | L3 | 2351 | sc-3225, sc-3232 | `POST /v1/sessions/:id/turns` with `Accept: text/event-stream` returns SSE stream via `axum::response::Sse`. Streaming `HttpIOProvider` variant forwards `UserIO.send()` calls as real-time SSE events with token-level granularity (via `IOContent::TextDelta`). Event types: `message` (agent text), `tool_call` (tool invocation), `tool_result` (tool output), `done` (turn complete with `ExecutionResult`). Falls back to buffered JSON response without the Accept header. Builds on completed `LlmProvider::stream()` + `StreamEvent` infrastructure and sc-3232 IO streaming primitives. |

### Phase 2A: Agent Patterns (Layer 3)

*Six real-world architectures as swappable Polaris graph implementations. Per design doc `docs/design/pattern-requirements-and-agent-patterns.md` (planned). Topologies are reference implementations based on open-source agent analysis; actual implementations will closely match the real agents and may diverge.*

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3206 | Implement `claude_code` pattern | feature | L3 | 2393 | sc-3186, sc-3194, sc-3195, sc-3199 | `polaris_pattern_claude_code` crate. While-loop + tool calling + parallel sub-agent spawning via Scope (Isolated). Default pattern. Includes adding `PatternRequirements` struct and `requirements()` default method to `Agent` trait in `polaris_agent`. |
| sc-3207 | Implement `devin` pattern | feature | L3 | 2393 | sc-3187, sc-3199 | `polaris_pattern_plan_execute` crate. Multi-model planner/coder/critic with dynamic re-planning. Nested Loop + Dynamic node. Model roles resolved in `setup()`. |
| sc-3208 | Implement `cursor` pattern | feature | L3 | 2393 | sc-3187, sc-3198, sc-3199 | `polaris_pattern_plan_execute` crate. Plan-execute-verify with codebase RAG. Dynamic node, verification branch. |
| sc-3209 | Implement `openclaw` pattern | feature | L3 | 2393 | sc-3199, sc-3201 | `polaris_pattern_routing_loop` crate. Gateway-dispatched loop with layered memory. Memory integration. |
| sc-3210 | Implement `zeroclaw` pattern | feature | L3 | 2393 | sc-3199 | `polaris_pattern_routing_loop` crate. Classify-route-execute with model routing. Switch node, cost-optimized routing. |
| sc-3211 | Implement `openfang` pattern | feature | L3 | 2393 | sc-3186, sc-3199 | `polaris_pattern_openfang` crate. Kernel-orchestrated workflow with capability gating. Sequential Scopes, RBAC, fan-out. |

### Phase 2B: Documentation

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3212 | Document Scope and Dynamic nodes | docs | L2 | 2449 | sc-3187, sc-3188 | Update `docs/reference/graph.md`, `docs/taxonomy.md`, `CLAUDE.md`. |
| sc-3213 | Document memory, MCP | docs | L3 | 2449 | sc-3200, sc-3201, sc-3203 | New `docs/reference/memory.md`, `docs/reference/mcp.md`. |
| sc-3214 | Document pattern contract and tool inventory | docs | L3 | 2449 | sc-3206 | New `docs/reference/patterns.md`. |
| sc-3229 | Document HTTP API and App infrastructure | docs | L3 | 2449 | sc-3227 | New `docs/reference/http.md` — all endpoints, request/response schemas, SSE event format, error codes, authentication, `AppPlugin` configuration, route registration API for plugin authors. Curl examples for every endpoint. |

### Phase 3A: Ship polaris-code

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3215 | Build `polaris-code` binary | feature | App | 3167 | sc-3204–sc-3211 | CLI: default `claude_code` pattern, `--pattern` flag, REPL with `/compact`, `/pattern`, `/visualize`. Session persistence. |
| sc-3216 | Add integration tests with mock LLM | feature | App | 3167 | sc-3215 | End-to-end tests: mock provider + mock IO. Verify file ops, shell, edits. Test each pattern. |
| sc-3217 | Binary packaging and release automation | chore | App | 3167 | sc-3215 | GitHub Actions cross-platform builds. `cargo install`. Homebrew formula. |

### Phase 3B: Viral Surface Products

| Ticket | Title | Type | Layer | Epic | Depends On | Scope |
|--------|-------|------|-------|------|------------|-------|
| sc-3228 | Build `polaris-http` example binary | feature | App | 2393 | sc-3225, sc-3227, sc-3206–sc-3211 | HTTP-served agent binary. Wires `AppPlugin` + `HttpPlugin` + model/tool plugins + agent patterns. Config-driven agent selection. Example curl walkthrough and optional minimal HTML client. |
| sc-3218 | Build `polaris-mcp` server | feature | App | 2393 | sc-3203, sc-3223, sc-3232, sc-3206–sc-3211 | MCP server exposing patterns as tools. Uses shared `AppPlugin` for SSE transport with token-level streaming (stdio transport remains independent). Connect Cursor/Claude Desktop. |
| sc-3219 | Build `polaris-arena` TUI | feature | App | 2544 | sc-3188, sc-3206–sc-3211 | Ratatui TUI: 2-4 patterns on same task, side-by-side live execution, metrics comparison. |
| sc-3220 | Build `polaris-edge` minimal runtime | feature | App | 2393 | sc-3210, sc-3215 | Stripped binary for edge. ARM64/RISC-V. Ollama. GPIO/sensor tools. |

---

## Parallel Tracks

```
Track A (Graph):      sc-3185 → sc-3186 → sc-3187 → sc-3188
Track B (Streaming):  sc-3189, sc-3190, sc-3191 (all parallel, no blockers)
Track C (Models):     sc-3192, sc-3193 (parallel, no blockers)
Track D (Tools):      sc-3194, sc-3195, sc-3196, sc-3197 (all parallel) → sc-3198
Track E (Memory):     sc-3192 → sc-3199 → sc-3200; sc-3199 → sc-3201 → sc-3202 (also needs sc-3193)
Track F (MCP):        sc-3203
Track G (UX):         sc-3204 → sc-3205
Track H (HTTP):       sc-3223 → sc-3224 → sc-3225 ─┬→ sc-3227; sc-3224 → sc-3226 (parallel with turns)
                      sc-3223 → sc-3232 ───────────┘    ↑ (sc-3232 also blocks sc-3218)
                      ─── converge on sc-3206–sc-3211 (patterns) ───
                      ─── then sc-3212–sc-3214 (docs) + sc-3215–sc-3217 (ship) ───
                      ─── then polaris-http, sc-3218–sc-3220 (products) ───
```

Tracks A-H are independent and can be worked in parallel. Track C (sc-3192, sc-3193) is a shared dependency for Track E. Track H (HTTP) has no dependencies on other tracks — it builds on already-completed `SessionsAPI`, `UserIO`/`IOProvider`, and LLM streaming infrastructure. Within Track H, sc-3232 (token-level streaming) is a prerequisite for both sc-3227 (SSE endpoints) and sc-3218 (MCP server) to avoid building message-level streaming and then rearchitecting. Patterns (sc-3206–sc-3211) are the convergence point for end products. Ship (sc-3215–sc-3217) requires patterns. Products (`polaris-http`, sc-3218–sc-3220) require patterns and their respective infrastructure tracks.

## Relationship to Existing Tickets

| Existing Ticket | Status | Relationship |
|-----------------|--------|--------------|
| sc-3137: Tools+LLM sugar | Merged | Done. |
| sc-3139: CaughtError | Merged | Done. |
| sc-3166: Middleware | Merged | Done. |
| sc-3141: JSON Schema hardening | Ready for Review | Continue. Tools (sc-3194–sc-3197) benefit. |
| sc-3168: Streaming from main | Complete | Done. Provider streaming (sc-3189–sc-3191) can proceed. |
| sc-3158: Graph visualization | Backlog | **Superseded by sc-3188.** |
| sc-3094: MemoryPlugin | Backlog | **Superseded by sc-3199.** |
| sc-3129: Separate Server/Graph schedules | Backlog | Orthogonal. Proceed independently. |
| sc-3130: Sessions and Multi-Agent Coordination | WIP (no progress) | **Archive.** ~70% delivered by `polaris_sessions` (sc-3154). Remaining Group/MessagingPlugin work is a separate future initiative. |
| sc-3159-3163: Graph utilities | Backlog | Some overlap with sc-3188. Others orthogonal. |

## Epics Reference

| Layer | Epic | Epic ID |
|-------|------|---------|
| Layer 1 — System Framework | Agentic Framework Systems design | 2350 |
| Layer 2 — Graph Execution | Agentic Framework Runtime and Infrastructure | 2352 |
| Layer 3 — Plugins | Agentic Framework Capability Plugins | 2351 |
| Documentation | Agentic Framework Documentation | 2449 |
| Applications / Examples | Agentic Framework Applications | 2393 |
| UX / Dashboard | Agentic Framework UX | 2544 |
| Polaris Code | Polaris Code | 3167 |

## Design Documents

| Area | Document | Covers |
|------|----------|--------|
| Graph Primitives | [`scope-and-dynamic-nodes.md`](../design/scope-and-dynamic-nodes.md) | sc-3185, sc-3186, sc-3187 |
| Memory | [`memory.md`](../design/memory.md) | sc-3199, sc-3200, sc-3201, sc-3202 |
| MCP Client | [`mcp-http-infrastructure.md`](../design/http-mcp-infrastructure.md) | sc-3203 |
| Agent Patterns | [`pattern-requirements-and-agent-patterns.md`](../design/pattern-requirements-and-agent-patterns.md) | sc-3206–sc-3211 |
| HTTP / App Infrastructure | `http-app.md` (planned) | Phase 1J (AppPlugin), Phase 1K (HttpPlugin, SSE streaming) |
