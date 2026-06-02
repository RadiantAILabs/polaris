//! `provides(T)` derives the declared version from `T::CONTRACT_VERSION`, so `T` must
//! implement `Contract`. Declaring `provides(...)` for a type that does not must be
//! rejected.

use polaris_system::plugin;
use polaris_system::plugin::Plugin;
use polaris_system::server::Server;

struct NotACapability;

struct BadProvider;

#[plugin(id = "test::bad_provider", version = "0.1.0", provides(NotACapability))]
impl Plugin for BadProvider {
    fn build(&self, _server: &mut Server) {}
}

fn main() {}
