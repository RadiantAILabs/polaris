//! An `impl Trait` return type cannot name a concrete `System::Output`, so a
//! system returning one is always rejected — pinned here so `inspect(return)`
//! on such a signature can never silently compile.

use polaris_system::system;

#[system(inspect(return))]
async fn opaque_return() -> impl std::fmt::Debug {
    42
}

fn main() {}
