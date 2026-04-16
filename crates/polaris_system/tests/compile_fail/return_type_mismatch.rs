//! A non-fallible system that `return`s a value of the wrong type should
//! produce a localized type error pointing at the `return` expression — not a
//! confusing cascade at the `#[system]` macro site.
//!
//! Previously, `return wrong_value` in an infallible system surfaced as
//! `expected Result<T, SystemError>, found T` at the macro invocation,
//! because the body lived directly inside the outer `async move { ... }` that
//! must yield a `Result`. The macro now isolates the body in an inner async
//! block whose return type is `T`, so this error now points at the `return`.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[derive(Debug)]
struct CounterOutput {
    value: i32,
}

#[system]
async fn wrong_return_type(counter: Res<Counter>) -> CounterOutput {
    if counter.count == 0 {
        return 42i32;
    }
    CounterOutput {
        value: counter.count,
    }
}

fn main() {}
