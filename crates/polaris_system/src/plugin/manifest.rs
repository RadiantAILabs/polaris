//! Resolved plugin manifest — the introspection surface for the capability graph.
//!
//! Built by the server at the end of [`finish()`](crate::server::Server::finish) and
//! returned from [`Server::plugin_manifest`](crate::server::Server::plugin_manifest). It
//! answers "what does this set of plugins provide, extend, and require, and in what
//! order were they resolved" — including which plugin a group pulled in — without reading
//! source.

use super::{Capability, CapabilityReq, PluginId, Version};
use std::fmt;

/// A fully resolved view of the plugin graph, in resolution (dependency) order.
#[derive(Debug, Clone, Default)]
pub struct PluginManifest {
    pub(crate) entries: Vec<PluginManifestEntry>,
}

impl PluginManifest {
    /// The manifest entries, in the order plugins were built.
    #[must_use]
    pub fn entries(&self) -> &[PluginManifestEntry] {
        &self.entries
    }

    /// Looks up a single plugin's manifest entry by id.
    #[must_use]
    pub fn entry(&self, id: &PluginId) -> Option<&PluginManifestEntry> {
        self.entries.iter().find(|entry| &entry.id == id)
    }

    /// Renders the manifest as a Graphviz DOT digraph of capability edges
    /// (provider → extender/requirer), suitable for `dot -Tsvg`.
    #[must_use]
    pub fn to_dot(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("digraph plugins {\n  rankdir=LR;\n");
        for entry in &self.entries {
            let _ = writeln!(out, "  \"{}\";", entry.id);
            for resolved in entry.extends.iter().chain(&entry.requires) {
                if let Some(provider) = &resolved.provider {
                    let _ = writeln!(
                        out,
                        "  \"{}\" -> \"{}\" [label=\"{}\"];",
                        provider,
                        entry.id,
                        resolved.req.name(),
                    );
                }
            }
        }
        out.push_str("}\n");
        out
    }
}

impl fmt::Display for PluginManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for entry in &self.entries {
            writeln!(f, "{} @ {}", entry.id, entry.version)?;
            for cap in &entry.provides {
                writeln!(f, "  provides {cap}")?;
            }
            for resolved in &entry.extends {
                writeln!(f, "  extends  {resolved}")?;
            }
            for resolved in &entry.requires {
                writeln!(f, "  requires {resolved}")?;
            }
            for resolved in &entry.optional {
                writeln!(f, "  optional {resolved}")?;
            }
        }
        Ok(())
    }
}

/// One plugin's resolved capabilities.
#[derive(Debug, Clone)]
pub struct PluginManifestEntry {
    /// The plugin's id.
    pub id: PluginId,
    /// The plugin's own version.
    pub version: Version,
    /// Capabilities this plugin provides.
    pub provides: Vec<Capability>,
    /// Capabilities this plugin extends, each resolved to its provider.
    pub extends: Vec<ResolvedReq>,
    /// Capabilities this plugin requires, each resolved to its provider.
    pub requires: Vec<ResolvedReq>,
    /// Optional requirements, resolved to a provider when one is present.
    pub optional: Vec<ResolvedReq>,
}

/// A capability requirement paired with the provider that satisfied it.
#[derive(Debug, Clone)]
pub struct ResolvedReq {
    /// The original requirement.
    pub req: CapabilityReq,
    /// The plugin that provides the capability, if any (always `None` for an
    /// unsatisfied optional requirement).
    pub provider: Option<PluginId>,
    /// The provider's contract version for this capability, if resolved.
    pub provider_version: Option<Version>,
}

impl fmt::Display for ResolvedReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.provider, self.provider_version) {
            (Some(provider), Some(version)) => {
                write!(f, "{} ← {provider} ({version})", self.req)
            }
            _ => write!(f, "{} ← (unresolved)", self.req),
        }
    }
}
