//! Scoped producer bindings for the `read_from` graph wiring feature.
//!
//! A [`ReadFromBindings`] set names, for some output types, the one producer
//! whose entry [`Out<T>`](super::Out) should read. It takes effect only
//! inside a [`ReadFromScope`], opened with
//! [`SystemContext::read_from_scope`](super::SystemContext::read_from_scope)
//! around one system's `run` call. While the scope is alive,
//! [`Out<T>`](super::Out)'s [`fetch`](super::SystemParam::fetch) resolves the
//! bound producer's entry (via
//! [`Outputs::by_producer`](crate::resource::Outputs::by_producer)) instead of
//! the newest write of `T` from any producer. When the scope ends — normally,
//! by an early return, or because the future that owns it was dropped — the
//! bindings that were in place before are restored.
//!
//! `polaris_graph` builds these sets from a graph's `read_from` wiring; a
//! system never needs to touch them.

use super::SystemContext;
use crate::resource::ProducerKey;
use hashbrown::HashMap;
use std::any::TypeId;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// Which producer each output type should resolve to, for the duration of a
/// [`ReadFromScope`].
///
/// # Examples
///
/// ```
/// use polaris_system::param::{Out, ReadFromBindings, SystemContext, SystemParam};
/// use polaris_system::resource::ProducerKey;
/// use std::any::TypeId;
/// use std::sync::Arc;
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output_from(ProducerKey::new("planner"), 1_u32);
/// ctx.insert_output_from(ProducerKey::new("critic"), 2_u32);
///
/// let bindings = ReadFromBindings::from_pairs([(TypeId::of::<u32>(), ProducerKey::new("planner"))]);
/// {
///     let scope = ctx.read_from_scope(Some(Arc::new(bindings)));
///     assert_eq!(*Out::<u32>::fetch(&scope).unwrap(), 1);
/// }
/// // Outside the scope, `Out<u32>` reads the newest entry again.
/// assert_eq!(*Out::<u32>::fetch(&ctx).unwrap(), 2);
/// ```
#[derive(Debug, Clone, Default)]
pub struct ReadFromBindings {
    by_type: HashMap<TypeId, ProducerKey>,
}

impl ReadFromBindings {
    /// Builds a binding set from `(output type, producer)` pairs.
    ///
    /// A duplicate `type_id` keeps the last pair in iteration order.
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (TypeId, ProducerKey)>) -> Self {
        Self {
            by_type: pairs.into_iter().collect(),
        }
    }

    /// The producer bound to `type_id`, if this binding set names one.
    #[must_use]
    pub(crate) fn producer_for(&self, type_id: TypeId) -> Option<&ProducerKey> {
        self.by_type.get(&type_id)
    }

    /// Returns `true` if this binding set names no producers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }
}

/// Installs `read_from` bindings on a [`SystemContext`] and restores the
/// previous ones when dropped.
///
/// Opened by [`SystemContext::read_from_scope`]. The scope dereferences to
/// the context, so the system that should see the bindings runs through it.
/// Restoring is tied to `Drop`, not to code after an `.await`, so a future
/// dropped while the system runs still leaves the context with its previous
/// bindings. See the [`ReadFromBindings`] example.
#[must_use = "dropping the scope at once restores the previous bindings"]
pub struct ReadFromScope<'a, 'p> {
    ctx: &'a mut SystemContext<'p>,
    previous: Option<Arc<ReadFromBindings>>,
}

impl<'a, 'p> ReadFromScope<'a, 'p> {
    /// Installs `bindings` on `ctx`, remembering what was installed before.
    pub(super) fn new(
        ctx: &'a mut SystemContext<'p>,
        bindings: Option<Arc<ReadFromBindings>>,
    ) -> Self {
        let previous = std::mem::replace(&mut ctx.read_from, bindings);
        Self { ctx, previous }
    }
}

impl<'p> Deref for ReadFromScope<'_, 'p> {
    type Target = SystemContext<'p>;

    fn deref(&self) -> &Self::Target {
        self.ctx
    }
}

impl<'p> DerefMut for ReadFromScope<'_, 'p> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx
    }
}

impl Drop for ReadFromScope<'_, '_> {
    fn drop(&mut self) {
        self.ctx.read_from = self.previous.take();
    }
}

impl std::fmt::Debug for ReadFromScope<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadFromScope")
            .field("installed", &self.ctx.read_from)
            .field("previous", &self.previous)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bindings_has_no_producer() {
        let bindings = ReadFromBindings::default();
        assert!(bindings.is_empty());
        assert!(bindings.producer_for(TypeId::of::<i32>()).is_none());
    }

    #[test]
    fn from_pairs_looks_up_by_type() {
        let producer = ProducerKey::new("node_a");
        let bindings = ReadFromBindings::from_pairs([(TypeId::of::<i32>(), producer.clone())]);
        assert!(!bindings.is_empty());
        assert_eq!(bindings.producer_for(TypeId::of::<i32>()), Some(&producer));
        assert!(bindings.producer_for(TypeId::of::<u8>()).is_none());
    }

    #[test]
    fn dropping_a_scope_restores_the_previous_bindings() {
        let outer = Arc::new(ReadFromBindings::from_pairs([(
            TypeId::of::<i32>(),
            ProducerKey::new("outer"),
        )]));
        let inner = Arc::new(ReadFromBindings::from_pairs([(
            TypeId::of::<i32>(),
            ProducerKey::new("inner"),
        )]));

        let mut ctx = SystemContext::new();
        {
            let mut outer_scope = ctx.read_from_scope(Some(outer));
            {
                let inner_scope = outer_scope.read_from_scope(Some(inner));
                assert_eq!(
                    inner_scope.read_from_producer(TypeId::of::<i32>()),
                    Some(&ProducerKey::new("inner"))
                );
            }
            assert_eq!(
                outer_scope.read_from_producer(TypeId::of::<i32>()),
                Some(&ProducerKey::new("outer"))
            );
            {
                let cleared = outer_scope.read_from_scope(None);
                assert_eq!(cleared.read_from_producer(TypeId::of::<i32>()), None);
            }
        }
        assert_eq!(ctx.read_from_producer(TypeId::of::<i32>()), None);
    }
}
