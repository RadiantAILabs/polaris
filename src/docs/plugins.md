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
| `DevToolsPlugin` | `HooksAPI`, `MiddlewareAPI`, `SystemInfo` injection | Global APIs / Hooks |
| `PersistencePlugin` | `PersistenceAPI` for resource serialization | Global API |
| `RandomPlugin` | Seedable RNG resource | Local |

# Feature-Gated Plugins

| Feature | Plugin | Purpose |
|---------|--------|---------|
| `graph-tracing` | `GraphTracingPlugin` | Tracing spans for graph execution |
| `models-tracing` | `ModelsTracingPlugin` | Tracing spans for model calls |
| `tools-tracing` | `ToolsTracingPlugin` | Tracing spans for tool invocations |
| `otel` | `OpenTelemetry` support | `OTel` exporter integration |

# Related

- [Plugin trait and lifecycle](crate::system) -- the `Plugin` trait definition
- [Graph hooks](crate::graph) -- hook system that tracing plugins use
- [Feature flags](crate#observability) -- enabling tracing features
