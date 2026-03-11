# polaris_graph

Graph execution primitives for Polaris (Layer 2).

## Overview

`polaris_graph` provides the core abstractions for defining agents as directed graphs of systems:

- **Graph** - Directed graph structure with builder API
- **Node** - Vertices representing computation units (systems, decisions, loops, etc.)
- **Edge** - Connections defining control flow between nodes

Agents are defined as data flowing through a computation graph, where graph structure determines behavior. The framework provides six node types for different control patterns and six edge types for different connection semantics.

See the [graph reference](../../docs/reference/graph.md) for detailed construction patterns and the full API.

## Core Abstractions

### Node Types

| Node | Purpose |
|------|---------|
| `SystemNode` | Executes a system function |
| `DecisionNode` | Routes flow based on predicate (binary branch) |
| `SwitchNode` | Routes flow based on discriminator (multi-way branch) |
| `ParallelNode` | Executes multiple paths concurrently |
| `LoopNode` | Repeats subgraph until termination condition |

### Edge Types

| Edge | Purpose |
|------|---------|
| `Sequential` | A → B, output flows to input |
| `Conditional` | A → B if predicate, else A → C |
| `Parallel` | A → [B, C, D] concurrently |
| `LoopBack` | Return to earlier node in graph |
| `Error` | Fallback path on failure |
| `Timeout` | Fallback path on timeout |

### Builder API

Graphs are constructed using a fluent builder API:

```rust
use polaris_graph::Graph;

let mut graph = Graph::new();

graph
    // Sequential systems
    .add_system(step_one)
    .add_system(step_two)
    // Conditional branching
    .add_conditional_branch(
        "should_continue",
        |g| g.add_system(continue_path),
        |g| g.add_system(fallback_path),
    )
    // Parallel execution
    .add_parallel(vec![
        |g| g.add_system(task_a),
        |g| g.add_system(task_b),
    ])
    // Loops with termination predicate
    .add_loop(|g| g.add_system(loop_body));
```

## Execution Semantics

The executor operates under two implicit rules:

1. **Implicit termination**: A node without an outgoing edge is a termination point. Execution ends when such a node completes.

2. **Subgraph return**: Control flow nodes (loops, branches, parallel) contain subgraphs. When a subgraph terminates, execution returns to the parent node and continues to its next sequential edge.

These rules apply uniformly whether in the root graph or nested within control flow constructs.

## Known Limitations

### Critical

| Limitation | Description | Workaround |
|------------|-------------|------------|
| **No cycle detection** | Graph validation does not detect unintended cycles via conditional/parallel edges | Manually verify graph structure does not contain cycles |

### Design Constraints

| Limitation | Description | Workaround |
|------------|-------------|------------|
| **Loop predicate priming** | Termination predicates require the output type to exist before the loop starts | Add an init system before the loop to produce the required output type |
| **No nested graphs** | Cannot compose graphs or add a `Graph` as a node | Inline all graph logic |
| **No output type validation** | Type mismatches between predicates and system outputs are only discovered at runtime | Ensure predicates use the exact output type of the preceding system |
| **Recursion depth limit** | Nested control flow structures (decisions, loops, parallels) are limited to 64 levels of recursion depth by default (configurable via `with_max_recursion_depth()`) | Flatten deeply nested control flow structures |
| **Subgraph exit constraints** | Subgraphs can only return to their parent node, not to arbitrary points in the parent graph | Structure control flow to avoid cross-subgraph jumps |
| **No explicit termination** | Subgraphs cannot terminate the entire parent graph execution | Use error propagation or state-based termination checks |

### Performance

| Limitation | Impact |
|------------|--------|
| **Sequential edge O(E) lookup** | Linear scan for each edge traversal; scales poorly for large graphs |

### Missing Features

- **Graph introspection**: No support for export, serialization, or path enumeration
- **Edge operations**: Cannot remove edges or query by target node

## License

Apache-2.0
