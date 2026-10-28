//! The [`OutAll`] system parameter.

use super::{Access, ParamError, SystemAccess, SystemContext, SystemParam};
use crate::resource::{Output, OutputRef, ProducerKey};

/// One output returned by [`OutAll`], together with its producer.
///
/// The value is borrowed from the execution context. Its producer is the
/// opaque [`ProducerKey`] the writer stored it under. Graph executions attach
/// their own provenance to that key — in `polaris_graph`, read it with
/// `ProducerInfo::of(entry.producer())` to get the node, its name, the run,
/// and the loop iteration.
///
/// # Examples
///
/// ```
/// use polaris_system::param::{OutAll, SystemContext, SystemParam};
/// use polaris_system::resource::ProducerKey;
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output_from(ProducerKey::new("planner"), 1_u32);
/// ctx.insert_output_from(ProducerKey::new("critic"), 2_u32);
///
/// let all = OutAll::<u32>::fetch(&ctx).unwrap();
/// let entry = &all[1];
/// assert_eq!(entry.producer().id(), "critic");
/// assert_eq!(*entry.value(), 2);
/// ```
pub struct OutAllEntry<'w, T: Output> {
    producer: &'w ProducerKey,
    value: OutputRef<'w, T>,
}

impl<T: Output> OutAllEntry<'_, T> {
    /// Returns the producer that wrote this output.
    #[must_use]
    pub fn producer(&self) -> &ProducerKey {
        self.producer
    }

    /// Returns the output value.
    #[must_use]
    pub fn value(&self) -> &T {
        &self.value
    }
}

impl<T: Output + std::fmt::Debug> std::fmt::Debug for OutAllEntry<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutAllEntry")
            .field("producer", self.producer)
            .field("value", self.value())
            .finish()
    }
}

impl<T: Output> std::ops::Deref for OutAllEntry<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

/// Read-only access to every visible output of type `T`, oldest first.
///
/// `OutAll<T>` is useful when several preceding systems produce the same type
/// and the consumer needs to fold or collect every result. Each
/// [`OutAllEntry`] retains a borrowed value and a reference to its
/// [`ProducerKey`], so fetching this parameter does not clone any outputs.
///
/// Like [`super::Out<T>`], it reads the output channel for `T`. It therefore
/// has the same scheduling access declaration and is visible only where an
/// ordinary `Out<T>` would be visible.
///
/// `OutAll<T>` does not participate in graph-level `read_from` binding: it
/// always returns every visible entry, so pinning it to one producer would
/// contradict its purpose. Wiring `read_from` onto a type read only through
/// `OutAll<T>` would have no effect. `polaris_graph` rejects that at
/// `validate()` time (`ValidationError::ReadFromUnread`) rather than
/// silently accepting a binding that cannot do anything.
///
/// # Example
///
/// ```
/// use polaris_system::param::OutAll;
/// use polaris_system::system;
///
/// #[derive(Debug)]
/// struct Score(u32);
///
/// #[system]
/// async fn total(scores: OutAll<Score>) -> u32 {
///     scores.iter().map(|score| score.0).sum()
/// }
/// ```
pub struct OutAll<'w, T: Output> {
    entries: Vec<OutAllEntry<'w, T>>,
}

impl<'w, T: Output> OutAll<'w, T> {
    /// Returns the entries in oldest-first write order.
    #[must_use]
    pub fn entries(&self) -> &[OutAllEntry<'w, T>] {
        &self.entries
    }
}

impl<T: Output + std::fmt::Debug> std::fmt::Debug for OutAll<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutAll")
            .field("entries", &self.entries)
            .finish()
    }
}

impl<'w, T: Output> std::ops::Deref for OutAll<'w, T> {
    type Target = [OutAllEntry<'w, T>];

    fn deref(&self) -> &Self::Target {
        self.entries()
    }
}

impl<'w, T: Output> AsRef<[OutAllEntry<'w, T>]> for OutAll<'w, T> {
    fn as_ref(&self) -> &[OutAllEntry<'w, T>] {
        &self.entries
    }
}

impl<'a, 'w, T: Output> IntoIterator for &'a OutAll<'w, T> {
    type Item = &'a OutAllEntry<'w, T>;
    type IntoIter = std::slice::Iter<'a, OutAllEntry<'w, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl<'a, T: Output> SystemParam for OutAll<'a, T> {
    type Item<'w> = OutAll<'w, T>;

    fn fetch<'w>(ctx: &'w SystemContext<'_>) -> Result<Self::Item<'w>, ParamError> {
        let entries = ctx.outputs().entries::<T>().map_err(|error| match error {
            crate::resource::OutputError::NotFound(name) => ParamError::OutputNotFound(name),
            crate::resource::OutputError::Busy(name) => ParamError::BorrowConflict(name),
        })?;
        Ok(OutAll {
            entries: entries
                .into_iter()
                .map(|(producer, value)| OutAllEntry { producer, value })
                .collect(),
        })
    }

    fn access() -> SystemAccess {
        let mut access = SystemAccess::new();
        access.outputs.push(Access::read::<T>());
        access
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Score;

    #[test]
    fn out_all_debug_shows_each_value() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(Score);
        let values = OutAll::<Score>::fetch(&ctx).expect("the output was inserted");
        let rendered = format!("{values:?}");
        assert!(rendered.starts_with("OutAll"), "{rendered}");
        assert!(rendered.contains("value: Score"), "{rendered}");
    }

    #[test]
    fn out_all_does_not_declare_a_bindable_output() {
        let access = <OutAll<Score>>::access();
        assert_eq!(access.outputs.len(), 1, "still reads the output channel");
        assert!(
            access.bindable_outputs.is_empty(),
            "OutAll<T> ignores any installed read_from binding, so it must \
             not claim the type as bindable — see OutAll's doc comment"
        );
    }
}
