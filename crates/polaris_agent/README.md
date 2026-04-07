# polaris_agent

Agent trait for defining reusable behavior patterns in Polaris.

## Overview

`polaris_agent` provides the `Agent` trait, which encapsulates agent behavior as a directed graph of systems. Concrete agent patterns (ReAct, ReWOO, etc.) implement this trait to define their execution flow.

- **`Agent` trait** - Define behavior by building a graph of systems
- **`SetupError`** - Error type for agent initialization failures
- **`to_graph()`** - Convenience method to build and return the agent's graph

## Example

```rust
use polaris_agent::Agent;
use polaris_graph::Graph;
use polaris_system::system;

struct SimpleAgent;

impl Agent for SimpleAgent {
    fn build(&self, graph: &mut Graph) {
        graph
            .add_system(reason)
            .add_system(decide)
            .add_system(respond);
    }

    fn name(&self) -> &'static str {
        "SimpleAgent"
    }
}

let graph = SimpleAgent.to_graph();
```

Agents are **builders**, not executors — they construct graphs that are executed by a separate executor component.

## License

Apache-2.0
