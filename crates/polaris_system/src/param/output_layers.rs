//! The [`OutputLayerScope`] guard over a context's output layers.
//!
//! [`Outputs`](crate::resource::Outputs) keeps its entries in a stack of
//! layers. This module is the only public way to open and close layers: a
//! caller opens a *region* with
//! [`SystemContext::output_layer_scope`], pushes layers inside it, and either
//! promotes the newest one ([`OutputLayerScope::promote_newest`]) or lets the
//! scope drop, which discards the whole region. Because closing is tied to
//! the guard rather than to code that runs after an `.await`, a future that
//! is dropped half-way through (for example a turn cancelled when an HTTP
//! client disconnects) still leaves the context's layers exactly as they were
//! before the region was opened.

use super::SystemContext;
use std::any::TypeId;
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

/// A region of output layers opened on a [`SystemContext`] and closed when
/// the scope ends.
///
/// Opened by [`SystemContext::output_layer_scope`]. The region starts empty
/// and covers every layer pushed through [`push_layer`](Self::push_layer)
/// while the scope is alive. Every write made through the context during
/// that time lands in the newest layer, and every read still sees every
/// layer, inside the region and below it.
///
/// The region closes in exactly one of two ways:
///
/// - [`promote_newest`](Self::promote_newest) folds the newest layer of the
///   region into the layer below the region (optionally filtered by type) and
///   drops the older ones.
/// - Dropping the scope without promoting, including when the future that
///   owns it is dropped, discards every layer of the region. The layers below
///   are left exactly as they were when the scope was opened.
///
/// The scope dereferences to the context, so work that must happen inside
/// the region (running systems, opening nested scopes) goes through it.
/// Nested scopes close before their parents by construction, because each
/// one borrows the context from the one around it.
///
/// None of the methods panic. If the context's outputs are replaced while
/// the scope is open (for example by
/// [`take_outputs`](SystemContext::take_outputs) or
/// [`clear_outputs`](SystemContext::clear_outputs)), the region no longer
/// exists and the scope's methods do nothing.
///
/// # Examples
///
/// ```
/// use polaris_system::param::SystemContext;
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output(1_i32);
///
/// // A region that fails: dropping the scope discards what it wrote.
/// {
///     let mut scope = ctx.output_layer_scope();
///     scope.push_layer();
///     scope.insert_output("draft".to_owned());
///     assert!(scope.contains_output::<String>());
/// }
/// assert!(!ctx.contains_output::<String>());
///
/// // A region that succeeds: the newest layer is promoted.
/// let mut scope = ctx.output_layer_scope();
/// scope.push_layer();
/// scope.insert_output("final".to_owned());
/// scope.promote_newest(None);
/// assert_eq!(*ctx.get_output::<String>().unwrap(), "final");
/// assert_eq!(*ctx.get_output::<i32>().unwrap(), 1);
/// ```
#[must_use = "dropping the scope at once discards the region it opened"]
pub struct OutputLayerScope<'a, 'p> {
    ctx: &'a mut SystemContext<'p>,
    /// Index of the first layer that belongs to this region; every layer
    /// below it existed before the scope was opened.
    first_layer: usize,
    /// `false` once [`promote_newest`](Self::promote_newest) has closed the
    /// region, so `Drop` leaves the promoted layer alone.
    open: bool,
}

impl<'a, 'p> OutputLayerScope<'a, 'p> {
    /// Opens an empty region above every layer that `ctx` currently has.
    pub(super) fn new(ctx: &'a mut SystemContext<'p>) -> Self {
        let first_layer = ctx.outputs.layer_count();
        Self {
            ctx,
            first_layer,
            open: true,
        }
    }

    /// Pushes a new, empty layer on top of the region. Every write made
    /// through the context after this lands in the new layer.
    pub fn push_layer(&mut self) {
        self.ctx.outputs.push_layer();
    }

    /// Returns how many layers the region holds.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::param::SystemContext;
    ///
    /// let mut ctx = SystemContext::new();
    /// let mut scope = ctx.output_layer_scope();
    /// assert_eq!(scope.layers(), 0);
    /// scope.push_layer();
    /// assert_eq!(scope.layers(), 1);
    /// ```
    #[must_use]
    pub fn layers(&self) -> usize {
        self.ctx
            .outputs
            .layer_count()
            .saturating_sub(self.first_layer)
    }

    /// Keeps, for each output type, only the entries in that type's newest
    /// `retention` layers of the region, and removes the older ones.
    ///
    /// `default_retention` applies to every type that `per_type` does not
    /// name; a `per_type` value wins for its type. A retention of zero
    /// removes every entry of that type from the region. Layers below the
    /// region are never touched.
    ///
    /// A loop calls this just before it pushes the layer for its next
    /// iteration, so `retention` is the number of completed iterations that
    /// stay readable beside the new one.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::param::SystemContext;
    /// use std::any::TypeId;
    ///
    /// let mut ctx = SystemContext::new();
    /// let mut scope = ctx.output_layer_scope();
    /// for iteration in 0..3_u32 {
    ///     // Keep one earlier `u32`, but no earlier `String`.
    ///     scope.retain_layers(1, [(TypeId::of::<String>(), 0)]);
    ///     scope.push_layer();
    ///     scope.insert_output(iteration);
    ///     scope.insert_output(format!("note {iteration}"));
    /// }
    /// assert_eq!(scope.outputs().entries::<u32>().unwrap().len(), 2);
    /// assert_eq!(scope.outputs().entries::<String>().unwrap().len(), 1);
    /// ```
    pub fn retain_layers<I>(&mut self, default_retention: usize, per_type: I)
    where
        I: IntoIterator<Item = (TypeId, usize)>,
    {
        self.ctx
            .outputs
            .retain_layers_from(self.first_layer, default_retention, per_type);
    }

    /// Closes the region by folding its newest layer into the layer below
    /// the region and dropping every older layer of the region.
    ///
    /// `Some(allowed)` folds only entries whose type is in `allowed`; `None`
    /// folds every entry. A folded entry replaces an entry of the same type
    /// from an equal [`ProducerKey`](crate::resource::ProducerKey) in the
    /// layer below. An empty region (no layer pushed) changes nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::param::SystemContext;
    /// use std::any::TypeId;
    /// use std::collections::HashSet;
    ///
    /// let mut ctx = SystemContext::new();
    /// let mut scope = ctx.output_layer_scope();
    /// scope.push_layer();
    /// scope.insert_output(7_u32);
    /// scope.insert_output("interior detail".to_owned());
    ///
    /// let allowed: HashSet<TypeId> = [TypeId::of::<u32>()].into_iter().collect();
    /// scope.promote_newest(Some(&allowed));
    ///
    /// assert_eq!(*ctx.get_output::<u32>().unwrap(), 7);
    /// assert!(!ctx.contains_output::<String>());
    /// ```
    pub fn promote_newest(mut self, allowed: Option<&HashSet<TypeId>>) {
        self.ctx
            .outputs
            .promote_newest_layer_from(self.first_layer, allowed);
        self.open = false;
    }

    /// Closes the region by discarding every layer in it. This is what
    /// dropping the scope does; calling it makes the intent explicit.
    pub fn discard(self) {}
}

impl<'p> Deref for OutputLayerScope<'_, 'p> {
    type Target = SystemContext<'p>;

    fn deref(&self) -> &Self::Target {
        self.ctx
    }
}

impl<'p> DerefMut for OutputLayerScope<'_, 'p> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx
    }
}

impl Drop for OutputLayerScope<'_, '_> {
    fn drop(&mut self) {
        if self.open {
            self.ctx.outputs.discard_layers_from(self.first_layer);
        }
    }
}

impl std::fmt::Debug for OutputLayerScope<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputLayerScope")
            .field("first_layer", &self.first_layer)
            .field("layers", &self.layers())
            .field("open", &self.open)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_scope_discards_every_layer_it_pushed() {
        let mut ctx = SystemContext::new();
        ctx.insert_output(1_i32);
        {
            let mut scope = ctx.output_layer_scope();
            scope.push_layer();
            scope.insert_output(2_i32);
            scope.push_layer();
            scope.insert_output(3_i32);
            assert_eq!(scope.layers(), 2);
        }
        assert_eq!(ctx.outputs().layer_count(), 1);
        assert_eq!(*ctx.get_output::<i32>().unwrap(), 1);
    }

    #[test]
    fn promote_newest_keeps_only_the_newest_layer() {
        let mut ctx = SystemContext::new();
        let mut scope = ctx.output_layer_scope();
        for value in 0..3_i32 {
            scope.push_layer();
            scope.insert_output(value);
        }
        scope.promote_newest(None);
        assert_eq!(ctx.outputs().layer_count(), 1);
        let values: Vec<i32> = ctx
            .outputs()
            .entries::<i32>()
            .unwrap()
            .iter()
            .map(|(_, value)| **value)
            .collect();
        assert_eq!(values, vec![2]);
    }

    #[test]
    fn nested_scopes_close_in_order() {
        let mut ctx = SystemContext::new();
        let mut outer = ctx.output_layer_scope();
        outer.push_layer();
        outer.insert_output(1_i32);
        {
            let mut inner = outer.output_layer_scope();
            inner.push_layer();
            inner.insert_output("inner".to_owned());
            // Dropped without promoting.
        }
        assert_eq!(
            outer.layers(),
            1,
            "the inner region is gone, the outer one stays"
        );
        assert!(!outer.contains_output::<String>());
        outer.promote_newest(None);
        assert_eq!(*ctx.get_output::<i32>().unwrap(), 1);
    }

    #[test]
    fn a_scope_whose_outputs_were_replaced_does_nothing() {
        let mut ctx = SystemContext::new();
        let mut scope = ctx.output_layer_scope();
        scope.push_layer();
        scope.push_layer();
        scope.insert_output(1_i32);
        let _taken = scope.take_outputs();
        assert_eq!(scope.layers(), 0);
        scope.retain_layers(0, []);
        scope.promote_newest(None);
        assert_eq!(ctx.outputs().layer_count(), 1);
        assert!(!ctx.contains_output::<i32>());
    }
}
