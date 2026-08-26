//! Naming a parameter whose type is not `Debug` in `inspect(..)` must fail to
//! compile, and the error must point at that parameter rather than at the
//! attribute — the whole point of selecting per parameter is a precise, local
//! diagnostic.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

struct NotDebug {
    _value: i32,
}

impl LocalResource for NotDebug {}

#[system(inspect(value))]
async fn inspect_non_debug(value: Res<NotDebug>) {
    let _ = &value;
}

fn main() {}
