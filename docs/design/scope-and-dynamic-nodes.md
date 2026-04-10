---
notion_page: https://www.notion.so/radiant-ai/Scope-Node-and-Dynamic-Node-327afe2e695d804c9c22fe83a19a7f51
title: "Design: Scope Node and Dynamic Node"
---

# Design: Scope Node and Dynamic Node

**Status:** Draft
**Layer:** 2 (Graph Execution)
**Crate:** `polaris_graph`
**Dependencies:** `polaris_system` (SystemContext, Resources)
**Date:** 2026-03-17

## Motivation

Polaris agents are directed graphs. Today, all nodes in a graph share a single flat namespace — the builder inlines branch subgraphs directly into the parent's node/edge vectors. This works for control flow (decisions, loops, switches, parallel branches) but cannot express two critical patterns found in real-world agent architectures:

1. **Embedding a pre-built agent graph as a scoped execution boundary** — e.g., Claude Code spawning sub-agents with isolated contexts, OpenFang chaining Hands sequentially.
2. **An agent constructing a sub-graph at runtime** — e.g., Devin's planner producing a step list that becomes an execution graph, Cursor's plan-execute-verify loop.

This design introduces two new node types — `Scope` and `Dynamic` — along with a `ContextPolicy` that governs how the parent's `SystemContext` is shared or isolated.

## Background: Current Graph Composition

### How subgraphs work today

The builder API creates subgraphs for control-flow constructs (decisions, loops, switches, parallel). These subgraphs are **inlined**: their nodes and edges are merged into the parent `Graph`'s flat vectors, and the control-flow node holds `NodeId` references to subgraph entry points.

```rust
// Decision branches are inlined into the same graph
graph.add_conditional_branch::<T, _, _, _>("name", predicate,
    |g| { g.add_system(true_handler); },   // nodes merged into parent
    |g| { g.add_system(false_handler); },   // nodes merged into parent
);
```

All inlined subgraph nodes share the parent's `SystemContext` (except parallel branches, which get `ctx.child()`).

### How `Graph::append` works today

`append()` merges another graph's nodes and edges into the current graph, wiring the current `last_node` to the other graph's entry via a sequential edge. The result is a single flat graph. This is inline embedding.

```rust
graph.add_system(step_d);
graph.append(sub_agent.to_graph())?;  // A, B, C inlined
graph.add_system(step_e);
// Result: D → A → B → C → E  (one flat graph, one context)
```

### How `SystemContext::child()` works today

```rust
pub fn child(&'parent self) -> SystemContext<'parent> {
    SystemContext {
        parent: Some(self),
        globals: self.globals.clone(),
        resources: Resources::new(),    // empty local scope
        outputs: Outputs::new(),        // empty output space
    }
}
```

A child context:
- `Res<T>` (read) — walks local scope → parent chain → globals
- `ResMut<T>` (write) — searches current (child) local scope only
- `Out<T>` (read output) — searches current (child) output space only

Parallel branches already use `ctx.child()` and merge outputs back on completion.

### What's missing

1. **No way to embed a graph as an opaque node with a context boundary.** You can inline (append) or build control flow, but you cannot create a scoped execution boundary where an embedded graph gets its own context.
2. **No way to generate a graph at runtime.** The graph structure is fixed at build time. An LLM cannot produce a plan that becomes an executable graph.
3. **No control over what resources cross a boundary.** Parallel branches always get a full child context. There is no way to say "give this sub-agent write access to `ContextManager` but nothing else."

## Design Overview

### Two composition modes

| Mode | Mechanism | Context | When to use |
|------|-----------|---------|-------------|
| **Inline** | `graph.append(other)` (exists today) | Shared — same `SystemContext` | Reusable graph fragments, agent decomposition for readability |
| **Scoped** | `graph.add_scope("name", other, policy)` (new) | Configurable — shared, inherited, or isolated | Sub-agent delegation, sandboxed execution, context boundaries |

The programming language analogy:
- **Inline** = macro expansion / inlining a function body
- **Scope** = calling a function (new stack frame with configurable visibility)

### Two new node types

| Node | Graph known at | Validated at | Use case |
|------|----------------|--------------|----------|
| `Scope` | Build time | Build time (`graph.validate()` recurses) | Static agent composition, sub-agent delegation |
| `Dynamic` | Runtime (produced by factory) | Runtime (before execution) | LLM-driven planning, adaptive execution |

A `Dynamic` node is a `Scope` node where the graph is produced at runtime. The executor handles them identically after the factory call returns.

---

## `ContextPolicy`

`ContextPolicy` controls how the parent's `SystemContext` is made available to the scoped graph.

### Definition

```rust
/// Controls how a parent `SystemContext` is shared with a scoped graph.
///
/// Fields are `pub(crate)` — callers construct policies through `shared()`,
/// `inherit()`, or `isolated()` constructors and chain `forward<T>()`.
/// Read access is through `mode()` and `forward_resources()` accessors.
pub struct ContextPolicy {
    pub(crate) mode: ContextMode,
    pub(crate) forward_resources: Vec<ResourceForward>,
}

/// Base isolation level for context sharing.
pub enum ContextMode {
    /// Pass the same `&mut SystemContext` through. No boundary.
    /// The scope is purely organizational — a labeled block.
    /// All resources and outputs are shared.
    Shared,

    /// Create a child context via `ctx.child()`.
    /// Reads walk the parent chain (parent locals + globals).
    /// Writes go to the child's own local scope.
    /// Outputs are in the child's output space, merged back on exit.
    Inherit,

    /// Create a fresh `SystemContext` with no parent.
    /// Nothing is accessible unless explicitly forwarded.
    Isolated,
}

/// Identifies a resource to forward across a scope boundary.
///
/// Fields are `pub(crate)` — constructed only through `ContextPolicy`
/// builder methods. `TypeId` is used for type-erased cloning; `type_name`
/// is retained for error messages and debugging.
///
/// The `clone_fn` is captured at policy-build time from the `T: Clone` bound
/// on the builder methods, so no separate `register_clone_fn` call is needed.
pub struct ResourceForward {
    pub(crate) type_id: TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) clone_fn: fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>,
}
```

### How each mode works

| Mode | Parent locals | Globals | Forwarded | Outputs |
|------|--------------|---------|-----------|---------|
| Shared | read + write | yes | n/a | parent's output space |
| Inherit | read only | yes | writable copies | merged back on exit |
| Isolated | **none** | yes | writable copies | merged back on exit |

#### `Shared`

```text
Parent SystemContext ←── scope graph reads/writes directly
```

The executor passes `&mut ctx` directly to the scoped graph's execution. No child context is created. The scope node is purely structural — it appears in the graph topology and fires hooks, but does not create a context boundary.

**`Res<T>`**: Resolves from the parent context (same as any normal node).
**`ResMut<T>`**: Resolves from the parent context (same as any normal node).
**`Out<T>`**: The scope's systems write to the parent's output space. After the scope completes, outputs are visible to subsequent nodes in the parent.

Use case: Organizational grouping. An agent's "memory recall" phase is a separate graph for readability, but shares the parent's context entirely.

#### `Inherit`

```text
Parent SystemContext
    └── child = ctx.child()
            ├── Res<T>: walks child → parent → globals ✓
            ├── ResMut<T>: child local scope only
            └── Out<T>: child output space (merged back on exit)
```

The executor creates `ctx.child()`. The child can read everything the parent can, but writes go to its own local scope. Outputs accumulate in the child and are merged back into the parent when the scope completes.

**Forwarded resources**: Before execution, the executor clones each listed resource from the parent's local scope into the child's local scope. This makes the resource available (both readable and writable) within the scope. The clone is one-way — mutations in the child do not propagate back to the parent. The scope communicates results via outputs (`Out<T>`), not via side-effects on forwarded resources.

Use case: Sub-agent with access to shared configuration but its own working state.

#### `Isolated`

```text
Fresh SystemContext (no parent chain)
    ├── Res<T>: only forwarded resources + globals (no parent locals)
    ├── ResMut<T>: only forwarded resources
    └── Out<T>: child output space (merged back on exit)
```

The executor creates a fresh `SystemContext` with no parent chain. Parent local resources are **not** accessible — only global (infrastructure) resources and explicitly forwarded resources are available. This is the key difference from `Inherit`: the parent chain is severed, so `Res<T>` cannot walk up to discover parent locals.

**Forwarded resources**: Cloned into the child's local scope, available for both read and write access.

**Global resources**: Inherited by default via `SystemContext::with_globals()`. Globals are infrastructure (model providers, tool registries) that every graph needs. The isolation is about *local* state, not global infrastructure. If a future use case needs full sandbox isolation (no globals), we add `Isolated { inherit_globals: bool }`.

Use case: Sandboxed execution of untrusted or LLM-generated graphs. Claude Code sub-agents that must not see the parent's conversation.

### Builder API

```rust
impl ContextPolicy {
    /// Shared mode — same context, no boundary.
    pub fn shared() -> Self {
        Self { mode: ContextMode::Shared, forward_resources: vec![] }
    }

    /// Inherit mode — child context, reads parent, writes own.
    pub fn inherit() -> Self {
        Self { mode: ContextMode::Inherit, forward_resources: vec![] }
    }

    /// Isolated mode — fresh context, only forwarded resources.
    pub fn isolated() -> Self {
        Self { mode: ContextMode::Isolated, forward_resources: vec![] }
    }

    /// Forward a local resource into the child scope.
    /// The resource is cloned from the parent's local scope.
    /// Only applicable to `Inherit` and `Isolated` modes.
    pub fn forward<T: LocalResource + Clone>(mut self) -> Self {
        self.forward_resources.push(ResourceForward {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            clone_fn: |any| Some(Box::new(any.downcast_ref::<T>()?.clone())),
        });
        self
    }
}
```

### Resource forwarding mechanism

Forwarding requires cloning a resource from one context to another. The current `Resources` container stores `Box<dyn Any + Send + Sync>` and does not support cloning by default.

**Approach**: The `forward` method requires `T: LocalResource + Clone`. The clone function is captured at policy-build time as a type-erased `fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>` stored directly in `ResourceForward`. This eliminates the need for a separate runtime `register_clone_fn` ceremony.

```rust
/// Clone a local resource using an externally-provided clone function.
/// Returns None if the resource doesn't exist or is currently write-locked.
pub fn clone_local_resource_with(
    &self,
    type_id: TypeId,
    clone_fn: fn(&dyn Any) -> Option<Box<dyn Any + Send + Sync>>,
) -> Option<Box<dyn Any + Send + Sync>>;
```

Only resources that are actually forwarded need `Clone` — this is enforced at compile time by the builder method bounds. Resources that are never forwarded across a scope boundary have no `Clone` requirement.

At runtime, the executor calls `clone_local_resource_with` for each entry in `forward_resources`, passing the captured `clone_fn`, and inserts the clone into the child context. If the resource is not found in the parent, a warning is emitted via `tracing::warn!`.

**Alternative considered**: Use serialization (via `Storable`) instead of `Clone`. Rejected: serialization is heavier, not all resources are `Storable`, and `Clone` is the idiomatic Rust approach.

### Output merging on scope exit

When a scope completes (in `Inherit` or `Isolated` mode), the child's outputs are merged into the parent's output space. This is identical to how parallel branches already work:

```rust
// From execute_parallel in run.rs:
let child_outputs: Vec<_> = child_contexts
    .iter_mut()
    .map(SystemContext::take_outputs)
    .collect();
drop(child_contexts);
for outputs in child_outputs {
    ctx.outputs_mut().merge_from(outputs);
}
```

The scope executor uses the same pattern but for a single child.

---

## `ScopeNode`

### Definition

```rust
/// A node that executes an embedded graph with a configurable context boundary.
///
/// The embedded graph is a self-contained directed graph that is executed
/// as a single unit within the parent graph. The `ContextPolicy` controls
/// how the parent's `SystemContext` is shared with the embedded graph.
///
/// From the parent graph's perspective, the scope node is a single opaque
/// node — execution enters the scope, runs the embedded graph to completion,
/// and exits from the scope's outgoing edge.
pub struct ScopeNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The embedded graph to execute.
    pub graph: Graph,
    /// Context sharing policy.
    pub context_policy: ContextPolicy,
}
```

### Builder API

```rust
impl Graph {
    /// Adds a scope node containing an embedded graph.
    ///
    /// The scope node executes the embedded graph as a single unit.
    /// The `ContextPolicy` controls context sharing between parent and child.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name for the scope node
    /// * `graph` - The embedded graph to execute
    /// * `policy` - Context sharing policy
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Embed a sub-agent with inherited context
    /// let sub_graph = research_agent.to_graph();
    /// graph
    ///     .add_system(prepare_task)
    ///     .add_scope("research", sub_graph, ContextPolicy::inherit())
    ///     .add_system(use_results);
    ///
    /// // Embed with specific resource forwarding
    /// graph.add_scope("sandbox", untrusted_graph,
    ///     ContextPolicy::isolated()
    ///         .forward::<TaskInput>()
    /// );
    /// ```
    pub fn add_scope(
        &mut self,
        name: &'static str,
        graph: Graph,
        policy: ContextPolicy,
    ) -> &mut Self {
        let scope = ScopeNode {
            id: NodeId::new(),
            name,
            graph,
            context_policy: policy,
        };
        let scope_id = scope.id.clone();

        // Connect to previous node if exists
        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, scope_id.clone());
        }

        // Set as entry if first node
        if self.entry.is_none() {
            self.entry = Some(scope_id.clone());
        }

        self.nodes.push(Node::Scope(scope));
        self.last_node = Some(scope_id);

        self
    }
}
```

Note: Unlike decision/loop/parallel, the embedded graph's nodes are NOT merged into the parent. The `ScopeNode` holds the `Graph` as a field. This is the key structural difference from all existing control-flow nodes.

### Execution

```rust
// In executor/run.rs, inside the main match on node type:
Node::Scope(scope) => {
    match &scope.context_policy.mode {
        ContextMode::Shared => {
            // No boundary — execute directly in parent context
            let count = self.execute(
                &scope.graph, ctx, hooks, middleware
            ).await?.nodes_executed;
            nodes_executed += count;
        }
        ContextMode::Inherit => {
            let mut child = ctx.child();
            // Forward resources (clone_fn captured at policy-build time)
            for fwd in &scope.context_policy.forward_resources {
                if let Some(cloned) = ctx.clone_local_resource_with(fwd.type_id, fwd.clone_fn) {
                    child.insert_boxed(fwd.type_id, cloned);
                }
            }
            let count = self.execute(
                &scope.graph, &mut child, hooks, middleware
            ).await?.nodes_executed;
            // Merge outputs back
            let child_outputs = child.take_outputs();
            ctx.outputs_mut().merge_from(child_outputs);
            nodes_executed += count;
        }
        ContextMode::Isolated => {
            let mut child = SystemContext::with_globals(ctx.globals());
            // Forward resources (clone_fn captured at policy-build time)
            for fwd in &scope.context_policy.forward_resources {
                if let Some(cloned) = ctx.clone_local_resource_with(fwd.type_id, fwd.clone_fn) {
                    child.insert_boxed(fwd.type_id, cloned);
                }
            }
            let count = self.execute(
                &scope.graph, &mut child, hooks, middleware
            ).await?.nodes_executed;
            // Merge outputs back
            let child_outputs = child.take_outputs();
            ctx.outputs_mut().merge_from(child_outputs);
            nodes_executed += count;
        }
    }

    // Follow parent's next sequential edge
    match self.find_next_sequential(graph, &current) {
        Ok(next) => current = next,
        Err(ExecutionError::NoNextNode(_)) => break,
        Err(err) => return Err(err),
    }
}
```

### Hooks

New hook schedules for scope lifecycle:

```rust
pub struct OnScopeStart;
impl Schedule for OnScopeStart {}

pub struct OnScopeComplete;
impl Schedule for OnScopeComplete {}
```

Events:

```rust
// In GraphEvent enum:
ScopeStart {
    node_id: NodeId,
    node_name: &'static str,
    context_mode: &'static str,  // "shared", "inherit", "isolated"
    inner_node_count: usize,
},
ScopeComplete {
    node_id: NodeId,
    node_name: &'static str,
    context_mode: &'static str,
    nodes_executed: usize,
    duration: Duration,
},
```

### Middleware

New middleware target:

```rust
pub struct ScopeInfo {
    pub node_id: NodeId,
    pub node_name: &'static str,
    pub context_mode: &'static str,
    pub inner_node_count: usize,
}
```

Registered in `MiddlewareAPI` as `scope: MiddlewareChain<ScopeInfo>`.

### Validation

`graph.validate()` recurses into scope nodes:

```rust
Node::Scope(scope) => {
    // The embedded graph must itself be valid.
    if scope.graph.is_empty() {
        errors.push(ValidationError::EmptyScopeGraph {
            node: scope.id.clone(),
            name: scope.name,
        });
    } else {
        if scope.graph.entry().is_none() {
            errors.push(ValidationError::ScopeGraphNoEntryPoint {
                node: scope.id.clone(),
                name: scope.name,
            });
        }
        let inner_result = scope.graph.validate();
        for inner_err in inner_result.errors {
            errors.push(ValidationError::ScopeGraphInvalid {
                node: scope.id.clone(),
                name: scope.name,
                inner: Box::new(inner_err),
            });
        }
        for inner_warn in inner_result.warnings {
            warnings.push(ValidationWarning::ScopeGraphWarning {
                node: scope.id.clone(),
                name: scope.name,
                inner: Box::new(inner_warn),
            });
        }
    }
}
```

New validation error variants:

```rust
/// A scope node contains an empty graph.
EmptyScopeGraph { node: NodeId, name: &'static str },

/// A scope node's embedded graph has no entry point.
ScopeGraphNoEntryPoint { node: NodeId, name: &'static str },

/// A scope node's embedded graph has a validation error.
ScopeGraphInvalid { node: NodeId, name: &'static str, inner: Box<ValidationError> },
```

New validation warning variant:

```rust
/// A scope node's embedded graph has a validation warning.
ScopeGraphWarning { node: NodeId, name: &'static str, inner: Box<ValidationWarning> },
```

### Resource validation

`executor.validate_resources()` recurses into scope nodes. For each scope, a synthetic child context is built matching the runtime semantics:

- **Shared**: same context — inner systems are validated against the parent context.
- **Inherit**: `ctx.child()` with forwarded mutable resources inserted — reads pass via parent chain, writes only pass for forwarded resources.
- **Isolated**: fresh `SystemContext` with globals + all forwarded resources — no parent chain, only explicitly forwarded resources and globals are available.

---

## `DynamicNode`

### Definition

```rust
/// A function that produces a `Graph` at runtime.
///
/// The factory receives mutable access to the current `SystemContext`,
/// enabling it to read outputs (e.g., `Out<Plan>`) and resources to
/// determine the graph structure.
///
/// # Errors
///
/// Returns `SystemError` on failure. The executor routes this to
/// error edges like any system error.
pub type BoxedGraphFactory = Box<
    dyn Fn(&mut SystemContext<'_>) -> BoxFuture<'_, Result<Graph, SystemError>>
        + Send
        + Sync
>;

/// A node that generates and executes a graph at runtime.
///
/// When the executor reaches a `DynamicNode`, it:
/// 1. Calls the factory to produce a `Graph`
/// 2. Applies default limits (timeouts, loop bounds)
/// 3. Validates the generated graph
/// 4. Creates a context per the `ContextPolicy`
/// 5. Executes the graph
/// 6. Merges outputs back to the parent
///
/// A `DynamicNode` is functionally a `ScopeNode` where the graph is
/// produced at runtime instead of build time.
pub struct DynamicNode {
    /// Unique identifier for this node.
    pub id: NodeId,
    /// Human-readable name for debugging and tracing.
    pub name: &'static str,
    /// The factory that produces a graph at runtime.
    pub factory: BoxedGraphFactory,
    /// Context sharing policy (same as ScopeNode).
    pub context_policy: ContextPolicy,
    /// Validation constraints for the generated graph.
    pub validation: DynamicValidation,
}
```

### `DynamicValidation`

```rust
/// Safety constraints for dynamically generated graphs.
///
/// These constraints are enforced at runtime after the factory produces
/// a graph, before execution begins. They prevent LLM-generated graphs
/// from being unbounded, hanging, or excessively complex.
pub struct DynamicValidation {
    /// Maximum number of nodes the generated graph may contain.
    /// Prevents runaway graph generation.
    /// Default: 100.
    pub max_nodes: usize,

    /// Maximum nesting depth within the generated graph.
    /// Counts nested control flow: a loop containing a decision
    /// containing another loop = depth 3.
    /// Default: 8.
    pub max_depth: usize,

    /// If true, every `LoopNode` in the generated graph must have
    /// `max_iterations` set. Prevents unbounded loops.
    /// Default: true.
    pub require_loop_limits: bool,

    /// Default `max_iterations` applied to `LoopNode`s that don't
    /// specify one. Applied as a patch before validation.
    /// Only used if `require_loop_limits` is false.
    /// Default: Some(50).
    pub default_loop_limit: Option<usize>,

    /// If true, every `SystemNode` in the generated graph must have
    /// a timeout set. Prevents nodes from blocking forever.
    /// Default: false.
    pub require_timeouts: bool,

    /// Default timeout applied to `SystemNode`s that don't specify one.
    /// Applied as a patch before validation.
    /// Default: Some(30s).
    pub default_timeout: Option<Duration>,

    /// Whether the generated graph may itself contain `DynamicNode`s.
    /// If false, nested dynamic generation is rejected at validation.
    /// Default: false.
    pub allow_nested_dynamic: bool,

    /// Whether the generated graph may contain `ScopeNode`s.
    /// Default: true.
    pub allow_scopes: bool,
}

impl Default for DynamicValidation {
    fn default() -> Self {
        Self {
            max_nodes: 100,
            max_depth: 8,
            require_loop_limits: true,
            default_loop_limit: Some(50),
            require_timeouts: false,
            default_timeout: Some(Duration::from_secs(30)),
            allow_nested_dynamic: false,
            allow_scopes: true,
        }
    }
}
```

### Builder API

```rust
impl Graph {
    /// Adds a dynamic node that generates and executes a graph at runtime.
    ///
    /// # Arguments
    ///
    /// * `name` - Human-readable name
    /// * `factory` - Closure that produces a `Graph` from the current context
    /// * `policy` - Context sharing policy
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// graph.add_system(create_plan);  // outputs Out<Plan>
    /// graph.add_dynamic("execute_plan", |ctx| {
    ///     Box::pin(async {
    ///         let plan = ctx.get_output::<Plan>()
    ///             .map_err(|e| SystemError::ParamError(e.to_string()))?;
    ///         let mut graph = Graph::new();
    ///         for step in &plan.steps {
    ///             graph.add_system(make_step_system(step));
    ///         }
    ///         Ok(graph)
    ///     })
    /// }, ContextPolicy::inherit().forward::<ContextManager>());
    /// ```
    pub fn add_dynamic<F>(
        &mut self,
        name: &'static str,
        factory: F,
        policy: ContextPolicy,
    ) -> &mut Self
    where
        F: Fn(&mut SystemContext<'_>) -> BoxFuture<'_, Result<Graph, SystemError>>
            + Send + Sync + 'static,
    {
        self.add_dynamic_with(name, factory, policy, DynamicValidation::default())
    }

    /// Adds a dynamic node with custom validation constraints.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// graph.add_dynamic_with("execute_plan", plan_factory,
    ///     ContextPolicy::inherit(),
    ///     DynamicValidation {
    ///         max_nodes: 20,
    ///         require_timeouts: true,
    ///         default_timeout: Some(Duration::from_secs(10)),
    ///         ..Default::default()
    ///     },
    /// );
    /// ```
    pub fn add_dynamic_with<F>(
        &mut self,
        name: &'static str,
        factory: F,
        policy: ContextPolicy,
        validation: DynamicValidation,
    ) -> &mut Self
    where
        F: Fn(&mut SystemContext<'_>) -> BoxFuture<'_, Result<Graph, SystemError>>
            + Send + Sync + 'static,
    {
        let dynamic = DynamicNode {
            id: NodeId::new(),
            name,
            factory: Box::new(factory),
            context_policy: policy,
            validation,
        };
        let dynamic_id = dynamic.id.clone();

        if let Some(prev_id) = self.last_node.clone() {
            self.add_sequential_edge(prev_id, dynamic_id.clone());
        }
        if self.entry.is_none() {
            self.entry = Some(dynamic_id.clone());
        }

        self.nodes.push(Node::Dynamic(dynamic));
        self.last_node = Some(dynamic_id);

        self
    }
}
```

### Execution sequence

The executor handles `DynamicNode` in seven steps:

```text
Step 1: Call factory
    factory(ctx) → Result<Graph, SystemError>
    ├─ Err(e) → route to error edge (agentic error, same as SystemNode failure)
    └─ Ok(graph) → continue to step 2

Step 2: Apply default patches
    For each LoopNode in the generated graph:
        if loop.max_iterations.is_none() && validation.default_loop_limit.is_some():
            loop.max_iterations = validation.default_loop_limit
    For each SystemNode in the generated graph:
        if sys.timeout.is_none() && validation.default_timeout.is_some():
            sys.timeout = validation.default_timeout

Step 3: Enforce hard limits
    if graph.node_count() > validation.max_nodes:
        → ExecutionError::DynamicGraphTooLarge { node, max, actual }
    if graph contains DynamicNode && !validation.allow_nested_dynamic:
        → ExecutionError::NestedDynamicNotAllowed { node }
    if graph contains ScopeNode && !validation.allow_scopes:
        → ExecutionError::ScopeNotAllowedInDynamic { node }
    if graph nesting depth > validation.max_depth:
        → ExecutionError::DynamicGraphTooDeep { node, max, actual }

Step 4: Enforce strict requirements
    if validation.require_loop_limits:
        for each LoopNode without max_iterations:
            → ExecutionError::DynamicLoopUnbounded { node, loop_node }
    if validation.require_timeouts:
        for each SystemNode without timeout:
            → ExecutionError::DynamicSystemNoTimeout { node, system_node }

Step 5: Full graph validation
    graph.validate()
    ├─ Checks entry point, edge validity, node completeness, etc.
    └─ Any error → ExecutionError::InvalidDynamicGraph { node, result: ValidationResult }

Step 6: Create context per ContextPolicy
    (Same logic as ScopeNode — Shared, Inherit, or Isolated with forwarding)

Step 7: Execute and merge
    executor.execute(&graph, &mut child_ctx, hooks, middleware)
    Merge outputs back to parent.
```

### Error handling

The factory call in step 1 is treated as an agentic error (like a system failure). If the dynamic node has an error edge, the executor follows it. If not, the error propagates.

Steps 3-5 (validation failures) are infrastructure errors — the agent produced an invalid graph. These propagate as `ExecutionError` without routing through error edges, because they indicate a programming error in the factory, not a recoverable agent failure.

### Hooks

New hook schedules:

```rust
pub struct OnDynamicStart;
impl Schedule for OnDynamicStart {}

pub struct OnDynamicGraphGenerated;
impl Schedule for OnDynamicGraphGenerated {}

pub struct OnDynamicComplete;
impl Schedule for OnDynamicComplete {}
```

Events:

```rust
DynamicStart {
    node_id: NodeId,
    node_name: &'static str,
},
DynamicGraphGenerated {
    node_id: NodeId,
    node_name: &'static str,
    generated_node_count: usize,
    validation_warnings: Vec<ValidationWarning>,
},
DynamicComplete {
    node_id: NodeId,
    node_name: &'static str,
    nodes_executed: usize,
    duration: Duration,
},
```

The `OnDynamicGraphGenerated` hook fires after the factory produces a graph and after validation passes. This is an observability point for logging or visualizing the generated graph structure.

### Build-time validation

A `DynamicNode` cannot be fully validated at build time (the graph doesn't exist yet). The build-time validator checks policy consistency:

```rust
Node::Dynamic(dyn_node) => {
    // Warn if strict loop limits are required but no default is set.
    // This means every loop in the generated graph must explicitly set
    // max_iterations — easy to forget in a factory.
    if dyn_node.validation.require_loop_limits
        && dyn_node.validation.default_loop_limit.is_none()
    {
        warnings.push(ValidationWarning::DynamicStrictLoopLimitsNoDefault {
            node: dyn_node.id.clone(),
            name: dyn_node.name,
        });
    }
    // Same for timeouts.
    if dyn_node.validation.require_timeouts
        && dyn_node.validation.default_timeout.is_none()
    {
        warnings.push(ValidationWarning::DynamicStrictTimeoutsNoDefault {
            node: dyn_node.id.clone(),
            name: dyn_node.name,
        });
    }
}
```

---

## Updated Node Enum

```rust
pub enum Node {
    /// Executes a system function.
    System(SystemNode),
    /// Routes flow based on predicate (binary branch).
    Decision(DecisionNode),
    /// Routes flow based on discriminator (multi-way branch).
    Switch(SwitchNode),
    /// Executes multiple paths of subgraphs concurrently.
    Parallel(ParallelNode),
    /// Repeats subgraph until termination condition.
    Loop(LoopNode),
    /// Executes an embedded graph with a configurable context boundary.
    Scope(ScopeNode),
    /// Generates and executes a graph at runtime.
    Dynamic(DynamicNode),
}
```

The `Node::id()` and `Node::name()` accessors are extended to cover the new variants.

### Impact on `reachable_nodes`

The `reachable_nodes` method currently follows internal links for Decision, Switch, Loop, and Parallel nodes. For `Scope` and `Dynamic` nodes, the embedded/generated graph is **not** traversed — it is opaque from the parent graph's perspective. This is by design: the scope is a boundary.

If full-graph analysis is needed (e.g., for visualization), a separate recursive traversal can walk into scope nodes explicitly.

### Impact on `collect_branch_output_types`

Scope and Dynamic nodes do not contribute to parent-level output type analysis. Their outputs are opaque — they are merged back at runtime, but the types are not known statically (especially for Dynamic nodes where the graph doesn't exist at build time).

---

## New `ExecutionError` Variants

```rust
/// The generated graph exceeds the node count limit.
DynamicGraphTooLarge {
    node: NodeId,
    max: usize,
    actual: usize,
},

/// The generated graph exceeds the nesting depth limit.
DynamicGraphTooDeep {
    node: NodeId,
    max: usize,
    actual: usize,
},

/// The generated graph contains a DynamicNode but nested dynamics are disallowed.
NestedDynamicNotAllowed {
    node: NodeId,
},

/// A loop in the generated graph has no max_iterations when required.
DynamicLoopUnbounded {
    node: NodeId,
    loop_node: NodeId,
},

/// A system in the generated graph has no timeout when required.
DynamicSystemNoTimeout {
    node: NodeId,
    system_node: NodeId,
},

/// The generated graph failed validation.
InvalidDynamicGraph {
    node: NodeId,
    result: ValidationResult,
},

/// A scope in the generated graph is not allowed.
ScopeNotAllowedInDynamic {
    node: NodeId,
},
```

---

## Real-World Pattern Mapping

This table shows how the new node types enable each real-world agent architecture.

| Pattern | Composition | Context Policy | Detail |
|---------|-------------|----------------|--------|
| **Claude Code** sub-agents | `Parallel` of `Scope` nodes | `Isolated` + `forward::<TaskInput>()` | Up to 10 parallel scopes, each with a fresh context. Parent sends task via forwarded resource, receives result via output merge. |
| **Claude Code** main loop | Inline (`append`) | `Shared` | The main while-loop with tool calling is a flat graph — no scope needed. |
| **Devin** planner → executor | `Dynamic` | `Inherit` + `forward::<ContextManager>()` | Planner system outputs `Plan`. Dynamic node's factory reads `Out<Plan>` and builds a graph with one system per step. Coder/Critic within each step are conditional branches. |
| **Devin** dynamic re-planning | Nested `Dynamic` inside a `Loop` | `Inherit` | Outer loop: plan → dynamic execute → roadblock check. If roadblock, loop back to re-plan. Dynamic's `allow_nested_dynamic: false` prevents infinite nesting. |
| **Cursor** plan-execute-verify | `Dynamic` | `Inherit` | Plan system outputs `Vec<Step>`. Dynamic builds a graph: for each step → execute → verify (conditional: fix or continue). |
| **OpenFang** Hand chaining | Sequential `Scope` nodes | `Inherit` | Each Hand is a scope. Researcher scope → Predictor scope → Clip scope → broadcast. Each reads parent resources (model registry) but has own working state. |
| **OpenClaw** skills | Inline (`append` or `pipe`) | `Shared` | Skills are just extra systems in the agent's graph. No boundary needed. |
| **ZeroClaw** delegate agents | `Scope` | `Inherit` + `forward::<DelegateConfig>()` | Delegate gets a subset of parent's capabilities via selective forwarding. |

---

## Implementation Plan

### Phase 1: `ContextPolicy` and `ScopeNode`

1. Add `ContextPolicy`, `ContextMode`, `ResourceForward` (with captured `clone_fn`) to `polaris_graph`
2. Add `clone_local_resource_with` to `SystemContext` in `polaris_system`
3. Add `SystemContext::with_globals(globals: Arc<Resources>)` constructor for `Isolated` mode
4. Add `ScopeNode` struct to `node.rs`
5. Extend `Node` enum with `Scope` variant
6. Add `add_scope` to the builder API in `builder.rs`
7. Add execution logic in `executor/run.rs`
8. Add validation logic in `validation.rs`
9. Add hook schedules and events
10. Add middleware target
11. Add tests:
    - Scope with `Shared` policy (systems read/write parent context)
    - Scope with `Inherit` policy (systems read parent, write own)
    - Scope with `Isolated` policy (systems see nothing without forwarding)
    - Scope with `forward` (resource cloned, mutations don't propagate back)
    - Scope output merging (outputs visible to parent after scope completes)
    - Nested scopes (scope within a scope)
    - Scope within a loop
    - Scope within a parallel branch
    - Validation: empty scope graph, no entry point, inner validation errors bubble up
    - Resource validation across scope boundaries

### Phase 2: `DynamicNode`

1. Add `BoxedGraphFactory` type alias
2. Add `DynamicValidation` struct with `Default` impl
3. Add `DynamicNode` struct to `node.rs`
4. Extend `Node` enum with `Dynamic` variant
5. Add `add_dynamic` and `add_dynamic_with` to the builder API
6. Add `apply_defaults` method to `DynamicValidation` (patches loop limits, timeouts)
7. Add `enforce_limits` method to `DynamicValidation` (checks node count, depth, nesting)
8. Add execution logic: factory call → patch → validate → execute
9. Add error edge support for factory failures
10. Add new `ExecutionError` variants
11. Add hook schedules and events
12. Add build-time validation warnings for policy consistency
13. Add tests:
    - Dynamic node with simple factory
    - Dynamic node with factory that reads `Out<T>`
    - Dynamic node with `forward`
    - Factory failure → error edge
    - Generated graph exceeds `max_nodes` → error
    - Generated graph has unbounded loop → error (when `require_loop_limits`)
    - Generated graph has no-timeout system → error (when `require_timeouts`)
    - Generated graph fails validation → error
    - Nested dynamic disallowed → error
    - Nested dynamic allowed → succeeds
    - Dynamic within a loop (plan → execute → re-plan)
    - Default patches applied correctly

### Phase 3: Documentation and examples

1. Update `docs/reference/graph.md` with Scope and Dynamic node documentation
2. Update `docs/taxonomy.md` if new concepts affect layer boundaries
3. Add code examples demonstrating each real-world pattern
4. Update `CLAUDE.md` quick reference tables

---

## Open Questions

1. **Should forwarded resources be written back to the parent on scope exit?** Current design: no. The scope communicates via outputs only. Write-back semantics can be added later as `forward_with_writeback::<T>()` if a use case demands it.

2. **Should `Isolated` contexts inherit globals?** Current design: yes, via `SystemContext::with_globals()`. Rationale: globals are infrastructure (model providers, tool registries). A future `Isolated { inherit_globals: false }` option can provide full sandboxing.

3. **Should `DynamicNode` support error edges for factory failures?** Current design: yes. The factory call is treated as an agentic error. If an error edge exists from the dynamic node, the executor follows it. This enables recovery patterns (e.g., fall back to a simpler graph).

4. **Should the generated graph's output types be declared at build time?** Current design: no. Dynamic outputs are opaque to the parent graph's static analysis. If the parent needs to read a specific `Out<T>`, the developer must ensure the factory produces a graph that outputs `T`. This is a documentation/convention concern, not a type-system enforcement.

5. **How does graph visualization handle Scope and Dynamic nodes?** Scope nodes should be rendered as a collapsible group containing the embedded graph. Dynamic nodes should be rendered as a placeholder (e.g., a dashed box) since the graph doesn't exist until runtime. Post-execution visualization could show the actual generated graph.
