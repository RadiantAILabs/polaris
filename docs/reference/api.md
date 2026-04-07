---
notion_page: https://www.notion.so/radiant-ai/API-327afe2e695d80469571dfdd84f99e98
title: API Primitive
---

# API Primitive

The `API` trait is a Layer 1 primitive that enables plugins to expose capabilities to other plugins during the build and ready phases. While resources are the mechanism for passing state to systems at execution time, APIs are the mechanism for plugin-to-plugin communication during server setup.

## API vs Resources

APIs and resources serve different roles:

| | API | Resource |
|---|---|---|
| **Purpose** | Plugin-to-plugin coordination | System execution state |
| **When accessed** | `build()` and `ready()` phases | System execution (via `Res<T>`, `ResMut<T>`) |
| **How accessed** | `server.api::<T>()` returns `&T` | `SystemParam::fetch()` from `SystemContext` |
| **Mutability** | Interior mutability (`RwLock`, `Arc`) | `Res<T>` (shared) or `ResMut<T>` (exclusive) |
| **Typical use** | Registries, route collection, configuration | LLM providers, memory, tool state |

Use an API when plugins need to register things with each other before execution begins. Use a resource when systems need access to state during graph execution.

## API Trait

```rust
pub trait API: Send + Sync + 'static {}
```

`API` is a marker trait with no required methods. Any type that is `Send + Sync + 'static` can implement it.

## Server Methods

```rust
impl Server {
    /// Insert an API (typically called by the providing plugin in `build()`).
    /// Returns the previous value if one existed.
    pub fn insert_api<A: API>(&mut self, api: A) -> Option<A>;

    /// Get an immutable reference to an API.
    pub fn api<A: API>(&self) -> Option<&A>;

    /// Check if an API is available.
    pub fn contains_api<A: API>(&self) -> bool;
}
```

APIs are stored in a type-erased map keyed by `TypeId`. `insert_api` requires `&mut self` (available during `build` and `ready`), while `api` and `contains_api` require only `&self`.

## Defining an API

### Simple API

The simplest API is a struct that implements the marker trait:

```rust
use polaris_system::api::API;

pub struct MyAPI {
    config: String,
}

impl API for MyAPI {}
```

### Interior Mutability

Since `server.api::<T>()` returns `&T`, APIs that need mutation after insertion must use interior mutability. The two common patterns are:

**`RwLock` for simple registries:**

```rust
use std::sync::Arc;
use parking_lot::RwLock;

pub struct PersistenceAPI {
    serializers: RwLock<Vec<Arc<dyn ResourceSerializer>>>,
}

impl API for PersistenceAPI {}

impl PersistenceAPI {
    pub fn register<R: Storable>(&self, plugin_id: &'static str) {
        self.serializers.write().push(/* ... */);
    }
}
```

Consumers call `register()` through `&self` because the `RwLock` provides interior mutability.

**`Arc`-wrapped inner state for complex APIs:**

```rust
use std::sync::Arc;

struct SessionsInner {
    sessions: RwLock<HashMap<SessionId, Arc<SessionState>>>,
    agents: RwLock<HashMap<AgentTypeId, Arc<dyn Agent>>>,
    // ...
}

#[derive(Clone)]
pub struct SessionsAPI {
    inner: Arc<SessionsInner>,
}

impl API for SessionsAPI {}
```

The `Arc` wrapper makes the API cheaply cloneable, which is useful when the API needs to be shared with background tasks or HTTP handlers.

## Providing an API

The plugin that owns an API inserts it during `build()`:

```rust
pub struct PersistencePlugin;

impl Plugin for PersistencePlugin {
    const ID: &'static str = "polaris::persistence";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_api(PersistenceAPI::new());
    }
}
```

If another plugin might have already inserted the API (e.g. in a plugin group), guard with `contains_api`:

```rust
fn build(&self, server: &mut Server) {
    if !server.contains_api::<PersistenceAPI>() {
        server.insert_api(PersistenceAPI::new());
    }
}
```

## Consuming an API

Consumer plugins declare a dependency on the provider plugin, then access the API during `build()` or `ready()`:

```rust
pub struct MyPlugin;

impl Plugin for MyPlugin {
    const ID: &'static str = "my_plugin";
    const VERSION: Version = Version::new(1, 0, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PersistencePlugin>()]
    }

    async fn ready(&self, server: &mut Server) {
        let api = server.api::<PersistenceAPI>()
            .expect("PersistenceAPI should be available");
        api.register::<Memory>(Self::ID);
    }
}
```

The dependency declaration ensures the provider's `build()` runs first, so the API is available by the time the consumer accesses it.

### Accessing During `build()` vs `ready()`

Both `build()` and `ready()` can call `server.api::<T>()`. The choice depends on timing:

- **`build()`**: The API is available if the provider plugin was added before this plugin. Use this when you need the API to configure your own resources.
- **`ready()`**: All plugins have been built. Use this for cross-plugin initialization that depends on the full server being configured.

```rust
// Accessing an API during build to register routes
fn build(&self, server: &mut Server) {
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }));

    server.api::<HttpRouter>()
        .expect("AppPlugin must be added first")
        .add_routes(router);
}
```

## Real-World Examples

### HttpRouter: Route Registration

`HttpRouter` is an API that collects axum route fragments from plugins during `build()`, then merges them into a single router when `AppPlugin` enters `ready()`.

**Provider** (`AppPlugin`):

```rust
fn build(&self, server: &mut Server) {
    server.insert_api(HttpRouter::new());
}

async fn ready(&self, server: &mut Server) {
    let router_api = server.api::<HttpRouter>()
        .expect("HttpRouter API must exist");
    let fragments = router_api.take_routes();
    let auth = router_api.take_auth();
    // merge fragments and start HTTP server...
}
```

**Consumer** (any plugin adding routes):

```rust
fn build(&self, server: &mut Server) {
    let router = Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions/:id", get(get_session));

    server.api::<HttpRouter>()
        .expect("AppPlugin must be added first")
        .add_routes(router);
}
```

### PersistenceAPI: Resource Serializer Registry

`PersistenceAPI` collects resource serializers so that session persistence knows which resources to save and restore.

**Provider** (`PersistencePlugin`):

```rust
fn build(&self, server: &mut Server) {
    if !server.contains_api::<PersistenceAPI>() {
        server.insert_api(PersistenceAPI::new());
    }
}
```

**Consumer** (any plugin with storable resources):

```rust
async fn ready(&self, server: &mut Server) {
    let api = server.api::<PersistenceAPI>()
        .expect("PersistenceAPI should be available");
    api.register::<Memory>(Self::ID);
}
```

## Summary

| Step | What to do | Where |
|------|-----------|-------|
| **Define** | Implement the `API` marker trait on a struct | Your API module |
| **Provide** | Call `server.insert_api(instance)` | Provider plugin's `build()` |
| **Consume** | Call `server.api::<T>()` to get `Option<&T>` | Consumer plugin's `build()` or `ready()` |
| **Mutate** | Use interior mutability (`RwLock`, `Arc`) | Within the API struct |
| **Depend** | Declare `PluginId::of::<ProviderPlugin>()` | Consumer plugin's `dependencies()` |
