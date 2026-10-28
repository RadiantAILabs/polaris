//! Executor-side plumbing for output layers.
//!
//! An output write's identity (the
//! [`ProducerKey`](polaris_system::resource::ProducerKey) stored alongside it)
//! is the writing node's id. On top of that, the executor records a
//! *diagnostic* breadcrumb of where in the graph's structure the write
//! happened (scope nesting, loop iteration, parallel branch), carried in the
//! key's [`ProducerInfo`](crate::ProducerInfo) provenance so two writes from
//! the same node in different structural positions are still
//! distinguishable in traces, without becoming part of any replace decision
//! (key equality compares the id only).
//!
//! # Why loop retention uses output layers, not a child context
//!
//! A loop iteration cannot run its body in a synthetic child
//! [`SystemContext`](polaris_system::param::SystemContext): a nested scope's
//! `fork`/`forward` crossing resolves its source resource from the
//! *immediate* parent only, and a synthetic child would become that parent
//! instead of the loop's real enclosing context, silently breaking every such
//! crossing inside any loop body.
//!
//! So isolation lives one level down instead: in `Outputs`' own layer stack,
//! which the body still reads and writes through the very same `ctx` —
//! resources never see it at all. [`GraphExecutor::execute_loop`](crate::executor::GraphExecutor::execute_loop)
//! opens an [`OutputLayerScope`](polaris_system::param::OutputLayerScope),
//! pushes one layer per iteration, retains each type's requested number of
//! most-recent layers, and promotes only the newest one at loop exit. An
//! error, an interrupt, or the run's future being dropped discards the entire
//! loop-owned layer region, because the scope does that when it drops.

use crate::node::NodeId;
use std::fmt;
use std::sync::Arc;

/// A cheaply-clonable, append-only breadcrumb of graph structure, threaded
/// through nested loop/scope/dynamic/parallel execution purely for
/// diagnostics — see the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WritePath(Arc<str>);

impl WritePath {
    /// The empty path, used at the top of a run before any boundary has been
    /// entered.
    pub(crate) fn root() -> Self {
        Self(Arc::from(""))
    }

    /// The path as a string; empty for [`root`](Self::root).
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a new path with `segment` appended after this one.
    #[must_use]
    fn push(&self, segment: impl fmt::Display) -> Self {
        Self(Arc::from(format!("{self}/{segment}")))
    }

    /// Appends a non-shared scope or dynamic boundary, named by `name`
    /// (diagnostic label, not a key — scope/dynamic boundaries don't carry a
    /// loop-style iteration counter).
    #[must_use]
    pub(crate) fn push_boundary(&self, kind: &str, name: impl fmt::Display) -> Self {
        self.push(format_args!("{kind}:{name}"))
    }

    /// Appends one loop iteration.
    #[must_use]
    pub(crate) fn push_loop_iteration(&self, node: &NodeId, iteration: u64) -> Self {
        self.push(format_args!("loop:{node}#{iteration}"))
    }

    /// Appends one parallel branch.
    #[must_use]
    pub(crate) fn push_parallel_branch(&self, node: &NodeId, branch_index: usize) -> Self {
        self.push(format_args!("par:{node}#{branch_index}"))
    }
}

impl Default for WritePath {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for WritePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path_is_empty() {
        assert_eq!(WritePath::root().as_str(), "");
    }

    #[test]
    fn nested_boundaries_compose_in_order() {
        let node = NodeId::from_string("loop_a");
        let path = WritePath::root()
            .push_boundary("scope", "outer")
            .push_loop_iteration(&node, 2)
            .push_parallel_branch(&node, 1);
        assert_eq!(
            path.as_str(),
            format!("/scope:outer/loop:{node}#2/par:{node}#1")
        );
    }

    #[test]
    fn distinct_paths_differ() {
        let a = WritePath::root().push_loop_iteration(&NodeId::from_string("n"), 0);
        let b = WritePath::root().push_loop_iteration(&NodeId::from_string("n"), 1);
        assert_ne!(a, b);
    }
}
