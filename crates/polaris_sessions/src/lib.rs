//! Session management and orchestration for Polaris agents.
//!
//! This crate provides server-managed sessions that own live
//! [`SystemContext`](polaris_system::param::SystemContext) instances in memory.
//! Sessions handle context creation, graph execution, checkpointing, and
//! persistence through the [`SessionsAPI`].
//!
//! # Quick Start
//!
//! ```
//! # use std::sync::Arc;
//! # use polaris_system::server::Server;
//! # use polaris_system::plugin::PluginGroup;
//! # use polaris_core_plugins::{MinimalPlugins, PersistencePlugin};
//! use polaris_sessions::{
//!     SessionsAPI, SessionsPlugin, SessionId,
//!     store::memory::InMemoryStore,
//! };
//!
//! # let mut server = Server::new();
//! server
//!     .add_plugins(MinimalPlugins.build())
//!     .add_plugins(PersistencePlugin)
//!     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
//! server.run();
//!
//! let sessions = server.api::<SessionsAPI>().unwrap();
//! ```

pub mod api;
pub mod error;
pub mod info;
pub mod store;

pub use api::{SessionsAPI, SessionsPlugin};
pub use error::SessionError;
pub use info::SessionInfo;
pub use store::memory::InMemoryStore;
pub use store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore};

#[cfg(feature = "file-store")]
pub use store::file::FileStore;

/// Common re-exports for convenient use.
pub mod prelude {
    pub use polaris_system::system::BoxFuture;

    pub use crate::api::{SessionsAPI, SessionsPlugin};
    pub use crate::error::SessionError;
    pub use crate::info::SessionInfo;
    pub use crate::store::memory::InMemoryStore;
    pub use crate::store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore};

    #[cfg(feature = "file-store")]
    pub use crate::store::file::FileStore;
}
