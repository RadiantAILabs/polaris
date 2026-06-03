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
    pub(crate) id: PluginId,
    pub(crate) version: Version,
    pub(crate) provides: Vec<Capability>,
    pub(crate) extends: Vec<ResolvedReq>,
    pub(crate) requires: Vec<ResolvedReq>,
    pub(crate) optional: Vec<ResolvedReq>,
}

impl PluginManifestEntry {
    /// The plugin's id.
    #[must_use]
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    /// The plugin's own version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Capabilities this plugin provides.
    #[must_use]
    pub fn provides(&self) -> &[Capability] {
        &self.provides
    }

    /// Capabilities this plugin extends, each resolved to its provider.
    #[must_use]
    pub fn extends(&self) -> &[ResolvedReq] {
        &self.extends
    }

    /// Capabilities this plugin requires, each resolved to its provider.
    #[must_use]
    pub fn requires(&self) -> &[ResolvedReq] {
        &self.requires
    }

    /// Optional requirements, resolved to a provider when one is present.
    #[must_use]
    pub fn optional(&self) -> &[ResolvedReq] {
        &self.optional
    }
}

/// A capability requirement paired with the provider that satisfied it.
#[derive(Debug, Clone)]
pub struct ResolvedReq {
    pub(crate) req: CapabilityReq,
    pub(crate) provider: Option<PluginId>,
    pub(crate) provider_version: Option<Version>,
}

impl ResolvedReq {
    /// The original requirement.
    #[must_use]
    pub fn req(&self) -> CapabilityReq {
        self.req
    }

    /// The plugin that provides the capability, if any (always `None` for an
    /// unsatisfied optional requirement).
    #[must_use]
    pub fn provider(&self) -> Option<&PluginId> {
        self.provider.as_ref()
    }

    /// The provider's contract version for this capability, if resolved.
    #[must_use]
    pub fn provider_version(&self) -> Option<Version> {
        self.provider_version
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::VersionReq;

    struct Registry;
    struct Persistence;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version::new(major, minor, patch)
    }

    /// A two-plugin manifest: a `provider` that provides `Registry`, and a `consumer`
    /// that extends `Registry`, requires `Registry`, and optionally requires the absent
    /// `Persistence`.
    fn sample_manifest() -> PluginManifest {
        let registry_cap = Capability::of::<Registry>(v(1, 2, 0));
        let registry_req = CapabilityReq::of::<Registry>(VersionReq::caret(v(1, 0, 0)));
        let persistence_req = CapabilityReq::of::<Persistence>(VersionReq::any());

        let provider = PluginManifestEntry {
            id: PluginId::new("test::provider"),
            version: v(1, 2, 0),
            provides: vec![registry_cap],
            extends: vec![],
            requires: vec![],
            optional: vec![],
        };
        let consumer = PluginManifestEntry {
            id: PluginId::new("test::consumer"),
            version: v(0, 1, 0),
            provides: vec![],
            extends: vec![ResolvedReq {
                req: registry_req,
                provider: Some(PluginId::new("test::provider")),
                provider_version: Some(v(1, 2, 0)),
            }],
            requires: vec![ResolvedReq {
                req: registry_req,
                provider: Some(PluginId::new("test::provider")),
                provider_version: Some(v(1, 2, 0)),
            }],
            optional: vec![ResolvedReq {
                req: persistence_req,
                provider: None,
                provider_version: None,
            }],
        };

        PluginManifest {
            entries: vec![provider, consumer],
        }
    }

    #[test]
    fn to_dot_emits_digraph_with_nodes_and_resolved_edges() {
        let dot = sample_manifest().to_dot();

        assert!(dot.starts_with("digraph plugins {"), "got: {dot}");
        assert!(dot.trim_end().ends_with('}'), "got: {dot}");
        // One node per plugin.
        assert!(dot.contains("\"test::provider\";"), "got: {dot}");
        assert!(dot.contains("\"test::consumer\";"), "got: {dot}");
        // A resolved extends/requires edge points provider → consumer, labelled with the
        // capability name. `extends` and `requires` both resolve to the provider, so the
        // edge appears twice.
        let edge = "\"test::provider\" -> \"test::consumer\"";
        assert_eq!(
            dot.matches(edge).count(),
            2,
            "expected one edge each for extends and requires, got: {dot}"
        );
        // The unsatisfied optional has no provider, so it contributes no edge.
        assert!(!dot.contains("Persistence"), "got: {dot}");
    }

    #[test]
    fn display_renders_each_capability_kind_including_unresolved_optional() {
        let rendered = sample_manifest().to_string();

        assert!(
            rendered.contains("test::provider @ 1.2.0"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("test::consumer @ 0.1.0"),
            "got: {rendered}"
        );
        assert!(rendered.contains("provides"), "got: {rendered}");
        assert!(rendered.contains("extends"), "got: {rendered}");
        assert!(rendered.contains("requires"), "got: {rendered}");
        // The absent optional renders through the `(unresolved)` branch.
        assert!(
            rendered.contains("optional") && rendered.contains("← (unresolved)"),
            "got: {rendered}"
        );
    }

    #[test]
    fn entry_lookup_finds_by_id_and_misses_unknown() {
        let manifest = sample_manifest();

        let entry = manifest
            .entry(&PluginId::new("test::consumer"))
            .expect("consumer entry present");
        assert_eq!(entry.requires().len(), 1);
        assert_eq!(
            entry.optional()[0].provider(),
            None,
            "unsatisfied optional has no provider"
        );

        assert!(manifest.entry(&PluginId::new("test::missing")).is_none());
    }
}
