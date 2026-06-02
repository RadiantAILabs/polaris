//! `#[plugin]` supplies the `ID` constant from its `id = "..."` argument, so omitting it
//! must be a hard error rather than a plugin with no identity.

use polaris_system::plugin;
use polaris_system::plugin::Plugin;
use polaris_system::server::Server;

struct NoId;

#[plugin(version = "0.1.0")]
impl Plugin for NoId {
    fn build(&self, _server: &mut Server) {}
}

fn main() {}
