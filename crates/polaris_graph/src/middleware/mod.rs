//! Graph execution middleware.
//!
//! Middleware allows custom logic to be layered around graph execution primitives.
//!
//! In most instances, you will find that the [`hooks`](super::hooks) system will be sufficient
//! for your needs. Middleware is needed when logic must span an execution unit. For example,
//! holding a tracing span guard open for the duration of a system's execution, which is
//! impossible with two disconnected point events.
//!
//! # Targets
//!
//! Each target type determines which execution unit a middleware wraps. Register
//! middleware using the corresponding `register_*` method on [`MiddlewareAPI`].
//!
//! | Target | Info type | Scope |
//! |--------|-----------|-------|
//! | [`GraphExecution`] | [`GraphInfo`](info::GraphInfo) | Entire graph run |
//! | [`System`] | [`SystemInfo`](info::SystemInfo) | Single system node |
//! | [`Loop`] | [`LoopInfo`](info::LoopInfo) | Entire loop node |
//! | [`Parallel`] | [`ParallelInfo`](info::ParallelInfo) | Entire parallel node |
//! | [`Decision`] | [`DecisionInfo`](info::DecisionInfo) | Decision node evaluation |
//! | [`Switch`] | [`SwitchInfo`](info::SwitchInfo) | Switch node evaluation |
//! | [`LoopIteration`] | [`LoopIterationInfo`](info::LoopIterationInfo) | Single loop iteration |
//! | [`ParallelBranch`] | [`ParallelBranchInfo`](info::ParallelBranchInfo) | Single parallel branch |
//! | [`Scope`] | [`ScopeInfo`](info::ScopeInfo) | Scope node execution |
//!
//! # Layer Ordering
//!
//! Multiple middlewares can be registered on the same target. Each layer wraps
//! the next, forming a chain that runs inward until it reaches the terminal,
//! consisting in the actual execution logic for the target (e.g. running a system node).
//! The last registered middleware is the outermost layer. Hooks execute after
//! all the middleware layers.
//!
//! If A is registered before B, execution flows:
//! B → A → hooks → terminal → hooks → A → B.
//!
//! # Handlers
//!
//! A handler (see [`MiddlewareHandler`]) receives three arguments:
//!
//! - `info` — typed metadata about the execution unit (e.g. [`SystemInfo`](info::SystemInfo)).
//! - `ctx` — exclusive `&mut` access to the [`SystemContext`](polaris_system::param::SystemContext).
//! - `next` — a [`Next`] value representing the rest of the chain. Call
//!   [`Next::run`] to continue inward. Code before the call runs on the way
//!   in, code after runs on the way out. Every handler must call
//!   [`Next::run`] exactly once; dropping `next` without invoking it
//!   (short-circuiting) is not permitted and will produce an
//!   [`ExecutionError::InternalError`](crate::executor::ExecutionError::InternalError).
//!
//! # Example
//!
//! ```
//! # use polaris_graph::middleware::{MiddlewareAPI, info::SystemInfo};
//! # let mw = MiddlewareAPI::new();
//! mw.register_system("logger", |info: SystemInfo, ctx, next| {
//!     Box::pin(async move {
//!         tracing::info!("before system: {}", info.node_name);
//!         let result = next.run(ctx).await;
//!         tracing::info!("after system: {}", info.node_name);
//!         result
//!     })
//! });
//! ```

mod api;
pub mod info;
pub use api::{MiddlewareAPI, MiddlewareError, MiddlewareHandler, Next};

// ─────────────────────────────────────────────────────────────────────────────
// Top-level targets
// ─────────────────────────────────────────────────────────────────────────────

/// Middleware target for the entire graph execution. See [`GraphInfo`](info::GraphInfo).
pub struct GraphExecution;

// ─────────────────────────────────────────────────────────────────────────────
// Node-level targets
// ─────────────────────────────────────────────────────────────────────────────

/// Middleware target for system node execution. See [`SystemInfo`](info::SystemInfo).
pub struct System;

/// Middleware target for the loop node as a whole, spanning every iteration
/// from entry to termination. For per-iteration middleware, see [`LoopIteration`].
///
/// See [`LoopInfo`](info::LoopInfo) for the metadata available to this middleware.
pub struct Loop;

/// Middleware target for the parallel node as a whole, spanning from the initial
/// fan-out through all branches to the final join. For per-branch middleware, see
/// [`ParallelBranch`].
///
/// See [`ParallelInfo`](info::ParallelInfo) for the metadata available to this middleware.
pub struct Parallel;

/// Middleware target for decision node evaluation. See [`DecisionInfo`](info::DecisionInfo).
pub struct Decision;

/// Middleware target for switch node evaluation. See [`SwitchInfo`](info::SwitchInfo).
pub struct Switch;

/// Middleware target for scope node execution. See [`ScopeInfo`](info::ScopeInfo).
pub struct Scope;

// ─────────────────────────────────────────────────────────────────────────────
// Sub-node-level targets
// ─────────────────────────────────────────────────────────────────────────────

/// Middleware target for a single loop iteration. See [`LoopIterationInfo`](info::LoopIterationInfo).
pub struct LoopIteration;

/// Middleware target for a single parallel branch. See [`ParallelBranchInfo`](info::ParallelBranchInfo).
pub struct ParallelBranch;
