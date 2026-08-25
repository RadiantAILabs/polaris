//! An attribute the macro cannot forward must be rejected: the function is
//! replaced by generated items, so the attribute would otherwise be silently
//! discarded.

use polaris_system::system;

#[system]
#[inline]
async fn with_inline() {}

fn main() {}
