//! Graph provenance for output writes.
//!
//! `polaris_system` stores every output entry under an opaque
//! [`ProducerKey`]: one id it compares, plus a provenance value it never
//! reads. The graph executor fills both in. The id is the writing node's
//! [`NodeId`], so "same producer" means "same node". The provenance is a
//! [`ProducerInfo`] describing where and when the write happened. Keeping
//! those graph details here, rather than as fields of the Layer 1 key, keeps
//! Layer 1 free of graph concepts — see `docs/taxonomy.md`.

use crate::hooks::RunId;
use crate::node::{NodeId, NodeName};
use crate::output_layers::WritePath;
use polaris_system::resource::ProducerKey;

/// Where and when a graph node wrote an output entry.
///
/// The executor attaches one to the [`ProducerKey`] of every output a system
/// node writes. Read it back from any key with [`ProducerInfo::of`], for
/// example from an [`OutAllEntry`](polaris_system::param::OutAllEntry) inside
/// a system that reads `OutAll<T>`.
///
/// Every field is for diagnostics. None of them affects which entry replaces
/// which, or which entry a `read_from` wire resolves to: those depend only on
/// the key's id, which is the node id.
///
/// # Examples
///
/// ```
/// use polaris_graph::ProducerInfo;
/// use polaris_system::param::OutAll;
/// use polaris_system::system;
///
/// #[derive(Debug)]
/// struct Score(u32);
///
/// /// Reports which node wrote each score, and on which loop iteration.
/// #[system]
/// async fn audit(scores: OutAll<Score>) -> Vec<String> {
///     scores
///         .iter()
///         .map(|entry| match ProducerInfo::of(entry.producer()) {
///             Some(info) => format!(
///                 "{} wrote {} on iteration {}",
///                 info.node_name(),
///                 entry.0,
///                 info.loop_iteration()
///             ),
///             None => format!("unknown producer wrote {}", entry.0),
///         })
///         .collect()
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerInfo {
    node: NodeId,
    node_name: NodeName,
    run_id: RunId,
    path: WritePath,
    loop_iteration: u64,
}

impl ProducerInfo {
    /// Builds the provenance for one write.
    pub(crate) fn new(
        node: NodeId,
        node_name: NodeName,
        run_id: RunId,
        path: WritePath,
        loop_iteration: u64,
    ) -> Self {
        Self {
            node,
            node_name,
            run_id,
            path,
            loop_iteration,
        }
    }

    /// Builds the key a write by this producer is stored under: the node id
    /// as the key's id, and `self` as its provenance.
    pub(crate) fn into_key(self) -> ProducerKey {
        ProducerKey::new(self.node.as_str()).with_provenance(self)
    }

    /// Returns the graph provenance attached to `key`, or `None` when the
    /// key was not written by a graph node (for example a caller-seeded
    /// output).
    #[must_use]
    pub fn of(key: &ProducerKey) -> Option<&Self> {
        key.provenance::<Self>()
    }

    /// The id of the node that wrote the entry.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The display name of the node that wrote the entry.
    #[must_use]
    pub fn node_name(&self) -> &NodeName {
        &self.node_name
    }

    /// The run the entry was written in.
    #[must_use]
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Where in the graph's structure the write happened, as a readable
    /// path of the scopes, dynamic slots, loop iterations, and parallel
    /// branches entered (for example `/scope:outer/loop:<id>#2`). Empty at
    /// the top level of a run. The format is for people, not for parsing,
    /// and may change.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// The iteration of the innermost enclosing loop the write happened on;
    /// `0` outside any loop. Use [`path`](Self::path) to see every enclosing
    /// loop's iteration.
    #[must_use]
    pub fn loop_iteration(&self) -> u64 {
        self.loop_iteration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_reads_back_the_attached_info_and_the_key_uses_the_node_id() {
        let node = NodeId::from_string("node_a");
        let info = ProducerInfo::new(
            node.clone(),
            NodeName::from("planner"),
            RunId::from_string("run_1"),
            WritePath::root().push_loop_iteration(&node, 2),
            2,
        );
        let key = info.clone().into_key();

        assert_eq!(key.id(), "node_a");
        assert_eq!(key, ProducerKey::new("node_a"));
        assert_eq!(ProducerInfo::of(&key), Some(&info));

        let read = ProducerInfo::of(&key).expect("attached above");
        assert_eq!(read.node(), &node);
        assert_eq!(read.node_name(), "planner");
        assert_eq!(read.run_id().as_str(), "run_1");
        assert_eq!(read.path(), format!("/loop:{node}#2"));
        assert_eq!(read.loop_iteration(), 2);
    }

    #[test]
    fn of_is_none_for_a_key_without_graph_provenance() {
        assert_eq!(ProducerInfo::of(&ProducerKey::new("seeded")), None);
    }
}
