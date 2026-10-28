# polaris_sessions

Session management and orchestration for Polaris agents.

## Overview

`polaris_sessions` provides server-managed sessions that own live `SystemContext` instances in memory. Sessions handle context creation, graph execution, checkpointing, and persistence.

- **`SessionsAPI`** - Core API for creating, resuming, and running sessions
- **`SessionsPlugin`** - Plugin that registers the sessions infrastructure
- **`SessionStore`** - Trait for pluggable persistence backends
- **`InMemoryStore`** / **`FileStore`** - Built-in store implementations. `FileStore` refuses to save a session past its file-size and checkpoint-count limits (raise them with `FileStore::with_load_limits`), so a stored session can always be resumed
- **`SessionsAPI::checkpoints()`** - Checkpoint capture, restore, pinning and retention. A failed restore leaves the session incoherent: turns, captures and `save_session` return `SessionError::RestoreFailed` until a restore succeeds

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

server.run().await.unwrap();

let sessions = server.api::<SessionsAPI>().unwrap();
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `file-store` | Yes | File-based session persistence |
| `sessions-http` | No | HTTP endpoints via `axum` |
| `typegen` | No | TypeScript bindings for `SessionMetadata` and the other model types |

## License

Apache-2.0
