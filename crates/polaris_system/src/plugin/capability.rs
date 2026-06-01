//! Capability-based plugin dependencies.
//!
//! A plugin's *real* dependency is almost always a **resource or API type**
//! (`ModelRegistry`, `ToolRegistry`, `HttpRouter`), not another plugin. Declaring
//! dependencies on plugin names couples a consumer to one concrete provider and lets
//! the declaration drift away from the actual `get_resource_mut::<T>()` call it stands
//! in for. Capabilities fix that by making the resource/API type the unit of dependency.
//!
//! A plugin declares, via [`Plugin::access`](super::Plugin::access), three kinds of
//! relationship to a capability type `T`:
//!
//! - [`provides`](PluginAccess::provides) — it inserts a *new* `T`. Exactly one plugin
//!   may provide a given `T`.
//! - [`extends`](PluginAccess::extends) — it mutates a `T` that another plugin provided
//!   (the model-provider / decorator pattern). Many plugins may extend one `T`.
//! - [`requires`](PluginAccess::requires) / [`optionally_requires`](PluginAccess::optionally_requires)
//!   — it reads `T` to do its own work. `optionally_requires` degrades gracefully when no
//!   provider is present.
//!
//! The server resolver uses these declarations to order plugins
//! (provider before extenders and requirers), to validate that every required/extended
//! capability has a compatible provider, and to power introspection.

use super::Version;
use std::any::TypeId;
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// Contract
// ─────────────────────────────────────────────────────────────────────────────

/// A resource or API type that is exposed as a versioned capability.
///
/// The contract version belongs to the *type* — the stable public surface that
/// downstream plugins build against — not to whichever plugin happens to provide it.
/// Bump [`CONTRACT_VERSION`](Self::CONTRACT_VERSION) when the type's public API changes
/// incompatibly. Implementing this trait is what lets a type be used with the typed
/// build parameters [`Requires`](super::Requires), [`Extends`](super::Extends), and
/// [`Optional`](super::Optional), and with the `provides(...)` form of the `#[plugin]`
/// macro: the requirement / declaration version is derived from this constant, so a
/// consumer never restates it.
///
/// # Example
///
/// ```
/// use polaris_system::plugin::{Contract, Version};
///
/// struct ModelRegistry;
///
/// impl Contract for ModelRegistry {
///     const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
/// }
/// ```
pub trait Contract: 'static {
    /// The contract version at which this capability type is exposed.
    const CONTRACT_VERSION: Version;
}

// ─────────────────────────────────────────────────────────────────────────────
// VersionReq
// ─────────────────────────────────────────────────────────────────────────────

/// A version requirement against a capability's contract [`Version`].
///
/// Modeled as a half-open range `[min, max_exclusive)`. The common constructor
/// [`caret`](Self::caret) builds the Cargo-style `^` range, where the upper bound is the
/// next incompatible release per semantic versioning.
///
/// # Example
///
/// ```
/// use polaris_system::plugin::{Version, VersionReq};
///
/// let req = VersionReq::caret(Version::new(0, 2, 0)); // >=0.2.0, <0.3.0
/// assert!(req.matches(Version::new(0, 2, 5)));
/// assert!(!req.matches(Version::new(0, 3, 0)));
/// assert!(!req.matches(Version::new(0, 1, 9)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionReq {
    /// Lowest acceptable version (inclusive).
    min: Version,
    /// First version that is no longer acceptable (exclusive upper bound).
    max_exclusive: Version,
}

impl VersionReq {
    /// Builds a Cargo-style caret (`^`) requirement from `version`.
    ///
    /// The upper bound is the next version that bumps the left-most non-zero component:
    /// `^1.2.3` → `<2.0.0`, `^0.2.3` → `<0.3.0`, `^0.0.3` → `<0.0.4`.
    #[must_use]
    pub const fn caret(version: Version) -> Self {
        let max_exclusive = if version.major > 0 {
            Version::new(version.major + 1, 0, 0)
        } else if version.minor > 0 {
            Version::new(0, version.minor + 1, 0)
        } else {
            Version::new(0, 0, version.patch + 1)
        };
        Self {
            min: version,
            max_exclusive,
        }
    }

    /// Builds a requirement satisfied only by exactly `version`.
    #[must_use]
    pub const fn exact(version: Version) -> Self {
        Self {
            min: version,
            max_exclusive: Version::new(version.major, version.minor, version.patch + 1),
        }
    }

    /// Builds a requirement satisfied by any version (`>=min`, no upper bound).
    #[must_use]
    pub const fn at_least(min: Version) -> Self {
        Self {
            min,
            max_exclusive: Version::new(u64::MAX, u64::MAX, u64::MAX),
        }
    }

    /// Builds a requirement satisfied by any version at all.
    #[must_use]
    pub const fn any() -> Self {
        Self::at_least(Version::new(0, 0, 0))
    }

    /// Returns `true` if `provided` satisfies this requirement.
    #[must_use]
    pub fn matches(&self, provided: Version) -> bool {
        self.min <= provided && provided < self.max_exclusive
    }

    /// The inclusive lower bound.
    #[must_use]
    pub const fn min(&self) -> Version {
        self.min
    }

    /// The exclusive upper bound.
    #[must_use]
    pub const fn max_exclusive(&self) -> Version {
        self.max_exclusive
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ">={}, <{}", self.min, self.max_exclusive)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Capability
// ─────────────────────────────────────────────────────────────────────────────

/// A versioned capability *provided* by a plugin: a resource or API type plus the
/// contract [`Version`] at which the providing plugin exposes it.
///
/// Keyed by the Rust [`TypeId`] of the underlying type, so providers and consumers that
/// name the same type refer to the same capability without naming each other.
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    type_id: TypeId,
    name: &'static str,
    version: Version,
}

impl Capability {
    /// Declares the capability for type `T` at the given contract `version`.
    #[must_use]
    pub fn of<T: 'static>(version: Version) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            name: std::any::type_name::<T>(),
            version,
        }
    }

    /// The [`TypeId`] of the underlying resource/API type.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// The fully-qualified type name, used in errors and the manifest.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The contract version this provider exposes.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CapabilityReq
// ─────────────────────────────────────────────────────────────────────────────

/// A requirement *on* a capability: a resource or API type plus the [`VersionReq`] the
/// consumer needs the provider to satisfy. Used for `extends`, `requires`, and
/// `optionally_requires`.
#[derive(Debug, Clone, Copy)]
pub struct CapabilityReq {
    type_id: TypeId,
    name: &'static str,
    req: VersionReq,
}

impl CapabilityReq {
    /// Declares a requirement on type `T` satisfying `req`.
    #[must_use]
    pub fn of<T: 'static>(req: VersionReq) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            name: std::any::type_name::<T>(),
            req,
        }
    }

    /// The [`TypeId`] of the required resource/API type.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// The fully-qualified type name, used in errors and the manifest.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The version requirement on the provider's contract version.
    #[must_use]
    pub fn req(&self) -> VersionReq {
        self.req
    }
}

impl fmt::Display for CapabilityReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.req)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PluginAccess
// ─────────────────────────────────────────────────────────────────────────────

/// The capabilities a plugin provides, extends, and requires.
///
/// Returned from [`Plugin::access`](super::Plugin::access) and consumed by the server
/// resolver. Built with a fluent API:
///
/// ```
/// use polaris_system::plugin::{PluginAccess, Version, VersionReq};
///
/// # struct ModelRegistry;
/// # struct PersistenceApi;
/// let access = PluginAccess::new()
///     .extends::<ModelRegistry>(VersionReq::caret(Version::new(0, 1, 0)))
///     .optionally_requires::<PersistenceApi>(VersionReq::any());
/// assert_eq!(access.extended().len(), 1);
/// assert_eq!(access.optionals().len(), 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct PluginAccess {
    provides: Vec<Capability>,
    extends: Vec<CapabilityReq>,
    requires: Vec<CapabilityReq>,
    optional: Vec<CapabilityReq>,
}

impl PluginAccess {
    /// Creates an empty access declaration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares that this plugin **provides** type `T` at `version` (inserts a new `T`).
    #[must_use]
    pub fn provides<T: 'static>(mut self, version: Version) -> Self {
        self.provides.push(Capability::of::<T>(version));
        self
    }

    /// Declares that this plugin **extends** a `T` provided elsewhere (mutates it).
    #[must_use]
    pub fn extends<T: 'static>(mut self, req: VersionReq) -> Self {
        self.extends.push(CapabilityReq::of::<T>(req));
        self
    }

    /// Declares that this plugin **requires** `T` to be provided (reads it).
    #[must_use]
    pub fn requires<T: 'static>(mut self, req: VersionReq) -> Self {
        self.requires.push(CapabilityReq::of::<T>(req));
        self
    }

    /// Declares an **optional** requirement on `T` — used when present, skipped otherwise.
    #[must_use]
    pub fn optionally_requires<T: 'static>(mut self, req: VersionReq) -> Self {
        self.optional.push(CapabilityReq::of::<T>(req));
        self
    }

    /// The capabilities this plugin provides.
    #[must_use]
    pub fn provided(&self) -> &[Capability] {
        &self.provides
    }

    /// The capabilities this plugin extends.
    #[must_use]
    pub fn extended(&self) -> &[CapabilityReq] {
        &self.extends
    }

    /// The capabilities this plugin requires.
    #[must_use]
    pub fn required(&self) -> &[CapabilityReq] {
        &self.requires
    }

    /// The optional capability requirements.
    #[must_use]
    pub fn optionals(&self) -> &[CapabilityReq] {
        &self.optional
    }

    /// Returns `true` if no capabilities are declared (the default for legacy plugins).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.provides.is_empty()
            && self.extends.is_empty()
            && self.requires.is_empty()
            && self.optional.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapA;
    struct CapB;

    #[test]
    fn caret_upper_bounds() {
        assert_eq!(
            VersionReq::caret(Version::new(1, 2, 3)).max_exclusive(),
            Version::new(2, 0, 0)
        );
        assert_eq!(
            VersionReq::caret(Version::new(0, 2, 3)).max_exclusive(),
            Version::new(0, 3, 0)
        );
        assert_eq!(
            VersionReq::caret(Version::new(0, 0, 3)).max_exclusive(),
            Version::new(0, 0, 4)
        );
    }

    #[test]
    fn caret_matches() {
        let req = VersionReq::caret(Version::new(0, 2, 0));
        assert!(req.matches(Version::new(0, 2, 0)));
        assert!(req.matches(Version::new(0, 2, 9)));
        assert!(!req.matches(Version::new(0, 1, 9)));
        assert!(!req.matches(Version::new(0, 3, 0)));
    }

    #[test]
    fn exact_matches_only_one() {
        let req = VersionReq::exact(Version::new(1, 0, 0));
        assert!(req.matches(Version::new(1, 0, 0)));
        assert!(!req.matches(Version::new(1, 0, 1)));
        assert!(!req.matches(Version::new(0, 9, 9)));
    }

    #[test]
    fn capability_and_req_share_type_id() {
        let cap = Capability::of::<CapA>(Version::new(1, 0, 0));
        let req = CapabilityReq::of::<CapA>(VersionReq::any());
        assert_eq!(cap.type_id(), req.type_id());
        assert_ne!(
            cap.type_id(),
            CapabilityReq::of::<CapB>(VersionReq::any()).type_id()
        );
    }

    #[test]
    fn access_builder_collects() {
        let access = PluginAccess::new()
            .provides::<CapA>(Version::new(1, 0, 0))
            .extends::<CapB>(VersionReq::any())
            .requires::<CapA>(VersionReq::caret(Version::new(1, 0, 0)));
        assert_eq!(access.provided().len(), 1);
        assert_eq!(access.extended().len(), 1);
        assert_eq!(access.required().len(), 1);
        assert!(!access.is_empty());
    }
}
