---
notion_page: https://www.notion.so/radiant-ai/API-327afe2e695d80469571dfdd84f99e98
title: API Primitive
---

# API Primitive

The `API` trait is a Layer 1 primitive that enables plugins to expose capabilities to other plugins during the build phase.

## API Trait

```rust
/// Marker trait for capability APIs.
pub trait API: Send + Sync + 'static {}

impl Server {
    /// Insert an API (typically called by the API-providing plugin).
    pub fn insert_api<A: API>(&mut self, api: A);

    /// Get a reference to an API.
    pub fn api<A: API>(&self) -> Option<&A>;

    /// Check if an API is available.
    pub fn contains_api<A: API>(&self) -> bool;
}
```

## Defining APIs

### AgentAPI

APIs are defined by plugins, and are inserted in the server at the build phase:

```rust
// Plugin that provides an API
pub struct MyAPIPlugin;

impl Plugin for MyAPIPlugin {
    fn build(&self, server: &mut Server) {
        server.insert_api(MyAPI::new());
    }
}
```

Other plugins may then declare the plugin providing the API as a dependency, and access the API from the server:

```rust
// Plugin that uses the API
pub struct ConsumerPlugin;

impl Plugin for ConsumerPlugin {
    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<MyAPIPlugin>()]
    }

    fn ready(&self, server: &mut Server) {
        server.api::<MyAPI>()
            .expect("MyAPI required")
            .register("my-item", item);
    }
}
```

## Interior Mutability

APIs that need registration should use interior mutability:

```rust
pub struct MyAPI {
    data: RwLock<HashMap<String, Value>>,
}

impl API for MyAPI {}

impl MyAPI {
    pub fn register(&self, key: &str, value: Value) {
        self.data.write().unwrap().insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.data.read().unwrap().get(key).cloned()
    }
}
```

This allows `server.api::<MyAPI>()` to return `&MyAPI` while multiple plugins call `register()` concurrently.
