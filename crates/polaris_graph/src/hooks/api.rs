//! Hook registration API for graph execution.
//!
//! The [`HooksAPI`] provides a registry for plugins to register lifecycle hooks
//! that are invoked during graph execution.
//!
//! # Observer vs Provider Pattern
//!
//! - **Observers** ([`register_observer`](HooksAPI::register_observer)): React to events
//!   without providing resources. Use for logging, metrics, tracing.
//! - **Providers** ([`register_provider`](HooksAPI::register_provider)): Produce resources
//!   that are inserted into the context. Use for injecting execution metadata.
//!
//! # Multi-Schedule Registration
//!
//! Register hooks on multiple schedules using tuple syntax:
//!
//! ```
//! # use polaris_graph::hooks::{HooksAPI, GraphEvent};
//! # use polaris_graph::hooks::schedule::{OnSystemStart, OnSystemComplete, OnSystemError};
//! # let hooks = HooksAPI::new();
//! hooks.register_observer::<(OnSystemStart, OnSystemComplete, OnSystemError), _>(
//!     "tracker",
//!     |event: &GraphEvent| match event {
//!         GraphEvent::SystemStart { node_name, .. } => println!("Start: {}", node_name),
//!         GraphEvent::SystemComplete { duration, .. } => println!("Done: {:?}", duration),
//!         GraphEvent::SystemError { error, .. } => println!("Error: {}", error),
//!         _ => {}
//!     },
//! )?;
//! # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
//! ```
//!
//! # Example: Observer
//!
//! ```
//! # use polaris_graph::hooks::{HooksAPI, GraphEvent};
//! # use polaris_graph::hooks::schedule::OnSystemStart;
//! # let hooks = HooksAPI::new();
//! hooks.register_observer::<OnSystemStart, _>("logger", |event: &GraphEvent| {
//!     if let GraphEvent::SystemStart { node_name, .. } = event {
//!         println!("System {} starting", node_name);
//!     }
//! })?;
//! # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
//! ```
//!
//! # Example: Provider
//!
//! ```
//! # use polaris_graph::hooks::{HooksAPI, GraphEvent};
//! # use polaris_graph::hooks::schedule::OnSystemStart;
//! # use polaris_graph::dev::SystemInfo;
//! # let hooks = HooksAPI::new();
//! hooks.register_provider::<OnSystemStart, SystemInfo, _>("devtools", |event: &GraphEvent| {
//!     match event {
//!         GraphEvent::SystemStart { node_id, node_name } => {
//!             Some(SystemInfo::new(node_id.clone(), node_name))
//!         }
//!         _ => None,
//!     }
//! })?;
//! # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
//! ```

use super::events::GraphEvent;
use hashbrown::HashMap;
use parking_lot::RwLock;
use polaris_system::api::API;
use polaris_system::param::SystemContext;
use polaris_system::plugin::{IntoScheduleIds, ScheduleId};
use polaris_system::resource::LocalResource;
use std::any::TypeId;
use std::fmt;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// BoxedHook
// ─────────────────────────────────────────────────────────────────────────────

/// Type-erased hook that receives [`GraphEvent`] directly.
///
/// This is a lower-level type used internally by [`HooksAPI`]. Most users should
/// use [`HooksAPI::register_observer`] or [`HooksAPI::register_provider`] instead.
///
/// # Fields
///
/// - `handler`: The hook function that receives [`SystemContext`] and [`GraphEvent`]
/// - `provided_resources`: Type IDs of resources this hook injects (empty for observers)
pub struct BoxedHook {
    /// The hook function that receives context and event.
    /// With the current implementation, the hooks don't actually
    /// need to receive the context, but we include it here for future flexibility.
    pub(crate) handler: Box<dyn Fn(&mut SystemContext<'_>, &GraphEvent) + Send + Sync>,
    /// Type IDs of resources this hook provides (empty for observers).
    pub(crate) provided_resources: Vec<TypeId>,
}

impl BoxedHook {
    /// Instantiates a new `BoxedHook` with the given handler and provided resources.
    ///
    /// The returned hook must be registered via [`HooksAPI::register_boxed`] to take effect.
    #[must_use = "BoxedHook must be registered via HooksAPI::register_boxed to take effect"]
    pub fn new(
        handler: impl Fn(&mut SystemContext<'_>, &GraphEvent) + Send + Sync + 'static,
        provided_resources: Vec<TypeId>,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            provided_resources,
        }
    }

    /// Invokes the hook with the given context and event.
    pub fn invoke(&self, ctx: &mut SystemContext<'_>, event: &GraphEvent) {
        (self.handler)(ctx, event);
    }

    /// Returns the type IDs of resources this hook provides.
    ///
    /// For observer hooks, this returns an empty slice.
    /// For provider hooks, this returns the type IDs of injected resources.
    #[must_use]
    pub fn provided_resources(&self) -> &[TypeId] {
        &self.provided_resources
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HookRegistrationError
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during hook registration.
#[derive(Debug, Clone)]
pub enum HookRegistrationError {
    /// A hook with this name already exists on the schedule.
    DuplicateName {
        /// The schedule where the duplicate was found.
        schedule: ScheduleId,
        /// The duplicate hook name.
        name: String,
    },
}

impl fmt::Display for HookRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookRegistrationError::DuplicateName { schedule, name } => {
                write!(
                    f,
                    "hook '{}' already registered for schedule '{}'",
                    name,
                    schedule.type_name()
                )
            }
        }
    }
}

impl std::error::Error for HookRegistrationError {}

// ─────────────────────────────────────────────────────────────────────────────
// HookEntry
// ─────────────────────────────────────────────────────────────────────────────

/// Entry in the hook registry, containing metadata and the hook function.
struct HookEntry {
    /// Human-readable name for debugging and logging.
    name: String,
    /// The hook function with metadata.
    hook: BoxedHook,
}

// ─────────────────────────────────────────────────────────────────────────────
// HooksAPI
// ─────────────────────────────────────────────────────────────────────────────

/// API for registering and invoking graph execution hooks.
///
/// Plugins use this API to extend the graph executor with lifecycle callbacks.
/// Hooks are organized by schedule (event type).
///
/// # Thread Safety
///
/// The `HooksAPI` uses interior mutability via [`RwLock`] to allow concurrent
/// registration during the build phase and concurrent invocation during execution.
///
/// # Observer vs Provider
///
/// Use [`register_observer`](Self::register_observer) for hooks that only react to events.
/// Use [`register_provider`](Self::register_provider) for hooks that inject resources.
#[derive(Default)]
pub struct HooksAPI {
    /// Maps schedule ID to a list of hook entries.
    hooks: RwLock<HashMap<ScheduleId, Vec<HookEntry>>>,
}

impl API for HooksAPI {}

impl HooksAPI {
    /// Creates a new empty hooks registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: RwLock::new(HashMap::new()),
        }
    }

    /// Registers an observer hook for one or more schedules.
    ///
    /// Observers react to events but don't provide resources to the context.
    /// Use this for logging, metrics, tracing, and other side-effect operations.
    ///
    /// # Type Parameters
    ///
    /// * `S` - Schedule marker type(s). Can be a single schedule or a tuple of schedules.
    /// * `F` - The hook function type (inferred)
    ///
    /// # Errors
    ///
    /// Returns [`HookRegistrationError::DuplicateName`] if a hook with the same name
    /// is already registered on any of the target schedules.
    ///
    /// # Examples
    ///
    /// ```
    /// # use polaris_graph::hooks::{HooksAPI, GraphEvent};
    /// # use polaris_graph::hooks::schedule::{OnSystemStart, OnSystemComplete, OnSystemError};
    /// # let hooks = HooksAPI::new();
    /// // Single schedule
    /// hooks.register_observer::<OnSystemStart, _>("logger", |event: &GraphEvent| {
    ///     if let GraphEvent::SystemStart { node_name, .. } = event {
    ///         println!("System {} starting", node_name);
    ///     }
    /// })?;
    ///
    /// // Multiple schedules
    /// hooks.register_observer::<(OnSystemStart, OnSystemComplete, OnSystemError), _>(
    ///     "tracker",
    ///     |event: &GraphEvent| match event {
    ///         GraphEvent::SystemStart { node_name, .. } => println!("Start: {}", node_name),
    ///         GraphEvent::SystemComplete { duration, .. } => println!("Done: {:?}", duration),
    ///         GraphEvent::SystemError { error, .. } => println!("Error: {}", error),
    ///         _ => {}
    ///     },
    /// )?;
    /// # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
    /// ```
    pub fn register_observer<S, F>(
        &self,
        name: impl Into<String>,
        hook: F,
    ) -> Result<&Self, HookRegistrationError>
    where
        S: IntoScheduleIds,
        F: Fn(&GraphEvent) + Send + Sync + 'static,
    {
        let schedules = S::schedule_ids();
        let name = name.into();
        // Arc is used internally to allow multiple schedules to access the same hook
        let hook = Arc::new(hook);

        for schedule in &schedules {
            let hook_name = if schedules.len() > 1 {
                format!("{}@{}", name, schedule.type_name())
            } else {
                name.clone()
            };
            let hook_clone = Arc::clone(&hook);

            self.register_boxed(
                *schedule,
                hook_name,
                BoxedHook::new(
                    move |_ctx, event: &GraphEvent| {
                        hook_clone(event);
                    },
                    Vec::new(), // observers provide no resources
                ),
            )?;
        }
        Ok(self)
    }

    /// Registers a provider hook for one or more schedules.
    ///
    /// Providers produce resources that are inserted into the [`SystemContext`], making
    /// them available to systems via [`polaris_system::param::Res`] or [`polaris_system::param::ResMut`].
    /// The provided resource type is tracked for validation.
    ///
    /// If multiple providers on the same schedule produce the same resource type,
    /// the last registered provider wins (last-write-wins semantics).
    ///
    /// # Type Parameters
    ///
    /// * `S` - Schedule marker type(s). Can be a single schedule or a tuple of schedules.
    /// * `T` - The resource type to provide (must implement [`LocalResource`])
    /// * `F` - The hook function type (inferred)
    ///
    /// # Errors
    ///
    /// Returns [`HookRegistrationError::DuplicateName`] if a hook with the same name
    /// is already registered on any of the target schedules.
    ///
    /// # Example
    ///
    /// ```
    /// # use polaris_graph::hooks::{HooksAPI, GraphEvent};
    /// # use polaris_graph::hooks::schedule::OnSystemStart;
    /// # use polaris_graph::dev::SystemInfo;
    /// # let hooks = HooksAPI::new();
    /// hooks.register_provider::<OnSystemStart, SystemInfo, _>(
    ///     "devtools",
    ///     |event: &GraphEvent| {
    ///         match event {
    ///             GraphEvent::SystemStart { node_id, node_name } => {
    ///                 Some(SystemInfo::new(node_id.clone(), node_name))
    ///             }
    ///             _ => None,
    ///         }
    ///     },
    /// )?;
    /// # Ok::<(), polaris_graph::hooks::HookRegistrationError>(())
    /// ```
    pub fn register_provider<S, T, F>(
        &self,
        name: impl Into<String>,
        hook: F,
    ) -> Result<&Self, HookRegistrationError>
    where
        S: IntoScheduleIds,
        T: LocalResource,
        F: Fn(&GraphEvent) -> Option<T> + Send + Sync + 'static,
    {
        let schedules = S::schedule_ids();
        let name = name.into();
        let hook = Arc::new(hook);

        for schedule in &schedules {
            let hook_name = if schedules.len() > 1 {
                format!("{}@{}", name, schedule.type_name())
            } else {
                name.clone()
            };
            let hook_clone = Arc::clone(&hook);

            self.register_boxed(
                *schedule,
                hook_name,
                BoxedHook::new(
                    move |ctx, event: &GraphEvent| {
                        if let Some(resource) = hook_clone(event) {
                            ctx.insert(resource);
                        }
                    },
                    vec![TypeId::of::<T>()], // track provided resource type
                ),
            )?;
        }

        Ok(self)
    }

    /// Registers a pre-built [`BoxedHook`] for the given schedule.
    ///
    /// This is the lower-level registration method used internally by
    /// [`register_observer`](Self::register_observer) and
    /// [`register_provider`](Self::register_provider).
    ///
    /// Most users should use those higher-level methods instead.
    ///
    /// # Errors
    ///
    /// Returns [`HookRegistrationError::DuplicateName`] if a hook with the same name
    /// is already registered on the target schedule.
    pub fn register_boxed(
        &self,
        schedule: ScheduleId,
        name: impl Into<String>,
        hook: BoxedHook,
    ) -> Result<(), HookRegistrationError> {
        let name = name.into();

        let mut hooks = self.hooks.write();
        let entries = hooks.entry(schedule).or_default();

        // Check for duplicate names
        if entries.iter().any(|entry| entry.name == name) {
            return Err(HookRegistrationError::DuplicateName { schedule, name });
        }

        entries.push(HookEntry { name, hook });
        Ok(())
    }

    /// Invokes all hooks registered for the given schedule with the event data.
    ///
    /// Hooks execute in registration order. For provider hooks that insert the same
    /// resource type, last-write-wins semantics apply.
    ///
    /// If no hooks are registered for the schedule, this is a no-op.
    pub fn invoke(&self, schedule: ScheduleId, ctx: &mut SystemContext<'_>, event: &GraphEvent) {
        let hooks = self.hooks.read();

        if let Some(entries) = hooks.get(&schedule) {
            for entry in entries {
                entry.hook.invoke(ctx, event);
            }
        }
    }

    /// Returns the number of hooks registered for the given schedule.
    #[must_use]
    pub fn hook_count(&self, schedule: ScheduleId) -> usize {
        let hooks = self.hooks.read();
        hooks.get(&schedule).map_or(0, Vec::len)
    }

    /// Returns all resource types provided by hooks on the given schedule.
    #[must_use]
    pub fn provided_resources_for(&self, schedule: ScheduleId) -> Vec<TypeId> {
        let hooks = self.hooks.read();
        hooks
            .get(&schedule)
            .map(|entries| {
                entries
                    .iter()
                    .flat_map(|entry| entry.hook.provided_resources().iter().copied())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Checks if a hook with the given name exists on the schedule.
    #[must_use]
    pub fn contains_hook(&self, schedule: ScheduleId, name: &str) -> bool {
        let hooks = self.hooks.read();
        hooks
            .get(&schedule)
            .is_some_and(|entries| entries.iter().any(|entry| entry.name == name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::schedule::{OnSystemComplete, OnSystemStart};
    use crate::node::NodeId;
    use polaris_system::plugin::Schedule;
    use polaris_system::resource::LocalResource;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn hooks_api_register_increments_count() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();

        api.register_observer::<OnSystemStart, _>("test_hook", |_: &GraphEvent| {})
            .expect("registration should succeed");

        assert_eq!(api.hook_count(schedule), 1);

        api.register_observer::<OnSystemStart, _>("another_hook", |_: &GraphEvent| {})
            .expect("registration should succeed");

        assert_eq!(api.hook_count(schedule), 2);
    }

    #[test]
    fn hooks_api_invoke_calls_hooks() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        api.register_observer::<OnSystemStart, _>("counting_hook", move |_: &GraphEvent| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        })
        .expect("registration should succeed");

        let mut ctx = SystemContext::new();
        let event = GraphEvent::SystemStart {
            node_id: NodeId::new(),
            node_name: "test",
        };

        api.invoke(schedule, &mut ctx, &event);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        api.invoke(schedule, &mut ctx, &event);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn hooks_api_invoke_calls_all_hooks_in_order() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();
        let execution_order = Arc::new(Mutex::new(Vec::new()));

        for name in ["first", "second", "third"] {
            let order_clone = execution_order.clone();
            let name_owned = name.to_owned();
            api.register_observer::<OnSystemStart, _>(name, move |_: &GraphEvent| {
                order_clone.lock().unwrap().push(name_owned.clone());
            })
            .expect("registration should succeed");
        }

        let mut ctx = SystemContext::new();
        let event = GraphEvent::SystemStart {
            node_id: NodeId::new(),
            node_name: "test",
        };

        api.invoke(schedule, &mut ctx, &event);

        let order = execution_order.lock().unwrap();
        assert_eq!(
            *order,
            vec!["first", "second", "third"],
            "hooks should execute in registration order"
        );
    }

    #[test]
    fn hooks_api_invoke_unknown_schedule_is_noop() {
        let api = HooksAPI::new();
        let mut ctx = SystemContext::new();
        let event = GraphEvent::SystemStart {
            node_id: NodeId::new(),
            node_name: "test",
        };

        // Should not panic when no hooks are registered
        api.invoke(OnSystemStart::schedule_id(), &mut ctx, &event);
    }

    // Test resource type for provider tests
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestResource {
        value: i32,
    }
    impl LocalResource for TestResource {}

    #[test]
    fn register_provider_inserts_resource() {
        let api = HooksAPI::new();

        api.register_provider::<OnSystemStart, TestResource, _>(
            "provider",
            |_event: &GraphEvent| Some(TestResource { value: 42 }),
        )
        .expect("registration should succeed");

        let mut ctx = SystemContext::new();
        let event = GraphEvent::SystemStart {
            node_id: NodeId::new(),
            node_name: "test",
        };

        // Before invoke, resource should not exist
        assert!(!ctx.contains_resource::<TestResource>());

        api.invoke(OnSystemStart::schedule_id(), &mut ctx, &event);

        // After invoke, resource should exist
        let resource = ctx
            .get_resource::<TestResource>()
            .expect("resource should be inserted");
        assert_eq!(resource.value, 42);
    }

    #[test]
    fn provided_resources_for_returns_provider_types() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();

        // No hooks registered yet
        assert!(api.provided_resources_for(schedule).is_empty());

        // Register observer (no resources)
        api.register_observer::<OnSystemStart, _>("observer", |_: &GraphEvent| {})
            .unwrap();
        assert!(
            api.provided_resources_for(schedule).is_empty(),
            "observers provide no resources"
        );

        // Register provider
        api.register_provider::<OnSystemStart, TestResource, _>("provider", |_: &GraphEvent| {
            Some(TestResource { value: 0 })
        })
        .unwrap();

        let provided = api.provided_resources_for(schedule);
        assert_eq!(provided.len(), 1);
        assert_eq!(provided[0], TypeId::of::<TestResource>());
    }

    #[test]
    fn register_boxed_rejects_duplicate_names() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();

        // First registration should succeed
        api.register_boxed(
            schedule,
            "my_hook",
            BoxedHook::new(move |_ctx, _event| {}, Vec::new()),
        )
        .expect("first registration should succeed");

        // Second registration with same name should fail
        let result = api.register_boxed(
            schedule,
            "my_hook",
            BoxedHook::new(move |_ctx, _event| {}, Vec::new()),
        );

        assert!(result.is_err());
        if let Err(HookRegistrationError::DuplicateName { name, .. }) = result {
            assert_eq!(name, "my_hook");
        } else {
            panic!("expected DuplicateName error");
        }
    }

    #[test]
    fn same_name_different_schedules_allowed() {
        let api = HooksAPI::new();

        api.register_observer::<OnSystemStart, _>("logger", |_: &GraphEvent| {})
            .expect("first registration should succeed");

        api.register_observer::<OnSystemComplete, _>("logger", |_: &GraphEvent| {})
            .expect("same name on different schedule should succeed");

        assert_eq!(api.hook_count(OnSystemStart::schedule_id()), 1);
        assert_eq!(api.hook_count(OnSystemComplete::schedule_id()), 1);
    }

    #[test]
    fn register_observer_chaining() {
        let api = HooksAPI::new();

        api.register_observer::<OnSystemStart, _>("first", |_: &GraphEvent| {})
            .unwrap()
            .register_observer::<OnSystemStart, _>("second", |_: &GraphEvent| {})
            .unwrap();

        assert_eq!(api.hook_count(OnSystemStart::schedule_id()), 2);
    }

    #[test]
    fn contains_hook() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();

        assert!(!api.contains_hook(schedule, "my_hook"));

        api.register_observer::<OnSystemStart, _>("my_hook", |_: &GraphEvent| {})
            .unwrap();

        assert!(api.contains_hook(schedule, "my_hook"));
        assert!(!api.contains_hook(schedule, "other_hook"));
    }

    #[test]
    fn multiple_providers_last_write_wins() {
        let api = HooksAPI::new();
        let schedule = OnSystemStart::schedule_id();

        // Register three providers that write the same resource type
        api.register_provider::<OnSystemStart, TestResource, _>(
            "first_provider",
            |_: &GraphEvent| Some(TestResource { value: 1 }),
        )
        .unwrap();

        api.register_provider::<OnSystemStart, TestResource, _>(
            "second_provider",
            |_: &GraphEvent| Some(TestResource { value: 2 }),
        )
        .unwrap();

        api.register_provider::<OnSystemStart, TestResource, _>(
            "third_provider",
            |_: &GraphEvent| Some(TestResource { value: 3 }),
        )
        .unwrap();

        let mut ctx = SystemContext::new();
        let event = GraphEvent::SystemStart {
            node_id: NodeId::new(),
            node_name: "test",
        };

        api.invoke(schedule, &mut ctx, &event);

        // Last provider's value should win
        let resource = ctx
            .get_resource::<TestResource>()
            .expect("resource should exist");
        assert_eq!(resource.value, 3, "last provider's value should win");
    }

    #[test]
    fn register_observer_multiple_schedules() {
        let api = HooksAPI::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);

        api.register_observer::<(OnSystemStart, OnSystemComplete), _>(
            "tracker",
            move |event: &GraphEvent| {
                events_clone
                    .lock()
                    .unwrap()
                    .push(event.schedule_name().to_string());
            },
        )
        .unwrap();

        // Should register on both schedules
        assert_eq!(api.hook_count(OnSystemStart::schedule_id()), 1);
        assert_eq!(api.hook_count(OnSystemComplete::schedule_id()), 1);

        let mut ctx = SystemContext::new();

        api.invoke(
            OnSystemStart::schedule_id(),
            &mut ctx,
            &GraphEvent::SystemStart {
                node_id: NodeId::new(),
                node_name: "test",
            },
        );

        api.invoke(
            OnSystemComplete::schedule_id(),
            &mut ctx,
            &GraphEvent::SystemComplete {
                node_id: NodeId::new(),
                node_name: "test",
                duration: Duration::ZERO,
            },
        );

        let names = events.lock().unwrap();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"OnSystemStart".to_string()));
        assert!(names.contains(&"OnSystemComplete".to_string()));
    }

    #[test]
    fn graph_event_provides_typed_access_in_hook() {
        let api = HooksAPI::new();
        let captured = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured);

        api.register_observer::<OnSystemStart, _>("capture", move |event: &GraphEvent| {
            if let GraphEvent::SystemStart {
                node_name: system_name,
                ..
            } = event
            {
                *captured_clone.lock().unwrap() = Some(system_name.to_string());
            }
        })
        .unwrap();

        let mut ctx = SystemContext::new();
        api.invoke(
            OnSystemStart::schedule_id(),
            &mut ctx,
            &GraphEvent::SystemStart {
                node_id: NodeId::new(),
                node_name: "my_system",
            },
        );

        let name = captured.lock().unwrap().take().unwrap();
        assert_eq!(name, "my_system");
    }
}
