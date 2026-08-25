//! A duplicated selector in `inspect(..)` is usually a typo of a different
//! parameter, so it is reported rather than silently deduplicated.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect(counter, counter))]
async fn duplicate_selector(counter: Res<Counter>) {
    let _ = &counter;
}

fn main() {}
