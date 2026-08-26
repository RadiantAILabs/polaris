//! Selecting `return` on a system whose return type is not `Debug` must fail
//! to compile, and the error must point at the return type rather than at the
//! attribute — the same precise, local diagnostic the parameter case gives.

use polaris_system::system;

struct NotDebug {
    _value: i32,
}

#[system(inspect(return))]
async fn produces_non_debug() -> NotDebug {
    NotDebug { _value: 1 }
}

fn main() {}
