//! Drift guard for the plugin capability graph.
//!
//! Collects the capability declarations (`provides` / `extends` / `requires` /
//! `optionally_requires`, each with its contract version) of a representative plugin set,
//! resolves every requirement to its provider, and serialises the result into a stable,
//! sorted form. The serialisation is compared against the checked-in
//! `examples/plugins.lock`. Any change to a plugin's declared capabilities, a contract
//! version, or the provider a requirement resolves to changes the serialisation and fails
//! this test — the capability-graph analog of the `tests/plugin_catalog.rs` documentation
//! drift guard.
//!
//! The graph is read straight from each plugin's [`Plugin::access`] declaration rather than
//! by running [`Server::finish`](polaris::system::server::Server::finish), so the guard is
//! hermetic (no TCP listener, no process-global `tracing` subscriber, no network) and
//! independent of feature flags: a plugin's capability *declarations* do not change with
//! features, even when its imperative `build()`/`ready()` wiring does. The same provider →
//! consumer ordering the server derives from these declarations is what this lock pins.
//!
//! ## Regenerating the lockfile
//!
//! When a capability change is intentional, regenerate the lock and commit it:
//!
//! ```bash
//! POLARIS_BLESS_PLUGINS_LOCK=1 cargo test -p examples --test plugins_lock
//! ```

use polaris::models::ModelsPlugin;
use polaris::shell::ShellPlugin;
use polaris::system::plugin::{Plugin, PluginAccess, PluginId, Version};
use polaris::tools::ToolsPlugin;
use std::any::TypeId;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const LOCK_FILE: &str = "plugins.lock";
const BLESS_ENV: &str = "POLARIS_BLESS_PLUGINS_LOCK";

/// One plugin's identity and capability declaration.
struct Entry {
    id: PluginId,
    version: Version,
    access: PluginAccess,
}

fn entry<P: Plugin>(plugin: &P) -> Entry {
    Entry {
        id: PluginId::of::<P>(),
        version: P::VERSION,
        access: plugin.access(),
    }
}

/// The representative plugin set whose capability graph the lockfile pins.
fn entries() -> Vec<Entry> {
    vec![
        entry(&ModelsPlugin),
        entry(&ToolsPlugin),
        entry(&ShellPlugin::with_working_dir("/tmp")),
    ]
}

/// Serialises the capability graph into a deterministic, sorted, order-independent form.
///
/// Resolution mirrors the server's resolver: a requirement on capability `T` is satisfied
/// by whichever entry `provides` `T`. The output depends only on the declarations, never on
/// the order entries are listed in.
fn serialize(entries: &[Entry]) -> String {
    // Build the provider map (capability TypeId → providing plugin + contract version),
    // exactly as the server's resolver does.
    let mut providers: HashMap<TypeId, (String, Version)> = HashMap::new();
    for entry in entries {
        for cap in entry.access.provided() {
            providers.insert(cap.type_id(), (entry.id.to_string(), cap.version()));
        }
    }

    let resolve = |req: &polaris::system::plugin::CapabilityReq| -> String {
        match providers.get(&req.type_id()) {
            Some((provider, version)) => format!("{req} ← {provider} ({version})"),
            None => format!("{req} ← (unresolved)"),
        }
    };

    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by_key(|entry| entry.id.to_string());

    let mut out = String::from(
        "# Resolved plugin capability graph. Regenerate with \
         `POLARIS_BLESS_PLUGINS_LOCK=1 cargo test -p examples --test plugins_lock`.\n",
    );

    for entry in sorted {
        let _ = writeln!(out, "{} @ {}", entry.id, entry.version);

        let mut lines: Vec<String> = entry
            .access
            .provided()
            .iter()
            .map(|cap| format!("  provides {cap}"))
            .collect();
        lines.sort();
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }

        for (label, reqs) in [
            ("extends ", entry.access.extended()),
            ("requires", entry.access.required()),
            ("optional", entry.access.optionals()),
        ] {
            let mut lines: Vec<String> = reqs
                .iter()
                .map(|req| format!("  {label} {}", resolve(req)))
                .collect();
            lines.sort();
            for line in &lines {
                let _ = writeln!(out, "{line}");
            }
        }
    }

    out
}

#[test]
fn capability_graph_matches_lockfile() {
    let actual = serialize(&entries());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(LOCK_FILE);

    if std::env::var(BLESS_ENV).is_ok() {
        fs::write(&path, &actual)
            .unwrap_or_else(|err| panic!("failed to write {}: {err}", path.display()));
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read {} ({err}); regenerate it with \
             `{BLESS_ENV}=1 cargo test -p examples --test plugins_lock`",
            path.display()
        )
    });

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "the plugin capability graph drifted from {LOCK_FILE}. If this change is \
         intentional, regenerate the lock with \
         `{BLESS_ENV}=1 cargo test -p examples --test plugins_lock` and commit it."
    );
}
