Core infrastructure plugins for the Polaris runtime.

This module provides plugin groups and individual plugins that deliver
common capabilities: server metadata, time, tracing, persistence,
graph execution, and more.

# Plugin Groups

[`DefaultPlugins`](crate::plugins::DefaultPlugins) bundles the standard set of infrastructure plugins
suitable for most applications:

```no_run
use polaris_ai::plugins::DefaultPlugins;
use polaris_ai::system::server::Server;
use polaris_ai::system::plugin::PluginGroup;

let mut server = Server::new();
server.add_plugins(DefaultPlugins::new().build());
```

[`MinimalPlugins`](crate::plugins::MinimalPlugins) provides a lighter set for testing and constrained
environments.

Groups support customization through a builder:

```no_run
# use polaris_ai::plugins::{DefaultPlugins, MinimalPlugins};
# use polaris_ai::system::server::Server;
# use polaris_ai::system::plugin::PluginGroup;
# let mut server = Server::new();
server.add_plugins(
    DefaultPlugins::new()
        .build()
);
```

# Included Plugins

| Plugin | Provides | Scope |
|--------|----------|-------|
| `ServerInfoPlugin` | Server metadata resource | Global |
| `TimePlugin` | Wall-clock time resource | Global |
| `TracingPlugin` | Graph/system execution tracing | Hooks |
| `PersistencePlugin` | `PersistenceAPI` for resource serialization | Global API |

# Feature-Gated Observability

Most observability features extend [`TracingPlugin`](crate::plugins::TracingPlugin)
rather than exporting a separate plugin type.

| Feature | Public item to look for | Existing surface it changes | Runtime/API surface |
|---------|--------------------------|----------------------------|---------------------|
| `graph-tracing` | [`TracingPlugin`](crate::plugins::TracingPlugin) | No new plugin type; augments the existing tracing plugin | Registers graph middleware via [`crate::graph::MiddlewareAPI`] |
| `models-tracing` | [`TracingPlugin`](crate::plugins::TracingPlugin) | No new plugin type; augments the existing tracing plugin | Decorates the global [`crate::models::ModelRegistry`] |
| `tools-tracing` | [`TracingPlugin`](crate::plugins::TracingPlugin) | No new plugin type; augments the existing tracing plugin | Decorates the global [`crate::tools::ToolRegistry`] |
| `otel` | [`OpenTelemetryPlugin`](crate::plugins::OpenTelemetryPlugin) | Integrates with [`TracingPlugin`](crate::plugins::TracingPlugin) and [`TracingLayersApi`](crate::plugins::TracingLayersApi) | Pushes an OTLP export layer into the tracing subscriber |

If you are trying to answer “what does the `otel` feature export?”, the public
type is [`OpenTelemetryPlugin`](crate::plugins::OpenTelemetryPlugin) under
[`polaris_ai::plugins`](crate::plugins). This is separate from the crate-local
`otel` feature on `polaris_app`.

# Related

- [Plugin trait and lifecycle](crate::system) -- the `Plugin` trait definition
- [Graph hooks](crate::graph) -- hook system that tracing plugins use
- [Feature flags](crate#observability) -- enabling tracing features
