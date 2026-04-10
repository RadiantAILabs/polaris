---
notion_page: https://www.notion.so/radiant-ai/Terminal-Based-Coding-Agent-327afe2e695d8046ad3bdc694d1be749
title: "Roadmap: Terminal-Based Coding Agent"
---

# Roadmap: Terminal-Based Coding Agent

**Goal:** Ship a standalone binary (`polaris` CLI) — a Claude Code-equivalent terminal coding agent built on the Polaris framework.

**Date:** 2026-03-10

---

## Current State

### Done

- Core framework (Layer 1 + Layer 2): Complete
- LLM providers — Anthropic, OpenAI, Bedrock (non-streaming): Complete
- Tool framework (`#[tool]`, `#[toolset]`, `ToolRegistry`): Complete
- IO system (`UserIO`, `IOProvider`): Complete
- Persistence (`Storable`, `PersistenceAPI`): Complete
- Sessions crate (`SessionsAPI`, `FileStore`, `InMemoryStore`): Complete (sc-3154)
- Sessions adoption in CLI example: Complete (sc-3165)
- Agent trait refactor (`to_graph()` on `Agent`): Complete (sc-3164)

### In Review / WIP

- sc-3137: Tools+LLM ergonomic sugar (Ready for Review)
- sc-3139: Native error propagation with CaughtError (Ready for Review)
- sc-3166: Middleware for Graph Execution (Ready for Review)
- sc-3141: JSON Schema normalization hardening (WIP)
- sc-3145: OpenTelemetry support (WIP)
- sc-3130: Sessions and Multi-Agent Coordination (WIP)

### Existing Backlog (already ticketed)

- sc-3158–3163: Graph manipulation utilities (visualization, node lookup, deep clone, dry-run, transformation, composition)
- sc-3094: MemoryPlugin with `MemoryBackend` trait
- sc-3129: Separate Server/Graph schedules

---

## Plugin Stack: Terminal Coding Agent

- **CodingAgent** (agentic loop — orchestrates tool calling + context management)
  - depends on **AnthropicPlugin** (streaming LLM provider)
    - depends on **ModelsPlugin** (already done — `LlmProvider` trait + `ModelRegistry`)
  - depends on **ShellToolPlugin**, **CodeSearchPlugin**, **FileEditPlugin**, **GitToolsPlugin** (concrete tools)
    - depend on **ToolsPlugin** (already done — `Tool` trait + `ToolRegistry`)
  - depends on **WorkspacePlugin** (project detection, ignore patterns, file tree)
    - depends on **CodeSearchPlugin**
  - depends on **ContextManagerPlugin** (token counting + sliding window truncation)
  - depends on IO abstractions (already done — `UserIO`, `IOProvider`, confirmation model)
    - extended by **StreamingIOProvider** (terminal renderer + syntax highlighting)
  - depends on **MCPClientPlugin** (external tool server discovery + invocation)

---

## Phases

Phases are **chronologically ordered** — each phase represents a wave of parallelizable work. All items within a phase can be worked on concurrently. A phase can only start once its dependencies from earlier phases are met.

### Phase 1: Foundation (no dependencies — all parallel)

*Core building blocks with no inter-dependencies. Start all at once.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 1 | Add `stream()` method to `LlmProvider` trait | feature | L3 | Trait | 2351 | — | Add `StreamDelta` types and `stream()` returning `Pin<Box<dyn Stream<Item = StreamDelta>>>` to `LlmProvider`. No provider impls yet. |
| 4 | Add shell execution tool | feature | L3 | Plugin | 2351 | — | `run_command(command, working_dir, timeout)` tool with stdout/stderr capture, exit code, timeout kill. Plugin in `polaris_core_plugins` or new crate. |
| 5 | Add code search tools (grep, glob, partial read) | feature | L3 | Plugin | 2351 | — | `grep(pattern, path, context_lines)`, `glob(pattern, path)`, `read_file` with `offset`/`limit` line params. Extend or replace examples `FileToolsPlugin`. |
| 6 | Add file edit tool with targeted replacement | feature | L3 | Plugin | 2351 | — | `edit_file(path, old_string, new_string)` tool — exact string match + replace, avoids full-file overwrite. |
| 7 | Add user confirmation model for tool execution | feature | L2/L3 | Resource/API | 2352 | — | `ToolPermission` enum (auto/confirm/deny), `UserIO::confirm()` method, pause graph execution mid-tool for user approval. |
| 8 | Add context window management with token counting | feature | L3 | Resource/API | 2351 | — | `TokenCounter` trait + tiktoken-rs impl, `ContextManager` with sliding window truncation, budget-aware file inclusion. |

### Phase 2: Extensions (depend on Phase 1 items)

*Build on Phase 1 primitives. Each item depends on a specific Phase 1 ticket.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 2 | Implement streaming for Anthropic provider | feature | L3 | Plugin | 2351 | #1 | SSE parsing for Anthropic Messages API, emit `StreamDelta` events (text, tool_use, stop). |
| 9 | Add git integration tools | feature | L3 | Plugin | 2351 | #4 | `git_status`, `git_diff`, `git_log`, `git_commit` tools. Wraps git CLI via shell execution. |
| 10 | Add workspace awareness plugin | feature | L3 | Plugin | 2351 | #5 | `WorkspacePlugin` — detect project type (Cargo.toml, package.json, etc.), provide project root, ignore patterns (.gitignore), file tree. |
| 11 | Add streaming terminal renderer | feature | App | Plugin | 2544 | #1 | `StreamingIOProvider` — render LLM tokens progressively as they arrive, handle markdown code blocks, spinners for tool calls. |

### Phase 3: Refinements (depend on Phase 2 items)

*Second-order extensions. Each depends on a Phase 2 ticket.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 3 | Implement streaming for Bedrock provider | feature | L3 | Plugin | 2351 | #2 | Bedrock `converseStream()` API support. |
| 12 | Add syntax highlighting for code output | feature | App | App | 2544 | #11 | `syntect` or `tree-sitter-highlight` for code blocks in terminal output. Diff rendering for file edits. |
| 16 | Add MCP client support | feature | L3 | Plugin | 2351 | #4 | MCP tool server discovery + invocation, extending `ToolRegistry` with external MCP tools. |

### Phase 4: Coding Agent (convergence — depends on Phases 1–2)

*Assemble all capabilities into the agent definition.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 13 | Define coding agent graph and systems | feature | App | App | 3167 | #1–#10 | New `CodingAgent` implementing `Agent` — agentic loop with tool calling, context management, file operations, shell execution. |
| 15 | Add integration tests with mock LLM | feature | App | App | 3167 | #13 | End-to-end tests using `MockIOProvider` + mock `LlmProvider` — verify the agent can read files, make edits, run commands in a test scenario. |

### Phase 5: Ship (depends on Phases 3–4)

*Package and distribute the binary.*

| # | Title | Type | Layer | Comp. Level | Epic | Depends On | Scope |
|---|-------|------|-------|-------------|------|------------|-------|
| 14 | Create `polaris-cli` binary crate | feature | App | App | 3167 | #13, #11 | Standalone binary with CLI args (project dir, model, session), config file support, REPL with full command set. |
| 17 | Binary packaging and release automation | chore | App | App | 3167 | #14 | GitHub Actions for cross-platform builds (macOS arm64/x86, Linux), `cargo install` support, homebrew formula. |

---

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
