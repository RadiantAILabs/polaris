//! Graph validation logic and error types.

use super::Graph;
use crate::edge::{Edge, EdgeId};
use crate::node::{Node, NodeId};
use hashbrown::{HashMap, HashSet};
use polaris_system::param::ERROR_CONTEXT;
use std::any::TypeId;
use std::fmt;

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
    /// - Checks all edges reference valid nodes
    /// - Ensures decision nodes have predicates and both branches
    /// - Ensures loop nodes have termination conditions
    /// - Ensures parallel nodes have branches (subgraphs)
    /// - Ensures switch nodes have discriminators
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
                        name: dec.name,
                    });
                }
                if dec.true_branch.is_none() {
                    errors.push(ValidationError::MissingBranch {
                        node: dec.id.clone(),
                        name: dec.name,
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
                        name: dec.name,
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
                        name: sw.name,
                    });
                }
                if sw.cases.is_empty() && sw.default.is_none() {
                    errors.push(ValidationError::EmptySwitch {
                        node: sw.id.clone(),
                        name: sw.name,
                    });
                }
                for (case_name, target) in &sw.cases {
                    if !valid_nodes.contains(target) {
                        errors.push(ValidationError::InvalidCaseTarget {
                            node: sw.id.clone(),
                            case: case_name,
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
                        name: par.name,
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
                            name: par.name,
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
                        name: lp.name,
                    });
                }
                if lp.body_entry.is_none() {
                    errors.push(ValidationError::EmptyLoopBody {
                        node: lp.id.clone(),
                        name: lp.name,
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
                // check that the predicate's input type is actually produced
                // by a system in the loop body.
                if let Some(term) = &lp.termination
                    && let Some(body) = &lp.body_entry
                    && valid_nodes.contains(body)
                {
                    let predicate_input = term.input_type_id();
                    let body_output_types: HashSet<TypeId> = self
                        .collect_branch_output_types(body)
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                    if !body_output_types.contains(&predicate_input) {
                        errors.push(ValidationError::LoopPredicateOutputNotProduced {
                            node: lp.id.clone(),
                            name: lp.name,
                            expected_output: term.input_type_name(),
                        });
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
pub enum ValidationWarning {
    /// Two or more parallel branches produce the same output type.
    /// The last branch in declaration order will win at merge time.
    ConflictingParallelOutputs {
        /// The parallel node ID.
        node: NodeId,
        /// The parallel node name.
        name: &'static str,
        /// The conflicting output type name.
        output_type: &'static str,
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
        }
    }
}

impl std::error::Error for ValidationWarning {}

/// Errors that can occur during graph validation.
///
/// These errors are detected at build time (when calling [`Graph::validate`])
/// before the graph is executed, allowing early detection of structural issues.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// The graph has no entry point.
    NoEntryPoint,
    /// The entry point references an invalid node.
    InvalidEntryPoint(NodeId),
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
    /// A decision node is missing its predicate.
    MissingPredicate {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A decision node is missing a branch target.
    MissingBranch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
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
        name: &'static str,
    },
    /// A switch node has no cases and no default.
    EmptySwitch {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A switch case target references an invalid node.
    InvalidCaseTarget {
        /// The node ID.
        node: NodeId,
        /// The case name.
        case: &'static str,
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
        name: &'static str,
    },
    /// A loop node has no termination condition.
    NoTerminationCondition {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A loop node has no body.
    EmptyLoopBody {
        /// The node ID.
        node: NodeId,
        /// The node name.
        name: &'static str,
    },
    /// A loop body entry references an invalid node.
    InvalidLoopBody {
        /// The node ID.
        node: NodeId,
        /// The invalid target node ID.
        target: NodeId,
    },
    /// A loop's termination predicate reads an output type that no system in
    /// the loop body produces.
    LoopPredicateOutputNotProduced {
        /// The loop node ID.
        node: NodeId,
        /// The loop node name.
        name: &'static str,
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
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::NoEntryPoint => write!(f, "graph has no entry point"),
            ValidationError::InvalidEntryPoint(id) => {
                write!(f, "entry point references invalid node: {id}")
            }
            ValidationError::InvalidEdgeSource { edge, node } => {
                write!(f, "edge {edge} has invalid source node: {node}")
            }
            ValidationError::InvalidEdgeTarget { edge, node } => {
                write!(f, "edge {edge} has invalid target node: {node}")
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
                    "loop node '{name}' ({node}) predicate expects output type '{expected_output}' not produced by any system in the loop body"
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
