//! Executed-graph coverage for the output store's producer rule: every write
//! keeps its producer, bounded by "same producer replaces its own entry."
//!
//! `crates/polaris_system/src/resource/output.rs`'s own unit tests cover the
//! storage rules directly; these tests exercise the same rules end to end
//! through a real `GraphExecutor` run, proving the executor wires real
//! `NodeId` values into `ProducerKey` and `ProducerInfo` correctly.

use polaris_graph::ProducerInfo;
use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_system::param::{Out, SystemContext};
use polaris_system::resource::Output;
use polaris_system::system::{BoxFuture, System, SystemError};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A system that reads every currently-stored entry of `T` and hands them to
/// the caller, so a test can observe mid-run storage state that
/// `GraphExecutor::execute`'s end-of-run `take_last()` would otherwise
/// disturb.
struct CaptureEntries<T> {
    captured: Arc<Mutex<Vec<T>>>,
    _marker: PhantomData<T>,
}

impl<T: Output + Clone> System for CaptureEntries<T> {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let entries = ctx
                .outputs()
                .entries::<T>()
                .expect("no concurrent writer holds the lock in a test");
            *self.captured.lock().unwrap() =
                entries.iter().map(|(_, value)| (**value).clone()).collect();
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "capture_entries"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Two different nodes writing the same type both keep an entry
// ─────────────────────────────────────────────────────────────────────────────

async fn produce_first() -> i32 {
    1
}

#[polaris_system::system]
async fn produce_second(_first: Out<i32>) -> i32 {
    2
}

#[tokio::test]
async fn two_different_nodes_writing_the_same_type_both_keep_an_entry() {
    let captured = Arc::new(Mutex::new(Vec::new()));

    let mut graph = Graph::new();
    graph
        .add_system(produce_first)
        .add_system(produce_second)
        .add_boxed_system(Box::new(CaptureEntries::<i32> {
            captured: Arc::clone(&captured),
            _marker: PhantomData,
        }));
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    let result = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    assert_eq!(
        result.output::<()>(),
        Some(&()),
        "the capture node is the terminal write"
    );
    assert_eq!(
        *captured.lock().unwrap(),
        vec![1, 2],
        "both producers' entries are kept, oldest first; a later `Out<i32>` read \
         still resolves to the newest (2), which `produce_second` itself proves \
         by successfully reading it before writing its own 2"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// A loop writing the same type on every iteration keeps one entry
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counter(u32);

async fn init_counter() -> Counter {
    Counter(0)
}

#[polaris_system::system]
async fn increment(counter: Out<Counter>) -> Counter {
    Counter(counter.0 + 1)
}

#[tokio::test]
async fn a_loop_writing_the_same_type_100_times_keeps_one_entry_for_that_producer() {
    let captured = Arc::new(Mutex::new(Vec::new()));

    let mut graph = Graph::new();
    graph
        .add_system(init_counter)
        .add_loop::<Counter, _, _>(
            "counter_loop",
            |c: &Counter| c.0 >= 100,
            |g| {
                g.add_system(increment);
            },
        )
        .add_boxed_system(Box::new(CaptureEntries::<Counter> {
            captured: Arc::clone(&captured),
            _marker: PhantomData,
        }));
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let entries = captured.lock().unwrap();
    // `init_counter` and `increment` are different nodes, so each keeps its
    // own entry — `init_counter`'s `Counter(0)` and `increment`'s single
    // retained entry. What bounded size actually claims is that the second
    // slot does not grow to 100 separate entries, one per iteration.
    assert_eq!(
        *entries,
        vec![Counter(0), Counter(100)],
        "a loop writing the same type 100 times keeps one entry for that \
         producer (not one entry per iteration), alongside the seed \
         producer's own entry: {entries:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Parallel merge order follows branch index, not completion time
// ─────────────────────────────────────────────────────────────────────────────

struct SlowBranch {
    delay: Duration,
    value: &'static str,
}

impl System for SlowBranch {
    type Output = Arc<str>;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        let delay = self.delay;
        let value = self.value;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(Arc::from(value))
        })
    }

    fn name(&self) -> &'static str {
        "slow_branch"
    }
}

#[tokio::test]
async fn parallel_merge_order_follows_branch_index_regardless_of_finish_time() {
    let captured = Arc::new(Mutex::new(Vec::new()));

    let mut graph = Graph::new();
    graph
        .add_parallel(
            "par",
            [
                // Branch 0 finishes *last*; if merge order tracked
                // completion time instead of branch index, its entry would
                // land after branch 2's.
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(SlowBranch {
                        delay: Duration::from_millis(30),
                        value: "branch_0",
                    }));
                }) as Box<dyn FnOnce(&mut Graph)>,
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(SlowBranch {
                        delay: Duration::from_millis(15),
                        value: "branch_1",
                    }));
                }),
                Box::new(|g: &mut Graph| {
                    g.add_boxed_system(Box::new(SlowBranch {
                        delay: Duration::from_millis(0),
                        value: "branch_2",
                    }));
                }),
            ],
        )
        .add_boxed_system(Box::new(CaptureEntries::<Arc<str>> {
            captured: Arc::clone(&captured),
            _marker: PhantomData,
        }));
    assert!(graph.validate().is_ok(), "{}", graph.validate());

    let mut ctx = SystemContext::new();
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    let entries: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        entries,
        vec!["branch_0", "branch_1", "branch_2"],
        "merge order follows branch declaration order even though branch 0 \
         finishes last and branch 2 finishes first"
    );
}

#[tokio::test]
async fn a_runtime_named_system_records_its_node_name_as_the_producer_name() {
    async fn seven() -> i32 {
        7
    }

    let mut graph = Graph::new();
    graph.system(seven).named("scorer_a");
    graph.system(seven);

    let mut ctx = SystemContext::new();
    GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap();

    // The executor takes the newest entry into the result, so the first
    // (runtime-named) producer's entry is the one left in the context.
    let names: Vec<String> = ctx
        .outputs()
        .entries::<i32>()
        .unwrap()
        .iter()
        .map(|(producer, _)| {
            ProducerInfo::of(producer)
                .expect("the executor attaches graph provenance to every write")
                .node_name()
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["scorer_a".to_owned()]);
}
