# Agent Trait

The `polaris_agent` crate defines a minimal abstraction for defining reusable agent behavior patterns. An agent is a type that knows how to build a graph and optionally initialize session resources.

## Overview

The `Agent` trait has two required methods (`build` and `name`) and two optional methods (`setup` and `to_graph`):

```rust
pub trait Agent: Send + Sync + 'static {
    fn build(&self, graph: &mut Graph);

    fn name(&self) -> &'static str;

    fn setup(&self, _ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        Ok(()) // default no-op
    }

    fn to_graph(&self) -> Graph {
        let mut graph = Graph::new();
        self.build(&mut graph);
        graph
    }
}
```

- **`build`** — Populates a `Graph` with systems and control flow. Called once when the agent is registered.
- **`name`** — Returns a stable, user-defined name for this agent type.
- **`setup`** — Initializes session resources before the first turn. Called automatically by the sessions layer during session creation and resume. The default is a no-op.
- **`to_graph`** — Convenience method that creates a new `Graph` and passes it to `build`.

See [graph.md](./graph.md) for further details on building graphs.

## Setup

`setup` receives `&self` and `&mut SystemContext`, so implementations can read configuration from the agent instance, from the context (injected by the caller's `init` closure), or both. The sessions layer calls `setup` automatically at two points:

1. **Session creation** — after the `init` closure runs
2. **Session resume** — after persisted resources are deserialized and the `init` closure runs

A separate `setup_session` method on `SessionsAPI` re-runs `setup` on a live session, which is useful after operations like rollback that replace the context and may lose non-persisted resources.

```rust
impl Agent for ReActAgent {
    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        let model_id = ctx
            .get_resource::<AgentConfig>()
            .map_err(SetupError::new)?
            .model_id
            .clone();
        let llm = ctx
            .get_resource::<ModelRegistry>()
            .map_err(SetupError::new)?
            .llm(&model_id)
            .map_err(SetupError::new)?;
        ctx.insert(AgentLlm(llm));
        Ok(())
    }

    // ...
}
```

`SetupError` is a newtype wrapping `Box<dyn Error + Send + Sync>`. Use `SetupError::new(err)` to wrap any error type.

## Usage

Defining an agent means implementing `build` to describe the desired graph topology. The following example implements a ReAct agent that loops through reasoning, tool use, and observation until the task is complete:

```rust
struct ReActAgent;

impl Agent for ReActAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(receive_user_input);
        graph.add_system(init_loop);

        graph.add_loop::<ReactState, _, _>(
            "react_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(act);
                g.add_conditional_branch::<LlmResponse, _, _, _>(
                    "has_tool_calls",
                    |response| response.has_tool_calls(),
                    |tool_branch| {
                        tool_branch.add_system(execute_tools);
                    },
                    |done_branch| {
                        done_branch.add_system(finalize);
                    },
                );
                g.add_error_handler(|h| {
                    h.add_system(recover);
                });
            },
        );
    }

    fn name(&self) -> &'static str { "ReActAgent" }
}
```

Executing an agent is the responsibility of the caller. The typical pattern is to build the graph, create a context from the server, and pass both to a `GraphExecutor`:

```rust
let mut server = Server::new();
server.add_plugins(DefaultPlugins);
server.add_plugins(MyModelPlugin);
server.finish();

let graph = ReActAgent.to_graph();
let mut ctx = server.create_context();

let executor = GraphExecutor::new();
executor.execute(&graph, &mut ctx, None).await?;
```

## Packaging as Plugins

To deliver a concrete agent implementation as a distributable unit, agents are packaged as a plugin. The plugin registers the resources the agent's systems depend on (LLM providers, tool registries, memory) and declares its dependencies on other plugins.

See [plugins.md](./plugins.md) for plugin structure and lifecycle, and `examples/` for a complete ReAct agent.
