#![cfg_attr(docsrs_dep, feature(doc_cfg))]

//! Session management and orchestration for Polaris agents.
//!
//! This crate provides server-managed sessions that own live
//! [`SystemContext`](polaris_system::param::SystemContext) instances in memory.
//! Sessions handle context creation, graph execution, checkpointing, and
//! persistence through the [`SessionsAPI`].
//!
//! Checkpoints — durable, identity-stable and bounded — live on the
//! [`SessionCheckpoints`] handle obtained from
//! [`SessionsAPI::checkpoints`]; see the [`checkpoint`] module.
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
//! # tokio_test::block_on(async {
//! server.run().await.unwrap();
//! # });
//!
//! let sessions = server.api::<SessionsAPI>().unwrap();
//! ```
//!
//! # See Also
//!
//! For the full framework guide, architecture overview, and integration patterns,
//! see the [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

pub mod api;
pub mod checkpoint;
pub mod error;
pub mod guard;
#[cfg(feature = "sessions-http")]
pub mod http;
pub mod info;
pub mod io;
pub mod models;
pub mod store;
mod uptime;

pub use api::{AgentTypeInfo, ContractName, ContractNameError, SessionsAPI, SessionsPlugin};
pub use checkpoint::{
    CheckpointId, CheckpointMeta, CheckpointOrigin, CheckpointRetention, RestoreFailureCause,
    SessionCheckpoints,
};
pub use error::{SessionError, WiringError};
pub use guard::SessionGuard;
#[cfg(feature = "sessions-http")]
pub use http::HttpPlugin;
pub use info::{SessionInfo, SessionMetadata, SessionStatus};
pub use io::{ChannelIOProvider, HttpIOProvider};
pub use models::{BucketGranularity, SessionUptimeResponse, Turn, TurnStatus, TurnSummary};
pub use store::memory::InMemoryStore;
pub use store::{AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore, TurnNumber};

#[cfg(feature = "file-store")]
pub use store::file::{FileStore, FileStoreLimits};

/// Common re-exports for convenient use.
pub mod prelude {
    pub use polaris_system::system::BoxFuture;

    pub use crate::api::{
        AgentTypeInfo, ContractName, ContractNameError, SessionsAPI, SessionsPlugin,
    };
    pub use crate::checkpoint::{
        CheckpointId, CheckpointMeta, CheckpointOrigin, CheckpointRetention, CheckpointStatus,
        RestoreFailureCause, RestoreOutcome, SessionCheckpoints,
    };
    pub use crate::error::{SessionError, WiringError};
    pub use crate::guard::SessionGuard;
    pub use crate::info::{SessionInfo, SessionMetadata, SessionStatus};
    pub use crate::io::ChannelIOProvider;
    pub use crate::store::memory::InMemoryStore;
    pub use crate::store::{
        AgentTypeId, ResourceEntry, SessionData, SessionId, SessionStore, TurnNumber,
    };

    #[cfg(feature = "file-store")]
    pub use crate::store::file::{FileStore, FileStoreLimits};
}
