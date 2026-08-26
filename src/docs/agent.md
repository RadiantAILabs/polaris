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

    /// Optional agent version, recorded on the turn span for observability.
    fn version(&self) -> Option<&str> {
        None
    }

    /// Optional description of the agent's purpose, recorded on the turn span.
    fn description(&self) -> Option<&str> {
        None
    }

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
- **`name`** -- stable identifier for agent type resolution; emitted in logs and
  traces and therefore should not contain PII
- **`version`** -- optional agent version; emitted in logs and traces and
  therefore should not contain PII. Defaults to `None`
- **`description`** -- optional human-readable summary of the agent's purpose;
  emitted in logs and traces and therefore should not contain PII. Defaults to
  `None`
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
# async fn init_loop() -> ReactState { ReactState { is_complete: false } }
# async fn act() -> LlmResponse { LlmResponse }
# async fn execute_tools() -> ReactState { ReactState { is_complete: false } }
# async fn finalize() -> ReactState { ReactState { is_complete: true } }
struct ReActAgent;

impl Agent for ReActAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(receive_user_input);
        // Produces the ReactState the loop's first termination check reads:
        // the predicate runs before the body, so the input must exist on entry.
        graph.add_system(init_loop);
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
