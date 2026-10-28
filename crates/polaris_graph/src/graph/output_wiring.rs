//! Side table binding a reader system's output read to one producer node.
//!
//! [`SystemNodeBuilder::read_from`](super::SystemNodeBuilder::read_from) lets
//! a system bind its `Out<T>` read of `T` to a specific producer node, chosen
//! by the graph rather than named in the system's own type — see
//! `docs/reference/graph.md` and the output-store design doc for why. The
//! binding is kept in this side table, keyed by the reader's [`NodeId`] and
//! the output [`TypeId`], rather than as a field on [`SystemNode`](crate::node::SystemNode):
//! that keeps `read_from` bindings and [`Graph::duplicate`](super::Graph::duplicate)'s
//! id remap in one place, independent of `SystemNode`'s own fields.

use crate::node::{NodeId, remap_node_id};
use hashbrown::HashMap;
use std::any::TypeId;

/// One `read_from` binding: which producer node a reader's `Out<T>` resolves
/// to, and `T`'s name for diagnostics.
#[derive(Debug, Clone)]
pub(crate) struct ReadFromBinding {
    /// The producer node this reader's `Out<T>` resolves to.
    pub(crate) producer: NodeId,
    /// `T`'s type name, for validation error messages.
    pub(crate) type_name: &'static str,
}

/// Every `read_from` binding in one [`Graph`](super::Graph), keyed by
/// `(reader, output type)`.
///
/// A second `read_from::<T>` call for the same reader and `T` replaces the
/// earlier binding — the same "last call wins" rule as
/// [`SystemNodeBuilder::with_timeout`](super::SystemNodeBuilder::with_timeout).
#[derive(Debug, Clone, Default)]
pub(crate) struct OutputWiring {
    bindings: HashMap<(NodeId, TypeId), ReadFromBinding>,
}

impl OutputWiring {
    /// Records that `reader`'s read of `type_id` resolves to `producer`,
    /// replacing any earlier binding for the same `(reader, type_id)`.
    pub(crate) fn bind(
        &mut self,
        reader: NodeId,
        type_id: TypeId,
        producer: NodeId,
        type_name: &'static str,
    ) {
        self.bindings.insert(
            (reader, type_id),
            ReadFromBinding {
                producer,
                type_name,
            },
        );
    }

    /// Every binding, for [`Graph::validate`](super::Graph::validate).
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&NodeId, TypeId, &ReadFromBinding)> {
        self.bindings
            .iter()
            .map(|((reader, type_id), binding)| (reader, *type_id, binding))
    }

    /// Absorbs every binding from `other`, keyed as-is (no remap).
    ///
    /// Builder methods that flatten a temporary `Graph` into `self` — node
    /// ids unchanged, unlike [`Graph::duplicate`] — do it through
    /// `Graph::absorb`, which calls this, so a binding recorded through the
    /// branch's own builder (`g.read_from(...)` inside the closure) is not
    /// silently dropped when the temporary graph is discarded.
    pub(crate) fn merge(&mut self, other: OutputWiring) {
        self.bindings.extend(other.bindings);
    }

    /// The bindings a reader installs for the duration of its own `run`
    /// call: every `(output type, binding)` pair recorded for `reader`.
    pub(crate) fn for_reader(
        &self,
        reader: &NodeId,
    ) -> impl Iterator<Item = (TypeId, &ReadFromBinding)> {
        self.bindings
            .iter()
            .filter_map(move |((r, type_id), binding)| (r == reader).then_some((*type_id, binding)))
    }

    /// Remaps every reader and producer id through `map` (old → new), for
    /// [`Graph::duplicate`](super::Graph::duplicate).
    pub(crate) fn remap(&self, map: &HashMap<NodeId, NodeId>) -> Self {
        let bindings = self
            .bindings
            .iter()
            .map(|((reader, type_id), binding)| {
                let new_reader = remap_node_id(map, reader);
                let new_producer = remap_node_id(map, &binding.producer);
                (
                    (new_reader, *type_id),
                    ReadFromBinding {
                        producer: new_producer,
                        type_name: binding.type_name,
                    },
                )
            })
            .collect();
        Self { bindings }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_then_for_reader_returns_the_binding() {
        let mut wiring = OutputWiring::default();
        let reader = NodeId::new();
        let producer = NodeId::new();
        wiring.bind(reader.clone(), TypeId::of::<i32>(), producer.clone(), "i32");

        let found: Vec<_> = wiring.for_reader(&reader).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, TypeId::of::<i32>());
        assert_eq!(found[0].1.producer, producer);
    }

    #[test]
    fn second_bind_for_same_reader_and_type_replaces_the_first() {
        let mut wiring = OutputWiring::default();
        let reader = NodeId::new();
        let first_producer = NodeId::new();
        let second_producer = NodeId::new();
        wiring.bind(reader.clone(), TypeId::of::<i32>(), first_producer, "i32");
        wiring.bind(
            reader.clone(),
            TypeId::of::<i32>(),
            second_producer.clone(),
            "i32",
        );

        let found: Vec<_> = wiring.for_reader(&reader).collect();
        assert_eq!(found.len(), 1, "the later bind replaces the earlier one");
        assert_eq!(found[0].1.producer, second_producer);
    }

    #[test]
    fn remap_translates_reader_and_producer_ids() {
        let mut wiring = OutputWiring::default();
        let reader = NodeId::new();
        let producer = NodeId::new();
        wiring.bind(reader.clone(), TypeId::of::<i32>(), producer.clone(), "i32");

        let new_reader = NodeId::new();
        let new_producer = NodeId::new();
        let map: HashMap<NodeId, NodeId> = [
            (reader.clone(), new_reader.clone()),
            (producer.clone(), new_producer.clone()),
        ]
        .into_iter()
        .collect();

        let remapped = wiring.remap(&map);
        assert!(wiring.for_reader(&reader).next().is_some());
        let found: Vec<_> = remapped.for_reader(&new_reader).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.producer, new_producer);
        assert!(remapped.for_reader(&reader).next().is_none());
    }
}
