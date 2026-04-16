//! Integration tests for the `#[system]` macro.
//!
//! These tests are in a separate integration test file because the macro
//! generates code using `::polaris_system::` paths, which only work when
//! the crate is used as an external dependency.

use polaris_system::param::{Out, Res, ResMut, SystemContext};
use polaris_system::prelude::SystemError;
use polaris_system::resource::LocalResource;
use polaris_system::system;
use polaris_system::system::{ErasedSystem, System};
use std::any::TypeId;

#[derive(Debug, PartialEq, Clone)]
struct TestOutput {
    value: i32,
}

#[derive(Debug, PartialEq)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[derive(Debug, PartialEq)]
struct Config {
    multiplier: i32,
}

impl LocalResource for Config {}

// ─────────────────────────────────────────────────────────────────────────────
// Single parameter system
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_read_counter(counter: Res<Counter>) -> TestOutput {
    TestOutput {
        value: counter.count,
    }
}

#[tokio::test]
async fn macro_single_param_system() {
    let system = macro_read_counter();
    let ctx = SystemContext::new().with(Counter { count: 42 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 42);
}

// ─────────────────────────────────────────────────────────────────────────────
// Multiple parameter system
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_compute(counter: Res<Counter>, config: Res<Config>) -> TestOutput {
    TestOutput {
        value: counter.count * config.multiplier,
    }
}

#[tokio::test]
async fn macro_multi_param_system() {
    let system = macro_compute();
    let ctx = SystemContext::new()
        .with(Counter { count: 7 })
        .with(Config { multiplier: 6 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 42);
}

// ─────────────────────────────────────────────────────────────────────────────
// Mutable resource system
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_increment(mut counter: ResMut<Counter>) -> TestOutput {
    counter.count += 1;
    TestOutput {
        value: counter.count,
    }
}

#[tokio::test]
async fn macro_mutable_resource_system() {
    let system = macro_increment();
    let ctx = SystemContext::new().with(Counter { count: 0 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 1);

    let result2 = system.run(&ctx).await.unwrap();
    assert_eq!(result2.value, 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Output chain system
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_double_output(prev: Out<TestOutput>) -> TestOutput {
    TestOutput {
        value: prev.value * 2,
    }
}

#[tokio::test]
async fn macro_output_chain_system() {
    let system = macro_double_output();
    let mut ctx = SystemContext::new();
    ctx.insert_output(TestOutput { value: 21 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 42);
}

// ─────────────────────────────────────────────────────────────────────────────
// System name
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn macro_system_has_correct_name() {
    let system = macro_read_counter();
    assert_eq!(System::name(&system), "macro_read_counter");
}

// ─────────────────────────────────────────────────────────────────────────────
// Fallible system (Result<T, SystemError>)
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_fallible(counter: Res<Counter>) -> Result<TestOutput, SystemError> {
    if counter.count < 0 {
        return Err(SystemError::ExecutionError("negative count".to_string()));
    }
    Ok(TestOutput {
        value: counter.count,
    })
}

#[test]
fn macro_fallible_output_type_is_unwrapped() {
    let system = macro_fallible();
    // Output type should be TestOutput, not Result<TestOutput, SystemError>
    assert_eq!(system.output_type_id(), TypeId::of::<TestOutput>());
    assert_ne!(
        system.output_type_id(),
        TypeId::of::<Result<TestOutput, SystemError>>()
    );
}

#[tokio::test]
async fn macro_fallible_ok_returns_output() {
    let system = macro_fallible();
    let ctx = SystemContext::new().with(Counter { count: 10 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 10);
}

#[tokio::test]
async fn macro_fallible_err_propagates() {
    let system = macro_fallible();
    let ctx = SystemContext::new().with(Counter { count: -1 });

    let result = system.run(&ctx).await;
    assert!(result.is_err());
    assert!(
        matches!(result.unwrap_err(), SystemError::ExecutionError(msg) if msg == "negative count")
    );
}

#[tokio::test]
async fn macro_fallible_erased_ok_stores_correct_type() {
    let system = macro_fallible();
    let erased: &dyn ErasedSystem = &system;
    let ctx = SystemContext::new().with(Counter { count: 5 });

    let boxed = erased.run_erased(&ctx).await.unwrap();
    let concrete = boxed
        .downcast::<TestOutput>()
        .expect("should downcast to TestOutput, not Result");
    assert_eq!(concrete.value, 5);
}

#[tokio::test]
async fn macro_fallible_erased_err_propagates() {
    let system = macro_fallible();
    let erased: &dyn ErasedSystem = &system;
    let ctx = SystemContext::new().with(Counter { count: -1 });

    let result = erased.run_erased(&ctx).await;
    assert!(result.is_err());
}

// ─────────────────────────────────────────────────────────────────────────────
// is_fallible
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn macro_infallible_is_not_fallible() {
    let system = macro_read_counter();
    assert!(!System::is_fallible(&system));

    let erased: &dyn ErasedSystem = &system;
    assert!(!erased.is_fallible());
}

#[test]
fn macro_fallible_is_fallible() {
    let system = macro_fallible();
    assert!(System::is_fallible(&system));

    let erased: &dyn ErasedSystem = &system;
    assert!(erased.is_fallible());
}

// ─────────────────────────────────────────────────────────────────────────────
// Early `return` in a non-fallible system (regression test)
//
// Before the fix, the macro wrapped the body in `Ok(#body)`, so a `return` in
// the body escaped the outer `async move` block and produced a confusing type
// mismatch (T vs Result<T, SystemError>). The macro now isolates the body in
// an inner `async move` block so `return` exits the inner block and the value
// is wrapped in `Ok` afterwards.
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn macro_early_return(counter: Res<Counter>) -> TestOutput {
    if counter.count == 0 {
        return TestOutput { value: -1 };
    }
    TestOutput {
        value: counter.count * 2,
    }
}

#[tokio::test]
async fn macro_non_fallible_early_return_branch() {
    let system = macro_early_return();
    let ctx = SystemContext::new().with(Counter { count: 0 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, -1);
}

#[tokio::test]
async fn macro_non_fallible_fallthrough_branch() {
    let system = macro_early_return();
    let ctx = SystemContext::new().with(Counter { count: 5 });

    let result = system.run(&ctx).await.unwrap();
    assert_eq!(result.value, 10);
}
