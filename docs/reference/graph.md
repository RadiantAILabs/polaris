---
notion_page: https://www.notion.so/radiant-ai/Graph-327afe2e695d80ffb941f98a5ec6d3ee
title: Graph Execution
---

# Graph Execution

Agent logic in Polaris is expressed as a directed graph of systems and control flow constructs. The `polaris_graph` crate provides the graph structure, a builder API for constructing it, and an executor for running it.

## Graphs

A `Graph` is a directed graph where nodes represent computation or control flow and edges define the connections between them. The graph is constructed using a builder API that handles node allocation, edge creation, and subgraph composition.

```rust
use polaris_graph::Graph;

let mut graph = Graph::new();
graph
    .add_system(receive_input)
    .add_system(reason)
    .add_system(respond);
```

The first node added becomes the graph's entry point. Each subsequent call to the builder connects the new node to the previous one via a sequential edge. This implicit chaining means that for linear pipelines, the builder reads as a sequence of steps.

Before execution, a graph can be validated via `graph.validate()`, which checks that: the graph has a valid entry point; all edges reference valid nodes; decision and switch nodes have the required predicates and branches; parallel nodes have branches; loop nodes have a body and termination condition or iteration limit; and dynamic nodes have a non-empty, key-unique inline candidate set whose every candidate is structurally valid, flat (no nested `Scope`/`Dynamic` boundary), and signature-compatible with the slot contract. Advanced checks include verifying that loop termination predicates can read outputs produced within the loop body and warning about conflicting output types in parallel branches.

A separate runtime validation pass, `executor.validate_resources()`, checks that all `Res<T>`, `ResMut<T>`, and `Out<T>` parameters can be satisfied before execution begins. Output reachability is validated along the linear (sequential) chain — each system's `Out<T>` dependencies are checked against outputs produced by preceding systems, as is each loop's termination predicate input (evaluated before the first iteration, so the loop body cannot satisfy it). Non-system nodes contribute all output types reachable from their subgraphs. See [Execution Context — Resource Validation](context.md#resource-validation) for details. Independently of this opt-in pass, the executor always verifies loop predicate inputs at run start — see [Loop](#loop).

Both passes are instances of the phase rules that govern every check in this layer, defined next.

## Verification Phases

Layer 2 recognizes five verification phases. Knowledge grows monotonically across them — types, then structure, then interfaces, then the live context, then the paths actually taken. Every check the graph layer performs belongs to exactly one phase and is bound by the rules below; a check that cannot satisfy its phase's rules must move to a later phase.

| Phase | When | Knowledge available |
|-------|------|---------------------|
| **1 — Rust compile time** | `cargo build` | Types, ownership, trait bounds |
| **2 — Graph validate time** | `graph.validate()`, plus errors raised eagerly by builder calls | The graph's own structure — nothing outside it |
| **3 — Composition time** | Signature matching: `validate()` for inline dynamic candidates, `SubgraphRegistry::register` for registry candidates (see [Dynamic](#dynamic)) | Both fragments' interfaces (`GraphSignature`) |
| **4 — Run start** | `execute()` before the first node — after `OnGraphStart` hooks and graph-execution middleware run — and again at each embedded scope/dynamic boundary | Full graph plus live context contents; **not** path choices |
| **5 — Execution time** | Node dispatch onward | Everything, exactly |

### Phase Rules

These rules are binding on every current and future Layer 2 check:

1. **No false positives, at any phase.** A check MUST NOT reject a graph that could still execute successfully given facts a later phase would reveal. A rejection is legitimate only when failure is certain on every possible path.
2. **Catch at the earliest sound phase.** Every invariant MUST be enforced at the earliest phase where Rule 1 holds for it, and MUST NOT be deferred to a later one. Prefer phase 1 whenever the invariant is expressible in the type system.
3. **Validate time sees only the fragment.** A phase-2 check MUST depend only on the graph's own structure — never on context contents, hooks, or the embedding graph. A graph is a fragment that can be embedded anywhere, so an environment-dependent rejection here violates Rule 1 by construction.
4. **Interfaces never under-claim.** Signature derivation MUST be pessimistic: a requirement may be omitted from `requires_outputs` only when production is guaranteed on every path. Over-claiming is sound (a composability tax, to be narrowed over time); under-claiming admits candidates that fail at runtime and is forbidden.
5. **Run start rejects only impossibility.** A phase-4 check MUST be optimistic: it may reject only what cannot succeed on *any* path, crediting every possible producer and everything already in the context. It is a fail-fast courtesy, never the guarantee — the authoritative check remains at execution time. The loop entry check ([Loop](#loop)) is the model instance.
6. **A check MUST NOT borrow another phase's crediting model.** Pessimistic interface analysis (phase 3) and optimistic run-start analysis (phase 4) answer different questions — "what does this fragment need from its surroundings?" versus "can this run possibly satisfy this need?" — and are documented as a deliberate split in `signature.rs`. Applying the pessimistic model at phase 4 falsely rejects runnable graphs; applying the optimistic model at phase 3 under-claims interfaces. Both violate the rules above.
7. **An advisory pass MUST NOT contradict the mandatory check it predicts.** Whatever an opt-in pass admits, the corresponding mandatory check MUST also admit. Concretely: `validate_resources` is phase-4 analysis performed ahead of time and credits no more than the mandatory run-start entry check does, so pre-flight success is never followed by a run-start refusal from the same analysis.
8. **Execution-time failures MUST be typed and routed, never panics.** Anticipated failures route through the graph's own error flow ([Error Handling](#error-handling)); infrastructure failures propagate as typed `ExecutionError` values through the normal failure path, firing `OnGraphFailure`, and leave the process running.

To place a new check, ask what it needs to know. If the answer includes "who embeds this graph," it belongs no earlier than phase 3. If it includes "what the caller seeded," no earlier than phase 4. If it includes "which branch was taken," it belongs at phase 5, expressed as a typed error.

## Adding System Nodes

There are three methods for adding a system node to a graph, each suited to a different use case.

**`add_system`** — the most common method. Adds the node and returns `&mut Self` for fluent chaining. Use this for simple linear pipelines where no per-node configuration is needed.

```rust
graph
    .add_system(step_a)
    .add_system(step_b)
    .add_system(step_c);
```

**`add_system_node`** — adds the node and returns its `NodeId`. Use this when you need the ID for later reference, such as wiring conditional branches or attaching edges manually.

```rust
let reason_id = graph.add_system_node(reason);
let act_id = graph.add_system_node(act);
```

**`system`** — adds the node and returns a `SystemNodeBuilder` for configuring error handling, timeouts, and retry policies. Call `.done()` to return to `&mut Graph` for continued chaining.

```rust
graph.system(risky_operation)
    .with_timeout(Duration::from_secs(30))
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .on_error(|h: &mut Graph| { h.add_system(fallback); })
    .on_timeout(|h: &mut Graph| { h.add_system(timeout_handler); })
    .done()
    .add_system(next_step);
```

All three methods accept any type implementing `IntoSystemNode`, which includes bare async functions and `(schedule, system)` tuples for attaching custom hook schedules.

## Construction Patterns

### Sequential

Systems are connected in order. Each `add_system` call appends a node and links it to the previous one.

```rust
graph
    .add_system(reason)
    .add_system(act)
    .add_system(respond);
```

### Conditional Branch

A decision node evaluates a typed predicate against a system output and routes execution to one of two subgraphs. The type parameter specifies which system output type to read (e.g., `Out<ReasoningResult>`), and the predicate closure receives a reference to that output and returns a boolean.

```rust
graph
    .add_system(reason)
    .add_conditional_branch::<ReasoningResult, _, _, _>(
        "should_use_tool",
        |result| result.needs_tool,
        |g| g.add_system(execute_tool),
        |g| g.add_system(respond),
    );
```

After the selected branch completes, execution continues from the decision node's next sequential edge.

### Multi-Way Branch

A switch node evaluates a discriminator against a system output that returns a string key, then routes to the matching case subgraph. The type parameter specifies which system output type to read (e.g., `Out<ClassificationResult>`), and the discriminator closure receives a reference to that output and returns a case key.

```rust
graph
    .add_system(classify)
    .add_switch::<ClassificationResult, _, _>(
        "route",
        |result| result.category,
        vec![
            ("question", |g: &mut Graph| { g.add_system(answer); }),
            ("task", |g: &mut Graph| { g.add_system(execute); }),
        ],
        Some(|g: &mut Graph| { g.add_system(fallback); }),
    );
```

### Parallel Execution

A parallel node forks execution across multiple subgraphs. Each branch receives its own child context and runs concurrently. If any branch fails, the remaining branches are cancelled, the error propagates, and no branch outputs reach the parent. The child context inherits resource reads through the parent chain but starts with an **empty output store**, so a branch cannot read an `Out<T>` produced upstream of the parallel node. Put values needed across the fork in a resource on the parent context and read them in each branch with `Res<T>`.

The parallel node is both the entry and exit point. Once all branches complete successfully, every branch's outputs are merged in declaration order and execution continues from the parallel node's outgoing sequential edge. This includes outputs produced behind a nested scope boundary, outputs declared by a nested dynamic node's contract, and outputs from a handler path. At run time, two or more branches producing the same output type collapse to the last one.

`validate()` reports `ValidationWarning::ConflictingParallelOutputs`, not an error, when it finds the same type from reachable `System` nodes in multiple branches. This check does not cover outputs hidden behind a nested scope, a dynamic node's contract, or a handler path. Validation also does not catch a branch `Out<T>` read that appears satisfied only by an output above the parallel node: signature derivation credits the parent chain to the branch region, but the branch's empty output store makes the read fail at run time with `ParamError::OutputNotFound`.

```rust
graph
    .add_system(plan_tools)
    .add_parallel("execute_tools", vec![
        |g: &mut Graph| g.add_system(tool_a),
        |g: &mut Graph| g.add_system(tool_b),
    ])
    .add_system(aggregate_results);
```

### Loop

A loop node repeats its body subgraph until a termination predicate returns true or an iteration limit is reached. The termination predicate is evaluated before each iteration — including the *first*, which happens before the body has ever run. The context persists across iterations, so outputs from iteration N are available to iteration N+1.

Because the first check precedes the body, the predicate's input type must already exist when the loop is reached: either an earlier node in the graph produces it, or a caller driving the graph directly pre-seeds it into the context with `SystemContext::insert_output` before executing. Earlier production may come from a preceding system, a parallel fan-out, a scope whose outputs merge back, or a dynamic node's contract `produces`. These sources still count when nested inside a control-flow branch. Outputs are the work products of systems: only system execution (and its merge-back across boundaries) is a sanctioned writer of the output channel — hooks and middleware receive context access for resources and observability, and writing outputs from them is unsupported.

How conditional production is credited depends on the verification phase:

- **Composition signatures are pessimistic.** A decision or switch branch has not been selected, so branch production is not guaranteed and cannot remove the predicate input from `requires_outputs`. A subgraph that begins with a loop therefore advertises the seed, and slot contracts must supply it (see [Dynamic](#dynamic)).
- **Pre-flight and run-start checks are optimistic.** Before path selection, they credit every branch that could produce the input — including production behind nested scopes, dynamic contracts, loop bodies, and parallel branches. They may reject only when no path can produce the input and the live context lacks it.
- **Execution is exact.** Once a decision or switch has selected a branch, a genuinely absent input surfaces as `PredicateError::OutputNotFound` when the loop is reached. Loops inside branch interiors likewise remain execution-time checks.

If no earlier node can possibly produce the input and the context lacks it, the executor fails before running the first node with `ExecutionError::LoopPredicateInputMissingOnEntry`, naming the loop and missing type. The run-start check is a fail-fast courtesy, not the guarantee.

```rust
graph
    .add_system(init_state) // produces LoopState: the first predicate check reads it
    .add_loop::<LoopState, _, _>(
        "react_loop",
        |state| state.is_done || state.iterations >= 10,
        |g| {
            g.add_system(reason)
             .add_system(act)
             .add_system(observe);
        },
    );
```

For loops that should run a fixed number of times without a predicate, `add_loop_n` accepts only an iteration count.

### Scope

A scope node executes an embedded graph with a configurable context boundary. A `ContextPolicy` is constructed upfront and passed to `add_scope`; it determines which resources cross from parent to child and how each one crosses. It does not filter outputs flowing back to the parent.

Two constructors anchor the surface:

| Constructor | Meaning |
|---|---|
| `ContextPolicy::shared()` | No boundary at all — the inner graph reuses the parent context. |
| `ContextPolicy::new()` | Empty per-resource policy — nothing crosses unless added. |

Compose by chaining per-resource verbs. Each acts on one resource type:

| Verb | Mechanism | Requires of `T` |
|---|---|---|
| `share::<T>()` | Child reads via parent chain (zero copy) | nothing |
| `forward::<T>()` | `Clone::clone` into child's local scope | `T: Clone` |
| `fork::<T>()` | `ForkStrategy::fork(&self)` into child's local scope | `T: ForkStrategy` |
| `forward_fresh::<T>()` | Re-invoke `T`'s registered factory | `T` registered via `Server::register_local(...)` |
| `exclude::<T>()` | Suppress any earlier verb / `share_rest()` for `T` | nothing |
| `share_rest()` | Apply `share` to every resource not otherwise mentioned | nothing |

Verbs are applied in declaration order; later verbs override earlier ones for the same `T`.

```rust
// Sub-agent: shared registry, fresh fragment store.
let policy = ContextPolicy::new()
    .share::<ToolRegistry>()
    .forward_fresh::<FragmentStore>();
graph.add_scope("sub_agent", inner_graph, policy);

// Sandbox: only `Memory` is cloned across; everything else is invisible.
let policy = ContextPolicy::new().forward::<Memory>();
graph.add_scope("sandboxed", inner_graph, policy);

// "Mostly inherit, but override one resource."
let policy = ContextPolicy::new()
    .fork::<FragmentStore>()
    .share_rest();
graph.add_scope("scope", inner_graph, policy);
```

At runtime the executor branches on the policy: `shared()` reuses the parent context; every other policy creates a child via `ctx.child_filtered(...)` so chain-reads only expose explicitly-shared types. A `share` verb (including `share_rest()`) widens that filter; pure-isolation policies (only `forward` / `fork` / `forward_fresh`) use an empty `AllowOnly` filter — the child still sees globals through the retained parent reference, but no parent local is readable. The reference is kept (rather than dropped) so a blocked read can report `ResourceOutOfScope` naming the verb that would expose it.

After the inner graph completes, child outputs are merged back into the parent context regardless of policy. With `shared()` they were written into that context directly; every non-shared policy merges the child's output channel on exit. The policy controls resource ingress, not output egress. See [Execution Context — Context Flow](context.md#context-flow-through-graph-execution) for details.

### Dynamic

A dynamic node **selects** one of several pre-built candidate subgraphs at execution time and runs the chosen one through the same context boundary as [Scope](#scope). Use it to *pick* a subgraph by a runtime signal (where `Switch` only routes to branches wired at build time), or to *swap* the unit behind a slot between turns without rebuilding the parent graph.

Selection is driven by a **selector** — a `Fn(&SystemContext<'_>) -> Arc<str>` that names a candidate key from anything readable in the context. The candidate set comes from one of two sources:

| Source | Constructor | Candidate set | Mutable between turns |
|---|---|---|---|
| **Inline** | `add_dynamic(...)` | fixed at build time | no |
| **Registry** | `add_dynamic_registry(...)` | a `SubgraphRegistry` local resource | yes |

Every dynamic node declares a **contract** `GraphSignature` describing the slot's interface — the outputs/resources it requires from its surroundings and the outputs it produces. Each candidate's own signature is checked against this contract: inline candidates during `validate()`, registry candidates at `SubgraphRegistry::register` time. **No graph is ever selected whose shape was not verified against the slot first**, so the parent — validated once against the fixed contract — stays sound no matter which candidate runs. This is composition preserving *soundness* without preserving *semantics*: the signature bounds the swappable space; which candidate runs inside it is a runtime choice.

A candidate is **sound to substitute** for the slot exactly when it demands no more than the slot guarantees and produces at least what the slot promises:

- `candidate.requires ⊆ slot.requires` — it reads no more than what's guaranteed present. Needing *fewer* inputs is fine; the extra available inputs just go unused.
- `candidate.requires_outputs ⊆ slot.requires_outputs` — the same logic for free `Out<T>` reads.
- `candidate.produces ⊇ slot.produces` — it produces at least what downstream expects. Producing *more* is harmless; downstream ignores the extras.

The rule is **contravariant in inputs, covariant in outputs**. v1 enforces the stricter special case — *exact-set match* (all three compare by `=`, not `⊆`/`⊇`), so a candidate must match the slot's interface exactly. That is a sound under-approximation: it never admits an unsound candidate, only rejects some safe ones. Loosening to the subset/superset rule above ("variance") is a localized future change. Where two signatures diverge, `GraphSignature::diff` reports the exact axes and types (returned as a `SignatureDiff`, and carried on every mismatch error), so a rejection reads as an edit rather than a puzzle.

Distinct from substitution is **capability matching** — `GraphSignature::satisfies`, a second comparison relation used by consumer registries (e.g. the sessions [capability-contract registry](./sessions.md#capability-contracts)) to ask whether an agent *can do* what a named capability describes. It matches by subsumption: the slot's `requires` and `produces` must be **subsets** of the candidate's, and the candidate's free-output reads must not exceed the slot's. Note the `requires` direction is *inverted* relative to substitution — a capability slot names the inputs the capability's driver will feed the agent, so a candidate's extra requires are presumed environment-provided by its registrant's setup (conservative derivation reports self-inserted resources as required), not violations. `satisfies_diff` is its lockstep diff mode, reporting only the violating directions. `compatible_with` remains the relation for `Dynamic` slot substitution; the two are deliberately separate, each named for its soundness envelope.

The contract is also validated against the parent's *surroundings*, not only the candidates — the contract is the interface, so this holds even for a registry slot with no registered candidates. At `validate_resources` time every resource the contract `requires` must be reachable from the context the candidate will run against (`DynamicContractMissingResource` otherwise), and every free `Out<T>` in `requires_outputs` must be produced upstream (`DynamicContractMissingOutput`). Free outputs never cross a non-shared boundary inward, so a non-shared `ContextPolicy` combined with a nonempty `requires_outputs` is statically unsatisfiable (`DynamicContractOutputsBlocked`). Finally, because signature derivation is opaque at nested context boundaries, a candidate that itself contains a `Scope` or `Dynamic` node is **rejected** until recursive derivation lands — inline candidates during `validate()` (`DynamicCandidateNested`), registry candidates at `register()` (`RegistryError::NestedCandidate`) — so candidates must be flat.

The slot's fixed configuration — its `contract`, boundary `ContextPolicy`, and optional default key — is bundled into a `DynamicSlot`, passed as one value so there are no positional holes at the call site:

```rust
// Inline: route between two pre-built sub-agents by a runtime `Choice`.
let contract = GraphSignature::new().require_read::<Base>().produce::<Reply>();
graph.add_dynamic(
    "route",
    // The selector may return any `impl Into<Arc<str>>` — `&str` included.
    |ctx| ctx.get_resource::<Choice>().map_or("fast", |c| c.pick),
    [("fast", fast_agent), ("thorough", thorough_agent)],
    DynamicSlot::new(contract.clone(), ContextPolicy::shared())
        .with_default_key("fast"), // chosen when the selector's key is absent
);

// Registry: the slot's candidate is swapped between turns via `SubgraphRegistry`.
graph.add_dynamic_registry(
    "route",
    select_plan,
    DynamicSlot::new(contract.clone(), ContextPolicy::shared()),
);

let mut registry = SubgraphRegistry::new(contract);
registry.register("v1", planner_v1)?; // rejected unless compatible with the contract
ctx.insert(registry);
```

If the selector returns a key with no matching candidate, the node falls back to its configured default; with no default it fails with `ExecutionError::DynamicCandidateNotFound`. Registry *resolution* failures are distinct and unrecoverable — the default key resolves through the same registry — so they surface as their own typed errors rather than a candidate miss: no registry in context is `DynamicRegistryMissing`, a write-locked one is `DynamicRegistryBusy`, and one hidden by an enclosing scope's policy is `DynamicRegistryOutOfScope` (carrying crossing-verb guidance). For the registry source, the resolved `SubgraphRegistry`'s contract must itself be compatible with the node's slot contract — candidates are signature-checked against the *registry's* contract at `register()` time, so a registry whose contract diverges from the slot is refused at execution with `ExecutionError::DynamicContractMismatch` (which carries a `SignatureDiff` naming the divergence) rather than running a candidate the parent graph was never validated against. The chosen candidate runs under the node's `ContextPolicy` exactly like a scope — see [Scope](#scope) for the boundary and output-merge semantics.

**Bounding runtime candidates.** Signature compatibility constrains a candidate's *interface*, not its *cost*: a candidate may contain arbitrarily many nodes. A candidate may **not**, however, nest another `Scope` or `Dynamic` node — signature derivation cannot see across those boundaries, so such candidates are rejected at build/registration time (see above). That also forecloses a self-referential dynamic candidate recursing unbounded, before the executor's recursion limit (`GraphExecutor::max_recursion_depth`, default 64) would ever catch it. Total node count and wall-clock time are still *not* bounded by default, so when candidates are generated or chosen from untrusted input (e.g. an LLM), set a [`max_duration`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/struct.Graph.html#method.with_max_duration) on the candidate graphs; it wraps each candidate's execution in a timeout at the dynamic boundary.

See the [`SubgraphRegistry`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/struct.SubgraphRegistry.html) reference for the registry's scope, access pattern, and a worked example, and the [integration guide](./guide.md#common-integration-patterns) row *"Select or swap a subgraph behind a slot at runtime"* for where this pattern fits among the others.

### Duplicating a Graph

`Graph::duplicate()` returns a structurally independent copy with **fresh node and edge IDs**. Every internal reference — branch targets, switch cases, loop bodies, parallel branches, and the `entry` / `last_node` markers — is remapped to the new IDs, and a `Scope` node's embedded graph is duplicated recursively. Modifying the copy (adding, removing, or rewiring nodes) does not affect the original.

This is the basis of the **duplicate-and-modify** workflow: build a base graph, duplicate it, mutate the copy, and compare. It is `duplicate()` rather than `Clone` on purpose — node/edge IDs are semantic identity, and they intentionally change in the copy.

```rust
let mut base = Graph::new();
base.add_system(reason).add_system(act);

let mut variant = base.duplicate();
variant.add_system(reflect); // does not touch `base`
```

System, predicate, and discriminator *behavior* is immutable and is shared cheaply (via `Arc`) rather than duplicated, so the copy executes identically to the original. A graph that passes `validate()` duplicates to one that also passes it.

### Finding Nodes by Name

Built graphs expose nodes by `NodeId`, but IDs are random nanoids — to locate a node you know by name (the prerequisite for transforming it), use the name-based lookups:

| Method | Returns |
|--------|---------|
| `find_node_by_name(name)` | `Option<&Node>` — the first node with that name |
| `find_nodes_by_name(name)` | `impl Iterator<Item = &Node>` — every match in insertion order (yields nothing when none) |
| `find_system_by_name(name)` | `Option<(NodeId, &SystemNode)>` — the first **system** node, skipping control-flow nodes |

```rust
let mut graph = Graph::new();
graph.add_system(reason).add_system(respond);

if let Some((id, _system)) = graph.find_system_by_name("reason") {
    // `id` is the handle for rewiring edges to or from this node.
}
```

Names are **not** unique: the branch subgraphs the builder produces (decision branches, switch cases, loop bodies, parallel branches) live in the same flat node list as the top level, so two nodes can share a name — `find_nodes_by_name` surfaces them all. A system node's name is its function name (`add_system(reason)` → `"reason"`); a control-flow node's name is its builder label. Lookup searches the graph's own node list only and does not descend into a `Scope` node's embedded graph, mirroring `nodes()` and `get_node()`.

### Per-Node Context Semantics

Different node types have different relationships to the `SystemContext`:

| Node Type | Creates Child Context? | Context Behavior |
|-----------|----------------------|------------------|
| **System** | No | Executes in parent's context directly |
| **Decision** | No | Evaluates predicate and branches in parent's context |
| **Switch** | No | Evaluates discriminator and routes in parent's context |
| **Loop** | No | Body runs in same context across iterations; outputs persist between iterations |
| **Parallel** | Yes (per branch) | Each branch gets `ctx.child()`; outputs merged back after all branches complete |
| **Scope** | Depends on policy | `shared()`: no child; every other policy: `ctx.child_filtered(...)` (pure isolation uses an empty `AllowOnly` filter — globals only) |
| **Dynamic** | Depends on policy | Selects a candidate subgraph, then runs it through the same policy-governed boundary as Scope |

## Nodes

Nodes are the vertices of the graph. Each node has a unique ID allocated.

```rust
pub enum Node {
    System(SystemNode),
    Decision(DecisionNode),
    Switch(SwitchNode),
    Parallel(ParallelNode),
    Loop(LoopNode),
    Scope(ScopeNode),
    Dynamic(DynamicNode),
}
```

Most builder methods return `&mut Self` for chaining. When a `NodeId` is needed (for example, to attach an error handler), `add_system_node` returns the ID directly.

## Edges

Edges define the connections between nodes. They are stored in a flat vector alongside the nodes.

```rust
pub enum Edge {
    Sequential(SequentialEdge),
    Conditional(ConditionalEdge),
    Parallel(ParallelEdge),
    LoopBack(LoopBackEdge),
    Error(ErrorEdge),
    Timeout(TimeoutEdge),
}
```

`SequentialEdge` connects one node to the next and is the primary mechanism for linear flow. The builder creates these automatically when chaining nodes.

`ErrorEdge` and `TimeoutEdge` define fallback paths from a system node to a handler subgraph.

`LoopBackEdge` connects the end of a loop body back to the loop node.

## Execution

The `GraphExecutor` traverses a graph starting from the entry node, executing each node and following edges to determine the next step.

```rust
pub struct GraphExecutor;

impl GraphExecutor {
    pub async fn execute(
        &self,
        graph: &Graph,
        ctx: &mut SystemContext<'_>,
        hooks: Option<&HooksAPI>,
        middleware: Option<&MiddlewareAPI>,
    ) -> Result<ExecutionResult, ExecutionError>;
}
```

When a system returns a value, the executor inserts it into the context's output storage keyed by `TypeId`. Downstream systems access it via `Out<T>`, which fetches from the same storage. If multiple systems return the same type, the last write wins. Outputs persist for the duration of graph execution.

After graph execution completes, the `ExecutionResult` contains the terminal system's output. Use `result.output::<T>()` to downcast:

```rust
let result = executor.execute(&graph, &mut ctx, None, None).await?;
let answer = result.output::<MyOutput>(); // Option<&MyOutput>
```

### Graph-Level Timeout

A total execution time limit can be set at two levels: on the `Graph` itself (via `with_max_duration`) or on the `GraphExecutor` (via `with_max_duration`). When the timeout elapses, the executor returns `ExecutionError::GraphTimeout` and fires `OnGraphFailure` hooks — unlike wrapping with `tokio::time::timeout` externally, which bypasses hooks and middleware.

**Graph-level timeout** — declared as part of the graph definition, travels with the graph. Use this when the agent author knows the intended time budget for their graph:

```rust
let mut graph = Graph::new();
graph
    .with_max_duration(Duration::from_secs(30))
    .add_system(step_one)
    .add_system(step_two);
```

**Executor-level timeout** — a fallback default applied by the runtime across all graphs it executes:

```rust
let executor = GraphExecutor::new()
    .with_max_duration(Duration::from_secs(60));

let result = executor.execute(&graph, &mut ctx, None, None).await;
// On timeout: Err(ExecutionError::GraphTimeout { elapsed, max })
```

**Precedence** — when both are set, the graph's `max_duration` wins. The executor's value is only used as a fallback when the graph does not declare one:

```rust
let effective_timeout = graph.max_duration().or(executor_max_duration);
```

**Scope graphs** — a scope node's embedded `Graph` can declare its own `max_duration`, which is enforced independently around that scope's execution. This lets authors give nested subgraphs their own time budgets without affecting the parent. When a scope graph times out, `OnScopeComplete` is **not** fired for that scope. The `ExecutionError::GraphTimeout` propagates up, and `OnGraphFailure` fires on the top-level (parent) graph — there is no scope-specific failure hook.

**`Graph::append`** — when a graph with `max_duration` is appended into another, the appended graph's timeout is discarded. The receiving graph's timeout policy takes precedence. A warning is logged if the appended graph had a timeout set but the receiver did not.

#### Cancel Safety

Graph-level timeout is a **hard abort**. When it fires mid-execution, tokio drops the currently-running system's future at its next `.await` point. Concretely:

- The cancelled system's Rust `Drop` impls run (RAII cleanup works), but any `async` code after the cancellation point does not execute.
- Writes already made to `SystemContext` via `ResMut<T>` persist; writes that would have happened after the cancellation are lost.
- External side effects (HTTP calls, DB writes) dispatched before the `.await` may have committed remotely but the response is never observed — the system cannot distinguish "never happened" from "happened but unacknowledged."
- The cancelled system's **error and timeout edges are not invoked** — graph-level timeout bypasses node-dispatch entirely.
- `OnSystemComplete` / `OnSystemError` hooks for the in-flight system do not fire. Only `OnGraphFailure` fires on the top-level graph, with `ExecutionError::GraphTimeout { elapsed, max }` — which does not identify which node was executing when the timeout fired.
- For scope graphs using `ContextPolicy::shared()`, the parent context may retain partial writes from systems that ran before the timeout fired.

This is distinct from **per-node timeout** (`SystemNodeBuilder::with_timeout` or `Graph::set_timeout(node_id, duration)`), which integrates with retry policies and timeout handler edges for structured recovery. Use per-node timeout when you need handler-based recovery; use graph-level timeout for hard time budgets.

Subgraph execution (branches, loop bodies, case handlers) is recursive with depth tracking. The default recursion limit is 64.

## Error Handling

Errors in graph execution fall into two categories with distinct handling semantics.

**Agentic errors** are anticipated failure modes within a system's domain — an LLM refusing a prompt, a tool returning an invalid result, a validation check failing. These are errors the agent is designed to reason about and recover from. Systems signal agentic errors by returning `Result<T, SystemError>` and are marked fallible by the `#[system]` macro (via `is_fallible() = true`). Error handler nodes are part of the agent's own graph and represent recovery logic the agent controls.

**Infrastructure errors** are failures outside the agent's responsibility — a missing resource, a network partition, a misconfigured context. These are not wired to error handler nodes because the agent cannot meaningfully recover from them within its graph. Instead, they propagate as `ExecutionError` from `executor.execute()`, where the agent implementer handles them directly.

This separation is enforced by the builder: `add_error_handler()` only auto-wires nodes where `is_fallible()` returns `true`. Infrastructure failures (e.g., `ParamError` from a missing resource) bypass error handler nodes entirely and escalate to the caller. Manual `System` implementations that can fail with agentic errors must override `is_fallible()` to return `true` for error handler wiring to apply.

### Error Edges

When a system node fails, the executor checks for an `ErrorEdge` from that node. If one exists, execution continues at the error handler subgraph. If none exists, the error propagates and execution stops.

```rust
// Per-node error handler:
let risky_id = graph.add_system_node(risky_operation);
graph.add_error_handler_for(risky_id, |g| {
    g.add_system(fallback_operation);
});

// Global error handler (auto-wires all fallible nodes without an existing error edge):
graph.add_error_handler(|g| {
    g.add_system(global_fallback);
});

// Closure-based error handler (no system definition needed):
graph.add_error_handler_fn(|error: &CaughtError| -> ErrorResponse {
    ErrorResponse { code: 500, message: error.message.to_string() }
});

// Closure-based handler for specific nodes:
graph.add_error_handler_fn_for([risky_id], |error: &CaughtError| -> String {
    format!("handled: {}", error.message)
});
```

The closure-based variants (`add_error_handler_fn`, `add_error_handler_fn_for`) are a convenience for trivial error mapping where defining a full `#[system]` function would be boilerplate. The closure receives `&CaughtError` and returns a value of type `T`, which is stored as the handler's output. The `SystemNodeBuilder` also exposes `on_error_fn` for inline use:

```rust
graph.system(risky_operation)
    .on_error_fn(|error: &CaughtError| -> ErrorResponse {
        ErrorResponse { code: 500, message: error.message.to_string() }
    })
    .done()
    .add_system(next_step);
```

### Timeout Handling

A system node can have a timeout set via `set_timeout`. The executor wraps the system call in `tokio::time::timeout`. If the timeout elapses, the executor checks for a `TimeoutEdge`. If one exists, execution continues at the timeout handler. If none exists, the executor returns `ExecutionError::Timeout`.

```rust
let slow_id = graph.add_system_node(slow_operation);
graph.set_timeout(slow_id, Duration::from_secs(5));
graph.add_timeout_handler(slow_id, |g| {
    g.add_system(timeout_fallback);
});
```

### Retry Policy

A system node can optionally have a retry policy. By default, no retry policy is set — a failed or timed-out system node immediately triggers its error or timeout edge. When a retry policy is configured, the executor retries the system up to `max_retries` additional times before giving up.

Two strategies are available:

- **Fixed** — constant delay between retries.
- **Exponential** — delay doubles each attempt (`2^attempt * initial_delay`), optionally capped by a maximum delay.

```rust
use std::time::Duration;
use polaris_graph::RetryPolicy;

// Fixed: retry up to 3 times with 100ms between attempts
graph
    .system(flaky_operation)
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .done();

// Exponential backoff: retry up to 5 times, starting at 50ms, capped at 2s
graph
    .system(network_call)
    .with_retry(
        RetryPolicy::exponential(5, Duration::from_millis(50))
            .with_max_delay(Duration::from_secs(2)),
    )
    .done();
```

Both errors and timeouts count as failed attempts. After all retries are exhausted, the final outcome is forwarded to the error or timeout edge as usual.

### ExecutionError Variants

| Variant | Cause |
|---------|-------|
| `EmptyGraph` | Graph has no entry point |
| `NodeNotFound(NodeId)` | Referenced node not in graph |
| `NoNextNode(NodeId)` | No sequential edge from node (terminal) |
| `MissingPredicate(NodeId)` | Decision/loop node missing its predicate |
| `MissingBranch { node, branch }` | Decision node missing true/false branch target |
| `MissingDiscriminator(NodeId)` | Switch node missing its discriminator |
| `NoMatchingCase { node, key }` | Switch: no case for key and no default |
| `DynamicCandidateNotFound { node, name, key }` | Dynamic: selector's key (and default) matched no candidate |
| `DynamicContractMismatch { node, name, diff }` | Dynamic (registry): the registry's contract does not fill the node's slot; `diff` names the divergence |
| `DynamicRegistryMissing { node, name }` | Dynamic (registry): no `SubgraphRegistry` in context |
| `DynamicRegistryBusy { node, name }` | Dynamic (registry): the `SubgraphRegistry` is write-locked during selection |
| `DynamicRegistryOutOfScope { node, name }` | Dynamic (registry): the `SubgraphRegistry` is hidden by an enclosing scope's policy |
| `SystemError(Arc<str>)` | System execution returned an error |
| `PredicateError(PredicateError)` | Predicate evaluation failed |
| `MaxIterationsExceeded { node, max }` | Loop exceeded iteration limit |
| `NoTerminationCondition(NodeId)` | Loop has neither predicate nor `max_iterations` |
| `LoopPredicateInputMissingOnEntry { node, name, output_type }` | Main-chain loop's termination predicate input is neither producible by any earlier node nor present in the context at run start (see [Loop](#loop)) |
| `Timeout { node, timeout }` | System execution exceeded timeout |
| `GraphTimeout { elapsed, max }` | Total graph execution exceeded `max_duration` |
| `RecursionLimitExceeded { depth, max }` | Nested control flow too deep (default: 64) |
| `MiddlewareError { middleware, message }` | A middleware layer failed |
| `InternalError(String)` | Framework invariant violated |
| `Unimplemented(&str)` | Feature not yet implemented |

### Error Context (ErrOut)

When a system fails and an error edge exists, the executor stores a `CaughtError` in the outputs before routing to the handler. Error handler systems read it via `ErrOut<CaughtError>`:

```rust
use polaris_graph::CaughtError;
use polaris_system::param::ErrOut;

#[system]
async fn handle_error(error: ErrOut<CaughtError>) -> RecoveryState {
    tracing::error!(
        "[{}] {} failed after {:?}: {}",
        error.node_id, error.system_name, error.duration, error.message
    );
    RecoveryState::default()
}
```

`CaughtError` contains: `message`, `system_name`, `node_id`, `duration`, and `kind` (`Execution` or `ParamResolution`).

## Hooks

The hook system provides extension points for observing and modifying graph execution at specific lifecycle events. Hooks are registered by plugins during the build phase via `HooksAPI` and invoked by the executor at runtime.

There are two kinds of hooks. **Observer hooks** are side-effect-only callbacks for logging, metrics, and tracing. **Provider hooks** inject resources into the `SystemContext` before a system executes, making them available to the system via `Res<T>`.

### Schedules

Each hook is registered against one or more schedule types. The executor invokes hooks for a given schedule at the corresponding point in graph traversal. All hooks receive a `&GraphEvent` and match on the relevant variant for typed access.

**Graph-level:** `OnGraphStart`, `OnGraphComplete`, `OnGraphFailure` — fired before execution begins, after it completes, and when it fails.

**System-level:** `OnSystemStart`, `OnSystemComplete`, `OnSystemError` — fired around each system node's execution.

**Decision:** `OnDecisionStart`, `OnDecisionComplete` — fired before a decision node evaluates its predicate and after a branch has executed.

**Switch:** `OnSwitchStart`, `OnSwitchComplete` — fired before a switch node evaluates its discriminator and after a case has executed.

**Loop:** `OnLoopStart`, `OnLoopIteration`, `OnLoopEnd` — fired before the loop begins, at the start of each iteration, and after the loop completes.

**Parallel:** `OnParallelStart`, `OnParallelComplete` — fired before parallel branches start and after all branches complete.

**Scope:** `OnScopeStart`, `OnScopeComplete` — fired before scope entry and after scope completion. Includes `mode` and inner node count.

**Dynamic:** `OnDynamicStart`, `OnDynamicComplete` — fired before a dynamic node selects its candidate and after the chosen candidate completes. `OnDynamicComplete` includes the `selected` candidate key.

When multiple hooks are registered for the same schedule, they execute in registration order, and each hook sees context changes made by previous hooks.

### Custom System Schedules

System nodes can be tagged with custom schedules. When the executor runs a tagged system, it re-emits the standard system lifecycle events (`SystemStart`, `SystemComplete`, `SystemError`) on each custom schedule in addition to the built-in schedules. This allows hooks to subscribe to lifecycle events for specific systems rather than all systems.

Define a custom schedule by implementing `Schedule`, then attach it when adding the system to the graph:

```rust
struct OnToolCall;
impl Schedule for OnToolCall {}

graph.add_system((OnToolCall, execute_tool));
```

Multiple custom schedules can be attached using a tuple:

```rust
graph.add_system(((OnToolCall, OnExpensiveOp), execute_tool));
```

### DevToolsPlugin

`DevToolsPlugin` demonstrates provider hooks. It registers a hook on `OnSystemStart` that injects `SystemInfo` into the context before each system executes. Systems can then access the current node ID and system name via `Res<SystemInfo>`:

```rust
#[system]
async fn my_system(info: Res<SystemInfo>) {
    println!("Running node {:?}: {}", info.node_id(), info.system_name());
}
```

## Middleware

Middleware wraps execution units with custom logic that must span the unit's duration — for example, holding a tracing span open across a system's execution, which is impossible with two disconnected hook events.

Each middleware is registered against a target type that determines which execution unit it wraps. The handler receives typed `info` metadata, `&mut SystemContext`, and a `Next` value. Every handler **must** call `next.run(ctx)` exactly once — dropping `next` without invoking it produces an `ExecutionError::InternalError`.

### Standalone Registration

```rust
use polaris_graph::middleware::{MiddlewareAPI, info::SystemInfo};

let mw = MiddlewareAPI::new();
mw.register_system("timer", |info: SystemInfo, ctx, next| {
    Box::pin(async move {
        let start = std::time::Instant::now();
        let result = next.run(ctx).await;
        tracing::info!("{}: {:?}", info.node_name, start.elapsed());
        result
    })
});

// Pass to the executor:
executor.execute(&graph, &mut ctx, None, Some(&mw)).await?;
```

### Plugin Registration

In a plugin, register middleware via `MiddlewareAPI` during `build()`:

```rust
impl Plugin for TracingPlugin {
    fn build(&self, server: &mut Server) {
        let mw = server.api::<MiddlewareAPI>()
            .expect("GraphPlugin must be added first");

        mw.register_system("tracing::system", |info: SystemInfo, ctx, next| {
            Box::pin(async move {
                let span = tracing::info_span!("system", name = info.node_name);
                let _guard = span.enter();
                next.run(ctx).await
            })
        });

        mw.register_graph("tracing::graph", |info: GraphInfo, ctx, next| {
            Box::pin(async move {
                let span = tracing::info_span!("graph", nodes = info.node_count);
                let _guard = span.enter();
                next.run(ctx).await
            })
        });
    }
}
```

### Targets

| Target | Info type | Scope |
|--------|-----------|-------|
| `GraphExecution` | `GraphInfo` | Entire graph run |
| `System` | `SystemInfo` | Single system node |
| `Decision` | `DecisionInfo` | Decision node evaluation |
| `Switch` | `SwitchInfo` | Switch node evaluation |
| `Loop` | `LoopInfo` | Entire loop node |
| `LoopIteration` | `LoopIterationInfo` | Single loop iteration |
| `Parallel` | `ParallelInfo` | Entire parallel node |
| `ParallelBranch` | `ParallelBranchInfo` | Single parallel branch |
| `Scope` | `ScopeInfo` | Scope node execution |
| `Dynamic` | `DynamicInfo` | Dynamic node selection and execution |

### Layer Ordering

Multiple middlewares on the same target form a chain. The last registered is outermost. Hooks execute inside all middleware layers, between the innermost middleware and the execution unit. If A is registered before B:

```text
B (enter) → A (enter) → hooks → execute → hooks → A (exit) → B (exit)
```
