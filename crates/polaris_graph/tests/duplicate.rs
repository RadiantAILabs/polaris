//! Behavioral integration tests for [`Graph::duplicate`].
//!
//! The unit tests in `graph/mod.rs` cover the structural contract (fresh ids,
//! remapped references, independence, validation). These tests prove the
//! complementary runtime contract: a duplicated graph *executes* identically
//! to the original, because the system / predicate behavior is shared (`Arc`)
//! rather than dropped during the clone.

use polaris_graph::executor::GraphExecutor;
use polaris_graph::graph::Graph;
use polaris_system::server::Server;

#[derive(Debug, Clone, PartialEq)]
struct Value(i32);

async fn produce_seven() -> Value {
    Value(7)
}

async fn small() -> Value {
    Value(1)
}

async fn large() -> Value {
    Value(100)
}

async fn produce_twenty() -> Value {
    Value(20)
}

/// A cloned sequential graph produces the same output as the original.
#[tokio::test]
async fn duplicate_executes_identically_to_original() {
    let server = Server::new();
    let mut original = Graph::new();
    original.add_system(produce_seven);
    let clone = original.duplicate();
    assert!(original.validate().is_ok());
    assert!(clone.validate().is_ok());

    let executor = GraphExecutor::new();

    let mut ctx_original = server.create_context();
    let original_result = executor
        .execute(&original, &mut ctx_original, None, None)
        .await
        .expect("original executes");

    let mut ctx_clone = server.create_context();
    let clone_result = executor
        .execute(&clone, &mut ctx_clone, None, None)
        .await
        .expect("clone executes");

    assert_eq!(clone_result.output::<Value>().unwrap().0, 7);
    assert_eq!(
        original_result.output::<Value>().unwrap().0,
        clone_result.output::<Value>().unwrap().0,
    );
}

/// The decision predicate survives the clone: the cloned graph routes on the
/// same condition and reaches the same branch.
#[tokio::test]
async fn duplicate_preserves_decision_predicate() {
    let server = Server::new();
    let mut original = Graph::new();
    original
        .add_system(produce_seven) // Out<Value> = 7
        .add_conditional_branch::<Value, _, _, _>(
            "is_small",
            |value| value.0 < 10,
            |branch| {
                branch.add_system(small);
            },
            |branch| {
                branch.add_system(large);
            },
        );
    let clone = original.duplicate();
    assert!(clone.validate().is_ok());

    let executor = GraphExecutor::new();
    let mut ctx = server.create_context();
    let result = executor
        .execute(&clone, &mut ctx, None, None)
        .await
        .expect("clone executes");

    // 7 < 10, so the true branch (`small`) runs in the clone just as it would
    // in the original.
    assert_eq!(result.output::<Value>().unwrap().0, 1);
}

/// The false branch of a cloned decision routes identically too: a value ≥ 10
/// reaches `large`. Together with the test above (true branch) this pins both
/// predicate outcomes across the clone, so a branch mis-remap can't slip
/// through by only ever exercising one side.
#[tokio::test]
async fn duplicate_routes_decision_false_branch_identically() {
    let server = Server::new();
    let mut original = Graph::new();
    original
        .add_system(produce_twenty) // Out<Value> = 20
        .add_conditional_branch::<Value, _, _, _>(
            "is_small",
            |value| value.0 < 10,
            |branch| {
                branch.add_system(small);
            },
            |branch| {
                branch.add_system(large);
            },
        );
    let clone = original.duplicate();
    assert!(clone.validate().is_ok());

    let executor = GraphExecutor::new();
    let mut ctx = server.create_context();
    let result = executor
        .execute(&clone, &mut ctx, None, None)
        .await
        .expect("clone executes");

    // 20 >= 10, so the false branch (`large`) runs in the clone.
    assert_eq!(result.output::<Value>().unwrap().0, 100);
}
