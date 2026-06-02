//! `#[plugin]` derives `access()` from the `build` method's parameters, so an impl block
//! with no `build` method has nothing to derive from and must be rejected.

use polaris_system::plugin;
use polaris_system::plugin::Plugin;
use polaris_system::server::Server;

struct NoBuild;

#[plugin(id = "test::no_build", version = "0.1.0")]
impl Plugin for NoBuild {
    async fn ready(&self, _server: &mut Server) {}
}

fn main() {}
