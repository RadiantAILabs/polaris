//! An unknown `#[system]` argument must be rejected at expansion with a
//! message naming the argument, rather than being silently ignored.

use polaris_system::system;

#[system(observe)]
async fn unknown_argument() {}

fn main() {}
