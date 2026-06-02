//! A `build` parameter that is neither a raw `&Server`/`&mut Server` nor a `BuildParam`
//! (here a plain `String`) must be rejected — the macro fetches each typed parameter via
//! `BuildParam`, so an arbitrary type has no way to be resolved from the server.

use polaris_system::plugin;
use polaris_system::plugin::Plugin;

struct BadParam;

#[plugin(id = "test::bad_param", version = "0.1.0")]
impl Plugin for BadParam {
    fn build(&self, _name: String) {}
}

fn main() {}
