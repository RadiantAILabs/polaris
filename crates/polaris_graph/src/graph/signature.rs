//! Aggregate IO signatures for subgraphs, used to type-check dynamic subgraph
//! selection against a slot contract.
//!
//! A [`GraphSignature`] is the input/output interface a subgraph presents to its
//! surroundings: the free inputs it needs from the parent context — required
//! resources (`requires`) and required free outputs (`requires_outputs`) — and
//! the output types it produces and merges back (`produces`). The
//! [`Dynamic`](crate::node::Node::Dynamic) node declares a *slot* signature and
//! only ever runs a candidate whose signature its [`MatchingMode`] mode admits — so
//! composition stays sound even when the concrete subgraph is chosen (or
//! swapped) at runtime.
//!
//! # Three comparison semantics
//!
//! Signatures are compared under three relations, each named for its soundness
//! envelope:
//!
//! - [`compatible_with`](GraphSignature::compatible_with) — **exact-set
//!   match**, the default for [`Dynamic`](crate::node::Node::Dynamic) slot
//!   substitution ([`MatchingMode::Exact`]), where the parent graph was validated
//!   against the slot's precise interface and any divergence could break it.
//! - [`substitutable_for`](GraphSignature::substitutable_for) — **substitut-
//!   ability**, the opt-in [`Dynamic`](crate::node::Node::Dynamic) relation
//!   ([`MatchingMode::Substitutable`]): contravariant on inputs, covariant on
//!   outputs. The candidate may consume *less* than the slot guarantees and
//!   produce *more* than it promises, but never the reverse.
//! - [`satisfies`](GraphSignature::satisfies) — **subsumption**, used for
//!   capability matching (e.g. a sessions contract registry asking "is this
//!   agent a knowledge accumulator?"): the candidate may require *more* than
//!   the slot names (extra requires are presumed environment-provided by the
//!   registrant's setup) and produce *more* (extra outputs merge back, and are
//!   inert only where the surrounding graph produces no colliding type), but
//!   may not read free outputs the slot never sanctioned.
//!
//! The middle and the last differ on exactly one axis, and the difference is
//! load-bearing: substitution is checked against an environment the *parent*
//! supplies, so a candidate requirement the slot never named would dangle,
//! while capability matching is checked against an environment the
//! *registrant* supplies, so an extra requirement is presumed self-provided.
//! `requires` is therefore contravariant in the first and covariant in the
//! second; the other two axes agree.
//!
//! Each relation ships with a lockstep diff mode ([`diff`](GraphSignature::diff)
//! / [`substitutable_diff`](GraphSignature::substitutable_diff) /
//! [`satisfies_diff`](GraphSignature::satisfies_diff)) so a rejection reads
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
//! satisfied only when a producer of `T` is *guaranteed* to have run first.
//! Four node kinds carry that guarantee, because the runtime makes it: a
//! system earlier on the sequential chain; a parallel node, since every branch
//! runs and merges its outputs back; a decision or switch, for outputs in the
//! intersection of every arm that can continue; and a scope, whose inner
//! outputs merge back on exit whatever its context policy. The latter three are
//! credited through their own *guaranteed* sets, applying the same rule
//! recursively — except that a scope carrying a declared contract is credited
//! with the contract's `produces` instead of a derived set (see *Declared scope
//! contracts* below).
//! A switch key with neither a matching case nor a default returns
//! [`ExecutionError::NoMatchingCase`](crate::executor::ExecutionError::NoMatchingCase)
//! before the downstream chain starts, so unmatched keys do not need an empty
//! arm in that intersection. The public switch builder omits an empty case
//! subgraph, making its key unmatched (and therefore an error or a default
//! selection), rather than silently continuing. A missing decision arm likewise
//! errors before the downstream chain; derivation conservatively credits neither
//! arm in that malformed shape. A system failure in a selected arm can instead
//! enter an error or timeout handler and then return successfully from the arm. The
//! guaranteed walk intersects its normal continuation with every handler
//! continuation, so it credits only outputs all successful paths share. Loop
//! bodies remain uncredited because every existing loop form can
//! execute zero iterations. A dynamic node credits its slot contract: validation
//! admits only candidates that produce every contracted output, and the boundary
//! merges those outputs back. Decision predicates, switch discriminators,
//! and loop termination predicates read an output at runtime too, so their input
//! types are counted as reads at the node's position — a loop's termination
//! predicate in particular is evaluated *before* the first iteration, so the
//! loop body's outputs can never satisfy it. The derived signature therefore
//! never claims *less* than the runtime truth — the only direction a soundness
//! interface may err.
//!
//! Error and timeout **handler subgraphs** are part of the interface: a handler
//! runs on the failure path and its outputs merge back like any other, so
//! handler systems contribute to `produces` and `requires`, and their output
//! reads are classified against what is guaranteed *before* the node the
//! handler is attached to (the failing node's own output is never credited —
//! it did not complete). A guarded source can still guarantee an output when
//! its normal path and every handler path guarantee that output before they
//! return to the enclosing graph.
//!
//! The unit type `()` is never part of an interface: a `()`-returning system
//! communicates through resources, not outputs, so `()` is filtered out of both
//! `produces` and `requires_outputs`.
//!
//! # Declared scope contracts
//!
//! An embedded [`Scope`](crate::node::Node::Scope) built with
//! [`add_scope_with_contract`](Graph::add_scope_with_contract) carries a
//! **declared** `GraphSignature` that stands in for its interior. Derivation
//! propagates the contract at the scope's chain position instead of treating
//! the boundary as opaque: the contract's `requires` surface into the
//! enclosing `requires`, its `requires_outputs` are classified against what is
//! guaranteed upstream (surfacing otherwise), and its `produces` enter the
//! enclosing `produces` and are credited as *guaranteed* production for
//! downstream reads. The declaration is trustworthy because
//! [`validate`](Graph::validate) holds the interior to it: every declared
//! produce must be production the interior *guarantees* — a produce backed
//! only by conditional interior production (a decision case, a loop body) is
//! rejected, since crediting it downstream would claim more than the runtime
//! truth. The contract's `produces` need not be the interior's *entire*
//! guaranteed set, though: it is checked as a **subset**, so a scope may
//! guarantee more than it declares and keep the rest private (see the next
//! paragraph for how that privacy is actually enforced at runtime). It is
//! deliberately a declaration rather than a derived inner signature: stopping
//! at the contract keeps derivation non-recursive and gives `validate()` a
//! stable interface to check drift against, mirroring how a
//! [`Dynamic`](crate::node::Node::Dynamic) slot's contract stands in for its
//! candidates. Both declared scopes and Dynamic slots propagate their complete
//! contract interface: `requires` surface unconditionally, `requires_outputs`
//! classify at the boundary's chain position, and `produces` enter both
//! production axes. Eager `forward`/`fork` inputs are part of the checked contract:
//! a crossing-only type requires a read; existing interior read/write entries
//! are retained. Declared scopes reject `forward_fresh` because signatures
//! cannot express factory availability. Only undeclared scopes keep the opaque
//! treatment described below.
//!
//! The declaration is not only a compile/validate-time interface — the
//! executor enforces it as a real runtime boundary. When a declared scope's
//! [`ContextPolicy`](crate::node::ContextPolicy) is **not**
//! [`shared()`](crate::node::ContextPolicy::shared), only output types named
//! in the contract's `produces` merge back into the parent context; every
//! other output the interior wrote is dropped along with the child context
//! that held it, and a parent output of the same type an undeclared interior
//! output would otherwise have clobbered is left untouched. This is what
//! makes the subset relation above sound to expose as a privacy tool rather
//! than just a validation nicety: an interior guarantee the contract omits
//! genuinely never reaches the parent. [`ContextPolicy::shared()`] is the one
//! exception, by construction: it denotes no boundary at all, so the interior
//! runs directly in the parent's own context and every write — declared or
//! not — is visible immediately, with nothing to filter on exit. A scope
//! combining `shared()` with a declared contract still gets a sound
//! `produces` credit (everything the contract names really is guaranteed
//! present), but gives up hiding; that trade only matters when the contract
//! also has a nonempty `requires_outputs`, since free outputs never cross a
//! non-shared boundary inward and so require `shared()` regardless.
//!
//! Derivation is sound over **declared** access: it reads each system's
//! [`SystemAccess`] (and each predicate's input type), which the `#[system]`
//! macro derives from the parameter list. A hand-implemented [`System`] whose
//! `access()` under-declares what `run` actually touches escapes the contract —
//! runtime resolution is not gated by declarations.
//!
//! [`SystemAccess`]: polaris_system::param::SystemAccess
//! [`System`]: polaris_system::system::System
//! [`ContextPolicy::shared()`]: crate::node::ContextPolicy::shared

use super::Graph;
use crate::edge::Edge;
use crate::node::{CrossingAction, Node, NodeId, NodeName};
use hashbrown::{HashMap, HashSet};
use polaris_system::param::{Access, AccessMode};
use std::any::TypeId;
use std::fmt;
use std::sync::{Arc, PoisonError};

/// The aggregate input/output interface of a subgraph.
///
/// - `requires` — non-global resources (`Res<T>` / `ResMut<T>`) the subgraph
///   reads from the parent context.
/// - `requires_outputs` — free outputs (`Out<T>`) the subgraph reads but no
///   *guaranteed* producer inside it deposits first, so the parent must supply
///   them. Guaranteed is narrower than "present somewhere": production in
///   every decision or switch arm counts, but production in only some arms, a
///   loop body, or behind an error or timeout handler does not. See
///   [`Graph::signature`](Graph::signature) for the exact rules.
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
    guaranteed_produces: Vec<Access>,
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
        let output = Access::write::<T>();
        self.produces.push(output.clone());
        self.guaranteed_produces.push(output);
        canonicalize(&mut self.produces);
        canonicalize(&mut self.guaranteed_produces);
        self
    }

    /// The non-global resources this subgraph reads from the parent context.
    #[must_use]
    pub fn requires(&self) -> &[Access] {
        &self.requires
    }

    /// The free outputs (`Out<T>`) this subgraph reads without a *guaranteed*
    /// producer deposited first, so the parent must supply them.
    ///
    /// A producer that may not run — in only some decision or switch arms, a
    /// loop body, beyond a dynamic slot's contract, or behind an error or
    /// timeout handler —
    /// does not clear the entry. [`Graph::signature`](Graph::signature) states
    /// the rules in full.
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

    /// The output types this subgraph may produce on any reachable path.
    ///
    /// This axis is a flat union, so it includes output types produced only in
    /// a conditional arm, loop body, or handler. Use
    /// [`guaranteed_produces`](Self::guaranteed_produces) when a caller needs
    /// only outputs present after every successful path.
    #[must_use]
    pub fn produces(&self) -> &[Access] {
        &self.produces
    }

    /// The output types this subgraph guarantees on every successful path.
    ///
    /// Derived signatures intersect alternative decision, switch, and guarded
    /// paths; loop bodies contribute nothing because they may not run. A
    /// hand-written signature treats every [`produce`](Self::produce)
    /// declaration as a guarantee, because it is a promise the caller makes
    /// about that boundary.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::Graph;
    /// use polaris_system::param::Access;
    /// use std::any::TypeId;
    ///
    /// async fn start() -> bool { true }
    /// async fn draft() -> String { String::new() }
    /// async fn score() -> u32 { 1 }
    ///
    /// let mut graph = Graph::new();
    /// graph.add_system(start).add_conditional_branch(
    ///     "check",
    ///     |ready: &bool| *ready,
    ///     |g| { g.add_system(draft); },
    ///     |g| { g.add_system(draft).add_system(score); },
    /// );
    ///
    /// let has = |list: &[Access], id: TypeId| list.iter().any(|access| access.type_id == id);
    /// let signature = graph.signature();
    /// // Both arms write a `String`, so it is guaranteed.
    /// assert!(has(signature.guaranteed_produces(), TypeId::of::<String>()));
    /// // Only one arm writes a `u32`: it may be produced, but is not guaranteed.
    /// assert!(has(signature.produces(), TypeId::of::<u32>()));
    /// assert!(!has(signature.guaranteed_produces(), TypeId::of::<u32>()));
    /// ```
    #[must_use]
    pub fn guaranteed_produces(&self) -> &[Access] {
        &self.guaranteed_produces
    }

    /// Returns contract outputs this signature does not guarantee.
    ///
    /// Dynamic admission uses this additional check because a slot promotes
    /// its contract outputs as guarantees. Compatibility relations deliberately
    /// continue to compare the flat `produces` axis.
    pub(crate) fn missing_guaranteed_produces(&self, contract: &Self) -> Vec<Access> {
        access_difference(&contract.produces, &self.guaranteed_produces)
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
            guaranteed_produces: self.guaranteed_produces.iter().map(render_output).collect(),
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
    /// load-bearing invariant (asserted in tests): the two must move in lockstep
    /// so the error messages stay correct. The variance relaxation this once
    /// anticipated (accept a subset of `requires`, a superset of `produces`) did
    /// not land here — it is the separate
    /// [`substitutable_for`](Self::substitutable_for) relation, so a slot opts
    /// into it rather than every slot silently loosening.
    ///
    /// For capability matching — where a candidate carrying *more* than the
    /// slot names should still qualify — use [`satisfies`](Self::satisfies)
    /// instead; the two relations are deliberately separate, each named for
    /// its soundness envelope. For slot substitution that admits a candidate
    /// consuming *less* than the slot guarantees, use
    /// [`substitutable_for`](Self::substitutable_for)
    /// ([`MatchingMode::Substitutable`]).
    #[must_use]
    pub fn compatible_with(&self, slot: &GraphSignature) -> bool {
        self.requires == slot.requires
            && access_types_equal(&self.requires_outputs, &slot.requires_outputs)
            && self.produces == slot.produces
    }

    /// Returns `true` if a candidate carrying `self` is a sound **substitute**
    /// for a slot declaring `slot`.
    ///
    /// This is the general form of the substitution relation whose strict
    /// special case is [`compatible_with`](Self::compatible_with): a slot is a
    /// hole in an already-validated parent, so a candidate is sound to drop
    /// into it exactly when it *demands no more than the slot guarantees* and
    /// *produces at least what the slot promises*. **Contravariant on inputs,
    /// covariant on outputs**, per axis:
    ///
    /// - the candidate's `requires` must be a **subset** of the slot's — the
    ///   parent was validated against the slot's inputs and supplies exactly
    ///   those, so a candidate reading fewer of them leaves the extras unused,
    ///   while one reading a resource the slot never named would find nothing
    ///   there at runtime. Entries compare by `(type_id, mode)`, so a
    ///   `ResMut<T>` candidate read is not admitted by a `Res<T>` slot entry
    ///   (or the reverse): there is no write-subsumes-read relaxation, exactly
    ///   as in [`satisfies`](Self::satisfies).
    /// - the candidate's `requires_outputs` must be a **subset** of the
    ///   slot's — the same argument for free `Out<T>` reads, which the parent
    ///   deposits before the boundary. A read the slot never sanctioned would
    ///   dangle.
    /// - the slot's `produces` must be a **subset** of the candidate's — the
    ///   parent's downstream nodes were validated against the slot's outputs,
    ///   so every one of them must still be merged back. Extra candidate
    ///   outputs merge back too, and they are **not** inert: merge-back
    ///   inserts by `TypeId`, so an extra output whose type the parent chain
    ///   also produces *shadows* it for every downstream reader, and may
    ///   become the run's final output. Nothing downstream was validated
    ///   against those extras, so the risk is confined to types the parent
    ///   already produces — where `validate()` warns
    ///   ([`DynamicCandidateShadowsOutput`]). Under
    ///   [`compatible_with`](Self::compatible_with) the produced set equals
    ///   the contract, so this cannot arise; it is the one property
    ///   [`MatchingMode::Substitutable`] gives up.
    ///
    /// [`DynamicCandidateShadowsOutput`]: crate::graph::ValidationWarning::DynamicCandidateShadowsOutput
    ///
    /// Note the `requires` direction is **inverted** relative to
    /// [`satisfies`](Self::satisfies), and deliberately so. Subsumption trusts
    /// the registrant's own environment claim, because there the candidate's
    /// `setup` provides the extras. A `Dynamic` slot has no such registrant:
    /// the *parent graph* is the environment, and it was validated against the
    /// slot contract alone, so a candidate requirement the slot never named has
    /// nothing to supply it. `satisfies` is therefore not a sound relation for
    /// slot substitution, and this is a third relation rather than a reuse of
    /// it.
    ///
    /// [`compatible_with`](Self::compatible_with) implies this relation (set
    /// equality is a subset in both directions), so opting a slot into
    /// [`MatchingMode::Substitutable`] only ever widens the admissible set.
    ///
    /// The `substitutable_for` ⇔ empty-[`substitutable_diff`](Self::substitutable_diff)
    /// equivalence is a load-bearing invariant (asserted in tests), mirroring
    /// the other two relations' lockstep: any relaxation must move both in step
    /// so error messages stay correct.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::GraphSignature;
    ///
    /// // The escalation-ladder case: a cheap rung needs less context than the
    /// // slot guarantees, and produces the verdict downstream expects.
    /// let slot = GraphSignature::new()
    ///     .require_read::<u8>()
    ///     .require_read::<u32>()
    ///     .produce::<u16>();
    /// let cheap_rung = GraphSignature::new()
    ///     .require_read::<u8>()
    ///     .produce::<u16>();
    ///
    /// assert!(cheap_rung.substitutable_for(&slot));
    /// assert!(!cheap_rung.compatible_with(&slot), "exact match rejects it");
    ///
    /// // A rung needing *more* than the slot guarantees is rejected: nothing
    /// // upstream supplies the extra read.
    /// let greedy_rung = slot.clone().require_read::<u64>();
    /// assert!(!greedy_rung.substitutable_for(&slot));
    /// ```
    #[must_use]
    pub fn substitutable_for(&self, slot: &GraphSignature) -> bool {
        is_subset(&self.requires, &slot.requires)
            && is_type_subset(&self.requires_outputs, &slot.requires_outputs)
            && is_subset(&slot.produces, &self.produces)
    }

    /// Computes the substitutability violations between this candidate
    /// signature and a `slot` contract — the diff mode of
    /// [`substitutable_for`](Self::substitutable_for).
    ///
    /// Only the directions that *violate* substitutability are populated:
    /// `extra_requires` (resource reads the slot never guaranteed),
    /// `extra_requires_outputs` (free output reads the slot never sanctioned),
    /// and `missing_produces` (slot outputs the candidate does not produce).
    /// Divergence the relation permits — fewer requires, fewer free output
    /// reads, extra produces — never appears. An empty diff means the candidate
    /// is [substitutable for](Self::substitutable_for) the slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_graph::GraphSignature;
    ///
    /// let slot = GraphSignature::new().require_read::<u8>().produce::<u16>();
    /// let candidate = GraphSignature::new().require_read::<u32>();
    ///
    /// let diff = candidate.substitutable_diff(&slot);
    /// assert!(!diff.is_empty());
    /// assert_eq!(diff.extra_requires().len(), 1); // Res<u32> is unguaranteed
    /// assert_eq!(diff.missing_produces().len(), 1); // Out<u16> is not produced
    /// assert!(diff.missing_requires().is_empty()); // needing less is permitted
    /// ```
    #[must_use]
    pub fn substitutable_diff(&self, slot: &GraphSignature) -> SignatureDiff {
        SignatureDiff {
            missing_requires: Vec::new(),
            extra_requires: access_difference(&self.requires, &slot.requires),
            missing_requires_outputs: Vec::new(),
            extra_requires_outputs: access_type_difference(
                &self.requires_outputs,
                &slot.requires_outputs,
            ),
            missing_produces: access_difference(&slot.produces, &self.produces),
            extra_produces: Vec::new(),
        }
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
    /// That trust is what makes this relation **unsound for
    /// [`Dynamic`](crate::node::Node::Dynamic) slot substitution**, where there
    /// is no registrant and the parent graph is the whole environment. Use
    /// [`substitutable_for`](Self::substitutable_for) there — same relation on
    /// `requires_outputs` and `produces`, inverted on `requires`.
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
    /// Renders the full interface on one line, all four axes in canonical
    /// order — e.g. `requires: [Res<TraceLine>]; requires_outputs: [];
    /// produces: [Out<Triples>]; guaranteed_produces: [Out<Triples>]`. The same advisory-only caveats as
    /// [`to_rendered`](GraphSignature::to_rendered) apply.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requires: [{}]; requires_outputs: [{}]; produces: [{}]; guaranteed_produces: [{}]",
            render_list(&self.requires, render_resource),
            render_list(&self.requires_outputs, render_output),
            render_list(&self.produces, render_output),
            render_list(&self.guaranteed_produces, render_output),
        )
    }
}

/// Which relation a [`Dynamic`](crate::node::Node::Dynamic) slot uses to admit
/// candidates against its contract.
///
/// Both variants admit only candidates that can run in the slot's place: every
/// input a candidate reads is one the parent was already obliged to supply.
/// They differ in how much divergence they tolerate, and
/// [`Exact`](Self::Exact) is the strict subset — `compatible_with` implies
/// `substitutable_for`, so switching a slot to
/// [`Substitutable`](Self::Substitutable) only ever widens the admissible set.
/// Widening it is not free on the output side: see
/// [`Substitutable`](Self::Substitutable) for the shadowing caveat, and
/// [Graph — Dynamic](../../docs/reference/graph.md#dynamic) for the full
/// reasoning, including why the contract's own obligations on the parent are
/// *not* relaxed.
///
/// | Axis | [`Exact`](Self::Exact) | [`Substitutable`](Self::Substitutable) |
/// |---|---|---|
/// | `requires` | `candidate = slot` | `candidate ⊆ slot` |
/// | `requires_outputs` | `candidate = slot` | `candidate ⊆ slot` |
/// | `produces` | `candidate = slot` | `candidate ⊇ slot` |
///
/// Set on a slot with
/// [`DynamicSlot::with_matching`](crate::node::DynamicSlot::with_matching), and
/// on a [`SubgraphRegistry`](crate::registry::SubgraphRegistry) with
/// [`with_matching`](crate::registry::SubgraphRegistry::with_matching); the
/// [default](Default) is [`Exact`](Self::Exact), so an existing slot's
/// admissions are unchanged.
///
/// # Example
///
/// ```
/// use polaris_graph::{GraphSignature, MatchingMode};
///
/// let slot = GraphSignature::new().require_read::<u8>().produce::<u16>();
/// let cheaper = GraphSignature::new().produce::<u16>();
///
/// assert!(!MatchingMode::Exact.admits(&cheaper, &slot));
/// assert!(MatchingMode::Substitutable.admits(&cheaper, &slot));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MatchingMode {
    /// Exact-set match on all three axes
    /// ([`GraphSignature::compatible_with`]).
    ///
    /// The default. It is a sound under-approximation of substitutability: it
    /// never admits an unsound candidate, only rejects some safe ones. Keep it
    /// when the candidates behind a slot are meant to be interchangeable
    /// *implementations* of one interface, so a divergence is more likely a
    /// mistake than an intent.
    #[default]
    Exact,
    /// Substitutability — contravariant on inputs, covariant on outputs
    /// ([`GraphSignature::substitutable_for`]).
    ///
    /// Opt into this when the candidates behind a slot deliberately differ in
    /// what they consume. The motivating shape is an **escalation ladder**,
    /// where a cheap rung legitimately needs less context than the expensive
    /// rung the contract was written for; under [`Exact`](Self::Exact) the
    /// cheap rung is rejected, which is backwards for substitutability.
    ///
    /// What it gives up is on the `produces` axis: a candidate may merge back
    /// output types the contract never named, and merge-back inserts by
    /// `TypeId`, so an extra output shadows a same-typed one the parent chain
    /// produced. `validate()` warns when it can see the collision
    /// ([`DynamicCandidateShadowsOutput`]); a registry candidate registered
    /// later is past that check, so treat a `Substitutable` slot's candidate
    /// set as able to write any type it declares.
    ///
    /// Known limitation on the input side: `requires` entries compare by
    /// `(type_id, mode)`, so a candidate declaring `Res<T>` is *rejected* by a
    /// slot guaranteeing `ResMut<T>` even though read access is strictly less
    /// than write. That errs safe (a false rejection, never a false
    /// admission) and keeps the axis consistent with
    /// [`satisfies`](GraphSignature::satisfies), but it is the escalation-
    /// ladder problem one level down.
    ///
    /// [`DynamicCandidateShadowsOutput`]: crate::graph::ValidationWarning::DynamicCandidateShadowsOutput
    Substitutable,
}

impl MatchingMode {
    /// Returns `true` if a `candidate` signature may fill a `slot` contract
    /// under this relation.
    ///
    /// Allocation-free; use [`diff`](Self::diff) when a rejection needs to
    /// explain itself.
    #[must_use]
    pub fn admits(self, candidate: &GraphSignature, slot: &GraphSignature) -> bool {
        match self {
            MatchingMode::Exact => candidate.compatible_with(slot),
            MatchingMode::Substitutable => candidate.substitutable_for(slot),
        }
    }

    /// Computes the violations of this relation between a `candidate`
    /// signature and a `slot` contract — the diff mode of
    /// [`admits`](Self::admits).
    ///
    /// Empty exactly when [`admits`](Self::admits) returns `true`, since each
    /// relation is paired with its own lockstep diff mode
    /// ([`GraphSignature::diff`] / [`GraphSignature::substitutable_diff`]).
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_graph::{GraphSignature, MatchingMode};
    ///
    /// let slot = GraphSignature::new().require_read::<u8>().produce::<u16>();
    /// let cheaper = GraphSignature::new().produce::<u16>();
    ///
    /// // The same candidate, under each relation's own diff mode.
    /// let exact = MatchingMode::Exact.diff(&cheaper, &slot);
    /// assert_eq!(exact.missing_requires().len(), 1); // Res<u8> is not read
    ///
    /// let relaxed = MatchingMode::Substitutable.diff(&cheaper, &slot);
    /// assert!(relaxed.is_empty()); // reading less is permitted
    /// ```
    #[must_use]
    pub fn diff(self, candidate: &GraphSignature, slot: &GraphSignature) -> SignatureDiff {
        match self {
            MatchingMode::Exact => candidate.diff(slot),
            MatchingMode::Substitutable => candidate.substitutable_diff(slot),
        }
    }
}

impl fmt::Display for MatchingMode {
    /// Names the relation, so a mismatch message can say which check ran —
    /// under [`Substitutable`](Self::Substitutable) the carried
    /// [`SignatureDiff`] reports only the violating directions, and without
    /// the mode the reader cannot tell a permitted divergence from an absent
    /// one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatchingMode::Exact => f.write_str("Exact"),
            MatchingMode::Substitutable => f.write_str("Substitutable"),
        }
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
    /// Rendered `guaranteed_produces` axis (`Out<T>` entries).
    pub guaranteed_produces: Vec<String>,
}

/// The per-axis difference between a candidate [`GraphSignature`] and a slot
/// contract, as computed by [`GraphSignature::diff`] (exact-match mode),
/// [`GraphSignature::substitutable_diff`] (substitutability mode), or
/// [`GraphSignature::satisfies_diff`] (subsumption mode). The latter two
/// populate only the directions their relation forbids.
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
/// [`GraphSignature::substitutable_for`] for
/// [`substitutable_diff`](GraphSignature::substitutable_diff),
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

/// A main-chain loop whose termination predicate input no earlier chain node
/// can deposit, as reported by [`Graph::unguaranteed_loop_predicate_inputs`].
///
/// The executor checks each entry against the live context at run start and
/// fails fast on any input still missing.
#[derive(Debug, Clone)]
pub(crate) struct UnguaranteedLoopInput {
    /// The loop node.
    pub(crate) node: NodeId,
    /// The loop node's name.
    pub(crate) name: NodeName,
    /// The `TypeId` of the output the termination predicate reads.
    pub(crate) input_type_id: TypeId,
    /// The human-readable name of that type (for error messages).
    pub(crate) input_type_name: &'static str,
}

/// Memoized [`Graph::unguaranteed_loop_predicate_inputs`] result, stored in
/// [`Graph::loop_entry_cache`](super::Graph) with the graph shape it was
/// computed for.
#[derive(Debug)]
pub(crate) struct LoopEntryCache {
    /// `(node_count, edge_count)` at computation time — every builder
    /// mutation appends nodes or edges, so a mismatch marks the entry stale.
    shape: (usize, usize),
    /// The computed analysis, shared so cache hits are refcount bumps.
    inputs: Arc<[UnguaranteedLoopInput]>,
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
    ///   type is guaranteed to have run earlier on that chain — an earlier
    ///   system, an earlier parallel node (every branch runs and merges back),
    ///   or an earlier scope (inner outputs merge back on exit). Outputs
    ///   produced only inside a region that may not run — a decision or switch
    ///   case, a loop body, beyond a dynamic slot's contract, or a handler
    ///   subgraph — are
    ///   never credited, so a read of such an output stays free. Decision
    ///   predicates, switch discriminators, and loop termination predicates
    ///   count as reads of their input type (a loop's termination predicate is
    ///   evaluated before the first iteration, so its body never satisfies
    ///   it); handler-subgraph reads are classified against what is guaranteed
    ///   before the node the handler is attached to.
    ///
    /// An empty graph has an empty signature. An embedded *undeclared*
    /// [`Scope`](crate::node::Node::Scope) is opaque: its inner requirements do
    /// not appear in the signature because that IO crosses a separate boundary.
    /// A scope carrying a **declared contract**
    /// ([`add_scope_with_contract`](Graph::add_scope_with_contract)) and every
    /// [`Dynamic`](crate::node::Node::Dynamic) node instead propagate their
    /// complete contract interface: `requires` surface unconditionally,
    /// `requires_outputs` classify at the boundary's chain position, and
    /// `produces` enter both production axes. A dynamic contract is trustworthy
    /// because admission holds every candidate to its inputs and outputs. A
    /// scope's *guaranteed* production is likewise credited to downstream reads
    /// on the enclosing chain, since those outputs merge back on exit — the
    /// boundary hides what the interior needs, not what it has already deposited
    /// by the time the chain continues.
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
        // Declared scopes and Dynamic slots contribute their contracts'
        // produces — the interface each advertises in place of its opaque
        // interior.
        let mut produced: HashSet<TypeId> = HashSet::new();
        let mut produces = Vec::new();
        for node in &nodes {
            match node {
                Node::System(sys) => {
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
                Node::Scope(scope) => {
                    if let Some(contract) = scope.contract() {
                        for access in contract.produces() {
                            if access.type_id != unit && produced.insert(access.type_id) {
                                produces.push(access.clone());
                            }
                        }
                    }
                }
                Node::Dynamic(dynamic) => {
                    for access in dynamic.contract().produces() {
                        if access.type_id != unit && produced.insert(access.type_id) {
                            produces.push(access.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        // requires: non-global resource reads across the whole subgraph.
        // Declared scopes and Dynamic slots surface their contracts' requires;
        // derivation has no notion of guaranteed upstream resource production
        // (it cannot see imperative `ctx.insert` calls), so — like system
        // resource reads — they surface unconditionally.
        let mut requires = Vec::new();
        for node in &nodes {
            match node {
                Node::System(sys) => {
                    for resource in &sys.system.access().resources {
                        if !resource.is_global {
                            requires.push(resource.clone());
                        }
                    }
                }
                Node::Scope(scope) => {
                    if let Some(contract) = scope.contract() {
                        requires.extend(contract.requires().iter().cloned());
                    }
                }
                Node::Dynamic(dynamic) => {
                    requires.extend(dynamic.contract().requires().iter().cloned());
                }
                _ => {}
            }
        }

        // requires_outputs: free Out<T> reads, derived in execution order so a
        // read preceding its only producer is not spuriously credited.
        let mut requires_outputs = self.derive_requires_outputs(&entry, unit);

        canonicalize(&mut requires);
        canonicalize(&mut requires_outputs);
        canonicalize(&mut produces);
        let mut guaranteed_type_ids = HashSet::new();
        self.must_produce_output_types(&mut guaranteed_type_ids);
        let guaranteed_produces = self.accesses_for_guaranteed_types(&guaranteed_type_ids);
        GraphSignature {
            requires,
            requires_outputs,
            produces,
            guaranteed_produces,
        }
    }

    /// Computes the divergence between a declared scope contract and this
    /// graph and its boundary policy — the interface the contract describes.
    ///
    /// This is the drift check behind
    /// [`add_scope_with_contract`](Graph::add_scope_with_contract), run at
    /// [`validate`](Graph::validate) time. `requires` and `requires_outputs`
    /// use the same exact-match relation as
    /// [`SubgraphRegistry::register`](crate::registry::SubgraphRegistry::register)
    /// admission — divergence in either direction is drift, because both axes
    /// surface verbatim into the enclosing graph's own signature (see the
    /// module docs' *Declared scope contracts* section), so an entry the
    /// interior does not actually read would falsely over-claim the parent's
    /// obligations, and one it does read but the contract omits would falsely
    /// under-claim them.
    ///
    /// `produces` is checked differently, and only in the direction
    /// soundness requires: every declared produce must be backed by the
    /// interior's **guaranteed** production
    /// ([`must_produce_output_types`](Self::must_produce_output_types)), so
    /// `missing_produces` (a contract entry the interior does not guarantee)
    /// is drift — crediting it downstream would let the parent signature
    /// claim more than the runtime truth. A produce backed only by
    /// production a decision or switch arm may skip, or by a loop body, fails
    /// this the same way, since it is absent from the guaranteed set — unless
    /// every reachable arm guarantees it, in which case it *is* guaranteed
    /// (see `must_produce_output_types_from_chains`) and the declaration is
    /// sound. Guaranteed production the contract *omits*, conversely, is not
    /// drift: the executor's runtime merge-back drops it before it reaches
    /// the parent (see [`ScopeNode`](crate::node::ScopeNode)'s *Declared
    /// contracts* docs), so it is simply private to the scope rather than a
    /// stale declaration. `extra_produces` is therefore always empty in this
    /// diff — unlike the registry's exact-match check, which still treats it
    /// as drift because there the derived signature *is* the interface, with
    /// no filter narrowing it afterward.
    ///
    /// Eager `forward`/`fork` inputs augment the interior's `requires` (as
    /// reads if the type is not already required) before the `requires`
    /// comparison above. Factory inputs cannot be represented and are
    /// rejected separately by validation.
    ///
    /// Callers gate on the interior being structurally valid and free of
    /// hidden (undeclared) boundaries — a nested plain scope makes the derived
    /// signature incomplete, so no diff against it is meaningful. Dynamic
    /// nodes expose their slot contracts to derivation.
    pub(crate) fn declared_contract_diff(
        &self,
        contract: &GraphSignature,
        policy: &crate::node::ContextPolicy,
    ) -> SignatureDiff {
        let mut derived = self.signature();
        // Copy crossings eagerly read the parent even when no interior system
        // accesses that type. An existing read or write already declares the
        // resource; preserve those conservative interior access modes.
        for crossing in policy.crossings() {
            if matches!(
                crossing.action,
                CrossingAction::Forward(_) | CrossingAction::Fork(_)
            ) && !derived
                .requires
                .iter()
                .any(|access| access.type_id == crossing.type_id)
            {
                derived.requires.push(Access {
                    type_id: crossing.type_id,
                    type_name: crossing.type_name,
                    mode: AccessMode::Read,
                    is_global: false,
                });
            }
        }
        canonicalize(&mut derived.requires);
        let mut guaranteed: HashSet<TypeId> = HashSet::new();
        self.must_produce_output_types(&mut guaranteed);
        let checkable = GraphSignature {
            requires: derived.requires,
            requires_outputs: derived.requires_outputs,
            // Guaranteed production is a subset of the derived produces in a
            // boundary-free interior (every guaranteed producer is reachable,
            // and nested declared contracts are folded into both), so
            // filtering keeps the type names the diff needs for rendering.
            produces: derived
                .produces
                .into_iter()
                .filter(|access| guaranteed.contains(&access.type_id))
                .collect(),
            guaranteed_produces: Vec::new(),
        };
        SignatureDiff {
            missing_requires: access_difference(&contract.requires, &checkable.requires),
            extra_requires: access_difference(&checkable.requires, &contract.requires),
            missing_requires_outputs: access_type_difference(
                &contract.requires_outputs,
                &checkable.requires_outputs,
            ),
            extra_requires_outputs: access_type_difference(
                &checkable.requires_outputs,
                &contract.requires_outputs,
            ),
            // A contract entry backed by less than the guaranteed set is
            // unsound to credit downstream — drift.
            missing_produces: access_difference(&contract.produces, &checkable.produces),
            // Guaranteed production the contract does not declare is not
            // drift: it stays private, filtered out at the boundary by
            // `execute_embedded`'s merge-back rather than reported here.
            extra_produces: Vec::new(),
        }
    }

    /// Derives the free-output (`requires_outputs`) axis in execution order.
    ///
    /// Walks the sequential chain from `entry`, accumulating the set of output
    /// types *guaranteed* produced so far. A node's `Out<T>` read — a system
    /// parameter, a decision predicate's input, a switch discriminator's
    /// input, or a loop termination predicate's input — is free unless `T` is
    /// already in that set.
    ///
    /// What contributes to that set is exactly what the runtime guarantees, as
    /// computed by
    /// [`must_produce_output_types_from`](Self::must_produce_output_types_from): a
    /// system's own output; the union of a parallel node's branch guarantees
    /// (every branch runs and merges back); the intersection of a decision or
    /// switch's continuing-arm guarantees; and a scope's inner guarantees
    /// (inner outputs merge back on exit) — for a scope with a declared
    /// contract, the contract's `produces`, with the contract's
    /// `requires_outputs` counted as reads at the scope's own position like a
    /// system parameter. A dynamic contract is similarly complete: its
    /// `requires_outputs` count as reads at the Dynamic node's own position and
    /// its admitted candidates guarantee its contract's `produces`. A switch
    /// without a default excludes unmatched keys from the intersection because
    /// the executor stops with `NoMatchingCase` before reaching the downstream
    /// chain. Loop nodes contribute nothing. Loop and control-flow branch
    /// interiors are still inspected for their own free reads,
    /// starting with the outputs guaranteed before the branch point and then
    /// crediting production in that region's own sequential order; region-local
    /// production never escapes back into the parent chain's guaranteed set.
    /// Parallel branch interiors are inspected the same way, from the state at
    /// the branch point — a branch cannot see what a sibling deposits.
    ///
    /// Error/timeout handler subgraphs are inspected the same way, classified
    /// against what is guaranteed *before* their source node — the source
    /// failed, so its own output is never credited to its handler. `()` is
    /// never free.
    ///
    /// A loop's termination predicate counts as a read at the loop's own
    /// position: the executor evaluates it *before* the first iteration
    /// (`// Check termination predicate first` in `execute_loop`), so the
    /// loop body's outputs can never satisfy the first check. A loop whose
    /// predicate input is not *guaranteed* produced earlier on the chain
    /// therefore surfaces here as a free output the parent must supply.
    ///
    /// This derivation is deliberately more pessimistic than the executor's
    /// run-start loop check ([`Graph::unguaranteed_loop_predicate_inputs`]):
    /// a signature must never claim *less* than the runtime truth, so only
    /// guaranteed chain production is credited, while the run-start check
    /// must never fail a graph the runtime would execute, so it credits
    /// every *possible* producer.
    fn derive_requires_outputs(&self, entry: &NodeId, unit: TypeId) -> Vec<Access> {
        let seq = self.sequential_successors();

        let mut free: Vec<Access> = Vec::new();
        // Each branch or handler is its own sequential region. It inherits
        // what was guaranteed at its entry, then credits producers inside the
        // region in execution order. Regions do not merge their production
        // back into their parent because a branch/handler may not run.
        let mut pending = vec![(entry.clone(), HashSet::new())];
        // Keyed by entry plus the *sorted* guaranteed set, so recognizing an
        // already-analyzed state is a hash lookup rather than a scan of every
        // state seen so far. Sorted-vector equality is set equality here — the
        // ids are unique — so this keys exactly what the linear form did.
        let mut analyzed: HashSet<(NodeId, Vec<TypeId>)> = HashSet::new();

        while let Some((region_entry, mut produced)) = pending.pop() {
            // Shared handlers can be reached from multiple sources with
            // different guaranteed inputs. Analyze each distinct state; if
            // any state leaves a read unsatisfied it correctly remains free.
            let mut state: Vec<TypeId> = produced.iter().copied().collect();
            state.sort_unstable();
            if !analyzed.insert((region_entry.clone(), state)) {
                continue;
            }

            let mut visited: HashSet<NodeId> = HashSet::new();
            let mut current = Some(region_entry);
            while let Some(node_id) = current {
                if !visited.insert(node_id.clone()) {
                    break;
                }
                if let Some(node) = self.get_node(node_id.clone()) {
                    // The node's own reads (system parameters, predicate /
                    // discriminator / termination inputs), classified against
                    // what is guaranteed so far in this region.
                    Self::collect_node_reads(node, &produced, unit, &mut free);

                    // A handler runs only after its source fails, so it starts
                    // with the state before that source's output is credited.
                    for handler in self.handler_entries(&node_id) {
                        pending.push((handler, produced.clone()));
                    }

                    match node {
                        Node::System(sys) => {
                            let out_id = sys.output_type_id();
                            if out_id != unit {
                                produced.insert(out_id);
                            }
                        }
                        // Each control-flow interior starts with the state at
                        // the branch point and tracks its own sequential
                        // production. Only an every-arm intersection merges
                        // into the parent region's guaranteed state.
                        Node::Decision(dec) => {
                            for branch_entry in self.branch_entries(node) {
                                pending.push((branch_entry, produced.clone()));
                            }
                            if let (Some(true_entry), Some(false_entry)) =
                                (&dec.true_branch, &dec.false_branch)
                                && let Some(guaranteed) = self
                                    .intersection_of_branch_guarantees([true_entry, false_entry])
                            {
                                produced.extend(guaranteed);
                            }
                        }
                        Node::Switch(sw) => {
                            for branch_entry in self.branch_entries(node) {
                                pending.push((branch_entry, produced.clone()));
                            }
                            let entries = sw
                                .cases
                                .iter()
                                .map(|(_, entry)| entry)
                                .chain(sw.default.iter());
                            if let Some(guaranteed) =
                                self.intersection_of_branch_guarantees(entries)
                            {
                                produced.extend(guaranteed);
                            }
                        }
                        Node::Loop(_) => {
                            for branch_entry in self.branch_entries(node) {
                                pending.push((branch_entry, produced.clone()));
                            }
                        }
                        // Every parallel branch runs and merges its outputs
                        // back, so each branch's *guaranteed* production is
                        // guaranteed to this chain too. Branch interiors are
                        // still analyzed as their own regions, starting from
                        // the state at the branch point — a branch cannot see
                        // what a sibling deposits.
                        Node::Parallel(par) => {
                            for branch_entry in &par.branches {
                                pending.push((branch_entry.clone(), produced.clone()));
                            }
                            // The push loop above and the crediting call below
                            // cannot be fused or reordered: every branch region
                            // must be queued against the *pre-branch* snapshot,
                            // before any branch's guarantees enter `produced`.
                            // Fusing them would let an earlier branch satisfy a
                            // later sibling's read — which isolated child
                            // contexts never do. Pinned by
                            // `a_parallel_branch_does_not_see_a_siblings_production`.
                            self.must_produce_output_types_from_chains(
                                &par.branches,
                                &mut produced,
                            );
                        }
                        // A scope's inner outputs merge back on exit whatever
                        // its context policy, so its guaranteed production is
                        // guaranteed here. A declared contract stands in for
                        // the interior: its produces are the guaranteed set
                        // (validate() checks that), and its requires_outputs
                        // were already classified as this node's own reads. A
                        // plain scope's interior *reads* cross a separate
                        // boundary and stay out of this signature — that scope
                        // is still opaque in the requirements direction.
                        Node::Scope(scope) => match scope.contract() {
                            Some(contract) => {
                                produced.extend(
                                    contract
                                        .produces()
                                        .iter()
                                        .map(|access| access.type_id)
                                        .filter(|type_id| *type_id != unit),
                                );
                            }
                            None => scope.graph.must_produce_output_types(&mut produced),
                        },
                        // Admission verifies every candidate guarantees the
                        // slot contract's outputs, and the boundary promotes
                        // those outputs back to this chain.
                        Node::Dynamic(dynamic) => {
                            produced.extend(
                                dynamic
                                    .contract()
                                    .produces()
                                    .iter()
                                    .map(|access| access.type_id)
                                    .filter(|type_id| *type_id != unit),
                            );
                        }
                    }
                }
                current = seq.get(&node_id).cloned();
            }
        }

        free
    }

    /// The `from → to` map of this graph's [`Sequential`](Edge::Sequential)
    /// edges.
    ///
    /// Chain walks — signature derivation, the guaranteed-production analysis,
    /// the run-start loop check, the `read_from` provenance walk — advance
    /// only along sequential edges. Branch, loop-back, error and timeout
    /// edges are reached through their owning node instead, so they are
    /// deliberately absent here.
    pub(crate) fn sequential_successors(&self) -> HashMap<NodeId, NodeId> {
        self.edges()
            .iter()
            .filter_map(|edge| match edge {
                Edge::Sequential(seq) => Some((seq.from.clone(), seq.to.clone())),
                _ => None,
            })
            .collect()
    }

    /// The set of nodes carrying an [`Error`](Edge::Error) or
    /// [`Timeout`](Edge::Timeout) edge — the chain positions where a failure
    /// diverts into a handler instead of aborting the run.
    ///
    /// The guaranteed-production walk treats each as an alternative to its
    /// normal continuation; see
    /// [`must_produce_output_types_from`](Self::must_produce_output_types_from).
    /// The `read_from` provenance walk treats each as a barrier but additionally
    /// searches down its handler subgraph — see
    /// [`Graph::guaranteed_producers_before`](super::Graph::guaranteed_producers_before).
    pub(crate) fn handler_sources(&self) -> HashSet<NodeId> {
        self.edges()
            .iter()
            .filter_map(|edge| match edge {
                Edge::Error(err) => Some(err.from.clone()),
                Edge::Timeout(timeout) => Some(timeout.from.clone()),
                _ => None,
            })
            .collect()
    }

    /// Appends every output `TypeId` this graph is *guaranteed* to have
    /// deposited into the context it runs against once it completes, starting
    /// at the graph entry.
    ///
    /// See [`must_produce_output_types_from`](Self::must_produce_output_types_from)
    /// for the crediting rules.
    pub(crate) fn must_produce_output_types(&self, acc: &mut HashSet<TypeId>) {
        let Some(entry) = self.entry() else {
            return;
        };
        self.must_produce_output_types_from(&entry, acc);
    }

    /// Returns canonical output access descriptors for guaranteed `types`.
    ///
    /// The guaranteed walk uses type IDs because it needs only set algebra.
    /// This companion walk recovers the public names after that algebra,
    /// descending into plain scopes whose internal outputs are deliberately
    /// absent from the flat `produces` axis but do merge back on scope exit.
    fn accesses_for_guaranteed_types(&self, types: &HashSet<TypeId>) -> Vec<Access> {
        let unit = TypeId::of::<()>();
        let mut accesses = Vec::new();
        let mut frames = vec![self];

        while let Some(graph) = frames.pop() {
            let Some(entry) = graph.entry() else {
                continue;
            };
            for node in graph.reachable_nodes_with_handlers(&entry) {
                match node {
                    Node::System(sys) if types.contains(&sys.output_type_id()) => {
                        let type_id = sys.output_type_id();
                        if type_id != unit {
                            accesses.push(Access {
                                type_id,
                                type_name: sys.output_type_name(),
                                mode: AccessMode::Write,
                                is_global: false,
                            });
                        }
                    }
                    Node::Scope(scope) => match scope.contract() {
                        Some(contract) => accesses.extend(
                            contract
                                .produces()
                                .iter()
                                .filter(|access| types.contains(&access.type_id))
                                .cloned(),
                        ),
                        None => frames.push(&scope.graph),
                    },
                    Node::Dynamic(dynamic) => accesses.extend(
                        dynamic
                            .contract()
                            .produces()
                            .iter()
                            .filter(|access| types.contains(&access.type_id))
                            .cloned(),
                    ),
                    _ => {}
                }
            }
        }

        canonicalize(&mut accesses);
        accesses
    }

    /// Appends every output `TypeId` the sequential chain from `entry` is
    /// *guaranteed* to have deposited into the context it runs against once
    /// that chain completes.
    ///
    /// The **pessimistic dual** of
    /// [`may_produce_output_types_from`](Self::may_produce_output_types_from):
    /// that walk answers "could any execution deposit this type?", this one
    /// answers "will every execution have deposited it?". The two models stay
    /// deliberately distinct — an interface must never claim less than the
    /// runtime truth, while a run-start check must never reject a graph the
    /// runtime would execute. See
    /// [`derive_requires_outputs`](Self::derive_requires_outputs), the only
    /// consumer of this analysis, and phase rule 6 in the graph reference.
    ///
    /// Credited, because the runtime guarantees them:
    ///
    /// - a [`System`](Node::System) on the chain — its output type;
    /// - a [`Parallel`](Node::Parallel) — the union of its branches' own
    ///   guaranteed sets; every branch runs and merges its outputs back in
    ///   branch order;
    /// - a [`Decision`](Node::Decision) — the intersection of the true and
    ///   false arms' guaranteed sets, when both arms are present;
    /// - a [`Switch`](Node::Switch) — the intersection of every case arm and,
    ///   when present, the default arm. An unmatched key without a default
    ///   stops with an error before the downstream chain and is therefore not
    ///   an arm in the intersection;
    /// - a [`Scope`](Node::Scope) — its declared contract's `produces` when it
    ///   carries one ([`validate`](Graph::validate) holds the interior to
    ///   guaranteeing them, and stopping at the declaration is what keeps
    ///   derivation non-recursive), otherwise its inner graph's guaranteed
    ///   set; inner outputs merge back on scope exit whatever the
    ///   [`ContextPolicy`](crate::node::ContextPolicy) is.
    ///
    /// Credited with nothing, because the region may not run: a [`Loop`](Node::Loop)
    /// body (zero iterations is legal) and unselected control-flow regions.
    /// A [`Dynamic`](Node::Dynamic) node credits its contract's `produces`:
    /// every candidate admitted by the slot produces those types before the
    /// boundary merges them back. A guarded system intersects its normal
    /// continuation with every error and timeout handler continuation, so a
    /// handler that lacks an output (or cannot guarantee it itself) withholds
    /// that output from the result.
    ///
    /// # Handler alternatives
    ///
    /// A node carrying an [`Error`](Edge::Error) or [`Timeout`](Edge::Timeout)
    /// edge has alternatives. Its normal continuation includes the source's
    /// output; each handler continuation begins without it. The intersection
    /// is therefore the only set the enclosing graph can safely credit.
    ///
    /// The unit type is skipped: `()` is never data flow. Nested scope graphs
    /// are traversed iteratively, so arbitrarily deep nesting does not consume
    /// the call stack.
    pub(crate) fn must_produce_output_types_from(&self, entry: &NodeId, acc: &mut HashSet<TypeId>) {
        self.must_produce_output_types_from_chains(std::slice::from_ref(entry), acc);
    }

    /// [`must_produce_output_types_from`](Self::must_produce_output_types_from)
    /// over several chain entries at once, crediting their union.
    ///
    /// Walking a parallel node's branches through one call keeps the frame
    /// invariant real: this graph's sequential-edge map is built once for all
    /// of them, rather than once per branch. The result is identical to
    /// calling the single-entry form in a loop — `acc` only ever grows, and
    /// each chain entry is walked once.
    pub(crate) fn must_produce_output_types_from_chains(
        &self,
        entries: &[NodeId],
        acc: &mut HashSet<TypeId>,
    ) {
        let unit = TypeId::of::<()>();
        // One frame per graph, so a graph's sequential-edge map is built once
        // however many chains inside it are walked — a parallel node
        // contributes one chain per branch, all within its own graph.
        let mut frames: Vec<(&Graph, Vec<NodeId>)> = vec![(self, entries.to_vec())];
        while let Some((graph, chain_entries)) = frames.pop() {
            let seq = graph.sequential_successors();
            let mut pending = chain_entries;
            let mut walked: HashSet<NodeId> = HashSet::new();

            while let Some(chain_entry) = pending.pop() {
                // Walking each chain entry once keeps a hand-wired graph whose
                // chains route into each other finite. The result is
                // unaffected: `acc` only ever grows.
                if !walked.insert(chain_entry.clone()) {
                    continue;
                }
                let mut visited: HashSet<NodeId> = HashSet::new();
                let mut current = Some(chain_entry);
                while let Some(node_id) = current {
                    if !visited.insert(node_id.clone()) {
                        break;
                    }
                    if !graph.handler_entries(&node_id).is_empty() {
                        // A guarded system either succeeds along its ordinary
                        // continuation or diverts to one of its handlers. The
                        // enclosing graph can rely only on outputs every one of
                        // those successful continuations deposits.
                        let mut normal = HashSet::new();
                        if let Some(Node::System(sys)) = graph.get_node(node_id.clone()) {
                            let out_id = sys.output_type_id();
                            if out_id != unit {
                                normal.insert(out_id);
                            }
                        }
                        if let Some(next) = seq.get(&node_id) {
                            graph.must_produce_output_types_from(next, &mut normal);
                        }

                        let mut guaranteed = normal;
                        for handler in graph.handler_entries(&node_id) {
                            let mut handler_guarantees = HashSet::new();
                            graph.must_produce_output_types_from(&handler, &mut handler_guarantees);
                            guaranteed.retain(|type_id| handler_guarantees.contains(type_id));
                        }
                        acc.extend(guaranteed);
                        break;
                    }
                    match graph.get_node(node_id.clone()) {
                        Some(Node::System(sys)) => {
                            let out_id = sys.output_type_id();
                            if out_id != unit {
                                acc.insert(out_id);
                            }
                        }
                        Some(Node::Parallel(par)) => {
                            pending.extend(par.branches.iter().cloned());
                        }
                        Some(Node::Scope(scope)) => match scope.contract() {
                            // The declared contract is the interface: its
                            // produces are what the scope guarantees (validate()
                            // holds the interior to that), so derivation stops
                            // at the declaration instead of descending.
                            Some(contract) => {
                                acc.extend(
                                    contract
                                        .produces()
                                        .iter()
                                        .map(|access| access.type_id)
                                        .filter(|type_id| *type_id != unit),
                                );
                            }
                            None => {
                                if let Some(inner_entry) = scope.graph.entry() {
                                    frames.push((&scope.graph, vec![inner_entry]));
                                }
                            }
                        },
                        Some(Node::Decision(dec)) => {
                            if let (Some(true_entry), Some(false_entry)) =
                                (&dec.true_branch, &dec.false_branch)
                                && let Some(guaranteed) = graph
                                    .intersection_of_branch_guarantees([true_entry, false_entry])
                            {
                                acc.extend(guaranteed);
                            }
                        }
                        Some(Node::Switch(sw)) => {
                            let entries = sw
                                .cases
                                .iter()
                                .map(|(_, entry)| entry)
                                .chain(sw.default.iter());
                            if let Some(guaranteed) =
                                graph.intersection_of_branch_guarantees(entries)
                            {
                                acc.extend(guaranteed);
                            }
                        }
                        Some(Node::Loop(_)) | None => {}
                        Some(Node::Dynamic(dynamic)) => {
                            acc.extend(
                                dynamic
                                    .contract()
                                    .produces()
                                    .iter()
                                    .map(|access| access.type_id)
                                    .filter(|type_id| *type_id != unit),
                            );
                        }
                    }
                    current = seq.get(&node_id).cloned();
                }
            }
        }
    }

    /// Returns the outputs every supplied branch entry guarantees on a
    /// successful return to its enclosing chain.
    ///
    /// `None` represents no continuable arms. It deliberately differs from a
    /// mathematical intersection over an empty family: no arm can establish a
    /// concrete output guarantee. Each arm uses the ordinary guaranteed walk,
    /// including its guarded continuations, before their sets are intersected.
    fn intersection_of_branch_guarantees<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a NodeId>,
    ) -> Option<HashSet<TypeId>> {
        let mut entries = entries.into_iter();
        let first = entries.next()?;
        let mut intersection = HashSet::new();
        self.must_produce_output_types_from(first, &mut intersection);
        for entry in entries {
            let mut branch_guarantees = HashSet::new();
            self.must_produce_output_types_from(entry, &mut branch_guarantees);
            intersection.retain(|type_id| branch_guarantees.contains(type_id));
        }
        Some(intersection)
    }

    /// Returns the main-chain loops whose termination predicate input cannot
    /// be produced by *any* node earlier on the sequential chain.
    ///
    /// The executor evaluates a loop's termination predicate *before* the
    /// first iteration, so the input must already exist when the loop is
    /// reached: deposited by an earlier chain node, or present in the context
    /// the graph runs in. This lists the loops for which no earlier chain
    /// node can deposit the input; the executor checks them against the live
    /// context at each execution boundary and fails fast on any still missing
    /// ([`ExecutionError::LoopPredicateInputMissingOnEntry`]).
    ///
    /// Crediting is *optimistic*, mirroring the executor's pre-flight
    /// (`validate_output_reachability`): a preceding system credits its
    /// output; decision / switch / loop / parallel nodes credit every output
    /// type reachable in their subgraphs (parallel branches all run and merge
    /// back; conditional branches *may*); scope and dynamic nodes credit what
    /// may merge back across their boundary
    /// ([`Graph::may_produce_output_types`] / the contract's `produces`).
    /// Optimism is deliberate: this feeds a hard run-start error, so
    /// under-crediting would reject compositions the runtime executes fine,
    /// while over-crediting merely defers a genuinely missing input to the
    /// loop's first predicate evaluation
    /// ([`PredicateError::OutputNotFound`](crate::predicate::PredicateError::OutputNotFound)).
    /// The signature's `requires_outputs` derivation is the opposite,
    /// pessimistic model — see
    /// [`derive_requires_outputs`](Self::derive_requires_outputs).
    ///
    /// Loops inside branch interiors execute conditionally and are not
    /// listed — a missing input there stays a mid-run predicate error. The
    /// unit type is skipped, mirroring signature derivation.
    ///
    /// The walk is a pure function of the graph structure, and this runs at
    /// every execution boundary (including per-iteration re-entry of
    /// boundaries nested in loops), so the result is memoized in
    /// [`Graph::loop_entry_cache`](super::Graph): re-entries pay only the
    /// per-entry context probe. Validity is keyed on `(node_count,
    /// edge_count)` — every builder mutation appends nodes or edges, so a
    /// shape change forces recomputation.
    ///
    /// [`ExecutionError::LoopPredicateInputMissingOnEntry`]: crate::executor::ExecutionError::LoopPredicateInputMissingOnEntry
    pub(crate) fn unguaranteed_loop_predicate_inputs(&self) -> Arc<[UnguaranteedLoopInput]> {
        let shape = (self.nodes.len(), self.edges.len());
        if let Some(cache) = self
            .loop_entry_cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && cache.shape == shape
        {
            return Arc::clone(&cache.inputs);
        }
        let inputs: Arc<[UnguaranteedLoopInput]> =
            self.compute_unguaranteed_loop_predicate_inputs().into();
        *self
            .loop_entry_cache
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(LoopEntryCache {
            shape,
            inputs: Arc::clone(&inputs),
        });
        inputs
    }

    /// The uncached walk behind
    /// [`unguaranteed_loop_predicate_inputs`](Self::unguaranteed_loop_predicate_inputs).
    fn compute_unguaranteed_loop_predicate_inputs(&self) -> Vec<UnguaranteedLoopInput> {
        // Cheap pre-pass: most boundary graphs contain no loop node at all —
        // skip building the edge map and walking for them.
        if !self
            .nodes()
            .iter()
            .any(|node| matches!(node, Node::Loop(_)))
        {
            return Vec::new();
        }
        let Some(entry) = self.entry() else {
            return Vec::new();
        };

        let seq = self.sequential_successors();

        let unit = TypeId::of::<()>();
        let mut produced: HashSet<TypeId> = HashSet::new();
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut unguaranteed = Vec::new();
        let mut current = Some(entry);

        while let Some(node_id) = current {
            if !visited.insert(node_id.clone()) {
                break;
            }
            if let Some(node) = self.get_node(node_id.clone()) {
                match node {
                    Node::System(sys) => {
                        produced.insert(sys.output_type_id());
                    }
                    Node::Decision(_) | Node::Switch(_) | Node::Parallel(_) => {
                        for branch in self.branch_entries(node) {
                            self.may_produce_output_types_from(&branch, &mut produced);
                        }
                    }
                    Node::Loop(lp) => {
                        // Check the loop's own input against what precedes it
                        // *before* crediting its body: the predicate runs
                        // before the first iteration.
                        if let Some(termination) = &lp.termination {
                            let input = termination.input_type_id();
                            if input != unit && !produced.contains(&input) {
                                unguaranteed.push(UnguaranteedLoopInput {
                                    node: lp.id.clone(),
                                    name: lp.name.clone(),
                                    input_type_id: input,
                                    input_type_name: termination.input_type_name(),
                                });
                            }
                        }
                        for branch in self.branch_entries(node) {
                            self.may_produce_output_types_from(&branch, &mut produced);
                        }
                    }
                    // A non-`shared()` declared scope merges back only its
                    // contract's `produces` (the runtime filter in
                    // `execute_embedded`); every other scope merges back
                    // whatever its interior wrote.
                    Node::Scope(scope) => match scope.contract() {
                        Some(contract) if !scope.context_policy().is_shared() => {
                            produced.extend(contract.produces().iter().map(|a| a.type_id));
                        }
                        _ => scope.graph.may_produce_output_types(&mut produced),
                    },
                    // The contract's `produces` are the outputs every
                    // admissible candidate merges back.
                    Node::Dynamic(dynamic) => {
                        produced.extend(dynamic.contract().produces().iter().map(|a| a.type_id));
                    }
                }
            }
            current = seq.get(&node_id).cloned();
        }
        unguaranteed
    }

    /// Pushes each of `node`'s output reads that is neither `()` nor already in
    /// `produced` onto `free`: a system's declared output parameters, a
    /// decision predicate's input, a switch discriminator's input, a loop
    /// termination predicate's input (evaluated before the first iteration —
    /// see [`derive_requires_outputs`](Self::derive_requires_outputs)), or a
    /// declared scope or Dynamic slot contract's `requires_outputs`.
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
            Node::Loop(lp) => {
                if let Some(termination) = &lp.termination {
                    push_read(termination.input_type_id(), termination.input_type_name());
                }
            }
            // A declared boundary's contract requires_outputs are the free
            // reads its interior makes across the boundary, so they classify
            // at the boundary's chain position exactly like a system's own
            // reads.
            Node::Scope(scope) => {
                if let Some(contract) = scope.contract() {
                    for access in contract.requires_outputs() {
                        push_read(access.type_id, access.type_name);
                    }
                }
            }
            Node::Dynamic(dynamic) => {
                for access in dynamic.contract().requires_outputs() {
                    push_read(access.type_id, access.type_name);
                }
            }
            Node::Parallel(_) => {}
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
    use crate::graph::{Graph, ValidationError};
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

    /// Reads `Out<Final>`, used to verify a guarded boundary's downstream
    /// production credit.
    struct ReadsFinal;
    impl System for ReadsFinal {
        type Output = Mid;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "reads_final"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new().with_output::<Final>()
        }
    }

    /// Reads `Out<Mid>` at runtime, returning the observed value as `Final`.
    struct RuntimeReadsMid;
    impl System for RuntimeReadsMid {
        type Output = Final;
        fn run<'a>(
            &'a self,
            ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async move {
                use polaris_system::param::{Out, SystemParam};

                Ok(u16::from(*Out::<Mid>::fetch(ctx)?))
            })
        }
        fn name(&self) -> &'static str {
            "runtime_reads_mid"
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
    fn every_decision_arm_produced_output_is_credited_to_a_downstream_read() {
        // Both branches produce `Mid`; a system after the branch reads `Out<Mid>`.
        // One arm is selected, but every execution that continues reaches a
        // producer first, so the downstream read is internally satisfied.
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
            !free.contains(&TypeId::of::<Mid>()),
            "every decision arm produces Mid before the downstream read: {free:?}"
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

    // ── Substitutability (`substitutable_for`) ──────────────────────────────

    #[test]
    fn substitutable_admits_a_candidate_needing_less_and_producing_more() {
        // The escalation-ladder case: a cheap rung reads only part of what the
        // slot guarantees and still delivers the verdict downstream expects.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .require_read::<Seed>()
            .require_output::<HandlerRes>()
            .produce::<Final>();
        let cheap_rung = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>()
            .produce::<Seed>(); // extra output merges back harmlessly

        assert!(cheap_rung.substitutable_for(&slot));
        assert!(
            !cheap_rung.compatible_with(&slot),
            "exact match rejects the cheap rung — the case this relation exists for"
        );
    }

    #[test]
    fn substitutable_rejects_a_candidate_needing_more_that_satisfies_admits() {
        // The polarity that makes this a third relation rather than a reuse of
        // `satisfies`. A `Dynamic` slot's environment is the *parent graph*,
        // validated against the contract alone, so a candidate read the slot
        // never named has nothing to supply it and must be refused.
        // `satisfies` admits exactly this case, because a capability
        // registrant's own `setup` is presumed to provide the extra.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();
        let greedy = GraphSignature::new()
            .require_read::<Mid>()
            .require_read::<Seed>()
            .produce::<Final>();

        assert!(!greedy.substitutable_for(&slot));
        assert!(
            greedy.satisfies(&slot),
            "subsumption admits it — which is why `satisfies` is unsound here"
        );
    }

    #[test]
    fn substitutable_compares_requires_by_mode_as_well_as_type() {
        // No write-subsumes-read relaxation, in either direction — mirroring
        // `satisfies`. A slot guaranteeing `Res<T>` says nothing about
        // mutability, and a slot guaranteeing `ResMut<T>` is not answered by a
        // candidate that only reads: the two are distinct declarations.
        let read_slot = GraphSignature::new().require_read::<Mid>();
        let write_candidate = GraphSignature::new().require_write::<Mid>();
        assert!(!write_candidate.substitutable_for(&read_slot));

        let write_slot = GraphSignature::new().require_write::<Mid>();
        let read_candidate = GraphSignature::new().require_read::<Mid>();
        assert!(!read_candidate.substitutable_for(&write_slot));
    }

    #[test]
    fn substitutable_bounds_free_output_reads_by_the_slot() {
        let slot = GraphSignature::new().require_output::<Seed>();

        // Reading fewer free outputs than the slot supplies is fine…
        assert!(GraphSignature::new().substitutable_for(&slot));
        assert!(slot.substitutable_for(&slot));

        // …but a free read the slot never sanctioned would dangle at runtime,
        // because the parent only deposits what the contract names.
        let reads_extra = GraphSignature::new()
            .require_output::<Seed>()
            .require_output::<Mid>();
        assert!(!reads_extra.substitutable_for(&slot));
    }

    #[test]
    fn substitutable_produces_is_covariant() {
        let slot = GraphSignature::new().produce::<Final>();

        // A superset produces everything downstream was validated against.
        let superset = GraphSignature::new().produce::<Final>().produce::<Mid>();
        assert!(superset.substitutable_for(&slot));

        // A subset drops an output downstream expects.
        let subset = GraphSignature::new();
        assert!(!subset.substitutable_for(&slot));

        // The mirror image on the `requires` axis, to pin both directions of
        // the contravariance in one place.
        let requires_slot = GraphSignature::new().require_read::<Mid>();
        assert!(GraphSignature::new().substitutable_for(&requires_slot));
        assert!(
            !requires_slot
                .clone()
                .require_read::<Seed>()
                .substitutable_for(&requires_slot)
        );
    }

    #[test]
    fn compatible_signatures_are_always_substitutable() {
        // Exact match is the degenerate case of substitutability, so opting a
        // slot into `MatchingMode::Substitutable` can only ever widen the
        // admissible set — never move a candidate from admitted to rejected.
        let signature = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>();
        assert!(signature.compatible_with(&signature.clone()));
        assert!(signature.substitutable_for(&signature.clone()));
    }

    #[test]
    fn substitutable_for_agrees_with_empty_substitutable_diff() {
        // The lockstep invariant, mirrored from `compatible_with` ⇔ `diff` and
        // `satisfies` ⇔ `satisfies_diff`: the predicate and its diff mode must
        // never disagree, so a rejection always names the real violation.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>();

        for candidate in [
            // Substitutable: exact, fewer requires, fewer free reads, extra
            // produces.
            slot.clone(),
            GraphSignature::new()
                .require_output::<Seed>()
                .produce::<Final>(),
            GraphSignature::new()
                .require_read::<Mid>()
                .produce::<Final>(),
            slot.clone().produce::<Seed>(),
            // Violating: each axis in turn.
            slot.clone().require_write::<HandlerRes>(), // unguaranteed require
            slot.clone().require_output::<Mid>(),       // unsanctioned free read
            GraphSignature::new()
                .require_read::<Mid>()
                .require_output::<Seed>(), // missing slot produce
        ] {
            assert_eq!(
                candidate.substitutable_for(&slot),
                candidate.substitutable_diff(&slot).is_empty(),
                "substitutable_for must equal substitutable_diff-is-empty for {candidate:?}"
            );
        }
    }

    #[test]
    fn substitutable_diff_reports_only_violating_directions() {
        // The candidate needs fewer resources and fewer free reads than the
        // slot (both permitted), but demands an unguaranteed resource, reads an
        // unsanctioned free output, and drops one of the slot's produces.
        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .require_output::<Seed>()
            .produce::<Final>()
            .produce::<Mid>();
        let candidate = GraphSignature::new()
            .require_read::<HandlerRes>() // violation: the slot never guarantees it
            .require_output::<Final>() // violation: unsanctioned free read
            .produce::<Mid>(); // Final is missing

        let diff = candidate.substitutable_diff(&slot);
        assert!(!diff.is_empty());
        // Permitted divergence never appears.
        assert!(
            diff.missing_requires().is_empty(),
            "needing less is permitted: {diff}"
        );
        assert!(diff.missing_requires_outputs().is_empty());
        assert!(diff.extra_produces().is_empty());
        // Violations are named, per axis.
        assert_eq!(diff.extra_requires().len(), 1);
        assert_eq!(diff.extra_requires()[0].type_id, TypeId::of::<HandlerRes>());
        assert_eq!(diff.extra_requires_outputs().len(), 1);
        assert_eq!(
            diff.extra_requires_outputs()[0].type_id,
            TypeId::of::<Final>()
        );
        assert_eq!(diff.missing_produces().len(), 1);
        assert_eq!(diff.missing_produces()[0].type_id, TypeId::of::<Final>());
    }

    #[test]
    fn derived_signature_of_a_cheap_rung_substitutes_for_a_richer_slot() {
        // The end-to-end shape: a real graph whose derived signature reads less
        // than the slot guarantees is a legal substitute, and exact-set match
        // is what rejects it today.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));

        let slot = GraphSignature::new()
            .require_read::<Seed>()
            .produce::<Mid>();
        assert!(graph.signature().substitutable_for(&slot));
        assert!(!graph.signature().compatible_with(&slot));
    }

    // ── MatchingMode mode dispatch ──────────────────────────────────────────────

    #[test]
    fn matching_defaults_to_exact_and_dispatches_to_its_relation() {
        assert_eq!(MatchingMode::default(), MatchingMode::Exact);

        let slot = GraphSignature::new()
            .require_read::<Mid>()
            .require_read::<Seed>()
            .produce::<Final>();
        let cheap = GraphSignature::new()
            .require_read::<Mid>()
            .produce::<Final>();

        assert!(!MatchingMode::Exact.admits(&cheap, &slot));
        assert!(MatchingMode::Substitutable.admits(&cheap, &slot));

        // Each mode's diff is its relation's diff, and stays in lockstep.
        for mode in [MatchingMode::Exact, MatchingMode::Substitutable] {
            assert_eq!(
                mode.admits(&cheap, &slot),
                mode.diff(&cheap, &slot).is_empty(),
                "{mode:?} must keep admits and diff in lockstep"
            );
        }
        assert_eq!(MatchingMode::Exact.diff(&cheap, &slot), cheap.diff(&slot));
        assert_eq!(
            MatchingMode::Substitutable.diff(&cheap, &slot),
            cheap.substitutable_diff(&slot)
        );
    }

    #[test]
    fn matching_mode_renders_the_relation_it_names() {
        // The mismatch messages interpolate this, so a rename must not silently
        // change what an error says the check was.
        assert_eq!(MatchingMode::Exact.to_string(), "Exact");
        assert_eq!(MatchingMode::Substitutable.to_string(), "Substitutable");
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
        assert_eq!(rendered.guaranteed_produces, vec!["Out<u16>"]);
    }

    #[test]
    fn display_renders_all_axes_even_when_empty() {
        let signature = GraphSignature::new().produce::<Final>();
        assert_eq!(
            signature.to_string(),
            "requires: []; requires_outputs: []; produces: [Out<u16>]; guaranteed_produces: [Out<u16>]"
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
    fn loop_termination_input_is_a_free_read_when_unproduced() {
        // The termination predicate is evaluated *before* the first
        // iteration, so the body's `ProduceMid` can never satisfy the first
        // check — the loop genuinely needs `Mid` supplied on entry, and the
        // interface must say so.
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
            free.contains(&TypeId::of::<Mid>()),
            "the termination predicate's input is a runtime read on entry: {free:?}"
        );
    }

    #[test]
    fn loop_termination_input_produced_upstream_is_not_free() {
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceMid));
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
            "a termination input guaranteed upstream is internally satisfied: {free:?}"
        );
    }

    #[test]
    fn loop_termination_input_produced_by_parallel_is_not_free() {
        // Every parallel branch runs and merges its outputs back, so a branch
        // chain that guarantees `Mid` guarantees it to the downstream loop
        // too: the interface no longer advertises a seed the graph supplies
        // itself. Both crediting models now agree on this graph — the
        // pessimistic interface and the optimistic run-start check.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            vec![Box::new(|g: &mut Graph| {
                g.add_boxed_system(Box::new(ProduceMid));
            }) as Box<dyn FnOnce(&mut Graph)>],
        );
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
            free.is_empty(),
            "guaranteed parallel production satisfies the downstream loop, \
             and nothing else is free: {free:?}"
        );
        assert!(
            graph.unguaranteed_loop_predicate_inputs().is_empty(),
            "the entry check credits parallel production"
        );
    }

    #[test]
    fn dynamic_contract_produces_credits_downstream_loop_entry() {
        // A dynamic node's contract `produces` names exactly what every
        // admissible candidate merges back, so a downstream main-chain loop
        // reading that type is credited by the entry check.
        use crate::node::{ContextPolicy, DynamicSlot};

        let mut candidate = Graph::new();
        candidate.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| "only",
            [("only", candidate)],
            DynamicSlot::new(
                GraphSignature::new().produce::<Mid>(),
                ContextPolicy::shared(),
            ),
        );
        graph.add_loop::<Mid, _, _>(
            "loop",
            |_mid| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        assert!(
            graph.unguaranteed_loop_predicate_inputs().is_empty(),
            "the entry check credits the dynamic contract's produces"
        );
    }

    #[test]
    fn loop_body_outputs_credit_downstream_loop_after_own_check() {
        // A loop's body outputs are credited *after* its own predicate input
        // is checked: the first loop still needs `Seed` on entry, but its
        // body's `Mid` satisfies the second loop downstream.
        let mut graph = Graph::new();
        graph.add_loop::<Seed, _, _>(
            "first",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        graph.add_loop::<Mid, _, _>(
            "second",
            |_mid| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let unguaranteed = graph.unguaranteed_loop_predicate_inputs();
        assert_eq!(
            unguaranteed.len(),
            1,
            "only the first loop's input is unguaranteed: {unguaranteed:?}"
        );
        assert_eq!(unguaranteed[0].name, "first", "the entry names the loop");
        assert_eq!(
            unguaranteed[0].input_type_id,
            TypeId::of::<Seed>(),
            "the entry names the missing input type"
        );
    }

    #[test]
    fn unit_predicate_input_is_skipped_by_entry_check() {
        // `()` is never data flow, mirroring signature derivation: a loop
        // whose predicate reads `()` is not entry-checked.
        let mut graph = Graph::new();
        graph.add_loop::<(), _, _>(
            "unit_loop",
            |_unit| true,
            |g| {
                g.add_boxed_system(Box::new(SideEffect));
            },
        );

        assert!(
            graph.unguaranteed_loop_predicate_inputs().is_empty(),
            "a unit predicate input is never entry-checked"
        );
    }

    #[test]
    fn unguaranteed_loop_inputs_are_memoized_until_the_graph_changes() {
        // Same shape → the memoized analysis is reused (pointer-equal Arc);
        // a builder mutation changes the shape and forces recomputation.
        let mut graph = Graph::new();
        graph.add_loop::<Mid, _, _>(
            "loop",
            |_mid| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let first = graph.unguaranteed_loop_predicate_inputs();
        let second = graph.unguaranteed_loop_predicate_inputs();
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged graph reuses the cached analysis"
        );
        assert_eq!(first.len(), 1, "the loop's input is unguaranteed");

        // Appending another unseeded loop invalidates the cache: the fresh
        // walk reports both.
        graph.add_loop::<Seed, _, _>(
            "appended",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        let third = graph.unguaranteed_loop_predicate_inputs();
        assert_eq!(
            third.len(),
            2,
            "a builder mutation forces recomputation: {third:?}"
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
                // The loop's own termination input (`Final`) also surfaces as
                // free — covered by `loop_termination_input_is_a_free_read_
                // when_unproduced`; here we assert the interior read.
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

    #[test]
    fn branch_local_producers_satisfy_later_loop_inputs() {
        // Every execution region must respect its own sequential order. The
        // producer is conditional from the parent's perspective, so it must
        // not satisfy a read *after* the control-flow node; within the same
        // branch/body/handler, however, it is guaranteed to run before the
        // nested loop and therefore satisfies that loop's first check.
        let cases: [(&str, Box<dyn FnOnce(&mut Graph)>); 5] = [
            (
                "decision",
                Box::new(|graph| {
                    graph.add_conditional_branch::<Seed, _, _, _>(
                        "branch",
                        |_seed| true,
                        |branch| {
                            branch.add_boxed_system(Box::new(ProduceMid));
                            branch.add_loop::<Mid, _, _>(
                                "nested_loop",
                                |_mid| true,
                                |body| {
                                    body.add_boxed_system(Box::new(ProduceMid));
                                },
                            );
                        },
                        |branch| {
                            branch.add_boxed_system(Box::new(SideEffect));
                        },
                    );
                }),
            ),
            (
                "switch",
                Box::new(|graph| {
                    let case: fn(&mut Graph) = |branch| {
                        branch.add_boxed_system(Box::new(ProduceMid));
                        branch.add_loop::<Mid, _, _>(
                            "nested_loop",
                            |_mid| true,
                            |body| {
                                body.add_boxed_system(Box::new(ProduceMid));
                            },
                        );
                    };
                    graph.add_switch::<Seed, _, _, _>(
                        "switch",
                        |_seed| "case",
                        [("case", case)],
                        None,
                    );
                }),
            ),
            (
                "loop body",
                Box::new(|graph| {
                    graph.add_loop::<Seed, _, _>(
                        "outer_loop",
                        |_seed| true,
                        |outer_body| {
                            outer_body.add_boxed_system(Box::new(ProduceMid));
                            outer_body.add_loop::<Mid, _, _>(
                                "nested_loop",
                                |_mid| true,
                                |inner_body| {
                                    inner_body.add_boxed_system(Box::new(ProduceMid));
                                },
                            );
                            outer_body.add_boxed_system(Box::new(ProduceSeed));
                        },
                    );
                }),
            ),
            (
                "parallel branch",
                Box::new(|graph| {
                    graph.add_parallel(
                        "parallel",
                        [|branch: &mut Graph| {
                            branch.add_boxed_system(Box::new(ProduceMid));
                            branch.add_loop::<Mid, _, _>(
                                "nested_loop",
                                |_mid| true,
                                |body| {
                                    body.add_boxed_system(Box::new(ProduceMid));
                                },
                            );
                        }],
                    );
                }),
            ),
            (
                "error handler",
                Box::new(|graph| {
                    graph.add_boxed_system(Box::new(FallibleFinal));
                    graph.add_error_handler(|handler| {
                        handler.add_boxed_system(Box::new(ProduceMid));
                        handler.add_loop::<Mid, _, _>(
                            "nested_loop",
                            |_mid| true,
                            |body| {
                                body.add_boxed_system(Box::new(ProduceMid));
                            },
                        );
                    });
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
                .map(|access| access.type_id)
                .collect();
            assert!(
                !free.contains(&TypeId::of::<Mid>()),
                "{label}: the branch-local producer runs before the nested loop: {free:?}"
            );
        }
    }

    #[test]
    fn branch_local_read_before_producer_stays_required() {
        let mut graph = Graph::new();
        graph.add_conditional_branch::<Seed, _, _, _>(
            "branch",
            |_seed| true,
            |branch| {
                branch.add_boxed_system(Box::new(ReadsMid));
                branch.add_boxed_system(Box::new(ProduceMid));
            },
            |branch| {
                branch.add_boxed_system(Box::new(SideEffect));
            },
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|access| access.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "a later branch-local producer cannot satisfy an earlier read: {free:?}"
        );
    }

    // ── Guaranteed production across parallel and scope boundaries ──────────

    #[test]
    fn scope_guaranteed_production_credits_a_downstream_read() {
        use crate::node::ContextPolicy;

        // A scope's inner outputs merge back on exit whatever its context
        // policy — this one is fully isolated — so an inner chain that
        // guarantees `Mid` guarantees it to the enclosing chain. The boundary
        // stays opaque in every other direction: the interior's production
        // does not enter the parent's `produces` axis, and its reads never
        // enter `requires` / `requires_outputs`.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsMid));

        let sig = graph.signature();
        assert!(
            sig.requires_outputs().is_empty(),
            "the scope guarantees the downstream read: {sig}"
        );
        let produced: Vec<TypeId> = sig.produces().iter().map(|a| a.type_id).collect();
        assert_eq!(
            produced,
            vec![TypeId::of::<Final>()],
            "only the outer chain's own production is advertised: {sig}"
        );
    }

    #[test]
    fn scope_guaranteed_production_credits_a_downstream_loop() {
        use crate::node::ContextPolicy;

        // The same crediting satisfies a loop's first termination check, which
        // is evaluated before its body has run. Both models agree here: the
        // pessimistic interface no longer advertises the seed, and the
        // optimistic run-start check never did.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
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
            free.is_empty(),
            "the scope's merged-back output satisfies the loop, \
             and nothing else is free: {free:?}"
        );
        assert!(
            graph.unguaranteed_loop_predicate_inputs().is_empty(),
            "the entry check agrees with the signature"
        );
    }

    #[test]
    fn guarantees_are_credited_through_nested_parallels_and_scopes() {
        use crate::node::ContextPolicy;

        // The rule is recursive, so it holds in both nestings.
        let cases: [(&str, Box<dyn FnOnce(&mut Graph)>); 2] = [
            (
                // The branch runs, the scope merges back, so `Mid` reaches the
                // chain after the parallel.
                "scope inside a parallel branch",
                Box::new(|graph| {
                    let mut scoped = Graph::new();
                    scoped.add_boxed_system(Box::new(ProduceMid));
                    graph.add_parallel(
                        "fan_out",
                        [move |g: &mut Graph| {
                            g.add_scope("seed", scoped, ContextPolicy::new());
                        }],
                    );
                }),
            ),
            (
                // Every inner branch runs, and the scope merges the joined
                // outputs back.
                "parallel inside a scope",
                Box::new(|graph| {
                    let mut inner = Graph::new();
                    inner.add_parallel(
                        "inner_fan_out",
                        [|g: &mut Graph| {
                            g.add_boxed_system(Box::new(ProduceMid));
                        }],
                    );
                    graph.add_scope("seed", inner, ContextPolicy::new());
                }),
            ),
        ];

        for (label, build) in cases {
            let mut graph = Graph::new();
            build(&mut graph);
            graph.add_boxed_system(Box::new(ReadsMid));
            assert!(
                graph.signature().requires_outputs().is_empty(),
                "{label} is credited: {}",
                graph.signature()
            );
        }
    }

    #[test]
    fn every_arm_production_inside_a_parallel_branch_is_credited() {
        // The branch itself runs, and every decision arm inside it produces
        // `Mid`, so the parallel's joined output guarantees it downstream.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            [|g: &mut Graph| {
                g.add_conditional_branch::<Seed, _, _, _>(
                    "inner_choice",
                    |_seed| true,
                    |t| {
                        t.add_boxed_system(Box::new(ProduceMid));
                    },
                    |f| {
                        f.add_boxed_system(Box::new(ProduceMid));
                    },
                );
            }],
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "an intersection within a parallel branch is guaranteed: {free:?}"
        );
    }

    #[test]
    fn every_arm_production_inside_a_scope_is_credited() {
        use crate::node::ContextPolicy;

        // Same rule one boundary down: the scope always runs, and both arms in
        // its interior produce `Mid`.
        let mut inner = Graph::new();
        inner.add_conditional_branch::<Seed, _, _, _>(
            "inner_choice",
            |_seed| true,
            |t| {
                t.add_boxed_system(Box::new(ProduceMid));
            },
            |f| {
                f.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let mut graph = Graph::new();
        graph.add_scope("maybe_seed", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "a scope credits its inner arm intersection: {free:?}"
        );
    }

    #[test]
    fn a_parallel_branch_does_not_see_a_siblings_production() {
        // Branches run against isolated child contexts and merge only at the
        // join, so a reader in one branch cannot be satisfied by a producer in
        // another — even though the join does guarantee `Mid` to the chain
        // *after* the parallel.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            vec![
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(ProduceMid));
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(ReadsMid));
                }),
            ],
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "a sibling branch's production is not credited: {free:?}"
        );
    }

    #[test]
    fn scope_production_is_not_credited_to_its_own_handler() {
        use crate::node::ContextPolicy;

        // A handler runs only because its source failed, and a failed scope
        // merges nothing back, so the scope's guaranteed production must not
        // satisfy a read inside its own handler.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
        let scope_id = graph.nodes()[0].id();
        graph.add_error_handler_for(scope_id, |g| {
            g.add_boxed_system(Box::new(ReadsMid));
        });

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "the handler is classified against the state before the scope: {free:?}"
        );
    }

    #[test]
    fn parallel_production_is_not_credited_to_its_own_handler() {
        // The symmetric case one node kind over: a handler runs only because
        // the parallel failed, and a failed parallel cancels its siblings and
        // merges nothing back, so the join's guarantee must not reach it.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            [|g: &mut Graph| {
                g.add_boxed_system(Box::new(ProduceMid));
            }],
        );
        let parallel_id = graph.last_node().expect("parallel was added");
        graph.add_error_handler_for(parallel_id, |g| {
            g.add_boxed_system(Box::new(ReadsMid));
        });

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "the handler is classified against the state before the parallel: {free:?}"
        );
    }

    // ── Guarded paths credit only their shared production ───────────────────

    #[test]
    fn production_behind_an_error_handler_inside_a_parallel_branch_is_not_credited() {
        // A caught failure does not abort the branch: it diverts into the
        // handler, whose chain ends there, so the branch completes and merges
        // back *without* `Mid` — and the parent chain carries on regardless.
        // Crediting `Mid` would under-claim, which phase rule 4 forbids.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            [|g: &mut Graph| {
                g.add_boxed_system(Box::new(ProduceMid));
                let risky = g.last_node().expect("producer was added");
                g.add_error_handler_for(risky, |h| {
                    h.add_boxed_system(Box::new(SideEffect));
                });
            }],
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "a caught failure inside the branch leaves Mid unproduced: {free:?}"
        );
    }

    #[test]
    fn production_behind_an_error_handler_inside_a_scope_is_not_credited() {
        use crate::node::ContextPolicy;

        // Same rule one boundary down. `execute_embedded` merges the child
        // outputs back whenever the interior returns `Ok`, and a recovered
        // failure does return `Ok`.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        let risky = inner.last_node().expect("producer was added");
        inner.add_error_handler_for(risky, |h| {
            h.add_boxed_system(Box::new(SideEffect));
        });

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "a caught failure inside the scope leaves Mid unproduced: {free:?}"
        );
    }

    #[test]
    fn production_behind_a_timeout_handler_is_not_credited() {
        use crate::node::ContextPolicy;
        use std::time::Duration;

        // A timeout handler follows the same intersection rule as an error
        // handler; this handler does not produce Mid.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        let slow = inner.last_node().expect("producer was added");
        inner.set_timeout(slow.clone(), Duration::from_secs(1));
        inner.add_timeout_handler(slow, |h| {
            h.add_boxed_system(Box::new(SideEffect));
        });

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsMid));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "a recovered timeout inside the scope leaves Mid unproduced: {free:?}"
        );
    }

    #[test]
    fn production_after_a_handler_missing_an_output_is_not_credited() {
        use crate::node::ContextPolicy;

        // The handler ends the region, so the normal path's later producer
        // never runs after recovery.
        // `ProduceFinal` downstream reads both `Mid` and `Seed`; neither is
        // guaranteed, so both stay free.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        let risky = inner.last_node().expect("producer was added");
        inner.add_error_handler_for(risky, |h| {
            h.add_boxed_system(Box::new(SideEffect));
        });
        inner.add_boxed_system(Box::new(ProduceSeed));

        let mut graph = Graph::new();
        graph.add_scope("seed", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ProduceFinal));

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|a| a.type_id)
            .collect();
        assert!(
            free.contains(&TypeId::of::<Mid>()),
            "the handler path does not guarantee the source output: {free:?}"
        );
        assert!(
            free.contains(&TypeId::of::<Seed>()),
            "the handler path does not guarantee later production either: {free:?}"
        );
    }

    #[test]
    fn a_guarded_path_does_not_suppress_an_unrelated_branch() {
        use crate::node::ContextPolicy;

        // The guarded path is scoped to its own chain. A sibling branch with
        // no handler still contributes its guarantee to the union.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceSeed));
        let risky = inner.last_node().expect("producer was added");
        inner.add_error_handler_for(risky, |h| {
            h.add_boxed_system(Box::new(SideEffect));
        });

        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            vec![
                Box::new(move |g: &mut Graph| {
                    g.add_scope("risky", inner, ContextPolicy::new());
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(ProduceMid));
                }),
            ],
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        assert!(
            graph.signature().requires_outputs().is_empty(),
            "the unguarded sibling branch still guarantees Mid: {}",
            graph.signature()
        );
    }

    #[test]
    fn matching_error_handler_output_is_guaranteed_after_a_scope() {
        use crate::node::ContextPolicy;

        // The source's normal path and its handler path both deposit Final, so
        // the scope returns it whichever path recovers successfully. The
        // downstream reader is therefore internally satisfied.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(FallibleFinal));
        let source = inner.last_node().expect("source was added");
        inner.add_error_handler_for(source, |handler| {
            handler.add_boxed_system(Box::new(FallibleFinal));
        });
        let inner_signature = inner.signature();
        assert!(
            inner_signature
                .guaranteed_produces()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Final>()),
            "the public guaranteed-production surface includes Final: {inner_signature:?}"
        );

        let mut graph = Graph::new();
        graph.add_scope("fallback", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsFinal));

        let signature = graph.signature();
        assert!(
            !signature
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Final>()),
            "matching error paths guarantee Final: {signature}"
        );
    }

    #[test]
    fn handler_missing_an_output_leaves_the_downstream_read_required() {
        use crate::node::ContextPolicy;

        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(FallibleFinal));
        let source = inner.last_node().expect("source was added");
        inner.add_error_handler_for(source, |handler| {
            handler.add_boxed_system(Box::new(ProduceMid));
        });

        let mut graph = Graph::new();
        graph.add_scope("fallback", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsFinal));

        assert!(
            graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Final>()),
            "a handler without Final cannot satisfy the downstream reader: {}",
            graph.signature()
        );
    }

    #[test]
    fn matching_timeout_handler_output_is_guaranteed_after_a_scope() {
        use crate::node::ContextPolicy;
        use std::time::Duration;

        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(FallibleFinal));
        let source = inner.last_node().expect("source was added");
        inner.set_timeout(source.clone(), Duration::from_secs(1));
        inner.add_timeout_handler(source, |handler| {
            handler.add_boxed_system(Box::new(FallibleFinal));
        });

        let mut graph = Graph::new();
        graph.add_scope("fallback", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsFinal));

        assert!(
            !graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Final>()),
            "matching timeout paths guarantee Final: {}",
            graph.signature()
        );
    }

    #[test]
    fn nested_matching_guards_preserve_their_shared_output() {
        use crate::node::ContextPolicy;

        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(FallibleFinal));
        let outer_source = inner.last_node().expect("outer source was added");
        inner.add_error_handler_for(outer_source, |outer_handler| {
            outer_handler.add_boxed_system(Box::new(FallibleFinal));
            let inner_source = outer_handler.last_node().expect("inner source was added");
            outer_handler.add_error_handler_for(inner_source, |inner_handler| {
                inner_handler.add_boxed_system(Box::new(FallibleFinal));
            });
        });

        let mut graph = Graph::new();
        graph.add_scope("fallback", inner, ContextPolicy::new());
        graph.add_boxed_system(Box::new(ReadsFinal));

        assert!(
            !graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Final>()),
            "every nested recovery path deposits Final: {}",
            graph.signature()
        );
    }

    // ── The parallel union spans every branch ───────────────────────────────

    #[test]
    fn a_later_parallel_branch_satisfies_a_downstream_read() {
        // Every branch runs, so the credited set is the union across all of
        // them — not just the first. Only the second branch produces `Mid`.
        let mut graph = Graph::new();
        graph.add_parallel(
            "fan_out",
            vec![
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(ProduceSeed));
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(ProduceMid));
                }),
            ],
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        assert!(
            graph.signature().requires_outputs().is_empty(),
            "a non-first branch's production is credited: {}",
            graph.signature()
        );
    }

    // ── The guaranteed walk, exercised directly ─────────────────────────────

    #[test]
    fn guaranteed_walk_credits_outputs_beyond_the_default_executor_depth() {
        use crate::node::ContextPolicy;

        // Nested scopes are traversed iteratively, so depth cannot exhaust the
        // call stack. Mirrors the optimistic dual's own deep-nesting test.
        let mut nested = Graph::new();
        nested.add_boxed_system(Box::new(ProduceMid));
        for _ in 0..65 {
            let mut outer = Graph::new();
            outer.add_scope("nest", nested, ContextPolicy::new());
            nested = outer;
        }

        let mut acc = HashSet::new();
        nested.must_produce_output_types(&mut acc);

        assert!(
            acc.contains(&TypeId::of::<Mid>()),
            "an output 65 scopes down is credited: {acc:?}"
        );
    }

    #[test]
    fn guaranteed_walk_credits_a_dynamic_contract() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        // Every candidate admitted by the slot produces the contract's
        // outputs, and the dynamic boundary merges them back.
        let mut candidate = Graph::new();
        candidate.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", candidate)],
            DynamicSlot::new(
                GraphSignature::new().produce::<Mid>(),
                ContextPolicy::shared(),
            ),
        );

        let mut acc = HashSet::new();
        graph.must_produce_output_types(&mut acc);

        assert!(
            acc.contains(&TypeId::of::<Mid>()),
            "a dynamic contract's produces are guaranteed: {acc:?}"
        );
    }

    #[test]
    fn dynamic_contract_satisfies_a_direct_downstream_read() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        let mut candidate = Graph::new();
        candidate.add_boxed_system(Box::new(ProduceMid));
        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", candidate)],
            DynamicSlot::new(
                GraphSignature::new().produce::<Mid>(),
                ContextPolicy::shared(),
            ),
        );
        graph.add_boxed_system(Box::new(RuntimeReadsMid));

        assert!(graph.validate().is_ok());
        assert!(
            !graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "the admitted slot contract satisfies the direct downstream read: {}",
            graph.signature()
        );
    }

    fn conditional_mid_candidate() -> Graph {
        let mut candidate = Graph::new();
        candidate.add_conditional_branch::<Seed, _, _, _>(
            "route",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(SideEffect));
            },
        );
        candidate
    }

    fn dynamic_contract() -> GraphSignature {
        GraphSignature::new()
            .require_output::<Seed>()
            .produce::<Mid>()
    }

    #[test]
    fn exact_dynamic_candidate_must_guarantee_contract_outputs() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", conditional_mid_candidate())],
            DynamicSlot::new(dynamic_contract(), ContextPolicy::shared()),
        );

        assert!(matches!(
            graph.validate().errors.as_slice(),
            [ValidationError::DynamicCandidateMissingGuaranteedOutputs { missing, .. }]
                if missing.iter().any(|access| access.type_id == TypeId::of::<Mid>())
        ));
    }

    #[test]
    fn dynamic_candidate_gap_is_rejected_inside_a_plain_scope() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        let mut inner = Graph::new();
        inner.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", conditional_mid_candidate())],
            DynamicSlot::new(dynamic_contract(), ContextPolicy::shared()),
        );
        let mut graph = Graph::new();
        graph.add_scope("inner", inner, ContextPolicy::shared());
        graph.add_boxed_system(Box::new(RuntimeReadsMid));

        assert!(graph.validate().errors.iter().any(|error| matches!(
            error,
            ValidationError::ScopeGraphInvalid { inner, .. }
                if matches!(
                    inner.as_ref(),
                    ValidationError::DynamicCandidateMissingGuaranteedOutputs { missing, .. }
                        if missing.iter().any(|access| access.type_id == TypeId::of::<Mid>())
                )
        )));
    }

    #[test]
    fn substitutable_dynamic_candidate_must_guarantee_contract_outputs() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", conditional_mid_candidate())],
            DynamicSlot::new(dynamic_contract(), ContextPolicy::shared())
                .with_matching(MatchingMode::Substitutable),
        );

        assert!(matches!(
            graph.validate().errors.as_slice(),
            [ValidationError::DynamicCandidateMissingGuaranteedOutputs { missing, .. }]
                if missing.iter().any(|access| access.type_id == TypeId::of::<Mid>())
        ));
    }

    #[test]
    fn switch_candidate_must_guarantee_contract_outputs() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        let mut candidate = Graph::new();
        let produce_mid: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        let side_effect: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(SideEffect));
        };
        candidate.add_switch::<Seed, _, _, _>(
            "route",
            |_seed| "mid",
            [("mid", produce_mid), ("empty", side_effect)],
            None,
        );
        let signature = candidate.signature();
        assert!(
            signature
                .produces()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>())
        );
        assert!(
            !signature
                .guaranteed_produces()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>())
        );

        let mut graph = Graph::new();
        graph.add_dynamic(
            "route",
            |_ctx| Arc::from("only"),
            [("only", candidate)],
            DynamicSlot::new(dynamic_contract(), ContextPolicy::shared()),
        );
        assert!(matches!(
            graph.validate().errors.as_slice(),
            [ValidationError::DynamicCandidateMissingGuaranteedOutputs { .. }]
        ));
    }

    #[test]
    fn guaranteed_walk_intersects_switch_arms_without_a_default() {
        // Every matching case runs exactly one arm. An unmatched key stops the
        // graph before the downstream reader, so both actual continuation arms
        // guarantee `Mid` here.
        let mut switched = Graph::new();
        let produce_mid: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        switched.add_switch::<Seed, _, _, _>(
            "route",
            |_seed| "case_a",
            [("case_a", produce_mid), ("case_b", produce_mid)],
            None,
        );
        switched.add_boxed_system(Box::new(ReadsMid));

        assert!(
            !switched
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "all switch arms guarantee Mid: {}",
            switched.signature()
        );
    }

    #[test]
    fn guaranteed_walk_intersects_switch_arms_with_a_default() {
        let mut switched = Graph::new();
        let produce_mid: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        switched.add_switch::<Seed, _, _, _>(
            "route",
            |_seed| "case_a",
            [("case_a", produce_mid), ("case_b", produce_mid)],
            Some(produce_mid),
        );
        switched.add_boxed_system(Box::new(ReadsMid));

        assert!(
            !switched
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "the default joins the switch intersection: {}",
            switched.signature()
        );
    }

    #[test]
    fn switch_arm_missing_an_output_leaves_the_read_required() {
        let mut switched = Graph::new();
        let produce_mid: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        let no_output: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(SideEffect));
        };
        switched.add_switch::<Seed, _, _, _>(
            "route",
            |_seed| "case_a",
            [("case_a", produce_mid), ("case_b", no_output)],
            None,
        );
        switched.add_boxed_system(Box::new(ReadsMid));

        assert!(
            switched
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "a missing arm output remains free: {}",
            switched.signature()
        );
    }

    #[test]
    fn guaranteed_walk_intersects_decision_arms() {
        let mut decision = Graph::new();
        decision.add_conditional_branch::<Seed, _, _, _>(
            "route",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        decision.add_boxed_system(Box::new(ReadsMid));

        assert!(
            !decision
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "both decision arms guarantee Mid: {}",
            decision.signature()
        );
    }

    #[test]
    fn decision_arm_missing_an_output_leaves_the_read_required() {
        let mut decision = Graph::new();
        decision.add_conditional_branch::<Seed, _, _, _>(
            "route",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(SideEffect));
            },
        );
        decision.add_boxed_system(Box::new(ReadsMid));

        assert!(
            decision
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "a missing decision-arm output remains free: {}",
            decision.signature()
        );
    }

    #[test]
    fn guaranteed_produces_is_narrower_than_may_produce() {
        let mut graph = Graph::new();
        graph.add_conditional_branch::<Seed, _, _, _>(
            "route",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceSeed));
            },
        );

        let signature = graph.signature();
        assert_eq!(signature.produces().len(), 2, "both arms may run");
        assert!(
            signature.guaranteed_produces().is_empty(),
            "no output is present after both alternatives: {signature:?}"
        );
    }

    #[test]
    fn nested_switch_intersections_are_credited() {
        let mut graph = Graph::new();
        let inner: fn(&mut Graph) = |g| {
            let produce_mid: fn(&mut Graph) = |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            };
            g.add_switch::<Seed, _, _, _>(
                "inner",
                |_seed| "left",
                [("left", produce_mid), ("right", produce_mid)],
                None,
            );
        };
        graph.add_switch::<Seed, _, _, _>(
            "outer",
            |_seed| "left",
            [("left", inner), ("right", inner)],
            None,
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        assert!(
            !graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "nested intersections guarantee Mid: {}",
            graph.signature()
        );
    }

    #[test]
    fn handler_recovery_in_an_arm_is_not_credited() {
        // A handler can complete the selected arm after its ordinary producer
        // fails. The guaranteed walk intentionally withholds that arm's output
        // rather than assuming the handler returns a compatible replacement.
        let mut graph = Graph::new();
        graph.add_conditional_branch::<Seed, _, _, _>(
            "route",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(FallibleFinal));
                let source = g.last_node().expect("fallible system was added");
                g.add_error_handler_for(source, |handler| {
                    handler.add_boxed_system(Box::new(ProduceMid));
                });
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        graph.add_boxed_system(Box::new(ReadsMid));

        assert!(
            graph
                .signature()
                .requires_outputs()
                .iter()
                .any(|access| access.type_id == TypeId::of::<Mid>()),
            "a recovered arm is conservatively not credited: {}",
            graph.signature()
        );
    }

    #[tokio::test]
    async fn switch_arm_outputs_reach_the_downstream_reader_at_runtime() {
        // Run once for each case. The signature's intersection matches the
        // executor: selected-arm output is deposited before the parent chain
        // advances to the reader.
        let mut graph = Graph::new();
        let produce_mid: fn(&mut Graph) = |g| {
            g.add_boxed_system(Box::new(ProduceMid));
        };
        graph.add_switch::<Seed, _, _, _>(
            "route",
            |seed| if *seed == 0 { "zero" } else { "one" },
            [("zero", produce_mid), ("one", produce_mid)],
            None,
        );
        graph.add_boxed_system(Box::new(RuntimeReadsMid));

        let executor = crate::executor::GraphExecutor::new();
        for seed in [0_u32, 1] {
            let mut ctx = SystemContext::new();
            ctx.insert_output(seed);
            let result = executor
                .execute(&graph, &mut ctx, None, None)
                .await
                .expect("every selected arm produces Mid before the reader runs");
            assert_eq!(result.output::<Final>(), Some(&0));
        }
    }

    #[test]
    fn guaranteed_walk_does_not_credit_loop_interiors() {
        // The predicate is evaluated before the first iteration, and the body
        // may therefore run zero times.
        let mut looped = Graph::new();
        looped.add_loop::<Seed, _, _>(
            "spin",
            |_seed| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
        );
        let mut acc = HashSet::new();
        looped.must_produce_output_types(&mut acc);
        assert!(
            acc.is_empty(),
            "a loop body's production is not guaranteed: {acc:?}"
        );
    }

    #[test]
    fn guaranteed_walk_on_an_empty_graph_credits_nothing() {
        use crate::node::ContextPolicy;

        // An empty inner graph has no entry to walk from; the scope arm must
        // skip it rather than assuming one exists.
        let mut graph = Graph::new();
        graph.add_scope("empty", Graph::new(), ContextPolicy::new());

        let mut acc = HashSet::new();
        graph.must_produce_output_types(&mut acc);
        assert!(acc.is_empty(), "an empty scope credits nothing: {acc:?}");

        let mut acc = HashSet::new();
        Graph::new().must_produce_output_types(&mut acc);
        assert!(acc.is_empty(), "an empty graph credits nothing: {acc:?}");
    }

    // ── Opacity, cycles, unit, over-declaration ─────────────────────────────

    #[test]
    fn embedded_scope_is_opaque_and_dynamic_exposes_its_contract() {
        use crate::node::{ContextPolicy, DynamicSlot};
        use std::sync::Arc;

        // A plain scope hides its may-produce interface, while its unconditional
        // merge-back still appears in the guaranteed-production surface.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        let mut scoped = Graph::new();
        scoped.add_scope("inner", inner, ContextPolicy::shared());
        let scoped_signature = scoped.signature();
        assert!(scoped_signature.produces().is_empty());
        assert_eq!(
            scoped_signature
                .guaranteed_produces()
                .iter()
                .map(|access| access.type_id)
                .collect::<Vec<_>>(),
            vec![TypeId::of::<Mid>()]
        );

        // A dynamic node's candidates stay opaque, but its validated slot
        // contract is the boundary's public production promise.
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
        let signature = dynamic.signature();
        assert_eq!(
            signature
                .produces()
                .iter()
                .map(|access| access.type_id)
                .collect::<Vec<_>>(),
            vec![TypeId::of::<Mid>()]
        );
        assert_eq!(signature.guaranteed_produces(), signature.produces());
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
        // A plain Scope boundary smuggled behind an error edge must still be
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

    // ── Declared scope contracts ─────────────────────────────────────────────

    /// Reads `Res<HandlerRes>` and free `Out<Seed>`, produces a [`Mid`] — an
    /// interior whose declared contract names all three axes.
    struct ScopeWorker;
    impl System for ScopeWorker {
        type Output = Mid;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "scope_worker"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new()
                .with_read::<HandlerRes>()
                .with_output::<Seed>()
        }
    }

    /// The contract [`ScopeWorker`]'s interior satisfies exactly.
    fn worker_contract() -> GraphSignature {
        GraphSignature::new()
            .require_read::<HandlerRes>()
            .require_output::<Seed>()
            .produce::<Mid>()
    }

    /// Appends a `ScopeWorker` interior as a declared scope onto `graph`.
    fn add_declared_worker_scope(graph: &mut Graph) {
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ScopeWorker));
        graph.add_scope_with_contract(
            "worker",
            inner,
            crate::node::ContextPolicy::shared(),
            worker_contract(),
        );
    }

    #[test]
    fn declared_scope_contract_propagates_into_the_parent_signature() {
        // With nothing upstream, every contract axis surfaces at the scope's
        // chain position: the boundary is no longer opaque.
        let mut graph = Graph::new();
        add_declared_worker_scope(&mut graph);

        assert!(graph.validate().is_ok(), "{}", graph.validate());
        assert_eq!(
            graph.signature(),
            worker_contract(),
            "the parent's derived signature is the declaration"
        );
    }

    #[test]
    fn contract_requirements_satisfied_upstream_are_omitted() {
        // A guaranteed upstream producer satisfies the contract's free output
        // read, so it no longer surfaces. The resource requirement has no
        // guarantee mechanism (derivation cannot see `ctx.insert`), so it
        // always surfaces — like any system's resource read.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(ProduceSeed));
        add_declared_worker_scope(&mut graph);

        let sig = graph.signature();
        assert!(
            sig.requires_outputs().is_empty(),
            "Seed is guaranteed upstream: {sig}"
        );
        let required: Vec<TypeId> = sig.requires().iter().map(|access| access.type_id).collect();
        assert_eq!(required, vec![TypeId::of::<HandlerRes>()]);
        let produced: Vec<TypeId> = sig.produces().iter().map(|access| access.type_id).collect();
        assert!(
            produced.contains(&TypeId::of::<Mid>()),
            "the contract's produce enters the parent produces axis: {sig}"
        );
    }

    #[test]
    fn declared_contract_produces_credit_a_downstream_loop() {
        use crate::node::ContextPolicy;

        // The contract's produces are guaranteed production for downstream
        // reads — here a loop's first termination check, evaluated before the
        // body has ever run. Both models agree: the interface no longer
        // advertises the seed, and the optimistic run-start check never did.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));

        let mut graph = Graph::new();
        graph.add_scope_with_contract(
            "seed",
            inner,
            ContextPolicy::new(),
            GraphSignature::new().produce::<Mid>(),
        );
        graph.add_loop::<Mid, _, _>(
            "refine",
            |_mid| true,
            |body| {
                body.add_boxed_system(Box::new(ProduceMid));
            },
        );

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|access| access.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "the declared produce satisfies the loop's first check: {free:?}"
        );
        assert!(
            graph.unguaranteed_loop_predicate_inputs().is_empty(),
            "the entry check agrees with the signature"
        );
        assert!(
            !graph.contains_nested_boundary(),
            "a declared scope is not a hidden boundary"
        );
    }

    #[test]
    fn crediting_stops_at_the_declared_contract() {
        use crate::node::ContextPolicy;

        // The interior also guarantees `Seed`, but the contract omits it, and
        // the declaration is the interface: derivation credits the contract
        // alone and never peeks past it. This is a legitimate subset contract
        // — validate() accepts it and the runtime merge-back filter keeps
        // `Seed` private to the scope (see `declared_contract_diff` and
        // `only_the_declared_produce_reaches_the_parent_context` below) — so
        // this is not merely how an unvalidated graph derives; it is the
        // validated, intended shape.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceMid));
        inner.add_boxed_system(Box::new(ProduceSeed));

        let mut graph = Graph::new();
        graph.add_scope_with_contract(
            "seed",
            inner,
            ContextPolicy::new(),
            GraphSignature::new().produce::<Mid>(),
        );
        // Reads Out<Mid> (declared, credited) and Out<Seed> (undeclared, free).
        graph.add_boxed_system(Box::new(ProduceFinal));

        assert!(graph.validate().is_ok(), "{}", graph.validate());

        let free: Vec<TypeId> = graph
            .signature()
            .requires_outputs()
            .iter()
            .map(|access| access.type_id)
            .collect();
        assert!(
            !free.contains(&TypeId::of::<Mid>()),
            "the declared produce is credited: {free:?}"
        );
        assert!(
            free.contains(&TypeId::of::<Seed>()),
            "undeclared interior production is not: {free:?}"
        );
    }

    #[test]
    fn a_contract_declaring_unguaranteed_production_is_rejected() {
        use crate::node::ContextPolicy;

        // Unlike `Seed` above (guaranteed, just undeclared), `Mid` here is
        // produced only inside a decision branch — never guaranteed. Naming
        // it in the contract would let the parent signature credit
        // conditional production as certain, which is unsound.
        let mut inner = Graph::new();
        inner.add_boxed_system(Box::new(ProduceSeed));
        inner.add_conditional_branch::<Seed, _, _, _>(
            "maybe_mid",
            |_| true,
            |g| {
                g.add_boxed_system(Box::new(ProduceMid));
            },
            |g| {
                g.add_boxed_system(Box::new(ProduceSeed));
            },
        );

        let mut graph = Graph::new();
        graph.add_scope_with_contract(
            "maybe",
            inner,
            ContextPolicy::new(),
            GraphSignature::new().produce::<Mid>(),
        );

        let errors = graph.validate().errors;
        assert!(
            errors.iter().any(|err| matches!(
                err,
                ValidationError::ScopeContractViolated { diff, .. }
                    if diff.missing_produces().iter().any(|a| a.type_id == TypeId::of::<Mid>())
            )),
            "expected ScopeContractViolated naming Mid as missing, got: {errors:?}"
        );
    }
}
