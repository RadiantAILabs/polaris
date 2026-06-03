//! Typed build parameters — the build-phase dependency-injection surface for plugins.
//!
//! These mirror the [`SystemParam`](crate::param::SystemParam) types that a `#[system]`
//! async fn uses, but apply during a plugin's `build()` phase instead of at runtime. A
//! plugin's `build` declares what it consumes through its parameter list, exactly as a
//! system does — so the declaration cannot drift away from the access, and a plugin can
//! only obtain a reference to a capability `T` it actually declared:
//!
//! - [`Requires<T>`] — read a `T` another plugin provided (`&T`).
//! - [`Extends<T>`] — mutate a `T` another plugin provided (`&mut T`), the
//!   model-provider / decorator pattern.
//! - [`Optional<T>`] — read a `T` if some plugin provides it (`Option<&T>`), for
//!   graceful degradation.
//!
//! The version requirement is derived from the capability type's
//! [`Contract::CONTRACT_VERSION`] (a caret range), so a consumer never restates the
//! version at the call site. The server resolver guarantees, before any `build()` runs,
//! that a compatible provider exists and is built first — so [`BuildParam::fetch`] cannot
//! fail in a resolved server, and the `.expect("X must be added first")` panics that
//! hand-written `build()` bodies used to carry disappear.
//!
//! # Build-time access differs from runtime access
//!
//! During `build()` the server is still being assembled, so even a
//! [`GlobalResource`](crate::resource::GlobalResource) is held as a mutable resource and
//! can be extended. At runtime the same global is read-only behind an `Arc`. That is why
//! these parameters are distinct from [`Res`](crate::param::Res) /
//! [`ResMut`](crate::param::ResMut): they are the build-phase DI surface.

use super::capability::Contract;
use super::{PluginAccess, VersionReq};
use crate::resource::{Resource, ResourceRef, ResourceRefMut};
use crate::server::Server;
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// BuildParamError
// ─────────────────────────────────────────────────────────────────────────────

/// Why fetching a typed build parameter failed.
///
/// In a server whose capabilities resolved successfully this never occurs for a
/// non-optional parameter — the resolver proves a compatible provider exists and orders
/// it first. It exists so the generated `build()` can surface a precise, named message if
/// a capability is somehow absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildParamError {
    /// No resource of the requested type was present during `build()`.
    NotFound(&'static str),
}

impl fmt::Display for BuildParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(name) => {
                write!(f, "capability `{name}` was not provided during build")
            }
        }
    }
}

impl std::error::Error for BuildParamError {}

// ─────────────────────────────────────────────────────────────────────────────
// BuildParam
// ─────────────────────────────────────────────────────────────────────────────

/// A parameter a plugin's `build` can request, fetched from the server during the build
/// phase.
///
/// This is the plugin-build analog of [`SystemParam`](crate::param::SystemParam): it pairs
/// a [`fetch`](Self::fetch) that resolves the value against the [`Server`] with a
/// [`contribute_access`](Self::contribute_access) that records the corresponding
/// capability declaration into a [`PluginAccess`]. The `#[plugin]` macro calls both — the
/// first to bind the parameter, the second to derive `access()` — so the two can never
/// drift.
pub trait BuildParam {
    /// The fetched value, borrowing the server for `'w`.
    type Item<'w>;

    /// Resolves this parameter against `server` during the build phase.
    ///
    /// # Errors
    ///
    /// Returns [`BuildParamError`] if a required capability is missing. Optional
    /// parameters never error.
    fn fetch(server: &Server) -> Result<Self::Item<'_>, BuildParamError>;

    /// Records this parameter's capability declaration into `access`.
    fn contribute_access(access: &mut PluginAccess);
}

// ─────────────────────────────────────────────────────────────────────────────
// Requires
// ─────────────────────────────────────────────────────────────────────────────

/// Read access, during `build()`, to a capability `T` that another plugin provides.
///
/// Yields `&T` via [`Deref`](std::ops::Deref). Declaring this parameter is equivalent to
/// [`PluginAccess::requires`] at `caret(T::CONTRACT_VERSION)`: the resolver requires a
/// compatible provider and orders it before this plugin.
pub struct Requires<'w, T: Resource + Contract> {
    inner: ResourceRef<'w, T>,
}

impl<T: Resource + Contract> std::ops::Deref for Requires<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Resource + Contract> BuildParam for Requires<'_, T> {
    type Item<'w> = Requires<'w, T>;

    fn fetch(server: &Server) -> Result<Self::Item<'_>, BuildParamError> {
        match server.get_resource::<T>() {
            Some(inner) => Ok(Requires { inner }),
            None => Err(BuildParamError::NotFound(std::any::type_name::<T>())),
        }
    }

    fn contribute_access(access: &mut PluginAccess) {
        let taken = std::mem::take(access);
        *access = taken.requires::<T>(VersionReq::caret(T::CONTRACT_VERSION));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Extends
// ─────────────────────────────────────────────────────────────────────────────

/// Mutable access, during `build()`, to a capability `T` that another plugin provides.
///
/// Yields `&mut T` via [`DerefMut`](std::ops::DerefMut). This is the model-provider /
/// decorator pattern: the provider builds first and inserts `T`, then every extender
/// builds and mutates it. Declaring this parameter is equivalent to
/// [`PluginAccess::extends`] at `caret(T::CONTRACT_VERSION)`.
pub struct Extends<'w, T: Resource + Contract> {
    inner: ResourceRefMut<'w, T>,
}

impl<T: Resource + Contract> std::ops::Deref for Extends<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Resource + Contract> std::ops::DerefMut for Extends<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T: Resource + Contract> BuildParam for Extends<'_, T> {
    type Item<'w> = Extends<'w, T>;

    fn fetch(server: &Server) -> Result<Self::Item<'_>, BuildParamError> {
        match server.get_resource_mut::<T>() {
            Some(inner) => Ok(Extends { inner }),
            None => Err(BuildParamError::NotFound(std::any::type_name::<T>())),
        }
    }

    fn contribute_access(access: &mut PluginAccess) {
        let taken = std::mem::take(access);
        *access = taken.extends::<T>(VersionReq::caret(T::CONTRACT_VERSION));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Optional
// ─────────────────────────────────────────────────────────────────────────────

/// Optional read access, during `build()`, to a capability `T`.
///
/// Yields `Option<&T>` — `Some` when some plugin provides `T`, `None` otherwise — so a
/// plugin can degrade gracefully when an optional collaborator is absent. Declaring this
/// parameter is equivalent to [`PluginAccess::optionally_requires`]; a missing provider is
/// not an error, but an incompatible version still is.
pub struct Optional<'w, T: Resource + Contract> {
    inner: Option<ResourceRef<'w, T>>,
}

impl<'w, T: Resource + Contract> Optional<'w, T> {
    /// Returns the borrowed resource if a provider was present, otherwise `None`.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        self.inner.as_deref()
    }

    /// Returns `true` if a provider was present during build.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.inner.is_some()
    }
}

impl<T: Resource + Contract> BuildParam for Optional<'_, T> {
    type Item<'w> = Optional<'w, T>;

    fn fetch(server: &Server) -> Result<Self::Item<'_>, BuildParamError> {
        Ok(Optional {
            inner: server.get_resource::<T>(),
        })
    }

    fn contribute_access(access: &mut PluginAccess) {
        let taken = std::mem::take(access);
        *access = taken.optionally_requires::<T>(VersionReq::caret(T::CONTRACT_VERSION));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Version;

    struct Registry {
        value: u32,
    }

    impl Contract for Registry {
        const CONTRACT_VERSION: Version = Version::new(1, 0, 0);
    }

    #[test]
    fn requires_fetch_reads_a_present_provider() {
        let mut server = Server::new();
        server.insert_resource(Registry { value: 7 });

        let got = Requires::<Registry>::fetch(&server).expect("registry present");
        assert_eq!(got.value, 7);
    }

    #[test]
    fn requires_fetch_reports_not_found_when_absent() {
        let server = Server::new();

        // `Requires` is not `Debug` (it wraps a borrow guard), so match rather than
        // `expect_err`, which would require the `Ok` type to be `Debug`.
        let Err(err) = Requires::<Registry>::fetch(&server) else {
            panic!("expected NotFound for an absent registry");
        };
        assert!(matches!(err, BuildParamError::NotFound(_)), "got {err:?}");
        // The message names the missing capability type.
        assert!(err.to_string().contains("Registry"), "got {err}");
    }

    #[test]
    fn optional_fetch_is_some_when_present_and_none_when_absent() {
        let mut server = Server::new();

        // Absent: Optional resolves to None and never errors.
        let absent = Optional::<Registry>::fetch(&server).expect("optional never errors");
        assert!(!absent.is_present());
        assert!(absent.get().is_none());
        drop(absent);

        // Present: Optional yields the resource.
        server.insert_resource(Registry { value: 42 });
        let present = Optional::<Registry>::fetch(&server).expect("optional never errors");
        assert!(present.is_present());
        assert_eq!(present.get().map(|reg| reg.value), Some(42));
    }
}
