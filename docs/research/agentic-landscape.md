---
notion_page: https://www.notion.so/radiant-ai/The-Agentic-Open-Source-Landscape-Architecture-Deep-Dive-327afe2e695d805e91b7df460e303675
title: "The Agentic Open-Source Landscape: Architecture Deep Dive"
---

# The Agentic Open-Source Landscape: Architecture Deep Dive

*Research conducted March 17, 2026*

This document catalogs the architectures, execution models, and technical decisions of the most significant open-source agentic projects. The goal is to understand what design patterns exist in the wild, how they differ, and what Polaris can learn from each.

---

## Table of Contents

1. [Market Overview](#1-market-overview)
2. [Project Profiles](#2-project-profiles)
   - [OpenClaw](#openclaw)
   - [ZeroClaw](#zeroclaw)
   - [OpenFang](#openfang)
   - [LangChain / LangGraph](#langchain--langgraph)
   - [CrewAI](#crewai)
   - [AutoGen](#autogen)
   - [MetaGPT](#metagpt)
   - [Mastra](#mastra)
   - [Rig (rig.rs)](#rig-rigrs)
   - [n8n](#n8n)
   - [Dify](#dify)
3. [Agent Loop Architectures in the Wild](#3-agent-loop-architectures-in-the-wild)
   - [Claude Code](#claude-code)
   - [Cursor Agent Mode](#cursor-agent-mode)
   - [Devin](#devin)
4. [Taxonomy of Approaches](#4-taxonomy-of-approaches)
5. [Protocol Landscape](#5-protocol-landscape)
6. [What Drives Virality](#6-what-drives-virality)
7. [Implications for Polaris](#7-implications-for-polaris)

---

## 1. Market Overview

GitHub's Octoverse report (2026) counts over 4.3 million AI-related repositories — a 178% year-over-year jump in LLM-focused projects. The agentic subset is the fastest-growing category.

### By GitHub Stars (March 2026)

| Tier | Project | Stars | Language | Category |
|------|---------|-------|----------|----------|
| **Mega** | OpenClaw | ~250K | TypeScript | End-user agent product |
| **Mega** | n8n | 150K+ | TypeScript | Visual workflow automation |
| **Mega** | Open WebUI | 124K+ | Python/Svelte | Self-hosted AI platform |
| **Giant** | LangChain | ~90K | Python | Agent framework |
| **Giant** | LangGraph | ~80K | Python | Graph-based agent workflows |
| **Giant** | Dify | 70K+ | Python/TS | Visual LLM app platform |
| **Major** | CrewAI | ~46K | Python | Role-based multi-agent |
| **Major** | MetaGPT | 40K+ | Python | Multi-agent software company |
| **Rising** | ZeroClaw | 27.5K | Rust | Lightweight agent runtime |
| **Rising** | Mastra | — | TypeScript | TS-native agent framework |
| **Rising** | OpenFang | 14.7K | Rust | Agent operating system |

### By Category

The projects fall into five distinct categories:

**1. End-user agent products** — Finished applications people install and use. Not frameworks. OpenClaw (250K stars), Open WebUI (124K). Highest star counts by far. The viral driver is instant utility, not architecture.

**2. Visual/no-code orchestrators** — Drag-and-drop interfaces for wiring agent pipelines. n8n (150K), Dify (70K), Langflow. Democratize agent building beyond developers. Visualization is the adoption driver.

**3. Opinionated multi-agent frameworks** — Ship a specific mental model. CrewAI (roles/crews), MetaGPT (software teams), AutoGen (conversations). Fast adoption, ceiling when the model doesn't fit the use case.

**4. Graph-based execution engines** — Agent behavior as graph topology. LangGraph (80K), Polaris. Maximum flexibility, steeper learning curve. This is Polaris's category.

**5. Infrastructure/runtime** — Deployment plumbing. Ollama (local models), OpenFang (agent OS), ZeroClaw (edge runtime). Polaris could position here if it ships as a deployable binary.

### Language Distribution

8 of the top 10 most-starred agent frameworks are Python. TypeScript holds the JS/TS space (Mastra, Vercel AI SDK). Rust is emerging with OpenFang (14.7K), ZeroClaw (27.5K), and Rig.

---

## 2. Project Profiles

### OpenClaw

**Repository**: github.com/openclaw/openclaw
**Stars**: ~250K (fastest-growing OSS project in GitHub history)
**Language**: TypeScript
**Created by**: Peter Steinberger (PSPDFKit founder)
**License**: MIT

#### What it is

A personal AI assistant that runs on your own hardware and connects to messaging platforms you already use: WhatsApp, Telegram, Slack, Discord, Signal, iMessage, Microsoft Teams, and more. Not a framework — a finished product.

#### Architecture

Hub-and-spoke centered on a single **Gateway** (WebSocket server) that acts as the control plane between user inputs and the Agent Runtime.

```text
Channel Adapters (WhatsApp, Slack, ...) → Gateway → Agent Runtime → Tool Runtime
                                              ↕                          ↕
                                         Session Mgmt              File I/O, Shell,
                                         Memory Layers             Browser, Canvas
```

**Agent execution pipeline** (single serialized loop per session):

1. **Intake** — Channel adapter normalizes inbound message into a unified format. Gateway assigns to a session and queues in a lane (serial per session to prevent race conditions on shared state).
2. **Context assembly** — Bootstrap/context files injected into system prompt. Session history loaded. Skills list (names + descriptions + file paths) appended for lazy loading. Memory recalled via vector search + keyword matching.
3. **Model inference** — Assembled context streamed to configured provider (Anthropic, OpenAI, Gemini, local). Token-by-token streaming, not batch.
4. **Tool loop** — If model emits a tool call: tool runtime executes (file I/O, shell, browser, Canvas, scheduled jobs), result appended to context, loop back to inference. Continues until model emits final response or 600-second hard timeout.
5. **Persistence** — Updated session state (messages, tool calls/results) persisted to disk.
6. **Streaming reply** — Assistant deltas buffered into chat delta messages. Final message emitted on lifecycle end.

**Key design decision**: OpenClaw embeds the Pi SDK (TypeScript monorepo by Mario Zechner) directly — `createAgentSession()` is imported, not subprocess-spawned. Deep integration, not RPC.

#### Memory Architecture (4 layers)

| Layer | Type | Purpose |
|-------|------|---------|
| 1 | Session Context | Current conversation in model's context window |
| 2 | Daily Logs | Raw daily notes, append-only |
| 3 | Long-term Memory | Manually curated insights, decisions, preferences |
| 4 | Semantic Vector Search | SQLite + embeddings, BM25 keyword relevance, ~400 token chunks with 80-token overlap |

Memory search combines vector similarity with BM25 keyword relevance. File watcher (debounced 1.5s) for incremental index updates.

#### Proactive Operation

Unlike reactive assistants, OpenClaw has a **heartbeat + cron** system that triggers the agent loop without user input. The agent can schedule its own tasks, monitor feeds, and report results proactively. This transforms "assistant" into "colleague."

#### Hooks System

Two levels: **Gateway hooks** (event-driven scripts for commands and lifecycle events) and **Plugin hooks** (extension points inside the agent/tool lifecycle pipeline).

#### Skills

Skills are expert domain knowledge packaged as SKILL.md files. The agent sees a compact list of available skills; when it decides a skill is relevant, it reads the full SKILL.md on demand. This is lazy context injection — keeps the prompt lean.

---

### ZeroClaw

**Repository**: github.com/zeroclaw-labs/zeroclaw
**Stars**: 27,457 (27K in ~30 days)
**Language**: 100% Rust
**MSRV**: Rust 1.87
**License**: Apache-2.0 / MIT dual
**Binary size**: ~3.4 MB (release, stripped with fat LTO)
**Memory**: <5 MB RSS
**Cold start**: <10 ms

#### What it is

A lightweight agent runtime designed for self-hosted and edge deployments. Runs on $10 hardware (Raspberry Pi, ESP32, ARM/RISC-V). Single binary, zero runtime dependencies, no Docker/Node/Python required.

#### Workspace Structure

| Crate | Purpose |
|-------|---------|
| `zeroclaw` (root) | Binary entry point + main library |
| `zeroclaw-core` | Shared trait definitions and core types |
| `zeroclaw-types` | Serializable data model types |
| `robot-kit` | Hardware abstraction layer (GPIO, sensors, motors) |

#### Trait System

Every subsystem is a Rust trait with `Send + Sync` bounds:

- **`Provider`**: `chat()`, `stream_chat()`, `warmup()`, `list_models()`, `supports_native_tools()`
- **`Channel`**: `name()`, `send()`, `listen()`, `health()`
- **`Tool`**: `name()`, `description()`, `parameters_schema()`, `execute()`
- **`Memory`**: `save()`, `recall()`, `get()`, `forget()`
- **`Peripheral`**: Hardware I/O (GPIO, sensors)
- **`Observer`**: Observability hooks
- **`RuntimeAdapter`**: Execution environment abstraction

Uses `async_trait` macro (boxed futures).

#### Agent Loop

ZeroClaw implements a **single linear agent loop** (no graph, no branching primitives):

```text
1. Load conversation history from memory
2. build_context() — recall relevant memories via vector/keyword hybrid
3. classify_query() — pattern/keyword matching → route hint
4. Select provider + model based on route hint (reasoning/fast/semantic)
5. Security validation (autonomy level, estop state, allowlist)
6. Tool call loop (up to max_tool_iterations):
   a. Call Provider::chat(messages, tools)
   b. Parse tool calls from JSON
   c. Execute each tool via Tool::execute(args) after security gate
   d. Append results to message history
   e. Loop if more tool calls
7. Persist memory via auto-save
8. Send response through channel
```

**Three runtime modes**: Agent (interactive REPL or one-shot CLI), Gateway (HTTP/WebSocket server via Axum), Daemon (full runtime with supervised components).

#### Memory

Default backend: **SQLiteHybridMemory** with dual indexing:
- Vector store (cosine similarity on embeddings, e.g., OpenAI `text-embedding-3-small`)
- FTS5 full-text search with BM25 ranking
- Weighted merge: `vector_weight * cosine_score + keyword_weight * fts_score`

Other backends: PostgresMemory (pgvector), MarkdownMemory (filesystem), NoneMemory.

Memory categories: Core, Daily, Conversation, Custom(String).

#### Query Classification and Model Routing

The `classify_query()` step is a lightweight pattern/keyword matcher that returns a route hint. The hint selects which provider+model to use:

| Route | Model tier | Use case |
|-------|-----------|----------|
| `reasoning` | Claude/GPT-4 class | Complex analysis, planning |
| `fast` | Haiku/GPT-4o-mini class | Simple Q&A, quick tasks |
| `semantic` | Embedding model | Memory search, classification |

This is a cost optimization pattern — avoid sending simple queries to expensive models.

#### Security

- **Autonomy levels**: Full / Assisted / Supervised
- **SecurityPolicy**: Gates every tool execution (autonomy check, estop state, allowlist, rate limit)
- **PairingGuard**: 6-digit OTP + bearer token for gateway auth
- **SecretStore**: ChaCha20-Poly1305 AEAD encryption
- **EstopManager**: Emergency stop state machine (KillAll, NetworkKill, DomainBlock, ToolFreeze)
- **Workspace scoping**: Path canonicalization for workspace-only file access

#### Hardware Integration

Unique among agent frameworks: GPIO, sensors, motors via `Peripheral` trait. STM32, RPi, ESP32 support. This enables physical-world agent scenarios (home automation, robotics).

#### LLM Providers (22+)

OpenAI, Anthropic (with prompt caching), Google Gemini, OpenRouter, Ollama, LM Studio, Amazon Bedrock, GitHub Copilot, Cursor, Azure OpenAI, and any OpenAI-compatible endpoint.

#### Channels (15+)

CLI, Telegram, Discord, Slack, Matrix (E2EE), WhatsApp, Mattermost, Lark/Feishu, iMessage, Email (IMAP/SMTP), DingTalk, IRC, Signal.

---

### OpenFang

**Repository**: github.com/RightNow-AI/openfang
**Stars**: 14,674 (in ~3 weeks)
**Language**: 87.9% Rust
**License**: Apache-2.0 / MIT
**LOC**: 137,728 across 14 crates
**Tests**: 1,767+
**Binary size**: ~32 MB
**Cold start**: 180ms (claimed)
**Idle memory**: 40MB (claimed)

#### What it is

An Agent Operating System. Not a framework or library — a standalone compiled binary that runs autonomous agents 24/7 on schedules. Configuration-driven (HAND.toml), not code-driven.

#### Crate Structure (14 crates)

| Crate | Purpose |
|-------|---------|
| **openfang-kernel** | Orchestration: AgentRegistry, AgentScheduler, CapabilityManager, EventBus, Supervisor, WorkflowEngine, TriggerEngine, WasmSandbox |
| **openfang-runtime** | Agent loop: 3 native LLM drivers, 53 tools, WASM sandbox, MCP client/server, A2A protocol |
| **openfang-types** | Shared types, taint tracking, Ed25519 manifest signing, model catalog |
| **openfang-memory** | SQLite persistence, KV store, vector search, knowledge graph, session mgmt, compaction |
| **openfang-api** | 76+ REST/WS/SSE endpoints (Axum 0.8), OpenAI-compatible API, dashboard backend |
| **openfang-channels** | 40 messaging adapters with rate limiting, DM/group policies |
| **openfang-skills** | 60 bundled skills, SKILL.md parser, FangHub marketplace, prompt injection scanner |
| **openfang-hands** | 7 autonomous Hands (agents), HAND.toml parser, lifecycle management |
| **openfang-extensions** | 25 MCP templates, AES-256-GCM credential vault, OAuth2 PKCE |
| **openfang-wire** | OFP (OpenFang Protocol) for P2P networking, HMAC-SHA256 mutual auth |
| **openfang-cli** | CLI with daemon management, TUI dashboard, MCP server mode |
| **openfang-desktop** | Tauri 2.0 native app with system tray, notifications |
| **openfang-migrate** | Migration engine from OpenClaw, LangChain, AutoGPT |
| **xtask** | Build automation |

#### Agent Definition Model

Agents ("Hands") are defined declaratively with three artifacts:

**1. HAND.toml manifest:**

```toml
name = "my-assistant"
version = "0.1.0"
module = "builtin:chat"

[model]
provider = "groq"
model = "llama-3.3-70b-versatile"

[capabilities]
tools = ["file_read", "file_list", "web_fetch"]
memory_read = ["*"]
memory_write = ["self.*"]
```

**2. System prompt:** Not one-liners — 500+ word multi-phase operational playbooks with decision trees, error recovery, quality gates.

**3. SKILL.md:** Expert domain knowledge injected into agent context.

Manifests are compiled into the binary and signed with Ed25519.

#### Kernel Architecture

The kernel orchestrates agent lifecycle through:
- **AgentRegistry** — identity and capability registration
- **AgentScheduler** — cron-based autonomous scheduling
- **CapabilityManager** — RBAC permission enforcement
- **EventBus** — inter-component communication
- **Supervisor** — health monitoring, restart policies
- **WorkflowEngine** — multi-step workflow execution (fan-out, conditional, loops, variable expansion)
- **TriggerEngine** — event-driven execution
- **WasmSandbox** — isolated tool execution via Wasmtime

#### Agent Loop

- 3 native LLM drivers (Anthropic, Gemini, OpenAI-compatible) covering 27 providers
- Intelligent model routing: task complexity scoring determines which model to use
- Streaming: full WebSocket and SSE support
- Loop guard: SHA256-based tool call dedup with circuit breaker (prevents ping-pong)
- Budget enforcement: per-model cost tracking with configurable limits
- Session repair: 7-phase message history validation and corruption recovery

#### WASM Tool Sandbox

All tool code runs in WebAssembly via Wasmtime:
- **Fuel metering** — tracks computational resource consumption
- **Epoch interruption** — watchdog thread kills runaway computations
- **Workspace confinement** — file operations restricted to agent workspace
- **Environment clearing** — subprocesses run with `env_clear()`, only whitelisted vars
- **60-second universal timeout** on tool execution
- **50K character hard cap** on tool result truncation

#### Memory Architecture

The `openfang-memory` crate provides six subsystems (not a clean 4-layer stack):
1. SQLite persistence (foundation)
2. Structured KV store
3. Vector embedding semantic search
4. Knowledge graph (structured relationships)
5. Session management (cross-channel canonical sessions)
6. Task boards (ongoing work items)

Additional: automatic LLM-based compaction (summarizes old conversations), JSONL session mirroring, deduplication.

#### Multi-Agent and Workflows

- **Workflow engine**: Fan-out parallelism, conditional logic, loops, variable expansion, pipeline chaining
- **Hand chaining**: e.g., Researcher → Predictor → Clip → broadcast to 40 channels
- **A2A protocol**: Google Agent-to-Agent task protocol for cross-framework interop
- **OFP**: Custom P2P protocol for inter-instance agent communication

#### Security (16 layers)

1. WASM dual-metered sandbox (fuel + epoch)
2. Merkle hash-chain audit trail
3. Information flow taint tracking
4. Ed25519 signed agent manifests
5. SSRF protection (private IP, metadata endpoint, DNS rebinding blocking)
6. Secret zeroization (`Zeroizing<String>`)
7. OFP mutual authentication (HMAC-SHA256)
8. Capability gates (kernel-enforced RBAC)
9. Security headers (CSP, HSTS)
10. Health endpoint redaction
11. Subprocess sandbox (env_clear + selective passthrough)
12. Prompt injection scanner
13. Loop guard (SHA256 circuit breaker)
14. Session repair (7-phase validation)
15. Path traversal prevention
16. GCRA rate limiter (cost-aware token bucket)

#### Performance Claims (self-reported, unverified)

| Metric | OpenFang | LangGraph | OpenClaw |
|--------|----------|-----------|----------|
| Cold start | 180ms | 2,500ms | 5,980ms |
| Idle memory | 40MB | 180MB | 394MB |
| Install size | 32MB | 150MB | 500MB |
| Throughput | ~13x LangGraph on routing | baseline | — |

---

### LangChain / LangGraph

**Repository**: github.com/langchain-ai/langchain, github.com/langchain-ai/langgraph
**Stars**: ~90K (LangChain), ~80K (LangGraph)
**Language**: Python
**Created by**: Harrison Chase
**Downloads**: 47M+ PyPI (LangChain)

#### What it is

LangChain is the foundational Python framework for building AI agents. LangGraph extends it with a graph-based workflow engine for stateful, multi-step agent orchestration. Together they form the largest agent ecosystem.

#### LangChain Core

Modular components: chains (sequential operations), agents (LLM + tool selection loops), memory (conversation persistence), retrieval (RAG pipelines), tool use (function calling). The "Swiss Army knife" of agent frameworks.

#### LangGraph Architecture

Graph-based workflow design that treats agent interactions as nodes in a directed graph. Key characteristics:

- **State graph**: Nodes are Python functions that receive and return state. State is a typed dict that flows through the graph.
- **Conditional edges**: Route based on state values (equivalent to Polaris Decision/Switch nodes).
- **Cycles**: First-class support for loops (unlike pure DAGs).
- **Persistence**: Built-in checkpointing for long-running workflows.
- **Human-in-the-loop**: Breakpoints where execution pauses for human input.
- **Streaming**: Token-level streaming through the graph.

LangGraph is the most architecturally similar project to Polaris. Both model agents as directed graphs with typed control flow. The key difference: LangGraph uses Python with runtime typing; Polaris uses Rust with compile-time guarantees.

#### LangGraph Studio

A visual debugger and IDE for LangGraph workflows. Shows the graph topology, current execution state, and allows stepping through nodes. This is the kind of visualization Polaris lacks.

---

### CrewAI

**Repository**: github.com/crewAIInc/crewAI
**Stars**: ~46K
**Language**: Python
**Community**: 100K+ certified developers

#### What it is

A role-based multi-agent orchestration framework. You define agents by role/backstory/goal, assemble them into a "crew" with tasks, and the framework orchestrates collaboration.

#### Mental Model

Inspired by real-world organizational structures:
- **Agent**: Has a role ("Senior Researcher"), backstory, goal, and available tools
- **Task**: A unit of work with a description, expected output, and assigned agent
- **Crew**: A team of agents with a process (sequential, hierarchical, or consensual)

```python
researcher = Agent(role="Senior Researcher", goal="Find cutting-edge AI developments", ...)
writer = Agent(role="Tech Writer", goal="Write engaging blog posts", ...)

research_task = Task(description="Research latest AI trends", agent=researcher)
write_task = Task(description="Write a blog post about findings", agent=writer)

crew = Crew(agents=[researcher, writer], tasks=[research_task, write_task], process=Process.sequential)
result = crew.kickoff()
```

#### Strengths

- Lowest barrier to entry for multi-agent workflows
- 40% faster time-to-production than LangGraph for standard business workflows
- Active development, A2A protocol support added in 2026
- Built-in support for common patterns (delegation, collaboration, voting)

#### Limitations

- Fixed set of process types (sequential, hierarchical, consensual)
- No arbitrary graph topologies
- Less control over execution flow than graph-based approaches

---

### AutoGen

**Repository**: github.com/microsoft/autogen
**Stars**: ~35K
**Language**: Python
**Status**: Microsoft shifted to maintenance mode in favor of broader Microsoft Agent Framework

#### What it is

Conversational agent architecture from Microsoft Research. Agents collaborate through multi-party conversations — group debates, consensus-building, sequential dialogues.

#### Key Concepts

- **ConversableAgent**: Base agent that can send/receive messages
- **AssistantAgent**: LLM-powered agent with optional tool use
- **UserProxyAgent**: Represents the human user, can execute code
- **GroupChat**: Multi-agent conversation with a speaker selection mechanism

AutoGen's strength is conversation patterns — the most diverse of any framework. Its weakness is lack of structured workflow control (everything is a conversation).

---

### MetaGPT

**Repository**: github.com/geekan/MetaGPT
**Stars**: 40K+
**Language**: Python

#### What it is

Multi-agent framework simulating a software company. Agents take on roles (Product Manager, Architect, Engineer, QA) and collaborate to produce software from a single requirement.

#### Architecture

Role-based division of labor with structured handoffs:
1. **Product Manager** → generates PRD from user requirement
2. **Architect** → produces system design and API specs
3. **Engineer** → writes code following the specs
4. **QA Engineer** → writes and runs tests

Each role has a fixed set of actions, and output from one role feeds as input to the next. This is a linear pipeline with role specialization.

---

### Mastra

**Repository**: github.com/mastra-ai/mastra
**Language**: TypeScript
**Created by**: Team behind Gatsby (YC-backed)

#### What it is

The TypeScript-native answer to LangChain. A framework for building AI agents with a modern TS stack, emphasizing type safety and developer experience.

#### Key Features

- **Model routing**: 40+ providers through one standard interface
- **Workflows**: Multi-step processes with human-in-the-loop (suspend/resume)
- **RAG**: Built-in knowledge integration
- **Evals**: Quality and accuracy measurement built in
- **MCP server authoring**: Expose agents and tools via the MCP interface
- **Context management**: Conversation history, semantic memory, API/database/file data retrieval
- **Frontend integration**: React, Next.js, Node, or standalone server

Mastra supports authoring MCP servers, exposing agents as callable tools — important for ecosystem interop.

---

### Rig (rig.rs)

**Repository**: github.com/0xPlaygrounds/rig
**Language**: Rust
**Website**: rig.rs

#### What it is

A Rust library for building portable, modular, lightweight LLM applications. More of a library than a framework — provides building blocks rather than prescribing architecture.

#### Key Features

- **Unified LLM interface**: Common abstractions over OpenAI, Cohere, etc.
- **Agent type**: High-level abstraction from simple agents to full RAG systems
- **Vector store support**: Common `VectorStoreIndex` trait for knowledge bases
- **Data extraction**: Structured data extraction from text via LLMs
- **Pipeline API**: Sequence of operations mixing AI and non-AI components
- **Streaming**: Streaming completion support via traits and types
- **Image generation**: Multi-provider image generation

Rig is the closest Rust-ecosystem peer to Polaris at the library level, but it does not have graph-based execution, ECS-style resource management, or a plugin system.

---

### n8n

**Repository**: github.com/n8n-io/n8n
**Stars**: 150K+
**Language**: TypeScript
**License**: Fair-code (source-available)

#### What it is

A visual workflow automation platform with native AI capabilities. Drag-and-drop interface for building workflows with 400+ integrations. Not agent-specific, but widely used for AI agent pipelines.

#### Why it matters

n8n proves that visual interfaces drive massive adoption. Domain experts (not just ML engineers) can build sophisticated AI pipelines. The combination of visual editing + code escape hatch is the sweet spot.

---

### Dify

**Repository**: github.com/langgenius/dify
**Stars**: 70K+
**Language**: Python/TypeScript

#### What it is

An open-source LLM application development platform combining:
- Visual interface for building AI workflows
- RAG pipeline management
- Agent capabilities with tool use
- Model management across providers
- Full-stack observability

Dify fills the gap for teams wanting to stand up AI services quickly under a self-hostable framework. It's more application-platform than developer-framework.

---

## 3. Agent Loop Architectures in the Wild

Beyond open-source frameworks, the most influential agent architectures are found in commercial products. These are the patterns users actually experience daily.

### Claude Code

**Product**: Anthropic's CLI for autonomous coding
**Architecture**: Single-threaded master loop + parallel sub-agent spawning

#### Main Loop

```text
while true:
    1. Receive prompt (user input + system prompt + tool definitions + history)
    2. Call Claude (evaluate current state, decide action)
    3. If response has tool calls:
        a. Execute each tool (read files, edit files, run commands, search)
        b. Collect results
        c. Feed results back to Claude → goto 2
    4. If response is text only (no tool calls):
        → Return to user, loop terminates
```

This is the "canonical agent architecture" — a while loop with tools. The loop continues as long as the model produces tool calls. When it produces plain text, the loop exits naturally.

#### Sub-Agent Spawning

Claude Code can spawn up to 10 concurrent sub-agents via the Task/Agent tool:
- Each sub-agent runs in its own context window (fresh, no parent conversation)
- The only input from parent to sub-agent is the prompt string
- Sub-agents work independently and return results
- Parallel execution only works when agents touch different files

Sub-agents are the management layer built on top of Task tools, sharing identical core capabilities: parallel execution, context isolation, and result coordination.

#### Key Design Decisions

- **Serial by default**: The main loop is single-threaded. Sub-agents are the concurrency mechanism.
- **Tools make it agentic**: Without tools, it's just a chatbot. The tool set (file read/write, shell exec, web search) is what enables autonomous behavior.
- **No fixed plan**: The model decides step-by-step. There's no upfront planning phase — the loop is reactive.

---

### Cursor Agent Mode

**Product**: Cursor IDE's agentic coding mode
**Architecture**: Plan-then-execute with verification loop and codebase RAG

#### Execution Flow

```text
1. Analyze request + codebase context
   └─ RAG: search codebase, docs, web for relevant files
2. Plan: break task into smaller steps
3. For each step:
   a. Execute code modifications
   b. Verify (linter, tests if supported)
   c. If issues found: fix them
   d. If more steps: next step
4. Summarize changes
```

#### Key Design Decisions

- **Plan before execute**: Unlike Claude Code's reactive loop, Cursor creates an explicit plan first. The plan is visible to the user and can be modified before execution.
- **Verification gate**: Non-LLM tools (linter, test runner) validate each step. The agent attempts to fix issues automatically before moving on.
- **MoE model**: Cursor uses a Mixture of Experts language model for the core reasoning, optimizing quality-at-latency.
- **Sandboxed execution**: Tool calls run in a sandbox (local or cloud VM) with strict guardrails.
- **Dynamic re-planning**: In 2026, Cursor supports altering the plan mid-execution if it hits a roadblock.

---

### Devin

**Product**: Cognition's autonomous software engineer
**Architecture**: Multi-model compound system with Planner/Coder/Critic + dynamic re-planning

#### Component Models

Devin is not a single model — it's a swarm of specialized models:

| Component | Role | Model class |
|-----------|------|-------------|
| **Planner** | Breaks down tasks into step-by-step plans | High-reasoning (GPT-6 / Claude-Next class) |
| **Coder** | Writes and modifies code | Code-specialized model |
| **Critic** | Reviews code for security, logic errors | Adversarial review model |
| **Browser** | Scrapes and synthesizes documentation | Web-interaction agent |

#### Execution Flow

```text
1. User provides task in natural language
2. Planner analyzes and creates step-by-step plan
   └─ Plan visible to user, can be modified before execution
3. For each step:
   a. Coder generates/modifies code
   b. Critic reviews for security and logic errors
   c. If issues → back to Coder (retry)
   d. If roadblock → back to Planner (dynamic re-planning)
4. Browser runs concurrently for documentation lookup
5. Final output delivered
```

#### Key Design Decisions

- **Multi-model orchestration**: Different models for different roles, not one model doing everything. Enables specialization and adversarial review.
- **Dynamic re-planning (v3.0, 2026)**: If the Coder hits a roadblock, the Planner re-evaluates and produces a new plan without human intervention.
- **Full sandbox environment**: Devin operates in a complete development environment with shell, browser, and editor — not just file operations.
- **Context management**: RAG-style retrieval maintains understanding of codebase, project structure, and development history across extended sessions.

---

## 4. Taxonomy of Approaches

### By Execution Model

| Model | Examples | Characteristics |
|-------|----------|-----------------|
| **While-loop with tools** | Claude Code, OpenClaw, ZeroClaw | Simplest. Model calls tools until done. No explicit control flow. |
| **Plan-then-execute** | Cursor, Devin (partial) | Upfront planning phase produces structured steps. Execution follows the plan. |
| **Multi-model pipeline** | Devin | Specialized models for different roles. Enables adversarial review and role specialization. |
| **Role-based crew** | CrewAI, MetaGPT | Agents defined by role/backstory. Framework orchestrates handoffs. |
| **Conversational** | AutoGen | Agents collaborate through multi-party conversations. |
| **Graph-based** | LangGraph, Polaris | Arbitrary directed graph of computation nodes. Maximum flexibility. |
| **Visual/no-code** | n8n, Dify, Langflow | Drag-and-drop workflow construction. |
| **Kernel-orchestrated** | OpenFang | OS-level agent scheduling, RBAC, budget enforcement, workflow chaining. |

### By Agent Definition Method

| Method | Examples | Trade-off |
|--------|----------|-----------|
| **Code (imperative)** | Polaris, LangGraph, Rig | Maximum flexibility, requires programming |
| **Code (declarative)** | CrewAI, Mastra | Structured API, less flexible, lower barrier |
| **Configuration (TOML/YAML)** | OpenFang, ZeroClaw | No code needed, least flexible |
| **Visual (drag-and-drop)** | n8n, Dify, Langflow | No code, most accessible, limited by UI |

### By Context Management

| Approach | Examples | Trade-off |
|----------|----------|-----------|
| **Shared mutable state** | Most while-loop agents | Simple, but no isolation between components |
| **Message passing** | AutoGen, CrewAI | Clean boundaries, but overhead |
| **Hierarchical contexts** | Polaris | Typed resources with scoped access, compile-time safety |
| **Session-based** | OpenClaw, ZeroClaw | Per-conversation state with persistence |

---

## 5. Protocol Landscape

### MCP (Model Context Protocol)

Anthropic's protocol for connecting LLMs to external tools and data sources. Rapidly becoming the standard for tool interop.

| Project | MCP Support |
|---------|-------------|
| OpenFang | Client + Server |
| ZeroClaw | Client only |
| Mastra | Server authoring |
| OpenAgents | Native support |
| Polaris | **Not implemented** |

MCP has two transports: stdio (subprocess with JSON-RPC) and SSE/HTTP (remote). Most implementations support both.

### A2A (Agent-to-Agent Protocol)

Google's protocol for cross-framework agent interop. Enables agents from different frameworks to delegate tasks to each other.

| Project | A2A Support |
|---------|-------------|
| OpenFang | Implemented |
| OpenAgents | Native support |
| CrewAI | Added in 2026 |
| ZeroClaw | Feature request stage |
| Polaris | **Not implemented** |

A2A defines: Agent Card discovery (`.well-known/agent.json`), task lifecycle (Submitted → Working → Completed/Failed/Cancelled), and a bounded task store.

### OFP (OpenFang Protocol)

OpenFang's custom P2P protocol for inter-instance communication with HMAC-SHA256 nonce-based mutual authentication. Specific to OpenFang.

---

## 6. What Drives Virality

Analysis of the top projects reveals consistent patterns:

### 1. Instant Gratification

Every viral project has a working demo in under 2 minutes:
- OpenClaw: `curl install | sh && openclaw init`
- ZeroClaw: `brew install zeroclaw`
- n8n: `npx n8n`
- CrewAI: `pip install crewai && crewai create crew`

Polaris is a library crate with no standalone entry point. There is no 2-minute demo.

### 2. Visual Proof

Projects that produce visible output go viral faster:
- n8n: Visual workflow editor
- Dify: Web-based agent builder
- LangGraph Studio: Graph debugger
- OpenFang: TUI dashboard + Tauri desktop app
- ZeroClaw: Embedded React SPA

Polaris graphs are invisible. No visualization, no dashboard, no TUI.

### 3. Ecosystem Bridges

Connecting to platforms people already use:
- OpenClaw: 13 messaging platforms
- OpenFang: 40 channel adapters
- ZeroClaw: 15+ channels
- MCP: connects to Cursor, Claude Desktop, VS Code

Polaris has an IO abstraction but no concrete adapters.

### 4. Strong Narrative

Each viral project has a one-line pitch:
- OpenClaw: "Your AI colleague" (proactive, always-on)
- ZeroClaw: "AI on a $10 device" (edge, minimal)
- CrewAI: "AI team" (roles, collaboration)
- MetaGPT: "AI software company" (PM, architect, engineer)
- n8n: "Workflow automation for everyone" (visual, no-code)

Polaris's narrative ("composable primitives for agent design") is accurate but not viral.

### 5. Founder Brand

OpenClaw (Peter Steinberger/PSPDFKit), Mastra (Gatsby team), LangChain (Harrison Chase). In the AI tools space, "who's building it" sometimes matters more than "what it is" for initial attention.

### 6. Timing and Community

OpenClaw arrived as interest in autonomous agents was rising and delivered a tangible, usable implementation rather than a research demo. The rapid rebrands became a meme across developer Twitter/Reddit/HN, creating viral attention loops.

---

## 7. Implications for Polaris

### Where Polaris is positioned

Polaris sits in Category 4 (graph-based execution engine), most directly competing with LangGraph (~80K stars). The framework is architecturally superior in several ways:

| Dimension | Polaris | LangGraph |
|-----------|---------|-----------|
| Language | Rust (compile-time safety, performance) | Python (runtime typing) |
| Resource access | Typed `Res<T>`, `ResMut<T>` with conflict detection | Typed state dict, runtime errors |
| Node types | 5 (System, Decision, Switch, Parallel, Loop) + planned Scope, Dynamic | Functions + conditional edges |
| Validation | Build-time graph validation | Runtime errors |
| Plugin system | First-class with lifecycle | Not applicable |

### What competitors do that Polaris doesn't

| Capability | Who has it | Priority for Polaris |
|------------|-----------|---------------------|
| Streaming | Everyone | Critical |
| MCP support | OpenFang, ZeroClaw, Mastra | Critical |
| Graph visualization | LangGraph Studio, n8n, Dify | Critical |
| Multiple agent patterns | CrewAI, MetaGPT, LangChain | Critical |
| Memory (vector + keyword) | OpenClaw, ZeroClaw, OpenFang | High |
| Structured output | Rig, Mastra, LangChain | High |
| Messaging channels | OpenClaw, OpenFang, ZeroClaw | Medium |
| Web UI / dashboard | OpenFang, ZeroClaw, n8n, Dify | Medium |
| WASM tool sandbox | OpenFang | Low |
| Hardware/GPIO | ZeroClaw | Low |

### Key Architectural Insights

1. **The while-loop-with-tools is the dominant pattern.** Claude Code, OpenClaw, ZeroClaw, and most agents use this. It maps to a Polaris loop with a conditional branch. Polaris should make this trivially easy to express.

2. **Multi-model orchestration is underserved.** Devin's planner/coder/critic pattern is powerful but no framework makes it easy. Polaris's `ModelRegistry` with multiple providers is well-positioned for this.

3. **Query classification for model routing is practical.** ZeroClaw's classify → route → execute pattern saves costs. This maps to a Polaris Switch node.

4. **Dynamic graph generation is the frontier.** No open-source framework supports "agent builds its own execution plan as a graph." The Scope/Dynamic node design positions Polaris uniquely here.

5. **Configuration-driven agents attract users faster than code-driven ones.** OpenFang and ZeroClaw both use TOML configuration. A `polaris-cli` that wraps the framework with configuration-driven agent selection would bridge this gap.

### The Opportunity

Neither OpenFang nor ZeroClaw can express arbitrary agent topologies. They hardcode their execution models. LangGraph can express graphs but lacks compile-time safety, typed resources, and plugin architecture.

Polaris is the only framework where you can implement Claude Code's architecture, swap it for Devin's multi-model pipeline, and run both on the same task to compare — all with compile-time guarantees. Making this capability visible and accessible is the path to adoption.
