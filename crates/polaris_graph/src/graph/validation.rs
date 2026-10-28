//! Graph validation logic and error types.

use super::{Graph, MatchingMode, SignatureDiff};
use crate::edge::{Edge, EdgeId};
use crate::node::{CandidateSource, CrossingAction, Node, NodeId, NodeName};
use hashbrown::hash_map::Entry;
use hashbrown::{HashMap, HashSet};
use polaris_system::param::{Access, AccessMode, ERROR_CONTEXT};
use std::any::TypeId;
use std::fmt;
use std::sync::Arc;

/// Context tag for timeout path validation.
///
/// Used by the graph validator to check that systems requiring timeout
/// context are wired behind a timeout edge.
const TIMEOUT_CONTEXT: &str = "timeout";

impl Graph {
    // ─────────────────────────────────────────────────────────────────────────
    // Validation API
    // ─────────────────────────────────────────────────────────────────────────

    /// Validates the graph structure for correctness.
    ///
    /// This method performs build-time validation to catch errors before execution:
    /// - Verifies the graph has an entry point
    /// - Ensures every node is reachable from the entry point, including
    ///   through error/timeout handler subgraphs
    /// - Checks all edges reference valid nodes
    /// - Ensures decision nodes have predicates and both branches
    /// - Ensures loop nodes have termination conditions
    /// - Ensures parallel nodes have branches (subgraphs)
    /// - Ensures switch nodes have discriminators
    /// - Ensures no node declares more than one route for the same outcome
    ///   (success, error, or timeout)
    /// - Warns if parallel branches produce overlapping output types
    /// - Errors if a loop predicate reads an output type no body system produces
    /// - Ensures edge requirements are met (e.g. error edges target nodes that can fail)
    ///
    /// # Returns
    ///
    /// A [`ValidationResult`] containing both errors and warnings. Use
    /// [`ValidationResult::is_ok()`] to check if the graph is structurally valid.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::Graph;
    /// # async fn my_system() -> i32 { 1 }
    /// let mut graph = Graph::new();
    /// graph.add_system(my_system);
    ///
    ///
    /// let result = graph.validate();
    /// for w in &result.warnings {
    ///     tracing::warn!(%w, "graph validation warning");
    /// }
    /// if !result.is_ok() {
    ///     for err in &result.errors {
    ///         tracing::error!(%err, "graph validation error");
    ///     }
    /// }
    /// ```
    pub fn validate(&self) -> ValidationResult {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Check for entry point
        if self.entry.is_none() {
            errors.push(ValidationError::NoEntryPoint);
        }

        // Build a set of valid node IDs for quick lookup
        let valid_nodes: HashSet<NodeId> = self.nodes.iter().map(Node::id).collect();

        // Validate entry point exists
        if let Some(entry) = &self.entry
            && !valid_nodes.contains(entry)
        {
            errors.push(ValidationError::InvalidEntryPoint(entry.clone()));
        }

        // Validate connectivity: every node must be reachable from the entry
        // point, including through error/timeout handler subgraphs. Skipped
        // when the entry is missing or invalid — those are already reported
        // above, and reachability from a bogus entry says nothing useful.
        // `check_connectivity` only ever fails with `DisconnectedNodes` (see
        // its doc comment).
        if let Some(entry) = &self.entry
            && valid_nodes.contains(entry)
            && let Err(MergeError::DisconnectedNodes {
                orphan_count,
                orphans,
            }) = self.check_connectivity(entry)
        {
            errors.push(ValidationError::DisconnectedNodes {
                orphan_count,
                orphans,
            });
        }

        // Validate edges reference valid nodes and build edge-target index
        // for edge requirement validation.
        let mut error_edge_targets: HashSet<NodeId> = HashSet::new();
        let mut timeout_edge_targets: HashSet<NodeId> = HashSet::new();

        for edge in &self.edges {
            self.validate_edge(edge, &valid_nodes, &mut errors);

            match edge {
                Edge::Error(err_edge) => {
                    error_edge_targets.insert(err_edge.to.clone());
                }
                Edge::Timeout(timeout_edge) => {
                    timeout_edge_targets.insert(timeout_edge.to.clone());
                }
                _ => {}
            }
        }

        self.validate_route_cardinality(&mut errors);
        self.validate_output_wiring(&mut errors);

        // Validate each node
        for node in &self.nodes {
            self.validate_node(
                node,
                &valid_nodes,
                &error_edge_targets,
                &timeout_edge_targets,
                &mut errors,
                &mut warnings,
            );
        }

        ValidationResult { errors, warnings }
    }

    /// Validates a single edge.
    ///
    /// # Edge Type Validation
    ///
    /// ## Sequential Edge (A -> B)
    /// - `from` must reference an existing node
    /// - `to` must reference an existing node
    ///
    /// ## Conditional Edge (A -> B if true, A -> C if false)
    /// - `from` must reference an existing node (typically a `DecisionNode`)
    /// - `true_target` must reference an existing node
    /// - `false_target` must reference an existing node
    ///
    /// ## Parallel Edge (A -> [B, C, D])
    /// - `from` must reference an existing node (typically a `ParallelNode`)
    /// - All `targets` must reference existing nodes
    ///
    /// ## `LoopBack` Edge (end -> start)
    /// - `from` must reference an existing node (end of loop body)
    /// - `to` must reference an existing node (loop entry point)
    ///
    /// ## Error Edge (A -> handler on failure)
    /// - `from` must reference an existing node (the node that may fail)
    /// - `to` must reference an existing node (error handler)
    ///
    /// ## Timeout Edge (A -> handler on timeout)
    /// - `from` must reference an existing node (the node with timeout)
    /// - `to` must reference an existing node (timeout handler)
    fn validate_edge(
        &self,
        edge: &Edge,
        valid_nodes: &HashSet<NodeId>,
        errors: &mut Vec<ValidationError>,
    ) {
        match edge {
            // Sequential: simple A -> B connection
            // Both source and target must exist
            Edge::Sequential(seq) => {
                if !valid_nodes.contains(&seq.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: seq.id.clone(),
                        node: seq.from.clone(),
                    });
                }
                if !valid_nodes.contains(&seq.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: seq.id.clone(),
                        node: seq.to.clone(),
                    });
                }
            }
            // Conditional: binary branch with true/false targets
            // Source and both targets must exist
            Edge::Conditional(cond) => {
                if !valid_nodes.contains(&cond.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: cond.id.clone(),
                        node: cond.from.clone(),
                    });
                }
                if !valid_nodes.contains(&cond.true_target) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: cond.id.clone(),
                        node: cond.true_target.clone(),
                    });
                }
                if !valid_nodes.contains(&cond.false_target) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: cond.id.clone(),
                        node: cond.false_target.clone(),
                    });
                }
            }
            // Parallel: fork to multiple targets
            // Source and all targets must exist
            Edge::Parallel(par) => {
                if !valid_nodes.contains(&par.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: par.id.clone(),
                        node: par.from.clone(),
                    });
                }
                for target in &par.targets {
                    if !valid_nodes.contains(target) {
                        errors.push(ValidationError::InvalidEdgeTarget {
                            edge: par.id.clone(),
                            node: target.clone(),
                        });
                    }
                }
            }
            // LoopBack: return to earlier node for iteration
            // Both source (loop body end) and target (loop entry) must exist
            Edge::LoopBack(lb) => {
                if !valid_nodes.contains(&lb.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: lb.id.clone(),
                        node: lb.from.clone(),
                    });
                }
                if !valid_nodes.contains(&lb.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: lb.id.clone(),
                        node: lb.to.clone(),
                    });
                }
            }
            // Error: fallback path when a system fails
            // Both the failing node and error handler must exist
            Edge::Error(err) => {
                if !valid_nodes.contains(&err.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: err.id.clone(),
                        node: err.from.clone(),
                    });
                }
                if !valid_nodes.contains(&err.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: err.id.clone(),
                        node: err.to.clone(),
                    });
                }
            }
            // Timeout: fallback path when a system times out
            // Both the timed-out node and timeout handler must exist
            Edge::Timeout(timeout) => {
                if !valid_nodes.contains(&timeout.from) {
                    errors.push(ValidationError::InvalidEdgeSource {
                        edge: timeout.id.clone(),
                        node: timeout.from.clone(),
                    });
                }
                if !valid_nodes.contains(&timeout.to) {
                    errors.push(ValidationError::InvalidEdgeTarget {
                        edge: timeout.id.clone(),
                        node: timeout.to.clone(),
                    });
                }
            }
        }
    }

    /// Reports every node that declares more than one *distinct* route for the
    /// same outcome.
    ///
    /// See [`ValidationError::AmbiguousRoute`] for why the shape is rejected and
    /// what to build instead. This is a phase-2 check: it reads only the graph's
    /// own edge list, and no later phase — composition, run start, or execution
    /// — can make a skipped route take effect.
    ///
    /// Two shapes are deliberately accepted, both exercised by the tests below:
    /// fan-*in*, since the constraint is one route per node per outcome and not
    /// one per target; and repeated routes to the *same* target, which name the
    /// node the executor already reaches and so change no behavior anywhere.
    fn validate_route_cardinality(&self, errors: &mut Vec<ValidationError>) {
        /// The distinct targets one source declares for one outcome, in edge
        /// order.
        struct Routes {
            followed: NodeId,
            followed_name: NodeName,
            /// The skipped targets, in edge order, so reporting is stable
            /// within a source.
            skipped: Vec<(NodeId, NodeName)>,
            /// `followed` plus every entry of `skipped`. Kept alongside the
            /// vector so recognising an already-recorded target is a hash
            /// lookup rather than a scan of the whole fan-out.
            seen: HashSet<NodeId>,
        }

        // Node names by ID. The lookup doubles as the "is a node of this graph"
        // test for edge targets, so it is built before the edge scan.
        let names: HashMap<NodeId, NodeName> = self
            .nodes
            .iter()
            .map(|node| (node.id(), node.name()))
            .collect();

        for kind in RouteKind::ALL {
            let mut routes: HashMap<NodeId, Routes> = HashMap::new();
            for edge in &self.edges {
                let Some((from, to)) = kind.endpoints(edge) else {
                    continue;
                };
                // A target that is not a node of this graph is left out
                // entirely: `validate_edge` already reports it as an invalid
                // edge target, and crediting it as the followed target here
                // would blame a real node for being skipped in its place.
                let Some(to_name) = names.get(to).cloned() else {
                    continue;
                };
                match routes.entry(from.clone()) {
                    // The first matching edge in edge order is the one the
                    // executor follows.
                    Entry::Vacant(slot) => {
                        slot.insert(Routes {
                            followed: to.clone(),
                            followed_name: to_name,
                            skipped: Vec::new(),
                            seen: HashSet::from_iter([to.clone()]),
                        });
                    }
                    // A target already recorded for this source and outcome —
                    // whether the followed one or an earlier skipped one —
                    // declares nothing new, so it is not reported twice.
                    Entry::Occupied(mut slot) => {
                        let entry = slot.get_mut();
                        if entry.seen.insert(to.clone()) {
                            entry.skipped.push((to.clone(), to_name));
                        }
                    }
                }
            }

            // Walk the nodes rather than the map: node order is deterministic,
            // so reports are stable both within one source and across sources.
            // An edge whose source is not a node of this graph is already
            // reported as an invalid edge source.
            for node in &self.nodes {
                let id = node.id();
                let Some(entry) = routes.get(&id) else {
                    continue;
                };
                for (skipped, skipped_name) in &entry.skipped {
                    errors.push(ValidationError::AmbiguousRoute {
                        kind,
                        node: id.clone(),
                        name: node.name(),
                        followed: entry.followed.clone(),
                        followed_name: entry.followed_name.clone(),
                        skipped: skipped.clone(),
                        skipped_name: skipped_name.clone(),
                    });
                }
            }
        }
    }

    /// Validates every `read_from` binding recorded by
    /// [`SystemNodeBuilder::read_from`](super::SystemNodeBuilder::read_from).
    ///
    /// Rejects a binding whose producer is missing from this graph (which
    /// also covers a producer belonging to a different `Graph` instance:
    /// node ids are unique per instance, so a foreign id is simply absent
    /// from `self`), is not a system node, has a different output type, is
    /// the reader itself, or is not guaranteed to have completed before the
    /// reader on every path — unless both nodes share a loop body, the one
    /// case that instead reads an earlier iteration's entry, and then only
    /// when the reader's read is optional
    /// (`Option<Out<T>>`; see `ReadFromBackwardRequiresOptional`). The
    /// forward guarantee reuses the same branch-crediting rules
    /// `must_produce_output_types_from_chains` established for output types,
    /// generalized to producer node identity — see
    /// `output_provenance.rs`.
    ///
    /// Also rejects a binding for a type the reader does not read through
    /// `Out<T>`/`Option<Out<T>>` at all (`ReadFromUnread`) — checked before
    /// anything about the producer, since it is purely a property of the
    /// reader and is also what supplies the optional/required flag the
    /// backward-wire check above needs.
    fn validate_output_wiring(&self, errors: &mut Vec<ValidationError>) {
        for (reader, type_id, binding) in self.output_wiring.iter() {
            let reader_node = self.get_node(reader.clone());
            let reader_name = reader_node.map(Node::name).unwrap_or_default();

            if *reader == binding.producer {
                errors.push(ValidationError::ReadFromSelf {
                    node: reader.clone(),
                    name: reader_name,
                });
                continue;
            }

            let reader_bound = match reader_node {
                Some(Node::System(reader_sys)) => reader_sys
                    .system
                    .access()
                    .bindable_outputs
                    .into_iter()
                    .find(|bound| bound.type_id == type_id),
                _ => None,
            };
            let Some(reader_bound) = reader_bound else {
                errors.push(ValidationError::ReadFromUnread {
                    reader: reader.clone(),
                    reader_name,
                    type_name: binding.type_name,
                });
                continue;
            };

            let Some(producer_node) = self.get_node(binding.producer.clone()) else {
                errors.push(ValidationError::ReadFromProducerMissing {
                    reader: reader.clone(),
                    reader_name,
                    producer: binding.producer.clone(),
                });
                continue;
            };
            let Node::System(producer_sys) = producer_node else {
                errors.push(ValidationError::ReadFromProducerNotASystem {
                    reader: reader.clone(),
                    reader_name,
                    producer: binding.producer.clone(),
                    producer_name: producer_node.name(),
                });
                continue;
            };
            if producer_sys.output_type_id() != type_id {
                errors.push(ValidationError::ReadFromTypeMismatch {
                    reader: reader.clone(),
                    reader_name,
                    producer: binding.producer.clone(),
                    producer_name: producer_node.name(),
                    expected: binding.type_name,
                    actual: producer_sys.output_type_name(),
                });
                continue;
            }

            let forward_ok = self
                .guaranteed_producers_before(reader)
                .is_some_and(|guaranteed| guaranteed.contains(&binding.producer));
            if forward_ok {
                continue;
            }
            if self.shares_loop_body(reader, &binding.producer) {
                if reader_bound.optional {
                    continue;
                }
                errors.push(ValidationError::ReadFromBackwardRequiresOptional {
                    reader: reader.clone(),
                    reader_name,
                    producer: binding.producer.clone(),
                    producer_name: producer_node.name(),
                });
                continue;
            }
            errors.push(ValidationError::ReadFromNotGuaranteed {
                reader: reader.clone(),
                reader_name,
                producer: binding.producer.clone(),
                producer_name: producer_node.name(),
            });
        }
    }

    /// Validates a single node.
    ///
    /// # Node Type Validation
    ///
    /// ## `SystemNode`
    /// - Must meet edge requirements for any attached edges
    ///   e.g. if an error edge targets this node, it must be able to fail,
    ///   and if a timeout edge targets this node, it must have a timeout set.
    ///
    /// ## `DecisionNode`
    /// - Must have a predicate function
    /// - Must have both `true_branch` and `false_branch` targets
    /// - Branch targets must reference existing nodes
    ///
    /// ## `SwitchNode`
    /// - Must have a discriminator function
    /// - Must have at least one case or a default
    /// - All case targets must reference existing nodes
    /// - Default target (if present) must reference an existing node
    ///
    /// ## `ParallelNode`
    /// - Must have at least one branch
    /// - All branch targets must reference existing nodes
    ///
    /// ## `LoopNode`
    /// - Must have either a termination predicate or `max_iterations`
    /// - Must have a body entry point
    /// - Body entry must reference an existing node
    fn validate_node(
        &self,
        node: &Node,
        valid_nodes: &HashSet<NodeId>,
        error_edge_targets: &HashSet<NodeId>,
        timeout_edge_targets: &HashSet<NodeId>,
        errors: &mut Vec<ValidationError>,
        warnings: &mut Vec<ValidationWarning>,
    ) {
        match node {
            // System nodes: check context requirements against actual edge wiring
            Node::System(sys) => {
                let access = sys.system.access();
                for &tag in &access.context_requirements {
                    let satisfied = match tag {
                        ERROR_CONTEXT => error_edge_targets.contains(&sys.id),
                        TIMEOUT_CONTEXT => timeout_edge_targets.contains(&sys.id),
                        _ => true, // unknown tags are not validated here
                    };
                    if !satisfied {
                        errors.push(ValidationError::MissingEdgeRequirement {
                            node: sys.id.clone(),
                            name: sys.system.name(),
                            requirement: tag,
                        });
                    }
                }
            }

            // Decision nodes need a predicate and both branch targets
            Node::Decision(dec) => {
                if dec.predicate.is_none() {
                    errors.push(ValidationError::MissingPredicate {
                        node: dec.id.clone(),
                        name: dec.name.clone(),
                    });
                }
                if dec.true_branch.is_none() {
                    errors.push(ValidationError::MissingBranch {
                        node: dec.id.clone(),
                        name: dec.name.clone(),
                        branch: "true",
                    });
                } else if let Some(target) = &dec.true_branch
                    && !valid_nodes.contains(target)
                {
                    errors.push(ValidationError::InvalidBranchTarget {
                        node: dec.id.clone(),
                        branch: "true",
                        target: target.clone(),
                    });
                }
                if dec.false_branch.is_none() {
                    errors.push(ValidationError::MissingBranch {
                        node: dec.id.clone(),
                        name: dec.name.clone(),
                        branch: "false",
                    });
                } else if let Some(target) = &dec.false_branch
                    && !valid_nodes.contains(target)
                {
                    errors.push(ValidationError::InvalidBranchTarget {
                        node: dec.id.clone(),
                        branch: "false",
                        target: target.clone(),
                    });
                }
            }

            // Switch nodes need a discriminator and at least one case or default
            Node::Switch(sw) => {
                if sw.discriminator.is_none() {
                    errors.push(ValidationError::MissingDiscriminator {
                        node: sw.id.clone(),
                        name: sw.name.clone(),
                    });
                }
                if sw.cases.is_empty() && sw.default.is_none() {
                    errors.push(ValidationError::EmptySwitch {
                        node: sw.id.clone(),
                        name: sw.name.clone(),
                    });
                }
                for (case_name, target) in &sw.cases {
                    if !valid_nodes.contains(target) {
                        errors.push(ValidationError::InvalidCaseTarget {
                            node: sw.id.clone(),
                            case: case_name.clone(),
                            target: target.clone(),
                        });
                    }
                }
                if let Some(default) = &sw.default
                    && !valid_nodes.contains(default)
                {
                    errors.push(ValidationError::InvalidDefaultTarget {
                        node: sw.id.clone(),
                        target: default.clone(),
                    });
                }
            }

            // Parallel nodes need branches
            Node::Parallel(par) => {
                if par.branches.is_empty() {
                    errors.push(ValidationError::EmptyParallel {
                        node: par.id.clone(),
                        name: par.name.clone(),
                    });
                }
                for branch in &par.branches {
                    if !valid_nodes.contains(branch) {
                        errors.push(ValidationError::InvalidBranchTarget {
                            node: par.id.clone(),
                            branch: "parallel",
                            target: branch.clone(),
                        });
                    }
                }

                // Check for overlapping output types across parallel branches.
                // If 2+ branches produce the same type, the last branch in
                // declaration order silently wins at merge time.
                let mut type_counts: HashMap<TypeId, (usize, &'static str)> = HashMap::new();
                for branch in &par.branches {
                    let branch_types: HashSet<_> = self
                        .collect_branch_output_types(branch)
                        .into_iter()
                        .collect();
                    for (type_id, type_name) in branch_types {
                        type_counts
                            .entry(type_id)
                            .and_modify(|(count, _)| *count += 1)
                            .or_insert((1, type_name));
                    }
                }
                for (_, (count, type_name)) in type_counts {
                    if count > 1 {
                        warnings.push(ValidationWarning::ConflictingParallelOutputs {
                            node: par.id.clone(),
                            name: par.name.clone(),
                            output_type: type_name,
                        });
                    }
                }
            }

            // Loop nodes need a termination condition and a body
            Node::Loop(lp) => {
                // Must have either termination predicate or max_iterations to prevent infinite loops
                if lp.termination.is_none() && lp.max_iterations.is_none() {
                    errors.push(ValidationError::NoTerminationCondition {
                        node: lp.id.clone(),
                        name: lp.name.clone(),
                    });
                }
                if lp.body_entry.is_none() {
                    errors.push(ValidationError::EmptyLoopBody {
                        node: lp.id.clone(),
                        name: lp.name.clone(),
                    });
                } else if let Some(body) = &lp.body_entry
                    && !valid_nodes.contains(body)
                {
                    errors.push(ValidationError::InvalidLoopBody {
                        node: lp.id.clone(),
                        target: body.clone(),
                    });
                }

                // If a termination predicate exists and the body is valid,
                // check that the predicate's input type can be produced in the
                // loop body. Contracted boundaries credit their declared
                // outputs, just as they do for signature derivation.
                if let Some(term) = &lp.termination
                    && let Some(body) = &lp.body_entry
                    && valid_nodes.contains(body)
                {
                    let predicate_input = term.input_type_id();
                    let mut body_output_types = HashSet::new();
                    self.may_produce_output_types_from(body, &mut body_output_types);
                    if !body_output_types.contains(&predicate_input) {
                        errors.push(ValidationError::LoopPredicateOutputNotProduced {
                            node: lp.id.clone(),
                            name: lp.name.clone(),
                            expected_output: term.input_type_name(),
                        });
                    }
                }
            }

            // Scope nodes: validate the embedded graph recursively, then hold
            // it (and the boundary policy) to any declared contract.
            Node::Scope(scope) => {
                let mut inner_ok = false;
                if scope.graph.is_empty() {
                    errors.push(ValidationError::EmptyScopeGraph {
                        node: scope.id.clone(),
                        name: scope.name.clone(),
                    });
                } else {
                    if scope.graph.entry().is_none() {
                        errors.push(ValidationError::ScopeGraphNoEntryPoint {
                            node: scope.id.clone(),
                            name: scope.name.clone(),
                        });
                    }
                    let inner_result = scope.graph.validate();
                    inner_ok = inner_result.is_ok() && scope.graph.entry().is_some();
                    for inner_err in inner_result.errors {
                        errors.push(ValidationError::ScopeGraphInvalid {
                            node: scope.id.clone(),
                            name: scope.name.clone(),
                            inner: Box::new(inner_err),
                        });
                    }
                    for inner_warn in inner_result.warnings {
                        warnings.push(ValidationWarning::ScopeGraphWarning {
                            node: scope.id.clone(),
                            name: scope.name.clone(),
                            inner: Box::new(inner_warn),
                        });
                    }
                }
                if let Some(contract) = scope.contract() {
                    // Contract-vs-policy satisfiability is independent of the
                    // interior: a declared requirement the boundary blocks can
                    // never resolve at runtime, whichever graph sits inside.
                    let policy = scope.context_policy();
                    for crossing in policy.crossings() {
                        if matches!(crossing.action, CrossingAction::ForwardFresh) {
                            errors.push(ValidationError::ScopeContractFactoryUnsupported {
                                node: scope.id.clone(),
                                name: scope.name.clone(),
                                resource: crossing.type_name,
                            });
                        }
                    }
                    for access in contract.requires() {
                        if !policy.admits_contract_require(access) {
                            errors.push(ValidationError::ScopeContractRequireBlocked {
                                node: scope.id.clone(),
                                name: scope.name.clone(),
                                resource: access.type_name,
                                mode: access.mode,
                            });
                        }
                    }
                    if !policy.is_shared() && !contract.requires_outputs().is_empty() {
                        errors.push(ValidationError::ScopeContractOutputsBlocked {
                            node: scope.id.clone(),
                            name: scope.name.clone(),
                        });
                    }
                    // Drift is only checkable against a structurally valid
                    // interior whose derived signature is complete — a nested
                    // undeclared boundary hides IO, so no diff against it
                    // would be meaningful (mirroring registry admission).
                    if inner_ok {
                        if scope.graph.contains_nested_boundary() {
                            errors.push(ValidationError::ScopeContractNested {
                                node: scope.id.clone(),
                                name: scope.name.clone(),
                            });
                        } else {
                            let diff = scope.graph.declared_contract_diff(contract, policy);
                            if !diff.is_empty() {
                                errors.push(ValidationError::ScopeContractViolated {
                                    node: scope.id.clone(),
                                    name: scope.name.clone(),
                                    diff: Box::new(diff),
                                });
                            }
                        }
                    }
                }
            }

            // Dynamic nodes: validate each inline candidate recursively and check
            // it against the slot contract. Registry candidates are checked on
            // insertion ([`SubgraphRegistry::register`]), not here.
            Node::Dynamic(dynamic) => {
                if let CandidateSource::Inline(candidates) = &dynamic.source {
                    if candidates.is_empty() {
                        errors.push(ValidationError::EmptyDynamicSource {
                            node: dynamic.id.clone(),
                            name: dynamic.name.clone(),
                        });
                    }
                    let mut seen_keys: HashSet<&str> = HashSet::new();
                    for (key, _) in candidates {
                        if !seen_keys.insert(key.as_ref()) {
                            errors.push(ValidationError::DynamicCandidateDuplicate {
                                node: dynamic.id.clone(),
                                name: dynamic.name.clone(),
                                key: Arc::clone(key),
                            });
                        }
                    }
                    for (key, candidate) in candidates {
                        let inner_result = candidate.validate();
                        let candidate_ok = inner_result.is_ok();
                        for inner_err in inner_result.errors {
                            errors.push(ValidationError::DynamicCandidateInvalid {
                                node: dynamic.id.clone(),
                                name: dynamic.name.clone(),
                                key: Arc::clone(key),
                                inner: Box::new(inner_err),
                            });
                        }
                        for inner_warn in inner_result.warnings {
                            warnings.push(ValidationWarning::DynamicCandidateWarning {
                                node: dynamic.id.clone(),
                                name: dynamic.name.clone(),
                                key: Arc::clone(key),
                                inner: Box::new(inner_warn),
                            });
                        }
                        // A candidate that nests an undeclared Scope boundary
                        // presents a signature that hides the IO crossing it, so
                        // it cannot be soundly checked against the slot yet.
                        // Reject it rather than trust an opaque signature.
                        let nested = candidate.contains_nested_boundary();
                        if nested {
                            errors.push(ValidationError::DynamicCandidateNested {
                                node: dynamic.id.clone(),
                                name: dynamic.name.clone(),
                                key: Arc::clone(key),
                            });
                        }
                        // Only signature-check structurally valid, flat
                        // candidates, to avoid stacking an incompatibility error
                        // on top of a structural or nesting error (whose signature
                        // is unreliable anyway). The diff names which axes diverge,
                        // so the error explains the mismatch rather than just
                        // flagging it, and it comes from the slot's own
                        // `MatchingMode` relation so the message describes the
                        // check that actually ran.
                        if candidate_ok && !nested {
                            let candidate_signature = candidate.signature();
                            let diff = dynamic
                                .matching
                                .diff(&candidate_signature, &dynamic.contract);
                            if !diff.is_empty() {
                                errors.push(ValidationError::DynamicCandidateIncompatible {
                                    node: dynamic.id.clone(),
                                    name: dynamic.name.clone(),
                                    key: Arc::clone(key),
                                    diff: Box::new(diff),
                                    matching: dynamic.matching,
                                });
                            } else {
                                let missing = candidate_signature
                                    .missing_guaranteed_produces(&dynamic.contract);
                                if !missing.is_empty() {
                                    errors.push(
                                        ValidationError::DynamicCandidateMissingGuaranteedOutputs {
                                            node: dynamic.id.clone(),
                                            name: dynamic.name.clone(),
                                            key: Arc::clone(key),
                                            missing,
                                        },
                                    );
                                } else if dynamic.matching == MatchingMode::Substitutable {
                                    // An admitted `Substitutable` candidate may
                                    // produce output types the contract never
                                    // named. Promotion out of a `Dynamic` boundary
                                    // filters by the slot contract's `produces`
                                    // (the same rule a declared scope contract
                                    // enforces — see `execute_embedded` in
                                    // `executor/run.rs`), so such an extra is
                                    // computed and then dropped at the boundary:
                                    // it never reaches the parent, whatever the
                                    // parent chain does or doesn't also produce.
                                    // Not unsound — the candidate still executes
                                    // correctly — so this warns rather than
                                    // rejects: wasted work is worth flagging, not
                                    // failing the build over.
                                    let contracted: HashSet<TypeId> = dynamic
                                        .contract
                                        .produces()
                                        .iter()
                                        .map(|access| access.type_id)
                                        .collect();
                                    for access in candidate_signature.produces() {
                                        if !contracted.contains(&access.type_id) {
                                            warnings.push(
                                                ValidationWarning::DynamicCandidateShadowsOutput {
                                                    node: dynamic.id.clone(),
                                                    name: dynamic.name.clone(),
                                                    key: Arc::clone(key),
                                                    output_type: access.type_name,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Result of graph validation, containing both errors and warnings.
#[derive(Debug, Clone, Default)]
pub struct ValidationResult {
    /// Structural errors that prevent execution.
    pub errors: Vec<ValidationError>,
    /// Non-fatal issues that may cause unexpected runtime behavior.
    pub warnings: Vec<ValidationWarning>,
}

impl ValidationResult {
    /// Returns true if no structural errors were found.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns true if structural errors were found.
    #[must_use]
    pub fn is_err(&self) -> bool {
        !self.is_ok()
    }

    /// Returns true if warnings were found.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Return a vector of warnings.
    #[must_use]
    pub fn warnings(&self) -> &[ValidationWarning] {
        &self.warnings
    }

    /// Return a vector of errors.
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.errors
    }
}

impl fmt::Display for ValidationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_ok() {
            write!(
                f,
                "validation passed with 0 error(s) and {} warning(s)",
                self.warnings.len()
            )?;
        } else {
            write!(
                f,
                "validation failed with {} error(s) and {} warning(s)",
                self.errors.len(),
                self.warnings.len()
            )?;
        }

        for err in &self.errors {
            write!(f, "\n  error: {err}")?;
        }
        for warn in &self.warnings {
            write!(f, "\n  warning: {warn}")?;
        }
        Ok(())
    }
}

/// Warnings produced during graph validation.
///
/// Warnings indicate potential issues that won't prevent execution but may
/// cause unexpected behavior at runtime.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ValidationWarning {
    /// Two or more parallel branches produce the same output type.
    /// The last branch in declaration order will win at merge time.
    ConflictingParallelOutputs {
        /// The parallel node ID.
        node: NodeId,
        /// The parallel node name.
        name: NodeName,
        /// The conflicting output type name.
        output_type: &'static str,
    },
    /// A scope node's embedded graph has a validation warning.
    ScopeGraphWarning {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
        /// The inner validation warning from the embedded graph.
        inner: Box<ValidationWarning>,
    },
    /// A [`Substitutable`](MatchingMode::Substitutable) dynamic candidate
    /// produces an output type its slot contract never named.
    ///
    /// A `Dynamic` boundary promotes only the slot contract's `produces`
    /// (the same rule a declared scope contract enforces), so this extra
    /// output is computed and then dropped at the boundary — it never
    /// reaches the parent, regardless of whether the surrounding graph also
    /// produces that type. Not unsound, hence a warning rather than an
    /// error: to remove it, drop the extra output from the candidate, name
    /// the type in the contract so it's promoted deliberately, or keep
    /// [`MatchingMode::Exact`], under which the divergence cannot arise.
    ///
    /// Only inline candidates are covered. A registry candidate is admitted
    /// by [`SubgraphRegistry::register`](crate::registry::SubgraphRegistry::register)
    /// after the parent has been validated, so no such check is possible there.
    DynamicCandidateShadowsOutput {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
        /// The dropped output type name.
        output_type: &'static str,
    },
    /// A dynamic node's inline candidate has a validation warning.
    DynamicCandidateWarning {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
        /// The inner validation warning from the candidate graph.
        inner: Box<ValidationWarning>,
    },
}

impl fmt::Display for ValidationWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationWarning::ConflictingParallelOutputs {
                name,
                node,
                output_type,
            } => {
                write!(
                    f,
                    "parallel node '{name}' ({node}) has multiple branches producing output type '{output_type}'; last branch wins"
                )
            }
            ValidationWarning::ScopeGraphWarning { node, name, inner } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) embedded graph warning: {inner}"
                )
            }
            ValidationWarning::DynamicCandidateShadowsOutput {
                node,
                name,
                key,
                output_type,
            } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' produces output type '{output_type}', which its contract does not name; the value is dropped at the boundary and never reaches the parent"
                )
            }
            ValidationWarning::DynamicCandidateWarning {
                node,
                name,
                key,
                inner,
            } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' warning: {inner}"
                )
            }
        }
    }
}

impl std::error::Error for ValidationWarning {}

/// Which outcome a route leaves a node for.
///
/// A node runs, then finishes one of three ways, and each way has its own kind
/// of outgoing edge. Lookup is first-match for all three, so a node has at most
/// one *effective* route per outcome — see
/// [`ValidationError::AmbiguousRoute`].
///
/// # Examples
///
/// ```
/// use polaris_graph::{Graph, RouteKind, ValidationError};
///
/// async fn risky() -> Result<i32, &'static str> {
///     Ok(1)
/// }
/// async fn first_handler() -> i32 {
///     0
/// }
/// async fn second_handler() -> i32 {
///     -1
/// }
///
/// let mut graph = Graph::new();
/// let risky = graph.add_system_node(risky);
/// graph.add_error_handler_for([risky.clone()], |handler| {
///     handler.add_system(first_handler);
/// });
/// graph.add_error_handler_for([risky], |handler| {
///     handler.add_system(second_handler);
/// });
///
/// let result = graph.validate();
/// assert!(result.errors.iter().any(|error| matches!(
///     error,
///     ValidationError::AmbiguousRoute {
///         kind: RouteKind::Error,
///         ..
///     }
/// )));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RouteKind {
    /// The step succeeded: follow the sequential edge to the next node.
    Sequential,
    /// The step returned an error: follow the error edge to its handler.
    Error,
    /// The step exceeded its timeout: follow the timeout edge to its handler.
    Timeout,
}

impl RouteKind {
    /// Every outcome, in the order routes are reported.
    const ALL: [Self; 3] = [Self::Sequential, Self::Error, Self::Timeout];

    /// The endpoints of `edge` if it is a route for this outcome.
    fn endpoints(self, edge: &Edge) -> Option<(&NodeId, &NodeId)> {
        match (self, edge) {
            (Self::Sequential, Edge::Sequential(seq)) => Some((&seq.from, &seq.to)),
            (Self::Error, Edge::Error(err)) => Some((&err.from, &err.to)),
            (Self::Timeout, Edge::Timeout(timeout)) => Some((&timeout.from, &timeout.to)),
            _ => None,
        }
    }

    /// The remedy the diagnostic names for this outcome.
    const fn remedy(self) -> &'static str {
        match self {
            Self::Sequential => {
                "give the node a single successor and express the alternatives as a decision, \
                 switch, or parallel node"
            }
            Self::Error | Self::Timeout => {
                "give the node a single handler for this outcome — a blanket handler only fills \
                 nodes that have none, so wire the specific one first"
            }
        }
    }
}

impl fmt::Display for RouteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Sequential => "sequential",
            Self::Error => "error",
            Self::Timeout => "timeout",
        };
        f.write_str(name)
    }
}

/// Errors that can occur during graph validation.
///
/// These errors are detected at build time (when calling [`Graph::validate`])
/// before the graph is executed, allowing early detection of structural issues.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ValidationError {
    /// The graph has no entry point.
    NoEntryPoint,
    /// The entry point references an invalid node.
    InvalidEntryPoint(NodeId),
    /// A node is not reachable from the entry point.
    ///
    /// Reachability follows every route the executor can actually take —
    /// sequential, conditional, switch, parallel, and loop-body links, plus
    /// error/timeout edges into handler subgraphs — so an orphan here is one
    /// nothing in the graph ever visits, not merely one outside the happy
    /// path. Mirrors [`Graph::append`]'s connectivity check
    /// ([`MergeError::DisconnectedNodes`]), applied to the whole graph rather
    /// than just the piece being merged. Skipped when the entry itself is
    /// missing or invalid, since reachability from a bogus entry says
    /// nothing useful.
    DisconnectedNodes {
        /// Number of unreachable nodes.
        orphan_count: usize,
        /// The unreachable node IDs.
        orphans: Vec<NodeId>,
    },
    /// An edge's source node doesn't exist.
    InvalidEdgeSource {
        /// The edge ID.
        edge: EdgeId,
        /// The invalid node ID.
        node: NodeId,
    },
    /// An edge's target node doesn't exist.
    InvalidEdgeTarget {
        /// The edge ID.
        edge: EdgeId,
        /// The invalid node ID.
        node: NodeId,
    },
    /// A node declares more than one route for the same outcome.
    ///
    /// Lookup is first-match for every [`RouteKind`], so a later route for the
    /// same outcome is ignored: the work it declares is not wired into the path
    /// the executor walks. For the sequential outcome, interface derivation
    /// additionally reads the same edges into a map keyed by source where the
    /// *last* edge wins, so the graph advertises — and is pre-flighted along —
    /// a chain execution does not take.
    ///
    /// Model alternative *successors* as control flow —
    /// [`Graph::add_conditional_branch`], [`Graph::add_switch`], or
    /// [`Graph::add_parallel`] — so the choice is explicit and every branch is
    /// wired. Give error and timeout outcomes a single handler per node:
    /// [`Graph::add_error_handler`] fills only nodes that have no handler yet,
    /// so a specific handler must be attached *before* the blanket one, never
    /// after.
    ///
    /// The skipped target is not necessarily unreachable: another route may
    /// still lead to it. What always holds is that *this* route is never taken.
    ///
    /// Repeated routes to the *same* target are not an error: they name the
    /// target the executor already reaches.
    ///
    /// Not `#[non_exhaustive]` at the variant level, unlike
    /// [`CandidateSource::Registry`](crate::node::CandidateSource::Registry):
    /// the sequential shape this reports cannot be built through the public
    /// API at all (every builder chains from `last_node` and then advances it
    /// past the edge just created), so hand-constructing this variant is the
    /// only way to test its `Display` rendering outside the crate. The
    /// enclosing enum is `#[non_exhaustive]`, so matching on it still
    /// requires a wildcard arm.
    AmbiguousRoute {
        /// The outcome whose routes conflict.
        kind: RouteKind,
        /// The node declaring more than one route for that outcome.
        node: NodeId,
        /// The name of that node.
        name: NodeName,
        /// The target the executor reaches — the target of the first matching
        /// edge in edge order.
        followed: NodeId,
        /// The name of the followed target.
        followed_name: NodeName,
        /// The target named by a later route, which that route never reaches.
        /// It may still run by way of a different route.
        skipped: NodeId,
        /// The name of the skipped target.
        skipped_name: NodeName,
    },
    /// A decision node is missing its predicate.
    MissingPredicate {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A decision node is missing a branch target.
    MissingBranch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
        /// Which branch is missing ("true" or "false").
        branch: &'static str,
    },
    /// A branch target references an invalid node.
    InvalidBranchTarget {
        /// The node ID.
        node: NodeId,
        /// The branch name.
        branch: &'static str,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A switch node is missing its discriminator.
    MissingDiscriminator {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A switch node has no cases and no default.
    EmptySwitch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A switch case target references an invalid node.
    InvalidCaseTarget {
        /// The node ID.
        node: NodeId,
        /// The case name.
        case: NodeName,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A switch default target references an invalid node.
    InvalidDefaultTarget {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A parallel node has no branches.
    EmptyParallel {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A loop node has no termination condition.
    NoTerminationCondition {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A loop node has no body.
    EmptyLoopBody {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: NodeName,
    },
    /// A loop body entry references an invalid node.
    InvalidLoopBody {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A loop's termination predicate reads an output type that the loop body
    /// cannot produce.
    LoopPredicateOutputNotProduced {
        /// The loop node ID.
        node: NodeId,
        /// The loop node name.
        name: NodeName,
        /// The output type the predicate expects.
        expected_output: &'static str,
    },
    /// A system requires a specific edge type but is not reachable via that edge.
    ///
    /// For example, a system using `CaughtError` must be the target of an
    /// error edge; placing it on a normal sequential path is a wiring mistake.
    MissingEdgeRequirement {
        /// The node ID.
        node: NodeId,
        /// The system name.
        name: &'static str,
        /// A human-readable description of the required edge type.
        requirement: &'static str,
    },
    /// A scope node contains an empty graph.
    EmptyScopeGraph {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
    },
    /// A scope node's embedded graph has no entry point.
    ScopeGraphNoEntryPoint {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
    },
    /// A scope node's embedded graph has a validation error.
    ScopeGraphInvalid {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
        /// The inner validation error from the embedded graph.
        inner: Box<ValidationError>,
    },
    /// A scope's declared contract names a resource requirement its
    /// [`ContextPolicy`](crate::node::ContextPolicy) blocks from crossing the
    /// boundary, so the requirement could never resolve at runtime.
    ///
    /// A read (`Res<T>`) crosses via `shared()`, `share::<T>()`, an
    /// un-excluded `share_rest()`, or a copy verb (`forward` / `fork`). A write
    /// (`ResMut<T>`) operates on the current scope only, so across a non-shared
    /// boundary it needs a copy verb — sharing
    /// gives the interior chain reads, never a local to mutate.
    ScopeContractRequireBlocked {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
        /// The blocked resource type.
        resource: &'static str,
        /// How the contract requires it (`Read` → `Res<T>`, `Write` →
        /// `ResMut<T>`).
        mode: AccessMode,
    },
    /// A declared scope uses `forward_fresh`, whose factory requirement cannot
    /// be represented by [`GraphSignature`](super::GraphSignature).
    ///
    /// Use `forward`/`fork` with a declared source resource, or an undeclared
    /// scope when factory-based initialization is needed.
    ScopeContractFactoryUnsupported {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
        /// The resource whose factory would be required at scope entry.
        resource: &'static str,
    },
    /// A scope with a non-shared [`ContextPolicy`](crate::node::ContextPolicy)
    /// declares a nonempty `requires_outputs` in its contract.
    ///
    /// Free outputs never cross a non-shared boundary inward — a child context
    /// starts with empty outputs and never walks the parent chain for them —
    /// so such a contract is statically unsatisfiable. Use a shared policy if
    /// the interior must read free outputs from the parent, or drop the
    /// `requires_outputs` from the contract.
    ScopeContractOutputsBlocked {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
    },
    /// A scope declares a contract but its embedded graph nests an undeclared
    /// [`Scope`](crate::node::Node::Scope) boundary, which flat signature
    /// derivation cannot see across, so the interior cannot be soundly checked
    /// against the contract. Declare a contract on the nested scope, or
    /// flatten the interior.
    ScopeContractNested {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
    },
    /// A scope's embedded graph and eager boundary inputs diverge from its
    /// declared contract.
    ///
    /// `requires` and `requires_outputs` use the same exact-match relation
    /// [`SubgraphRegistry::register`](crate::registry::SubgraphRegistry::register)
    /// admission uses, since both surface verbatim into the enclosing graph's
    /// signature. `produces` is checked as a **subset**: every declared
    /// produce must be backed by the interior's *guaranteed* production — the
    /// parent signature credits it as guaranteed downstream, so a produce
    /// backed only by conditional interior production (a decision case, a
    /// loop body) is drift — but guaranteed production the contract omits is
    /// not; the executor's merge-back keeps it private to the scope instead.
    ScopeContractViolated {
        /// The scope node ID.
        node: NodeId,
        /// The scope node name.
        name: NodeName,
        /// Which signature axes diverge, and in which direction. Boxed to keep
        /// [`ValidationError`] small.
        diff: Box<SignatureDiff>,
    },
    /// A dynamic node's inline candidate source is empty.
    EmptyDynamicSource {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
    },
    /// A dynamic node's inline candidate has a validation error.
    DynamicCandidateInvalid {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
        /// The inner validation error from the candidate graph.
        inner: Box<ValidationError>,
    },
    /// A dynamic node's inline candidate signature is not admitted by the
    /// node's slot contract under the slot's
    /// [`MatchingMode`](crate::graph::MatchingMode) relation.
    DynamicCandidateIncompatible {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
        /// Which signature axes diverge, and in which direction. Boxed to keep
        /// [`ValidationError`] small.
        diff: Box<SignatureDiff>,
        /// The relation the candidate was checked under. The `diff` reports
        /// only the directions *this* relation forbids, so without the mode a
        /// reader cannot tell a permitted divergence from an absent one.
        matching: MatchingMode,
    },
    /// A candidate may produce a slot output on some path, but does not
    /// guarantee it on every successful path as the slot contract promises.
    DynamicCandidateMissingGuaranteedOutputs {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
        /// Contract output entries the candidate does not guarantee.
        missing: Vec<Access>,
    },
    /// A dynamic node's inline candidate nests an undeclared
    /// [`Scope`](crate::node::Node::Scope) boundary, which flat signature
    /// derivation cannot see across, so the candidate cannot be soundly checked
    /// against the slot contract. A scope carrying a declared contract
    /// ([`Graph::add_scope_with_contract`](crate::Graph::add_scope_with_contract))
    /// and a nested dynamic node do not disqualify a candidate — their
    /// contracts are folded into the candidate's signature.
    DynamicCandidateNested {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The candidate key.
        key: Arc<str>,
    },
    /// Two of a dynamic node's inline candidates share the same key.
    ///
    /// Inline lookup is first-match, so the later candidate can never be
    /// selected — it is validated and signature-checked as if it participates,
    /// then silently ignored at runtime. (A [`SubgraphRegistry`] replaces by
    /// key instead, through its `Result`-returning `register`.)
    ///
    /// [`SubgraphRegistry`]: crate::registry::SubgraphRegistry
    DynamicCandidateDuplicate {
        /// The dynamic node ID.
        node: NodeId,
        /// The dynamic node name.
        name: NodeName,
        /// The key shared by more than one candidate.
        key: Arc<str>,
    },
    /// A [`SystemNodeBuilder::read_from`](super::SystemNodeBuilder::read_from)
    /// binding names its own reader as its producer.
    ReadFromSelf {
        /// The reader (and would-be producer) node ID.
        node: NodeId,
        /// The reader's name.
        name: NodeName,
    },
    /// A `read_from` binding's producer node does not exist in this graph.
    ///
    /// This also covers a producer belonging to a different [`Graph`]
    /// instance: node ids are unique per instance, so a foreign id is simply
    /// absent from `self`'s own node list.
    ReadFromProducerMissing {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The missing producer node ID.
        producer: NodeId,
    },
    /// A `read_from` binding's producer node exists but is not a
    /// [`System`](crate::node::Node::System) node.
    ReadFromProducerNotASystem {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The producer node ID.
        producer: NodeId,
        /// The producer's name.
        producer_name: NodeName,
    },
    /// A `read_from` binding's producer node's declared output type differs
    /// from the bound type.
    ReadFromTypeMismatch {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The producer node ID.
        producer: NodeId,
        /// The producer's name.
        producer_name: NodeName,
        /// The type name the binding names.
        expected: &'static str,
        /// The producer's actual declared output type name.
        actual: &'static str,
    },
    /// A `read_from` binding's producer is not guaranteed to have completed
    /// before the reader on every path, and the two nodes do not share a
    /// loop body.
    ///
    /// A wire inside one loop body is the one exception: it reads the
    /// producer's entry from an earlier iteration rather than requiring
    /// same-iteration ordering, so it is admitted even
    /// when this forward check fails.
    ReadFromNotGuaranteed {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The producer node ID.
        producer: NodeId,
        /// The producer's name.
        producer_name: NodeName,
    },
    /// A `read_from` binding names a type the reader never reads through
    /// `Out<T>` or `Option<Out<T>>` — the only forms a binding can redirect
    /// (`SystemAccess::bindable_outputs` in `polaris_system`).
    ///
    /// Covers a typo'd type, a reader that is not a system node, and a type
    /// the reader reads only through `OutAll<T>` (which returns every
    /// producer's entry, so pinning it to one would contradict its
    /// purpose) or only through `ErrOut<T>` (which always reads the newest
    /// `CaughtError`-like entry, ignoring any installed binding). In every
    /// case the binding would have no observable effect, so it is rejected
    /// the same way a binding that resolves nothing at all would be.
    ReadFromUnread {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The bound type's name, for diagnostics.
        type_name: &'static str,
    },
    /// A backward `read_from` wire (same loop body, producer not guaranteed
    /// before the reader) is bound through `Out<T>`
    /// instead of `Option<Out<T>>`.
    ///
    /// A backward wire reads the producer's entry from an earlier
    /// iteration, which does not exist on the loop's first iteration.
    /// `Out<T>` would surface a runtime `ParamError::OutputNotFound` on
    /// that iteration instead of failing at `validate()` time; decision 6
    /// requires `Option<Out<T>>` so the system handles the missing entry
    /// itself.
    ReadFromBackwardRequiresOptional {
        /// The reader node ID.
        reader: NodeId,
        /// The reader's name.
        reader_name: NodeName,
        /// The producer node ID.
        producer: NodeId,
        /// The producer's name.
        producer_name: NodeName,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::NoEntryPoint => write!(f, "graph has no entry point"),
            ValidationError::InvalidEntryPoint(id) => {
                write!(f, "entry point references invalid node: {id}")
            }
            ValidationError::DisconnectedNodes {
                orphan_count,
                orphans,
            } => {
                write!(f, "graph has {orphan_count} node(s) unreachable from entry")?;
                if *orphan_count > 0 {
                    write!(f, ": ")?;
                    for (i, orphan) in orphans.iter().take(3).enumerate() {
                        write!(f, "{orphan}")?;
                        if i < orphans.len().min(3) - 1 {
                            write!(f, ", ")?;
                        }
                    }
                    if *orphan_count > 3 {
                        write!(f, ", ...")?;
                    }
                }
                Ok(())
            }
            ValidationError::InvalidEdgeSource { edge, node } => {
                write!(f, "edge {edge} has invalid source node: {node}")
            }
            ValidationError::InvalidEdgeTarget { edge, node } => {
                write!(f, "edge {edge} has invalid target node: {node}")
            }
            ValidationError::AmbiguousRoute {
                kind,
                node,
                name,
                followed,
                followed_name,
                skipped,
                skipped_name,
            } => {
                write!(
                    f,
                    "node '{name}' ({node}) declares more than one distinct {kind} route — lookup is first-match, so it follows '{followed_name}' ({followed}) and the route to '{skipped_name}' ({skipped}) is never followed; {remedy}",
                    remedy = kind.remedy()
                )
            }
            ValidationError::MissingPredicate { node, name } => {
                write!(f, "decision node '{name}' ({node}) is missing predicate")
            }
            ValidationError::MissingBranch { node, name, branch } => {
                write!(
                    f,
                    "decision node '{name}' ({node}) is missing {branch} branch"
                )
            }
            ValidationError::InvalidBranchTarget {
                node,
                branch,
                target,
            } => {
                write!(
                    f,
                    "node {node} has {branch} branch pointing to invalid node: {target}"
                )
            }
            ValidationError::MissingDiscriminator { node, name } => {
                write!(f, "switch node '{name}' ({node}) is missing discriminator")
            }
            ValidationError::EmptySwitch { node, name } => {
                write!(
                    f,
                    "switch node '{name}' ({node}) has no cases and no default"
                )
            }
            ValidationError::InvalidCaseTarget { node, case, target } => {
                write!(
                    f,
                    "switch node {node} has case '{case}' pointing to invalid node: {target}"
                )
            }
            ValidationError::InvalidDefaultTarget { node, target } => {
                write!(
                    f,
                    "switch node {node} has default pointing to invalid node: {target}"
                )
            }
            ValidationError::EmptyParallel { node, name } => {
                write!(f, "parallel node '{name}' ({node}) has no branches")
            }
            ValidationError::NoTerminationCondition { node, name } => {
                write!(
                    f,
                    "loop node '{name}' ({node}) has no termination condition (predicate or max_iterations)"
                )
            }
            ValidationError::EmptyLoopBody { node, name } => {
                write!(f, "loop node '{name}' ({node}) has no body")
            }
            ValidationError::InvalidLoopBody { node, target } => {
                write!(
                    f,
                    "loop node {node} has body entry pointing to invalid node: {target}"
                )
            }
            ValidationError::LoopPredicateOutputNotProduced {
                node,
                name,
                expected_output,
            } => {
                write!(
                    f,
                    "loop node '{name}' ({node}) predicate expects output type '{expected_output}' not produced in the loop body"
                )
            }
            ValidationError::MissingEdgeRequirement {
                node,
                name,
                requirement,
            } => {
                write!(
                    f,
                    "system '{name}' ({node}) requires {requirement} edge context but is not reachable via a matching edge"
                )
            }
            ValidationError::EmptyScopeGraph { node, name } => {
                write!(f, "scope node '{name}' ({node}) contains an empty graph")
            }
            ValidationError::ScopeGraphNoEntryPoint { node, name } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) embedded graph has no entry point"
                )
            }
            ValidationError::ScopeGraphInvalid { node, name, inner } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) embedded graph error: {inner}"
                )
            }
            ValidationError::ScopeContractRequireBlocked {
                node,
                name,
                resource,
                mode,
            } => {
                let (rendered, remedy) = match mode {
                    AccessMode::Read => (
                        format!("Res<{resource}>"),
                        "add share/forward/fork for the type, share_rest(), or a shared policy",
                    ),
                    AccessMode::Write => (
                        format!("ResMut<{resource}>"),
                        "writes need a local copy: add forward/fork for the type, or a shared policy",
                    ),
                };
                write!(
                    f,
                    "scope node '{name}' ({node}) declares a contract requiring {rendered} but its context policy blocks it from crossing the boundary — {remedy}"
                )
            }
            ValidationError::ScopeContractFactoryUnsupported {
                node,
                name,
                resource,
            } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) uses forward_fresh for {resource}, but declared contracts cannot represent factory requirements — use forward/fork with a declared source resource, or an undeclared scope"
                )
            }
            ValidationError::ScopeContractOutputsBlocked { node, name } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) has a non-shared context policy but its declared contract requires free outputs, which cannot cross the boundary — use a shared policy or drop the required outputs"
                )
            }
            ValidationError::ScopeContractNested { node, name } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) declares a contract but its embedded graph nests an undeclared scope, which signature derivation cannot see across — declare a contract on the nested scope or flatten the interior"
                )
            }
            ValidationError::ScopeContractViolated { node, name, diff } => {
                write!(
                    f,
                    "scope node '{name}' ({node}) embedded graph and boundary inputs do not match its declared contract ({diff})"
                )
            }
            ValidationError::EmptyDynamicSource { node, name } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) has an empty inline candidate set"
                )
            }
            ValidationError::DynamicCandidateInvalid {
                node,
                name,
                key,
                inner,
            } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' error: {inner}"
                )
            }
            ValidationError::DynamicCandidateIncompatible {
                node,
                name,
                key,
                diff,
                matching,
            } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' is not admitted against the slot contract under {matching} matching ({diff})"
                )
            }
            ValidationError::DynamicCandidateMissingGuaranteedOutputs {
                node,
                name,
                key,
                missing,
            } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' does not guarantee slot output(s): "
                )?;
                for (index, access) in missing.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "Out<{}>", access.type_name)?;
                }
                Ok(())
            }
            ValidationError::DynamicCandidateNested { node, name, key } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) candidate '{key}' nests an undeclared scope; signature derivation does not see across that boundary — declare a contract on the nested scope (add_scope_with_contract) or flatten the candidate"
                )
            }
            ValidationError::DynamicCandidateDuplicate { node, name, key } => {
                write!(
                    f,
                    "dynamic node '{name}' ({node}) has more than one inline candidate keyed '{key}' — lookup is first-match, so the later candidate can never be selected"
                )
            }
            ValidationError::ReadFromSelf { node, name } => {
                write!(
                    f,
                    "system '{name}' ({node}) has a read_from binding naming itself as its own producer"
                )
            }
            ValidationError::ReadFromProducerMissing {
                reader,
                reader_name,
                producer,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a read_from binding to producer {producer}, which is not a node of this graph (missing, or belonging to a different Graph instance)"
                )
            }
            ValidationError::ReadFromProducerNotASystem {
                reader,
                reader_name,
                producer,
                producer_name,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a read_from binding to '{producer_name}' ({producer}), which is not a system node"
                )
            }
            ValidationError::ReadFromTypeMismatch {
                reader,
                reader_name,
                producer,
                producer_name,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a read_from binding to '{producer_name}' ({producer}) expecting output type '{expected}', but that producer's output type is '{actual}'"
                )
            }
            ValidationError::ReadFromNotGuaranteed {
                reader,
                reader_name,
                producer,
                producer_name,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a read_from binding to '{producer_name}' ({producer}), which is not guaranteed to have completed on every path before '{reader_name}' runs, and the two nodes do not share a loop body"
                )
            }
            ValidationError::ReadFromUnread {
                reader,
                reader_name,
                type_name,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a read_from binding for type '{type_name}', but does not read it through Out<T> or Option<Out<T>> (OutAll<T> and ErrOut<T> are not affected by read_from bindings)"
                )
            }
            ValidationError::ReadFromBackwardRequiresOptional {
                reader,
                reader_name,
                producer,
                producer_name,
            } => {
                write!(
                    f,
                    "system '{reader_name}' ({reader}) has a backward read_from binding to '{producer_name}' ({producer}) inside the same loop body, through Out<T> instead of Option<Out<T>>; the producer's entry does not exist on the loop's first iteration"
                )
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Errors that can occur when appending one graph to another via
/// [`Graph::append`].
#[derive(Debug, Clone)]
pub enum MergeError {
    /// A graph has no entry point.
    NoEntry,
    /// A graph has no exit point (last node).
    NoExit,
    /// A graph contains nodes not reachable from its entry point.
    DisconnectedNodes {
        /// Number of unreachable nodes.
        orphan_count: usize,
        /// Orphan node IDs (up to 3 for error message).
        orphans: Vec<NodeId>,
    },
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MergeError::NoEntry => write!(f, "graph has no entry point"),
            MergeError::NoExit => write!(f, "graph has no exit point (last node)"),
            MergeError::DisconnectedNodes {
                orphan_count,
                orphans,
            } => {
                write!(f, "graph has {orphan_count} node(s) unreachable from entry")?;
                if *orphan_count > 0 {
                    write!(f, ": ")?;
                    for (i, orphan) in orphans.iter().take(3).enumerate() {
                        write!(f, "{orphan}")?;
                        if i < orphans.len().min(3) - 1 {
                            write!(f, ", ")?;
                        }
                    }
                    if *orphan_count > 3 {
                        write!(f, ", ...")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for MergeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::{ErrorEdge, TimeoutEdge};
    use crate::graph::GraphSignature;
    use crate::node::{ContextPolicy, DynamicSlot, Node, SystemNode};
    use polaris_system::system::IntoSystem;

    async fn dummy() -> i32 {
        1
    }

    async fn second() -> String {
        "second".to_string()
    }

    async fn third() -> bool {
        true
    }

    async fn fourth() -> u8 {
        4
    }

    /// Collects the ambiguous-route errors for one outcome as
    /// `(node, followed, skipped)` triples.
    fn routes(result: &ValidationResult, kind: RouteKind) -> Vec<(NodeId, NodeId, NodeId)> {
        result
            .errors
            .iter()
            .filter_map(|err| match err {
                ValidationError::AmbiguousRoute {
                    kind: reported,
                    node,
                    followed,
                    skipped,
                    ..
                } if *reported == kind => Some((node.clone(), followed.clone(), skipped.clone())),
                _ => None,
            })
            .collect()
    }

    /// The sequential-outcome ambiguities — the common case in these tests.
    fn ambiguities(result: &ValidationResult) -> Vec<(NodeId, NodeId, NodeId)> {
        routes(result, RouteKind::Sequential)
    }

    /// Collects the ambiguous-route errors for one outcome as
    /// `(name, followed_name, skipped_name)` triples, so the reported names can
    /// be asserted independently of IDs.
    fn route_names(
        result: &ValidationResult,
        kind: RouteKind,
    ) -> Vec<(NodeName, NodeName, NodeName)> {
        result
            .errors
            .iter()
            .filter_map(|err| match err {
                ValidationError::AmbiguousRoute {
                    kind: reported,
                    name,
                    followed_name,
                    skipped_name,
                    ..
                } if *reported == kind => {
                    Some((name.clone(), followed_name.clone(), skipped_name.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The sequential-outcome names — the common case in these tests.
    fn ambiguity_names(result: &ValidationResult) -> Vec<(NodeName, NodeName, NodeName)> {
        route_names(result, RouteKind::Sequential)
    }

    /// A node with no route leading to it from the entry point is reported,
    /// not silently accepted because its own fields look valid.
    ///
    /// The shape needs `pub(crate)` access to `Graph::nodes`: every public
    /// builder method wires the node it adds into the chain (or a branch,
    /// case, body, or handler slot) before returning, so there is no way to
    /// reach this state through the public API today.
    #[test]
    fn a_node_with_no_route_to_it_is_reported_as_disconnected() {
        let mut graph = Graph::new();
        graph.add_system(dummy);
        let orphan = Node::System(SystemNode::new_boxed(Box::new(second.into_system())));
        let orphan_id = orphan.id();
        graph.nodes.push(orphan);

        let result = graph.validate();

        assert!(
            result.errors.iter().any(|err| matches!(
                err,
                ValidationError::DisconnectedNodes { orphan_count, orphans }
                    if *orphan_count == 1 && orphans == std::slice::from_ref(&orphan_id)
            )),
            "expected a DisconnectedNodes error naming the orphan, got: {:?}",
            result.errors
        );
    }

    /// Connectivity is meaningless when the entry itself is invalid, so the
    /// validator reports that root cause without labelling every real node an
    /// orphan as a secondary consequence.
    #[test]
    fn an_invalid_entry_does_not_report_disconnected_nodes() {
        let mut graph = Graph::new();
        graph.add_system(dummy);
        let invalid_entry = NodeId::from_string("missing-entry");
        graph.entry = Some(invalid_entry.clone());

        let result = graph.validate();

        assert!(
            result.errors.iter().any(
                |error| matches!(error, ValidationError::InvalidEntryPoint(id) if id == &invalid_entry)
            ),
            "expected the invalid entry point to be reported, got: {:?}",
            result.errors,
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|error| matches!(error, ValidationError::DisconnectedNodes { .. })),
            "connectivity should be skipped for an invalid entry, got: {:?}",
            result.errors,
        );
    }

    /// A handler subgraph is reachable only through its error/timeout edge —
    /// never a sequential one — and must not be reported as disconnected.
    #[test]
    fn a_handler_subgraph_is_not_reported_as_disconnected() {
        let mut graph = Graph::new();
        graph
            .system(dummy)
            .on_error(|h: &mut Graph| {
                h.add_system(second);
            })
            .done();

        assert!(
            graph.validate().is_ok(),
            "an error-handler subgraph is reachable via its error edge",
        );
    }

    /// A second outgoing sequential edge is unreachable by construction, so it
    /// is rejected at validate time rather than silently skipping its target.
    ///
    /// The shape needs `pub(crate)` access: no public builder method produces
    /// it, because every builder chains from `last_node` and then advances
    /// `last_node` to the node it just added.
    #[test]
    fn second_distinct_sequential_successor_is_rejected() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let followed = graph.add_system_node(second);
        let skipped = graph.add_system_node(third);
        // `third` was chained after `second`; re-point it at `first` as well,
        // which is the ambiguity.
        graph.add_sequential_edge(first.clone(), skipped.clone());

        let result = graph.validate();

        assert_eq!(
            ambiguities(&result),
            vec![(first, followed, skipped)],
            "the error names the offending node, the successor taken, and the one skipped",
        );
    }

    /// Every further distinct successor is reported, so one pass names the
    /// whole ambiguity rather than only its first half.
    #[test]
    fn every_skipped_successor_is_reported() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let followed = graph.add_system_node(second);
        let skipped_a = graph.add_system_node(third);
        let skipped_b = graph.add_system_node(fourth);
        graph.add_sequential_edge(first.clone(), skipped_a.clone());
        graph.add_sequential_edge(first.clone(), skipped_b.clone());

        assert_eq!(
            ambiguities(&graph.validate()),
            vec![
                (first.clone(), followed.clone(), skipped_a),
                (first, followed, skipped_b),
            ],
            "each skipped successor is reported against the one that is followed",
        );
    }

    /// Repeated edges to the *same* successor declare exactly the node the
    /// executor already follows, so the graph runs as written and the check
    /// must not reject it.
    #[test]
    fn repeated_edge_to_the_same_successor_is_accepted() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let only = graph.add_system_node(second);
        graph.add_sequential_edge(first, only);

        let result = graph.validate();
        assert!(
            result.is_ok(),
            "a redundant edge is not an ambiguity, got: {:?}",
            result.errors
        );
    }

    /// Error-route cardinality is based on distinct handlers, not duplicate
    /// edge declarations that all name the handler execution already follows.
    #[test]
    fn repeated_error_routes_to_the_same_handler_are_accepted() {
        let mut graph = Graph::new();
        let source = graph.add_system_node(dummy);
        let handler = Node::System(SystemNode::new_boxed(Box::new(second.into_system())));
        let handler_id = handler.id();
        graph.nodes.push(handler);
        graph.edges.push(Edge::Error(ErrorEdge::new(
            source.clone(),
            handler_id.clone(),
        )));
        graph
            .edges
            .push(Edge::Error(ErrorEdge::new(source, handler_id)));

        let result = graph.validate();
        assert!(
            result.is_ok(),
            "repeated error routes to one handler are not ambiguous, got: {:?}",
            result.errors,
        );
    }

    /// Timeout routes use the same distinct-target rule as success and error
    /// routes, so a duplicate edge to the effective handler remains valid.
    #[test]
    fn repeated_timeout_routes_to_the_same_handler_are_accepted() {
        let mut graph = Graph::new();
        let source = graph.add_system_node(dummy);
        graph.set_timeout(source.clone(), std::time::Duration::from_secs(1));
        let handler = Node::System(SystemNode::new_boxed(Box::new(second.into_system())));
        let handler_id = handler.id();
        graph.nodes.push(handler);
        graph.edges.push(Edge::Timeout(TimeoutEdge::new(
            source.clone(),
            handler_id.clone(),
        )));
        graph
            .edges
            .push(Edge::Timeout(TimeoutEdge::new(source, handler_id)));

        let result = graph.validate();
        assert!(
            result.is_ok(),
            "repeated timeout routes to one handler are not ambiguous, got: {:?}",
            result.errors,
        );
    }

    /// The mirror of the case above, for the *skipped* successor: naming one
    /// skipped node twice is still one piece of skipped work, so it is reported
    /// once rather than once per edge.
    #[test]
    fn a_repeated_edge_to_a_skipped_successor_is_reported_once() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let followed = graph.add_system_node(second);
        let skipped = graph.add_system_node(third);
        // Two further edges naming the same skipped successor.
        graph.add_sequential_edge(first.clone(), skipped.clone());
        graph.add_sequential_edge(first.clone(), skipped.clone());

        assert_eq!(
            ambiguities(&graph.validate()),
            vec![(first, followed, skipped)],
            "one skipped successor is one error, however many edges name it",
        );
    }

    /// The error carries the *names* of both successors, not only their IDs:
    /// node IDs render as opaque nanoids, so the names are what identify the
    /// skipped work to a reader.
    #[test]
    fn both_successors_are_reported_by_name() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let _followed = graph.add_system_node(second);
        let skipped = graph.add_system_node(third);
        graph.add_sequential_edge(first, skipped);

        assert_eq!(
            ambiguity_names(&graph.validate()),
            vec![("dummy".into(), "second".into(), "third".into())],
            "the offending node, the followed successor, and the skipped one are all named",
        );
    }

    /// A sequential edge to a node that does not exist in this graph is left
    /// out of the pass entirely. It is already reported as an invalid edge
    /// target, and crediting it as the followed successor would blame the real
    /// successor for being skipped in its place.
    #[test]
    fn edge_to_a_nonexistent_target_is_not_credited_as_followed() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        // Wired before the chain continues, so the bogus edge comes first in
        // edge order and would otherwise be taken as the followed successor.
        graph.add_sequential_edge(first, NodeId::from_string("ghost"));
        graph.add_system(second);

        let result = graph.validate();
        assert!(
            ambiguities(&result).is_empty(),
            "a nonexistent target is not an ambiguity, got: {:?}",
            result.errors,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| matches!(err, ValidationError::InvalidEdgeTarget { .. })),
            "the bogus edge is still reported as an invalid edge target, got: {:?}",
            result.errors,
        );
    }

    /// The mirror case for edge *sources*: reporting walks the node list, so
    /// edges leaving a node this graph does not own contribute nothing here.
    /// `validate_edge` already reports them as invalid edge sources, and this
    /// pass has no node to blame for them.
    #[test]
    fn edges_from_a_nonexistent_source_are_left_to_the_edge_check() {
        let mut graph = Graph::new();
        let followed = graph.add_system_node(dummy);
        let skipped = graph.add_system_node(second);
        // A ghost source with two distinct successors: the ambiguous shape,
        // but attached to a node that does not exist.
        let ghost = NodeId::from_string("ghost");
        graph.add_sequential_edge(ghost.clone(), followed);
        graph.add_sequential_edge(ghost, skipped);

        let result = graph.validate();
        assert!(
            ambiguities(&result).is_empty(),
            "a nonexistent source is not blamed for an ambiguity, got: {:?}",
            result.errors,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| matches!(err, ValidationError::InvalidEdgeSource { .. })),
            "the ghost edges are still reported as invalid edge sources, got: {:?}",
            result.errors,
        );
    }

    /// Two offending nodes in one graph are both reported, in node order, so
    /// the report is deterministic across sources and not only within one.
    #[test]
    fn ambiguities_from_several_nodes_are_reported_in_node_order() {
        let mut graph = Graph::new();
        let first = graph.add_system_node(dummy);
        let middle = graph.add_system_node(second);
        let last = graph.add_system_node(third);
        // Both `first` and `middle` gain a second distinct successor.
        graph.add_sequential_edge(first.clone(), last.clone());
        graph.add_sequential_edge(middle.clone(), first.clone());

        assert_eq!(
            ambiguities(&graph.validate()),
            vec![
                // `first` follows `middle` (edge 1) and skips `last` (edge 3).
                (first.clone(), middle.clone(), last.clone()),
                // `middle` follows `last` (edge 2) and skips `first` (edge 4).
                (middle, last, first),
            ],
            "both offending nodes are reported, ordered by node position",
        );
    }

    /// The check runs inside a scope's embedded graph too, and the error
    /// arrives wrapped in the scope envelope rather than being dropped.
    #[test]
    fn ambiguity_inside_a_scope_graph_is_reported_through_the_envelope() {
        let mut inner = Graph::new();
        let inner_first = inner.add_system_node(dummy);
        let _inner_followed = inner.add_system_node(second);
        let inner_skipped = inner.add_system_node(third);
        inner.add_sequential_edge(inner_first, inner_skipped);

        let mut graph = Graph::new();
        graph.add_system(dummy);
        graph.add_scope("scoped", inner, ContextPolicy::shared());

        let result = graph.validate();
        let nested: Vec<_> = result
            .errors
            .iter()
            .filter_map(|err| match err {
                ValidationError::ScopeGraphInvalid { inner, .. } => match inner.as_ref() {
                    ValidationError::AmbiguousRoute {
                        kind: RouteKind::Sequential,
                        name,
                        followed_name,
                        skipped_name,
                        ..
                    } => Some((name.clone(), followed_name.clone(), skipped_name.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            nested,
            vec![("dummy".into(), "second".into(), "third".into())],
            "the scope graph's ambiguity is reported through ScopeGraphInvalid, naming the \
             inner nodes rather than the scope, got: {:?}",
            result.errors,
        );
    }

    /// The dynamic envelope is the other half of the pair: an ambiguity inside
    /// an inline candidate arrives wrapped in `DynamicCandidateInvalid`, keyed
    /// by the candidate it came from.
    #[test]
    fn ambiguity_inside_a_dynamic_candidate_is_reported_through_the_envelope() {
        let mut candidate = Graph::new();
        let candidate_first = candidate.add_system_node(dummy);
        let _candidate_followed = candidate.add_system_node(second);
        let candidate_skipped = candidate.add_system_node(third);
        candidate.add_sequential_edge(candidate_first, candidate_skipped);

        let mut graph = Graph::new();
        graph.add_system(dummy);
        graph.add_dynamic(
            "slot",
            |_ctx| "only",
            [("only", candidate)],
            DynamicSlot::new(GraphSignature::new(), ContextPolicy::shared()),
        );

        let result = graph.validate();
        let nested: Vec<_> = result
            .errors
            .iter()
            .filter_map(|err| match err {
                ValidationError::DynamicCandidateInvalid { key, inner, .. } => {
                    match inner.as_ref() {
                        ValidationError::AmbiguousRoute {
                            kind: RouteKind::Sequential,
                            skipped_name,
                            ..
                        } => Some((key.as_ref(), skipped_name.clone())),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            nested,
            vec![("only", "third".into())],
            "the candidate's ambiguity is reported through DynamicCandidateInvalid, naming \
             the candidate key and the skipped node, got: {:?}",
            result.errors,
        );
    }

    /// The error outcome carries the same rule: error lookup is first-match
    /// too, so a second handler wired to the same node is never reached.
    #[test]
    fn a_second_error_handler_for_one_node_is_rejected() {
        let mut graph = Graph::new();
        let risky = graph.add_system_node(dummy);
        graph.add_error_handler_for([risky.clone()], |handler| {
            handler.add_system(second);
        });
        graph.add_error_handler_for([risky], |handler| {
            handler.add_system(third);
        });

        let result = graph.validate();
        assert_eq!(
            route_names(&result, RouteKind::Error),
            vec![("dummy".into(), "second".into(), "third".into())],
            "the second error handler is reported as an unreachable error route",
        );
        assert!(
            ambiguities(&result).is_empty(),
            "the sequential outcome is untouched by an error-route conflict, got: {:?}",
            result.errors,
        );
    }

    /// The timeout outcome behaves identically, through its own builder.
    #[test]
    fn a_second_timeout_handler_for_one_node_is_rejected() {
        let mut graph = Graph::new();
        let slow = graph.add_system_node(dummy);
        graph.add_timeout_handler([slow.clone()], |handler| {
            handler.add_system(second);
        });
        graph.add_timeout_handler([slow], |handler| {
            handler.add_system(third);
        });

        let result = graph.validate();
        assert_eq!(
            route_names(&result, RouteKind::Timeout),
            vec![("dummy".into(), "second".into(), "third".into())],
            "the second timeout handler is reported as an unreachable timeout route",
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.to_string().contains("distinct timeout route")),
            "the rendered message names the timeout outcome, got: {:?}",
            result.errors,
        );
    }

    /// Outcomes are counted independently: one route each for success, error,
    /// and timeout is the normal shape and must stay clean.
    #[test]
    fn one_route_per_outcome_is_accepted() {
        let mut graph = Graph::new();
        let risky = graph.add_system_node(dummy);
        graph.add_error_handler_for([risky.clone()], |handler| {
            handler.add_system(second);
        });
        graph.add_timeout_handler([risky], |handler| {
            handler.add_system(third);
        });
        graph.add_system(fourth);

        let result = graph.validate();
        assert!(
            result.is_ok(),
            "a node may have one route per outcome, got: {:?}",
            result.errors
        );
    }

    /// Fan-*in* is legitimate — the constraint is one successor per node, not
    /// one predecessor.
    #[test]
    fn several_predecessors_of_one_node_are_accepted() {
        let mut graph = Graph::new();
        graph.add_system(dummy);
        let shared = graph.add_system_node(second);
        let other = graph.add_system_node(third);
        // `other` was chained after `shared`; point it back at `shared` so two
        // distinct nodes name the same successor.
        graph.add_sequential_edge(other, shared);

        let result = graph.validate();
        assert!(
            result.is_ok(),
            "two nodes may share a successor, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn scope_graph_no_entry_point() {
        // Construct a Graph with nodes but no entry point.
        // This can only happen through pub(crate) access since all public
        // builder methods set the entry on the first node.
        let mut inner = Graph::default();
        let node = Node::System(SystemNode::new(dummy.into_system()));
        inner.nodes.push(node);
        // entry remains None, but is_empty() is false

        let mut graph = Graph::new();
        graph.add_scope("no_entry_scope", inner, ContextPolicy::shared());

        let result = graph.validate();
        assert!(
            result
                .errors
                .iter()
                .any(|err| matches!(err, ValidationError::ScopeGraphNoEntryPoint { .. })),
            "expected ScopeGraphNoEntryPoint error, got: {:?}",
            result.errors
        );
    }
}
