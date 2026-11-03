# polaris_system

A lightweight ECS-inspired system framework for building AI agents in Rust.

## Overview

`polaris_system` provides the foundational primitives for the Polaris agentic framework:

- **Systems** - Pure async functions with dependency injection
- **Resources** - Long-lived shared state with hierarchical scoping
- **Outputs** - Ephemeral system return values
- **APIs** - Build-time capability registries for plugin orchestration
- **SystemParam** - Trait for injectable parameters (`Res<T>`, `ResMut<T>`, `Out<T>`)
- **Plugins** - Extension mechanism for modular functionality
- **Server** - Runtime orchestrator for plugins and execution contexts

## Quick Start

```rust
use polaris_system::param::{Res, ResMut, SystemContext};
use polaris_system::resource::LocalResource;
use polaris_system::system::System;
use polaris_system::system;

// Define a local resource (mutable, per-context)
struct Counter { value: i32 }
impl LocalResource for Counter {}

// Define a system with the #[system] macro
#[system]
async fn increment(mut counter: ResMut<Counter>) -> i32 {
    counter.value += 1;
    counter.value
}

// Use the system
let system = increment();
let ctx = SystemContext::new().with(Counter { value: 0 });
let result = system.run(&ctx).await.unwrap();
assert_eq!(result, 1);
```

## Core Concepts

### Resource Scoping

Resources are categorized by their scope and mutability:

| Trait | Scope | Mutability | Access Type |
|-------|-------|------------|-------------|
| `GlobalResource` | Server lifetime | Read-only | `Res<T>` |
| `LocalResource` | Per-context | Mutable | `Res<T>`, `ResMut<T>` |

```rust
use polaris_system::resource::{GlobalResource, LocalResource};

// Global: shared config, read-only across all agents
struct Config { max_tokens: usize }
impl GlobalResource for Config {}

// Local: per-agent state, mutable and isolated
struct Memory { history: Vec<String> }
impl LocalResource for Memory {}
```

### Storage Model

| Container | Purpose | Accessed By | Phase |
|-----------|---------|-------------|-------|
| **Resources** | Long-lived shared state | Systems via `Res<T>`, `ResMut<T>` | Execution |
| **Outputs** | Ephemeral system return values | Systems via `Out<T>` | Execution |
| **APIs** | Capability registries | Plugins via `server.api::<A>()` | Build/Ready |

### System Parameters

| Type | Purpose | Borrowing |
|------|---------|-----------|
| `Res<T>` | Read shared state | Immutable, concurrent |
| `ResMut<T>` | Modify local state | Exclusive (requires `LocalResource`) |
| `Out<T>` | Read previous system output | Immutable, concurrent |
| `Option<Res<T>>` | Read a global resource that may be absent (`None`) | Immutable, concurrent (requires `GlobalResource`) |

### The `#[system]` Macro

The `#[system]` attribute macro transforms async functions into `System` implementations:

```rust
use polaris_system::system;
use polaris_system::param::{Res, ResMut, Out};

#[system]
async fn my_system(
    config: Res<Config>,
    mut state: ResMut<State>,
    prev: Out<PreviousResult>,
) -> MyOutput {
    MyOutput::new()
}

// Creates: MySystemSystem struct + my_system() factory function
let system = my_system();
```

### Zero-Parameter Systems

For systems without parameters, use `into_system()` directly:

```rust
use polaris_system::system::IntoSystem;

async fn simple() -> Output {
    Output { value: 42 }
}

let system = simple.into_system();
```

## Design Philosophy

This crate follows ECS-inspired patterns from [Bevy](https://bevyengine.org/):

- **Pure functions** - Systems have no hidden state
- **Explicit dependencies** - All params declared in function signature
- **Hierarchical resources** - Global (read-only) vs Local (mutable) scoping
- **Async by default** - Built for LLM calls, tool invocations, I/O

Safety guarantees:
- `ResMut<T>` requires `T: LocalResource` (compile-time)
- Concurrent access conflicts detected at runtime via `RwLock`

## License

Apache-2.0
