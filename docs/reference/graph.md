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

Before execution, a graph can be validated via `graph.validate()`, which checks that: the graph has a valid entry point; every node is reachable from it, including through error/timeout handler subgraphs; all edges reference valid nodes; no node declares more than one route for the same outcome — success, error, or timeout (see [Sequential](#sequential) and [Error Edges](#error-edges)); decision and switch nodes have the required predicates and branches; parallel nodes have branches; loop nodes have a body and termination condition or iteration limit; dynamic nodes have a non-empty, key-unique inline candidate set whose every candidate is structurally valid, flat (no nested *undeclared* `Scope` boundary), and admitted against the slot contract by the slot's `MatchingMode` relation; and every scope carrying a declared contract has an interior that matches it and a `ContextPolicy` that lets its requirements cross (see [Scope](#scope)). Advanced checks include verifying that loop termination predicates can read outputs produced within the loop body, including outputs a declared scope or dynamic contract guarantees, and warning about conflicting output types in parallel branches or about a `Substitutable` dynamic candidate whose extra output the surrounding graph also produces, since that extra is dropped at the boundary (see [Dynamic](#dynamic)).

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

1. **No false positives, at any phase.** A check MUST NOT reject a graph that could still execute successfully given facts a later phase would reveal. A rejection is legitimate only when failure is certain on every possible path — and "executing successfully" means realizing the structure the graph declares, not merely finishing without error. A declared edge, route, or candidate that the executor's own selection rule guarantees can never be taken (first-match shadowing, a duplicate key) is already a certain failure: no later phase can make it reachable, so the graph finishing anyway, through some other path, does not rescue it.
2. **Catch at the earliest sound phase.** Every invariant MUST be enforced at the earliest phase where Rule 1 holds for it, and MUST NOT be deferred to a later one. Prefer phase 1 whenever the invariant is expressible in the type system.
3. **Validate time sees only the fragment.** A phase-2 check MUST depend only on the graph's own structure — never on context contents, hooks, or the embedding graph. A graph is a fragment that can be embedded anywhere, so an environment-dependent rejection here violates Rule 1 by construction.
4. **Interfaces never under-claim.** Signature derivation MUST be pessimistic: a requirement may be omitted from `requires_outputs` only when production is guaranteed on every path. Over-claiming is sound (a composability tax, to be narrowed over time); under-claiming admits candidates that fail at runtime and is forbidden.
5. **Run start rejects only impossibility.** A phase-4 check MUST be optimistic: it may reject only what cannot succeed on *any* path, crediting every possible producer and everything already in the context. It is a fail-fast courtesy, never the guarantee — the authoritative check remains at execution time. The loop entry check ([Loop](#loop)) is the model instance.
6. **A check MUST NOT borrow another phase's crediting model.** Pessimistic interface analysis (phase 3) and optimistic run-start analysis (phase 4) answer different questions — "what does this fragment need from its surroundings?" versus "can this run possibly satisfy this need?" — and are documented as a deliberate split in `signature.rs`. Applying the pessimistic model at phase 4 falsely rejects runnable graphs; applying the optimistic model at phase 3 under-claims interfaces. Both violate the rules above.
7. **An advisory pass MUST NOT contradict the mandatory check it predicts.** Whatever an opt-in pass admits, the corresponding mandatory check MUST also admit. Concretely: `validate_resources` is phase-4 analysis performed ahead of time and credits no more than the mandatory run-start entry check does, so pre-flight success is never followed by a run-start refusal from the same analysis.
8. **Execution-time failures MUST be typed and routed, never panics.** Anticipated failures route through the graph's own error flow ([Error Handling](#error-handling)); infrastructure failures propagate as typed `ExecutionError` values through the normal failure path, firing `OnGraphFailure`, and leave the process running.

`GraphSignature::produces()` is the complete set of outputs a graph may produce on any reachable path. To check a built graph against an output contract that must hold after every successful path, use `GraphSignature::guaranteed_produces()` instead. The latter applies the pessimistic branch, guard, loop, scope, and dynamic crediting rules described below.

To place a new check, ask what it needs to know. If the answer includes "who embeds this graph," it belongs no earlier than phase 3. If it includes "what the caller seeded," no earlier than phase 4. If it includes "which branch was taken," it belongs at phase 5, expressed as a typed error.

## Adding System Nodes

There are five methods for adding a system node to a graph, each suited to a different use case.

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

**`system`** — adds the node and returns a `SystemNodeBuilder` for configuring custom schedules, error handling, timeouts, and retry policies. Call `.done()` to return to `&mut Graph` for continued chaining.

```rust
graph.system(risky_operation)
    .with_timeout(Duration::from_secs(30))
    .with_retry(RetryPolicy::fixed(3, Duration::from_millis(100)))
    .on_error(|h: &mut Graph| { h.add_system(fallback); })
    .on_timeout(|h: &mut Graph| { h.add_system(timeout_handler); })
    .done()
    .add_system(next_step);
```

The first three methods accept any type implementing `IntoSystemNode`, which includes bare async functions and `(schedule, system)` tuples for attaching custom hook schedules.

**`system_boxed`** — accepts any of the three boxed forms listed below and
returns the same `SystemNodeBuilder`. `TypedSystem<O>` is produced by
`erase(system)`; it hides the concrete system and `IntoSystem` marker types from
composition-helper signatures while retaining `O` for compile-time stage-slot
checks. See
[System — Typed Erasure for Composition](system.md#typed-erasure-for-composition)
for the handle itself and what it preserves.

```rust
fn add_stage(graph: &mut Graph, stage: TypedSystem<StageOutput>) {
    graph.system_boxed(stage)
        .with_schedules(OnStage)
        .done();
}
```

**`add_boxed_system`** — the same as `system_boxed`, but returns the new `NodeId`
instead of a builder. Use it when a composition helper needs the ID to wire
edges itself.

Both boxed methods accept three forms: a boxed concrete system (`Box<S>` where
`S: ErasedSystem`, which by the blanket impl covers every `S: System`), an
already-erased `BoxedSystem`, or a `TypedSystem<O>`. Note that
`Box<dyn System>` is not one of them — `System` has an associated `Output` type
and so is not object safe, which is why `BoxedSystem` is `Box<dyn ErasedSystem>`.

## Building Graphs from Runtime Data

Graph plans loaded from configuration can supply owned names, case keys, and a
choice of pre-registered predicates without leaking strings or spelling the
selected input type at the builder call site. Create the type-erased wrappers
at the typed registration boundary, then move them into the runtime builder:

| API | Runtime-built role |
|-----|--------------------|
| `NodeName` | Owned node names and switch keys; accepts `String`, `Arc<str>`, `Cow<'static, str>`, or a string literal |
| `BoxedPredicate::new::<T>(predicate)` | Erases a typed `Fn(&T) -> bool` while retaining `T` for signature derivation and validation |
| `BoxedDiscriminator::new::<T, _>(discriminator)` | Erases a typed discriminator whose result implements `Into<NodeName>` |
| `add_conditional_branch_boxed(name, predicate, true_path, false_path)` | Adds a decision using a selected `BoxedPredicate` |
| `add_loop_boxed(name, termination, body)` | Adds a predicate loop using a selected `BoxedPredicate` |
| `add_switch_boxed(name, discriminator, cases, default)` | Adds a switch whose case keys and name may be runtime strings |
| `SystemNodeBuilder::named(name)` | Gives a system node a runtime display name before `.done()` |

For example, both the node name and case key below can come from a parsed plan,
and the discriminator can come from a registry of boxed discriminators:

```rust
use polaris_graph::predicate::BoxedDiscriminator;
use polaris_graph::{Graph, NodeName};

struct Route {
    key: String,
}

async fn classify() -> Route { Route { key: "tool".into() } }
async fn call_tool() {}
async fn fallback() {}

let classifier_name = String::from("configured_classifier");
let tool_key: NodeName = String::from("tool").into();
let discriminator =
    BoxedDiscriminator::new::<Route, _>(|route| route.key.clone());

let mut graph = Graph::new();
graph.system(classify).named(classifier_name).done();
graph.add_switch_boxed(
    String::from("configured_route"),
    discriminator,
    vec![
        (tool_key, Box::new(|branch: &mut Graph| {
            branch.add_system(call_tool);
        }) as Box<dyn FnOnce(&mut Graph)>),
    ],
    Some(Box::new(|branch: &mut Graph| {
        branch.add_system(fallback);
    }) as Box<dyn FnOnce(&mut Graph)>),
);
```

Use `BoxedPredicate::new::<T>(...)` in the same way with
`add_conditional_branch_boxed` or `add_loop_boxed`. `Graph::validate()`,
signature derivation, and the loop-entry check inspect the erased wrapper's
retained input type exactly as they do for the generic builders.

## Construction Patterns

### Sequential

Systems are connected in order. Each `add_system` call appends a node and links it to the previous one.

```rust
graph
    .add_system(reason)
    .add_system(act)
    .add_system(respond);
```

**A node has at most one sequential successor.**

- **Why.** Traversal is first-match: the executor follows the first sequential edge whose source is the current node, so every later edge from that node is ignored and the work it declares is never wired into the path the executor walks. Interface derivation reads the same edges into a map keyed by source where the *last* edge wins, so the shape also leaves the graph advertising — and pre-flighted along — a chain execution does not take.
- **What you get.** `validate()` rejects it with `ValidationError::AmbiguousRoute` (`kind: RouteKind::Sequential`), naming the offending node, the successor followed, and the one whose edge is skipped, each by name as well as ID.
- **What to write instead.** Model the alternatives as a decision, switch, or parallel node, so the choice is explicit and every branch is wired.
- **Still allowed.** Fan-*in*, since the rule is one successor per node and not one predecessor; and repeated edges to the *same* successor, which name the node the executor already follows.

Two points about this rule look like exceptions and are not.

**The skipped node may still run.** Another edge can lead to it — a node further along the chain might name it too — so a graph containing a skipped edge can execute start to finish with every node running. That is not a reason to accept it. Validation asks whether the graph says what it means, not whether it happens to survive one particular run. An edge that nothing ever follows is a claim the graph makes and execution ignores; the graph running anyway only means the mistake cost nothing this time. This is exactly what [Rule 1](#phase-rules) means by "executing successfully": the skipped edge is a certain failure of the graph's own declared structure, independent of whether some other path happens to reach the same node.

**This is one instance of a general rule.** A node finishes one of three ways — it succeeded, it failed, or it timed out — and each way has its own kind of outgoing edge. Lookup is first-match for all three, so a node has at most one *effective* route per outcome, and `validate()` applies the same check to each. The error and timeout outcomes are covered under [Error Edges](#error-edges).

**The builder cannot produce the sequential shape.** Every `add_*` method chains from the previous node and then advances the chain point to the node it just added, and no public API wires a sequential edge between two nodes of your choosing or exposes the edge list for mutation. So the check guards an invariant the crate maintains internally, rather than constraining anything you can write today.

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

A parallel node forks execution across multiple subgraphs. Each branch receives its own child context and runs concurrently. If any branch fails, the remaining branches are cancelled, the error propagates, and no branch outputs reach the parent. The one exception is a raised [cancellation](#cancellation) signal: it never causes a running branch to be dropped. The child context inherits resource reads through the parent chain but starts with an **empty output store**, so a branch cannot read an `Out<T>` produced upstream of the parallel node. Put values needed across the fork in a resource on the parent context and read them in each branch with `Res<T>`.

The parallel node is both the entry and exit point. Once all branches complete successfully, every branch's outputs are merged in declaration order and execution continues from the parallel node's outgoing sequential edge. This includes outputs produced behind a nested scope boundary, outputs declared by a nested dynamic node's contract, and outputs from a handler path. When two or more branches produce the same output type, the store retains one entry per producer: `Out<T>` reads the newest entry, while `OutAll<T>` yields all of them oldest first in branch declaration order.

`validate()` reports `ValidationWarning::ConflictingParallelOutputs`, not an error, when it finds the same type from reachable `System` nodes in multiple branches. This check does not cover outputs hidden behind a nested scope, a dynamic node's contract, or a handler path. Validation also does not catch a branch `Out<T>` read that appears satisfied only by an output above the parallel node: signature derivation credits the parent chain to the branch region, but the branch's empty output store makes the read fail at run time with `ParamError::OutputNotFound`.

Because every branch runs, that merge is a *guarantee*, and signature derivation credits it: the union of the branches' own guaranteed production satisfies reads on the chain after the parallel. It does not satisfy reads *inside* a sibling branch — branches run against isolated child contexts and merge only at the join. A guarded system contributes only the intersection of its normal continuation and every error or timeout handler continuation: matching fallback output is guaranteed, while a handler that lacks an output withholds it.

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

Structural validation also requires the loop body to be able to produce the predicate input on a later iteration. A direct system output and an output a declared scope or dynamic contract guarantees both satisfy that check. This does not relax the first-check requirement: production in the body cannot seed the predicate before the body runs.

How conditional production is credited depends on the verification phase:

- **Composition signatures are pessimistic — but they credit what the runtime guarantees.** A parallel node runs *every* branch and merges the results; a decision credits the intersection of its true and false arms; a switch credits the intersection of all case arms and its default when present; and a scope merges its inner outputs back on exit whatever its context policy. Those guaranteed sets remove a predicate input from `requires_outputs`, recursively through nested control flow. A switch key that matches neither a case nor a default stops with `NoMatchingCase` before the downstream chain, so it is not an arm in the intersection. A scope with a [declared contract](#declared-scope-contracts) and a dynamic node with a slot contract are credited with their contract's `produces`: validation ensures every admitted interior or candidate supplies them. A loop body (zero iterations is legal) contributes nothing. A guarded system instead intersects its normal continuation with every error and timeout handler continuation, so recovery contributes only output all successful paths share. A subgraph that begins with a loop, or seeds it in only some branch arms, therefore advertises the seed, and slot contracts must supply it (see [Dynamic](#dynamic)).
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

For loops that should run a fixed number of times without a predicate, `add_loop_n` accepts only an iteration count. To give a predicate loop its own bound rather than relying on the executor default, call `set_loop_max_iterations` with the loop's `last_node()` ID after adding it. The predicate is evaluated first: when the cap is zero, an already-true predicate ends normally, while a false predicate returns `MaxIterationsExceeded { max: 0 }` without running the body.

#### Loop output retention

Each loop iteration writes its outputs into its own layer. Retention is how many **completed** iterations stay readable while the next one runs:

- The default is 1: an iteration can read what the previous iteration wrote, through `Out<T>` and `OutAll<T>`.
- A larger value keeps more earlier iterations readable.
- 0 keeps none: an iteration sees only what it wrote itself, plus what existed before the loop.

Set it per loop with `set_loop_retention(loop_id, n)`, or for one output type with `set_loop_retention_for_type::<T>(loop_id, n)`. The per-type value wins over the per-loop value for `T`, and 0 means the same on both. Call them after adding the loop, with its `last_node()` id.

```rust
graph.add_loop_n("refine", 5, |g| {
    g.add_system(draft).add_system(score);
});
let loop_id = graph.last_node().expect("the loop was just added");
// Keep three earlier scores readable, but only the current draft.
graph
    .set_loop_retention(loop_id.clone(), 3)
    .set_loop_retention_for_type::<Draft>(loop_id, 0);
```

The termination predicate runs before older layers are dropped, so it always sees the iteration that just finished. Whatever the retention, only the **newest** iteration's outputs leave the loop when it ends normally. If the loop ends with an error, a timeout, a cancellation, `MaxIterationsExceeded`, a predicate error, or its future is dropped, every iteration's outputs are discarded and the context is left as it was before the loop.

### Reading a Specific Producer (`read_from`)

By default `Out<T>` reads the newest `T`, whichever system wrote it. `read_from` pins one reader's read of `T` to one producer node:

```rust
let mut graph = Graph::new();
let planner = graph.system(plan).id();
graph.add_system(revise_plan);          // also returns Plan
graph.system(act).read_from::<Plan>(planner); // reads `plan`'s Plan, not `revise_plan`'s
```

There are two forms:

- `graph.system(reader).read_from::<T>(producer_id)` — on the node builder, when the producer already exists.
- `graph.read_from::<T>(reader_id, producer_id)` — by id, when the producer is added after the reader. A loop back-wire needs this form.

A second call for the same reader and `T` replaces the first. The wire is part of the graph, not of the system, so the same system can be reused with a different producer. `Graph::duplicate()` remaps wires to the copy's new ids.

**Loop back-wires.** When the reader and producer are in the same loop body and the producer runs later in the body, the reader reads the producer's value from an **earlier iteration**. The first iteration has no such value, so the read must be `Option<Out<T>>`. It also depends on [loop retention](#loop-output-retention): with retention 0 for `T`, no earlier iteration is readable and the read is always `None`.

```rust
graph.add_loop_n("loop", 3, |g| {
    let remember_id = g.system(remember).id(); // remember(previous: Option<Out<Counter>>)
    let advance_id = g.system(advance).id();   // advance(...) -> Counter
    g.read_from::<Counter>(remember_id, advance_id);
});
```

`validate()` rejects a wire when:

| Error | Cause |
|-------|-------|
| `ReadFromSelf` | The reader is its own producer. |
| `ReadFromProducerMissing` | The producer id is not in this graph (including an id from another `Graph`). |
| `ReadFromProducerNotASystem` | The producer is a control-flow node, not a system. |
| `ReadFromTypeMismatch` | The producer does not return `T`. |
| `ReadFromUnread` | The reader does not read `T` through `Out<T>` or `Option<Out<T>>` (for example only through `OutAll<T>` or `ErrOut<T>`). |
| `ReadFromNotGuaranteed` | The producer is not sure to have run before the reader on every path. A wire inside one loop body is the exception (a back-wire). |
| `ReadFromBackwardRequiresOptional` | A loop back-wire reads `Out<T>` instead of `Option<Out<T>>`. |

At run time the executor installs a reader's wires only while that node runs. A pinned read whose producer has no visible entry resolves like a missing output: `Option<Out<T>>` gets `None`, `Out<T>` fails. To see which node wrote each entry without pinning, use `OutAll<T>` with [`ProducerInfo`](context.md#producer-keys).

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

That merge-back is unconditional, so signature derivation credits the scope's own guaranteed inner production to reads on the enclosing chain — a scope may seed a downstream loop or `Out<T>` read without the parent supplying it. For an inner guarded system, that guarantee is the intersection of normal and handler continuations; a recovered failure contributes a matching fallback output, but not an output absent from any handler. A plain `add_scope` boundary stays opaque in the other direction: what the interior *needs* (its resource reads and its free `Out<T>` reads) never reaches the parent's signature, and its production does not enter the parent's `produces` axis. Outputs do not cross a non-shared boundary *inward* either, so an interior free `Out<T>` read is satisfiable only under `shared()`. That asymmetry is why a candidate containing an *undeclared* scope is inadmissible to a `Dynamic` slot (see [Dynamic](#dynamic)) — declaring the boundary's contract lifts it.

#### Declared scope contracts

`add_scope_with_contract` takes the same arguments as `add_scope` plus a declared `GraphSignature` — the interface the scope presents to the enclosing graph. Where a plain scope silently erases its interior's contract, a declared scope propagates it at the scope's chain position:

- the contract's `requires` surface into the parent's `requires` (resources have no guaranteed-upstream mechanism — derivation cannot see `ctx.insert` — so, like system resource reads, they always surface);
- its `requires_outputs` are classified against what is guaranteed upstream and surface otherwise, like a system's own `Out<T>` reads;
- its `produces` enter the parent's `produces` axis and are credited as *guaranteed* production for downstream reads — a declared scope may seed a downstream loop through its contract.

The contract is a **declaration, not a derived inner signature** — mirroring how a `Dynamic` slot's contract stands in for its candidates. Stopping derivation at the declaration keeps it non-recursive and gives `validate()` a stable interface to check drift against; derivation never peeks past a contract, so guaranteed interior production the contract omits is *not* credited.

**The boundary is sealed at runtime, not only at validate time.** On a non-`shared()` policy, when the scope's embedded graph finishes, only the output types named in the contract's `produces` merge back into the parent — every other output the interior wrote, whether or not it is one the interior *guarantees*, is dropped along with the child context that held it. A parent output of the same type as an undeclared interior output is left untouched: the interior's write to it never crosses the boundary, so there is nothing to overwrite. `shared()` is the one exception, by construction: it denotes no boundary at all, so the interior runs directly in the parent's own context and every write is visible immediately — a `shared()` scope's declared `produces` are still a sound credit, just not a privacy boundary, and combining the two only matters when the contract also needs a nonempty `requires_outputs`, which requires `shared()` regardless (see below). A plain `add_scope` is unaffected either way: it keeps merging every interior output unfiltered, contract or no contract, because it never carries one.

`validate()` holds both the interior and the boundary policy to the declaration:

- **Drift** (`ScopeContractViolated`, carrying the `SignatureDiff`): `requires` and `requires_outputs` — augmented with eager `forward`/`fork` inputs — must exactly match the contract on the interior's derived signature, the same exact-match relation `SubgraphRegistry::register` admission uses, because both axes surface verbatim into the parent's own signature. `produces` is checked differently: it is narrowed to the interior's *guaranteed* production first (`must_produce_output_types`), and then every **declared** produce must be present in that guaranteed set — a **subset** relation, not exact match. A declared produce backed only by some decision or switch arms, or by a loop body, is rejected — an output every arm produces is guaranteed, so it is admissible, but one only some arms produce is not — since the parent credits contract produces as guaranteed, and admitting a conditional declaration would let the signature claim more than the runtime truth. Guaranteed production the contract *omits*, conversely, is not drift — it is exactly the case the runtime boundary above exists to keep private, so the contract may name a genuine subset of what the interior guarantees.
- **Checkability** (`ScopeContractNested`): an interior nesting an *undeclared* `Scope` boundary has an incomplete derived signature, so no drift check against it is meaningful. Declare a contract on the nested scope or flatten the interior. Nested declared scopes and dynamic slots fold their contracts into the interior's signature and validate recursively.
- **Policy reachability** (`ScopeContractRequireBlocked`, `ScopeContractOutputsBlocked`): a contract `requires` entry the `ContextPolicy` blocks from crossing can never resolve at runtime — a read needs `shared()`, a crossing verb, or an un-excluded `share_rest()`; a write (`ResMut<T>` operates on the current scope only) needs a copy verb (`forward` / `fork`). And since free outputs never cross a non-shared boundary inward, a non-shared policy with a nonempty contract `requires_outputs` is statically unsatisfiable.

The policy copies every `forward::<T>()` and `fork::<T>()` resource at scope entry, even when the interior never accesses `T`. The contract must therefore include those inputs: add `require_read::<T>()` for a crossing-only type. If the interior already requires the type, retain its existing read/write entries; validation does not add a redundant read beside an existing write. This also lets an isolated no-op scope declare the source of an otherwise unused copy without triggering drift.

A factory requirement is distinct from a resource value, and `GraphSignature` currently cannot express it. Declared scopes therefore reject `forward_fresh` with `ScopeContractFactoryUnsupported`, even if the resource is listed in `requires`. Use `forward`/`fork` with a declared source resource, or plain `add_scope` for factory-based initialization. Factory availability for plain scopes remains checked at run start and execution.

Plain `add_scope` keeps today's opaque behavior — declared contracts are opt-in, and no existing derived signature changes.

An error or timeout handler wired *inside* the scope's own embedded graph is not a special case for the runtime filter: its outputs flow into the same child output store as the rest of the interior. A contract can draw on a guarded output only when the normal continuation and every handler continuation guarantee it; a handler-only output remains conditional and is rejected as drift. This is distinct from an error or timeout handler wired to the *scope node itself* in the enclosing graph: `Scope` is not a fallible node, so such a handler is never dispatched by the executor (only `System` nodes route to error/timeout edges); a scope failure always propagates as an `ExecutionError`.

```rust
// The scope advertises its interface instead of erasing it: the parent's
// derived signature carries Res<Corpus> and produces Summary at this position.
let contract = GraphSignature::new()
    .require_read::<Corpus>()
    .produce::<Summary>();
graph.add_scope_with_contract(
    "research",
    research_graph,
    ContextPolicy::new().share::<Corpus>(), // the declared read can cross
    contract,
);
```

### Dynamic

A dynamic node **selects** one of several pre-built candidate subgraphs at execution time and runs the chosen one through the same context boundary as [Scope](#scope). Use it to *pick* a subgraph by a runtime signal (where `Switch` only routes to branches wired at build time), or to *swap* the unit behind a slot between turns without rebuilding the parent graph.

Selection is driven by a **selector** — a `Fn(&SystemContext<'_>) -> Arc<str>` that names a candidate key from anything readable in the context. The candidate set comes from one of two sources:

| Source | Constructor | Candidate set | Mutable between turns |
|---|---|---|---|
| **Inline** | `add_dynamic(...)` | fixed at build time | no |
| **Registry** | `add_dynamic_registry(...)` | a `SubgraphRegistry` local resource | yes |

Every dynamic node declares a **contract** `GraphSignature` describing the slot's complete interface — the outputs/resources it requires from its surroundings and the outputs it produces. Each candidate's own signature is checked against this contract: inline candidates during `validate()`, registry candidates at `SubgraphRegistry::register` time. In addition, every output named by the contract must appear in the candidate's `guaranteed_produces()`, not merely its flat `produces()` set: the boundary promotes contract output as a promise to downstream readers, so output that only a branch, switch case, or loop iteration produces cannot fill the slot. When a Dynamic node is itself nested, its contract contributes all three axes to the enclosing signature: `requires` surface unconditionally, `requires_outputs` are classified at the boundary's chain position, and `produces` are credited downstream. **No graph is ever selected whose shape was not verified against the slot first**, so the parent — validated once against the fixed contract — stays sound no matter which candidate runs. This is composition preserving *soundness* without preserving *semantics*: the signature bounds the swappable space; which candidate runs inside it is a runtime choice.

A candidate is **sound to substitute** for the slot exactly when it demands no more than the slot guarantees and produces at least what the slot promises:

- `candidate.requires ⊆ slot.requires` — it reads no more than what's guaranteed present. Needing *fewer* inputs is fine; the extra available inputs just go unused.
- `candidate.requires_outputs ⊆ slot.requires_outputs` — the same logic for free `Out<T>` reads.
- `candidate.produces ⊇ slot.produces` — it exposes at least what downstream expects on its flat interface.
- `candidate.guaranteed_produces ⊇ slot.produces` — it deposits every promised output on every successful path. Producing *more* is admissible, but those undeclared outputs stop at the Dynamic boundary; see [Extra outputs stop at the dynamic boundary](#extra-outputs-stop-at-the-dynamic-boundary) below.

The rule is **contravariant in inputs, covariant in outputs**, and a slot chooses which of two relations it is checked under via `DynamicSlot::with_matching`:

| `MatchingMode` | `requires` | `requires_outputs` | `produces` | Predicate | Diff mode |
|---|---|---|---|---|---|
| `Exact` (default) | `candidate = slot` | `candidate = slot` | `candidate = slot` | `GraphSignature::compatible_with` | `diff` |
| `Substitutable` | `candidate ⊆ slot` | `candidate ⊆ slot` | `candidate ⊇ slot` | `GraphSignature::substitutable_for` | `substitutable_diff` |

`Exact` is the default and the stricter special case: a sound under-approximation that never admits an unsound candidate, only rejects some safe ones. It is the right posture when the candidates behind a slot are meant to be interchangeable *implementations* of one interface, where a divergence is more likely a mistake than an intent.

`Substitutable` enforces the full variance rule above. Opt into it when the candidates deliberately differ in what they consume — the motivating shape is an **escalation ladder**, where a cheap rung legitimately needs less context than the expensive rung the contract was written for; under `Exact` that rung is rejected, which is backwards for substitutability. `Exact` implies `Substitutable`, so opting in only ever widens the admissible set; existing slots are unaffected. On the `requires` axis, entries compare by `(type_id, mode)` under both relations — there is no write-subsumes-read relaxation in either direction. That is a **known limitation** rather than a property: a candidate declaring `Res<T>` is rejected by a slot guaranteeing `ResMut<T>`, even though read access is strictly less than write, so it is the escalation-ladder rejection one level down. It errs safe — a false rejection, never a false admission — and keeps the axis consistent with `satisfies`.

#### Extra outputs stop at the Dynamic boundary

Widening the relation is not free on the `produces` axis. A Dynamic boundary promotes only the slot contract's `produces`, so a candidate output the contract does not name is dropped before it can reach the parent — even when the parent already produced that type. Under `Exact` this cannot arise: the candidate's produced set equals the contract the parent was validated against.

Nothing downstream was ever validated against the extras, so the exposure is bounded to types the parent already produces. Where `validate()` can see that collision it warns, naming the candidate and the type:

```text
warning: dynamic node 'ladder' (…) candidate 'a' produces output type 'alloc::string::String',
which its contract does not name; the value is dropped at the boundary and
never reaches the parent
```

It is a warning, not an error: the graph still executes, but the candidate did work whose result is deliberately not exported. To clear it, drop the extra output from the candidate, name the type in the contract so it is promoted deliberately, or keep `Exact`. The check covers **inline** candidates only — a registry candidate is admitted after the parent was validated, so no equivalent check is possible there.

Where two signatures diverge, the diff reports the exact axes and types (returned as a `SignatureDiff`, and carried on every mismatch error), so a rejection reads as an edit rather than a puzzle. Each relation is paired with its own lockstep diff mode — the predicate holds exactly when the diff is empty — and the substitutability diff populates only the directions its relation forbids (`extra_requires`, `extra_requires_outputs`, `missing_produces`). `MatchingMode::admits` / `MatchingMode::diff` select the pair.

Distinct from substitution is **capability matching** — `GraphSignature::satisfies`, a third comparison relation used by consumer registries (e.g. the sessions [capability-contract registry](./sessions.md#capability-contracts)) to ask whether an agent *can do* what a named capability describes. It matches by subsumption: the slot's `requires` and `produces` must be **subsets** of the candidate's, and the candidate's free-output reads must not exceed the slot's. Note the `requires` direction is *inverted* relative to substitution — a capability slot names the inputs the capability's driver will feed the agent, so a candidate's extra requires are presumed environment-provided by its registrant's setup (conservative derivation reports self-inserted resources as required), not violations. `satisfies_diff` is its lockstep diff mode, reporting only the violating directions.

That inversion is why `satisfies` is **not** the relaxed relation a `Dynamic` slot selects, even though it agrees with `substitutable_for` on the other two axes. Subsumption is sound only because it trusts a registrant's claim about an environment the registrant controls. A `Dynamic` slot has no registrant: the *parent graph* is the environment, and it was validated against the slot contract alone, so a candidate requirement the contract never named has nothing to supply it and would dangle at runtime. The three relations are deliberately separate, each named for its soundness envelope.

The contract is also validated against the parent's *surroundings*, not only the candidates — the contract is the interface, so this holds even for a registry slot with no registered candidates. At `validate_resources` time every resource the contract `requires` must be reachable from the context the candidate will run against (`DynamicContractMissingResource` otherwise), and every free `Out<T>` in `requires_outputs` must be produced upstream (`DynamicContractMissingOutput`). Free outputs never cross a non-shared boundary inward, so a non-shared `ContextPolicy` combined with a nonempty `requires_outputs` is statically unsatisfiable (`DynamicContractOutputsBlocked`).

These surroundings checks are **unaffected by the matching mode**. `MatchingMode` governs candidate admission; it does not weaken the contract's own claim on the parent. Under `Substitutable` the contract's `requires` / `requires_outputs` become the *upper bound* of what any admissible candidate may consume — a candidate consuming the whole declared set is still admitted — so the parent must be able to satisfy that upper bound whichever candidate the selector picks. Relaxing the checks under `Substitutable` would admit a parent that cannot feed the maximal admissible candidate, which is exactly the runtime failure composition-time checking exists to prevent. A slot that wants a cheaper obligation on its parent should declare a smaller contract; `Substitutable` widens the candidate set, not the parent's duty. Finally, signature derivation is opaque across an *undeclared* nested scope: its interior resource reads and free `Out<T>` reads never reach the enclosing signature, so a candidate containing one is **rejected** — inline candidates during `validate()` (`DynamicCandidateNested`), registry candidates at `register()` (`RegistryError::NestedCandidate`). A scope carrying a [declared contract](#declared-scope-contracts) does not disqualify a candidate, and neither does a nested dynamic node: their contracts fold into the candidate's signature and validate their hidden interiors or candidates, so the boundary IO is fully accounted for.

The slot's fixed configuration — its `contract`, `MatchingMode` mode, boundary `ContextPolicy`, and optional default key — is bundled into a `DynamicSlot`, passed as one value so there are no positional holes at the call site:

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

// An escalation ladder: rungs deliberately differ in what they consume, so the
// slot matches by substitutability instead of exactly.
graph.add_dynamic(
    "ladder",
    pick_rung,
    [("cheap", cheap_rung), ("deep", deep_rung)],
    DynamicSlot::new(deep_rung_contract, ContextPolicy::shared())
        .with_matching(MatchingMode::Substitutable),
);
```

If the selector returns a key with no matching candidate, the node falls back to its configured default; with no default it fails with `ExecutionError::DynamicCandidateNotFound`. Registry *resolution* failures are distinct and unrecoverable — the default key resolves through the same registry — so they surface as their own typed errors rather than a candidate miss: no registry in context is `DynamicRegistryMissing`, a write-locked one is `DynamicRegistryBusy`, and one hidden by an enclosing scope's policy is `DynamicRegistryOutOfScope` (carrying crossing-verb guidance). For the registry source, the resolved `SubgraphRegistry`'s contract must itself be compatible with the node's slot contract — candidates are signature-checked against the *registry's* contract at `register()` time, so a registry whose contract diverges from the slot is refused at execution with `ExecutionError::DynamicContractMismatch` (which carries a `SignatureDiff` naming the divergence) rather than running a candidate the parent graph was never validated against. That check compares two *contracts* for identity, so it stays exact-set whatever mode either side admits its candidates under. A registry carries its own matching mode (`SubgraphRegistry::with_matching`, also defaulting to `Exact`), and on the registry path **the registry's mode is the only gate**: it admits at `register()`, and the executor re-checks contracts only, never modes — so `DynamicSlot::with_matching` records intent but gates nothing on a registry-backed node. Set them together. A divergence is silent in both directions: a stricter registry registers fewer candidates than its slot would have taken, and a laxer one runs candidates the slot's own mode would have refused. Neither direction is unsound — both modes admit only sound substitutes for the shared contract, so no candidate reads an input the parent does not supply — but a laxer registry does hand a slot that asked for `Exact` the dropped-extra-output case above, past the point where `validate()` could warn about it. The chosen candidate runs under the node's `ContextPolicy` exactly like a scope — see [Scope](#scope) for the boundary and output-merge semantics.

**Bounding runtime candidates.** Signature compatibility constrains a candidate's *interface*, not its *cost*: a candidate may contain arbitrarily many nodes. A candidate may not nest an undeclared `Scope` — signature derivation cannot see across that boundary, so it is rejected at build/registration time (see above). Declared-contract scopes and nested dynamic nodes are admissible because their contracts provide checked interfaces. Dynamic nesting is bounded by the executor's recursion limit (`GraphExecutor::max_recursion_depth`, default 64). Total node count and wall-clock time are still *not* bounded by default, so when candidates are generated or chosen from untrusted input (e.g. an LLM), set a [`max_duration`](https://docs.rs/polaris-ai/latest/polaris_ai/graph/struct.Graph.html#method.with_max_duration) on the candidate graphs; it wraps each candidate's execution in a timeout at the dynamic boundary.

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

`SequentialEdge` connects one node to the next and is the primary mechanism for linear flow. The builder creates these automatically when chaining nodes. A node may have at most one sequential successor — see [Sequential](#sequential) for what `validate()` rejects and why.

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

It is also distinct from [Cancellation](#cancellation) below, which is cooperative: the executor stops *between* nodes and never drops a running system's future, so none of the costs listed above apply to an interrupted run.

Subgraph execution (branches, loop bodies, case handlers) is recursive with depth tracking. The default recursion limit is 64.

### Cancellation

Where a graph-level timeout is a hard abort, cancellation is **cooperative**: the executor stops between nodes and never drops a running system's future. It is how a user's Ctrl-C, a UI stop button, or a host's shutdown ends a run in flight without the costs listed under [Cancel Safety](#cancel-safety).

One run carries at most one signal, split into two halves so that the authority to raise it and the ability to notice it are separate capabilities:

- `CancellationSource` — held by whoever drives the executor. `cancel()` latches and is idempotent. It is synchronous and does not await, but it takes a short internal lock (and `RunCancellation::is_cancelled` does too), so it is not lock-free and not safe to call from a raw signal handler. `Clone`, so a signal-listener task and a UI callback can share one.
- `RunCancellation` — an ordinary per-run `LocalResource`, read with `Res<RunCancellation>`. It has **no** `cancel`: a system cannot end the run it is inside.

Whoever owns the run inserts the observer into the run's root context before execution. A caller that inserts nothing gets today's behaviour — the run is not cancellable, and the check costs one `Option` test per node.

```rust
let source = CancellationSource::new();
let mut ctx = SystemContext::new();
ctx.insert(source.token());

// From any other task:
source.cancel();
```

To end a run on Ctrl-C, forward the signal to an ordinary task rather than calling `cancel()` from a raw signal handler:

```rust
let on_signal = source.clone();
tokio::spawn(async move {
    if tokio::signal::ctrl_c().await.is_ok() {
        on_signal.cancel();
    }
});
```

`SessionsAPI` inserts one on every turn, so a session-driven turn is always cancellable; see [Sessions — Interrupting a turn](sessions.md#interrupting-a-turn).

#### Where the executor checks

The executor snapshots the observer once at run start and evaluates it at two points inside the single dispatch loop:

1. **Before starting a node.** The node is not dispatched and is not counted.
2. **Before letting a node-attributable error propagate.** A system that returned `Err` because it noticed the signal ends the run as interrupted, not as a system failure.

Because every construct recurses through the same loop, the first point is observed at every boundary in the graph model:

| Boundary | Behaviour |
|---|---|
| Next node on a sequential chain | Declined; the run stops. |
| Entry of a decision, switch, or scope/dynamic interior | Declined; the enclosing construct's `Complete` hook does not fire. |
| Before each loop iteration | Declined *before* `OnLoopIteration` fires — an iteration that never starts is not reported as one that did. The termination predicate still runs first, so a loop that is already done ends normally. |
| Inside each parallel branch | Declined per branch; see below. |
| Entry of an error or timeout handler | The edge still *selects* the handler; the boundary check then declines to start it. So `on_error` / `on_timeout` are never entered under an interrupt. |
| Before the first node | `Interrupted { nodes_executed: 0 }`, after `OnGraphStart` has fired. |

Three places the executor owns *inside* a node also observe it:

- **Retry.** A raised signal stops the sequence: the attempt already running is never dropped, the next one does not start, and the inter-attempt backoff is raced rather than waited out.
- **Parallel branches.** An interrupted branch is returned to `try_join_all` as an ordinary completed branch, so no sibling's future is dropped and a consumer's branch middleware does not observe it as a failure. A branch that fails while the signal is raised — including a failure raised by the branch's own middleware after its body returned — is set aside the same way: the join waits for every branch, the child outputs are merged into the parent, and only then does the parallel node report the stop, counting the work of every branch. A structural fault in a branch is still reported as itself, after the siblings have finished. `OnParallelComplete` does not fire when the node reports a stop. Interrupt latency is therefore the slowest branch's — the price of never dropping a future. With no raise, a failing branch still aborts its siblings, as described under [Parallel Execution](#parallel-execution).
- **Timeouts.** Unchanged. A graph-level timeout still returns `GraphTimeout` with its dropped-future semantics; a per-node timeout still wraps one attempt.

#### The outcome

```rust
ExecutionError::Interrupted { run_id, node, nodes_executed, elapsed }
```

Reported through the normal failure path, so `OnGraphFailure` fires with it; there is no new hook schedule and `ExecutionResult` is unchanged. `node` is where the stop was observed — the node declined, or the node whose error it superseded; when a branch of a parallel node stopped, it is the parallel node's own id, because the innermost branch boundary is not recoverable once branches are joined. `nodes_executed` counts as `ExecutionResult::nodes_executed` counts: a declined node is excluded, a failed one is included.

Two rules keep the outcome honest:

- **A completed run stays completed.** If the signal is raised while the terminal node runs, the run finishes and returns its `ExecutionResult`. There is nothing left to decline to start, and reporting a turn that did all its work as interrupted would discard its output. A terminal parallel node follows the same rule: it completes, `OnParallelComplete` fires, and the run succeeds, unless a branch declined a node or failed under the raised signal. If a successor follows the parallel node, the stop is reported at that successor.
- **An interrupt never masks a defect.** It supersedes exactly a system failure, a per-node timeout, a middleware failure, and an already-propagating interrupt. Every other variant — `NodeNotFound`, `RecursionLimitExceeded`, `MaxIterationsExceeded`, `MissingPredicate`, the scope and signature faults, and `GraphTimeout` — propagates unchanged. A developer who happens to press Ctrl-C at the wrong moment is still told their graph is malformed.

#### Inside a system

Granularity is the system's choice, and both choices are safe — the executor stores exactly what a system returns, never a partial result.

```rust
#[system]
async fn act(cancel: Res<RunCancellation>, llm: Res<AgentLlm>) -> Result<LlmResponse, SystemError> {
    let Some(response) = cancel.run_until_cancelled(stream_and_collect(&llm)).await else {
        return Err(SystemError::ExecutionError("interrupted".into()));
    };
    Ok(response)
}
```

`run_until_cancelled` returning `None` means the work future was dropped at its own await point — the ordinary cost of `select!`, accepted by the system author who chose this form. The returned `Err` is diagnostic only: the boundary check reports `Interrupted` whatever the message says, and the error edge is not followed. A system that never checks runs to completion and the run stops at the next boundary, so **interrupt latency is bounded by the slowest node that ignores the signal**.

Two costs this carries, stated rather than hidden:

- **`requires` grows.** A `Res<RunCancellation>` read lands in the graph's derived `GraphSignature`, so a registered contract or a dynamic slot over that graph must carry the type. That is the honest interface of a cancellable graph, and the same cost `UserIO` pays.
- **Isolated scopes need `share`.** Inside a `ContextPolicy::new()` scope or dynamic candidate, the in-node read fails with `ResourceOutOfScope` unless the policy declares `.share::<RunCancellation>()` — the same rule `SessionInfo` follows. The *between-node* stop needs nothing: the executor holds the token from the root and checks it inside every boundary.

What an interrupted run leaves behind: outputs produced on the chain the interrupt stopped are present, and `ResMut` writes are committed. Outputs produced inside a scope or dynamic interior the interrupt unwound through are discarded, as they are for any propagating error. Parallel branch outputs are preserved, per the merge rule above.

## Error Handling

Errors in graph execution fall into two categories with distinct handling semantics.

**Agentic errors** are anticipated failure modes within a system's domain — an LLM refusing a prompt, a tool returning an invalid result, a validation check failing. These are errors the agent is designed to reason about and recover from. Systems signal agentic errors by returning `Result<T, SystemError>` and are marked fallible by the `#[system]` macro (via `is_fallible() = true`). Error handler nodes are part of the agent's own graph and represent recovery logic the agent controls.

**Infrastructure errors** are failures outside the agent's responsibility — a missing resource, a network partition, a misconfigured context. These are not wired to error handler nodes because the agent cannot meaningfully recover from them within its graph. Instead, they propagate as `ExecutionError` from `executor.execute()`, where the agent implementer handles them directly.

This separation is enforced by the builder: `add_error_handler()` only auto-wires nodes where `is_fallible()` returns `true`. Infrastructure failures (e.g., `ParamError` from a missing resource) bypass error handler nodes entirely and escalate to the caller. Manual `System` implementations that can fail with agentic errors must override `is_fallible()` to return `true` for error handler wiring to apply.

### Error Edges

When a system node fails, the executor checks for an `ErrorEdge` from that node. If one exists, execution continues at the error handler subgraph. If none exists, the error propagates and execution stops.

**A node has at most one error handler, and at most one timeout handler.** That lookup is first-match too, so a second handler wired to the same node is never reached — the recovery logic you wrote would simply not run, which is the worst place for a silent failure. `validate()` rejects it as `ValidationError::AmbiguousRoute` with `kind: RouteKind::Error` or `RouteKind::Timeout`.

**Order matters when you mix specific and global handlers.** `add_error_handler` fills only nodes that have *no* handler yet, so the working pattern is specific first, global second — as in the example below. The reverse order is what `validate()` catches: the global call already gave the node a handler, so a specific one added afterwards is dead. There is no override; to change a node's handler, wire that node's handler before the global call.

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

// Closure-based handler for specific nodes — a different node than `risky_id`
// above, which already has its own specific handler from `add_error_handler_for`:
let flaky_id = graph.add_system_node(flaky_operation);
graph.add_error_handler_fn_for([flaky_id], |error: &CaughtError| -> String {
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

Pre-boxed and typed-erased systems attach the same schedule markers through the
node builder:

```rust
graph.system_boxed(erase(execute_tool))
    .with_schedules((OnToolCall, OnExpensiveOp))
    .done();
```

`with_schedules` **accumulates** rather than replaces. Markers already attached
— by the tuple form or by an earlier `with_schedules` call — are preserved, and
a marker already present is not added twice:

```rust
graph.system((OnAudit, execute_tool))
    .with_schedules(OnToolCall)
    .done();
// node schedules: [OnAudit, OnToolCall]
```

This is what lets a reusable composition helper stamp its own role marker onto a
caller-supplied stage without silently dropping the graph author's observability
markers.

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
                let span = tracing::info_span!("system", name = %info.node_name);
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
