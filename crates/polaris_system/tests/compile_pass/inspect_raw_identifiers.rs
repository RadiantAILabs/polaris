//! Raw identifiers are legal parameter names, and `inspect(..)` must accept
//! them — including `r#return`, which stays distinct from the `return`
//! keyword that selects the return value.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect(r#type, r#return, return))]
async fn raw_identifiers(r#type: Res<Counter>, r#return: Res<Counter>) -> i32 {
    r#type.count + r#return.count
}

fn main() {
    let _ = raw_identifiers();
}
