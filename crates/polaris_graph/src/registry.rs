//! A per-session registry of swappable candidate subgraphs for
//! [`Dynamic`](crate::node::Node::Dynamic) nodes.
//!
//! A [`SubgraphRegistry`] holds a set of named candidate graphs behind a fixed
//! [`GraphSignature`] contract. A [`Dynamic`](crate::node::Node::Dynamic) node
//! configured with [`CandidateSource::Registry`](crate::node::CandidateSource::Registry)
//! looks candidates up by the key its selector returns. Because the registry is
//! a [`LocalResource`], each session's context gets its own instance, so
//! registering or removing a candidate between turns swaps the graph a slot runs
//! **for that session only**.
//!
//! Every candidate is checked at insertion time: it must pass its own
//! [`validate`](crate::Graph::validate) **and** carry a
//! [signature](crate::Graph::signature) [compatible](GraphSignature::compatible_with)
//! with the registry contract. A candidate added mid-session is therefore held
//! to the same interface the parent graph was validated against — selection
//! never runs an unchecked graph.

use crate::graph::{Graph, GraphSignature, SignatureDiff, ValidationError};
use hashbrown::HashMap;
use polaris_system::resource::LocalResource;
use std::fmt;
use std::sync::Arc;

/// A signature-checked, mutable set of candidate subgraphs keyed by name.
///
/// Reach for this when a [`Dynamic`](crate::node::Node::Dynamic) node's
/// candidate set must change *while a session is running* — A/B-testing planner
/// variants, hot-swapping a sub-agent between turns, or registering
/// tenant-specific behavior at request time — rather than being fixed at build
/// time (for which the inline candidate set is simpler; see **Alternatives**).
/// Every candidate is held to the registry's [`GraphSignature`] contract, so
/// whatever the slot runs is always shape-compatible with the parent graph.
///
/// # Scope
///
/// **Local.** Each agent context gets its own instance, so a candidate
/// registered or swapped mid-session changes the slot **for that session only**
/// — concurrent sessions are unaffected and never observe a torn candidate set.
/// A server-wide candidate set would make every session share (and race on) the
/// same mutable map, which is exactly what per-session selection must avoid.
///
/// # Provided by
///
/// Consumer-supplied — no plugin registers a `SubgraphRegistry` by default.
/// Insert one whose contract matches the dynamic node's contract into the
/// context before execution (via [`SystemContext::with`] /
/// [`SystemContext::insert`], or a session-init closure).
///
/// Keys are meant to be developer- or configuration-derived names ("v1",
/// "tenant-acme"). Don't key registrations from unbounded untrusted strings:
/// the backing map's default hasher is not guaranteed HashDoS-resistant, and an
/// attacker-controlled key space would also grow the registry without bound.
/// Where registration is wired to request-derived input despite that guidance,
/// [`with_max_candidates`](Self::with_max_candidates) turns the growth half of
/// the contract into an enforced cap ([`RegistryError::Full`]).
///
/// # Access pattern
///
/// - `Res<SubgraphRegistry>` — the read path: the executor's
///   [`Dynamic`](crate::node::Node::Dynamic) node resolves the selector's key
///   against the registry on each execution. Systems rarely read it directly.
/// - `ResMut<SubgraphRegistry>` — the write path:
///   [`register`](Self::register) / [`remove`](Self::remove) seed or swap
///   candidates. Mutations made between turns are observed by the next
///   execution; there is no snapshotting.
///
/// # Scope crossing
///
/// `SubgraphRegistry` is neither `Clone` nor forkable — a copied registry would
/// fork the candidate set, so swaps made through one copy would be invisible to
/// the other. When a registry-backed dynamic node sits inside a [`Scope`] whose
/// policy filters the parent chain, the registry must cross by **sharing**: add
/// `.share::<SubgraphRegistry>()` (or `.share_rest()`) to that scope's
/// [`ContextPolicy`]. A hidden registry fails selection with
/// [`DynamicRegistryOutOfScope`](crate::executor::ExecutionError::DynamicRegistryOutOfScope),
/// which carries the same guidance.
///
/// [`Scope`]: crate::node::Node::Scope
/// [`ContextPolicy`]: crate::node::ContextPolicy
///
/// # Alternatives
///
/// - [`CandidateSource::Inline`](crate::node::CandidateSource::Inline)
///   ([`Graph::add_dynamic`](crate::Graph::add_dynamic)) — a build-time fixed
///   candidate set, validated once at [`Graph::validate`] time. Prefer it when
///   the candidates never change at runtime; a registry is only needed for
///   between-turn mutation.
///
/// # Example system
///
/// The registry's read consumer is the [`Dynamic`](crate::node::Node::Dynamic)
/// node itself (resolved by the executor, not user code). A system participates
/// by *mutating* it through `ResMut<SubgraphRegistry>` to swap what the slot
/// runs on the next turn:
///
/// ```
/// use polaris_graph::{ContextPolicy, DynamicSlot, Graph, GraphSignature, SubgraphRegistry};
/// use polaris_system::param::{ResMut, SystemContext};
/// use polaris_system::system;
///
/// async fn plan_v1() -> i32 { 1 }
/// async fn plan_v2() -> i32 { 2 }
///
/// // The write path is an ordinary system taking `ResMut<SubgraphRegistry>`.
/// // Run between turns, it swaps what the "planner" slot executes next; the
/// // next execution observes the new candidate (there is no snapshotting).
/// #[system]
/// async fn swap_planner(mut registry: ResMut<SubgraphRegistry>) {
///     let mut v2 = Graph::new();
///     v2.add_system(plan_v2);
///     // A rejected swap leaves the previous candidate in place — surface it,
///     // don't discard it.
///     if let Err(rejected) = registry.register("current", v2) {
///         tracing::warn!("planner swap rejected: {rejected}");
///     }
/// }
///
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let contract = GraphSignature::new().produce::<i32>();
///
///     // A graph whose "planner" slot is filled from the registry at run time.
///     let mut graph = Graph::new();
///     graph.add_dynamic_registry(
///         "planner",
///         |_ctx| "current",
///         DynamicSlot::new(contract.clone(), ContextPolicy::shared()),
///     );
///
///     // Seed the registry and insert it into the session context; the
///     // `swap_planner` system then swaps the candidate on a later turn.
///     let mut registry = SubgraphRegistry::new(contract);
///     let mut v1 = Graph::new();
///     v1.add_system(plan_v1);
///     registry.register("current", v1)?; // rejected unless contract-compatible
///     assert!(registry.contains("current"));
///     let _ctx = SystemContext::new().with(registry);
///     Ok(())
/// }
/// ```
///
/// [`SystemContext::with`]: polaris_system::param::SystemContext::with
/// [`SystemContext::insert`]: polaris_system::param::SystemContext::insert
/// [`Graph::validate`]: crate::Graph::validate
#[derive(Debug)]
pub struct SubgraphRegistry {
    contract: GraphSignature,
    graphs: HashMap<Arc<str>, Arc<Graph>>,
    max_candidates: Option<usize>,
}

impl LocalResource for SubgraphRegistry {}

impl SubgraphRegistry {
    /// Creates an empty, unbounded registry whose candidates must satisfy
    /// `contract`.
    #[must_use]
    pub fn new(contract: GraphSignature) -> Self {
        Self {
            contract,
            graphs: HashMap::new(),
            max_candidates: None,
        }
    }

    /// Caps the registry at `max` candidates.
    ///
    /// Once the cap is reached, [`register`](Self::register) refuses new keys
    /// with [`RegistryError::Full`]; replacing an existing key still succeeds,
    /// and [`remove`](Self::remove) frees a slot. Use this when registration
    /// keys derive from runtime input, so a misbehaving key source cannot grow
    /// the per-session candidate set without bound.
    #[must_use]
    pub fn with_max_candidates(mut self, max: usize) -> Self {
        self.max_candidates = Some(max);
        self
    }

    /// The candidate cap, if one was configured via
    /// [`with_max_candidates`](Self::with_max_candidates).
    #[must_use]
    pub fn max_candidates(&self) -> Option<usize> {
        self.max_candidates
    }

    /// The contract every registered candidate must satisfy.
    #[must_use]
    pub fn contract(&self) -> &GraphSignature {
        &self.contract
    }

    /// Registers `graph` under `id`, replacing any existing candidate with that
    /// key.
    ///
    /// # Errors
    ///
    /// - [`RegistryError::Full`] if the registry is at its
    ///   [`max_candidates`](Self::with_max_candidates) cap and `id` is a new
    ///   key (replacement is always allowed).
    /// - [`RegistryError::Invalid`] if the graph fails its own structural
    ///   validation.
    /// - [`RegistryError::NestedCandidate`] if the graph nests a scope or dynamic
    ///   node that flat signature derivation cannot see across.
    /// - [`RegistryError::Incompatible`] if the graph's signature does not match
    ///   the registry contract.
    pub fn register(&mut self, id: impl Into<Arc<str>>, graph: Graph) -> Result<(), RegistryError> {
        let id = id.into();

        // Capacity is an admission gate checked first: even a perfectly valid
        // candidate cannot fit, and the check must not depend on how expensive
        // the rejected graph is to validate.
        if let Some(max) = self.max_candidates
            && self.graphs.len() >= max
            && !self.graphs.contains_key(&id)
        {
            return Err(RegistryError::Full { id, max });
        }

        let validation = graph.validate();
        if !validation.is_ok() {
            return Err(RegistryError::Invalid {
                id,
                errors: validation.errors,
            });
        }

        // A candidate nesting a scope or dynamic node presents a signature that
        // hides the IO crossing that boundary, so it cannot be soundly checked
        // against the contract — refuse it rather than trust an opaque signature.
        if graph.contains_nested_boundary() {
            return Err(RegistryError::NestedCandidate { id });
        }

        let diff = graph.signature().diff(&self.contract);
        if !diff.is_empty() {
            return Err(RegistryError::Incompatible {
                id,
                diff: Box::new(diff),
            });
        }

        self.graphs.insert(id, Arc::new(graph));
        Ok(())
    }

    /// Removes and returns the candidate registered under `id`, if any.
    pub fn remove(&mut self, id: &str) -> Option<Arc<Graph>> {
        self.graphs.remove(id)
    }

    /// Returns `true` if a candidate is registered under `id`.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.graphs.contains_key(id)
    }

    /// Iterates over the registered candidate keys, in arbitrary order.
    ///
    /// Useful for the swap/A-B systems this registry exists for: enumerate
    /// what is currently registered before deciding what to
    /// [`register`](Self::register) or [`remove`](Self::remove).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.graphs.keys().map(AsRef::as_ref)
    }

    /// Iterates over `(key, candidate)` pairs, in arbitrary order.
    ///
    /// Every yielded candidate already passed [`register`](Self::register)'s
    /// checks, so it is contract-compatible by construction.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<Graph>)> {
        self.graphs.iter().map(|(key, graph)| (key.as_ref(), graph))
    }

    /// Returns the number of registered candidates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    /// Returns `true` if no candidates are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }

    /// Looks up a candidate by key, returning a cheap `Arc` clone so the caller
    /// can drop the registry borrow before running the graph.
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Graph>> {
        self.graphs.get(id).cloned()
    }
}

/// An error returned by [`SubgraphRegistry::register`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RegistryError {
    /// The candidate graph failed its own structural validation.
    Invalid {
        /// The key the candidate was being registered under.
        id: Arc<str>,
        /// The validation errors reported by [`Graph::validate`].
        errors: Vec<ValidationError>,
    },
    /// The candidate's signature is not compatible with the registry contract.
    Incompatible {
        /// The key the candidate was being registered under.
        id: Arc<str>,
        /// Which signature axes diverge, and in which direction. Boxed to keep
        /// [`RegistryError`] small on the common success path.
        diff: Box<SignatureDiff>,
    },
    /// The candidate nests a [`Scope`](crate::node::Node::Scope) or
    /// [`Dynamic`](crate::node::Node::Dynamic) node that flat signature
    /// derivation cannot see across, so it cannot be soundly checked against the
    /// registry contract.
    NestedCandidate {
        /// The key the candidate was being registered under.
        id: Arc<str>,
    },
    /// The registry is at its [`with_max_candidates`] cap and the key is not
    /// already registered. Remove a candidate to free a slot, or replace an
    /// existing key instead.
    ///
    /// [`with_max_candidates`]: SubgraphRegistry::with_max_candidates
    Full {
        /// The key the candidate was being registered under.
        id: Arc<str>,
        /// The configured candidate cap.
        max: usize,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Surface the actual validation error(s) rather than just a count —
            // the first error is the most actionable, with a tail count so the
            // message stays bounded. The full list remains on the `errors`
            // field, and [`source`](RegistryError::source) exposes the first.
            RegistryError::Invalid { id, errors } => match errors.as_slice() {
                [] => write!(f, "candidate '{id}' failed validation"),
                [first] => write!(f, "candidate '{id}' failed validation: {first}"),
                [first, rest @ ..] => write!(
                    f,
                    "candidate '{id}' failed validation: {first} (and {} more error(s))",
                    rest.len()
                ),
            },
            RegistryError::Incompatible { id, diff } => write!(
                f,
                "candidate '{id}' is not compatible with the registry contract ({diff})"
            ),
            RegistryError::NestedCandidate { id } => write!(
                f,
                "candidate '{id}' nests a scope or dynamic node; signature derivation does not yet see across nested context boundaries — flatten the candidate (recursive signatures are planned)"
            ),
            RegistryError::Full { id, max } => write!(
                f,
                "candidate '{id}' rejected: the registry is at its cap of {max} candidate(s) — remove one to free a slot, or replace an existing key"
            ),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Invalid { errors, .. } => errors
                .first()
                .map(|err| err as &(dyn std::error::Error + 'static)),
            RegistryError::Incompatible { .. }
            | RegistryError::NestedCandidate { .. }
            | RegistryError::Full { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::param::{SystemAccess, SystemContext};
    use polaris_system::system::{BoxFuture, System, SystemError};
    use std::error::Error as _;

    /// Produces an `i32`, reads nothing — the canonical contract-compatible
    /// candidate body.
    struct Produce;
    impl System for Produce {
        type Output = i32;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(1) })
        }
        fn name(&self) -> &'static str {
            "produce"
        }
    }

    /// A fallible `i32` producer, so an error handler can attach to it.
    struct FallibleProduce;
    impl System for FallibleProduce {
        type Output = i32;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(1) })
        }
        fn name(&self) -> &'static str {
            "fallible_produce"
        }
        fn is_fallible(&self) -> bool {
            true
        }
    }

    /// A handler producing a `u8` the contract never sanctioned.
    struct HandlerExtra;
    impl System for HandlerExtra {
        type Output = u8;
        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async { Ok(0) })
        }
        fn name(&self) -> &'static str {
            "handler_extra"
        }
        fn access(&self) -> SystemAccess {
            SystemAccess::new()
        }
    }

    fn contract() -> GraphSignature {
        GraphSignature::new().produce::<i32>()
    }

    fn candidate() -> Graph {
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(Produce));
        graph
    }

    #[test]
    fn keys_and_iter_enumerate_registered_candidates() {
        let mut registry = SubgraphRegistry::new(contract());
        registry.register("v1", candidate()).unwrap();
        registry.register("v2", candidate()).unwrap();

        let mut keys: Vec<&str> = registry.keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["v1", "v2"]);

        let mut pairs: Vec<&str> = registry.iter().map(|(key, _)| key).collect();
        pairs.sort_unstable();
        assert_eq!(pairs, vec!["v1", "v2"]);
        assert!(
            registry
                .iter()
                .all(|(_, graph)| graph.signature().compatible_with(registry.contract())),
            "every yielded candidate is contract-compatible by construction"
        );
    }

    #[test]
    fn max_candidates_caps_new_keys_but_allows_replacement() {
        let mut registry = SubgraphRegistry::new(contract()).with_max_candidates(1);
        assert_eq!(registry.max_candidates(), Some(1));
        registry.register("v1", candidate()).unwrap();

        // At the cap, a new key is refused with the cap in the error…
        let err = registry.register("v2", candidate()).unwrap_err();
        match err {
            RegistryError::Full { ref id, max } => {
                assert_eq!(&**id, "v2");
                assert_eq!(max, 1);
            }
            other => panic!("expected Full, got {other:?}"),
        }
        assert!(
            !registry.contains("v2"),
            "the refused candidate was not stored"
        );

        // …but replacing the existing key still succeeds (no net growth)…
        registry
            .register("v1", candidate())
            .expect("replacement at the cap must succeed");
        assert_eq!(registry.len(), 1);

        // …and removing frees a slot for a genuinely new key.
        registry.remove("v1");
        registry
            .register("v2", candidate())
            .expect("a freed slot admits a new key");
    }

    #[test]
    fn register_rejects_handler_io_the_contract_never_sanctioned() {
        // The candidate's happy path matches the contract exactly, but its
        // error handler produces `u8` — IO that would merge back unchecked if
        // derivation skipped error edges. Admission must refuse it.
        let mut graph = Graph::new();
        graph.add_boxed_system(Box::new(FallibleProduce));
        graph.add_error_handler(|g| {
            g.add_boxed_system(Box::new(HandlerExtra));
        });

        let mut registry = SubgraphRegistry::new(contract());
        let err = registry.register("sneaky", graph).unwrap_err();
        match err {
            RegistryError::Incompatible { ref diff, .. } => {
                assert_eq!(
                    diff.extra_produces().len(),
                    1,
                    "the handler's output is the extra axis: {diff}"
                );
            }
            other => panic!("expected Incompatible, got {other:?}"),
        }
    }

    #[test]
    fn invalid_display_renders_zero_one_and_many_errors() {
        // The three slice-match branches: no inner error, one, and one + tail
        // count. Hand-built because `register` never produces an empty list.
        let empty = RegistryError::Invalid {
            id: Arc::from("cand"),
            errors: vec![],
        };
        assert_eq!(empty.to_string(), "candidate 'cand' failed validation");

        let invalid = Graph::new(); // no entry point → exactly one error
        let mut registry = SubgraphRegistry::new(contract());
        let one = registry.register("cand", invalid).unwrap_err();
        let rendered = one.to_string();
        assert!(
            rendered.starts_with("candidate 'cand' failed validation: "),
            "{rendered}"
        );
        assert!(!rendered.contains("more error(s)"), "{rendered}");

        let RegistryError::Invalid { id, errors } = one else {
            panic!("expected Invalid");
        };
        let first = errors[0].clone();
        let many = RegistryError::Invalid {
            id,
            errors: vec![first.clone(), first],
        };
        assert!(
            many.to_string().ends_with("(and 1 more error(s))"),
            "{many}"
        );
    }

    #[test]
    fn source_exposes_the_first_validation_error_and_nothing_else() {
        let mut registry = SubgraphRegistry::new(contract());
        let invalid = registry.register("cand", Graph::new()).unwrap_err();
        assert!(
            invalid.source().is_some(),
            "Invalid chains to its first inner error"
        );

        // A valid candidate whose signature diverges (produces u8, not i32).
        let mut diverging = Graph::new();
        diverging.add_boxed_system(Box::new(HandlerExtra));
        let incompatible = registry.register("cand", diverging).unwrap_err();
        assert!(matches!(incompatible, RegistryError::Incompatible { .. }));
        assert!(incompatible.source().is_none());
    }
}
