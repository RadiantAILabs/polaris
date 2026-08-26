//! Parameter names under the `__polaris` prefix are reserved for identifiers
//! the macro generates. A parameter named `__polaris_sink` would be shadowed
//! inside its own capture block — rejected at expansion so it can never
//! silently record the wrong value.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system(inspect(__polaris_sink))]
async fn reserved_prefix(__polaris_sink: Res<Counter>) {
    let _ = &__polaris_sink;
}

fn main() {}
