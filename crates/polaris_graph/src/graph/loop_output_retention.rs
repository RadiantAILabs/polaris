//! Per-loop output retention configuration.
//!
//! A loop's retention setting lives in a side table keyed by [`NodeId`]
//! rather than as fields on [`LoopNode`](crate::node::LoopNode). The loop's
//! id is already how [`Graph::duplicate`](crate::Graph::duplicate) remaps
//! every other per-node reference, so this table rides along the same
//! remapping without a new field threading through node cloning.
//!
//! Retention bounds how many of a loop's most recent *completed* iterations
//! remain readable while the next iteration runs; the running iteration is
//! always readable on top of them. The executor keeps each iteration in its
//! own output layer, applies the default or per-type setting just before each
//! new iteration starts, and promotes only the newest layer when the loop
//! exits. The default is one: the previous iteration stays readable, so a body
//! system can read what a later system wrote in the previous iteration.
//!
//! Zero is a valid retention for a loop's default and for a single type, and
//! it means the same in both places: no completed iteration stays readable
//! while the next one runs. It never affects the loop exit — the last
//! iteration is still the one whose outputs cross the loop boundary.

use crate::node::NodeId;
use hashbrown::HashMap;
use std::any::TypeId;

/// Number of completed iterations retained for a loop's outputs when nothing
/// overrides it.
pub(crate) const DEFAULT_LOOP_RETENTION: usize = 1;

/// One loop's retention configuration: an optional default and per-type
/// overrides. A per-type override wins over the loop's own default for that
/// type.
#[derive(Debug, Clone, Default)]
struct LoopRetention {
    default: Option<usize>,
    per_type: HashMap<TypeId, usize>,
}

impl LoopRetention {
    /// The default retention for types with no explicit override.
    fn default_retention(&self) -> usize {
        self.default.unwrap_or(DEFAULT_LOOP_RETENTION)
    }
}

/// Side table of [`LoopRetention`] configuration, keyed by the owning loop
/// node's [`NodeId`].
///
/// Stored on [`Graph`](crate::Graph) — see
/// [`set_loop_retention`](crate::Graph::set_loop_retention) and
/// [`set_loop_retention_for_type`](crate::Graph::set_loop_retention_for_type)
/// for the builder API, both of which preserve `add_loop*`'s existing
/// `&mut Graph` chaining return.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoopRetentionTable(HashMap<NodeId, LoopRetention>);

impl LoopRetentionTable {
    /// Sets `node`'s own default retention, in iterations.
    pub(crate) fn set_default(&mut self, node: NodeId, iterations: usize) {
        self.0.entry(node).or_default().default = Some(iterations);
    }

    /// Sets a per-type retention override for `node`; wins over the loop's
    /// default for that type.
    pub(crate) fn set_for_type(&mut self, node: NodeId, type_id: TypeId, iterations: usize) {
        self.0
            .entry(node)
            .or_default()
            .per_type
            .insert(type_id, iterations);
    }

    /// Returns `node`'s default retention and its per-type overrides. The
    /// executor supplies these to Layer 1's output store, which applies them
    /// without depending on graph types.
    pub(crate) fn for_node(&self, node: &NodeId) -> (usize, Vec<(TypeId, usize)>) {
        self.0.get(node).map_or_else(
            || (DEFAULT_LOOP_RETENTION, Vec::new()),
            |retention| {
                (
                    retention.default_retention(),
                    retention
                        .per_type
                        .iter()
                        .map(|(type_id, iterations)| (*type_id, *iterations))
                        .collect(),
                )
            },
        )
    }

    /// Absorbs every entry from `other`, keyed as-is (no remap).
    ///
    /// A builder method that assembles a loop body, branch, case, or handler
    /// as a temporary `Graph` and then moves its nodes into `self` keeps the
    /// node ids unchanged, so the retention configured on a loop inside that
    /// temporary graph — or on a subgraph passed to
    /// [`Graph::append`](crate::Graph::append) — must come along. Node ids are
    /// unique, so `other` never configures a loop `self` also configures;
    /// were it to, `other`'s entry would replace `self`'s.
    pub(crate) fn merge(&mut self, other: LoopRetentionTable) {
        self.0.extend(other.0);
    }

    /// Remaps every entry's key through `map` (old id -> new id), for
    /// [`Graph::duplicate`](crate::Graph::duplicate). Every loop node id is
    /// always present in `map` (it covers every node in the graph being
    /// duplicated), so an entry is only ever dropped if the table outlived
    /// the loop node it was configured for, which the builder API — keyed by
    /// a `NodeId` obtained from the graph itself — never allows.
    pub(crate) fn remap(&self, map: &HashMap<NodeId, NodeId>) -> Self {
        Self(
            self.0
                .iter()
                .filter_map(|(id, retention)| {
                    map.get(id)
                        .map(|new_id| (new_id.clone(), retention.clone()))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retention_is_one_when_unconfigured() {
        let table = LoopRetentionTable::default();
        assert_eq!(table.for_node(&NodeId::new()), (1, Vec::new()));
    }

    #[test]
    fn a_default_of_zero_is_kept_as_zero() {
        let mut table = LoopRetentionTable::default();
        let node = NodeId::new();
        table.set_default(node.clone(), 0);
        table.set_for_type(node.clone(), TypeId::of::<TypeA>(), 0);
        assert_eq!(
            table.for_node(&node),
            (0, vec![(TypeId::of::<TypeA>(), 0)]),
            "zero means the same for the default and for a type override"
        );
    }

    #[test]
    fn set_default_configures_unoverridden_types() {
        let mut table = LoopRetentionTable::default();
        let node = NodeId::new();
        table.set_default(node.clone(), 3);
        assert_eq!(table.for_node(&node), (3, Vec::new()));
    }

    struct TypeA;
    struct TypeB;

    #[test]
    fn per_type_override_is_preserved_with_the_default() {
        let mut table = LoopRetentionTable::default();
        let node = NodeId::new();
        table.set_default(node.clone(), 2);
        table.set_for_type(node.clone(), TypeId::of::<TypeA>(), 5);
        let (default, overrides) = table.for_node(&node);
        assert_eq!(default, 2);
        assert_eq!(overrides, vec![(TypeId::of::<TypeA>(), 5)]);

        table.set_for_type(node.clone(), TypeId::of::<TypeB>(), 1);
        let (default, overrides) = table.for_node(&node);
        assert_eq!(default, 2);
        assert!(overrides.contains(&(TypeId::of::<TypeA>(), 5)));
        assert!(overrides.contains(&(TypeId::of::<TypeB>(), 1)));
    }

    #[test]
    fn unrelated_nodes_do_not_share_configuration() {
        let mut table = LoopRetentionTable::default();
        let configured = NodeId::new();
        let other = NodeId::new();
        table.set_default(configured.clone(), 4);
        assert_eq!(table.for_node(&configured), (4, Vec::new()));
        assert_eq!(table.for_node(&other), (1, Vec::new()));
    }

    #[test]
    fn merge_carries_default_and_per_type_entries_under_their_own_ids() {
        let mut receiving = LoopRetentionTable::default();
        let existing = NodeId::new();
        receiving.set_default(existing.clone(), 2);

        let mut incoming = LoopRetentionTable::default();
        let configured = NodeId::new();
        incoming.set_default(configured.clone(), 4);
        incoming.set_for_type(configured.clone(), TypeId::of::<TypeA>(), 0);
        let only_per_type = NodeId::new();
        incoming.set_for_type(only_per_type.clone(), TypeId::of::<TypeB>(), 6);

        receiving.merge(incoming);

        assert_eq!(
            receiving.for_node(&existing),
            (2, Vec::new()),
            "merging leaves the receiving table's own entries alone"
        );
        assert_eq!(
            receiving.for_node(&configured),
            (4, vec![(TypeId::of::<TypeA>(), 0)])
        );
        assert_eq!(
            receiving.for_node(&only_per_type),
            (1, vec![(TypeId::of::<TypeB>(), 6)]),
            "a loop with only a per-type override keeps the default of one"
        );
    }

    #[test]
    fn merging_an_empty_table_changes_nothing() {
        let mut table = LoopRetentionTable::default();
        let node = NodeId::new();
        table.set_default(node.clone(), 3);

        table.merge(LoopRetentionTable::default());

        assert_eq!(table.for_node(&node), (3, Vec::new()));
    }

    #[test]
    fn remap_translates_keys_and_drops_unmapped_entries() {
        let mut table = LoopRetentionTable::default();
        let old_id = NodeId::new();
        let new_id = NodeId::new();
        let unmapped = NodeId::new();
        table.set_default(old_id.clone(), 3);
        table.set_default(unmapped.clone(), 7);

        let mut map = HashMap::new();
        map.insert(old_id, new_id.clone());

        let remapped = table.remap(&map);
        assert_eq!(remapped.for_node(&new_id), (3, Vec::new()));
        assert_eq!(
            remapped.for_node(&unmapped),
            (1, Vec::new()),
            "an id absent from the remap is dropped, not carried over under its old id"
        );
    }
}
