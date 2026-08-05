//! Aggregate IO signatures for subgraphs, used to type-check dynamic subgraph
//! selection against a slot contract.
//!
//! A [`GraphSignature`] is the input/output interface a subgraph presents to its
//! surroundings: the free inputs it needs from the parent context — required
//! resources (`requires`) and required free outputs (`requires_outputs`) — and
//! the output types it produces and merges back (`produces`). The
//! [`Dynamic`](crate::node::Node::Dynamic) node declares a *slot* signature and
//! only ever runs a candidate whose signature is [compatible](GraphSignature::compatible_with)
//! with it — so composition stays sound even when the concrete subgraph is
//! chosen (or swapped) at runtime.
//!
//! # Two comparison semantics
//!
//! Signatures are compared under two relations, each named for its soundness
//! envelope:
//!
//! - [`compatible_with`](GraphSignature::compatible_with) — **exact-set
//!   match**, used for [`Dynamic`](crate::node::Node::Dynamic) slot
//!   substitution, where the parent graph was validated against the slot's
//!   precise interface and any divergence could break it.
//! - [`satisfies`](GraphSignature::satisfies) — **subsumption**, used for
//!   capability matching (e.g. a sessions contract registry asking "is this
//!   agent a knowledge accumulator?"): the candidate may require *more* than
//!   the slot names (extra requires are presumed environment-provided by the
//!   registrant's setup) and produce *more* (extra outputs merge back
//!   harmlessly), but may not read free outputs the slot never sanctioned.
//!
//! Each relation ships with a lockstep diff mode ([`diff`](GraphSignature::diff)
//! / [`satisfies_diff`](GraphSignature::satisfies_diff)) so a rejection reads
//! as an edit rather than a puzzle.
//!
//! # Conservative resource handling (v1)
//!
//! [`Graph::signature`] aggregates each system's declared [`SystemAccess`]. It
//! cannot see imperative `ctx.insert` calls, so it treats *every* non-global
//! resource read as required-from-parent. This over-declares (a candidate that
//! self-inserts a resource still reports it as required) but is never unsound.
//!
//! Free-output (`requires_outputs`) derivation is likewise conservative: it
//! walks the graph in execution order and credits a `Out<T>` read as internally
//! satisfied only when a producer of `T` is *guaranteed* to have run first — a
//! system earlier on the sequential chain. Outputs produced only inside a
//! branch (decision / switch / loop / parallel) are never credited, because the
//! branch may not run, so a downstream read of such an output stays a required
//! free input. Decision predicates and switch discriminators read an output at
//! runtime too, so their input types are counted as reads at the node's
//! position. The derived signature therefore never claims *less* than the
//! runtime truth — the only direction a soundness interface may err.
//!
//! Error and timeout **handler subgraphs** are part of the interface: a handler
//! runs on the failure path and its outputs merge back like any other, so
//! handler systems contribute to `produces` and `requires`, and their output
//! reads are classified against what is guaranteed *before* the node the
//! handler is attached to (the failing node's own output is never credited —
//! it did not complete).
//!
//! The unit type `()` is never part of an interface: a `()`-returning system
//! communicates through resources, not outputs, so `()` is filtered out of both
//! `produces` and `requires_outputs`.
//!
//! Derivation is sound over **declared** access: it reads each system's
//! [`SystemAccess`] (and each predicate's input type), which the `#[system]`
//! macro derives from the parameter list. A hand-implemented [`System`] whose
//! `access()` under-declares what `run` actually touches escapes the contract —
//! runtime resolution is not gated by declarations.
//!
//! [`SystemAccess`]: polaris_system::param::SystemAccess
//! [`System`]: polaris_system::system::System

use super::Graph;
use crate::edge::Edge;
use crate::node::{Node, NodeId};
use hashbrown::{HashMap, HashSet};
use polaris_system::param::{Access, AccessMode};
use std::any::TypeId;
use std::fmt;

/// The aggregate input/output interface of a subgraph.
///
/// - `requires` — non-global resources (`Res<T>` / `ResMut<T>`) the subgraph
///   reads from the parent context.
/// - `requires_outputs` — free outputs (`Out<T>`) the subgraph reads but no
///   system inside it produces, so the parent must supply them.
/// - `produces` — the output types the subgraph's systems produce, which the
///   scope boundary merges back into the parent on exit.
///
/// Resources and outputs are kept apart (mirroring
/// [`SystemAccess`](polaris_system::param::SystemAccess), where they live in
/// separate lists and never cross-conflict) so a required `ResMut<T>` and a
/// required `Out<T>` of the same type stay distinct in the contract.
///
/// Obtain one from a built graph with [`Graph::signature`], or declare a slot
/// contract by hand with the builder methods. Two signatures compare as **sets**
/// — order and duplicate declarations are normalized away — so
/// [`compatible_with`](GraphSignature::compatible_with) is a stable check.
///
/// # Example
///
/// ```
/// use polaris_graph::GraphSignature;
///
/// struct Input;
/// struct Reply;
///
/// // A slot that reads `Input` and produces a `Reply`.
/// let slot = GraphSignature::new().require_read::<Input>().produce::<Reply>();
///
/// // A candidate with the same interface is admissible. Declaration order is
/// // normalized away, so the sets still match.
/// let candidate = GraphSignature::new().produce::<Reply>().require_read::<Input>();
/// assert!(candidate.compatible_with(&slot));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphSignature {
    requires: Vec<Access>,
    requires_outputs: Vec<Access>,
    produces: Vec<Access>,
}

impl GraphSignature {
    /// Creates an empty signature (no inputs, no outputs).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a required non-global resource read (`Res<T>`).
    #[must_use]
    pub fn require_read<T: 'static>(mut self) -> Self {
        self.requires.push(Access::read::<T>());
        canonicalize(&mut self.requires);
        self
    }

    /// Declares a required non-global resource write (`ResMut<T>`).
    #[must_use]
    pub fn require_write<T: 'static>(mut self) -> Self {
        self.requires.push(Access::write::<T>());
        canonicalize(&mut self.requires);
        self
    }

    /// Declares a required free output (`Out<T>` the parent must supply).
    ///
    /// Distinct from [`require_write`](Self::require_write): that declares a
    /// non-global resource (`ResMut<T>`) the subgraph mutates, whereas this
    /// declares a previous system's return value (`Out<T>`) the subgraph reads.
    /// The two are tracked in separate lists, so requiring `ResMut<T>` and
    /// requiring `Out<T>` of the same type produce different signatures.
    ///
    /// The stored [`Access`] carries `AccessMode::Write` as a representation
    /// detail; the mode has no meaning on this axis (see
    /// [`requires_outputs`](Self::requires_outputs)).
    #[must_use]
    pub fn require_output<T: 'static>(mut self) -> Self {
        self.requires_outputs.push(Access::write::<T>());
        canonicalize(&mut self.requires_outputs);
        self
    }

    /// Declares an output the subgraph produces.
    ///
    /// The unit type `()` is never part of a data-flow interface — a
    /// `()`-returning system communicates through resources, not outputs — so
    /// [`Graph::signature`] filters `()` out of derived signatures. A
    /// hand-written `produce::<()>()` is therefore honored as declared but will
    /// never match a *derived* signature; prefer omitting it.
    #[must_use]
    pub fn produce<T: 'static>(mut self) -> Self {
        self.produces.push(Access::write::<T>());
        canonicalize(&mut self.produces);
        self
    }

    /// The non-global resources this subgraph reads from the parent context.
    #[must_use]
    pub fn requires(&self) -> &[Access] {
        &self.requires
    }

    /// The free outputs (`Out<T>`) this subgraph reads but does not produce, so
    /// the parent must supply them.
    ///
    /// Each entry's [`AccessMode`](polaris_system::param::AccessMode) is a
    /// representation detail with no meaning on this axis — output reads are
    /// just reads; there is no read/write distinction to encode. Compare
    /// entries by `type_id` only, and don't infer anything from the stored
    /// mode. (Rendered forms show these entries as `Out<T>` for the same
    /// reason.)
    #[must_use]
    pub fn requires_outputs(&self) -> &[Access] {
        &self.requires_outputs
    }

    /// The output types this subgraph produces.
    #[must_use]
    pub fn produces(&self) -> &[Access] {
        &self.produces
    }

    /// Converts the signature into human-readable type-name strings.
    ///
    /// Each axis is rendered in parameter terms — `Res<T>` / `ResMut<T>` for
    /// resources, `Out<T>` for outputs — in canonical (sorted, deduplicated)
    /// order. See [`RenderedSignature`] for why the result is display-only
    /// and deliberately not convertible back into a `GraphSignature`.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::GraphSignature;
    ///
    /// let rendered = GraphSignature::new()
    ///     .require_read::<u8>()
    ///     .produce::<u16>()
    ///     .to_rendered();
    ///
    /// assert_eq!(rendered.requires, vec!["Res<u8>"]);
    /// assert_eq!(rendered.produces, vec!["Out<u16>"]);
    /// ```
    #[must_use]
    pub fn to_rendered(&self) -> RenderedSignature {
        RenderedSignature {
            requires: self.requires.iter().map(render_resource).collect(),
            requires_outputs: self.requires_outputs.iter().map(render_output).collect(),
            produces: self.produces.iter().map(render_output).collect(),
        }
    }

    /// Returns `true` if a candidate carrying `self` may fill a slot declaring
    /// `slot`.
    ///
    /// **v1: exact-set match** — the candidate must require exactly the resources
    /// and free outputs the slot requires and produce exactly what the slot
    /// produces. This is the allocation-free equivalent of
    /// `self.diff(slot).is_empty()`: signatures are always canonicalized (sorted
    /// and de-duplicated by `(type_id, mode)`), so per-axis `Vec` equality *is*
    /// set equality and the answer never depends on declaration order. Reach for
    /// [`diff`](Self::diff) when you need *why* two signatures diverge — it builds
    /// the per-axis difference for error messages; reach for this on the hot path
    /// (e.g. the executor's per-execution registry lookup) where only the yes/no
    /// matters and no allocation should happen on the compatible case.
    ///
    /// The `compatible_with` ⇔ empty-[`diff`](Self::diff) equivalence is a
    /// load-bearing invariant (asserted in tests): a future variance relaxation
    /// (accept a subset of `requires`, a superset of `produces`) must relax both
    /// in lockstep so the error messages stay correct.
    ///
    /// For capability matching — where a candidate carrying *more* than the
    /// slot names should still qualify — use [`satisfies`](Self::satisfies)
    /// instead; the two relations are deliberately separate, each named for
    /// its soundness envelope.
    #[must_use]
    pub fn compatible_with(&self, slot: &GraphSignature) -> bool {
        self.requires == slot.requires
            && access_types_equal(&self.requires_outputs, &slot.requires_outputs)
            && self.produces == slot.produces
    }

    /// Returns `true` if a candidate carrying `self` **satisfies** a `slot`
    /// contract by subsumption.
    ///
    /// Unlike the exact-set [`compatible_with`](Self::compatible_with) (used
    /// for [`Dynamic`](crate::node::Node::Dynamic) slot substitution), this is
    /// the *capability* relation: it asks whether the candidate can do at
    /// least what the slot describes, in an environment its registrant
    /// controls. Per axis:
    ///
    /// - the slot's `requires` must be a **subset** of the candidate's —
    ///   extra candidate requires are presumed environment-provided (e.g. a
    ///   resource the agent's own `setup` inserts, which conservative
    ///   derivation still reports as required);
    /// - the candidate's `requires_outputs` must not exceed the slot's — a
    ///   free `Out<T>` read the slot never sanctioned would dangle at runtime;
    /// - the slot's `produces` must be a **subset** of the candidate's —
    ///   extra candidate outputs merge back harmlessly.
    ///
    /// Subsumption trusts the registrant's environment claim: a candidate
    /// whose extra `requires` nothing actually provides still satisfies the
    /// slot here and fails loudly on first execution — the same honor-system
    /// boundary as declared access generally.
    ///
    /// The `satisfies` ⇔ empty-[`satisfies_diff`](Self::satisfies_diff)
    /// equivalence is a load-bearing invariant (asserted in tests), mirroring
    /// the [`compatible_with`](Self::compatible_with) ⇔ [`diff`](Self::diff)
    /// lockstep: any relaxation must move both in step so error messages stay
    /// correct.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::GraphSignature;
    ///
    /// let contract = GraphSignature::new()
    ///     .require_read::<u8>()
    ///     .produce::<u16>();
    /// let candidate = GraphSignature::new()
    ///     .require_read::<u8>()
    ///     .require_read::<u32>()
    ///     .produce::<u16>()
    ///     .produce::<u64>();
    ///
    /// assert!(candidate.satisfies(&contract));
    /// ```
    #[must_use]
    pub fn satisfies(&self, slot: &GraphSignature) -> bool {
        is_subset(&slot.requires, &self.requires)
            && is_type_subset(&self.requires_outputs, &slot.requires_outputs)
            && is_subset(&slot.produces, &self.produces)
    }

    /// Computes the subsumption violations between this candidate signature
    /// and a `slot` contract — the diff mode of [`satisfies`](Self::satisfies).
    ///
    /// Only the directions that *violate* subsumption are populated:
    /// `missing_requires` (slot requires the candidate lacks),
    /// `extra_requires_outputs` (free output reads the slot never
    /// sanctioned), and `missing_produces` (slot outputs the candidate does
    /// not produce). Divergence the relation permits — extra requires, fewer
    /// free output reads, extra produces — never appears. An empty diff means
    /// the candidate [satisfies](Self::satisfies) the slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::GraphSignature;
    ///
    /// let contract = GraphSignature::new().produce::<u16>();
    /// let candidate = GraphSignature::new();
    ///
    /// let diff = candidate.satisfies_diff(&contract);
    /// assert!(!diff.is_empty());
    /// assert_eq!(diff.missing_produces().len(), 1);
    /// ```
    #[must_use]
    pub fn satisfies_diff(&self, slot: &GraphSignature) -> SignatureDiff {
        SignatureDiff {
            missing_requires: access_difference(&slot.requires, &self.requires),
            extra_requires: Vec::new(),
            missing_requires_outputs: Vec::new(),
            extra_requires_outputs: access_type_difference(
                &self.requires_outputs,
                &slot.requires_outputs,
            ),
            missing_produces: access_difference(&slot.produces, &self.produces),
            extra_produces: Vec::new(),
        }
    }

    /// Computes the per-axis difference between this candidate signature and a
    /// `slot` contract.
    ///
    /// The result names *which* types diverge on *which* axis and in *which*
    /// direction — what the slot demands that the candidate lacks
    /// (`missing_*`), and what the candidate declares that the slot did not
    /// (`extra_*`). An empty diff means the candidate
    /// [is compatible](Self::compatible_with) with the slot.
    #[must_use]
    pub fn diff(&self, slot: &GraphSignature) -> SignatureDiff {
        SignatureDiff {
            missing_requires: access_difference(&slot.requires, &self.requires),
            extra_requires: access_difference(&self.requires, &slot.requires),
            missing_requires_outputs: access_type_difference(
                &slot.requires_outputs,
                &self.requires_outputs,
            ),
            extra_requires_outputs: access_type_difference(
                &self.requires_outputs,
                &slot.requires_outputs,
            ),
            missing_produces: access_difference(&slot.produces, &self.produces),
            extra_produces: access_difference(&self.produces, &slot.produces),
        }
    }
}

impl fmt::Display for GraphSignature {
    /// Renders the full interface on one line, all three axes in canonical
    /// order — e.g. `requires: [Res<TraceLine>]; requires_outputs: [];
    /// produces: [Out<Triples>]`. The same advisory-only caveats as
    /// [`to_rendered`](GraphSignature::to_rendered) apply.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requires: [{}]; requires_outputs: [{}]; produces: [{}]",
            render_list(&self.requires, render_resource),
            render_list(&self.requires_outputs, render_output),
            render_list(&self.produces, render_output),
        )
    }
}

/// A human-readable rendering of a [`GraphSignature`], produced by
/// [`GraphSignature::to_rendered`].
///
/// Each axis holds rendered type-name strings in parameter terms —
/// `Res<T>` / `ResMut<T>` for resources, `Out<T>` for outputs.
///
/// This form is **advisory and display-only, by construction**: it carries no
/// `TypeId`s, so it cannot be turned back into a `GraphSignature` and cannot
/// participate in [`compatible_with`](GraphSignature::compatible_with) /
/// [`satisfies`](GraphSignature::satisfies) — equality and compatibility
/// checks stay in-process, on the typed signature. That is deliberate:
/// type-name strings do not unify across separately compiled binaries, so a
/// wire form built from them must not masquerade as a checkable contract.
/// The rendered text is also unstable — [`std::any::type_name`] makes no
/// format guarantee — so treat it as documentation for humans, never parse it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderedSignature {
    /// Rendered `requires` axis (`Res<T>` / `ResMut<T>` entries).
    pub requires: Vec<String>,
    /// Rendered `requires_outputs` axis (`Out<T>` entries).
    pub requires_outputs: Vec<String>,
    /// Rendered `produces` axis (`Out<T>` entries).
    pub produces: Vec<String>,
}

/// The per-axis difference between a candidate [`GraphSignature`] and a slot
/// contract, as computed by [`GraphSignature::diff`] (exact-match mode) or
/// [`GraphSignature::satisfies_diff`] (subsumption mode, violating
/// directions only).
///
/// Each axis lists the [`Access`] entries the two signatures disagree on, split
/// by direction:
///
/// - `missing_*` — the slot declares it but the candidate does not (the
///   candidate lacks something the slot demands).
/// - `extra_*` — the candidate declares it but the slot does not (the candidate
///   demands or produces something the slot never sanctioned).
///
/// A diff empty on every axis means the check that produced it passed —
/// [`GraphSignature::compatible_with`] for [`diff`](GraphSignature::diff),
/// [`GraphSignature::satisfies`] for
/// [`satisfies_diff`](GraphSignature::satisfies_diff). [`Display`](fmt::Display)
/// renders only the non-empty axes, in human terms (`Res<T>` / `ResMut<T>` for
/// resources, `Out<T>` for outputs).
///
/// # Example
///
/// ```
/// use polaris_graph::GraphSignature;
///
/// struct Input;
/// struct Reply;
///
/// let slot = GraphSignature::new().require_read::<Input>().produce::<Reply>();
/// let candidate = GraphSignature::new().produce::<Reply>(); // forgot the read
///
/// let diff = candidate.diff(&slot);
/// assert!(!diff.is_empty());
/// assert_eq!(diff.missing_requires().len(), 1); // slot reads Input, candidate does not
/// assert!(diff.extra_requires().is_empty());
/// assert!(diff.missing_produces().is_empty()); // both produce Reply
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureDiff {
    missing_requires: Vec<Access>,
    extra_requires: Vec<Access>,
    missing_requires_outputs: Vec<Access>,
    extra_requires_outputs: Vec<Access>,
    missing_produces: Vec<Access>,
    extra_produces: Vec<Access>,
}

impl SignatureDiff {
    /// Returns `true` if the signatures match on every axis (no missing or extra
    /// entries anywhere).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing_requires.is_empty()
            && self.extra_requires.is_empty()
            && self.missing_requires_outputs.is_empty()
            && self.extra_requires_outputs.is_empty()
            && self.missing_produces.is_empty()
            && self.extra_produces.is_empty()
    }

    /// Resource reads (`Res<T>` / `ResMut<T>`) the slot requires that the
    /// candidate does not.
    #[must_use]
    pub fn missing_requires(&self) -> &[Access] {
        &self.missing_requires
    }

    /// Resource reads (`Res<T>` / `ResMut<T>`) the candidate requires that the
    /// slot does not.
    #[must_use]
    pub fn extra_requires(&self) -> &[Access] {
        &self.extra_requires
    }

    /// Free outputs (`Out<T>`) the slot requires that the candidate does not.
    #[must_use]
    pub fn missing_requires_outputs(&self) -> &[Access] {
        &self.missing_requires_outputs
    }

    /// Free outputs (`Out<T>`) the candidate requires that the slot does not.
    #[must_use]
    pub fn extra_requires_outputs(&self) -> &[Access] {
        &self.extra_requires_outputs
    }

    /// Outputs (`Out<T>`) the slot produces that the candidate does not.
    #[must_use]
    pub fn missing_produces(&self) -> &[Access] {
        &self.missing_produces
    }

    /// Outputs (`Out<T>`) the candidate produces that the slot does not.
    #[must_use]
    pub fn extra_produces(&self) -> &[Access] {
        &self.extra_produces
    }
}

impl fmt::Display for SignatureDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("no signature differences");
        }
        let mut clauses: Vec<String> = Vec::new();
        push_axis(
            &mut clauses,
            "requires",
            &self.missing_requires,
            &self.extra_requires,
            render_resource,
        );
        push_axis(
            &mut clauses,
            "requires_outputs",
            &self.missing_requires_outputs,
            &self.extra_requires_outputs,
            render_output,
        );
        push_axis(
            &mut clauses,
            "produces",
            &self.missing_produces,
            &self.extra_produces,
            render_output,
        );
        write!(f, "signature mismatch — {}", clauses.join("; "))
    }
}

/// Returns `true` if every element of `sub` (by `(type_id, mode)`) is present
/// in `sup`. Allocation-free equivalent of `access_difference(sub, sup)` being
/// empty; both inputs are canonicalized and tiny, so the quadratic scan is
/// cheaper than hashing.
fn is_subset(sub: &[Access], sup: &[Access]) -> bool {
    sub.iter().all(|access| {
        sup.iter()
            .any(|other| access.type_id == other.type_id && access.mode == other.mode)
    })
}

/// Returns `true` if two access slices name the same set of types, ignoring
/// access mode. Used for `requires_outputs`, where access mode is only a
/// storage detail.
fn access_types_equal(left: &[Access], right: &[Access]) -> bool {
    is_type_subset(left, right) && is_type_subset(right, left)
}

/// Returns `true` if every type in `sub` is present in `sup`, ignoring access
/// mode.
fn is_type_subset(sub: &[Access], sup: &[Access]) -> bool {
    sub.iter()
        .all(|access| sup.iter().any(|other| access.type_id == other.type_id))
}

/// Elements of `from` (by `(type_id, mode)`) not present in `remove`. Both
/// inputs are canonicalized, so the result keeps canonical order.
fn access_difference(from: &[Access], remove: &[Access]) -> Vec<Access> {
    from.iter()
        .filter(|access| {
            !remove
                .iter()
                .any(|other| access.type_id == other.type_id && access.mode == other.mode)
        })
        .cloned()
        .collect()
}

/// Elements of `from` whose type is not present in `remove`, ignoring access
/// mode. Used for `requires_outputs`, where access mode is only a storage
/// detail.
fn access_type_difference(from: &[Access], remove: &[Access]) -> Vec<Access> {
    from.iter()
        .filter(|access| !remove.iter().any(|other| access.type_id == other.type_id))
        .cloned()
        .collect()
}

/// Appends a `"{axis}: ..."` clause to `clauses` for one signature axis, unless
/// both directions are empty.
fn push_axis(
    clauses: &mut Vec<String>,
    axis: &str,
    missing: &[Access],
    extra: &[Access],
    render: fn(&Access) -> String,
) {
    if missing.is_empty() && extra.is_empty() {
        return;
    }
    let mut segments = Vec::new();
    if !missing.is_empty() {
        segments.push(format!("candidate lacks {}", render_list(missing, render)));
    }
    if !extra.is_empty() {
        segments.push(format!(
            "candidate additionally has {}",
            render_list(extra, render)
        ));
    }
    clauses.push(format!("{axis}: {}", segments.join(", ")));
}

/// Renders a list of accesses as a comma-separated string via `render`.
fn render_list(list: &[Access], render: fn(&Access) -> String) -> String {
    list.iter().map(render).collect::<Vec<_>>().join(", ")
}

/// Renders a resource access as `Res<T>` or `ResMut<T>`.
fn render_resource(access: &Access) -> String {
    match access.mode {
        AccessMode::Read => format!("Res<{}>", access.type_name),
        AccessMode::Write => format!("ResMut<{}>", access.type_name),
    }
}

/// Renders an output access as `Out<T>`.
fn render_output(access: &Access) -> String {
    format!("Out<{}>", access.type_name)
}

/// Sorts and de-duplicates an access list by `(type_id, mode)` so that a
/// signature has one canonical representation — making `==` set equality.
fn canonicalize(accesses: &mut Vec<Access>) {
    accesses.sort_by_key(|access| (access.type_id, mode_rank(access.mode)));
    accesses.dedup_by(|a, b| a.type_id == b.type_id && a.mode == b.mode);
}

/// Total order on [`AccessMode`] for canonical sorting.
fn mode_rank(mode: AccessMode) -> u8 {
    match mode {
        AccessMode::Read => 0,
        AccessMode::Write => 1,
    }
}

impl Graph {
    /// Returns the aggregate IO [`GraphSignature`] of the subgraph reachable from
    /// this graph's entry point.
    ///
    /// - `produces` is the union of every reachable system's output type — what
    ///   *may* merge back — excluding the unit type `()`. Systems reachable only
    ///   through an error/timeout handler edge count: their outputs merge back
    ///   on the failure path.
    /// - `requires` is every non-global resource read, handler systems included.
    /// - `requires_outputs` is every `Out<T>` read (excluding `()`) that is not
    ///   *guaranteed* to be produced upstream. It is derived in execution order
    ///   along the sequential chain: a read is free unless a producer of its
    ///   type ran earlier on that chain. Outputs produced only inside a branch
    ///   (decision / switch / loop / parallel) are never credited, so a read of
    ///   such an output stays free. Decision predicates and switch
    ///   discriminators count as reads of their input type; handler-subgraph
    ///   reads are classified against what is guaranteed before the node the
    ///   handler is attached to.
    ///
    /// An empty graph has an empty signature. Embedded
    /// [`Scope`](crate::node::Node::Scope) and
    /// [`Dynamic`](crate::node::Node::Dynamic) nodes are opaque: their inner
    /// systems do not contribute to the signature (their IO crosses a separate
    /// boundary).
    ///
    /// Derivation is sound over **declared** access: it reads each system's
    /// [`SystemAccess`](polaris_system::param::SystemAccess) (and each
    /// predicate's input type). A hand-implemented
    /// [`System`](polaris_system::system::System) whose `access()`
    /// under-declares what `run` actually touches escapes the contract —
    /// runtime resolution is not gated by declarations.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::{Graph, GraphSignature};
    ///
    /// async fn plan() -> i32 { 1 }
    ///
    /// let mut graph = Graph::new();
    /// graph.add_system(plan);
    ///
    /// // The derived signature is the graph's IO interface: `plan` reads
    /// // nothing and produces an `i32`.
    /// let derived = graph.signature();
    /// assert!(derived.compatible_with(&GraphSignature::new().produce::<i32>()));
    /// ```
    #[must_use]
    pub fn signature(&self) -> GraphSignature {
        let Some(entry) = self.entry() else {
            return GraphSignature::default();
        };
        let nodes = self.reachable_nodes_with_handlers(&entry);
        let unit = TypeId::of::<()>();

        // produces: one entry per distinct system output type (unit filtered).
        let mut produced: HashSet<TypeId> = HashSet::new();
        let mut produces = Vec::new();
        for node in &nodes {
            if let Node::System(sys) = node {
                let type_id = sys.output_type_id();
                if type_id != unit && produced.insert(type_id) {
                    produces.push(Access {
                        type_id,
                        type_name: sys.output_type_name(),
                        mode: AccessMode::Write,
                        is_global: false,
                    });
                }
            }
        }

        // requires: non-global resource reads across the whole subgraph.
        let mut requires = Vec::new();
        for node in &nodes {
            if let Node::System(sys) = node {
                for resource in &sys.system.access().resources {
                    if !resource.is_global {
                        requires.push(resource.clone());
                    }
                }
            }
        }

        // requires_outputs: free Out<T> reads, derived in execution order so a
        // read preceding its only producer is not spuriously credited.
        let mut requires_outputs = self.derive_requires_outputs(&entry, unit);

        canonicalize(&mut requires);
        canonicalize(&mut requires_outputs);
        canonicalize(&mut produces);
        GraphSignature {
            requires,
            requires_outputs,
            produces,
        }
    }

    /// Derives the free-output (`requires_outputs`) axis in execution order.
    ///
    /// Walks the sequential chain from `entry`, accumulating the set of output
    /// types *guaranteed* produced so far. A node's `Out<T>` read — a system
    /// parameter, a decision predicate's input, or a switch discriminator's
    /// input — is free unless `T` is already in that set. Branch nodes
    /// (decision / switch / loop / parallel) never contribute to the produced
    /// set — their outputs are conditional — so a read of a branch-only output
    /// stays free. Branch interiors are still inspected for their own free
    /// reads, classified against the outputs guaranteed before the branch.
    ///
    /// Error/timeout handler subgraphs are inspected the same way, classified
    /// against what is guaranteed *before* their source node — the source
    /// failed, so its own output is never credited to its handler. `()` is
    /// never free.
    ///
    /// A loop's termination predicate is deliberately *not* counted: [`Graph::
    /// validate`](Graph::validate) already requires the loop body to produce
    /// the predicate's input type
    /// ([`LoopPredicateOutputNotProduced`](super::ValidationError::LoopPredicateOutputNotProduced)),
    /// and the body runs before the first termination check, so the read is
    /// internally satisfied in every graph that validates.
    fn derive_requires_outputs(&self, entry: &NodeId, unit: TypeId) -> Vec<Access> {
        let seq: HashMap<NodeId, NodeId> = self
            .edges()
            .iter()
            .filter_map(|edge| match edge {
                Edge::Sequential(seq) => Some((seq.from.clone(), seq.to.clone())),
                _ => None,
            })
            .collect();

        let mut produced: HashSet<TypeId> = HashSet::new();
        let mut free: Vec<Access> = Vec::new();
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut current = Some(entry.clone());

        while let Some(node_id) = current {
            if !visited.insert(node_id.clone()) {
                break;
            }
            if let Some(node) = self.get_node(node_id.clone()) {
                // The node's own reads (system parameters, predicate /
                // discriminator inputs), classified against what is guaranteed
                // so far.
                Self::collect_node_reads(node, &produced, unit, &mut free);

                // Handler subgraphs attached to this node run only after it
                // fails, so inspect them *before* crediting its output.
                for handler in self.handler_entries(&node_id) {
                    for inner in self.reachable_nodes_with_handlers(&handler) {
                        Self::collect_node_reads(inner, &produced, unit, &mut free);
                    }
                }

                match node {
                    Node::System(sys) => {
                        let out_id = sys.output_type_id();
                        if out_id != unit {
                            produced.insert(out_id);
                        }
                    }
                    // Conditional branches: classify their interior reads
                    // against what is guaranteed before the branch, but do not
                    // credit the branch's own outputs.
                    Node::Decision(_) | Node::Switch(_) | Node::Loop(_) | Node::Parallel(_) => {
                        for entry in self.branch_entries(node) {
                            for inner in self.reachable_nodes_with_handlers(&entry) {
                                Self::collect_node_reads(inner, &produced, unit, &mut free);
                            }
                        }
                    }
                    // Opaque boundaries — their IO crosses separately.
                    Node::Scope(_) | Node::Dynamic(_) => {}
                }
            }
            current = seq.get(&node_id).cloned();
        }

        free
    }

    /// Pushes each of `node`'s output reads that is neither `()` nor already in
    /// `produced` onto `free`: a system's declared output parameters, a
    /// decision predicate's input, or a switch discriminator's input. Loop
    /// termination inputs are skipped — see
    /// [`derive_requires_outputs`](Self::derive_requires_outputs).
    fn collect_node_reads(
        node: &Node,
        produced: &HashSet<TypeId>,
        unit: TypeId,
        free: &mut Vec<Access>,
    ) {
        let mut push_read = |type_id: TypeId, type_name: &'static str| {
            if type_id != unit && !produced.contains(&type_id) {
                free.push(Access {
                    type_id,
                    type_name,
                    mode: AccessMode::Write,
                    is_global: false,
                });
            }
        };
        match node {
            Node::System(sys) => {
                for output in &sys.system.access().outputs {
                    push_read(output.type_id, output.type_name);
                }
            }
            Node::Decision(dec) => {
                if let Some(predicate) = &dec.predicate {
                    push_read(predicate.input_type_id(), predicate.input_type_name());
                }
            }
            Node::Switch(sw) => {
                if let Some(discriminator) = &sw.discriminator {
                    push_read(
                        discriminator.input_type_id(),
                        discriminator.input_type_name(),
                    );
                }
            }
            Node::Loop(_) | Node::Parallel(_) | Node::Scope(_) | Node::Dynamic(_) => {}
        }
    }

    /// Returns the branch entry node IDs of a control-flow node (empty for
    /// non-branching nodes).
    fn branch_entries(&self, node: &Node) -> Vec<NodeId> {
        match node {
            Node::Decision(dec) => [&dec.true_branch, &dec.false_branch]
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            Node::Switch(sw) => sw
                .cases
                .iter()
                .map(|(_, target)| target.clone())
                .chain(sw.default.clone())
                .collect(),
            Node::Loop(lp) => lp.body_entry.iter().cloned().collect(),
            Node::Parallel(par) => par.branches.clone(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use polaris_system::param::{SystemAccess, SystemContext};
    use polaris_system::system::{BoxFuture, System, SystemError};

    // Marker types stand in for distinct IO types. Primitives avoid a
    // never-constructed `dead_code` warning on bespoke marker structs.
    type Mid = u8;
    type Final = u16;
    type Seed = u32;

    /// Produces a [`Mid`] output, reads nothing.
    struct ProduceMid;
    impl System for ProduceMid {
        type Output = Mid;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "produce_mid"
        }
    }

    /// Reads `Out<Mid>` (produced upstream) and `Out<Seed>` (free), produces a
    /// [`Final`].
    struct ProduceFinal;
    impl System for ProduceFinal {
        type Output = Final;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "produce_final"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new()
                .with_output::<Mid>()
                .with_output::<Seed>()
        }
    }

    /// Reads `Out<Mid>`, produces a [`Final`]. Used to place a reader before its
    /// producer, or after a branch.
    struct ReadsMid;
    impl System for ReadsMid {
        type Output = Final;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "reads_mid"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new().with_output::<Mid>()
        }
    }

    /// Produces `()` — a resource-mutating helper whose return type is not data
    /// flow.
    struct SideEffect;
    impl System for SideEffect {
        type Output = ();
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn name(&self) -> &'static str {
            "side_effect"
        }
    }

    #[test]
    fn free_output_read_before_its_producer_stays_required() {
        // `ProduceFinal` reads `Out<Mid>` and `Out<Seed>`; place it *before*
        // `ProduceMid` so its `Mid` read precedes `Mid`'s only producer.
        // Order-aware derivation must classify `Mid` as free — a required input —
        // rather than crediting the later producer.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceFinal));
        graph.add_boxed_system(Box::new(ProduceMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|access| access.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "Mid is read before its producer, so it is a free input: {free:?}"
        );
        assert!(
            free.contains(&TypeId::of::<Seed>()),
            "Seed is never produced"
        );
        assert_eq!(free.len(), 2);
    }

    #[test]
    fn branch_produced_output_is_not_credited_to_a_downstream_read() {
        // Both branches produce `Mid`; a system after the branch reads `Out<Mid>`.
        // Because the branch is conditional, `Mid` is not guaranteed produced, so
        // the downstream read stays free — while `produces` still lists `Mid` as
        // something that *may* merge back.
        let mut graph = Graph::new();
        graph.add_conditional_branch::<Seed, _, _, _>(
            "branch",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        let sig = graph.signature();
        let free: Vec<TypeId> = sig.requires_outputs().iter().map(|a| a.type_id).collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "branch-only output is not credited, so the downstream read is free: {free:?}"
        );
        let produced: Vec<TypeId> = sig.produces().iter().map(|a| a.type_id).collect();
        assert!(
            produced.contains(&TypeId::of::<Mid>()),
            "produces still lists what may merge back: {produced:?}"
        );
    }

    #[test]
    fn unit_output_is_never_part_of_the_signature() {
        // A `()`-returning helper alongside a real producer: the interface lists
        // only the real output, never `()`.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
        graph.add_boxed_system(Box::new(SideEffect));

        let produced: Vec<TypeId> = graph
            .signature()
            .produces()
            .iter()
            .map(|access| access.type_id)
            .collect();
        assert_eq!(produced, vec![TypeId::of::<Mid>()]);
        assert!(!produced.contains(&TypeId::of::<()>()));
    }

    #[test]
    fn signature_is_order_and_duplicate_insensitive() {
        let a = GraphSignature::new()
            .require_read::<Mid>()
            .require_read::<Seed>()
            .produce::<Final>();
        let b = GraphSignature::new()
            .require_read::<Seed>()
            .require_read::<Mid>()
            .require_read::<Mid>() // duplicate
            .produce::<Final>();

        assert_eq!(a, b, "declaration order and duplicates are normalized away");
        assert!(a.compatible_with(&b));
        assert_eq!(a.requires().len(), 2, "duplicate read was deduped");
    }

    #[test]
    fn require_write_and_require_output_are_distinct() {
        let writes = GraphSignature::new().require_write::<Mid>();
        let outputs = GraphSignature::new().require_output::<Mid>();

        assert_ne!(
            writes, outputs,
            "a required ResMut<T> and a required Out<T> must not collapse"
        );
        assert!(!writes.compatible_with(&outputs));

        assert_eq!(writes.requires().len(), 1);
        assert!(writes.requires_outputs().is_empty());
        assert!(outputs.requires().is_empty());
        assert_eq!(outputs.requires_outputs().len(), 1);
    }

    #[test]
    fn derivation_routes_free_outputs_to_requires_outputs() {
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
        graph.add_boxed_system(Box::new(ProduceFinal));

        let sig = graph.signature();

        assert!(sig.requires().is_empty(), "no resource reads in this graph");
        // `Out<Mid>` is produced internally, so only `Out<Seed>` is free.
        assert_eq!(sig.requires_outputs().len(), 1);
        assert_eq!(sig.requires_outputs()[0].type_id, TypeId::of::<Seed>());

        let produced: Vec<TypeId> = sig.produces().iter().map(|access| access.type_id).collect();
        assert_eq!(produced.len(), 2);
        assert!(produced.contains(&TypeId::of::<Mid>()));
        assert!(produced.contains(&TypeId::of::<Final>()));
    }

    #[test]
    fn diff_names_the_diverging_axis_and_direction() {
        // Candidate reads `Mid` and produces `Final`; slot requires a `Seed` free
        // output and produces `Final`. They agree on `produces` and diverge on
        // both `requires` (candidate has extra) and `requires_outputs` (slot has
        // missing).
        let candidate = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        let slot = GraphSignature::new()
            .require_output::<Seed>()
            .produce::<Final>();

        assert!(!candidate.compatible_with(&slot));
        let diff = candidate.diff(&slot);
        assert!(!diff.is_empty());

        // Candidate reads `Mid` the slot never asked for.
        assert_eq!(diff.extra_requires().len(), 1);
        assert_eq!(diff.extra_requires()[0].type_id, TypeId::of::<Mid>());
        assert!(diff.missing_requires().is_empty());

        // Slot demands a free `Out<Seed>` the candidate does not read.
        assert_eq!(diff.missing_requires_outputs().len(), 1);
        assert_eq!(
            diff.missing_requires_outputs()[0].type_id,
            TypeId::of::<Seed>()
        );

        // Both produce `Final`, so that axis is clean.
        assert!(diff.missing_produces().is_empty());
        assert!(diff.extra_produces().is_empty());
    }

    #[test]
    fn compatible_signatures_have_an_empty_diff() {
        let signature = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        let diff = signature.diff(&signature.clone());
        assert!(diff.is_empty());
        assert_eq!(diff.to_string(), "no signature differences");
    }

    #[test]
    fn compatible_with_agrees_with_empty_diff() {
        // `compatible_with` is an allocation-free fast path decoupled from
        // `diff`; this pins the load-bearing invariant that the two never
        // disagree, in both directions, so a future variance relaxation can't
        // update one and forget the other.
        let base = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>();

        // Identical signatures: compatible, empty diff.
        let same = base.clone();
        assert_eq!(
            base.compatible_with(&same),
            base.diff(&same).is_empty(),
            "compatible pair must agree"
        );
        assert!(base.compatible_with(&same));

        // Diverging on each axis in turn: incompatible, non-empty diff.
        for candidate in [
            GraphSignature::new()
                .require_output::<Seed>()
                .produce::<Final>(), // missing a required read
            GraphSignature::new()
                .require_read::<Mid>()
                .require_read::<Seed>()
                .require_output::<Seed>()
                .produce::<Final>(), // an extra required read
            GraphSignature::new()
                .require_read::<Mid>()
                .require_output::<Seed>(), // missing a produced output
        ] {
            assert_eq!(
                base.compatible_with(&candidate),
                base.diff(&candidate).is_empty(),
                "compatible_with must equal diff-is-empty for {candidate:?}"
            );
            assert!(!base.compatible_with(&candidate));
        }
    }

    #[test]
    fn diff_display_renders_only_the_diverging_axes() {
        let candidate = GraphSignature::new().require_read::<Mid>();
        let slot = GraphSignature::new().produce::<Final>();
        let rendered = candidate.diff(&slot).to_string();

        // Only the two non-empty axes appear, in human terms.
        assert!(rendered.starts_with("signature mismatch — "), "{rendered}");
        assert!(
            rendered.contains("requires: candidate additionally has"),
            "{rendered}"
        );
        assert!(rendered.contains("produces: candidate lacks"), "{rendered}");
        assert!(!rendered.contains("requires_outputs"), "{rendered}");
    }

    // ── Subsumption (`satisfies`) ────────────────────────────────────────────

    #[test]
    fn satisfies_allows_extra_requires_and_extra_produces() {
        // The capability relation: a candidate that needs more (environment-
        // provided) and produces more (merges back harmlessly) still
        // satisfies the slot — exactly the case exact-match rejects.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        let candidate = GraphSignature::new()
            .require_read::<Mid>()
            .require_write::<HandlerRes>() // setup-provided environment
            .produce::<Final>()
            .produce::<Seed>(); // extra output

        assert!(candidate.satisfies(&slot));
        assert!(!candidate.compatible_with(&slot), "exact match rejects it");
    }

    #[test]
    fn satisfies_requires_every_slot_demand() {
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();

        // Missing the slot's required read.
        let no_read = GraphSignature::new().produce::<Final>();
        assert!(!no_read.satisfies(&slot));

        // Missing the slot's produced output.
        let no_produce = GraphSignature::new().require_read::<Mid>();
        assert!(!no_produce.satisfies(&slot));

        // Mode matters on the requires axis: the slot demands a read, the
        // candidate only declares a write.
        let wrong_mode = GraphSignature::new()
            .require_write::<Mid>()
            .produce::<Final>();
        assert!(!wrong_mode.satisfies(&slot));

        // …and strictly in the reverse direction: a slot demanding a write
        // is not satisfied by a candidate that only declares a read. Modes
        // compare exactly — there is no "write subsumes read" relaxation.
        let write_slot = GraphSignature::new()
            .require_write::<Mid>()
            .produce::<Final>();
        let read_candidate = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        assert!(!read_candidate.satisfies(&write_slot));
    }

    #[test]
    fn satisfies_bounds_free_output_reads_by_the_slot() {
        let slot = GraphSignature::new().require_output::<Seed>();

        // Reading fewer free outputs than the slot sanctions is fine…
        let reads_none = GraphSignature::new();
        assert!(reads_none.satisfies(&slot));

        // …but a free read the slot never sanctioned would dangle at runtime.
        let reads_extra = GraphSignature::new()
            .require_output::<Seed>()
            .require_output::<Mid>();
        assert!(!reads_extra.satisfies(&slot));
    }

    #[test]
    fn compatible_signatures_always_satisfy() {
        // Exact match is the degenerate case of subsumption, so
        // `compatible_with` must imply `satisfies`.
        let signature = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>();
        assert!(signature.compatible_with(&signature.clone()));
        assert!(signature.satisfies(&signature.clone()));
    }

    #[test]
    fn satisfies_agrees_with_empty_satisfies_diff() {
        // The lockstep invariant, mirrored from `compatible_with` ⇔ `diff`:
        // `satisfies` and `satisfies_diff` must never disagree, so error
        // messages always name the real violation.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>();

        for candidate in [
            // Satisfying: exact, extra require, extra produce, fewer free reads.
            slot.clone(),
            slot.clone().require_write::<HandlerRes>(),
            slot.clone().produce::<Seed>(),
            GraphSignature::new()
                .require_read::<Mid>()
                .produce::<Final>(),
            // Violating: each subsumption axis in turn.
            GraphSignature::new()
                .require_output::<Seed>()
                .produce::<Final>(), // missing slot require
            slot.clone().require_output::<Mid>(), // unsanctioned free read
            GraphSignature::new()
                .require_read::<Mid>()
                .require_output::<Seed>(), // missing slot produce
        ] {
            assert_eq!(
                candidate.satisfies(&slot),
                candidate.satisfies_diff(&slot).is_empty(),
                "satisfies must equal satisfies_diff-is-empty for {candidate:?}"
            );
        }
    }

    #[test]
    fn satisfies_diff_reports_only_violating_directions() {
        // Candidate has an extra require and an extra produce (both permitted)
        // but misses the slot's produce and reads an unsanctioned free output.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        let candidate = GraphSignature::new()
            .require_read::<Mid>()
            .require_read::<Seed>() // permitted: environment-provided
            .require_output::<HandlerRes>() // violation: unsanctioned free read
            .produce::<Mid>(); // permitted extra, but Final is missing

        let diff = candidate.satisfies_diff(&slot);
        assert!(!diff.is_empty());
        // Permitted divergence never appears.
        assert!(diff.extra_requires().is_empty());
        assert!(diff.missing_requires_outputs().is_empty());
        assert!(diff.extra_produces().is_empty());
        // Violations are named.
        assert!(diff.missing_requires().is_empty());
        assert_eq!(diff.extra_requires_outputs().len(), 1);
        assert_eq!(
            diff.extra_requires_outputs()[0].type_id,
            TypeId::of::<HandlerRes>()
        );
        assert_eq!(diff.missing_produces().len(), 1);
        assert_eq!(diff.missing_produces()[0].type_id, TypeId::of::<Final>());
    }

    #[test]
    fn derived_signature_satisfies_a_narrower_capability_slot() {
        // The motivating case: derivation conservatively reports a
        // setup-provided resource as required, so the derived signature is
        // wider than the capability slot — subsumption still admits it.
        struct ReadsEnvProducesFinal;
        impl System for ReadsEnvProducesFinal {
            type Output = Final;
            fn run<'a>(
                &'a self,
                _ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async { Ok(0) })
            }
            fn name(&self) -> &'static str {
                "reads_env_produces_final"
            }
            fn access(&self) -> SystemAccess {
                SystemAccess::new()
                    .with_read::<Mid>()
                    .with_read::<HandlerRes>()
            }
        }

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ReadsEnvProducesFinal));

        // The slot names only the input read and the produced output; the
        // graph's extra `HandlerRes` read is presumed environment-provided.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        assert!(graph.signature().satisfies(&slot));
        assert!(!graph.signature().compatible_with(&slot));
    }

    // ── Rendering ────────────────────────────────────────────────────────────

    #[test]
    fn rendered_covers_all_axes_in_parameter_terms() {
        let signature = GraphSignature::new()
            .require_read::<Mid>()
            .require_write::<Seed>()
            .require_output::<HandlerRes>()
            .produce::<Final>();

        // Canonical order sorts by opaque `TypeId`, so compare as sets.
        let mut rendered = signature.to_rendered();
        rendered.requires.sort();
        assert_eq!(rendered.requires, vec!["Res<u8>", "ResMut<u32>"]);
        assert_eq!(rendered.requires_outputs, vec!["Out<u64>"]);
        assert_eq!(rendered.produces, vec!["Out<u16>"]);
    }

    #[test]
    fn display_renders_all_axes_even_when_empty() {
        let signature = GraphSignature::new().produce::<Final>();
        assert_eq!(
            signature.to_string(),
            "requires: []; requires_outputs: []; produces: [Out<u16>]"
        );
    }

    #[test]
    fn derived_signature_matches_handwritten_contract() {
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
        graph.add_boxed_system(Box::new(ProduceFinal));

        // A hand-written contract using `require_output` for the free `Out<Seed>`
        // matches the derived signature exactly — proving the builder and the
        // derivation agree on where free outputs land.
        let contract = GraphSignature::new()
            .require_output::<Seed>()
            .produce::<Mid>()
            .produce::<Final>();

        assert!(graph.signature().compatible_with(&contract));
        assert_eq!(graph.signature(), contract);
    }

    // ── Error/timeout handler visibility ────────────────────────────────────

    /// A marker resource read by the handler fixture.
    type HandlerRes = u64;

    /// A fallible [`Final`] producer — the source node error handlers attach to.
    struct FallibleFinal;
    impl System for FallibleFinal {
        type Output = Final;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "fallible_final"
        }
        fn is_fallible(&self) -> bool {
            true
        }
    }

    /// A handler system: reads `Res<HandlerRes>` and `Out<Seed>`, produces a
    /// [`Mid`].
    struct HandlerSystem;
    impl System for HandlerSystem {
        type Output = Mid;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "handler_system"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new()
                .with_read::<HandlerRes>()
                .with_output::<Seed>()
        }
    }

    /// A handler system reading both `Out<Mid>` and `Out<Final>`, producing a
    /// [`Seed`]. Used to prove handler reads are classified against what is
    /// guaranteed *before* the handler's source node.
    struct HandlerReadsMidAndFinal;
    impl System for HandlerReadsMidAndFinal {
        type Output = Seed;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "handler_reads_mid_and_final"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new()
                .with_output::<Mid>()
                .with_output::<Final>()
        }
    }

    #[test]
    fn error_handler_io_is_part_of_the_signature() {
        // The handler runs on the failure path and its outputs merge back, so
        // its produces / requires / free reads are part of the interface. A
        // derivation that skipped error edges would claim *less* than the
        // runtime truth — the unsound direction.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(FallibleFinal));
        graph.add_error_handler(|g| {
            g.add_boxed_system(Box::new(HandlerSystem));
        });

        let sig = graph.signature();
        let produced: Vec<TypeId> = sig.produces().iter().map(|a| a.type_id).collect();
        assert!(
            produced.contains(&TypeId::of::<Mid>()),
            "handler output may merge back on the failure path: {produced:?}"
        );
        assert!(produced.contains(&TypeId::of::<Final>()));

        let required: Vec<TypeId> = sig.requires().iter().map(|a| a.type_id).collect();
        assert_eq!(
            required,
            vec![TypeId::of::<HandlerRes>()],
            "the handler's resource read is required from the parent"
        );

        let free: Vec<TypeId> = sig.requires_outputs().iter().map(|a| a.type_id).collect();
        assert_eq!(
            free,
            vec![TypeId::of::<Seed>()],
            "the handler's unproduced output read is a free input"
        );
    }

    #[test]
    fn handler_reads_credit_only_pre_source_outputs() {
        // Chain: ProduceMid → FallibleFinal (with a handler reading `Out<Mid>`
        // and `Out<Final>`). `Mid` is guaranteed before the source, so the
        // handler's `Mid` read is satisfied; the source's own `Final` never
        // completed on the failure path, so the handler's `Final` read is free.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
        graph.add_boxed_system(Box::new(FallibleFinal));
        graph.add_error_handler(|g| {
            g.add_boxed_system(Box::new(HandlerReadsMidAndFinal));
        });

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Final>()),
            "the failing source's own output is never credited to its handler: {free:?}"
        );
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "outputs guaranteed before the source are credited: {free:?}"
        );
    }

    // ── Predicate / discriminator inputs ────────────────────────────────────

    /// Produces a [`Seed`] output, reads nothing.
    struct ProduceSeed;
    impl System for ProduceSeed {
        type Output = Seed;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "produce_seed"
        }
    }

    #[test]
    fn decision_predicate_input_is_a_free_read_when_unproduced() {
        // The predicate reads `Out<Seed>` at runtime; nothing produces it, so
        // the interface must require it — otherwise a candidate headed by this
        // branch would match an empty slot and fail at runtime instead of at
        // admission.
        let mut graph = Graph::new();
        graph.add_conditional_branch::<Seed, _, _, _>(
            "branch",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Seed>()),
            "the predicate's input is a runtime read: {free:?}"
        );
    }

    #[test]
    fn decision_predicate_input_produced_upstream_is_not_free() {
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceSeed));
        graph.add_conditional_branch::<Seed, _, _, _>(
            "branch",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Seed>()),
            "a predicate input guaranteed upstream is internally satisfied: {free:?}"
        );
    }

    #[test]
    fn switch_discriminator_input_is_a_free_read_when_unproduced() {
        let mut graph = Graph::new();
        let case: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        graph.add_switch::<Seed, _, _, _>("switch", |_seed| "case", [("case", case)], None);

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Seed>()),
            "the discriminator's input is a runtime read: {free:?}"
        );
    }

    #[test]
    fn loop_termination_input_is_not_counted() {
        // Deliberate: `Graph::validate` requires the loop body to produce the
        // termination predicate's input type, and the body runs before the
        // first termination check — the read is internally satisfied in every
        // graph that validates, so counting it would spuriously require the
        // loop's internal type from the parent.
        let mut graph = Graph::new();
        graph.add_loop::<Mid, _, _>(
            "loop",
            |_mid| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "a validated loop's termination input is body-satisfied: {free:?}"
        );
    }

    // ── Branch interiors beyond decisions ───────────────────────────────────

    #[test]
    fn switch_loop_and_parallel_interior_reads_stay_free() {
        // Each branch-entry arm must surface interior free reads. `ReadsMid`
        // reads `Out<Mid>` with no producer upstream, so `Mid` must be free
        // whichever control-flow node hosts it.
        let cases: [(&str, Box<dyn FnOnce(&mut Graph)>); 3] = [
            (
                "switch",
                Box::new(|g: &mut Graph| {
                    let case: fn(&mut Graph) = |g| {
                        g.add_boxed_system(Box::new(ReadsMid));
                    };
                    g.add_switch::<Seed, _, _, _>("sw", |_seed| "case", [("case", case)], None);
                }),
            ),
            (
                "loop",
                Box::new(|g: &mut Graph| {
                    g.add_loop::<Final, _, _>(
                        "lp",
                        |_f| true,
                        |g| {
                            g.add_boxed_system(Box::new(ReadsMid));
                        },
                    );
                }),
            ),
            (
                "parallel",
                Box::new(|g: &mut Graph| {
                    g.add_parallel(
                        "par",
                        [|g: &mut Graph| {
                            g.add_boxed_system(Box::new(ReadsMid));
                        }],
                    );
                }),
            ),
        ];

        for (label, build) in cases {
            let mut graph = Graph::new();
            build(&mut graph);
            let free: Vec<TypeId> = graph
                .signature()
                .requires_outputs()
                .iter()
                .map(|a| a.type_id)
                .collect();
            assert!(
                free.contains(&TypeId::of::<Mid>()),
                "{label}: interior read of an unproduced output must stay free: {free:?}"
            );
        }
    }

    // ── Opacity, cycles, unit, over-declaration ─────────────────────────────

    #[test]
    fn embedded_scope_and_dynamic_are_opaque() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        // A scope's inner systems cross a separate boundary — they contribute
        // nothing to the outer signature.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        let mut scoped = Graph::new();
        scoped.add_scope("inner", inner, ContextPolicy::shared());
        assert_eq!(scoped.signature(), GraphSignature::default());

        // Same for a dynamic node: its candidates' IO is governed by the slot
        // contract, not the outer derivation.
        let mut candidate = Graph::new();
        candidate.add_boxed_system(Box::new(ProduceMid));
        let mut dynamic = Graph::new();
        dynamic.add_dynamic(
            "route",
            |_ctx| Arc::from("a"),
            [("a", candidate)],
            DynamicSlot::new(
                GraphSignature::new().produce::<Mid>(),
                ContextPolicy::shared(),
            ),
        );
        assert_eq!(dynamic.signature(), GraphSignature::default());
    }

    #[test]
    fn sequential_cycle_terminates_derivation() {
        // A hand-wired sequential cycle must not hang the visited-set guard.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
        graph.add_boxed_system(Box::new(ProduceFinal));
        let first = graph.nodes()[0].id();
        let last = graph.nodes()[1].id();
        graph.add_sequential_edge(last, first);

        let produced: Vec<TypeId> = graph
            .signature()
            .produces()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert_eq!(produced.len(), 2, "both systems seen exactly once");
    }

    #[test]
    fn produce_unit_never_matches_a_derived_signature() {
        // `()` is filtered out of derived signatures, so a hand-written
        // `produce::<()>()` is honored as declared but can never match one.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(SideEffect));

        let derived = graph.signature();
        assert!(derived.produces().is_empty());

        let handwritten = GraphSignature::new().produce::<()>();
        assert!(!handwritten.compatible_with(&derived));
        assert!(!derived.compatible_with(&handwritten));
    }

    #[test]
    fn derived_requires_over_declares_self_inserted_resources() {
        // Conservative v1 rule: derivation reads declarations, not runtime
        // behavior, so a resource the graph would self-insert is still
        // required-from-parent. Over-declaring is the sound direction.
        struct ReadsHandlerRes;
        impl System for ReadsHandlerRes {
            type Output = Mid;
            fn run<'a>(
                &'a self,
                _ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async { Ok(0) })
            }
            fn name(&self) -> &'static str {
                "reads_handler_res"
            }
            fn access(&self) -> SystemAccess {
                SystemAccess::new().with_read::<HandlerRes>()
            }
        }

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ReadsHandlerRes));

        let required: Vec<(TypeId, AccessMode)> = graph
            .signature()
            .requires()
            .iter()
            .map(|a| (a.type_id, a.mode))
            .collect();
        assert_eq!(
            required,
            vec![(TypeId::of::<HandlerRes>(), AccessMode::Read)]
        );
    }

    #[test]
    fn nested_boundary_inside_error_handler_is_detected() {
        // A Scope/Dynamic boundary smuggled behind an error edge must still be
        // caught — otherwise a candidate could re-enter selection through its
        // handler with IO the slot contract never saw.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(FallibleFinal));
        graph.add_error_handler(|g| {
            g.add_scope("hidden", inner, crate::node::ContextPolicy::shared());
        });

        assert!(graph.contains_nested_boundary());
    }
}
