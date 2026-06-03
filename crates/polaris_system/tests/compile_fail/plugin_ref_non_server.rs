//! A reference parameter on a plugin `build` is only treated as the `&Server` passthrough
//! when its referent is `Server`. Any other reference (here `&Config`) is routed through
//! `BuildParam`, so it must implement that trait — a stray non-capability reference is
//! rejected with a clear trait bound rather than silently binding to the server.

use polaris_system::plugin;
use polaris_system::plugin::Plugin;

struct Config;

struct BadPlugin;

#[plugin(id = "test::bad_plugin", version = "0.1.0")]
impl Plugin for BadPlugin {
    fn build(&self, _cfg: &Config) {}
}

fn main() {}
