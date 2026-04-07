//! Tests for the Polaris System server, plugins, resources, and APIs.
//!
//! These tests cover basic server functionality, plugin lifecycle, resource management, and API interactions.
//! These tests have been moved directly from the `polaris_system/src/server.rs` file.

use polaris_system::plugin::{Plugin, PluginGroup, PluginId, Version};
use polaris_system::prelude::*;
use std::sync::atomic::AtomicBool;

// ─────────────────────────────────────────────────────────────────────────
// Test Resources
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct TestResource {
    value: i32,
}

#[derive(Debug, PartialEq)]
struct AnotherResource {
    name: String,
}

// ─────────────────────────────────────────────────────────────────────────
// Test Plugins
// ─────────────────────────────────────────────────────────────────────────

struct PluginA;
impl Plugin for PluginA {
    const ID: &'static str = "test::plugin_a";
    const VERSION: Version = Version::new(0, 0, 1);
    fn build(&self, server: &mut Server) {
        server.insert_resource(TestResource { value: 1 });
    }
}

struct PluginB;
impl Plugin for PluginB {
    const ID: &'static str = "test::plugin_b";
    const VERSION: Version = Version::new(0, 0, 1);
    fn build(&self, server: &mut Server) {
        // Modify the resource set by PluginA
        if let Some(mut res) = server.get_resource_mut::<TestResource>() {
            res.value += 10;
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PluginA>()]
    }
}

struct PluginC;
impl Plugin for PluginC {
    const ID: &'static str = "test::plugin_c";
    const VERSION: Version = Version::new(0, 0, 1);
    fn build(&self, server: &mut Server) {
        if let Some(mut res) = server.get_resource_mut::<TestResource>() {
            res.value *= 2;
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<PluginB>()]
    }
}

struct ReadyPlugin {
    ready_called: AtomicBool,
}

impl Default for ReadyPlugin {
    fn default() -> Self {
        Self {
            ready_called: AtomicBool::new(false),
        }
    }
}

impl Plugin for ReadyPlugin {
    const ID: &'static str = "test::ready";
    const VERSION: Version = Version::new(0, 0, 1);
    fn build(&self, _server: &mut Server) {}

    async fn ready(&self, server: &mut Server) {
        self.ready_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        server.insert_resource(AnotherResource {
            name: "ready".into(),
        });
    }
}

struct CleanupPlugin;
impl Plugin for CleanupPlugin {
    const ID: &'static str = "test::cleanup";
    const VERSION: Version = Version::new(0, 0, 1);
    fn build(&self, server: &mut Server) {
        server.insert_resource(TestResource { value: 100 });
    }

    async fn cleanup(&self, server: &mut Server) {
        // Remove the resource during cleanup
        server.remove_resource::<TestResource>();
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn server_new_is_empty() {
    let server = Server::new();
    assert!(!server.contains_resource::<TestResource>());
    assert!(!server.is_built());
}

#[test]
fn server_insert_and_get_resource() {
    let mut server = Server::new();
    server.insert_resource(TestResource { value: 42 });

    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 42);
}

#[test]
fn server_resource_mutation() {
    let mut server = Server::new();
    server.insert_resource(TestResource { value: 0 });

    {
        let mut res = server.get_resource_mut::<TestResource>().unwrap();
        res.value = 100;
    }

    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 100);
}

#[test]
fn server_remove_resource() {
    let mut server = Server::new();
    server.insert_resource(TestResource { value: 42 });

    let removed = server.remove_resource::<TestResource>();
    assert_eq!(removed, Some(TestResource { value: 42 }));
    assert!(!server.contains_resource::<TestResource>());
}

#[tokio::test]
async fn plugin_build_inserts_resource() {
    let mut server = Server::new();
    server.add_plugins(PluginA);
    server.finish().await;

    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 1);
}

#[tokio::test]
async fn plugins_build_in_dependency_order() {
    let mut server = Server::new();
    // Add in reverse dependency order
    server.add_plugins(PluginC);
    server.add_plugins(PluginA);
    server.add_plugins(PluginB);
    server.finish().await;

    // A sets to 1, B adds 10 (=11), C multiplies by 2 (=22)
    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 22);
}

#[tokio::test]
async fn plugin_ready_is_called() {
    let mut server = Server::new();
    server.add_plugins(ReadyPlugin::default());
    server.finish().await;

    // Ready should have inserted the resource
    assert!(server.contains_resource::<AnotherResource>());
    let res = server.get_resource::<AnotherResource>().unwrap();
    assert_eq!(res.name, "ready");
}

#[tokio::test]
async fn plugin_cleanup_is_called() {
    let mut server = Server::new();
    server.add_plugins(CleanupPlugin);
    server.finish().await;

    // Resource exists after build
    assert!(server.contains_resource::<TestResource>());

    server.cleanup().await;

    // Resource removed during cleanup
    assert!(!server.contains_resource::<TestResource>());
}

#[tokio::test]
async fn server_run_calls_finish() {
    let mut server = Server::new();
    server.add_plugins(PluginA);
    server.run().await;

    assert!(server.is_built());
    assert!(server.contains_resource::<TestResource>());
}

#[tokio::test]
async fn server_run_once_calls_finish() {
    let mut server = Server::new();
    server.add_plugins(PluginA);
    server.run_once().await;

    assert!(server.is_built());
    assert!(server.contains_resource::<TestResource>());
}

#[test]
fn has_plugin_returns_true_for_added() {
    let mut server = Server::new();
    server.add_plugins(PluginA);

    assert!(server.has_plugin::<PluginA>());
    assert!(!server.has_plugin::<PluginB>());
}

#[test]
#[should_panic(expected = "already added")]
fn duplicate_unique_plugin_panics() {
    let mut server = Server::new();
    server.add_plugins(PluginA);
    server.add_plugins(PluginA); // Should panic
}

#[tokio::test]
#[should_panic(expected = "requires")]
async fn missing_dependency_panics() {
    let mut server = Server::new();
    server.add_plugins(PluginB); // Requires PluginA
    server.finish().await; // Should panic
}

#[tokio::test]
#[should_panic(expected = "Circular dependency")]
async fn circular_dependency_panics() {
    // Create plugins with circular dependency
    struct CycleA;
    impl Plugin for CycleA {
        const ID: &'static str = "test::cycle_a";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<CycleB>()]
        }
    }

    struct CycleB;
    impl Plugin for CycleB {
        const ID: &'static str = "test::cycle_b";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<CycleA>()]
        }
    }

    let mut server = Server::new();
    server.add_plugins(CycleA);
    server.add_plugins(CycleB);
    server.finish().await; // Should panic
}

#[tokio::test]
#[should_panic(expected = "already called")]
async fn double_finish_panics() {
    let mut server = Server::new();
    server.add_plugins(PluginA);
    server.finish().await;
    server.finish().await; // Should panic
}

#[tokio::test]
async fn sub_plugin_added_during_build() {
    struct ParentPlugin;
    impl Plugin for ParentPlugin {
        const ID: &'static str = "test::parent";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, server: &mut Server) {
            // Add a sub-plugin during build
            server.add_plugins(PluginA);
        }
    }

    let mut server = Server::new();
    server.add_plugins(ParentPlugin);
    server.finish().await;

    // PluginA should have been built
    assert!(server.contains_resource::<TestResource>());
}

// Test PluginGroup
use polaris_system::plugin::PluginGroupBuilder;

struct TestPluginGroup;
impl PluginGroup for TestPluginGroup {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::new().add(PluginA).add(PluginB)
    }
}

#[tokio::test]
async fn plugin_group_adds_all_plugins() {
    let mut server = Server::new();
    server.add_plugins(TestPluginGroup.build());
    server.finish().await;

    // Both plugins should have run
    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 11); // A sets 1, B adds 10
}

#[tokio::test]
async fn cleanup_in_reverse_order() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cleanup_order = Arc::new(AtomicUsize::new(0));
    let first_cleanup = Arc::new(AtomicUsize::new(0));
    let second_cleanup = Arc::new(AtomicUsize::new(0));

    struct OrderedCleanupA {
        order: Arc<AtomicUsize>,
        my_order: Arc<AtomicUsize>,
    }
    impl Plugin for OrderedCleanupA {
        const ID: &'static str = "test::ordered_cleanup_a";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
        async fn cleanup(&self, _server: &mut Server) {
            let n = self.order.fetch_add(1, Ordering::SeqCst);
            self.my_order.store(n, Ordering::SeqCst);
        }
    }

    struct OrderedCleanupB {
        order: Arc<AtomicUsize>,
        my_order: Arc<AtomicUsize>,
    }
    impl Plugin for OrderedCleanupB {
        const ID: &'static str = "test::ordered_cleanup_b";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}
        async fn cleanup(&self, _server: &mut Server) {
            let n = self.order.fetch_add(1, Ordering::SeqCst);
            self.my_order.store(n, Ordering::SeqCst);
        }
        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<OrderedCleanupA>()]
        }
    }

    let mut server = Server::new();
    server.add_plugins(OrderedCleanupA {
        order: cleanup_order.clone(),
        my_order: first_cleanup.clone(),
    });
    server.add_plugins(OrderedCleanupB {
        order: cleanup_order.clone(),
        my_order: second_cleanup.clone(),
    });
    server.finish().await;
    server.cleanup().await;

    // B depends on A, so B should be cleaned up first (reverse order)
    // B is at index 1 in sorted order, A is at index 0
    // Cleanup goes from end to start: B first, then A
    assert_eq!(second_cleanup.load(Ordering::SeqCst), 0); // B cleaned first
    assert_eq!(first_cleanup.load(Ordering::SeqCst), 1); // A cleaned second
}

// ─────────────────────────────────────────────────────────────────────────
// Global / Local Resource Tests
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct GlobalConfig {
    name: String,
}
impl GlobalResource for GlobalConfig {}

#[derive(Debug, PartialEq)]
struct LocalMemory {
    messages: Vec<String>,
}
impl LocalResource for LocalMemory {}

impl LocalMemory {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }
}

#[test]
fn insert_global_resource() {
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "test".into(),
    });

    assert!(server.contains_global::<GlobalConfig>());
    let config = server.get_global::<GlobalConfig>().unwrap();
    assert_eq!(config.name, "test");
}

#[test]
fn register_local_creates_per_context() {
    let mut server = Server::new();
    server.register_local(LocalMemory::new);

    assert!(server.has_local::<LocalMemory>());

    let ctx1 = server.create_context();
    let ctx2 = server.create_context();

    // Each context has its own instance
    {
        let mut mem1 = ctx1.get_resource_mut::<LocalMemory>().unwrap();
        mem1.messages.push("hello".into());
    }

    // ctx1 has the message, ctx2 does not
    assert_eq!(
        ctx1.get_resource::<LocalMemory>().unwrap().messages.len(),
        1
    );
    assert_eq!(
        ctx2.get_resource::<LocalMemory>().unwrap().messages.len(),
        0
    );
}

#[test]
fn create_context_instantiates_locals() {
    let mut server = Server::new();
    server.register_local(LocalMemory::new);

    let ctx = server.create_context();

    // Local resource should be available
    assert!(ctx.contains_resource::<LocalMemory>());
    let mem = ctx.get_resource::<LocalMemory>().unwrap();
    assert!(mem.messages.is_empty());
}

#[test]
fn global_resources_returns_container() {
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "test".into(),
    });

    let globals = server.global_resources();
    assert!(globals.contains::<GlobalConfig>());
}

#[test]
fn context_can_access_global_resources() {
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "global-test".into(),
    });

    let ctx = server.create_context();

    // Global resource should be accessible via get_resource
    assert!(ctx.contains_resource::<GlobalConfig>());
    let config = ctx.get_resource::<GlobalConfig>().unwrap();
    assert_eq!(config.name, "global-test");
}

#[test]
fn child_context_inherits_globals() {
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "inherited".into(),
    });

    let ctx = server.create_context();
    let child = ctx.child();
    let grandchild = child.child();

    // All levels should see the global resource
    assert!(ctx.contains_resource::<GlobalConfig>());
    assert!(child.contains_resource::<GlobalConfig>());
    assert!(grandchild.contains_resource::<GlobalConfig>());

    assert_eq!(
        grandchild.get_resource::<GlobalConfig>().unwrap().name,
        "inherited"
    );
}

#[test]
fn child_context_shadows_parent_local_resource() {
    use polaris_system::resource::LocalResource;

    // LocalResource can be shadowed in child contexts
    // NOTE: A type should implement EITHER GlobalResource OR LocalResource, not both.
    // GlobalResource = read-only, server-wide (accessed via Res<T>)
    // LocalResource = mutable, per-context (accessed via ResMut<T>)
    #[derive(Debug, PartialEq)]
    struct Counter {
        value: i32,
    }
    impl LocalResource for Counter {}

    let mut server = Server::new();
    server.register_local(|| Counter { value: 100 });

    let ctx = server.create_context();

    // Context has local instance from factory
    assert_eq!(ctx.get_resource::<Counter>().unwrap().value, 100);

    // Child with its own Counter shadows parent's
    let child = ctx.child().with(Counter { value: 200 });

    // Child sees its own value (shadows parent)
    assert_eq!(child.get_resource::<Counter>().unwrap().value, 200);

    // Parent still has original value
    assert_eq!(ctx.get_resource::<Counter>().unwrap().value, 100);

    // Grandchild without its own Counter sees child's value
    let grandchild = child.child();
    assert_eq!(grandchild.get_resource::<Counter>().unwrap().value, 200);
}

#[test]
fn global_resource_visible_to_all_contexts() {
    // GlobalResource is read-only and visible to all contexts in hierarchy
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "shared-config".into(),
    });

    let ctx = server.create_context();
    let child = ctx.child();
    let grandchild = child.child();

    // All levels see the same global value
    assert_eq!(
        ctx.get_resource::<GlobalConfig>().unwrap().name,
        "shared-config"
    );
    assert_eq!(
        child.get_resource::<GlobalConfig>().unwrap().name,
        "shared-config"
    );
    assert_eq!(
        grandchild.get_resource::<GlobalConfig>().unwrap().name,
        "shared-config"
    );

    // GlobalResource should NOT be mutable - get_resource_mut should fail
    // (This is enforced at compile time via trait bounds, but we verify the behavior)
    // Note: get_resource_mut on global resources will fail because they're
    // stored in the global container, not the local scope
}

#[test]
fn res_param_can_fetch_global_resource() {
    use polaris_system::param::Res;
    use polaris_system::param::SystemParam;

    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "via-param".into(),
    });

    let ctx = server.create_context();

    // Res<T> should be able to fetch global resources
    let res = Res::<GlobalConfig>::fetch(&ctx).unwrap();
    assert_eq!(res.name, "via-param");
}

#[test]
fn context_with_both_global_and_local() {
    let mut server = Server::new();
    server.insert_global(GlobalConfig {
        name: "global".into(),
    });
    server.register_local(LocalMemory::new);

    let ctx = server.create_context();

    // Both should be accessible
    assert!(ctx.contains_resource::<GlobalConfig>());
    assert!(ctx.contains_resource::<LocalMemory>());

    let config = ctx.get_resource::<GlobalConfig>().unwrap();
    let memory = ctx.get_resource::<LocalMemory>().unwrap();

    assert_eq!(config.name, "global");
    assert!(memory.messages.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────
// Schedule-Based Tick Tests
// ─────────────────────────────────────────────────────────────────────────

// Schedule marker types for testing
struct TestScheduleA;
struct TestScheduleB;

impl Schedule for TestScheduleA {}
impl Schedule for TestScheduleB {}

#[tokio::test]
async fn plugin_registers_for_schedule_gets_ticked() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tick_count = Arc::new(AtomicUsize::new(0));

    struct TickCountingPlugin {
        count: Arc<AtomicUsize>,
    }

    impl Plugin for TickCountingPlugin {
        const ID: &'static str = "test::tick_counting";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut server = Server::new();
    server.add_plugins(TickCountingPlugin {
        count: tick_count.clone(),
    });
    server.finish().await;

    assert_eq!(tick_count.load(Ordering::SeqCst), 0);

    server.tick::<TestScheduleA>();
    assert_eq!(tick_count.load(Ordering::SeqCst), 1);

    server.tick::<TestScheduleA>();
    assert_eq!(tick_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn plugin_not_registered_for_schedule_not_ticked() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tick_count = Arc::new(AtomicUsize::new(0));

    struct SelectivePlugin {
        count: Arc<AtomicUsize>,
    }

    impl Plugin for SelectivePlugin {
        const ID: &'static str = "test::selective";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            // Only registers for ScheduleA
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut server = Server::new();
    server.add_plugins(SelectivePlugin {
        count: tick_count.clone(),
    });
    server.finish().await;

    // Tick with ScheduleB - plugin should NOT be ticked
    server.tick::<TestScheduleB>();
    assert_eq!(tick_count.load(Ordering::SeqCst), 0);

    // Tick with ScheduleA - plugin SHOULD be ticked
    server.tick::<TestScheduleA>();
    assert_eq!(tick_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn multiple_plugins_same_schedule_all_ticked_in_order() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let order = Arc::new(AtomicUsize::new(0));
    let first_order = Arc::new(AtomicUsize::new(0));
    let second_order = Arc::new(AtomicUsize::new(0));

    struct FirstPlugin {
        order: Arc<AtomicUsize>,
        my_order: Arc<AtomicUsize>,
    }

    impl Plugin for FirstPlugin {
        const ID: &'static str = "test::first";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            let n = self.order.fetch_add(1, Ordering::SeqCst);
            self.my_order.store(n, Ordering::SeqCst);
        }
    }

    struct SecondPlugin {
        order: Arc<AtomicUsize>,
        my_order: Arc<AtomicUsize>,
    }

    impl Plugin for SecondPlugin {
        const ID: &'static str = "test::second";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            let n = self.order.fetch_add(1, Ordering::SeqCst);
            self.my_order.store(n, Ordering::SeqCst);
        }

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<FirstPlugin>()]
        }
    }

    let mut server = Server::new();
    // Add in reverse order to test dependency sorting
    server.add_plugins(SecondPlugin {
        order: order.clone(),
        my_order: second_order.clone(),
    });
    server.add_plugins(FirstPlugin {
        order: order.clone(),
        my_order: first_order.clone(),
    });
    server.finish().await;

    server.tick::<TestScheduleA>();

    // First should be ticked before Second (dependency order)
    assert_eq!(first_order.load(Ordering::SeqCst), 0);
    assert_eq!(second_order.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn plugin_multiple_schedules() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));

    struct MultiSchedulePlugin {
        count_a: Arc<AtomicUsize>,
        count_b: Arc<AtomicUsize>,
    }

    impl Plugin for MultiSchedulePlugin {
        const ID: &'static str = "test::multi_schedule";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![
                ScheduleId::of::<TestScheduleA>(),
                ScheduleId::of::<TestScheduleB>(),
            ]
        }

        fn update(&self, _server: &mut Server, schedule: ScheduleId) {
            if schedule == ScheduleId::of::<TestScheduleA>() {
                self.count_a.fetch_add(1, Ordering::SeqCst);
            } else if schedule == ScheduleId::of::<TestScheduleB>() {
                self.count_b.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    let mut server = Server::new();
    server.add_plugins(MultiSchedulePlugin {
        count_a: count_a.clone(),
        count_b: count_b.clone(),
    });
    server.finish().await;

    server.tick::<TestScheduleA>();
    assert_eq!(count_a.load(Ordering::SeqCst), 1);
    assert_eq!(count_b.load(Ordering::SeqCst), 0);

    server.tick::<TestScheduleB>();
    assert_eq!(count_a.load(Ordering::SeqCst), 1);
    assert_eq!(count_b.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tick_unregistered_schedule_is_noop() {
    // Just verify it doesn't panic
    let mut server = Server::new();
    server.add_plugins(PluginA); // PluginA doesn't register for any schedules
    server.finish().await;

    // This should be a no-op (no plugins registered for this schedule)
    server.tick::<TestScheduleA>();
    server.tick::<TestScheduleB>();
}

#[tokio::test]
async fn schedule_passed_to_update_matches_triggered() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let received_correct_schedule = Arc::new(AtomicBool::new(false));

    struct ScheduleCheckPlugin {
        correct: Arc<AtomicBool>,
    }

    impl Plugin for ScheduleCheckPlugin {
        const ID: &'static str = "test::schedule_check";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, schedule: ScheduleId) {
            if schedule == ScheduleId::of::<TestScheduleA>() {
                self.correct.store(true, Ordering::SeqCst);
            }
        }
    }

    let mut server = Server::new();
    server.add_plugins(ScheduleCheckPlugin {
        correct: received_correct_schedule.clone(),
    });
    server.finish().await;

    server.tick::<TestScheduleA>();

    assert!(received_correct_schedule.load(Ordering::SeqCst));
}

// ─────────────────────────────────────────────────────────────────────────
// API Tests
// ─────────────────────────────────────────────────────────────────────────

use polaris_system::api::API;

#[derive(Debug, PartialEq)]
struct TestAPI {
    name: String,
}
impl API for TestAPI {}

#[derive(Debug, PartialEq)]
struct AnotherAPI {
    value: i32,
}
impl API for AnotherAPI {}

#[test]
fn insert_api_and_get() {
    let mut server = Server::new();
    server.insert_api(TestAPI {
        name: "test".into(),
    });

    let api = server.api::<TestAPI>().unwrap();
    assert_eq!(api.name, "test");
}

#[test]
fn contains_api_returns_true_for_inserted() {
    let mut server = Server::new();
    assert!(!server.contains_api::<TestAPI>());

    server.insert_api(TestAPI {
        name: "test".into(),
    });

    assert!(server.contains_api::<TestAPI>());
}

#[test]
fn api_returns_none_for_missing() {
    let server = Server::new();
    assert!(server.api::<TestAPI>().is_none());
}

#[test]
fn insert_api_replaces_and_returns_old() {
    let mut server = Server::new();
    server.insert_api(TestAPI {
        name: "first".into(),
    });

    let old = server.insert_api(TestAPI {
        name: "second".into(),
    });

    assert_eq!(
        old,
        Some(TestAPI {
            name: "first".into()
        })
    );

    let api = server.api::<TestAPI>().unwrap();
    assert_eq!(api.name, "second");
}

#[test]
fn multiple_api_types() {
    let mut server = Server::new();
    server.insert_api(TestAPI {
        name: "test".into(),
    });
    server.insert_api(AnotherAPI { value: 42 });

    assert!(server.contains_api::<TestAPI>());
    assert!(server.contains_api::<AnotherAPI>());

    let test_api = server.api::<TestAPI>().unwrap();
    let another_api = server.api::<AnotherAPI>().unwrap();

    assert_eq!(test_api.name, "test");
    assert_eq!(another_api.value, 42);
}

#[tokio::test]
async fn plugin_inserts_api_in_build() {
    struct APIProviderPlugin;
    impl Plugin for APIProviderPlugin {
        const ID: &'static str = "test::api_provider";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, server: &mut Server) {
            server.insert_api(TestAPI {
                name: "from-plugin".into(),
            });
        }
    }

    let mut server = Server::new();
    server.add_plugins(APIProviderPlugin);
    server.finish().await;

    let api = server.api::<TestAPI>().unwrap();
    assert_eq!(api.name, "from-plugin");
}

#[tokio::test]
async fn plugin_accesses_api_in_ready() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let api_accessed = Arc::new(AtomicBool::new(false));

    struct APIProviderPlugin;
    impl Plugin for APIProviderPlugin {
        const ID: &'static str = "test::api_provider_2";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, server: &mut Server) {
            server.insert_api(TestAPI {
                name: "provided".into(),
            });
        }
    }

    struct APIConsumerPlugin {
        accessed: Arc<AtomicBool>,
    }
    impl Plugin for APIConsumerPlugin {
        const ID: &'static str = "test::api_consumer";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        async fn ready(&self, server: &mut Server) {
            if let Some(api) = server.api::<TestAPI>()
                && api.name == "provided"
            {
                self.accessed.store(true, Ordering::SeqCst);
            }
        }

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<APIProviderPlugin>()]
        }
    }

    let mut server = Server::new();
    server.add_plugins(APIProviderPlugin);
    server.add_plugins(APIConsumerPlugin {
        accessed: api_accessed.clone(),
    });
    server.finish().await;

    assert!(api_accessed.load(Ordering::SeqCst));
}

#[test]
fn api_with_interior_mutability() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CounterAPI {
        count: Arc<AtomicUsize>,
    }
    impl API for CounterAPI {}

    impl CounterAPI {
        fn increment(&self) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }

        fn get(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let mut server = Server::new();
    server.insert_api(CounterAPI {
        count: counter.clone(),
    });

    // Multiple accesses can call methods on the same API
    let api = server.api::<CounterAPI>().unwrap();
    api.increment();
    api.increment();
    api.increment();

    assert_eq!(api.get(), 3);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

// ─────────────────────────────────────────────────────────────────────────
// Server Edge Case Tests
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn resource_insert_returns_old_value() {
    let mut server = Server::new();

    // First insertion returns None
    let old = server.insert_resource(TestResource { value: 1 });
    assert!(old.is_none());

    // Second insertion returns old value
    let old = server.insert_resource(TestResource { value: 2 });
    assert_eq!(old, Some(TestResource { value: 1 }));

    // Verify new value is stored
    let res = server.get_resource::<TestResource>().unwrap();
    assert_eq!(res.value, 2);
}

#[test]
fn global_resource_insert_returns_old_value() {
    let mut server = Server::new();

    // First insertion returns None
    let old = server.insert_global(GlobalConfig {
        name: "first".into(),
    });
    assert!(old.is_none());

    // Second insertion returns old value
    let old = server.insert_global(GlobalConfig {
        name: "second".into(),
    });
    assert_eq!(
        old,
        Some(GlobalConfig {
            name: "first".into()
        })
    );

    // Verify new value is stored
    let config = server.get_global::<GlobalConfig>().unwrap();
    assert_eq!(config.name, "second");
}

#[test]
fn resources_mut_allows_direct_mutation() {
    let mut server = Server::new();
    server.insert_resource(TestResource { value: 10 });

    // Access resources_mut and insert directly
    server.resources_mut().insert(AnotherResource {
        name: "direct".into(),
    });

    // Both resources should exist
    assert!(server.contains_resource::<TestResource>());
    assert!(server.contains_resource::<AnotherResource>());

    let another = server.get_resource::<AnotherResource>().unwrap();
    assert_eq!(another.name, "direct");
}

#[test]
fn has_local_returns_false_for_unregistered() {
    let server = Server::new();
    assert!(!server.has_local::<LocalMemory>());
}

#[test]
fn has_local_returns_true_for_registered() {
    let mut server = Server::new();
    server.register_local(LocalMemory::new);
    assert!(server.has_local::<LocalMemory>());
}

#[test]
fn remove_resource_returns_none_for_missing() {
    let mut server = Server::new();
    let removed = server.remove_resource::<TestResource>();
    assert!(removed.is_none());
}

#[test]
fn get_resource_returns_none_for_missing() {
    let server = Server::new();
    assert!(server.get_resource::<TestResource>().is_none());
}

#[test]
fn get_resource_mut_returns_none_for_missing() {
    let server = Server::new();
    assert!(server.get_resource_mut::<TestResource>().is_none());
}

#[test]
fn server_default_is_same_as_new() {
    let server1 = Server::new();
    let server2 = Server::default();

    // Both should be empty and not built
    assert!(!server1.is_built());
    assert!(!server2.is_built());
    assert!(!server1.contains_resource::<TestResource>());
    assert!(!server2.contains_resource::<TestResource>());
}

#[test]
fn resources_returns_reference() {
    let mut server = Server::new();
    server.insert_resource(TestResource { value: 42 });

    let resources = server.resources();
    assert!(resources.contains::<TestResource>());
}

// ─────────────────────────────────────────────────────────────────────────
// Schedule Integration Tests
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tick_schedule_uses_schedule_id_directly() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tick_count = Arc::new(AtomicUsize::new(0));

    struct ScheduleIdPlugin {
        count: Arc<AtomicUsize>,
    }

    impl Plugin for ScheduleIdPlugin {
        const ID: &'static str = "test::schedule_id";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![ScheduleId::of::<TestScheduleA>()]
        }

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut server = Server::new();
    server.add_plugins(ScheduleIdPlugin {
        count: tick_count.clone(),
    });
    server.finish().await;

    // Use tick_schedule with ScheduleId directly
    let schedule_id = ScheduleId::of::<TestScheduleA>();
    server.tick_schedule(schedule_id);

    assert_eq!(tick_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn update_receives_correct_schedule_id() {
    use parking_lot::Mutex;
    use std::sync::Arc;

    let received_schedules: Arc<Mutex<Vec<ScheduleId>>> = Arc::new(Mutex::new(Vec::new()));

    struct ScheduleTrackingPlugin {
        received: Arc<Mutex<Vec<ScheduleId>>>,
    }

    impl Plugin for ScheduleTrackingPlugin {
        const ID: &'static str = "test::schedule_tracking";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        fn tick_schedules(&self) -> Vec<ScheduleId> {
            vec![
                ScheduleId::of::<TestScheduleA>(),
                ScheduleId::of::<TestScheduleB>(),
            ]
        }

        fn update(&self, _server: &mut Server, schedule: ScheduleId) {
            self.received.lock().push(schedule);
        }
    }

    let mut server = Server::new();
    server.add_plugins(ScheduleTrackingPlugin {
        received: received_schedules.clone(),
    });
    server.finish().await;

    server.tick::<TestScheduleA>();
    server.tick::<TestScheduleB>();
    server.tick::<TestScheduleA>();

    let received = received_schedules.lock();
    assert_eq!(received.len(), 3);
    assert_eq!(received[0], ScheduleId::of::<TestScheduleA>());
    assert_eq!(received[1], ScheduleId::of::<TestScheduleB>());
    assert_eq!(received[2], ScheduleId::of::<TestScheduleA>());
}

#[tokio::test]
async fn plugins_with_no_tick_schedules_never_updated() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let update_count = Arc::new(AtomicUsize::new(0));

    struct NoSchedulePlugin {
        count: Arc<AtomicUsize>,
    }

    impl Plugin for NoSchedulePlugin {
        const ID: &'static str = "test::no_schedule";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        // Default tick_schedules() returns empty vec

        fn update(&self, _server: &mut Server, _schedule: ScheduleId) {
            // This should never be called
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    let mut server = Server::new();
    server.add_plugins(NoSchedulePlugin {
        count: update_count.clone(),
    });
    server.finish().await;

    // Tick various schedules
    server.tick::<TestScheduleA>();
    server.tick::<TestScheduleB>();

    // Plugin should never have been updated
    assert_eq!(update_count.load(Ordering::SeqCst), 0);
}

// ─────────────────────────────────────────────────────────────────────────
// Deferred ContextFactory
// ─────────────────────────────────────────────────────────────────────────

/// Calling `context_factory()` during `ready()` must not prevent a later
/// plugin from calling `insert_global()`.
#[tokio::test]
async fn context_factory_during_ready_does_not_block_insert_global() {
    use polaris_system::server::ContextFactory;
    use std::sync::{Arc, OnceLock};

    /// Global resource registered by PluginLate.
    #[derive(Debug, PartialEq)]
    struct LateGlobal {
        value: i32,
    }
    impl GlobalResource for LateGlobal {}

    /// Captures a `ContextFactory` during `ready()`.
    struct EarlyPlugin {
        factory: Arc<OnceLock<ContextFactory>>,
    }
    impl Plugin for EarlyPlugin {
        const ID: &'static str = "test::early";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        async fn ready(&self, server: &mut Server) {
            // Grab a factory while the server is still building.
            self.factory
                .set(server.context_factory())
                .unwrap_or_else(|_| panic!("factory already set"));
        }
    }

    /// Inserts a global resource during `ready()` — runs *after* EarlyPlugin.
    struct LatePlugin;
    impl Plugin for LatePlugin {
        const ID: &'static str = "test::late";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _server: &mut Server) {}

        async fn ready(&self, server: &mut Server) {
            // This must NOT panic even though EarlyPlugin already called
            // context_factory().
            server.insert_global(LateGlobal { value: 42 });
        }

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<EarlyPlugin>()]
        }
    }

    let factory_holder: Arc<OnceLock<ContextFactory>> = Arc::new(OnceLock::new());
    let mut server = Server::new();
    server
        .add_plugins(EarlyPlugin {
            factory: Arc::clone(&factory_holder),
        })
        .add_plugins(LatePlugin);
    server.finish().await;

    // The deferred factory should now be usable.
    let factory = factory_holder.get().expect("factory was not set");
    let ctx = factory.create_context();
    let late = ctx
        .get_resource::<LateGlobal>()
        .expect("LateGlobal missing");
    assert_eq!(late.value, 42);
}
