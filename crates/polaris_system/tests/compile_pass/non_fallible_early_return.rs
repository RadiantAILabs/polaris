//! Regression test: a non-fallible system may use `return` for early exit.
//!
//! Previously the macro wrapped the body in `Ok(#body)`, so an early `return`
//! escaped the outer `async move` block as the raw output type rather than the
//! `Result<T, SystemError>` the block must produce. The macro now isolates the
//! body in an inner `async move` block so `return` exits the inner block and
//! the value is wrapped in `Ok` afterwards.

use polaris_system::param::{Res, SystemContext};
use polaris_system::resource::LocalResource;
use polaris_system::system;
use polaris_system::system::System;

struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[derive(Debug)]
struct CounterOutput {
    value: i32,
}

#[system]
async fn early_return_system(counter: Res<Counter>) -> CounterOutput {
    if counter.count == 0 {
        return CounterOutput { value: -1 };
    }
    CounterOutput {
        value: counter.count * 2,
    }
}

fn main() {
    let _system = early_return_system();
}
