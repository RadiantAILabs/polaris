//! Access descriptors for borrow checking.
//!
//! This module provides types for describing resource access patterns,
//! enabling compile-time and runtime conflict detection between systems.
//!
//! # Overview
//!
//! Each [`SystemParam`](super::SystemParam) declares what resources it accesses and how:
//! - `Res<T>` declares read access to resources
//! - `ResMut<T>` declares write access to resources
//! - `Out<T>` declares read access to outputs (previous system's return value)
//!
//! Systems aggregate their parameter accesses into a [`SystemAccess`] descriptor,
//! which can be checked for conflicts with other systems.
//!
//! # Conflict Rules
//!
//! Within the same context:
//! - Read + Read: OK (multiple readers allowed)
//! - Read + Write: CONFLICT
//! - Write + Write: CONFLICT
//!
//! Systems in different contexts (different agents) never conflict on
//! [`LocalResource`](crate::resource::LocalResource), because each context has its own instance.

use std::any::{TypeId, type_name};

/// The mode of access to a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessMode {
    /// Read-only access (e.g., `Res<T>`).
    Read,
    /// Read-write access (e.g., `ResMut<T>`).
    Write,
}

/// Describes access to a single resource type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The type ID of the resource being accessed.
    pub type_id: TypeId,
    /// The human-readable name of the type (for error messages).
    pub type_name: &'static str,
    /// The mode of access (read or write).
    pub mode: AccessMode,
    /// Whether this is a global resource.
    ///
    /// Global resources are read-only across all contexts, so they
    /// only conflict within the same context for write access.
    pub is_global: bool,
}

impl Access {
    /// Creates a new read access descriptor.
    #[must_use]
    pub fn read<T: 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            mode: AccessMode::Read,
            is_global: false,
        }
    }

    /// Creates a new write access descriptor.
    #[must_use]
    pub fn write<T: 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            mode: AccessMode::Write,
            is_global: false,
        }
    }

    /// Marks this access as global.
    #[must_use]
    pub fn global(mut self) -> Self {
        self.is_global = true;
        self
    }
}

/// One output type read through `Out<T>` or `Option<Out<T>>` — the only
/// forms a graph-level `read_from` binding can redirect to a chosen
/// producer.
///
/// `OutAll<T>` (reads every producer) and `ErrOut<T>` (always reads the
/// newest `CaughtError`, ignoring any installed binding) do not add an
/// entry here even though they also read `outputs` — a `read_from` binding
/// naming a type reachable only through one of them would have no effect,
/// so the graph layer rejects it the same way it rejects a typo'd type
/// (`ValidationError::ReadFromUnread` in `polaris_graph`).
///
/// # Examples
///
/// ```
/// use polaris_system::param::{Out, OutAll, SystemParam};
/// use std::any::TypeId;
///
/// struct Plan;
///
/// let bindable = <Out<Plan> as SystemParam>::access().bindable_outputs;
/// assert_eq!(bindable.len(), 1);
/// assert_eq!(bindable[0].type_id, TypeId::of::<Plan>());
/// assert!(!bindable[0].optional);
///
/// // `OutAll<T>` reads the same channel but cannot be bound.
/// assert!(<OutAll<Plan> as SystemParam>::access().bindable_outputs.is_empty());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindableOutput {
    /// The type ID of the output being read.
    pub type_id: TypeId,
    /// The human-readable name of the type (for error messages).
    pub type_name: &'static str,
    /// Whether the read tolerates a missing entry (`Option<Out<T>>`) rather
    /// than requiring one (`Out<T>`).
    ///
    /// Only `read_from` backward-wire validation consults this flag —
    /// existing resource/output conflict detection is unaffected and still
    /// treats an optional read the same as a required one.
    pub optional: bool,
}

/// Aggregated access patterns for a system or parameter.
///
/// Used to detect conflicts between systems that would violate
/// Rust's borrowing rules at the resource level.
///
/// `PartialEq`/`Eq` are implemented by hand (below) to compare only
/// `resources`, `outputs` and `context_requirements` — the fields that
/// describe scheduling/conflict behavior, which is what "two systems have
/// the same access" has always meant here. `bindable_outputs` is
/// `read_from`-validation metadata layered on top of an existing `outputs`
/// read, not an independent access, so two descriptors that read the same
/// outputs through different bindable-ness (`Out<T>` vs `OutAll<T>`, say)
/// still compare equal.
#[derive(Debug, Clone, Default)]
pub struct SystemAccess {
    /// Resources accessed by this system/parameter.
    pub resources: Vec<Access>,
    /// Outputs accessed by this system/parameter.
    pub outputs: Vec<Access>,
    /// Execution context tags this system requires.
    ///
    /// Parameters can declare opaque string tags describing the execution
    /// context they need (e.g. [`ERROR_CONTEXT`](super::ERROR_CONTEXT)).
    pub context_requirements: Vec<&'static str>,
    /// Output types read through `Out<T>` or `Option<Out<T>>`, for
    /// `read_from` binding validation. A subset of `outputs`: every entry
    /// here has a matching `outputs` entry, but not every `outputs` entry
    /// has a match here (`OutAll<T>`, `ErrOut<T>`).
    pub bindable_outputs: Vec<BindableOutput>,
}

impl PartialEq for SystemAccess {
    fn eq(&self, other: &Self) -> bool {
        self.resources == other.resources
            && self.outputs == other.outputs
            && self.context_requirements == other.context_requirements
    }
}

impl Eq for SystemAccess {}

impl SystemAccess {
    /// Creates an empty access descriptor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a resource read access.
    pub fn add_read<T: 'static>(&mut self) {
        self.resources.push(Access::read::<T>());
    }

    /// Adds a resource write access.
    pub fn add_write<T: 'static>(&mut self) {
        self.resources.push(Access::write::<T>());
    }

    /// Adds an output write access.
    pub fn add_output<T: 'static>(&mut self) {
        self.outputs.push(Access::write::<T>());
    }

    /// Records a `read_from`-bindable output read (`Out<T>` or
    /// `Option<Out<T>>`). Callers also push the matching `outputs` read
    /// access separately — this only affects `read_from` validation.
    ///
    /// A hand-written [`SystemParam`](super::SystemParam) that resolves its
    /// value through `Out<T>` calls this so a graph can wire it with
    /// `read_from`.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_system::param::{Access, SystemAccess};
    ///
    /// struct Plan;
    ///
    /// let mut access = SystemAccess::new();
    /// access.outputs.push(Access::read::<Plan>());
    /// access.add_bindable_output::<Plan>(true);
    ///
    /// assert_eq!(access.bindable_outputs.len(), 1);
    /// assert!(access.bindable_outputs[0].optional);
    /// ```
    pub fn add_bindable_output<T: 'static>(&mut self, optional: bool) {
        self.bindable_outputs.push(BindableOutput {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            optional,
        });
    }

    /// Creates access for a read-only resource.
    #[must_use]
    pub fn with_read<T: 'static>(mut self) -> Self {
        self.add_read::<T>();
        self
    }

    /// Creates access for a read-write resource.
    #[must_use]
    pub fn with_write<T: 'static>(mut self) -> Self {
        self.add_write::<T>();
        self
    }

    /// Creates access for an output.
    #[must_use]
    pub fn with_output<T: 'static>(mut self) -> Self {
        self.add_output::<T>();
        self
    }

    /// Adds a context requirement tag.
    pub fn require_context(&mut self, tag: &'static str) {
        if !self.context_requirements.contains(&tag) {
            self.context_requirements.push(tag);
        }
    }

    /// Merges another access descriptor into this one.
    ///
    /// This is used to aggregate access from multiple parameters.
    pub fn merge(&mut self, other: &SystemAccess) {
        self.resources.extend(other.resources.iter().cloned());
        self.outputs.extend(other.outputs.iter().cloned());
        self.bindable_outputs
            .extend(other.bindable_outputs.iter().copied());
        for &tag in &other.context_requirements {
            self.require_context(tag);
        }
    }

    /// Creates a new access descriptor by merging two descriptors.
    #[must_use]
    pub fn merged(mut self, other: &SystemAccess) -> Self {
        self.merge(other);
        self
    }

    /// Checks if this access conflicts with another within the same context.
    ///
    /// Two accesses conflict if they access the same resource and at least
    /// one of them is a write access.
    ///
    /// # Returns
    ///
    /// `true` if there is a conflict, `false` otherwise.
    #[must_use]
    pub fn conflicts_with(&self, other: &SystemAccess) -> bool {
        // Check resource conflicts
        for a in &self.resources {
            for b in &other.resources {
                if a.type_id == b.type_id {
                    // Read/Read is OK, anything else is a conflict
                    if a.mode == AccessMode::Write || b.mode == AccessMode::Write {
                        return true;
                    }
                }
            }
        }

        // Check output conflicts (outputs are always write)
        for a in &self.outputs {
            for b in &other.outputs {
                if a.type_id == b.type_id {
                    return true;
                }
            }
        }

        false
    }

    /// Returns a list of conflicting resource types with another access.
    ///
    /// Useful for generating detailed error messages.
    #[must_use]
    pub fn find_conflicts(&self, other: &SystemAccess) -> Vec<&'static str> {
        let mut conflicts = Vec::new();

        for a in &self.resources {
            for b in &other.resources {
                if a.type_id == b.type_id
                    && (a.mode == AccessMode::Write || b.mode == AccessMode::Write)
                {
                    conflicts.push(a.type_name);
                }
            }
        }

        for a in &self.outputs {
            for b in &other.outputs {
                if a.type_id == b.type_id {
                    conflicts.push(a.type_name);
                }
            }
        }

        conflicts
    }

    /// Returns `true` if this descriptor has no accesses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.outputs.is_empty() && self.context_requirements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter {
        _value: i32,
    }

    struct Config {
        _name: String,
    }

    struct Output {
        _data: Vec<u8>,
    }

    #[test]
    fn read_read_no_conflict() {
        let a = SystemAccess::new().with_read::<Counter>();
        let b = SystemAccess::new().with_read::<Counter>();
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn read_write_conflicts() {
        let a = SystemAccess::new().with_read::<Counter>();
        let b = SystemAccess::new().with_write::<Counter>();
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn write_read_conflicts() {
        let a = SystemAccess::new().with_write::<Counter>();
        let b = SystemAccess::new().with_read::<Counter>();
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn write_write_conflicts() {
        let a = SystemAccess::new().with_write::<Counter>();
        let b = SystemAccess::new().with_write::<Counter>();
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn different_types_no_conflict() {
        let a = SystemAccess::new().with_write::<Counter>();
        let b = SystemAccess::new().with_write::<Config>();
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn output_conflicts() {
        let a = SystemAccess::new().with_output::<Output>();
        let b = SystemAccess::new().with_output::<Output>();
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn output_different_types_no_conflict() {
        let a = SystemAccess::new().with_output::<Output>();
        let b = SystemAccess::new().with_output::<Counter>();
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn merge_combines_accesses() {
        let a = SystemAccess::new().with_read::<Counter>();
        let b = SystemAccess::new().with_write::<Config>();

        let merged = a.merged(&b);
        assert_eq!(merged.resources.len(), 2);
    }

    #[test]
    fn find_conflicts_returns_type_names() {
        let a = SystemAccess::new()
            .with_read::<Counter>()
            .with_write::<Config>();
        let b = SystemAccess::new().with_write::<Counter>();

        let conflicts = a.find_conflicts(&b);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("Counter"));
    }

    #[test]
    fn is_empty_works() {
        let empty = SystemAccess::new();
        assert!(empty.is_empty());

        let non_empty = SystemAccess::new().with_read::<Counter>();
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn access_global_flag() {
        let local = Access::read::<Counter>();
        assert!(!local.is_global);

        let global = Access::read::<Counter>().global();
        assert!(global.is_global);

        // Write access can also be global
        let global_write = Access::write::<Counter>().global();
        assert!(global_write.is_global);
        assert_eq!(global_write.mode, AccessMode::Write);
    }

    #[test]
    fn access_read_write_factory_methods() {
        let read = Access::read::<Counter>();
        assert_eq!(read.mode, AccessMode::Read);
        assert_eq!(read.type_id, TypeId::of::<Counter>());
        assert!(read.type_name.contains("Counter"));
        assert!(!read.is_global);

        let write = Access::write::<Config>();
        assert_eq!(write.mode, AccessMode::Write);
        assert_eq!(write.type_id, TypeId::of::<Config>());
        assert!(write.type_name.contains("Config"));
        assert!(!write.is_global);
    }

    #[test]
    fn system_access_add_read_write_methods() {
        let mut access = SystemAccess::new();
        assert!(access.resources.is_empty());

        access.add_read::<Counter>();
        assert_eq!(access.resources.len(), 1);
        assert_eq!(access.resources[0].mode, AccessMode::Read);

        access.add_write::<Config>();
        assert_eq!(access.resources.len(), 2);
        assert_eq!(access.resources[1].mode, AccessMode::Write);

        access.add_output::<Output>();
        assert_eq!(access.outputs.len(), 1);
        assert_eq!(access.outputs[0].mode, AccessMode::Write);
    }

    #[test]
    fn add_bindable_output_records_type_and_optionality() {
        let mut access = SystemAccess::new();
        assert!(access.bindable_outputs.is_empty());

        access.add_bindable_output::<Counter>(false);
        assert_eq!(access.bindable_outputs.len(), 1);
        assert_eq!(access.bindable_outputs[0].type_id, TypeId::of::<Counter>());
        assert!(!access.bindable_outputs[0].optional);

        access.add_bindable_output::<Config>(true);
        assert_eq!(access.bindable_outputs.len(), 2);
        assert!(access.bindable_outputs[1].optional);
    }

    #[test]
    fn merge_combines_bindable_outputs() {
        let mut a = SystemAccess::new();
        a.add_bindable_output::<Counter>(false);
        let mut b = SystemAccess::new();
        b.add_bindable_output::<Config>(true);

        a.merge(&b);
        assert_eq!(a.bindable_outputs.len(), 2);
        assert!(a.bindable_outputs.iter().any(|bound| bound.optional));
    }

    #[test]
    fn mixed_resource_and_output_no_cross_conflict() {
        // Resource and output of same type should NOT conflict
        // (they are in different collections)
        let a = SystemAccess::new().with_write::<Counter>();
        let b = SystemAccess::new().with_output::<Counter>();

        // These should not conflict because one is a resource, one is an output
        assert!(!a.conflicts_with(&b));
    }

    #[test]
    fn three_resource_conflict_scenario() {
        // System A reads Counter, writes Config
        let a = SystemAccess::new()
            .with_read::<Counter>()
            .with_write::<Config>();

        // System B writes Counter, reads Config, writes Output
        let b = SystemAccess::new()
            .with_write::<Counter>()
            .with_read::<Config>()
            .with_output::<Output>();

        // Should conflict on both Counter (read vs write) and Config (write vs read)
        assert!(a.conflicts_with(&b));

        let conflicts = a.find_conflicts(&b);
        assert_eq!(conflicts.len(), 2);
    }

    #[test]
    fn find_conflicts_returns_all_conflicting_types() {
        let a = SystemAccess::new()
            .with_write::<Counter>()
            .with_write::<Config>()
            .with_output::<Output>();

        let b = SystemAccess::new()
            .with_read::<Counter>()
            .with_write::<Config>()
            .with_output::<Output>();

        let conflicts = a.find_conflicts(&b);
        // Counter: write vs read = conflict
        // Config: write vs write = conflict
        // Output: both have it = conflict
        assert_eq!(conflicts.len(), 3);

        // Verify all types are included
        let conflict_str = conflicts.join(" ");
        assert!(conflict_str.contains("Counter"));
        assert!(conflict_str.contains("Config"));
        assert!(conflict_str.contains("Output"));
    }

    #[test]
    fn global_flag_preserved_in_access() {
        let global_read = Access::read::<Counter>().global();
        let global_write = Access::write::<Counter>().global();

        // Global flag should be preserved
        assert!(global_read.is_global);
        assert!(global_write.is_global);

        // Mode should still be correct
        assert_eq!(global_read.mode, AccessMode::Read);
        assert_eq!(global_write.mode, AccessMode::Write);

        // TypeId should be unchanged
        assert_eq!(global_read.type_id, TypeId::of::<Counter>());
    }
}
