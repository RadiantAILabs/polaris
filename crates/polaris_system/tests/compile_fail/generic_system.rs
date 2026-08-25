//! A generic system function must be rejected: the generated struct and
//! factory carry no generics, so accepting the signature would silently
//! discard an unused parameter or fail unspanned inside the body for a used
//! one.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system]
async fn generic_system<T: Send>(counter: Res<Counter>) {
    let _ = &counter;
}

fn main() {}
