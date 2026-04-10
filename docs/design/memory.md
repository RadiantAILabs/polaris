---
notion_page: https://www.notion.so/radiant-ai/Design-Memory-32fafe2e695d805ab2fddfde4031ffec
title: "Design: Memory"
---

# Design: Memory

**Status:** Draft
**Layer:** 3 (Plugin-Provided Abstractions)
**Crate:** `polaris_memory`
**Dependencies:** `polaris_system` (plugin, resources), `polaris_models` (TokenCounter, EmbeddingProvider)
**Date:** 2026-03-26

## Motivation

Memory is all information an agent encounters during operation. Every agent needs to store, retrieve, update, and remove information — whether that information is a conversation turn from seconds ago, a durable fact from a previous session, or a scratch plan being refined mid-execution.

Most agent frameworks split this into two unrelated systems: a "context manager" for conversation history and a "memory backend" for long-term recall. This split is artificial. Both store text, both retrieve by relevance, both manage capacity, and both need token counting. The difference is configuration — time horizon, retrieval strategy, persistence — not a fundamentally different abstraction.

Polaris provides a single set of memory primitives. How those primitives are used is a pattern-level decision.

## Philosophy Alignment

From `docs/philosophy.md`:

> *"Polaris provides composable primitives without prescribing how they should be assembled."*

This design provides:

- **Primitives**: store, retrieve, update, remove — the CRUD operations every memory system needs
- **No prescribed assembly**: The framework does not dictate turn segmentation rules, budgeting formulas, compaction prompts, retrieval algorithms, or chunking policies. These are pattern-level opinions.
- **Replaceability**: Every component behind the memory abstractions is swappable — backends, retrieval strategies, capacity policies
- **Separation of state and behavior**: Memory data lives in resources, memory operations live in systems

## Design Overview

### Memory primitives

The framework provides four operations:

- **Store** — persist an item with metadata
- **Retrieve** — get items back by key, by query, by recency, by relevance, or any combination
- **Update** — modify existing items (upsert semantics when a key is provided)
- **Remove** — delete items by selector

These are the only operations the framework owns. Everything else is policy.

### Context management as a subset

Context management answers one question: given everything the agent remembers, what goes into the next LLM request?

It is a **projection** of memory onto a model's finite context window. Context management reads from memory and produces a bounded message list shaped by the model's budget (input limit, output reservation, system prompt overhead).

Strategies like sliding window, compaction, and semantic retrieval are pattern-level policies — different patterns may manage context differently, all reading from the same memory primitives.

### What is a primitive vs what is a policy

| Primitive (framework) | Policy (pattern-level) |
|------------------------|------------------------|
| Store an item | Turn segmentation (how to group messages into items) |
| Retrieve items | Retrieval strategy (recency, semantic, keyword, hybrid) |
| Update an item | Capacity management (when to evict, summarize, or compress) |
| Remove items | Budgeting formula (how to calculate available space) |
| | Compaction (how to summarize old items via LLM) |
| | Chunking (how to split large items for indexing) |
| | Scoring/ranking (RRF weights, recency bonuses) |
| | Scoping (ephemeral vs durable, local vs global) |

---

## Framework Abstractions

These types live in `polaris_memory` and define the contract that all memory implementations fulfill.

### Memory operations

The exact trait design will be determined during implementation, but the contract covers:

- **Store**: Accept an item with namespace, optional key, optional category, content, and metadata. When a key is provided, store acts as an upsert on `(namespace, key)`.
- **Retrieve**: Accept a query with namespace, optional text, optional limit, optional category filter. Return scored results with enough metadata for debugging (scores, ranks).
- **Update**: Modify an existing item's content or metadata by key.
- **Remove**: Delete items matching a selector (by namespace, key, or category).

### Namespace isolation

Every operation requires a namespace. This is the primary partition key. Polaris does not accidentally search every memory bucket — callers must be explicit about scope.

### Resource model

Memory can be either a **Global** or **Local** resource depending on the use case:

- **Global**: Shared infrastructure for durable, cross-session memory (e.g., a SQLite-backed store that all agents can read)
- **Local**: Per-agent state for conversation-scoped memory (e.g., the current conversation history)

The framework supports both. Patterns choose which model fits their needs. A single pattern may use both — global for long-term recall, local for conversation state.

### Plugin

`MemoryPlugin` registers memory resources at server build time. The plugin accepts a backend implementation and exposes it as a typed resource.

---

## Supporting Infrastructure

Memory implementations may depend on shared infrastructure from `polaris_models`:

### TokenCounter

Counts tokens for any model. Used by context management policies (budget accounting) and chunking policies (splitting large items into indexable chunks).

Lives in `polaris_models` as shared infrastructure. Memory consumes it via `Arc<dyn TokenCounter>`.

### EmbeddingProvider

Embeds text into vectors for semantic retrieval. Lives in `polaris_models` as a sibling to `LlmProvider`.

Not all memory backends need embeddings — only those implementing semantic retrieval. The dependency is on the backend, not on the memory abstraction itself.

---

## Context Management

Context management is not a separate system. It is the pattern-level policy layer that projects memory into LLM requests.

### What context management does

1. Reads from memory (the agent's stored conversation turns, summaries, recalled facts)
2. Accounts for the model's budget (input limit minus output reservation minus system prompt minus tool schemas)
3. Selects and shapes a subset of memory that fits within the budget
4. Returns a bounded message list ready for request construction

### What context management does NOT own

- The memory data itself (that's the memory resource)
- The storage backend (that's the memory plugin)
- The specific selection algorithm (that's a pattern-level strategy)

### Strategies as pattern opinions

A pattern implements its own context management strategy by composing memory operations. Examples:

- **Sliding window**: Retrieve most recent items until budget is full. Zero LLM cost.
- **Compaction**: When budget is tight, use an LLM to summarize old items into a shorter representation. Store the summary back into memory, remove the originals.
- **Semantic retrieval**: Embed the current query, retrieve top-K semantically similar items from memory, combine with a small window of recent items.
- **Hybrid**: Combine any of the above.

These are implementations, not framework contracts. Patterns own the logic; the framework provides the store/retrieve/update/remove primitives they compose.

---

## Reference Implementations

The following are **opinionated implementations** shipped as defaults. They are not part of the framework contract — they are pattern-level code that demonstrates how to use the primitives.

### In-memory backend

Ephemeral storage for conversation-scoped use. Items stored in a `Vec` behind the memory abstraction. Useful for simple patterns and testing. No persistence.

### SQLite backend

Durable storage as a single SQLite file. Supports cross-session memory. Items stored in `memory_entries` table with metadata. No semantic retrieval in the base backend — that is added by the semantic retrieval layer.

### Semantic retrieval layer

Adds semantic and keyword retrieval capabilities on top of any durable backend:

- **Chunking**: Splits large items into overlapping chunks for better retrieval granularity. `Chunker` trait with a default `TokenChunker` that consumes shared `TokenCounter`.
- **Embedding**: Embeds chunks via `EmbeddingProvider`. Stores vectors in SQLite as BLOBs.
- **Keyword indexing**: FTS5 virtual table for BM25 keyword search.
- **Hybrid scoring**: Combines semantic and keyword ranks. The specific algorithm (e.g., weighted reciprocal rank fusion) is a configuration choice, not a framework decision.
- **Entry collapse**: Multiple matching chunks from the same item collapse to one result.

Cosine similarity is computed in Rust, not via a SQLite extension. This keeps deployment to one binary plus one SQLite file.

### Sliding window context strategy

Selects items from newest to oldest until the budget is full. Never splits an atomic item. Returns a hard error if even the newest item doesn't fit — silent data loss is not acceptable.

### Compaction context strategy

Extends sliding window with LLM-generated summaries. When the budget is tight, selects a span of old items, summarizes them via a configured model, stores the summary back into memory, and retries. Failure behavior is configurable: stop the agent (`Fail`) or fall back to sliding window (`Degrade`).

---

## Errors

```rust
pub enum MemoryError {
    /// Malformed input (empty namespace, invalid content).
    InvalidEntry(String),
    /// EmbeddingProvider failure.
    Embedding(String),
    /// Backend storage failure (SQLite, I/O).
    Storage(String),
    /// Retrieval failure.
    Retrieval(String),
}
```

---

## Implementation Plan

### sc-3199: Define Memory primitives and plugin

1. Create `polaris_memory` crate.
2. Define memory abstractions (store, retrieve, update, remove).
3. Add in-memory backend for conversation-scoped use.
4. Add `MemoryPlugin`.
5. Tests: store/retrieve round-trip, upsert on key, namespace isolation.

### sc-3200: Add context management layer

1. Add context management abstractions for projecting memory onto LLM context windows.
2. Add budget accounting (input limit, output reservation, system prompt overhead).
3. Add sliding window as a default context strategy.
4. Tests: budget accounting, turn preservation, hard error on overflow.

Depends on: sc-3199.

### sc-3201: Add durable memory backend

1. Add SQLite backend implementing memory abstractions.
2. Single-file deployment.
3. Tests: persistence across close/reopen, cross-session retrieval.

Depends on: sc-3199.

### sc-3202: Add semantic retrieval support

1. Add `Chunker` trait and `TokenChunker`.
2. Add embedding-based retrieval via `EmbeddingProvider`.
3. Add FTS5 keyword retrieval.
4. Add hybrid scoring.
5. Tests: keyword hit beats unrelated semantic, semantic beats lexical mismatch, hybrid ranks combined highest.

Depends on: sc-3201, sc-3192 (TokenCounter), sc-3193 (EmbeddingProvider).

---

## Open Questions

1. The exact trait signatures for memory operations will be determined during sc-3199 implementation. The design doc describes the contract, not the final API surface.
2. `metadata_filter` for retrieval is exact-match in v1. If callers need richer filtering, add a typed filter DSL rather than ad-hoc JSON operators.
3. If memory size grows past local-agent scale, the answer is an alternate backend, not retrofitting SQLite into a vector database.
