//! Provenance analysis backing `read_from` wiring validation.
//!
//! `read_from` binds a reader system's `Out<T>` to one specific producer
//! *node*, not merely to a type — so the existing type-level "guaranteed
//! production" analysis in `signature.rs`
//! ([`Graph::must_produce_output_types_from_chains`]) cannot answer the
//! question `Graph::validate` needs: is *this exact* producer node guaranteed
//! to have completed by the time *this exact* reader node begins, along every
//! path? This module generalizes that analysis from output types to producer
//! node ids.
//!
//! The branch-crediting rules are the same ones `signature.rs` already
//! established (see [`Graph::must_produce_output_types_from_chains`] for the
//! type-level original), specialized to node identity:
//!
//! - a [`System`](crate::node::Node::System) node credits its own id;
//! - a [`Decision`](crate::node::Node::Decision) or
//!   [`Switch`](crate::node::Node::Switch) node credits nothing new past
//!   itself: unlike output *types*, two arms can never share a node id (they
//!   are disjoint node sets), so the type-level rule's "intersection of arm
//!   guarantees" always collapses to the empty set here — whatever was
//!   already guaranteed before the branch simply carries through unchanged;
//! - a [`Parallel`](crate::node::Node::Parallel) node credits the union of
//!   every branch's own guaranteed set once merged, since every branch
//!   unconditionally runs and merges back before the chain continues; a
//!   reader positioned *inside* one branch, however, cannot see anything
//!   written outside that branch (or in a sibling branch) at all, because the
//!   executor gives each branch a fresh, isolated output store
//!   (`SystemContext::child`) — so a producer search inside a branch starts
//!   from an empty accumulator, not the one carried in from outside;
//! - a [`Loop`](crate::node::Node::Loop) body credits nothing past the loop
//!   (zero iterations is legal), but is searched with whatever was already
//!   guaranteed on entry when the reader is nested inside it, since a loop
//!   body shares its enclosing context (no boundary is created per
//!   iteration);
//! - a node carrying an error/timeout edge is *also* searched down its
//!   handler subgraph, seeded with whatever was guaranteed just *before* that
//!   node (its own output is not credited on the failure path) — unlike the
//!   type-level walk, which simply stops at such a node, this walk needs to
//!   keep searching, since the reader may itself live inside the handler.
//!
//! [`Scope`](crate::node::Node::Scope) and
//! [`Dynamic`](crate::node::Node::Dynamic) nodes are opaque waypoints here:
//! their interiors are separate [`Graph`] instances with their own node id
//! namespace and their own `read_from` wiring table (checked independently
//! when [`Graph::validate`] recurses into them), so neither a reader nor a
//! producer belonging to `self` can be inside one.

use super::Graph;
use crate::node::{Node, NodeId};
use hashbrown::{HashMap, HashSet};

impl Graph {
    /// Returns the set of system node ids guaranteed to have completed by the
    /// time `target` begins execution, along every path from this graph's
    /// entry that reaches it — or `None` if `target` is not reached by this
    /// walk at all (for example: it lives in a decision arm or parallel
    /// branch this walk never enters from the entry, or it is genuinely
    /// unreachable).
    ///
    /// `target` itself is never a member of the returned set — the question
    /// is what is guaranteed *before* it begins, not once it completes.
    pub(crate) fn guaranteed_producers_before(&self, target: &NodeId) -> Option<HashSet<NodeId>> {
        let entry = self.entry()?;
        let seq = self.sequential_successors();
        let barriers = self.handler_sources();
        self.walk_for_target(HashSet::new(), entry, target, &seq, &barriers)
    }

    /// Core recursive search. `guaranteed` is whatever is already established
    /// on entry to `current`'s chain. Returns `Some(set)` the first time
    /// `target` is reached along a structurally reachable path — `set` is
    /// exactly what was guaranteed at that point, never including `target`
    /// itself. Returns `None` if this chain runs out, or dead-ends in a
    /// visited-node cycle, without ever reaching `target`.
    fn walk_for_target(
        &self,
        guaranteed: HashSet<NodeId>,
        mut current: NodeId,
        target: &NodeId,
        seq: &HashMap<NodeId, NodeId>,
        barriers: &HashSet<NodeId>,
    ) -> Option<HashSet<NodeId>> {
        let mut guaranteed = guaranteed;
        let mut visited: HashSet<NodeId> = HashSet::new();
        loop {
            if current == *target {
                return Some(guaranteed);
            }
            if !visited.insert(current.clone()) {
                return None;
            }

            // A failure here diverts into the handler subgraph instead of
            // continuing this chain — `target` may live there, seeded with
            // what was guaranteed *before* this node (its own output is not
            // produced on the failure path).
            if barriers.contains(&current) {
                for handler_entry in self.handler_entries(&current) {
                    if let Some(found) = self.walk_for_target(
                        guaranteed.clone(),
                        handler_entry,
                        target,
                        seq,
                        barriers,
                    ) {
                        return Some(found);
                    }
                }
            }

            match self.get_node(current.clone()) {
                Some(Node::System(_)) => {
                    guaranteed.insert(current.clone());
                }
                Some(Node::Decision(dec)) => {
                    for branch in [&dec.true_branch, &dec.false_branch].into_iter().flatten() {
                        if let Some(found) = self.walk_for_target(
                            guaranteed.clone(),
                            branch.clone(),
                            target,
                            seq,
                            barriers,
                        ) {
                            return Some(found);
                        }
                    }
                    // Neither arm contains `target`; arms are disjoint node
                    // sets, so nothing arm-specific could be credited past
                    // the decision anyway (see module docs).
                }
                Some(Node::Switch(sw)) => {
                    for branch in sw.cases.iter().map(|(_, t)| t).chain(sw.default.iter()) {
                        if let Some(found) = self.walk_for_target(
                            guaranteed.clone(),
                            branch.clone(),
                            target,
                            seq,
                            barriers,
                        ) {
                            return Some(found);
                        }
                    }
                }
                Some(Node::Parallel(par)) => {
                    for branch in &par.branches {
                        // Each branch runs against a fresh, isolated output
                        // store, so nothing from outside is visible inside —
                        // search with an empty accumulator.
                        if let Some(found) = self.walk_for_target(
                            HashSet::new(),
                            branch.clone(),
                            target,
                            seq,
                            barriers,
                        ) {
                            return Some(found);
                        }
                    }
                    // `target` is not inside any branch. Every branch
                    // unconditionally runs and merges back in branch order
                    // before the chain continues, so credit each branch's own
                    // guaranteed-on-completion set into the outer
                    // accumulator.
                    for branch in &par.branches {
                        self.credit_guaranteed_node_ids_from(
                            branch,
                            seq,
                            barriers,
                            &mut guaranteed,
                        );
                    }
                }
                Some(Node::Loop(lp)) => {
                    if let Some(body) = &lp.body_entry {
                        // Same context as the enclosing chain (no boundary is
                        // created per iteration), so whatever is already
                        // guaranteed carries into the body search.
                        if let Some(found) = self.walk_for_target(
                            guaranteed.clone(),
                            body.clone(),
                            target,
                            seq,
                            barriers,
                        ) {
                            return Some(found);
                        }
                    }
                    // Zero iterations is legal, so nothing inside the body is
                    // credited past the loop.
                }
                Some(Node::Scope(_) | Node::Dynamic(_)) | None => {
                    // A different `Graph` instance (or a dangling id) —
                    // `target`, which belongs to `self`, cannot be inside.
                }
            }

            current = seq.get(&current)?.clone();
        }
    }

    /// Credits every system node id guaranteed once `entry`'s chain runs to
    /// completion into `acc` — used only for the "every parallel branch
    /// unconditionally runs" half of [`walk_for_target`](Self::walk_for_target)'s
    /// parallel rule, so unlike that search there is no target to stop at.
    ///
    /// Mirrors [`Graph::must_produce_output_types_from_chains`]'s
    /// conservative barrier rule exactly (stop crediting entirely at a node
    /// with an error/timeout edge, never descending into its handler): this
    /// function answers "what does this region *unconditionally* deposit",
    /// the same question that function answers for types, just for node ids.
    fn credit_guaranteed_node_ids_from(
        &self,
        entry: &NodeId,
        seq: &HashMap<NodeId, NodeId>,
        barriers: &HashSet<NodeId>,
        acc: &mut HashSet<NodeId>,
    ) {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut current = Some(entry.clone());
        while let Some(node_id) = current {
            if !visited.insert(node_id.clone()) {
                break;
            }
            if barriers.contains(&node_id) {
                break;
            }
            match self.get_node(node_id.clone()) {
                Some(Node::System(_)) => {
                    acc.insert(node_id.clone());
                }
                Some(Node::Parallel(par)) => {
                    for branch in &par.branches {
                        self.credit_guaranteed_node_ids_from(branch, seq, barriers, acc);
                    }
                }
                // Decision/Switch: disjoint arms, nothing new credited (see
                // module docs). Loop: zero iterations is legal, nothing
                // credited. Scope/Dynamic: a different `Graph` instance.
                Some(
                    Node::Decision(_)
                    | Node::Switch(_)
                    | Node::Loop(_)
                    | Node::Scope(_)
                    | Node::Dynamic(_),
                )
                | None => {}
            }
            current = seq.get(&node_id).cloned();
        }
    }

    /// Returns `true` if some loop node's body (searched with
    /// [`reachable_nodes_with_handlers`](Self::reachable_nodes_with_handlers),
    /// so a handler subgraph nested inside the body counts too) contains both
    /// `a` and `b`.
    ///
    /// Backs the loop backward-wire exception: a
    /// `read_from` wire that fails the forward guarantee check is still
    /// admitted when both its reader and its producer are in the same loop
    /// body, since such a wire reads the producer's entry from an earlier
    /// iteration rather than requiring same-iteration ordering.
    pub(crate) fn shares_loop_body(&self, a: &NodeId, b: &NodeId) -> bool {
        for node in &self.nodes {
            let Node::Loop(lp) = node else { continue };
            let Some(body) = &lp.body_entry else {
                continue;
            };
            let ids: HashSet<NodeId> = self
                .reachable_nodes_with_handlers(body)
                .into_iter()
                .map(Node::id)
                .collect();
            if ids.contains(a) && ids.contains(b) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ContextPolicy;

    async fn out_i32() -> i32 {
        1
    }
    async fn out_string() -> String {
        "x".to_string()
    }
    async fn out_bool() -> bool {
        true
    }

    #[test]
    fn sequential_producer_is_guaranteed_before_reader() {
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let reader = graph.add_system_node(out_string);

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn reader_before_producer_is_not_guaranteed() {
        let mut graph = Graph::new();
        let reader = graph.add_system_node(out_string);
        let producer = graph.add_system_node(out_i32);

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(!guaranteed.contains(&producer));
    }

    #[test]
    fn producer_in_sibling_decision_arm_is_not_guaranteed() {
        let mut graph = Graph::new();
        let mut producer_id = None;
        let mut reader_id = None;
        graph.add_conditional_branch::<i32, _, _, _>(
            "dec",
            |v| *v > 0,
            |g| {
                producer_id = Some(g.add_system_node(out_i32));
            },
            |g| {
                reader_id = Some(g.add_system_node(out_string));
            },
        );
        let producer = producer_id.unwrap();
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(!guaranteed.contains(&producer));
    }

    #[test]
    fn producer_before_decision_is_guaranteed_in_either_arm() {
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let mut reader_id = None;
        graph.add_conditional_branch::<i32, _, _, _>(
            "dec",
            |v| *v > 0,
            |g| {
                g.add_system(out_bool);
            },
            |g| {
                reader_id = Some(g.add_system_node(out_string));
            },
        );
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn producer_in_sibling_parallel_branch_is_not_guaranteed() {
        let mut graph = Graph::new();
        let mut producer_id = None;
        let mut reader_id = None;
        graph.add_parallel(
            "par",
            [
                Box::new(|g: &mut Graph| {
                    producer_id = Some(g.add_system_node(out_i32));
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    reader_id = Some(g.add_system_node(out_string));
                }),
            ],
        );
        let producer = producer_id.unwrap();
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(!guaranteed.contains(&producer));
    }

    #[test]
    fn producer_before_parallel_is_not_visible_inside_a_branch() {
        // Each branch gets a fresh, isolated output store, so a producer
        // from before the fork is not guaranteed for a reader inside it.
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let mut reader_id = None;
        graph.add_parallel(
            "par",
            [|g: &mut Graph| {
                reader_id = Some(g.add_system_node(out_string));
            }],
        );
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(!guaranteed.contains(&producer));
    }

    #[test]
    fn producer_in_a_parallel_branch_is_guaranteed_after_the_join() {
        let mut graph = Graph::new();
        let mut producer_id = None;
        graph.add_parallel(
            "par",
            [|g: &mut Graph| {
                producer_id = Some(g.add_system_node(out_i32));
            }],
        );
        let reader = graph.add_system_node(out_string);
        let producer = producer_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn producer_before_loop_is_guaranteed_inside_the_body() {
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let mut reader_id = None;
        graph.add_loop_n("loop", 3, |g| {
            reader_id = Some(g.add_system_node(out_string));
        });
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn producer_inside_loop_body_is_not_guaranteed_after_the_loop() {
        let mut graph = Graph::new();
        let mut producer_id = None;
        graph.add_loop_n("loop", 3, |g| {
            producer_id = Some(g.add_system_node(out_i32));
        });
        let reader = graph.add_system_node(out_string);
        let producer = producer_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(!guaranteed.contains(&producer));
    }

    #[test]
    fn forward_producer_inside_loop_body_is_guaranteed_for_a_later_reader_in_the_same_body() {
        let mut graph = Graph::new();
        let mut producer_id = None;
        let mut reader_id = None;
        graph.add_loop_n("loop", 3, |g| {
            producer_id = Some(g.add_system_node(out_i32));
            reader_id = Some(g.add_system_node(out_string));
        });
        let producer = producer_id.unwrap();
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn backward_pair_in_same_loop_body_is_not_forward_guaranteed_but_shares_a_loop_body() {
        let mut graph = Graph::new();
        let mut reader_id = None;
        let mut producer_id = None;
        graph.add_loop_n("loop", 3, |g| {
            reader_id = Some(g.add_system_node(out_string));
            producer_id = Some(g.add_system_node(out_i32));
        });
        let reader = reader_id.unwrap();
        let producer = producer_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(
            !guaranteed.contains(&producer),
            "producer runs later in the same iteration, so it is not forward-guaranteed"
        );
        assert!(graph.shares_loop_body(&reader, &producer));
    }

    #[test]
    fn unrelated_nodes_do_not_share_a_loop_body() {
        let mut graph = Graph::new();
        let a = graph.add_system_node(out_i32);
        let mut reader_id = None;
        graph.add_loop_n("loop", 3, |g| {
            reader_id = Some(g.add_system_node(out_string));
        });
        let reader = reader_id.unwrap();
        assert!(!graph.shares_loop_body(&a, &reader));
    }

    #[test]
    fn producer_before_a_fallible_node_is_guaranteed_inside_its_error_handler() {
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let risky = graph.add_system_node(out_bool);
        let mut reader_id = None;
        graph.add_error_handler_for([risky], |g| {
            reader_id = Some(g.add_system_node(out_string));
        });
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable via the error edge");
        assert!(guaranteed.contains(&producer));
    }

    #[test]
    fn fallible_node_is_not_guaranteed_inside_its_own_handler() {
        let mut graph = Graph::new();
        let risky = graph.add_system_node(out_i32);
        let mut reader_id = None;
        graph.add_error_handler_for([risky.clone()], |g| {
            reader_id = Some(g.add_system_node(out_string));
        });
        let reader = reader_id.unwrap();

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable via the error edge");
        assert!(
            !guaranteed.contains(&risky),
            "the failing node's own output is not produced on the failure path"
        );
    }

    #[test]
    fn producer_before_a_declared_scope_is_not_visible_across_it() {
        // Scope/Dynamic are opaque here regardless of contract or policy —
        // their interior is a separate `Graph` instance a `read_from` binding
        // cannot even name (see module docs) — but this exercises that the
        // provenance walk itself does not mistakenly credit into or past one.
        let mut graph = Graph::new();
        let producer = graph.add_system_node(out_i32);
        let mut inner = Graph::new();
        inner.add_system(out_string);
        graph.add_scope("scope", inner, ContextPolicy::shared());
        let reader = graph.add_system_node(out_bool);

        let guaranteed = graph
            .guaranteed_producers_before(&reader)
            .expect("reader is reachable");
        assert!(guaranteed.contains(&producer));
    }
}
