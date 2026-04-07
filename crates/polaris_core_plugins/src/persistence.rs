//! Persistence for local resources.
//!
//! This module provides:
//!
//! - [`Storable`] — Metadata trait marking a resource as eligible for persistence.
//!   Storage keys must be unique within a plugin.
//!
//! - [`ResourceSerializer`] — Trait encapsulating typed save/load logic for a resource.
//!
//! - [`PersistenceAPI`] — Build-time registry where plugins may register local
//!   resources to be persisted.
//!
//! - [`PersistencePlugin`] — Plugin that inserts [`PersistenceAPI`] into the
//!   server.
//!
//! - [`PersistenceError`] — Error type for persistence operations.
//!
//! # Derive Macro
//!
//! The derive macro `#[derive(Storable)]` may be used to directly implement the [`Storable`] trait:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! # use polaris_core_plugins::persistence::Storable;
//!
//! #[derive(Serialize, Deserialize, Storable)]
//! #[storable(key = "ConversationMemory", schema_version = "2.0.0")]
//! struct ConversationMemory {
//!     messages: Vec<String>,
//! }
//! ```

use core::marker::PhantomData;
use parking_lot::RwLock;
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

// Re-export the derive macro.
pub use persistence_macros::Storable;

/// Metadata for a resource eligible for persistency.
///
/// Can be derived via `#[derive(Storable)]`:
///
/// ```
/// # use serde::{Serialize, Deserialize};
/// # use polaris_core_plugins::persistence::Storable;
///
/// #[derive(Serialize, Deserialize, Storable)]
/// #[storable(key = "ConversationMemory", schema_version = "2.0.0")]
/// struct ConversationMemory {
///     messages: Vec<String>,
/// }
/// ```
pub trait Storable: Send + Sync + 'static {
    /// Stable identifier for this resource.
    /// Must be unique within the registering plugin's namespace.
    fn storage_key() -> &'static str;

    /// Semantic version for the schema. Defaults to `"1.0.0"`.
    fn schema_version() -> &'static str {
        "1.0.0"
    }
}

/// Error type for persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Failed to access a resource from the context.
    #[error("resource access error: {0}")]
    ResourceAccess(String),
}

/// Encapsulates typed save/load logic for a single persisted resource.
///
/// Serializers are created internally by [`PersistenceAPI::register`] and store
/// the concrete type knowledge needed to interact with [`SystemContext`] using
/// typed methods.
pub trait ResourceSerializer: Send + Sync {
    /// The plugin that registered this resource.
    fn plugin_id(&self) -> &'static str;

    /// Stable storage key for the resource.
    fn storage_key(&self) -> &'static str;

    /// Schema version for the resource.
    fn schema_version(&self) -> &'static str;

    /// Serializes the resource from the context to JSON.
    ///
    /// Returns `Ok(None)` if the resource is not present in the local scope,
    /// `Ok(Some(json))` on success.
    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError>;

    /// Deserializes JSON and inserts the resource into the context.
    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError>;
}

/// Typed serializer created by [`PersistenceAPI::register`].
struct TypedSerializer<R> {
    plugin_id: &'static str,
    _marker: PhantomData<R>,
}

impl<R> ResourceSerializer for TypedSerializer<R>
where
    R: LocalResource + Storable + Serialize + DeserializeOwned,
{
    fn plugin_id(&self) -> &'static str {
        self.plugin_id
    }

    fn storage_key(&self) -> &'static str {
        R::storage_key()
    }

    fn schema_version(&self) -> &'static str {
        R::schema_version()
    }

    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError> {
        if !ctx.contains_local_resource::<R>() {
            return Ok(None);
        }
        let guard = ctx.get_resource::<R>().map_err(|err| {
            PersistenceError::ResourceAccess(format!("failed to read {}: {err}", R::storage_key()))
        })?;
        let value = serde_json::to_value(&*guard)?;
        Ok(Some(value))
    }

    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError> {
        let resource: R = serde_json::from_value(value)?;
        ctx.insert(resource);
        Ok(())
    }
}

/// Build-time registry for storable resources.
#[derive(Default)]
pub struct PersistenceAPI {
    serializers: RwLock<Vec<Arc<dyn ResourceSerializer>>>,
}

impl API for PersistenceAPI {}

impl PersistenceAPI {
    /// Creates a new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a local resource for persistence, namespaced by the `plugin_id`.
    ///
    /// # Type Parameters
    ///
    /// * `R` - The resource type to register. Must implement [`LocalResource`],
    ///   [`Storable`], [`Serialize`] and [`DeserializeOwned`].
    ///
    /// # Parameters
    ///
    /// * `plugin_id` - Static string identifier of the plugin registering this resource.
    ///   Used to namespace the storage key and avoid collisions between plugins.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::resource::LocalResource;
    /// use polaris_system::plugin::{Plugin, PluginId, Version};
    /// use polaris_system::server::Server;
    /// use serde::{Serialize, Deserialize};
    /// use polaris_core_plugins::persistence::{Storable, PersistenceAPI, PersistencePlugin};
    ///
    /// #[derive(Serialize, Deserialize, Storable)]
    /// #[storable(key = "Memory", schema_version = "1.0.0")]
    /// struct Memory {
    ///     messages: Vec<String>,
    /// }
    ///
    /// impl LocalResource for Memory {}
    ///
    /// struct MyPlugin;
    ///
    /// impl Plugin for MyPlugin {
    ///     const ID: &'static str = "my_plugin";
    ///     const VERSION: Version = Version::new(1, 0, 0);
    ///
    ///     fn dependencies(&self) -> Vec<PluginId> {
    ///         vec![PluginId::of::<PersistencePlugin>()]
    ///     }
    ///
    ///     fn build(&self, server: &mut Server) {
    ///         server.insert_resource(Memory { messages: vec![] });
    ///     }
    ///
    ///     async fn ready(&self, server: &mut Server) {
    ///         // Register the resource type for persistence
    ///         let api = server.api::<PersistenceAPI>()
    ///             .expect("PersistenceAPI should be available");
    ///         api.register::<Memory>(Self::ID);
    ///     }
    /// }
    /// ```
    pub fn register<R>(&self, plugin_id: &'static str)
    where
        R: LocalResource + Storable + Serialize + DeserializeOwned,
    {
        let mut serializers = self.serializers.write();

        let duplicate = serializers
            .iter()
            .any(|v| v.plugin_id() == plugin_id && v.storage_key() == R::storage_key());
        assert!(
            !duplicate,
            "duplicate storage key '{}' registered by plugin '{plugin_id}'",
            R::storage_key(),
        );

        serializers.push(Arc::new(TypedSerializer::<R> {
            plugin_id,
            _marker: PhantomData,
        }));
    }

    /// Returns a snapshot of all registered serializers.
    pub fn serializers(&self) -> Vec<Arc<dyn ResourceSerializer>> {
        self.serializers.read().clone()
    }
}

/// Plugin that inserts [`PersistenceAPI`] into the server.
pub struct PersistencePlugin;

impl Plugin for PersistencePlugin {
    const ID: &'static str = "polaris::persistence";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        if !server.contains_api::<PersistenceAPI>() {
            server.insert_api(PersistenceAPI::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::server::Server;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Memory {
        messages: Vec<String>,
    }

    impl LocalResource for Memory {}

    impl Storable for Memory {
        fn storage_key() -> &'static str {
            "Memory"
        }
    }

    #[test]
    fn default_version() {
        assert_eq!(Memory::schema_version(), "1.0.0");
    }

    #[test]
    fn custom_version() {
        struct Notes;
        impl Storable for Notes {
            fn storage_key() -> &'static str {
                "Notes"
            }
            fn schema_version() -> &'static str {
                "2.0.0"
            }
        }

        assert_eq!(Notes::storage_key(), "Notes");
        assert_eq!(Notes::schema_version(), "2.0.0");
    }

    #[tokio::test]
    async fn round_trip_via_serializer() {
        let api = PersistenceAPI::new();
        api.register::<Memory>("test.plugin");

        let original = Memory {
            messages: vec!["hello".into(), "world".into()],
        };

        let mut server = Server::new();
        server.finish().await;
        let mut ctx = server.create_context();
        ctx.insert(original.clone());

        let serializers = api.serializers();
        let serializer = &serializers[0];

        // Save
        let json = serializer
            .save(&ctx)
            .expect("save should succeed")
            .expect("resource should be present");

        // Load into a fresh context
        let mut ctx2 = server.create_context();
        serializer
            .load(json, &mut ctx2)
            .expect("load should succeed");

        let restored = ctx2
            .get_resource::<Memory>()
            .expect("resource should exist");
        assert_eq!(&original, &*restored);
    }

    #[test]
    fn serializer_metadata_matches_registration() {
        let api = PersistenceAPI::new();
        api.register::<Memory>("test_plugin");

        let serializers = api.serializers();
        let serializer = &serializers[0];

        assert_eq!(serializer.plugin_id(), "test_plugin");
        assert_eq!(serializer.storage_key(), "Memory");
        assert_eq!(serializer.schema_version(), "1.0.0");
    }

    #[tokio::test]
    async fn save_missing_resource_returns_none() {
        let api = PersistenceAPI::new();
        api.register::<Memory>("test.plugin");

        let mut server = Server::new();
        server.finish().await;
        let ctx = server.create_context();

        let serializers = api.serializers();
        let serializer = &serializers[0];

        let result = serializer.save(&ctx).expect("save should not error");
        assert!(
            result.is_none(),
            "save should return None for missing resource"
        );
    }

    #[test]
    #[should_panic(expected = "duplicate storage key")]
    fn same_key_same_plugin_panics() {
        let api = PersistenceAPI::new();
        api.register::<Memory>("plugin.a");
        api.register::<Memory>("plugin.a");
    }

    #[test]
    fn same_key_different_plugins() {
        let api = PersistenceAPI::new();
        api.register::<Memory>("plugin.a");
        api.register::<Memory>("plugin.b");

        let serializers = api.serializers();
        assert_eq!(serializers.len(), 2);
        assert_eq!(serializers[0].plugin_id(), "plugin.a");
        assert_eq!(serializers[1].plugin_id(), "plugin.b");
    }
}
