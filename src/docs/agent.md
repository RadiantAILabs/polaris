Agent trait for defining reusable behavior patterns.

An agent is a type that knows how to build a graph and optionally initialize
session resources. The [`Agent`](crate::agent::Agent) trait provides a minimal interface for
packaging any behavior pattern (`ReAct`, `ReWOO`, or custom) as a reusable unit.

# The Agent Trait

```no_run
# use polaris_ai::graph::Graph;
# use polaris_ai::system::param::SystemContext;
# use polaris_ai::agent::SetupError;
pub trait Agent: Send + Sync + 'static {
    /// Populate a graph with systems and control flow.
    fn build(&self, graph: &mut Graph);

    /// Stable, user-defined name for this agent type.
    fn name(&self) -> &'static str;

    /// Initialize session resources before the first turn.
    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        Ok(())
    }

    /// Create a new graph and pass it to `build`.
    fn to_graph(&self) -> Graph {
        let mut graph = Graph::new();
        self.build(&mut graph);
        graph
    }
}
```

- **`build`** -- called once when the agent is registered; populates the graph
- **`name`** -- stable identifier for agent type resolution
- **`setup`** -- called at session creation and resume; reads config from `&self`
  and the context to initialize per-session resources
- **`to_graph`** -- convenience that creates a `Graph` and delegates to `build`

# Example: `ReAct` Agent

```no_run
# use polaris_ai::agent::Agent;
# use polaris_ai::graph::Graph;
# struct ReactState { is_complete: bool }
# struct LlmResponse;
# impl LlmResponse { fn has_tool_calls(&self) -> bool { false } }
# async fn receive_user_input() {}
# async fn act() -> LlmResponse { LlmResponse }
# async fn execute_tools() {}
# async fn finalize() {}
struct ReActAgent;

impl Agent for ReActAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(receive_user_input);
        graph.add_loop::<ReactState, _, _>(
            "react_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(act);
                g.add_conditional_branch::<LlmResponse, _, _, _>(
                    "has_tool_calls",
                    |r| r.has_tool_calls(),
                    |tool| { tool.add_system(execute_tools); },
                    |done| { done.add_system(finalize); },
                );
            },
        );
    }

    fn name(&self) -> &'static str { "ReActAgent" }
}
```

# Packaging as Plugins

Agents are delivered as plugins that register the resources their systems
depend on. The plugin declares dependencies on other plugins (model
providers, tool registries) and registers the agent with the session layer.

# Related

- [Graph construction](crate::graph) -- the builder API used in `build()`
- [Sessions](crate::sessions) -- executing agents through the session lifecycle
- [Plugins](crate::system#plugins) -- the plugin system for distributing agents
