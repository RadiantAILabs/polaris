# polaris_sessions

Session management and orchestration for Polaris agents.

## Overview

`polaris_sessions` provides server-managed sessions that own live `SystemContext` instances in memory. Sessions handle context creation, graph execution, checkpointing, and persistence.

- **`SessionsAPI`** - Core API for creating, resuming, and running sessions
- **`SessionsPlugin`** - Plugin that registers the sessions infrastructure
- **`SessionStore`** - Trait for pluggable persistence backends
- **`InMemoryStore`** / **`FileStore`** - Built-in store implementations

## Quick Start

```rust
use std::sync::Arc;
use polaris_sessions::{SessionsAPI, SessionsPlugin, store::memory::InMemoryStore};
use polaris_core_plugins::{MinimalPlugins, PersistencePlugin};
use polaris_system::plugin::PluginGroup;
use polaris_system::server::Server;

let mut server = Server::new();
server
    .add_plugins(MinimalPlugins.build())
    .add_plugins(PersistencePlugin)
    .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));

server.run().await;

let sessions = server.api::<SessionsAPI>().unwrap();
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `file-store` | Yes | File-based session persistence |
| `http` | No | HTTP endpoints via `axum` |

## License

Apache-2.0
