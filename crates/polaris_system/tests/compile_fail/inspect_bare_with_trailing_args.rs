//! Bare `inspect` selects everything, so combining it with further arguments
//! is contradictory and must be rejected — not silently accepted with the
//! extra argument ignored.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect, counter)]
async fn bare_with_args(counter: Res<Counter>) {
    let _ = &counter;
}

fn main() {}
