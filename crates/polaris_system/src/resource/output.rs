//! System output storage.
//!
//! This module provides the [`Outputs`] container for storing ephemeral
//! system return values that flow between systems during a single execution.
//!
//! Unlike [`Resources`](super::Resources) which store long-lived shared state,
//! outputs are cleared between agent runs and are immutable once stored.

use hashbrown::HashMap;
use parking_lot::{RwLock, RwLockReadGuard};
use std::any::{Any, TypeId, type_name};
use std::sync::Arc;

/// Marker trait for types that can be stored as system outputs.
///
/// Any type that is `Send + Sync + 'static` automatically implements `Output`.
/// This is the same bound as [`Resource`](super::Resource), but semantically
/// outputs represent ephemeral data flowing between systems.
pub trait Output: Send + Sync + 'static {}

// Blanket implementation for all compatible types
impl<T: Send + Sync + 'static> Output for T {}

/// Unique identifier for an output type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(TypeId);

impl OutputId {
    /// Creates an `OutputId` for the given type.
    #[must_use]
    pub fn of<T: Output>() -> Self {
        Self(TypeId::of::<T>())
    }
}

/// Errors that can occur during output operations.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// The requested output type was not found.
    #[error("output not found: {0}")]
    NotFound(&'static str),

    /// The output is currently being written.
    #[error("output busy: {0}")]
    Busy(&'static str),
}

/// The opaque identity of whatever wrote an output entry.
///
/// `polaris_system` (Layer 1) knows nothing about what a producer is — a
/// graph node, a test, or some other executor's unit of work. A key is one
/// opaque string id, and two keys are equal exactly when their ids are equal.
/// [`Outputs`] uses that equality for two things only:
///
/// - **Replacement**: a later write from an equal key replaces its earlier
///   entry for the same type (within one layer), so the store holds one entry
///   per producer per type instead of growing with every write.
/// - **By-producer reads**: [`Outputs::by_producer`] returns the newest entry
///   written under an equal key.
///
/// A key can also carry **provenance**: any value the writer wants to attach
/// for diagnostics, such as which run or loop iteration a write happened in.
/// Layer 1 stores it and hands it back through [`provenance`](Self::provenance),
/// but never reads it and never compares it — it does not affect equality,
/// hashing, replacement, or by-producer reads. The executor in `polaris_graph`
/// attaches its own provenance type there (`polaris_graph::ProducerInfo`), so
/// graph concepts such as node names, runs, and loop iterations stay in
/// Layer 2 — see `docs/taxonomy.md`.
///
/// # Examples
///
/// ```
/// use polaris_system::resource::{Outputs, ProducerKey};
///
/// #[derive(Debug, PartialEq)]
/// struct Attempt(u32);
///
/// let planner = ProducerKey::new("planner").with_provenance(Attempt(1));
///
/// // Provenance never affects identity.
/// assert_eq!(planner, ProducerKey::new("planner"));
/// assert_eq!(planner.provenance::<Attempt>(), Some(&Attempt(1)));
/// assert_eq!(planner.provenance::<String>(), None);
///
/// let mut outputs = Outputs::new();
/// outputs.insert_from(planner.clone(), 1_u32);
/// outputs.insert_from(ProducerKey::new("critic"), 2_u32);
/// // A second write from an equal key replaces the first.
/// outputs.insert_from(ProducerKey::new("planner").with_provenance(Attempt(2)), 3_u32);
///
/// assert_eq!(*outputs.by_producer::<u32>(&planner).unwrap(), 3);
/// assert_eq!(outputs.entries::<u32>().unwrap().len(), 2);
/// ```
#[derive(Clone)]
pub struct ProducerKey {
    id: Arc<str>,
    provenance: Option<Arc<dyn Any + Send + Sync>>,
}

impl ProducerKey {
    /// Builds a key with the opaque identity `id` and no provenance.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::ProducerKey;
    ///
    /// let key = ProducerKey::new("node_42");
    /// assert_eq!(key.id(), "node_42");
    /// ```
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self {
            id: id.into(),
            provenance: None,
        }
    }

    /// Sentinel producer used by writes that carry no caller-supplied
    /// identity (the typed/erased [`Outputs::insert`] family). Every such
    /// write shares this identity, so they keep overwriting each other for a
    /// given type exactly as a single-slot store would.
    fn unspecified() -> Self {
        Self::new("")
    }

    /// Attaches `provenance` to this key, replacing any provenance it
    /// already carries.
    ///
    /// Provenance is stored and returned, never read or compared, by this
    /// crate — see the type docs.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::ProducerKey;
    ///
    /// let key = ProducerKey::new("node").with_provenance("first pass");
    /// assert_eq!(key.provenance::<&'static str>(), Some(&"first pass"));
    /// ```
    #[must_use]
    pub fn with_provenance<P: Any + Send + Sync>(mut self, provenance: P) -> Self {
        let provenance: Arc<dyn Any + Send + Sync> = Arc::new(provenance);
        self.provenance = Some(provenance);
        self
    }

    /// The opaque identity, as supplied by the writer.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The attached provenance, if there is one and it is a `P`.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::ProducerKey;
    ///
    /// let key = ProducerKey::new("node").with_provenance(7_u64);
    /// assert_eq!(key.provenance::<u64>(), Some(&7));
    /// assert_eq!(ProducerKey::new("node").provenance::<u64>(), None);
    /// ```
    #[must_use]
    pub fn provenance<P: Any>(&self) -> Option<&P> {
        self.provenance
            .as_deref()
            .and_then(|provenance| provenance.downcast_ref::<P>())
    }
}

/// Keys are equal when their ids are equal; provenance is ignored.
impl PartialEq for ProducerKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ProducerKey {}

/// Hashes the id only, consistent with [`PartialEq`].
impl std::hash::Hash for ProducerKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl std::fmt::Debug for ProducerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProducerKey")
            .field("id", &self.id)
            .field("has_provenance", &self.provenance.is_some())
            .finish()
    }
}

/// Internal storage for a single output entry with thread-safe read access.
struct StoredEntry {
    /// Who wrote this entry, and when.
    producer: ProducerKey,
    /// Total write order across every type in the container: higher is
    /// newer. Assigned by [`Outputs`], never by the caller.
    seq: u64,
    /// Type-erased output data protected by `RwLock`.
    data: RwLock<Box<dyn Any + Send + Sync>>,
}

/// Container for storing system outputs during execution.
///
/// `Outputs` provides type-safe storage for system return values. Unlike
/// [`Resources`](super::Resources), outputs are:
///
/// - **Ephemeral**: Cleared between agent runs
/// - **Write-once**: Set by the executor when a system returns
/// - **Read-only**: Systems can only read outputs, not modify them
///
/// Storage keeps every entry, keyed by type and then by
/// [`ProducerKey`] equality — a later write from
/// the same producer replaces its previous entry for that type; a write from
/// a different producer keeps both. [`get`](Self::get) and
/// [`take_last`](Self::take_last) return the newest entry by write order, so
/// unless a caller starts using [`entries`](Self::entries) or
/// [`by_producer`](Self::by_producer), reading behaves exactly as it did when
/// each type held a single value.
///
/// # Layers
///
/// Storage is a stack of layers, one map per layer. A fresh container starts
/// with one layer — the base — which is never removed. Layers above it are
/// opened and closed only through an
/// [`OutputLayerScope`](crate::param::OutputLayerScope)
/// ([`SystemContext::output_layer_scope`](crate::param::SystemContext::output_layer_scope)):
/// every write lands in the newest layer, and the scope either promotes its
/// newest layer into the one below it or discards all of its layers. Because
/// closing is tied to the scope, the stack cannot be left unbalanced.
///
/// Every read (`get`, `entries`, `by_producer`, `contains`,
/// `contains_by_type_id`, `take_last`) searches every layer: a layer never
/// hides what an enclosing layer already holds, only a write can shadow an
/// enclosing entry, and only by replacing a same-producer entry that already
/// lives in *that write's own* layer — the replace rule above is scoped to a
/// single layer, so the same producer writing once per layer keeps one entry
/// *per layer*, not one entry overall, until the layers are folded together.
/// This is what lets a loop iteration keep its own layer (pushed per
/// iteration by the executor) without losing the "same producer replaces"
/// bound *within* one iteration, while still allowing several iterations'
/// entries to coexist until the executor folds or discards them — see
/// `polaris_graph`'s `output_layers` module docs for how loop retention uses
/// this.
///
/// # Example
///
/// ```
/// use polaris_system::resource::Outputs;
///
/// struct ReasoningResult { action: String }
///
/// let mut outputs = Outputs::new();
///
/// // Executor stores system return value
/// outputs.insert(ReasoningResult { action: "search".into() });
///
/// // Next system reads the output
/// {
///     let result = outputs.get::<ReasoningResult>().unwrap();
///     assert_eq!(result.action, "search");
/// }
///
/// // Clear between runs
/// outputs.clear();
/// ```
pub struct Outputs {
    /// Stack of layers; index 0 is the base layer, created with the
    /// container and never popped. Writes always target the top layer —
    /// see the type docs' "Layers" section for what that means for reads.
    layers: Vec<HashMap<OutputId, Vec<StoredEntry>>>,
    /// Monotonic counter; the source of every [`StoredEntry::seq`]. Never
    /// reset except by [`clear`](Self::clear). Shared across every layer, so
    /// "newest by `seq`" is a total order over the whole container
    /// regardless of which layer an entry lives in.
    next_seq: u64,
}

impl Default for Outputs {
    fn default() -> Self {
        Self::new()
    }
}

impl Outputs {
    /// Creates a new empty outputs container, with a single (base) layer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            layers: vec![HashMap::new()],
            next_seq: 0,
        }
    }

    /// Returns the top layer's map, mutably — the target of every write.
    fn top_mut(&mut self) -> &mut HashMap<OutputId, Vec<StoredEntry>> {
        self.layers
            .last_mut()
            .expect("layers is never empty: the base layer is never popped")
    }

    /// Inserts or replaces the entry at `id` for `producer` in the top
    /// layer, giving it a fresh sequence number.
    ///
    /// If an entry with the same `id` and an equal producer key already
    /// exists **in the top
    /// layer**, it is removed and returned (still boxed); otherwise a new
    /// entry is appended and `None` is returned. A same-producer entry
    /// living in a lower (enclosing) layer is untouched — see the type
    /// docs' "Layers" section. Entries for a given `id` within one layer are
    /// always appended in increasing sequence order, so that layer's last
    /// element for `id` is always its newest.
    fn insert_entry(
        &mut self,
        id: OutputId,
        producer: ProducerKey,
        data: Box<dyn Any + Send + Sync>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        self.next_seq += 1;
        let seq = self.next_seq;
        let entries = self.top_mut().entry(id).or_default();
        let displaced = entries
            .iter()
            .position(|entry| entry.producer == producer)
            .map(|pos| entries.remove(pos).data.into_inner());
        entries.push(StoredEntry {
            producer,
            seq,
            data: RwLock::new(data),
        });
        displaced
    }

    /// Pushes a new, empty layer on top of the stack. Every write after this
    /// lands in the new layer until the layer is closed. Only
    /// [`OutputLayerScope`](crate::param::OutputLayerScope) calls this, so
    /// every pushed layer is closed by that scope.
    pub(crate) fn push_layer(&mut self) {
        self.layers.push(HashMap::new());
    }

    /// Returns the number of layers, including the permanent base layer.
    pub(crate) fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Keeps, for each output type, only the entries in that type's newest
    /// `retention` layers of the region starting at `first_layer`.
    ///
    /// Entries in layers before `first_layer` are untouched. For every type
    /// in the region, `default_retention` applies unless `per_type` supplies
    /// a value for that type; an override wins. A retention of zero drops all
    /// matching entries already in the region. A `first_layer` of zero is
    /// treated as one, so the base layer is never touched; a region that no
    /// longer exists is left alone.
    ///
    /// This is type-aware rather than dropping whole layers: one loop may
    /// retain three `Score` iterations but only one `Trace` iteration without
    /// making the latter visible merely because another type asked for a
    /// wider history.
    pub(crate) fn retain_layers_from<I>(
        &mut self,
        first_layer: usize,
        default_retention: usize,
        per_type: I,
    ) where
        I: IntoIterator<Item = (TypeId, usize)>,
    {
        let first_layer = first_layer.max(1);
        let last_layer = self.layers.len();
        if first_layer >= last_layer {
            return;
        }
        let overrides: std::collections::HashMap<TypeId, usize> = per_type.into_iter().collect();

        for (layer_index, layer) in self.layers.iter_mut().enumerate().skip(first_layer) {
            layer.retain(|id, _| {
                let retention = overrides.get(&id.0).copied().unwrap_or(default_retention);
                layer_index + retention >= last_layer
            });
        }
    }

    /// Drops every layer from `first_layer` up, leaving the layers below it
    /// exactly as they were. A `first_layer` of zero is treated as one, so
    /// the base layer is never dropped.
    pub(crate) fn discard_layers_from(&mut self, first_layer: usize) {
        self.layers.truncate(first_layer.max(1));
    }

    /// Folds the newest layer at or above `first_layer` into the layer just
    /// below `first_layer`, and drops every other layer of that region.
    ///
    /// `None` folds every type; `Some(allowed)` keeps only entries whose type
    /// is in `allowed` — the mechanism behind a declared scope contract or a
    /// `Dynamic` slot contract's `produces`. Entries replay in the folded
    /// layer's own write order (sorted by `seq`, never `HashMap` iteration
    /// order) through the same replace rule `insert_entry` always uses, so an
    /// allowed entry whose producer already has an entry in the destination
    /// layer replaces it — matching [`merge_from`](Self::merge_from) /
    /// [`merge_filtered`](Self::merge_filtered), which do the same thing
    /// across two separate containers. An empty region changes nothing, and
    /// a `first_layer` of zero is treated as one.
    pub(crate) fn promote_newest_layer_from(
        &mut self,
        first_layer: usize,
        allowed: Option<&std::collections::HashSet<TypeId>>,
    ) {
        let first_layer = first_layer.max(1);
        if first_layer >= self.layers.len() {
            return;
        }
        let newest = self.layers.pop().expect("checked non-empty region");
        self.layers.truncate(first_layer);
        self.replay_layer(newest, allowed);
    }

    /// Replays `layer`'s entries into the current top layer in the layer's
    /// own write order, filtered by `allowed` when given — the shared core
    /// of [`promote_newest_layer_from`](Self::promote_newest_layer_from) and
    /// [`merge_entries`](Self::merge_entries).
    fn replay_layer(
        &mut self,
        layer: HashMap<OutputId, Vec<StoredEntry>>,
        allowed: Option<&std::collections::HashSet<TypeId>>,
    ) {
        let mut incoming: Vec<(OutputId, StoredEntry)> = layer
            .into_iter()
            .filter(|(id, _)| allowed.is_none_or(|allowed| allowed.contains(&id.0)))
            .flat_map(|(id, entries)| entries.into_iter().map(move |entry| (id, entry)))
            .collect();
        incoming.sort_by_key(|(_, entry)| entry.seq);

        for (id, entry) in incoming {
            self.insert_entry(id, entry.producer, entry.data.into_inner());
        }
    }

    /// Inserts a system output under a shared "unspecified producer" sentinel.
    ///
    /// Called directly by callers with no producer identity to supply (tests,
    /// caller-seeded values); the executor instead calls
    /// [`insert_from`](Self::insert_from). If an output of this type already
    /// exists under the unspecified producer, it is replaced and returned.
    ///
    /// The displaced value is returned as `Some(T)`. `None` also comes back in
    /// the pathological case where the occupying entry was stored under this
    /// type ID but holds a different type — a violation of the
    /// [`insert_boxed`](Self::insert_boxed) contract, in which case the
    /// displaced value is dropped rather than returned.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when the displaced entry holds a type other than
    /// `T`, reporting the contract violation at the write instead of leaving it
    /// to be inferred from a `None` that means something else. The check is a
    /// `debug_assert!`: release builds drop the value and return `None`.
    pub fn insert<T: Output>(&mut self, value: T) -> Option<T> {
        self.insert_from(ProducerKey::unspecified(), value)
    }

    /// Inserts a system output for `producer`.
    ///
    /// Called by the executor after a system returns a value, naming the
    /// system as the producer. If an entry from an equal [`ProducerKey`]
    /// already exists for this type, it is replaced and returned; a write
    /// from a different producer keeps both.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when the displaced entry holds a type other than
    /// `T` — see [`insert`](Self::insert).
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::{Outputs, ProducerKey};
    ///
    /// let mut outputs = Outputs::new();
    /// assert_eq!(outputs.insert_from(ProducerKey::new("a"), 1_u32), None);
    /// assert_eq!(outputs.insert_from(ProducerKey::new("b"), 2_u32), None);
    /// assert_eq!(outputs.insert_from(ProducerKey::new("a"), 3_u32), Some(1));
    /// ```
    pub fn insert_from<T: Output>(&mut self, producer: ProducerKey, value: T) -> Option<T> {
        let id = OutputId::of::<T>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(value);
        let displaced = self.insert_entry(id, producer, boxed)?;
        let downcast = displaced.downcast::<T>();
        debug_assert!(
            downcast.is_ok(),
            "output slot for `{}` held a different type — an earlier \
             `insert_boxed` violated its type-correctness contract, and the \
             displaced value is dropped rather than returned",
            type_name::<T>()
        );
        downcast.ok().map(|boxed| *boxed)
    }

    /// Inserts a type-erased system output under the shared "unspecified
    /// producer" sentinel.
    ///
    /// This is used when the concrete output type is not known at compile
    /// time. The `type_id` must match the correct type of the boxed value.
    ///
    /// If an output with this type ID already exists under the unspecified
    /// producer, it is replaced and the displaced value is returned still
    /// boxed.
    pub fn insert_boxed(
        &mut self,
        type_id: TypeId,
        value: Box<dyn Any + Send + Sync>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        self.insert_entry(OutputId(type_id), ProducerKey::unspecified(), value)
    }

    /// Inserts a type-erased system output for `producer`.
    ///
    /// Called by the executor when storing a system's boxed return value.
    /// The `type_id` must match the correct type of the boxed value. If an
    /// entry from the same producer already exists for this type, it is
    /// replaced and the displaced value is returned still boxed.
    pub fn insert_boxed_from(
        &mut self,
        producer: ProducerKey,
        type_id: TypeId,
        value: Box<dyn Any + Send + Sync>,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        self.insert_entry(OutputId(type_id), producer, value)
    }

    /// Returns `true` if an output of type `T` exists in any layer.
    #[must_use]
    pub fn contains<T: Output>(&self) -> bool {
        self.contains_by_type_id(TypeId::of::<T>())
    }

    /// Returns `true` if an output with the given `TypeId` exists in any
    /// layer.
    ///
    /// This is useful for validation when the concrete type is not known
    /// at compile time (e.g., validating access declarations).
    #[must_use]
    pub fn contains_by_type_id(&self, type_id: TypeId) -> bool {
        let id = OutputId(type_id);
        self.layers.iter().any(|layer| layer.contains_key(&id))
    }

    /// Gets an immutable reference to the newest output of type `T`, across
    /// every layer.
    ///
    /// # Errors
    ///
    /// - [`OutputError::NotFound`] if no system has produced this output type
    /// - [`OutputError::Busy`] if the output is currently being written
    pub fn get<T: Output>(&self) -> Result<OutputRef<T>, OutputError> {
        let id = OutputId::of::<T>();
        let type_name = type_name::<T>();

        // Each layer's own entries for `id` are seq-ascending (see
        // `insert_entry`), so a layer's `.last()` is that layer's newest;
        // the overall newest is necessarily one of those per-layer
        // candidates, whichever has the highest `seq`.
        let newest = self
            .layers
            .iter()
            .filter_map(|layer| layer.get(&id).and_then(|entries| entries.last()))
            .max_by_key(|entry| entry.seq)
            .ok_or(OutputError::NotFound(type_name))?;

        let guard = newest.data.try_read().ok_or(OutputError::Busy(type_name))?;

        Ok(OutputRef {
            guard,
            _marker: std::marker::PhantomData,
        })
    }

    /// Returns every entry of `T` across every layer, oldest first, paired
    /// with the producer that wrote it.
    ///
    /// Order reflects write order — never `HashMap` iteration order, and
    /// never layer order either — so it is stable across merges and layer
    /// promotions. Returns an empty `Vec` when no system has produced this
    /// output type. This is the ordered, provenance-carrying read that
    /// `OutAll<T>` builds on; the same producer may appear more than once
    /// when its entries live in different, still-open layers (e.g. two
    /// retained loop iterations that have not yet been folded together).
    ///
    /// # Errors
    ///
    /// [`OutputError::Busy`] if any entry of `T` is currently being written.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::{Outputs, ProducerKey};
    ///
    /// let mut outputs = Outputs::new();
    /// outputs.insert_from(ProducerKey::new("planner"), 1_u32);
    /// outputs.insert_from(ProducerKey::new("critic"), 2_u32);
    ///
    /// let entries = outputs.entries::<u32>().unwrap();
    /// let seen: Vec<(&str, u32)> = entries
    ///     .iter()
    ///     .map(|(producer, value)| (producer.id(), **value))
    ///     .collect();
    /// assert_eq!(seen, [("planner", 1), ("critic", 2)]);
    /// ```
    pub fn entries<T: Output>(&self) -> Result<Vec<(&ProducerKey, OutputRef<'_, T>)>, OutputError> {
        let type_name = type_name::<T>();
        let id = OutputId::of::<T>();
        let mut all: Vec<&StoredEntry> = self
            .layers
            .iter()
            .filter_map(|layer| layer.get(&id))
            .flat_map(|entries| entries.iter())
            .collect();
        all.sort_by_key(|entry| entry.seq);
        all.into_iter()
            .map(|entry| {
                let guard = entry.data.try_read().ok_or(OutputError::Busy(type_name))?;
                Ok((
                    &entry.producer,
                    OutputRef {
                        guard,
                        _marker: std::marker::PhantomData,
                    },
                ))
            })
            .collect()
    }

    /// Returns the newest entry of `T` written under a key equal to
    /// `producer` (provenance is ignored), across every layer.
    ///
    /// This is the by-producer read that `read_from` wiring builds on.
    ///
    /// # Errors
    ///
    /// - [`OutputError::NotFound`] if no matching entry exists
    /// - [`OutputError::Busy`] if it is currently being written
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::{OutputError, Outputs, ProducerKey};
    ///
    /// let mut outputs = Outputs::new();
    /// outputs.insert_from(ProducerKey::new("planner"), 1_u32);
    /// outputs.insert_from(ProducerKey::new("critic"), 2_u32);
    ///
    /// assert_eq!(*outputs.by_producer::<u32>(&ProducerKey::new("planner")).unwrap(), 1);
    /// assert!(matches!(
    ///     outputs.by_producer::<u32>(&ProducerKey::new("unknown")),
    ///     Err(OutputError::NotFound(_))
    /// ));
    /// ```
    pub fn by_producer<T: Output>(
        &self,
        producer: &ProducerKey,
    ) -> Result<OutputRef<'_, T>, OutputError> {
        let type_name = type_name::<T>();
        let id = OutputId::of::<T>();
        let newest = self
            .layers
            .iter()
            .filter_map(|layer| layer.get(&id))
            .flat_map(|entries| entries.iter())
            .filter(|entry| entry.producer == *producer)
            .max_by_key(|entry| entry.seq)
            .ok_or(OutputError::NotFound(type_name))?;
        let guard = newest.data.try_read().ok_or(OutputError::Busy(type_name))?;
        Ok(OutputRef {
            guard,
            _marker: std::marker::PhantomData,
        })
    }

    /// Clears all outputs and collapses back to a single (base) layer.
    ///
    /// Called by the executor between agent runs to reset ephemeral state.
    pub fn clear(&mut self) {
        self.layers.clear();
        self.layers.push(HashMap::new());
        self.next_seq = 0;
    }

    /// Removes and returns the newest entry across every type and every
    /// layer, as a type-erased box.
    ///
    /// Returns `None` if the store is empty.
    pub fn take_last(&mut self) -> Option<Box<dyn Any + Send + Sync>> {
        let (layer_index, newest_id) = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(layer_index, layer)| {
                layer
                    .iter()
                    .filter_map(|(id, entries)| entries.last().map(|entry| (*id, entry.seq)))
                    .max_by_key(|(_, seq)| *seq)
                    .map(|(id, seq)| (layer_index, id, seq))
            })
            .max_by_key(|(_, _, seq)| *seq)
            .map(|(layer_index, id, _)| (layer_index, id))?;

        let layer = &mut self.layers[layer_index];
        let entries = layer.get_mut(&newest_id)?;
        let entry = entries.pop().expect("newest_id names a non-empty vec");
        if entries.is_empty() {
            layer.remove(&newest_id);
        }
        Some(entry.data.into_inner())
    }

    /// Returns the number of distinct output *types* stored across every
    /// layer — unchanged by how many producers (or layers) wrote each type.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut ids: std::collections::HashSet<OutputId> = std::collections::HashSet::new();
        for layer in &self.layers {
            ids.extend(layer.keys().copied());
        }
        ids.len()
    }

    /// Returns `true` if no outputs are stored in any layer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(HashMap::is_empty)
    }

    /// Merges all outputs from `other` into this container's top layer.
    ///
    /// Consumes `other` — flattening every one of its layers first, so the
    /// caller need not know or care whether `other` itself used layers —
    /// then moves every entry into `self` in `other`'s write order — sorted
    /// by `other`'s own sequence numbers, never by `HashMap` iteration order
    /// or layer order — giving each a fresh sequence number in `self` so
    /// "newest" stays correct after the merge. An entry whose producer
    /// already has an entry of the same type in `self`'s top layer (an equal
    /// [`ProducerKey`]) replaces it; otherwise both are
    /// kept.
    ///
    /// This is the parallel-branch join: call it once per branch, in branch
    /// order, so each branch's entries land after the previous branch's in
    /// the merged write order. It is also how a non-shared scope/`Dynamic`
    /// boundary's child context merges back into its parent — both stay
    /// separate `Outputs` containers with their own layer stacks; layers
    /// never cross that boundary, only the flattened result of a merge does.
    pub fn merge_from(&mut self, other: Outputs) {
        self.merge_entries(other, None);
    }

    /// Merges only the entries of `other` whose type is in `allowed`, dropping
    /// the rest along with the rest of `other`.
    ///
    /// This is [`merge_from`](Self::merge_from) narrowed to a declared subset —
    /// the boundary primitive a declared scope contract or a `Dynamic` slot
    /// contract uses to keep undeclared interior outputs from ever reaching
    /// the parent: not merged and not overwritten, so a parent entry of the
    /// same type an undeclared interior output would otherwise have
    /// clobbered is left untouched. As with `merge_from`, an allowed entry
    /// whose producer already has an entry of the same type in `self`
    /// replaces it.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::resource::Outputs;
    /// use std::any::TypeId;
    /// use std::collections::HashSet;
    ///
    /// let mut parent = Outputs::new();
    /// parent.insert("parent note".to_owned());
    ///
    /// let mut child = Outputs::new();
    /// child.insert(7_u32);
    /// child.insert("interior note".to_owned());
    ///
    /// let allowed: HashSet<TypeId> = [TypeId::of::<u32>()].into_iter().collect();
    /// parent.merge_filtered(child, &allowed);
    ///
    /// assert_eq!(*parent.get::<u32>().unwrap(), 7);
    /// // The undeclared `String` never reached the parent.
    /// assert_eq!(*parent.get::<String>().unwrap(), "parent note");
    /// ```
    pub fn merge_filtered(&mut self, other: Outputs, allowed: &std::collections::HashSet<TypeId>) {
        self.merge_entries(other, Some(allowed));
    }

    /// Shared implementation of [`merge_from`](Self::merge_from) and
    /// [`merge_filtered`](Self::merge_filtered): flattens every layer of
    /// `other` into one, filters by `allowed` when given, sorts by `other`'s
    /// own sequence numbers so order never depends on `HashMap` iteration or
    /// which of `other`'s layers an entry came from, and replays the result
    /// through [`replay_layer`](Self::replay_layer) into `self`'s top layer.
    fn merge_entries(
        &mut self,
        other: Outputs,
        allowed: Option<&std::collections::HashSet<TypeId>>,
    ) {
        let mut flattened: HashMap<OutputId, Vec<StoredEntry>> = HashMap::new();
        for layer in other.layers {
            for (id, entries) in layer {
                flattened.entry(id).or_default().extend(entries);
            }
        }
        self.replay_layer(flattened, allowed);
    }
}

/// RAII guard for immutable output access.
///
/// This guard is returned by [`Outputs::get`] and provides read-only
/// access to the underlying output. The lock is released when the
/// guard is dropped.
pub struct OutputRef<'a, T: Output> {
    guard: RwLockReadGuard<'a, Box<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<&'a T>,
}

impl<T: Output> std::ops::Deref for OutputRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.guard
            .downcast_ref::<T>()
            .expect("output type mismatch (this is a bug)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folds the top layer into the one beneath it.
    fn pop_layer_promote(
        outputs: &mut Outputs,
        allowed: Option<&std::collections::HashSet<TypeId>>,
    ) {
        let top = outputs.layer_count() - 1;
        outputs.promote_newest_layer_from(top, allowed);
    }

    /// Drops the top layer without folding anything.
    fn discard_layer(outputs: &mut Outputs) {
        let top = outputs.layer_count() - 1;
        outputs.discard_layers_from(top);
    }

    #[derive(Debug, PartialEq)]
    struct ReasoningResult {
        action: String,
    }

    #[derive(Debug, PartialEq)]
    struct ToolResult {
        value: i32,
    }

    #[test]
    fn insert_and_get() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "search".into(),
        });

        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "search");
    }

    #[test]
    fn insert_replaces_existing() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "first".into(),
        });

        let old = outputs.insert(ReasoningResult {
            action: "second".into(),
        });
        assert_eq!(
            old,
            Some(ReasoningResult {
                action: "first".into()
            })
        );

        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "second");
    }

    /// Builds the one state where `insert` returns a `None` that does not mean
    /// "nothing was displaced": a slot keyed by `ReasoningResult`'s type ID
    /// holding a `ToolResult`. Only reachable by violating `insert_boxed`'s
    /// type-correctness contract, which is why the typed write asserts on it in
    /// debug builds. Mirrors `Resources::insert`, which documents and enforces
    /// the same rule.
    fn mismatched_occupant() -> Outputs {
        let mut outputs = Outputs::new();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ToolResult { value: 7 });
        assert!(
            outputs
                .insert_boxed(TypeId::of::<ReasoningResult>(), boxed)
                .is_none()
        );
        outputs
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn insert_drops_a_mismatched_occupant_and_reports_nothing_displaced() {
        let mut outputs = mismatched_occupant();

        // The typed write cannot downcast the occupant, so it drops it and
        // returns `None` — a `None` that does not mean "nothing was displaced".
        assert!(
            outputs
                .insert(ReasoningResult {
                    action: "replacement".into(),
                })
                .is_none(),
            "a mismatched occupant is dropped rather than returned"
        );

        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "replacement"
        );
        assert_eq!(outputs.len(), 1);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "output slot for")]
    fn insert_asserts_in_debug_when_the_occupant_is_a_different_type() {
        let _ = mismatched_occupant().insert(ReasoningResult {
            action: "replacement".into(),
        });
    }

    #[test]
    fn multiple_output_types() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "search".into(),
        });
        outputs.insert(ToolResult { value: 42 });

        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "search");
        assert_eq!(outputs.get::<ToolResult>().unwrap().value, 42);
    }

    #[test]
    fn not_found_error() {
        let outputs = Outputs::new();
        let result = outputs.get::<ReasoningResult>();
        assert!(matches!(result, Err(OutputError::NotFound(_))));
    }

    #[test]
    fn clear_removes_all() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });
        outputs.insert(ToolResult { value: 1 });

        assert_eq!(outputs.len(), 2);

        outputs.clear();

        assert!(outputs.is_empty());
        assert!(outputs.get::<ReasoningResult>().is_err());
        assert!(outputs.get::<ToolResult>().is_err());
    }

    #[test]
    fn contains_checks_presence() {
        let mut outputs = Outputs::new();

        assert!(!outputs.contains::<ReasoningResult>());
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });
        assert!(outputs.contains::<ReasoningResult>());
    }

    #[test]
    fn multiple_concurrent_reads() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        let read1 = outputs.get::<ReasoningResult>().unwrap();
        let read2 = outputs.get::<ReasoningResult>().unwrap();

        assert_eq!(read1.action, read2.action);
    }

    #[test]
    fn insert_boxed_type_erased() {
        let mut outputs = Outputs::new();

        // Insert via type-erased method
        let type_id = TypeId::of::<ReasoningResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ReasoningResult {
            action: "boxed".into(),
        });
        assert!(outputs.insert_boxed(type_id, boxed).is_none());

        // Should be retrievable via normal get
        assert!(outputs.contains::<ReasoningResult>());
        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "boxed");
    }

    #[test]
    fn contains_by_type_id() {
        let mut outputs = Outputs::new();

        let reasoning_id = TypeId::of::<ReasoningResult>();
        let tool_id = TypeId::of::<ToolResult>();

        assert!(!outputs.contains_by_type_id(reasoning_id));
        assert!(!outputs.contains_by_type_id(tool_id));

        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        assert!(outputs.contains_by_type_id(reasoning_id));
        assert!(!outputs.contains_by_type_id(tool_id));
    }

    #[test]
    fn output_ref_raii_releases_on_drop() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        // Take a read borrow
        {
            let _borrow = outputs.get::<ReasoningResult>().unwrap();
            // Multiple reads should still work (RwLock allows multiple readers)
            assert!(outputs.get::<ReasoningResult>().is_ok());
        }
        // After drop, reads should still succeed
        assert!(outputs.get::<ReasoningResult>().is_ok());
    }

    #[test]
    fn insert_boxed_replaces_existing() {
        let mut outputs = Outputs::new();

        // Insert first via normal method
        outputs.insert(ReasoningResult {
            action: "first".into(),
        });

        // Replace via type-erased method
        let type_id = TypeId::of::<ReasoningResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ReasoningResult {
            action: "second".into(),
        });
        let displaced = outputs
            .insert_boxed(type_id, boxed)
            .expect("replacing an occupied slot returns its previous occupant");
        assert_eq!(
            displaced.downcast::<ReasoningResult>().unwrap().action,
            "first"
        );

        // Should have the new value
        let result = outputs.get::<ReasoningResult>().unwrap();
        assert_eq!(result.action, "second");

        // Should still be only one entry
        assert_eq!(outputs.len(), 1);
    }

    // ─────────────────────────────────────────────────────────────────────
    // merge_from tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn merge_from_overwrites_existing() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "original".into(),
        });

        let mut source = Outputs::new();
        source.insert(ReasoningResult {
            action: "overwritten".into(),
        });

        target.merge_from(source);

        assert_eq!(target.len(), 1);
        assert_eq!(
            target.get::<ReasoningResult>().unwrap().action,
            "overwritten"
        );
    }

    #[test]
    fn merge_from_combines_different_types() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "reasoning".into(),
        });

        let mut source = Outputs::new();
        source.insert(ToolResult { value: 99 });

        target.merge_from(source);

        assert_eq!(target.len(), 2);
        assert_eq!(target.get::<ReasoningResult>().unwrap().action, "reasoning");
        assert_eq!(target.get::<ToolResult>().unwrap().value, 99);
    }

    #[test]
    fn output_id_of_method() {
        let id = OutputId::of::<ReasoningResult>();
        let id2 = OutputId::of::<ReasoningResult>();
        let tool_id = OutputId::of::<ToolResult>();

        // Same type produces same id
        assert_eq!(id, id2);

        // Different types produce different ids
        assert_ne!(id, tool_id);
    }

    // ─────────────────────────────────────────────────────────────────────
    // take_last tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn take_last_returns_most_recent_insert() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "first".into(),
        });
        outputs.insert(ToolResult { value: 42 });

        let last = outputs.take_last().unwrap();
        let tool = last.downcast_ref::<ToolResult>().unwrap();
        assert_eq!(tool.value, 42);
    }

    #[test]
    fn take_last_returns_none_when_empty() {
        let mut outputs = Outputs::new();
        assert!(outputs.take_last().is_none());
    }

    #[test]
    fn take_last_removes_from_storage() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        let _ = outputs.take_last();

        assert!(!outputs.contains::<ReasoningResult>());
        assert!(outputs.is_empty());
    }

    #[test]
    fn take_last_returns_none_after_clear() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });
        outputs.clear();

        assert!(outputs.take_last().is_none());
    }

    #[test]
    fn take_last_idempotent() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "test".into(),
        });

        assert!(outputs.take_last().is_some());
        assert!(outputs.take_last().is_none());
    }

    #[test]
    fn take_last_after_insert_boxed() {
        let mut outputs = Outputs::new();

        let type_id = TypeId::of::<ToolResult>();
        let boxed: Box<dyn Any + Send + Sync> = Box::new(ToolResult { value: 99 });
        outputs.insert_boxed(type_id, boxed);

        let last = outputs.take_last().unwrap();
        let tool = last.downcast_ref::<ToolResult>().unwrap();
        assert_eq!(tool.value, 99);
    }

    /// Regression coverage for the bug DESIGN.md calls Finding 2: after a
    /// filtered merge drops the child's chronologically-last write, the
    /// newest *allowed* entry must still win `take_last`, not whatever the
    /// target's own last write happened to be before the merge. The
    /// executed-graph version of this (a real declared-scope contract) lives
    /// in `polaris_graph`'s `scope_contract_boundary` tests.
    #[test]
    fn take_last_after_filtered_merge_returns_the_newest_allowed_entry() {
        let mut target = Outputs::new();

        let mut source = Outputs::new();
        source.insert(ReasoningResult {
            action: "allowed".into(),
        });
        source.insert(ToolResult { value: 1 }); // written last, but filtered out

        let allowed: std::collections::HashSet<TypeId> =
            [TypeId::of::<ReasoningResult>()].into_iter().collect();
        target.merge_filtered(source, &allowed);

        let last = target.take_last().unwrap();
        assert_eq!(
            last.downcast_ref::<ReasoningResult>().unwrap().action,
            "allowed",
            "the allowed entry must win even though it was not the child's \
             chronologically last write"
        );
    }

    #[test]
    fn merge_filtered_drops_types_not_allowed() {
        let mut target = Outputs::new();
        let mut source = Outputs::new();
        source.insert(ReasoningResult {
            action: "kept".into(),
        });
        source.insert(ToolResult { value: 7 });

        let allowed: std::collections::HashSet<TypeId> =
            [TypeId::of::<ReasoningResult>()].into_iter().collect();
        target.merge_filtered(source, &allowed);

        assert_eq!(target.len(), 1);
        assert_eq!(target.get::<ReasoningResult>().unwrap().action, "kept");
        assert!(target.get::<ToolResult>().is_err());
    }

    #[test]
    fn merge_filtered_leaves_unmatched_parent_entry_untouched() {
        let mut target = Outputs::new();
        target.insert(ToolResult { value: 1 });

        let mut source = Outputs::new();
        source.insert(ToolResult { value: 99 });

        // `ToolResult` is not in the allowed set, so the source's write to it
        // never reaches the target — the parent's own value survives.
        let allowed: std::collections::HashSet<TypeId> =
            [TypeId::of::<ReasoningResult>()].into_iter().collect();
        target.merge_filtered(source, &allowed);

        assert_eq!(target.get::<ToolResult>().unwrap().value, 1);
    }

    #[test]
    fn merge_filtered_propagates_last_type_id_only_when_allowed() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "target".into(),
        });

        let mut source = Outputs::new();
        source.insert(ToolResult { value: 42 });

        let allowed: std::collections::HashSet<TypeId> = std::collections::HashSet::new();
        target.merge_filtered(source, &allowed);

        // Nothing was allowed through, so `take_last` still reflects the
        // target's own last insert rather than the dropped source entry.
        let last = target.take_last().unwrap();
        assert!(last.downcast_ref::<ReasoningResult>().is_some());
    }

    #[test]
    fn merge_from_propagates_last_type_id() {
        let mut target = Outputs::new();
        target.insert(ReasoningResult {
            action: "target".into(),
        });

        let mut source = Outputs::new();
        source.insert(ToolResult { value: 42 });

        target.merge_from(source);

        let last = target.take_last().unwrap();
        assert!(last.downcast_ref::<ToolResult>().is_some());
    }

    // ─────────────────────────────────────────────────────────────────────
    // ProducerKey / per-producer storage tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn two_different_producers_writing_the_same_type_both_survive() {
        let mut outputs = Outputs::new();
        let a = ProducerKey::new("node_a");
        let b = ProducerKey::new("node_b");

        outputs.insert_from(a.clone(), ReasoningResult { action: "a".into() });
        outputs.insert_from(b.clone(), ReasoningResult { action: "b".into() });

        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "b",
            "Out<T> keeps returning the newest entry"
        );
        let entries = outputs.entries::<ReasoningResult>().unwrap();
        assert_eq!(entries.len(), 2, "both producers' entries are kept");
        assert_eq!(entries[0].0, &a);
        assert_eq!(entries[1].0, &b);
    }

    #[test]
    fn same_producer_writing_repeatedly_keeps_one_entry() {
        let mut outputs = Outputs::new();
        let node = ProducerKey::new("node_a");

        for i in 0..100u64 {
            outputs.insert_from(
                node.clone().with_provenance(i),
                ReasoningResult {
                    action: format!("iter_{i}"),
                },
            );
        }

        assert_eq!(
            outputs.entries::<ReasoningResult>().unwrap().len(),
            1,
            "a loop writing the same type 100 times keeps one entry for that producer"
        );
        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "iter_99");
    }

    #[test]
    fn same_node_different_runs_replaces_rather_than_accumulates() {
        // A session turn reuses one `SystemContext` (and thus one `Outputs`)
        // across many turns without clearing it, so a fresh run id carried as
        // provenance must not by itself keep old turns' entries alive.
        let mut outputs = Outputs::new();

        for run in 0..50 {
            let producer = ProducerKey::new("node_a").with_provenance(format!("run_{run}"));
            outputs.insert_from(
                producer,
                ReasoningResult {
                    action: format!("turn_{run}"),
                },
            );
        }

        assert_eq!(outputs.entries::<ReasoningResult>().unwrap().len(), 1);
        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "turn_49");
    }

    #[test]
    fn by_producer_returns_the_matching_producers_entry() {
        let mut outputs = Outputs::new();
        let a = ProducerKey::new("node_a");
        let b = ProducerKey::new("node_b");

        outputs.insert_from(a.clone(), ReasoningResult { action: "a".into() });
        outputs.insert_from(b.clone(), ReasoningResult { action: "b".into() });

        assert_eq!(
            outputs.by_producer::<ReasoningResult>(&a).unwrap().action,
            "a"
        );
        assert_eq!(
            outputs.by_producer::<ReasoningResult>(&b).unwrap().action,
            "b"
        );

        let unknown = ProducerKey::new("node_c");
        assert!(matches!(
            outputs.by_producer::<ReasoningResult>(&unknown),
            Err(OutputError::NotFound(_))
        ));
    }

    #[test]
    fn entries_returns_empty_when_type_never_written() {
        let outputs = Outputs::new();
        assert!(outputs.entries::<ReasoningResult>().unwrap().is_empty());
    }

    #[test]
    fn merge_from_preserves_branch_write_order_regardless_of_hashmap_order() {
        // Simulate three parallel branches merged in branch-index order, each
        // writing the same type from a distinct producer; entries must come
        // back in branch order even though `HashMap` iteration order is
        // unspecified.
        let mut parent = Outputs::new();
        for branch in 0..3 {
            let mut child = Outputs::new();
            let producer = ProducerKey::new(format!("node_{branch}"));
            child.insert_from(
                producer,
                ReasoningResult {
                    action: format!("branch_{branch}"),
                },
            );
            parent.merge_from(child);
        }

        let entries = parent.entries::<ReasoningResult>().unwrap();
        let actions: Vec<_> = entries.iter().map(|(_, r)| r.action.clone()).collect();
        assert_eq!(actions, vec!["branch_0", "branch_1", "branch_2"]);
    }

    #[test]
    fn producer_key_equality_ignores_provenance() {
        let a = ProducerKey::new("node_a").with_provenance(3_u64);
        let b = ProducerKey::new("node_a").with_provenance("other provenance");
        assert_eq!(a, b);
        assert_eq!(a.provenance::<u64>(), Some(&3));

        let c = ProducerKey::new("node_b").with_provenance(3_u64);
        assert_ne!(a, c);
    }

    #[test]
    fn a_replacing_write_keeps_the_newer_provenance() {
        let mut outputs = Outputs::new();
        outputs.insert_from(ProducerKey::new("node_a").with_provenance(1_u64), 10_i32);
        outputs.insert_from(ProducerKey::new("node_a").with_provenance(2_u64), 20_i32);

        let entries = outputs.entries::<i32>().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.provenance::<u64>(), Some(&2));
        assert_eq!(*entries[0].1, 20);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Layer stack tests
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn a_pushed_layer_is_visible_to_reads_immediately() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "base".into(),
        });

        outputs.push_layer();
        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "base",
            "an enclosing layer's entry is visible from inside a fresh layer \
             that never wrote its own"
        );
    }

    #[test]
    fn a_write_in_a_layer_does_not_replace_an_enclosing_layer_entry() {
        let node = ProducerKey::new("node_a");
        let mut outputs = Outputs::new();
        outputs.insert_from(
            node.clone(),
            ReasoningResult {
                action: "iter_0".into(),
            },
        );

        outputs.push_layer();
        outputs.insert_from(
            node.clone(),
            ReasoningResult {
                action: "iter_1".into(),
            },
        );

        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "iter_1",
            "the newest entry across every layer wins the ordinary read"
        );
        let entries = outputs.entries::<ReasoningResult>().unwrap();
        let actions: Vec<_> = entries.iter().map(|(_, v)| v.action.clone()).collect();
        assert_eq!(
            entries.len(),
            2,
            "the same producer keeps one entry per layer, not one overall, \
             until the layers are folded together: {actions:?}"
        );
    }

    #[test]
    fn pop_layer_promote_folds_the_top_layer_into_the_one_beneath() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "base".into(),
        });
        outputs.push_layer();
        outputs.insert(ToolResult { value: 1 });

        pop_layer_promote(&mut outputs, None);

        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "base");
        assert_eq!(outputs.get::<ToolResult>().unwrap().value, 1);
    }

    #[test]
    fn pop_layer_promote_replaces_a_same_producer_entry_below() {
        let node = ProducerKey::new("node_a");
        let mut outputs = Outputs::new();
        outputs.insert_from(
            node.clone(),
            ReasoningResult {
                action: "iter_0".into(),
            },
        );
        outputs.push_layer();
        outputs.insert_from(
            node.clone(),
            ReasoningResult {
                action: "iter_1".into(),
            },
        );

        pop_layer_promote(&mut outputs, None);

        let entries = outputs.entries::<ReasoningResult>().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "folding into a layer with the same producer's entry replaces it, \
             exactly like an ordinary same-layer write"
        );
        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "iter_1");
    }

    #[test]
    fn pop_layer_promote_filters_by_allowed_type() {
        let mut outputs = Outputs::new();
        outputs.push_layer();
        outputs.insert(ReasoningResult {
            action: "kept".into(),
        });
        outputs.insert(ToolResult { value: 7 });

        let allowed: std::collections::HashSet<TypeId> =
            [TypeId::of::<ReasoningResult>()].into_iter().collect();
        pop_layer_promote(&mut outputs, Some(&allowed));

        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "kept");
        assert!(outputs.get::<ToolResult>().is_err());
    }

    #[test]
    fn discard_layer_drops_everything_it_holds() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "base".into(),
        });
        outputs.push_layer();
        outputs.insert(ToolResult { value: 1 });
        outputs.insert(ReasoningResult {
            action: "shadowed".into(),
        });

        discard_layer(&mut outputs);

        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "base",
            "the enclosing layer's entry is untouched by discarding the one above it"
        );
        assert!(
            outputs.get::<ToolResult>().is_err(),
            "an entry that only ever existed in the discarded layer is gone"
        );
    }

    #[test]
    fn layer_operations_never_touch_the_base_layer() {
        let mut outputs = Outputs::new();
        outputs.insert(ToolResult { value: 1 });

        outputs.promote_newest_layer_from(0, None);
        outputs.promote_newest_layer_from(1, None);
        outputs.retain_layers_from(0, 0, []);
        outputs.retain_layers_from(5, 0, []);
        outputs.discard_layers_from(0);

        assert_eq!(outputs.layer_count(), 1);
        assert_eq!(outputs.get::<ToolResult>().unwrap().value, 1);
    }

    #[test]
    fn retention_of_zero_drops_every_entry_of_that_type_in_the_region() {
        let mut outputs = Outputs::new();
        outputs.insert(ToolResult { value: 0 });
        outputs.push_layer();
        outputs.insert(ToolResult { value: 1 });
        outputs.insert(ReasoningResult {
            action: "kept".into(),
        });

        outputs.retain_layers_from(1, 1, [(TypeId::of::<ToolResult>(), 0)]);

        let values: Vec<i32> = outputs
            .entries::<ToolResult>()
            .unwrap()
            .iter()
            .map(|(_, result)| result.value)
            .collect();
        assert_eq!(
            values,
            vec![0],
            "only the entry below the region survives a retention of zero"
        );
        assert_eq!(
            outputs.get::<ReasoningResult>().unwrap().action,
            "kept",
            "a type with retention 1 keeps its newest layer"
        );
    }

    #[test]
    fn retention_keeps_the_newest_layers() {
        let mut outputs = Outputs::new();
        for value in 0..4 {
            outputs.retain_layers_from(1, 2, []);
            outputs.push_layer();
            outputs.insert(ToolResult { value });
        }

        let values: Vec<i32> = outputs
            .entries::<ToolResult>()
            .unwrap()
            .iter()
            .map(|(_, result)| result.value)
            .collect();
        assert_eq!(
            values,
            vec![1, 2, 3],
            "the two newest completed layers plus the open one"
        );
    }

    #[test]
    fn take_last_finds_the_newest_entry_regardless_of_layer() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "base".into(),
        });
        outputs.push_layer();
        outputs.insert(ToolResult { value: 42 });

        // The base layer's own write is older than the pushed layer's, so
        // `take_last` must find the latter even though it lives one layer up.
        let last = outputs.take_last().unwrap();
        assert_eq!(last.downcast_ref::<ToolResult>().unwrap().value, 42);
    }

    #[test]
    fn take_last_after_discard_falls_back_to_the_enclosing_layer() {
        let mut outputs = Outputs::new();
        outputs.insert(ReasoningResult {
            action: "base".into(),
        });
        outputs.push_layer();
        outputs.insert(ToolResult { value: 42 });
        discard_layer(&mut outputs);

        let last = outputs.take_last().unwrap();
        assert_eq!(
            last.downcast_ref::<ReasoningResult>().unwrap().action,
            "base"
        );
    }

    #[test]
    fn nested_layers_promote_in_order() {
        // Two nested loop iterations (an inner loop nested in an outer
        // iteration's own layer): pushing twice and promoting twice must
        // leave the base with both writes, newest reflecting the inner one.
        let mut outputs = Outputs::new();
        outputs.push_layer(); // outer iteration 0
        outputs.insert(ReasoningResult {
            action: "outer".into(),
        });
        outputs.push_layer(); // inner iteration 0
        outputs.insert(ToolResult { value: 1 });
        pop_layer_promote(&mut outputs, None); // inner loop exits
        pop_layer_promote(&mut outputs, None); // outer loop exits

        assert_eq!(outputs.get::<ReasoningResult>().unwrap().action, "outer");
        assert_eq!(outputs.get::<ToolResult>().unwrap().value, 1);
    }

    #[test]
    fn merge_from_flattens_a_source_with_open_layers() {
        // A child `Outputs` (e.g. a scope's) can itself have pushed layers if
        // something inside it used them; `merge_from`/`merge_filtered` must
        // not need to know or care.
        let mut source = Outputs::new();
        source.insert(ReasoningResult {
            action: "outer".into(),
        });
        source.push_layer();
        source.insert(ToolResult { value: 9 });

        let mut target = Outputs::new();
        target.merge_from(source);

        assert_eq!(target.get::<ReasoningResult>().unwrap().action, "outer");
        assert_eq!(target.get::<ToolResult>().unwrap().value, 9);
    }

    #[test]
    fn is_empty_and_len_account_for_every_layer() {
        let mut outputs = Outputs::new();
        assert!(outputs.is_empty());

        outputs.push_layer();
        outputs.insert(ReasoningResult { action: "x".into() });
        assert!(!outputs.is_empty());
        assert_eq!(outputs.len(), 1);

        discard_layer(&mut outputs);
        assert!(outputs.is_empty());
        assert_eq!(outputs.len(), 0);
    }
}
