//! `#[plugin]` should accept a provider (imperative `&mut Server` insert plus a
//! `provides(...)` declaration) alongside consumers that declare their capability needs as
//! typed build parameters — `Extends<T>` (`&mut T`), `Requires<T>` (`&T`), and
//! `Optional<T>` (`Option<&T>`). All four forms must compile and produce a usable
//! `Plugin` impl.

use polaris_system::plugin;
use polaris_system::plugin::{Contract, Extends, Optional, Plugin, Requires, Version};
use polaris_system::server::Server;

struct Registry {
    value: i32,
}

impl Contract for Registry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

struct Provider;

#[plugin(id = "test::provider", version = "0.1.0", provides(Registry))]
impl Plugin for Provider {
    fn build(&self, server: &mut Server) {
        server.insert_resource(Registry { value: 0 });
    }
}

struct Extender;

#[plugin(id = "test::extender", version = "0.1.0")]
impl Plugin for Extender {
    fn build(&self, mut registry: Extends<Registry>) {
        registry.value += 1;
    }
}

struct Reader;

#[plugin(id = "test::reader", version = "0.1.0")]
impl Plugin for Reader {
    fn build(&self, registry: Requires<Registry>) {
        let _ = registry.value;
    }
}

struct OptionalReader;

#[plugin(id = "test::optional_reader", version = "0.1.0")]
impl Plugin for OptionalReader {
    fn build(&self, registry: Optional<Registry>) {
        let _ = registry.is_present();
    }
}

fn main() {
    let mut server = Server::new();
    server.add_plugins(Provider);
    server.add_plugins(Extender);
    server.add_plugins(Reader);
    server.add_plugins(OptionalReader);
}
