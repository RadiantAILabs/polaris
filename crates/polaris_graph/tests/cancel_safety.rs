//! Dropping a run's future part-way leaves its `SystemContext` clean.
//!
//! A session reuses one context across turns, and a turn's future can be
//! dropped at any `.await` (for example when an HTTP client disconnects). The
//! executor changes two pieces of context state while a node runs: the
//! `read_from` bindings of the running node, and the output layers of an
//! enclosing loop or output boundary. Both must be undone when the future is
//! dropped, or the next run on the same context reads the wrong outputs.

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_system::param::{Out, OutAll, SystemAccess, SystemContext, SystemParam};
use polaris_system::system::{BoxFuture, System, SystemError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Val(i32);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Note(&'static str);

async fn produce_a() -> Val {
    Val(1)
}

async fn produce_b() -> Val {
    Val(2)
}

async fn produce_c() -> Val {
    Val(3)
}

async fn leaked_note() -> Note {
    Note("from the dropped run")
}

async fn fresh_note() -> Note {
    Note("from the later run")
}

#[polaris_system::system]
async fn read_val(val: Out<Val>) -> i32 {
    val.0
}

#[polaris_system::system]
async fn read_notes(notes: OutAll<Note>) -> Vec<Note> {
    notes.iter().map(|note| note.value().clone()).collect()
}

/// Reads `Out<Val>` (so a `read_from` wire can target it), records what it
/// read, sets `stalled`, and then never finishes.
struct Stall {
    stalled: Arc<AtomicBool>,
    read: Arc<std::sync::Mutex<Option<i32>>>,
}

impl System for Stall {
    type Output = ();

    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            {
                let val = Out::<Val>::fetch(ctx).map_err(SystemError::ParamError)?;
                *self.read.lock().expect("test mutex") = Some(val.0);
            }
            self.stalled.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "stall"
    }

    fn access(&self) -> SystemAccess {
        <Out<Val> as SystemParam>::access()
    }
}

fn stall() -> (Stall, Arc<AtomicBool>, Arc<std::sync::Mutex<Option<i32>>>) {
    let stalled = Arc::new(AtomicBool::new(false));
    let read = Arc::new(std::sync::Mutex::new(None));
    let system = Stall {
        stalled: Arc::clone(&stalled),
        read: Arc::clone(&read),
    };
    (system, stalled, read)
}

/// Polls `graph`'s execution until `stalled` is set, then drops the future,
/// cancelling the run in the middle of the stalling node.
async fn drop_execution_mid_node(graph: &Graph, ctx: &mut SystemContext<'_>, stalled: &AtomicBool) {
    let executor = GraphExecutor::new();
    let mut run = std::pin::pin!(executor.execute(graph, ctx, None, None));
    while !stalled.load(Ordering::SeqCst) {
        assert!(
            futures::poll!(run.as_mut()).is_pending(),
            "the stalling system never lets the run finish"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn a_dropped_run_does_not_leave_its_read_from_binding_installed() {
    let (system, stalled, read) = stall();
    let mut first = Graph::new();
    let a = first.system(produce_a).id();
    first.add_system(produce_b);
    let reader = first.add_boxed_system(Box::new(system));
    first.read_from::<Val>(reader, a);
    assert!(first.validate().is_ok(), "{}", first.validate());

    let mut ctx = SystemContext::new();
    drop_execution_mid_node(&first, &mut ctx, &stalled).await;
    assert_eq!(
        *read.lock().expect("test mutex"),
        Some(1),
        "the binding was in effect while the node ran"
    );

    // The binding to `produce_a` must be gone: an unbound `Out<Val>` read
    // sees the newest entry, which `produce_b` wrote.
    assert_eq!(
        Out::<Val>::fetch(&ctx)
            .expect("both producers wrote a Val")
            .0,
        2
    );

    // A later run on the same context reads outputs normally.
    let mut later = Graph::new();
    later.add_system(produce_c).add_system(read_val);
    let result = GraphExecutor::new()
        .execute(&later, &mut ctx, None, None)
        .await
        .expect("the later run executes");
    assert_eq!(result.output::<i32>(), Some(&3));
}

#[tokio::test]
async fn a_run_dropped_inside_a_loop_does_not_leave_its_iteration_layer_open() {
    let (system, stalled, _read) = stall();
    let mut first = Graph::new();
    first
        .add_system(produce_a)
        .add_loop_n("loop", 3, move |body| {
            body.add_system(leaked_note)
                .add_boxed_system(Box::new(system));
        });

    let mut ctx = SystemContext::new();
    drop_execution_mid_node(&first, &mut ctx, &stalled).await;
    assert!(
        !ctx.contains_output::<Note>(),
        "the dropped iteration's output was discarded with its layer"
    );

    // If the iteration layer were still open, this run would write into it
    // and still see the dropped run's note.
    let mut later = Graph::new();
    later.add_system(fresh_note).add_system(read_notes);
    let result = GraphExecutor::new()
        .execute(&later, &mut ctx, None, None)
        .await
        .expect("the later run executes");
    assert_eq!(
        result.output::<Vec<Note>>(),
        Some(&vec![Note("from the later run")])
    );
}
