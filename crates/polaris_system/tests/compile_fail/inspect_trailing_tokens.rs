//! Tokens after `inspect(..)` must be rejected rather than silently dropped —
//! a typo like a stray word must not compile into a system that quietly
//! ignores it.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect(counter) garbage)]
async fn trailing_tokens(counter: Res<Counter>) {
    let _ = &counter;
}

fn main() {}
