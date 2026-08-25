//! `inspect(..)` naming a parameter the system does not have must fail to
//! compile rather than silently capturing nothing.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect(nonexistent))]
async fn inspect_unknown(counter: Res<Counter>) {
    let _ = &counter;
}

fn main() {}
