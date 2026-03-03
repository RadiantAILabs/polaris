//! Schedule identifiers for tick-based plugin updates.
//!
//! Schedules are the mechanism by which the executor notifies plugins of
//! lifecycle events. A schedule is identified by a marker type (any `'static`
//! type) wrapped in a [`ScheduleId`]. See [`ScheduleId`] for the layered
//! architecture and a full example.

use core::any::TypeId;
use variadics_please::all_tuples;

/// Identifier for a tick schedule, derived from a marker type.
///
/// A `ScheduleId` wraps a `TypeId` so that any `'static` type can serve as a
/// schedule marker. The tick system is split across layers:
///
/// - **Layer 1** (`polaris_system`) — provides the tick mechanism via
///   `ScheduleId` and [`Server::tick()`](crate::server::Server::tick).
/// - **Layer 2** (`polaris_graph`) — defines schedule marker types (e.g.
///   `OnGraphStart`, `OnSystemComplete`) and triggers ticks at the
///   appropriate points during execution.
/// - **Layer 3** (plugins) — declares interest in schedules via
///   [`Plugin::tick_schedules()`](super::Plugin::tick_schedules) and
///   responds in [`Plugin::update()`](super::Plugin::update).
///
/// # Example
///
/// ```
/// # use polaris_system::plugin::{Plugin, PluginId, Schedule, Version, ScheduleId};
/// # use polaris_system::server::Server;
/// // Layer 2 defines a schedule marker type
/// pub struct PostAgentRun;
/// impl Schedule for PostAgentRun {}
///
/// // Layer 3 plugin subscribes to it
/// # struct MetricsPlugin;
/// impl Plugin for MetricsPlugin {
///     const ID: &'static str = "metrics";
///     const VERSION: Version = Version::new(0, 1, 0);
///
///     fn build(&self, _server: &mut Server) {}
///
///     fn tick_schedules(&self) -> Vec<ScheduleId> {
///         vec![ScheduleId::of::<PostAgentRun>()]
///     }
///
///     fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
///         // called when the executor runs server.tick::<PostAgentRun>()
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScheduleId {
    type_id: TypeId,
    type_name: &'static str,
}

impl ScheduleId {
    /// Creates a `ScheduleId` for the given schedule marker type.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_system::plugin::{Schedule, ScheduleId};
    /// // Define a schedule marker type
    /// pub struct PostAgentRun;
    /// impl Schedule for PostAgentRun {}
    ///
    /// // Create an identifier for it
    /// let schedule = ScheduleId::of::<PostAgentRun>();
    /// ```
    #[must_use]
    pub fn of<S: Schedule + 'static>() -> Self {
        Self {
            type_id: TypeId::of::<S>(),
            type_name: core::any::type_name::<S>(),
        }
    }

    /// Returns the underlying `TypeId`.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Returns the type name for debugging.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Schedule Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Marker trait for schedule types.
///
/// Layer 2 implements this trait for schedule markers. The trait is intentionally
/// minimal (a pure marker) to keep Layer 1 generic. Event types are defined and
/// associated with schedules at Layer 2.
pub trait Schedule: 'static {
    /// Returns the [`ScheduleId`] for this schedule type.
    fn schedule_id() -> ScheduleId
    where
        Self: Sized,
    {
        ScheduleId::of::<Self>()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IntoScheduleIds Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for types that can be converted into a list of schedule IDs.
///
/// Implemented for single schedules and tuples of schedules, enabling flexible
/// registration patterns in Layer 2.
pub trait IntoScheduleIds {
    /// Returns the schedule IDs for this type.
    fn schedule_ids() -> Vec<ScheduleId>;
}

/// Single schedule implements `IntoScheduleIds`.
impl<S: Schedule> IntoScheduleIds for S {
    fn schedule_ids() -> Vec<ScheduleId> {
        vec![ScheduleId::of::<S>()]
    }
}

/// Macro to implement `IntoScheduleIds` for tuples of schedules.
macro_rules! impl_into_schedule_ids_for_tuple {
    ($($S:ident),*) => {
        impl<$($S: Schedule),*> IntoScheduleIds for ($($S,)*) {
            fn schedule_ids() -> Vec<ScheduleId> {
                vec![$(ScheduleId::of::<$S>()),*]
            }
        }
    };
}

// Generate implementations for tuples from 2 to 16 elements
all_tuples!(impl_into_schedule_ids_for_tuple, 2, 16, S);

#[cfg(test)]
mod tests {
    use super::*;

    struct ScheduleA;
    impl Schedule for ScheduleA {}

    struct ScheduleB;
    impl Schedule for ScheduleB {}

    struct ScheduleC;
    impl Schedule for ScheduleC {}

    #[test]
    fn schedule_id_equality() {
        let id1 = ScheduleId::of::<ScheduleA>();
        let id2 = ScheduleId::of::<ScheduleA>();
        let id3 = ScheduleId::of::<ScheduleB>();

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn schedule_id_type_name() {
        let id = ScheduleId::of::<ScheduleA>();
        assert!(id.type_name().contains("ScheduleA"));
    }

    #[test]
    fn schedule_id_method() {
        assert_eq!(ScheduleA::schedule_id(), ScheduleId::of::<ScheduleA>());
        assert_eq!(ScheduleB::schedule_id(), ScheduleId::of::<ScheduleB>());
        assert_ne!(ScheduleA::schedule_id(), ScheduleB::schedule_id());
    }

    #[test]
    fn into_schedule_ids_single() {
        let ids = ScheduleA::schedule_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], ScheduleId::of::<ScheduleA>());
    }

    #[test]
    fn into_schedule_ids_tuple() {
        let ids = <(ScheduleA, ScheduleB, ScheduleC)>::schedule_ids();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], ScheduleId::of::<ScheduleA>());
        assert_eq!(ids[1], ScheduleId::of::<ScheduleB>());
        assert_eq!(ids[2], ScheduleId::of::<ScheduleC>());
    }
}
