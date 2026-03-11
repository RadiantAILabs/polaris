//! Schedule markers for graph execution lifecycle events.
//!
//! These marker types identify when hooks are invoked during graph execution.
//! Use them with [`ScheduleId::of::<T>()`](polaris_system::plugin::ScheduleId::of)
//! for registration, or use the type-safe registration methods like
//! [`register_observer::<OnSystemStart>`](super::HooksAPI::register_observer).
//!
//! # Pure Markers
//!
//! Schedule markers are pure marker types implementing the [`Schedule`] trait.
//! Event data is provided via the unified [`GraphEvent`](super::events::GraphEvent)
//! enum, which all hooks receive.

use polaris_system::plugin::Schedule;

// ─────────────────────────────────────────────────────────────────────────────
// System Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called before a system starts execution.
///
/// # Validation
///
/// Resources provided by `OnSystemStart` hooks are considered during validation,
/// as they are guaranteed to be available before the system runs.
///
/// Event data: [`GraphEvent::SystemStart`](super::events::GraphEvent::SystemStart)
pub struct OnSystemStart;
impl Schedule for OnSystemStart {}

/// Marker type for hooks called after a system completes successfully.
///
/// # Validation
///
/// Resources provided by `OnSystemComplete` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::SystemComplete`](super::events::GraphEvent::SystemComplete)
pub struct OnSystemComplete;
impl Schedule for OnSystemComplete {}

/// Marker type for hooks called when a system fails.
///
/// # Validation
///
/// Resources provided by `OnSystemError` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::SystemError`](super::events::GraphEvent::SystemError)
pub struct OnSystemError;
impl Schedule for OnSystemError {}

// ─────────────────────────────────────────────────────────────────────────────
// Decision Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called before a decision node evaluates its predicate.
///
/// This hook fires before the decision node evaluates its predicate, allowing you
/// to inspect the input data or context before the branch is selected. Use this
/// for logging, metrics, or resource injection that should occur regardless of
/// which branch is taken.
///
/// # Validation
///
/// Resources provided by `OnDecisionStart` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::DecisionStart`](super::events::GraphEvent::DecisionStart)
pub struct OnDecisionStart;
impl Schedule for OnDecisionStart {}

/// Marker type for hooks called after a decision branch is selected and executed.
///
/// # Validation
///
/// Resources provided by `OnDecisionComplete` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::DecisionComplete`](super::events::GraphEvent::DecisionComplete)
pub struct OnDecisionComplete;
impl Schedule for OnDecisionComplete {}

// ─────────────────────────────────────────────────────────────────────────────
// Switch Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called before a switch node evaluates its discriminator.
///
/// # Validation
///
/// Resources provided by `OnSwitchStart` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::SwitchStart`](super::events::GraphEvent::SwitchStart)
pub struct OnSwitchStart;
impl Schedule for OnSwitchStart {}

/// Marker type for hooks called after a switch case is selected and executed.
///
/// # Validation
///
/// Resources provided by `OnSwitchComplete` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::SwitchComplete`](super::events::GraphEvent::SwitchComplete)
pub struct OnSwitchComplete;
impl Schedule for OnSwitchComplete {}

// ─────────────────────────────────────────────────────────────────────────────
// Loop Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called before a loop begins execution.
///
/// # Validation
///
/// Resources provided by `OnLoopStart` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::LoopStart`](super::events::GraphEvent::LoopStart)
pub struct OnLoopStart;
impl Schedule for OnLoopStart {}

/// Marker type for hooks called at the start of each loop iteration.
///
/// # Validation
///
/// Resources provided by `OnLoopIteration` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::LoopIteration`](super::events::GraphEvent::LoopIteration)
pub struct OnLoopIteration;
impl Schedule for OnLoopIteration {}

/// Marker type for hooks called after a loop completes all iterations.
///
/// # Validation
///
/// Resources provided by `OnLoopEnd` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::LoopEnd`](super::events::GraphEvent::LoopEnd)
pub struct OnLoopEnd;
impl Schedule for OnLoopEnd {}

// ─────────────────────────────────────────────────────────────────────────────
// Parallel Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called at the start of parallel execution.
///
/// # Validation
///
/// Resources provided by `OnParallelStart` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::ParallelStart`](super::events::GraphEvent::ParallelStart)
pub struct OnParallelStart;
impl Schedule for OnParallelStart {}

/// Marker type for hooks called after all parallel branches complete.
///
/// # Validation
///
/// Resources provided by `OnParallelComplete` hooks are **not** considered during
/// validation.
///
/// Event data: [`GraphEvent::ParallelComplete`](super::events::GraphEvent::ParallelComplete)
pub struct OnParallelComplete;
impl Schedule for OnParallelComplete {}

// ─────────────────────────────────────────────────────────────────────────────
// Graph-Level Schedules
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for hooks called before graph execution begins.
///
/// This hook fires once at the start of graph execution, before any systems
/// are run. Providers registered on this schedule make resources available
/// to all systems in the graph.
///
/// # Validation
///
/// Resources provided by `OnGraphStart` hooks are considered during validation,
/// as they are guaranteed to be available before any system executes.
///
/// Event data: [`GraphEvent::GraphStart`](super::events::GraphEvent::GraphStart)
pub struct OnGraphStart;
impl Schedule for OnGraphStart {}

/// Marker type for hooks called after graph execution completes.
///
/// This hook fires once at the end of graph execution if the graph completes successfully. Use this
/// for final reporting, cleanup, or post-processing actions. Use `OnGraphFailure` for error handling instead.
///
/// Event data: [`GraphEvent::GraphComplete`](super::events::GraphEvent::GraphComplete)
///
/// # Validation
///
/// Resources provided by `OnGraphComplete` hooks are **not** considered during
/// validation, as they arrive after system execution.
pub struct OnGraphComplete;
impl Schedule for OnGraphComplete {}

/// Marker type for hooks called when graph execution fails with an error.
///
/// This hook fires once at the end of graph execution if an error occurs. Use this
/// for error reporting, cleanup, or compensating actions.
///
/// Event data: [`GraphEvent::GraphFailure`](super::events::GraphEvent::GraphFailure)
pub struct OnGraphFailure;
impl Schedule for OnGraphFailure {}

// ─────────────────────────────────────────────────────────────────────────────
// Composite Schedule Type
// ─────────────────────────────────────────────────────────────────────────────

/// Composite type covering all graph execution schedules.
///
/// This type can be used with
/// [`IntoScheduleIds`](polaris_system::plugin::IntoScheduleIds) to register a
/// hook on every graph lifecycle event at once.
///
/// # Example
///
/// ```
/// # use polaris_graph::hooks::schedule::AllGraphSchedules;
/// # use polaris_graph::hooks::GraphEvent;
/// # use polaris_graph::hooks::HooksAPI;
/// # let hooks = HooksAPI::default();
///
/// hooks.register_observer::<AllGraphSchedules, _>(
///     "trace_all",
///     |event: &GraphEvent| {
///         tracing::debug!("{event}");
///     },
/// )?;
/// # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
/// ```
pub type AllGraphSchedules = (
    OnGraphStart,
    OnGraphComplete,
    OnGraphFailure,
    OnSystemStart,
    OnSystemComplete,
    OnSystemError,
    OnDecisionStart,
    OnDecisionComplete,
    OnSwitchStart,
    OnSwitchComplete,
    OnLoopStart,
    OnLoopIteration,
    OnLoopEnd,
    OnParallelStart,
    OnParallelComplete,
);
