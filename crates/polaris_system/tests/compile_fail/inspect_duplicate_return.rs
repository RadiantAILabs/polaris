//! A duplicated `return` in `inspect(..)` is reported rather than silently
//! deduplicated, matching the treatment of a duplicated parameter name.

use polaris_system::system;

#[system(inspect(return, return))]
async fn duplicate_return() -> i32 {
    7
}

fn main() {}
