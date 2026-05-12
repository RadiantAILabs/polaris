//! Validated newtypes for public-route configuration.
//!
//! [`AppConfig`](crate::AppConfig)'s public-route allowlist stores
//! [`PublicPath`] (exact match) and [`PublicPrefix`] (string-prefix match)
//! values instead of raw `String`s, so invalid inputs (empty, missing
//! leading `/`) cannot reach the request-matching middleware. Empty input
//! is the highest-risk case: an empty prefix makes every request public
//! because [`str::starts_with`] returns `true` for the empty needle.
//!
//! Construct via [`PublicPath::new`] / [`PublicPrefix::new`] (or
//! [`TryFrom`]) to handle invalid input as a [`PublicRouteError`], or via
//! [`AppConfig::with_public_path`](crate::AppConfig::with_public_path) /
//! [`with_public_prefix`](crate::AppConfig::with_public_prefix) which route
//! through the same constructors and panic on misconfiguration at startup.

use std::fmt;

/// Error returned when a [`PublicPath`] or [`PublicPrefix`] is constructed
/// from an invalid string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicRouteError {
    /// The supplied value was the empty string.
    Empty,
    /// The supplied value did not start with `/`.
    NoLeadingSlash(String),
    /// A [`PublicPrefix`] value did not end with `/`. A prefix without a
    /// trailing slash matches sibling paths (`"/dashboard"` matches
    /// `/dashboard-attack`), which is almost never what callers want — use
    /// [`PublicPath`] for an exact-match exemption instead.
    MissingTrailingSlash(String),
}

impl fmt::Display for PublicRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("must not be empty"),
            Self::NoLeadingSlash(value) => {
                write!(f, "must start with '/' (got {value:?})")
            }
            Self::MissingTrailingSlash(value) => write!(
                f,
                "prefix must end with '/' to avoid matching sibling paths \
                 (got {value:?}; use `with_public_path` for an exact-match exemption)"
            ),
        }
    }
}

impl std::error::Error for PublicRouteError {}

/// Validated exact request path exempt from [`AuthProvider`](crate::AuthProvider).
///
/// The wrapped string is guaranteed non-empty and to start with `/`.
/// Construct via [`PublicPath::new`] or [`TryFrom`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicPath(String);

impl PublicPath {
    /// Returns a `PublicPath` if `value` is non-empty and starts with `/`.
    ///
    /// # Errors
    ///
    /// - [`PublicRouteError::Empty`] when `value` is `""`.
    /// - [`PublicRouteError::NoLeadingSlash`] when `value` does not start with `/`.
    pub fn new(value: impl Into<String>) -> Result<Self, PublicRouteError> {
        let value = value.into();
        validate(&value)?;
        Ok(Self(value))
    }

    /// Borrows the validated path as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PublicPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublicPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for PublicPath {
    type Error = PublicRouteError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for PublicPath {
    type Error = PublicRouteError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Validated request-path prefix exempt from [`AuthProvider`](crate::AuthProvider).
///
/// The wrapped string is guaranteed non-empty, to start with `/`, and to
/// end with `/`. The trailing-slash requirement prevents the foot-gun
/// where `"/dashboard"` would also match `/dashboard-attack` — for an
/// exact-match exemption use [`PublicPath`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicPrefix(String);

impl PublicPrefix {
    /// Returns a `PublicPrefix` if `value` is non-empty, starts with `/`,
    /// and ends with `/`.
    ///
    /// # Errors
    ///
    /// - [`PublicRouteError::Empty`] when `value` is `""`. An empty prefix
    ///   would make every request public (`str::starts_with("")` is always
    ///   `true`), silently disabling [`AuthProvider`](crate::AuthProvider).
    /// - [`PublicRouteError::NoLeadingSlash`] when `value` does not start with `/`.
    /// - [`PublicRouteError::MissingTrailingSlash`] when `value` does not
    ///   end with `/`. Without the trailing slash, `"/dashboard"` would
    ///   match `/dashboard-attack` — use [`PublicPath`] for exact-match
    ///   exemptions.
    pub fn new(value: impl Into<String>) -> Result<Self, PublicRouteError> {
        let value = value.into();
        validate(&value)?;
        if !value.ends_with('/') {
            return Err(PublicRouteError::MissingTrailingSlash(value));
        }
        Ok(Self(value))
    }

    /// Borrows the validated prefix as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PublicPrefix {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PublicPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for PublicPrefix {
    type Error = PublicRouteError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for PublicPrefix {
    type Error = PublicRouteError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

fn validate(value: &str) -> Result<(), PublicRouteError> {
    if value.is_empty() {
        return Err(PublicRouteError::Empty);
    }
    if !value.starts_with('/') {
        return Err(PublicRouteError::NoLeadingSlash(value.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_path_accepts_valid_input() {
        let path = PublicPath::new("/healthz").unwrap();
        assert_eq!(path.as_str(), "/healthz");
        assert_eq!(path.to_string(), "/healthz");
        assert_eq!(<PublicPath as AsRef<str>>::as_ref(&path), "/healthz");
    }

    #[test]
    fn public_path_rejects_empty() {
        assert_eq!(PublicPath::new(""), Err(PublicRouteError::Empty));
    }

    #[test]
    fn public_path_rejects_missing_leading_slash() {
        assert_eq!(
            PublicPath::new("healthz"),
            Err(PublicRouteError::NoLeadingSlash("healthz".to_string()))
        );
    }

    #[test]
    fn public_prefix_accepts_valid_input() {
        let prefix = PublicPrefix::new("/dashboard/").unwrap();
        assert_eq!(prefix.as_str(), "/dashboard/");
        assert_eq!(prefix.to_string(), "/dashboard/");
    }

    #[test]
    fn public_prefix_rejects_empty() {
        assert_eq!(PublicPrefix::new(""), Err(PublicRouteError::Empty));
    }

    #[test]
    fn public_prefix_rejects_missing_leading_slash() {
        assert_eq!(
            PublicPrefix::new("dashboard"),
            Err(PublicRouteError::NoLeadingSlash("dashboard".to_string()))
        );
    }

    #[test]
    fn public_prefix_rejects_missing_trailing_slash() {
        assert_eq!(
            PublicPrefix::new("/dashboard"),
            Err(PublicRouteError::MissingTrailingSlash(
                "/dashboard".to_string()
            ))
        );
    }

    #[test]
    fn public_prefix_root_is_accepted() {
        // `/` satisfies both the leading- and trailing-slash requirement
        // (it is both). Allowing every request through is a valid choice
        // if the operator explicitly opts in.
        let prefix = PublicPrefix::new("/").unwrap();
        assert_eq!(prefix.as_str(), "/");
    }

    #[test]
    fn try_from_str_works() {
        let path: PublicPath = "/v1/auth/login".try_into().unwrap();
        let prefix: PublicPrefix = "/static/".try_into().unwrap();
        assert_eq!(path.as_str(), "/v1/auth/login");
        assert_eq!(prefix.as_str(), "/static/");
    }

    #[test]
    fn try_from_string_works() {
        let path: PublicPath = String::from("/healthz").try_into().unwrap();
        let prefix: PublicPrefix = String::from("/dashboard/").try_into().unwrap();
        assert_eq!(path.as_str(), "/healthz");
        assert_eq!(prefix.as_str(), "/dashboard/");
    }

    #[test]
    fn error_display_renders_clearly() {
        assert_eq!(PublicRouteError::Empty.to_string(), "must not be empty");
        assert_eq!(
            PublicRouteError::NoLeadingSlash("dashboard".to_string()).to_string(),
            "must start with '/' (got \"dashboard\")"
        );
        assert!(
            PublicRouteError::MissingTrailingSlash("/dashboard".to_string())
                .to_string()
                .contains("must end with '/'")
        );
    }
}
