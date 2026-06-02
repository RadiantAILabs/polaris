//! A typed build parameter may only name a capability type that implements `Contract`.
//! `Extends<NotACapability>` (no `Contract` impl) must be rejected — this is what keeps a
//! plugin from declaring an access to something that is not a versioned capability.

use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

struct NotACapability;

struct BadExtender;

#[plugin(id = "test::bad_extender", version = "0.1.0")]
impl Plugin for BadExtender {
    fn build(&self, _cap: Extends<NotACapability>) {}
}

fn main() {}
