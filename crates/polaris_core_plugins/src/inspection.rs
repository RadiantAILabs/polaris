//! Runtime policy and listener registry for system parameter inspection.
//!
//! Layer 1 makes parameter values *capturable* (`#[system(inspect(..))]`) and
//! defines the [`InspectionSink`] delivery seam, but installs nothing and
//! stores nothing. This module is the plugin-layer half:
//!
//! - [`InspectionPlugin`] installs one fan-out sink on every graph run.
//! - [`InspectionPolicy`] decides at run time whether records flow — off by
//!   default, on globally, or narrowed to named systems — flippable through
//!   [`InspectionAPI`] without a rebuild.
//! - [`RedactionRules`] decide at run time which values are delivered as
//!   [`Inspection::Redacted`] instead of being formatted at all.
//! - [`InspectionSinkRegistry`] is the sign-up sheet: any plugin contributes a
//!   listener during its `build()` and each listener receives every record
//!   the policy admits. A listener registered under a name can be switched
//!   off and on at run time. The framework itself stores none of them.
//! - [`TracingInspectionSink`] is the shipped listener (registered under
//!   [`INSPECTION_TRACING_LISTENER`]): it forwards records onto the ambient `tracing`
//!   span, so they reach OpenTelemetry (via
//!   [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin)) already correlated
//!   to the surrounding step, run, and session.
//!
//! There is deliberately no in-framework buffer of past records and no HTTP
//! query surface — the registry is an export boundary, the same shape as
//! [`SpanProcessorRegistry`](crate::SpanProcessorRegistry).

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};

use parking_lot::{Mutex, RwLock};
use polaris_graph::MiddlewareAPI;
use polaris_system::api::API;
use polaris_system::param::inspect::{Inspection, InspectionSink, ParamMeta};
use polaris_system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris_system::server::Server;

// ─────────────────────────────────────────────────────────────────────────────
// InspectionPolicy
// ─────────────────────────────────────────────────────────────────────────────

/// Decides at run time whether a captured record is delivered to listeners.
///
/// This is the activation axis of inspection: `#[system(inspect(..))]` fixes
/// at compile time which parameters *can* be recorded, and the policy decides
/// per record whether delivery actually happens. The default is [`Off`]: a
/// server with [`InspectionPlugin`] registered records nothing until the
/// policy is changed through [`InspectionAPI`].
///
/// Narrowing matches on [`ParamMeta::system`] — the `#[system]` function name.
/// Two graph nodes running the same system function are indistinguishable to
/// the policy, because a record identifies its system, not its node.
///
/// There is no session, run, or tenant axis either — see
/// [`InspectionAPI`](InspectionAPI#scope-of-a-policy-change) for what that
/// means for who else's values a policy change picks up.
///
/// Marked `#[non_exhaustive]`: further modes may be added, so downstream
/// matches must include a wildcard arm.
///
/// [`Off`]: InspectionPolicy::Off
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionPolicy {
    /// No records are delivered and no value is formatted. The default.
    #[default]
    Off,
    /// Every captured record is delivered.
    All,
    /// Only records from the named systems are delivered.
    ///
    /// Names are `#[system]` function names, as carried by
    /// [`ParamMeta::system`].
    ///
    /// The variant carries its own `#[non_exhaustive]` on top of the enum's:
    /// `#[non_exhaustive]` on an enum seals the *set of variants*, not the
    /// payload inside one, so without it the `HashSet<String>` would be part
    /// of the public contract and could not be swapped for a better lookup
    /// structure without a breaking release. Build the variant with
    /// [`systems`](Self::systems), ask it questions with
    /// [`allows`](Self::allows), and read it back with
    /// [`narrowed_systems`](Self::narrowed_systems).
    #[non_exhaustive]
    Systems(HashSet<String>),
}

impl InspectionPolicy {
    /// Builds a [`Systems`](Self::Systems) policy from system names.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_core_plugins::InspectionPolicy;
    ///
    /// let policy = InspectionPolicy::systems(["plan", "act"]);
    /// assert!(policy.allows("plan"));
    /// assert!(!policy.allows("observe"));
    /// ```
    #[must_use]
    pub fn systems(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Systems(names.into_iter().map(Into::into).collect())
    }

    /// Whether records from `system` are delivered under this policy.
    ///
    /// Matching is on the `#[system]` function name, so this answers "is this
    /// system being recorded", not "is this particular value safe to render" —
    /// that second question belongs to [`RedactionRules`].
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_core_plugins::InspectionPolicy;
    ///
    /// assert!(!InspectionPolicy::Off.allows("plan"));
    /// assert!(InspectionPolicy::All.allows("plan"));
    ///
    /// let narrowed = InspectionPolicy::systems(["plan"]);
    /// assert!(narrowed.allows("plan"));
    /// assert!(!narrowed.allows("act"));
    ///
    /// // An empty narrowing denies everything.
    /// assert!(!InspectionPolicy::systems(Vec::<String>::new()).allows("plan"));
    /// ```
    #[must_use]
    pub fn allows(&self, system: &str) -> bool {
        match self {
            Self::Off => false,
            Self::All => true,
            Self::Systems(names) => names.contains(system),
        }
    }

    /// The systems a narrowed policy admits, or `None` when it does not narrow.
    ///
    /// The read side of [`Systems`](Self::Systems), whose payload is sealed:
    /// an operator surface that lists what is currently being recorded reads it
    /// through this rather than destructuring the variant, which is what lets
    /// the payload change without a breaking release. [`Off`](Self::Off) and
    /// [`All`](Self::All) both answer `None` — neither names systems — so pair
    /// this with [`allows`](Self::allows) rather than treating `None` as "records
    /// nothing".
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_core_plugins::InspectionPolicy;
    ///
    /// let policy = InspectionPolicy::systems(["plan", "act"]);
    /// let mut named: Vec<&str> = policy
    ///     .narrowed_systems()
    ///     .expect("a narrowed policy names its systems")
    ///     .collect();
    /// named.sort_unstable();
    /// assert_eq!(named, ["act", "plan"]);
    ///
    /// // Neither unnarrowed mode names systems.
    /// assert!(InspectionPolicy::All.narrowed_systems().is_none());
    /// assert!(InspectionPolicy::Off.narrowed_systems().is_none());
    /// ```
    #[must_use]
    pub fn narrowed_systems(&self) -> Option<impl Iterator<Item = &str>> {
        match self {
            Self::Systems(names) => Some(names.iter().map(String::as_str)),
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RedactionRules
// ─────────────────────────────────────────────────────────────────────────────

/// Values that are delivered as [`Inspection::Redacted`] instead of being
/// formatted.
///
/// [`InspectionPolicy`] decides *which systems* are recorded; this decides
/// *which values within them* are safe to render. The two are separate because
/// they answer different questions, and because a value worth withholding is
/// usually worth withholding everywhere rather than in one system.
///
/// A covered value is **never formatted**: the fan-out substitutes
/// [`Inspection::Redacted`] without calling the render closure, so a credential
/// never reaches a `String` — not even a truncated one — let alone a listener.
///
/// Matching is against the names a record already carries:
///
/// | Rule | Matches | Use when |
/// |------|---------|----------|
/// | [`redact_param`](Self::redact_param) | [`ParamMeta::param`] — the binding name, exactly | one named binding is sensitive |
/// | [`redact_type`](Self::redact_type) | [`ParamMeta::type_name`] — the declared type, path-stripped, whole spelling or any type named inside it | a whole type is sensitive wherever it appears |
///
/// [`ParamMeta::type_name`] records the spelling declared at each parameter, so
/// the same resource arrives spelled several ways: `ApiCredentials` from one
/// system, `credentials::ApiCredentials` from another, and
/// `Option<ApiCredentials>` or `Vec<ApiCredentials>` from a parameter that
/// declares it inside a container. A rule compared against that spelling as a
/// whole would silently miss every variant — fail-open, for exactly the values
/// most worth withholding — so type matching is deliberately generous in two
/// directions:
///
/// - **Paths are stripped** on both sides: every path collapses to its final
///   segment and whitespace that separates nothing disappears
///   (`credentials::ApiCredentials` → `ApiCredentials`, `Vec<token::Token>` →
///   `Vec<Token>`, `nested :: Deep` → `Deep`).
/// - **Matching descends into the spelling**: a rule matches the whole
///   path-stripped spelling *or* any type named within it, so a rule on
///   `ApiCredentials` covers `Option<ApiCredentials>`,
///   `Vec<ApiCredentials>`, `Box<dyn ApiCredentials>`, and any wrapper the
///   macro could not classify.
///
/// Both directions over-match rather than under-match: two distinct types
/// sharing a final segment are both covered by that segment's rule, and a rule
/// on `String` covers every spelling that mentions one. For a control that
/// withholds values, over-matching is the safe direction.
///
/// Prefer [`redact_type`](Self::redact_type) for credential-bearing types: it
/// follows the type into systems added later, under whatever path spelling or
/// container they declare, whereas a binding name has to be remembered at every
/// call site. Name the sensitive type itself rather than a container spelling
/// (`ApiCredentials`, not `Vec<ApiCredentials>`) — a rule naming a container
/// matches that spelling whole, so it would not cover the same container nested
/// one level deeper. One spelling stays out of reach: a type **alias** records
/// the alias itself (see [`ParamMeta::type_name`]), which no rule on the
/// underlying name can see. For a value that must never render anywhere, the
/// robust guarantee is a hand-written masking `Debug` impl on the type — the
/// pipeline renders through it.
///
/// # What a rule cannot see
///
/// Matching is on the record's **metadata** — the binding name
/// ([`ParamMeta::param`]) and the declared type
/// ([`ParamMeta::type_name`]) — never on the value. A rule therefore only ever
/// matches a type some parameter *declares*; it cannot follow a type reached
/// through another value's [`Debug`].
///
/// So for a system taking `Res<AppConfig>`, where `AppConfig` holds an
/// `ApiCredentials` field, the record carries `type_name = "AppConfig"` and
/// `redact_type("ApiCredentials")` is **inert** — `AppConfig`'s derived `Debug`
/// prints the nested credential in full. The generosity described above widens
/// which *spellings of a declared type* match; it does not reach inside a
/// value. Two controls do:
///
/// - Do not name the parameter in `inspect(..)` — nothing is captured, so
///   nothing can leak.
/// - Give the inner type a hand-written masking `Debug`, which every rendering
///   of every enclosing type goes through.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::RedactionRules;
///
/// let redactions = RedactionRules::new()
///     .redact_type("ApiCredentials")
///     .redact_param("token");
///
/// assert!(redactions.covers_type("ApiCredentials"));
/// // Path spelling does not matter: types match on the final path segment.
/// assert!(redactions.covers_type("credentials::ApiCredentials"));
/// // Nor does a container the parameter declared the type inside.
/// assert!(redactions.covers_type("Option<ApiCredentials>"));
/// assert!(redactions.covers_param("token"));
/// assert!(!redactions.covers_param("memory"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactionRules {
    /// Binding names withheld wherever they appear.
    params: HashSet<String>,
    /// Inner resource type names withheld wherever they appear.
    types: HashSet<String>,
}

impl RedactionRules {
    /// Withholds nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Withholds any parameter bound to `name`, in every system.
    #[must_use]
    pub fn redact_param(mut self, name: impl Into<String>) -> Self {
        self.params.insert(name.into());
        self
    }

    /// Withholds every parameter whose declared type is `name` or names it
    /// inside a container.
    ///
    /// The name is matched against [`ParamMeta::type_name`] — the inner type
    /// as declared at the parameter (`Memory`, not `ResMut<'_, Memory>`) —
    /// with paths stripped on both sides and matching descending into the
    /// spelling, so one rule covers the type however each parameter spells its
    /// path and whatever container it declares it inside. An alias records the
    /// alias: no rule on the underlying name can match it, so a type that must
    /// never render should mask in its own `Debug` impl instead.
    #[must_use]
    pub fn redact_type(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        let name = match strip_type_paths(&name) {
            Cow::Borrowed(_) => name,
            Cow::Owned(stripped) => stripped,
        };
        self.types.insert(name);
        self
    }

    /// Whether any rule is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.params.is_empty() && self.types.is_empty()
    }

    /// Whether a binding named `param` is withheld.
    #[must_use]
    pub fn covers_param(&self, param: &str) -> bool {
        self.params.contains(param)
    }

    /// Whether the resource type `type_name` is withheld.
    ///
    /// Compared path-stripped, like the rules themselves, and matched against
    /// both the whole spelling and every type named within it, so a rule on
    /// `Token` covers a parameter declared as `Option<token::Token>`.
    #[must_use]
    pub fn covers_type(&self, type_name: &str) -> bool {
        if self.types.is_empty() {
            return false;
        }
        self.types.contains(type_name)
            || self
                .types
                .iter()
                .any(|rule| type_spellings_match(type_name, rule))
            || type_tokens(type_name).any(|token| match token {
                TypeToken::Ident(ident) => self.types.contains(ident),
                TypeToken::Punct(_) => false,
            })
    }

    /// Whether this record's value must be withheld.
    fn covers(&self, meta: &ParamMeta) -> bool {
        self.covers_param(meta.param) || self.covers_type(meta.type_name)
    }

    /// The rules in force here that `next` does not carry, labelled for a
    /// diagnostic.
    ///
    /// Replacing a rule set is the one edit that can *un*-withhold, so
    /// [`InspectionAPI`] reports what a replacement dropped rather than
    /// letting a control disappear quietly. Sorted, because `HashSet` iteration
    /// order is arbitrary and a warning that reorders between runs is one
    /// nobody can diff or assert on.
    fn rules_absent_from(&self, next: &Self) -> Vec<String> {
        let mut dropped: Vec<String> = self
            .params
            .difference(&next.params)
            .map(|name| format!("param `{name}`"))
            .chain(
                self.types
                    .difference(&next.types)
                    .map(|name| format!("type `{name}`")),
            )
            .collect();
        dropped.sort();
        dropped
    }
}

/// One meaningful token in a declared type spelling.
///
/// Paths are collapsed while iterating: `credentials::ApiCredentials` yields
/// only `ApiCredentials`. Punctuation is retained so whole-spelling comparison
/// can distinguish `Vec<Token>` from `Option<Token>` without allocating a
/// normalized `String` on the record path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeToken<'a> {
    /// A path's final identifier segment.
    Ident(&'a str),
    /// Non-whitespace punctuation from the spelling.
    Punct(char),
}

/// Allocation-free iterator over a declared type's normalized tokens.
struct TypeTokens<'a> {
    spelling: &'a str,
    offset: usize,
}

impl<'a> TypeTokens<'a> {
    /// Advances past whitespace and returns the next character, if any.
    fn next_char(&self) -> Option<char> {
        self.spelling[self.offset..].chars().next()
    }

    /// Skips every whitespace character at the current offset.
    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.next_char() {
            if !ch.is_whitespace() {
                break;
            }
            self.offset += ch.len_utf8();
        }
    }

    /// Consumes one identifier and returns its byte range.
    fn take_ident(&mut self) -> std::ops::Range<usize> {
        let start = self.offset;
        while let Some(ch) = self.next_char() {
            if !is_ident_char(ch) {
                break;
            }
            self.offset += ch.len_utf8();
        }
        start..self.offset
    }
}

impl<'a> Iterator for TypeTokens<'a> {
    type Item = TypeToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.skip_whitespace();
            let ch = self.next_char()?;

            if is_ident_char(ch) {
                let mut ident = self.take_ident();
                loop {
                    self.skip_whitespace();
                    if !self.spelling[self.offset..].starts_with("::") {
                        return Some(TypeToken::Ident(&self.spelling[ident]));
                    }
                    self.offset += 2;
                    self.skip_whitespace();
                    if self.next_char().is_none_or(|next| !is_ident_char(next)) {
                        return Some(TypeToken::Ident(&self.spelling[ident]));
                    }
                    ident = self.take_ident();
                }
            }

            if self.spelling[self.offset..].starts_with("::") {
                // A leading path separator has no semantic token of its own.
                self.offset += 2;
                continue;
            }

            self.offset += ch.len_utf8();
            return Some(TypeToken::Punct(ch));
        }
    }
}

/// The path-stripped tokens in `spelling`, produced without allocation.
fn type_tokens(spelling: &str) -> TypeTokens<'_> {
    TypeTokens {
        spelling,
        offset: 0,
    }
}

/// Whether two type spellings normalize to the same token sequence.
fn type_spellings_match(left: &str, right: &str) -> bool {
    type_tokens(left).eq(type_tokens(right))
}

/// Whether `ch` can appear in a Rust identifier.
///
/// The boundary [`type_tokens`] splits on and [`strip_type_paths`] preserves
/// whitespace at: two identifier characters with a gap between them are two
/// names, and closing that gap would forge a third that is neither.
fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Reduces a declared type spelling to its path-stripped form: every path
/// collapses to its final segment, and whitespace is dropped except where it
/// separates two identifiers (`nested::Deep` → `Deep`, `Vec<token::Token>` →
/// `Vec<Token>`, `Vec< Token >` → `Vec<Token>`, `Box<dyn Credentials>`
/// unchanged).
///
/// [`ParamMeta::type_name`] is invariant across wrapper and lifetime forms but
/// not across path spellings, so [`RedactionRules`] compares rules and records in
/// this form — matching the declared spelling exactly would let a parameter
/// that imports the type under a different path slip past the rule, the
/// fail-open direction a withholding control must not have.
///
/// Whitespace is where that stripping can turn against itself. Dropping it
/// unconditionally merges the identifiers it separated: `Box<dyn Credentials>`
/// would normalize to `Box<dynCredentials>`, and `Credentials` — the name the
/// rule is written against — then appears in neither the whole spelling nor
/// [`type_tokens`]' descent, so the value renders. That is the same fail-open
/// as an unstripped path, reached from the other direction, so a run of
/// whitespace *between two identifier characters* collapses to one space and
/// every other run disappears. `&'a Token` and `mut Secret` keep their names
/// apart for the same reason.
///
/// Borrows when the name needs no stripping, so the common plain spelling
/// costs no allocation on the record path.
fn strip_type_paths(name: &str) -> Cow<'_, str> {
    if !name
        .bytes()
        .any(|byte| byte == b':' || byte.is_ascii_whitespace())
    {
        return Cow::Borrowed(name);
    }
    let mut out = String::with_capacity(name.len());
    // Byte offset in `out` where the identifier currently being copied began;
    // `::` truncates back to it, discarding the path segment just copied.
    let mut ident_start = 0;
    let mut in_ident = false;
    // Whitespace seen since the last character was copied, not yet resolved:
    // whether it separated two identifiers is only knowable once the next
    // non-whitespace character arrives.
    let mut pending_space = false;
    let mut chars = name.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            in_ident = false;
            pending_space = true;
            continue;
        }
        if ch == ':' && chars.peek() == Some(&':') {
            chars.next();
            out.truncate(ident_start);
            in_ident = false;
            // A path separator subsumes the gap around it: `nested :: Deep`
            // is one path, not two names.
            pending_space = false;
            continue;
        }
        let ident_char = is_ident_char(ch);
        if pending_space {
            pending_space = false;
            // Only a gap with an identifier character on both sides is
            // load-bearing; `Vec< Token >` loses its spaces to punctuation.
            if ident_char && out.chars().next_back().is_some_and(is_ident_char) {
                out.push(' ');
            }
        }
        if ident_char {
            if !in_ident {
                ident_start = out.len();
                in_ident = true;
            }
            out.push(ch);
        } else {
            out.push(ch);
            ident_start = out.len();
            in_ident = false;
        }
    }
    Cow::Owned(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Spec parsing
// ─────────────────────────────────────────────────────────────────────────────

/// Why an inspection spec string was rejected.
///
/// Produced by the [`FromStr`] impls on [`InspectionPolicy`] and
/// [`RedactionRules`]. Parsing rejects loudly instead of narrowing to a guess,
/// because each control's silent failure mode is the one it exists to prevent:
/// a policy typo that quietly matched no system would be a debugging trap, and
/// a redaction typo that quietly withheld nothing would leak exactly the
/// values the rule was written to withhold.
///
/// Marked `#[non_exhaustive]`: the grammars may grow, so downstream matches
/// must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionSpecError {
    /// A policy spec was punctuation only (for example `","`), naming no
    /// systems. Spell "record nothing" as `off` (or an empty spec) instead.
    EmptyPolicy,
    /// A policy entry is not spelled like a `#[system]` function name.
    ///
    /// The usual cause is a forgotten comma: `"plan act"` arrives as one
    /// entry. A name that matched no system would merely record nothing, but
    /// that silence is indistinguishable from "the system never ran" — so the
    /// spelling is checked where it can be, at the parse.
    ///
    /// The variant carries its own `#[non_exhaustive]` on top of the enum's:
    /// that seals the payload, not just the variant set, so a field (say, an
    /// entry offset) can be added without a breaking release.
    #[non_exhaustive]
    MalformedSystem {
        /// The offending entry, verbatim.
        entry: String,
    },
    /// A redaction entry was not `param:<binding>` or `type:<Type>`.
    ///
    /// The kind prefix is mandatory: a bare name would have to be guessed at,
    /// and a guessed-wrong rule (or a misspelled kind silently ignored) is a
    /// withholding control that withholds nothing.
    ///
    /// `#[non_exhaustive]` on the variant seals the payload — see
    /// [`MalformedSystem`](Self::MalformedSystem).
    #[non_exhaustive]
    MalformedRedaction {
        /// The offending entry, verbatim.
        entry: String,
    },
}

impl std::fmt::Display for InspectionSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPolicy => f.write_str(
                "policy spec names no systems; use `off`, `all`, \
                 or a comma-separated list of `#[system]` function names",
            ),
            Self::MalformedSystem { entry } => write!(
                f,
                "malformed system name `{entry}`: expected a bare `#[system]` \
                 function name (did you forget a comma?)"
            ),
            Self::MalformedRedaction { entry } => write!(
                f,
                "malformed redaction `{entry}`: expected `param:<binding>` or \
                 `type:<Type>` (a container spelling like `Vec<Token>` is not \
                 accepted here — name the inner type, which covers every \
                 container it appears in)"
            ),
        }
    }
}

impl std::error::Error for InspectionSpecError {}

/// Whether `name` is spelled like one Rust identifier.
fn is_ident(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_alphanumeric())
}

/// Whether `name` is spelled like a bare or `::`-qualified identifier.
fn is_path_ident(name: &str) -> bool {
    !name.is_empty() && name.split("::").all(is_ident)
}

/// Parses a policy spec: `off` (or an empty string), `all`, or a
/// comma-separated list of `#[system]` function names.
///
/// This is the same policy the typed constructors build, in a form that can
/// live in an environment variable or an admin request — see
/// [`InspectionPlugin::with_policy_from_env`]. Keywords are matched
/// ASCII-case-insensitively and take precedence when the spec is exactly one
/// of them; system names are case-sensitive identifiers, entries are trimmed,
/// and empty entries (a trailing comma) are ignored.
///
/// # Errors
///
/// [`InspectionSpecError::MalformedSystem`] for an entry not spelled like an
/// identifier — rejected rather than carried as a name that would silently
/// match nothing — and [`InspectionSpecError::EmptyPolicy`] for a spec that is
/// punctuation only, naming no systems.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::InspectionPolicy;
///
/// let policy: InspectionPolicy = "plan,act".parse().expect("two system names");
/// assert!(policy.allows("plan"));
/// assert!(!policy.allows("observe"));
///
/// assert_eq!("off".parse(), Ok(InspectionPolicy::Off));
/// assert_eq!("ALL".parse(), Ok(InspectionPolicy::All));
///
/// // A forgotten comma is an error, not a name that matches nothing.
/// assert!("plan act".parse::<InspectionPolicy>().is_err());
/// ```
impl FromStr for InspectionPolicy {
    type Err = InspectionSpecError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("off") {
            return Ok(Self::Off);
        }
        if spec.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        let mut names = Vec::new();
        for entry in spec.split(',').map(str::trim) {
            if entry.is_empty() {
                continue;
            }
            if !is_ident(entry) {
                return Err(InspectionSpecError::MalformedSystem {
                    entry: entry.to_owned(),
                });
            }
            names.push(entry);
        }
        if names.is_empty() {
            return Err(InspectionSpecError::EmptyPolicy);
        }
        Ok(Self::systems(names))
    }
}

/// Parses a redaction spec: comma-separated `param:<binding>` and
/// `type:<Type>` entries. An empty string is a valid spec with no rules.
///
/// This is the same rule set the typed builders compose, in a form that can
/// live in an environment variable — see
/// [`InspectionPlugin::with_redactions_from_env`]. Entries and the two halves
/// around the `:` are trimmed. Type names may be `::`-qualified
/// (`type:credentials::ApiCredentials`) and are path-stripped exactly as
/// [`redact_type`](RedactionRules::redact_type) strips them.
///
/// # Errors
///
/// [`InspectionSpecError::MalformedRedaction`] for any entry that is not
/// `param:<binding>` or `type:<Type>` — a bare name, a misspelled kind, or a
/// *container* spelling (`type:Vec<Token>`): commas inside generics are
/// indistinguishable from entry separators, and the typed API's own guidance
/// is to name the inner type, which covers every container it appears in. A
/// rule that genuinely must name a container spelling whole goes through
/// [`redact_type`](RedactionRules::redact_type).
///
/// # Example
///
/// ```
/// use polaris_core_plugins::RedactionRules;
///
/// let rules: RedactionRules = "param:token, type:ApiCredentials"
///     .parse()
///     .expect("one binding rule, one type rule");
/// assert!(rules.covers_param("token"));
/// assert!(rules.covers_type("Option<ApiCredentials>"));
///
/// // A misspelled kind is an error, not a rule that withholds nothing.
/// assert!("parm:token".parse::<RedactionRules>().is_err());
/// ```
impl FromStr for RedactionRules {
    type Err = InspectionSpecError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut rules = Self::new();
        for entry in spec.split(',').map(str::trim) {
            if entry.is_empty() {
                continue;
            }
            let malformed = || InspectionSpecError::MalformedRedaction {
                entry: entry.to_owned(),
            };
            let (kind, name) = entry.split_once(':').ok_or_else(malformed)?;
            let name = name.trim();
            rules = match kind.trim() {
                "param" if is_ident(name) => rules.redact_param(name),
                "type" if is_path_ident(name) => rules.redact_type(name),
                _ => return Err(malformed()),
            };
        }
        Ok(rules)
    }
}

/// Reads `var`, distinguishing "unset" (`None`) from unusable.
///
/// # Panics
///
/// Panics if the variable is set but not valid Unicode: a spec that cannot
/// even be read must fail the boot, for the same reason a spec that cannot be
/// parsed does.
fn env_spec(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(spec) => Some(spec),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("environment variable {var} is set but is not valid Unicode")
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Listener registration
// ─────────────────────────────────────────────────────────────────────────────

/// Stable identity of a named parameter-inspection listener.
///
/// Listener identities are static plugin metadata rather than runtime input:
/// requiring a typed `&'static str` keeps them distinct from tracing targets,
/// prevents unbounded operator-supplied names, and lets [`InspectionAPI`]
/// expose the exact registered set for checked toggles.
///
/// # Example
///
/// ```
/// use polaris_core_plugins::InspectionListenerName;
///
/// const AUDIT_LISTENER: InspectionListenerName =
///     InspectionListenerName::new("my_plugin::audit");
/// assert_eq!(AUDIT_LISTENER.as_str(), "my_plugin::audit");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InspectionListenerName(&'static str);

impl InspectionListenerName {
    /// Creates a static listener identity.
    ///
    /// # Panics
    ///
    /// Panics when `name` is empty or longer than 128 bytes. Listener names are
    /// plugin metadata, so invalid constants should fail at build time rather
    /// than enter the runtime registry.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        assert!(
            !name.is_empty(),
            "inspection listener names cannot be empty"
        );
        assert!(
            name.len() <= 128,
            "inspection listener names cannot exceed 128 bytes"
        );
        Self(name)
    }

    /// Returns the listener's static string spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for InspectionListenerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl AsRef<str> for InspectionListenerName {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// Name of the tracing listener [`InspectionPlugin`] registers by default.
///
/// Pass it to [`InspectionAPI::disable_listener`] to stop rendered values
/// reaching the tracing subscriber (and therefore OTLP export) while leaving
/// every other listener recording.
///
/// Deliberately has the same spelling as the tracing target the listener emits
/// on, so the two documented ways to silence the export are easy to relate:
/// `RUST_LOG="info,polaris::inspection=off"` and
/// `disable_listener(INSPECTION_TRACING_LISTENER)`. The typed listener identity
/// and the tracing target remain separate namespaces.
pub const INSPECTION_TRACING_LISTENER: InspectionListenerName =
    InspectionListenerName::new(INSPECTION_TARGET);

/// A contributed listener, optionally named so it can be toggled at run time.
///
/// The listener entry keeps only the switch needed on the record path. The
/// shared toggle registry retains the typed name for discovery through
/// [`InspectionAPI::registered_listeners`].
#[derive(Clone)]
struct Listener {
    /// This listener's delivery switch, shared with every other listener
    /// registered under the same name and with [`InspectionAPI`]'s toggles.
    /// `None` for an unnamed listener, which is always on and cannot be
    /// addressed by a toggle.
    enabled: Option<Arc<AtomicBool>>,
    /// The listener itself.
    sink: Arc<dyn InspectionSink>,
}

impl Listener {
    /// Whether this listener currently receives records.
    ///
    /// Reading one atomic per listener is what keeps the record path free of
    /// the disabled-name set: consulting that set would mean either holding the
    /// settings lock across arbitrary listener code or cloning it — a fresh
    /// `HashSet<String>` allocation per record — for a toggle that is off
    /// almost always.
    ///
    /// `Acquire` against the `AcqRel` swaps in
    /// [`disable_listener`](InspectionAPI::disable_listener) and
    /// [`enable_listener`](InspectionAPI::enable_listener). `Relaxed` would be
    /// enough to make the switch itself eventually visible, but this switch
    /// gates a data-export path, so it is worth the pairing to order it against
    /// whatever the caller did before flipping it. At one load per listener per
    /// record the difference is unmeasurable.
    fn is_enabled(&self) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|switch| switch.load(Ordering::Acquire))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// InspectionAPI
// ─────────────────────────────────────────────────────────────────────────────

/// Runtime settings shared between [`InspectionAPI`] and the fan-out sink.
///
/// One struct behind one lock: the fan-out consults both fields on every
/// admitted record, so grouping them costs one read acquisition instead of two.
///
/// Listener toggles are deliberately *not* here. They are per-listener state
/// the fan-out has to consult once per listener rather than once per record,
/// and answering that from a shared set would put a `HashSet` clone on
/// the record path (see [`Listener::is_enabled`]).
#[derive(Debug, Default)]
struct InspectionSettings {
    /// Which systems' records are delivered.
    policy: InspectionPolicy,
    /// Which values are withheld from rendering.
    redactions: RedactionRules,
}

/// Shared settings state between [`InspectionAPI`] and the fan-out sink.
type SharedSettings = Arc<RwLock<InspectionSettings>>;

/// Per-name delivery switches, shared between [`InspectionAPI`] and
/// [`InspectionSinkRegistry`].
///
/// A name maps to one switch, which every listener registered under it holds a
/// clone of. Only registration inserts keys, so the map is also the authoritative
/// discoverable set and unknown operator input can never grow it.
type SharedToggles = Arc<RwLock<HashMap<InspectionListenerName, Arc<AtomicBool>>>>;

/// The switch for a registered `name`, creating an enabled one on first use.
///
/// Registration is the only insertion path. Runtime toggles perform checked
/// lookups and return `None` for unknown names instead of minting entries.
fn register_toggle(toggles: &SharedToggles, name: InspectionListenerName) -> Arc<AtomicBool> {
    let existing = toggles.read().get(&name).map(Arc::clone);
    if let Some(switch) = existing {
        return switch;
    }
    Arc::clone(
        toggles
            .write()
            .entry(name)
            .or_insert_with(|| Arc::new(AtomicBool::new(true))),
    )
}

/// Runtime switch for parameter-value recording.
///
/// Use this to turn recording on while chasing a bug and off again when done,
/// without a rebuild: the handle is cheaply cloneable and every clone shares
/// the same settings, which the installed sink consults per record.
///
/// # Provided by
///
/// [`InspectionPlugin`], which inserts it during `build()`.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`new`](Self::new) | Constructs the shared switch for provider-side insertion. |
/// | [`enable`](Self::enable) | Delivers every captured record ([`InspectionPolicy::All`]); returns the displaced policy. |
/// | [`disable`](Self::disable) | Delivers nothing ([`InspectionPolicy::Off`]); returns the displaced policy. |
/// | [`enable_only`](Self::enable_only) | Narrows delivery to the named systems; returns the displaced policy. |
/// | [`set_policy`](Self::set_policy) | Replaces the policy wholesale; returns the displaced policy. |
/// | [`policy`](Self::policy) | A snapshot of the current policy. |
/// | [`set_redactions`](Self::set_redactions) | Replaces the [`RedactionRules`] wholesale; returns the displaced rules. |
/// | [`add_redacted_param`](Self::add_redacted_param) | Atomically adds one binding-name redaction without replacing existing rules. |
/// | [`add_redacted_type`](Self::add_redacted_type) | Atomically adds one type redaction without replacing existing rules. |
/// | [`redactions`](Self::redactions) | A snapshot of the current redactions. |
/// | [`registered_listeners`](Self::registered_listeners) | A sorted snapshot of names available for checked toggling. |
/// | [`disable_listener`](Self::disable_listener) | Stops delivery to a registered named listener; returns its previous state or `None` when unknown. |
/// | [`enable_listener`](Self::enable_listener) | Restores delivery to a registered named listener; returns its previous state or `None` when unknown. |
/// | [`listener_enabled`](Self::listener_enabled) | Returns a registered listener's state, or `None` when unknown. |
///
/// # Lifecycle
///
/// Available from `build()` onward and intended for **runtime** use — changing
/// the settings takes effect on the very next captured record, including
/// records later in a run that is already executing. Before
/// [`InspectionPlugin`]'s `build()` has inserted the handle,
/// `server.api::<InspectionAPI>()` returns `None`.
///
/// # Composition rule
///
/// Shared switch: [`InspectionPlugin`] owns the settings, but every mutating
/// method here is deliberately consumer-facing — the whole point is that an
/// operator surface flips recording at run time. Policy writes are **global and
/// last-writer-wins**: two holders setting the policy do not compose, the later
/// write replaces the earlier one, and no diagnostic reports the overwrite.
/// Treat the handle as a single shared switch, not a per-consumer view.
///
/// Redaction rules are the exception, because they *withhold*: composing through
/// [`set_redactions`](Self::set_redactions) would let one holder silently
/// un-redact what another covered. Add one rule atomically through
/// [`add_redacted_param`](Self::add_redacted_param) or
/// [`add_redacted_type`](Self::add_redacted_type); both finish caller-controlled
/// string construction before taking the shared lock, then perform one bounded
/// set insertion. Reserve `set_redactions` for a caller that genuinely owns the
/// whole rule set. A replacement that drops a rule warns on the
/// `polaris::inspection` target rather than withholding less than before in
/// silence.
///
/// # Scope of a policy change
///
/// The policy has **no session, run, or tenant axis**, and one fan-out serves
/// every graph run in the process. So a change here is never scoped to the
/// thing that prompted it: enabling recording to chase one session records the
/// same systems for *every* session executing concurrently, and delivers all of
/// it to every registered listener — including the shipped tracing listener,
/// whose records leave the process as OTLP span events. In a multi-tenant
/// deployment that is other tenants' parameter values, exported for as long as
/// the switch is on.
///
/// [`InspectionPolicy::Systems`] narrows by system name, which is the only axis
/// there is; it is a *precision* control, not an isolation boundary. Treat
/// enabling as a deployment-wide act:
///
/// - Narrow with [`enable_only`](Self::enable_only) rather than
///   [`enable`](Self::enable) where the system name is known.
/// - Keep the window short, and close it by setting back the policy the
///   opening call returned rather than by calling [`disable`](Self::disable),
///   which sets `Off` and so discards a `with_policy(..)` narrowing the
///   deployment started with.
/// - Put [`RedactionRules`] in place *before* enabling, not after — rules added
///   later do nothing for what already shipped.
/// - Cut the export path with
///   [`disable_listener`](Self::disable_listener)`(`[`INSPECTION_TRACING_LISTENER`]`)`
///   when the values should reach in-process listeners but not telemetry.
///
/// # Exposing this over HTTP
///
/// Every mutating method here is unauthenticated by construction: the handle is
/// `&self`, cheaply cloned, and resolvable by any plugin. That is deliberate
/// in-process, but it means a route or panel that forwards these calls is
/// handing an unauthenticated caller a process-wide, all-tenant data-export
/// switch. Any such surface must gate the mutators behind
/// [`AuthProvider`](https://docs.rs/polaris-ai/latest/polaris_ai/app/trait.AuthProvider.html),
/// prefer [`enable_only`](Self::enable_only) over [`enable`](Self::enable), and
/// treat [`set_redactions`](Self::set_redactions) and
/// [`disable_listener`](Self::disable_listener) as privileged — the first can
/// *remove* withholding rules and the second can silence an audit listener.
/// Read-only exposure of [`policy`](Self::policy) and
/// [`redactions`](Self::redactions) carries no such requirement.
///
/// Listener controls accept [`InspectionListenerName`] rather than caller-owned
/// strings, and unknown identities return `None` without allocating. Build an
/// operator surface from [`registered_listeners`](Self::registered_listeners)
/// and reject any external spelling that is absent from that snapshot.
///
/// # Example consumers
///
/// No shipped plugin consumes this API yet. The representative in-repo
/// consumer is the end-to-end suite
/// (`polaris_core_plugins/tests/inspection_e2e.rs`), which resolves the handle
/// after `finish()` and flips recording around graph runs — the same shape an
/// operator surface (a dashboard panel, an admin route) would use.
///
/// # Example
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{InspectionAPI, InspectionPolicy, RedactionRules};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Provider-side excerpt: InspectionPlugin constructs and inserts this API
/// // during build(), alongside its listener registry and graph middleware.
/// let mut server = Server::new();
/// server.insert_api(InspectionAPI::new(
///     InspectionPolicy::Off,
///     RedactionRules::new(),
/// ));
///
/// // Consumer side: resolve the same handle and flip recording at run time.
/// let inspection = server
///     .api::<InspectionAPI>()
///     .expect("the provider inserted InspectionAPI");
/// inspection.enable_only(["plan"]);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct InspectionAPI {
    /// Settings shared with the fan-out sink installed on graph runs.
    settings: SharedSettings,
    /// Per-name delivery switches, shared with [`InspectionSinkRegistry`].
    toggles: SharedToggles,
}

impl std::fmt::Debug for InspectionAPI {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let settings = self.settings.read();
        let toggles = self.toggles.read();
        // A `BTreeSet` rather than the map's own order: this is diagnostic
        // output, and an arbitrary order makes two dumps hard to compare.
        let disabled: std::collections::BTreeSet<&str> = toggles
            .iter()
            .filter(|(_, switch)| !switch.load(Ordering::Acquire))
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("InspectionAPI")
            .field("policy", &settings.policy)
            .field("redactions", &settings.redactions)
            .field("disabled_listeners", &disabled)
            .finish()
    }
}

impl API for InspectionAPI {}

impl Contract for InspectionAPI {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl InspectionAPI {
    /// Constructs a shared inspection switch for provider-side insertion.
    ///
    /// [`InspectionPlugin`] calls this while wiring the matching listener
    /// registry and graph middleware. Custom providers may construct and insert
    /// the handle directly, but an API without a sink consulting it changes no
    /// recording behavior by itself.
    #[must_use]
    pub fn new(policy: InspectionPolicy, redactions: RedactionRules) -> Self {
        Self {
            settings: Arc::new(RwLock::new(InspectionSettings { policy, redactions })),
            toggles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Delivers every captured record ([`InspectionPolicy::All`]).
    ///
    /// Process-wide and every session: see
    /// [Scope of a policy change](Self#scope-of-a-policy-change) before
    /// enabling on a deployment serving more than one tenant.
    pub fn enable(&self) -> InspectionPolicy {
        self.set_policy(InspectionPolicy::All)
    }

    /// Delivers nothing ([`InspectionPolicy::Off`]).
    ///
    /// Returns the policy this displaced, so a diagnostic window can be closed
    /// by restoring what the deployment started with rather than by assuming
    /// `Off` was it.
    pub fn disable(&self) -> InspectionPolicy {
        self.set_policy(InspectionPolicy::Off)
    }

    /// Narrows delivery to the named systems.
    ///
    /// Names are `#[system]` function names (see [`InspectionPolicy::Systems`]).
    pub fn enable_only(
        &self,
        systems: impl IntoIterator<Item = impl Into<String>>,
    ) -> InspectionPolicy {
        self.set_policy(InspectionPolicy::systems(systems))
    }

    /// Replaces the policy wholesale, returning the policy it displaced.
    ///
    /// Policy writes are last-writer-wins across every holder of this handle,
    /// so the displaced value is the only record that another holder's setting
    /// was overwritten. Keep it to restore that setting; ignore it when this
    /// caller owns the policy outright. [`disable`](Self::disable) sets `Off`
    /// rather than restoring, so an enable/disable pair around a diagnostic
    /// window loses a `with_policy(..)` narrowing unless the displaced value is
    /// kept and set back.
    pub fn set_policy(&self, policy: InspectionPolicy) -> InspectionPolicy {
        std::mem::replace(&mut self.settings.write().policy, policy)
    }

    /// A snapshot of the current policy.
    #[must_use]
    pub fn policy(&self) -> InspectionPolicy {
        self.settings.read().policy.clone()
    }

    /// Replaces the [`RedactionRules`] wholesale.
    ///
    /// Takes effect on the very next captured record: a value covered by the
    /// new rules is delivered as [`Inspection::Redacted`] without ever being
    /// formatted.
    ///
    /// This **discards** any rules another holder set. To add a rule while
    /// keeping theirs, use [`add_redacted_param`](Self::add_redacted_param) or
    /// [`add_redacted_type`](Self::add_redacted_type).
    /// Every rule dropped by the replacement is named in a warning on the
    /// `polaris::inspection` target — a withholding rule that disappears is a
    /// value that starts rendering, so it is not allowed to happen quietly.
    ///
    /// Returns the rule set it displaced, so a caller that must replace can
    /// still put back what another holder had in force.
    pub fn set_redactions(&self, redactions: RedactionRules) -> RedactionRules {
        self.replace_redactions(redactions, "set_redactions")
    }

    /// Atomically adds a binding-name redaction without replacing other rules.
    ///
    /// String conversion finishes before the settings lock is acquired; the
    /// critical section performs only one set insertion and never invokes
    /// caller code. Concurrent holders therefore compose without lost updates
    /// or a callback-under-lock deadlock.
    pub fn add_redacted_param(&self, name: impl Into<String>) {
        let name = name.into();
        self.settings.write().redactions.params.insert(name);
    }

    /// Atomically adds a declared-type redaction without replacing other rules.
    ///
    /// Type normalization and allocation finish before the settings lock is
    /// acquired; the critical section performs only one set insertion and
    /// never invokes caller code. Matching remains path-insensitive and
    /// descends into container spellings as described by [`RedactionRules`].
    pub fn add_redacted_type(&self, name: impl Into<String>) {
        let name = name.into();
        let name = match strip_type_paths(&name) {
            Cow::Borrowed(_) => name,
            Cow::Owned(stripped) => stripped,
        };
        self.settings.write().redactions.types.insert(name);
    }

    /// Installs `next`, reporting any rule it drops once the lock is released.
    ///
    /// The warning is emitted outside the guard on purpose: a subscriber runs
    /// arbitrary code, and this is the lock every recording system contends on.
    fn replace_redactions(&self, next: RedactionRules, method: &str) -> RedactionRules {
        let (dropped, previous) = {
            let mut settings = self.settings.write();
            let dropped = settings.redactions.rules_absent_from(&next);
            let previous = std::mem::replace(&mut settings.redactions, next);
            (dropped, previous)
        };
        warn_dropped_redactions(&dropped, method);
        previous
    }

    /// A snapshot of the current redactions.
    #[must_use]
    pub fn redactions(&self) -> RedactionRules {
        self.settings.read().redactions.clone()
    }

    /// A sorted snapshot of every registered listener identity.
    ///
    /// The snapshot is the validation source for operator surfaces: compare
    /// external input with [`InspectionListenerName::as_str`], then pass the
    /// matched typed identity to a toggle method. Unnamed listeners do not
    /// appear because they are always on and cannot be toggled.
    #[must_use]
    pub fn registered_listeners(&self) -> Vec<InspectionListenerName> {
        let mut names: Vec<_> = self.toggles.read().keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Stops delivery to the listener registered under `name`.
    ///
    /// Other listeners keep receiving records. Disabling
    /// [`INSPECTION_TRACING_LISTENER`] keeps rendered values out of the tracing
    /// subscriber (and OTLP export) while recording stays on. Listeners
    /// registered without a name (plain [`InspectionSinkRegistry::push`])
    /// cannot be addressed here.
    ///
    /// Takes effect from the next record onward. A record already past the
    /// fan-out's per-listener check when this returns is still delivered, so
    /// treat this as closing the tap rather than as a barrier — it bounds what
    /// is exported *next*, not what is already in flight.
    ///
    /// Returns the previous enabled state, or `None` when no listener has
    /// registered under `name`. Unknown names never allocate registry state.
    #[must_use = "check whether the listener name was registered"]
    pub fn disable_listener(&self, name: InspectionListenerName) -> Option<bool> {
        self.toggles
            .read()
            .get(&name)
            .map(|switch| switch.swap(false, Ordering::AcqRel))
    }

    /// Restores delivery to the listener registered under `name`.
    ///
    /// Returns the previous enabled state, or `None` when no listener has
    /// registered under `name`. Unknown names never allocate registry state.
    #[must_use = "check whether the listener name was registered"]
    pub fn enable_listener(&self, name: InspectionListenerName) -> Option<bool> {
        self.toggles
            .read()
            .get(&name)
            .map(|switch| switch.swap(true, Ordering::AcqRel))
    }

    /// Whether the listener registered under `name` currently receives
    /// records.
    ///
    /// Returns `None` for an unknown identity, which distinguishes an invalid
    /// operator request from a registered listener that is currently enabled.
    #[must_use]
    pub fn listener_enabled(&self, name: InspectionListenerName) -> Option<bool> {
        self.toggles
            .read()
            .get(&name)
            .map(|switch| switch.load(Ordering::Acquire))
    }

    /// The shared state handed to the fan-out sink.
    fn shared(&self) -> SharedSettings {
        Arc::clone(&self.settings)
    }

    /// The switch map handed to [`InspectionSinkRegistry`].
    fn toggles(&self) -> SharedToggles {
        Arc::clone(&self.toggles)
    }
}

/// Reports redaction rules that a replacement dropped.
///
/// Free function rather than a method because both doors into a replacement
/// call it after releasing the lock.
fn warn_dropped_redactions(dropped: &[String], method: &str) {
    if dropped.is_empty() {
        return;
    }
    // `event!` rather than `warn!`: the shorthand macros cannot parse
    // dotted field names without a local ambiguity error.
    tracing::event!(
        target: INSPECTION_TARGET,
        tracing::Level::WARN,
        polaris.inspection.method = method,
        polaris.inspection.dropped_rules = ?dropped,
        "dropped redaction rules another holder had in force; values they \
         withheld will now be rendered and delivered to every listener. Add \
         rules with InspectionAPI::add_redacted_param or \
         InspectionAPI::add_redacted_type instead of replacing the \
         rule set",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// InspectionSinkRegistry
// ─────────────────────────────────────────────────────────────────────────────

/// Listener list shared between the registry and the fan-out sink.
///
/// Copy-on-write: the outer `Mutex` guards the slot, the inner `Arc<Vec<..>>`
/// is the list itself. Registration clones-and-swaps the inner `Arc`
/// ([`Arc::make_mut`]), so the fan-out's per-record snapshot is a single `Arc`
/// bump with no `Vec` allocation.
type SharedListeners = Arc<Mutex<Arc<Vec<Listener>>>>;

/// Reach for this when a plugin should receive the parameter-value recording
/// stream — push an [`InspectionSink`] here and it gets every record the
/// [`InspectionPolicy`] admits.
///
/// # Provided by
///
/// [`InspectionPlugin`], which inserts it during `build()`.
///
/// Consumers contribute during their own `build()` via
/// [`Extends`](polaris_system::plugin::Extends); each listener independently
/// receives every admitted record. The framework keeps no buffer of past
/// records — listeners bring their own storage.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`push`](Self::push) | Contributes an always-on [`InspectionSink`]. |
/// | [`push_named`](Self::push_named) | Contributes a listener under a name, so [`InspectionAPI`] can switch it off and on at run time. |
///
/// # Lifecycle
///
/// Contribute from a consumer plugin's `build()` phase, while holding the
/// [`Extends`](polaris_system::plugin::Extends) handle — that is the window the
/// capability resolver orders, so a listener registered there is in place before
/// any run. Later pushes are late-but-OK: the fan-out reads the live list, so a
/// sink added after `build()` receives every subsequent record, it just forfeits
/// those ordering guarantees. Listeners are never removed; use a name plus
/// [`InspectionAPI::disable_listener`] to stop delivery instead.
///
/// # Composition rule
///
/// Open extension: any plugin may [`push`](Self::push) (or
/// [`push_named`](Self::push_named)) a listener; contributions accumulate and
/// never displace one another.
///
/// # Example consumers
///
/// - [`InspectionPlugin`] itself — pushes [`TracingInspectionSink`] under
///   [`INSPECTION_TRACING_LISTENER`] unless
///   [`without_tracing_sink`](InspectionPlugin::without_tracing_sink) opted out.
/// - `polaris_core_plugins/tests/inspection_e2e.rs` — two independent listener
///   plugins, the shape a dashboard or audit-log consumer would take.
///
/// # Example
///
/// ```
/// use polaris_system::param::inspect::{Inspection, InspectionSink, ParamMeta};
/// use polaris_system::plugin;
/// use polaris_system::plugin::{Extends, Plugin};
/// use polaris_core_plugins::InspectionSinkRegistry;
///
/// struct MyListener;
/// impl InspectionSink for MyListener {
///     fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
///         let _ = (meta, render());
///     }
/// }
///
/// struct MyPlugin;
///
/// #[plugin(id = "my::plugin", version = "0.1.0")]
/// impl Plugin for MyPlugin {
///     fn build(&self, mut registry: Extends<InspectionSinkRegistry>) {
///         registry.push(MyListener);
///     }
/// }
///
/// # fn wire(server: &mut polaris_system::server::Server) {
/// // Provider side: InspectionPlugin inserts the registry during build().
/// // Without it, MyPlugin's Extends<InspectionSinkRegistry> is unsatisfied and
/// // the build fails rather than silently registering nothing.
/// server.add_plugins(polaris_core_plugins::InspectionPlugin::default());
/// server.add_plugins(MyPlugin);
/// # }
/// ```
pub struct InspectionSinkRegistry {
    /// Listeners shared with the fan-out installed on graph runs.
    listeners: SharedListeners,
    /// Per-name delivery switches, shared with [`InspectionAPI`].
    toggles: SharedToggles,
}

impl InspectionSinkRegistry {
    /// Empty registry over an [`InspectionAPI`]'s switches. Inserted by
    /// [`InspectionPlugin`] during `build()`.
    fn new(toggles: SharedToggles) -> Self {
        Self {
            listeners: Arc::new(Mutex::new(Arc::new(Vec::new()))),
            toggles,
        }
    }

    /// Contribute an [`InspectionSink`].
    ///
    /// Call this from a consumer plugin's `build()` phase, while holding an
    /// [`Extends<InspectionSinkRegistry>`](polaris_system::plugin::Extends).
    /// The fan-out reads the live listener list, so a sink pushed after
    /// `build()` still receives subsequent records — but only build-phase
    /// pushes get the resolver's ordering guarantees.
    ///
    /// A listener registered this way is always on; use
    /// [`push_named`](Self::push_named) to make it toggleable through
    /// [`InspectionAPI::disable_listener`].
    ///
    /// Contributed sinks run **synchronously on the execution path of the
    /// system being observed** (the fan-out invokes them lock-free, off a
    /// snapshot of this list): a listener must not block or perform I/O in
    /// `record` — hand the record off to a channel and drain it elsewhere. A
    /// panic in a listener unwinds
    /// through the system being observed (only panics inside a value's `Debug`
    /// are absorbed, at the render boundary).
    pub fn push(&mut self, sink: impl InspectionSink + 'static) {
        self.add(Listener {
            enabled: None,
            sink: Arc::new(sink),
        });
    }

    /// Contribute an [`InspectionSink`] under a name, so it can be switched
    /// off and on at run time via [`InspectionAPI::disable_listener`] /
    /// [`InspectionAPI::enable_listener`].
    ///
    /// Names are typed, static plugin identities rather than runtime strings.
    /// They are not deduplicated: two listeners registered under the same name
    /// both receive records and are both addressed by that name's toggle.
    /// Everything on [`push`](Self::push) about execution context applies here
    /// too.
    pub fn push_named(
        &mut self,
        name: InspectionListenerName,
        sink: impl InspectionSink + 'static,
    ) {
        let enabled = register_toggle(&self.toggles, name);
        self.add(Listener {
            enabled: Some(enabled),
            sink: Arc::new(sink),
        });
    }

    /// Copy-on-write append: clones the list once per registration so the
    /// fan-out's snapshot stays a single `Arc` bump.
    fn add(&mut self, listener: Listener) {
        let mut slot = self.listeners.lock();
        Arc::make_mut(&mut slot).push(listener);
    }

    /// A single [`InspectionSink`] that forwards each admitted record to every
    /// contributed listener.
    fn fanout(&self, settings: SharedSettings) -> InspectionFanout {
        InspectionFanout {
            settings,
            listeners: Arc::clone(&self.listeners),
        }
    }
}

impl std::fmt::Debug for InspectionSinkRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `finish_non_exhaustive`: `toggles` is deliberately omitted (it is
        // InspectionAPI's to dump), so claiming a complete rendering would be
        // a lie.
        f.debug_struct("InspectionSinkRegistry")
            .field("listeners", &self.listeners.lock().len())
            .finish_non_exhaustive()
    }
}

impl Contract for InspectionSinkRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// InspectionFanout
// ─────────────────────────────────────────────────────────────────────────────

/// Fan-out [`InspectionSink`] installed on graph runs by [`InspectionPlugin`].
///
/// Consults the shared [`InspectionSettings`] before rendering: a record the
/// policy excludes is dropped without formatting, and a value the
/// [`RedactionRules`] cover is delivered as [`Inspection::Redacted`] without the
/// render closure ever running. An admitted, unredacted record is rendered at
/// most once — each listener receives a memoizing render closure, so a
/// listener that declines still pays nothing and N listeners cost one
/// formatting pass. Each `render()` call hands back a clone of the memoized
/// rendering, because the [`InspectionSink`] contract returns an owned
/// [`Inspection`]; formatting itself never repeats.
struct InspectionFanout {
    /// Settings shared with [`InspectionAPI`].
    settings: SharedSettings,
    /// Listeners shared with [`InspectionSinkRegistry`].
    listeners: SharedListeners,
}

impl InspectionSink for InspectionFanout {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        // One read acquisition covers the policy gate and the redaction
        // decision, and is released before any listener code runs. Nothing is
        // cloned out of it: each listener carries its own delivery switch, so
        // the record path allocates nothing whatever the toggles say.
        let redacted = {
            let settings = self.settings.read();
            if !settings.policy.allows(meta.system) {
                return;
            }
            settings.redactions.covers(&meta)
        };
        // Snapshot the list (one Arc bump) so no lock is held while the
        // listeners run: they are arbitrary plugin code, and concurrent
        // records must not serialize on one another.
        let listeners = Arc::clone(&self.listeners.lock());
        if listeners.is_empty() {
            return;
        }
        let enabled = listeners.iter().filter(|listener| listener.is_enabled());
        if redacted {
            // A covered value is never formatted: substitute the sentinel
            // without calling `render`, so it never reaches a `String`.
            for listener in enabled {
                listener.sink.record(meta, &|| Inspection::Redacted);
            }
            return;
        }
        let rendered: OnceCell<Inspection> = OnceCell::new();
        let memoized = || rendered.get_or_init(render).clone();
        for listener in enabled {
            listener.sink.record(meta, &memoized);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TracingInspectionSink
// ─────────────────────────────────────────────────────────────────────────────

/// The shipped listener: forwards each record onto the ambient `tracing` span.
///
/// Emits one `INFO` event under target `polaris::inspection` per record, with
/// fields
/// `polaris.inspection.{system,param,type_name,kind,phase,rendering,value,truncated}`.
/// Because it runs synchronously inside the observed system's future,
/// `Span::current()` is the `polaris.graph.execute_system` span opened by
/// [`TracingPlugin`](crate::TracingPlugin)'s middleware (itself nested under
/// the session's turn span), so the event arrives correlated to the step, run,
/// and session with no extra plumbing — and flows to OpenTelemetry as a span
/// event when [`OpenTelemetryPlugin`](crate::OpenTelemetryPlugin) is active.
///
/// Registered by [`InspectionPlugin`] unless
/// [`without_tracing_sink`](InspectionPlugin::without_tracing_sink) opted out;
/// push it into an [`InspectionSinkRegistry`] manually on custom setups.
///
/// # Reading the emitted fields
///
/// Two fields exist because a rendered value is attacker-influenced data being
/// written into a log:
///
/// - `value` is emitted through [`Debug`], so it arrives quoted and escaped. A
///   value containing newlines cannot forge additional log lines, and a
///   [`tracing`] subscriber that formats it verbatim gets one field, not
///   several.
/// - `rendering` says which [`Inspection`] variant produced `value` —
///   `"text"`, `"redacted"`, `"opaque"`, or `"unrenderable"`. Key alerting and
///   filtering on this rather than on the shape of `value`: a real value whose
///   `Debug` renders the literal string `<redacted>` is indistinguishable from
///   the redaction sentinel by `value` alone, and a hand-written `Debug` can
///   emit exactly that on purpose. `rendering` is produced here and cannot be
///   influenced by the value.
///
/// Neither closes the underlying point that a hand-written [`Debug`] chooses
/// its own bytes. They make it detectable rather than authoritative.
///
/// # Filtering
///
/// The listener checks whether the target is enabled *before* rendering, so a
/// subscriber filter that excludes `polaris::inspection` also prevents this
/// listener formatting the value at all. This costs nothing when enabled
/// ([`tracing`] caches callsite interest) and makes
/// `RUST_LOG="info,polaris::inspection=off"` a real "never reaches a `String`
/// via this listener" guarantee rather than only a "never reaches the
/// subscriber" one. Other listeners are unaffected: they render independently,
/// so the value may still be formatted for them.
///
/// # Example
///
/// A custom setup that opted out of the default registration signs the sink
/// back up like any other listener:
///
/// ```
/// use polaris_system::plugin;
/// use polaris_system::plugin::{Extends, Plugin};
/// use polaris_core_plugins::{InspectionSinkRegistry, TracingInspectionSink};
///
/// struct TracingListenerPlugin;
///
/// #[plugin(id = "my::tracing_listener", version = "0.1.0")]
/// impl Plugin for TracingListenerPlugin {
///     fn build(&self, mut registry: Extends<InspectionSinkRegistry>) {
///         registry.push(TracingInspectionSink);
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TracingInspectionSink;

/// Tracing target for everything this module emits.
///
/// Kept as one constant because it is an operational control surface, not just
/// a label: `EnvFilter` directives address it by name, so
/// `RUST_LOG="info,polaris::inspection=off"` keeps rendered parameter values out
/// of the subscriber (and therefore out of OTLP export) while recording stays
/// enabled for other listeners.
const INSPECTION_TARGET: &str = "polaris::inspection";

impl InspectionSink for TracingInspectionSink {
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
        // Ask before rendering. The event macro would drop the record anyway
        // when the target is filtered out, but only after `render()` had
        // already put the value in a `String` — which is the one thing the
        // documented `RUST_LOG=...polaris::inspection=off` route promises not
        // to do.
        if !tracing::enabled!(target: INSPECTION_TARGET, tracing::Level::INFO) {
            return;
        }
        let rendered = render();
        // `rendering` is this listener's own word for which variant it got, so
        // a value that renders as the literal `<redacted>` cannot pass itself
        // off as the sentinel to anything keying on it.
        let (rendering, value, truncated): (&str, Cow<'_, str>, bool) = match &rendered {
            Inspection::Text { value, truncated } => {
                ("text", Cow::Borrowed(value.as_str()), *truncated)
            }
            Inspection::Redacted => ("redacted", Cow::Borrowed("<redacted>"), false),
            Inspection::Opaque(reason) => {
                ("opaque", Cow::Owned(format!("<opaque: {reason}>")), false)
            }
            _ => ("unrenderable", Cow::Borrowed("<unrenderable>"), false),
        };
        tracing::event!(
            target: INSPECTION_TARGET,
            tracing::Level::INFO,
            polaris.inspection.system = meta.system,
            polaris.inspection.param = meta.param,
            polaris.inspection.type_name = meta.type_name,
            polaris.inspection.kind = ?meta.kind,
            polaris.inspection.phase = ?meta.phase,
            polaris.inspection.rendering = rendering,
            // `?` not `%`: a rendered value is data, and Debug quotes and
            // escapes it so it cannot forge log structure.
            polaris.inspection.value = ?value,
            polaris.inspection.truncated = truncated,
            "system parameter value",
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// InspectionPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Turns the Layer-1 inspection mechanism ([`polaris_system::param::inspect`])
/// into a usable recording pipeline: a runtime on/off switch, runtime
/// [`RedactionRules`], a listener registry, and a default listener that forwards
/// records into the tracing setup.
///
/// Add it when you want `#[system(inspect(..))]` captures to go somewhere.
/// Recording is **off by default** — the plugin is inert until
/// [`InspectionAPI::enable`] (or a non-[`Off`](InspectionPolicy::Off) initial
/// policy via [`with_policy`](Self::with_policy) or
/// [`with_policy_from_env`](Self::with_policy_from_env)) admits records, so it
/// is safe to keep registered everywhere; [`DefaultPlugins`](crate::DefaultPlugins)
/// includes it. The `_from_env` builders fail closed: an unset variable
/// changes nothing, and a set-but-malformed spec fails startup rather than
/// coming up under a posture nobody chose.
///
/// # How the pieces fit
///
/// Layer 1 makes parameter values *capturable* and defines the
/// [`InspectionSink`] delivery seam, but installs nothing and stores nothing.
/// This plugin is the other half, and it hands out four things:
///
/// | Type | Decides |
/// |------|---------|
/// | [`InspectionPolicy`] | *whether* a record flows — off, on, or narrowed to named systems |
/// | [`RedactionRules`] | *which values* are delivered as [`Inspection::Redacted`] instead of being formatted at all |
/// | [`InspectionAPI`] | both of the above, at run time, without a rebuild |
/// | [`InspectionSinkRegistry`] | *who receives* records — any plugin signs up during its `build()` |
///
/// [`TracingInspectionSink`] is the one listener shipped pre-registered (under
/// [`INSPECTION_TRACING_LISTENER`]). The plugin installs a single fan-out sink
/// per graph run; the policy is consulted per record, so with recording off no
/// value is ever formatted.
///
/// There is deliberately no in-framework buffer of past records and no HTTP
/// query surface — the registry is an export boundary, the same shape as
/// [`SpanProcessorRegistry`](crate::SpanProcessorRegistry).
///
/// Values render through each type's own `Debug` impl. Enabling recording
/// exposes those renderings to every registered listener — do not enable it
/// (or select the parameter in `inspect(..)` at all) where a derived `Debug`
/// would expose credentials or other sensitive data to a destination that
/// should not hold it; for a value that must stay selected but never render,
/// add a [`RedactionRules`] rule ([`with_redactions`](Self::with_redactions) or
/// [`InspectionAPI::set_redactions`]). A rule matches the record's metadata,
/// not the value, so it cannot withhold a credential nested inside another
/// type — see [What a rule cannot see](RedactionRules#what-a-rule-cannot-see).
/// Note that a **hand-written** `Debug` impl need not escape what it writes
/// where a derived one escapes string contents via `escape_debug`; the shipped
/// [`TracingInspectionSink`] escapes the whole rendering again on the way out,
/// so log structure is safe there either way, but a listener that writes the
/// rendering verbatim is not covered. Hand-written
/// [`System`](polaris_system::system::System)
/// impls get no capture: the mechanism lives in the `#[system]` macro, so this
/// plugin only ever sees records from macro-defined systems, and only from
/// runs that execute through the server's [`MiddlewareAPI`].
///
/// With this plugin present, do not call
/// [`SystemContext::replace_inspection`](polaris_system::param::SystemContext::replace_inspection)
/// yourself — the plugin installs its fan-out at the start of every graph run,
/// replacing any manually installed sink. Register a listener instead, or drop
/// the plugin if the manual sink is what you want. The displacement is not
/// silent: the first run that finds a *foreign* sink already installed logs a
/// warning on the `polaris::inspection` target. Its own fan-out is recognized by
/// identity, so a context reused across turns (the sessions-per-turn shape) is
/// not mistaken for a manual installation and the warning stays available for
/// the case it describes.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`InspectionSinkRegistry`] | Build-time (server resource — not reachable as `Res<T>`) | Sign-up sheet for listeners; contributed to via [`Extends<InspectionSinkRegistry>`](polaris_system::plugin::Extends) during consumer `build()`. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`InspectionAPI`] | Runtime switch for the recording pipeline: policy (on, off, or narrowed to named systems), [`RedactionRules`], and per-name listener toggles. |
///
/// # Middleware Registered
///
/// Registered via [`MiddlewareAPI`] under the name `"polaris::inspection"` —
/// the handle it appears under in middleware ordering and introspection.
///
/// | Target | Behavior | Description |
/// |--------|----------|-------------|
/// | Graph execution | Wraps every run | Installs the fan-out sink on the run's root context, so every system in the run (including scopes, branches, and loop iterations) inherits it. |
///
/// # Dependencies
///
/// None.
///
/// # Extends
///
/// - [`MiddlewareAPI`] — registers the sink-installing middleware. Inserts the
///   API if no other plugin provided it, which is why no capability relationship
///   to it is declared in [`access`](Plugin::access): every in-repo inserter
///   (this plugin and [`TracingPlugin`](crate::TracingPlugin)) guards on
///   `contains_api::<MiddlewareAPI>()`, so the API is shared rather than
///   displaced and no build order between them is load-bearing.
///
///   That last part is a convention, not something the type system enforces:
///   [`MiddlewareAPI`] implements no `Contract`, so the relationship cannot be
///   declared and the resolver cannot order around it. A plugin that calls
///   `insert_api(MiddlewareAPI::new())` *unguarded*, and builds after this one,
///   replaces the API this plugin registered onto — recording then silently
///   captures nothing, with no warning and no build error. Guard the insert, or
///   depend on whichever plugin owns the API.
///
/// # Lifecycle
///
/// - **`build()`** — inserts [`InspectionAPI`] and [`InspectionSinkRegistry`]
///   (pre-loading [`TracingInspectionSink`] under the name
///   [`INSPECTION_TRACING_LISTENER`] unless opted out) and registers the graph-execution
///   middleware.
/// - Settings changes through [`InspectionAPI`] apply immediately at any
///   point after `build()`; listener registration belongs in consumer
///   `build()` phases.
///
/// # Example
///
/// The plugin becomes useful when a consumer signs a listener up — the switch
/// alone records to nobody but the tracing listener:
///
/// ```no_run
/// use polaris_system::param::inspect::{Inspection, InspectionSink, ParamMeta};
/// use polaris_system::plugin;
/// use polaris_system::plugin::{Extends, Plugin};
/// use polaris_system::server::Server;
/// use polaris_core_plugins::{InspectionAPI, InspectionPlugin, InspectionSinkRegistry};
///
/// struct MyListener;
/// impl InspectionSink for MyListener {
///     fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
///         // Hand off to a channel here — no blocking, no I/O.
///         let _ = (meta, render());
///     }
/// }
///
/// struct MyListenerPlugin;
///
/// #[plugin(id = "my::listener", version = "0.1.0")]
/// impl Plugin for MyListenerPlugin {
///     fn build(&self, mut registry: Extends<InspectionSinkRegistry>) {
///         registry.push(MyListener);
///     }
/// }
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut server = Server::new();
/// server.add_plugins(InspectionPlugin::default());
/// server.add_plugins(MyListenerPlugin);
/// server.finish().await?;
///
/// // Recording is off; flip it on to chase a bug, no rebuild needed.
/// server
///     .api::<InspectionAPI>()
///     .expect("InspectionPlugin provides InspectionAPI")
///     .enable();
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectionPlugin {
    /// Policy the server starts with.
    initial_policy: InspectionPolicy,
    /// The redaction rules the server starts with.
    initial_redactions: RedactionRules,
    /// Whether `build()` pre-loads [`TracingInspectionSink`].
    tracing_sink: bool,
}

/// Starts with recording [`Off`](InspectionPolicy::Off), nothing redacted,
/// and the tracing listener registered.
impl Default for InspectionPlugin {
    fn default() -> Self {
        Self {
            initial_policy: InspectionPolicy::Off,
            initial_redactions: RedactionRules::new(),
            tracing_sink: true,
        }
    }
}

impl InspectionPlugin {
    /// Creates the plugin with default settings (recording off, tracing
    /// listener registered).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the policy the server starts with.
    ///
    /// The default is [`InspectionPolicy::Off`]. Starting with an enabling
    /// policy exposes rendered values from the first run onward — see the
    /// sensitive-data note on the plugin.
    #[must_use]
    pub fn with_policy(mut self, policy: InspectionPolicy) -> Self {
        self.initial_policy = policy;
        self
    }

    /// Sets the [`RedactionRules`] the server starts with.
    ///
    /// The default withholds nothing. Rules apply from the first run onward;
    /// [`InspectionAPI::set_redactions`] changes them at run time.
    ///
    /// This **replaces** the configured rules wholesale — including any an
    /// earlier
    /// [`with_redactions_from_env`](Self::with_redactions_from_env) call
    /// added, silently dropping what the environment asked to withhold. Call
    /// this first and let the environment widen the result.
    #[must_use]
    pub fn with_redactions(mut self, redactions: RedactionRules) -> Self {
        self.initial_redactions = redactions;
        self
    }

    /// Sets the starting policy from the environment variable `var`, when set.
    ///
    /// The variable holds a policy spec — `off`, `all`, or a comma-separated
    /// list of `#[system]` function names (the [`FromStr`] grammar on
    /// [`InspectionPolicy`]) — so one binary carries different inspection
    /// postures per deployment without a rebuild. The variable's *name* is the
    /// caller's: the framework reads whichever variable the application
    /// chooses and prescribes none of its own.
    ///
    /// An unset variable changes nothing: the policy stays whatever it already
    /// was, which is [`Off`](InspectionPolicy::Off) unless
    /// [`with_policy`](Self::with_policy) set it earlier — a deployment that
    /// says nothing records nothing.
    ///
    /// # Panics
    ///
    /// Panics if the variable is set but not valid Unicode or does not parse.
    /// A server must not come up recording under a posture nobody chose, and
    /// for this control both silent fallbacks are wrong ways: falling back to
    /// `Off` silently disables the observability staging asked for, and any
    /// other guess may record what production meant to keep off.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use polaris_core_plugins::InspectionPlugin;
    ///
    /// // prod leaves the variables unset and stays off;
    /// // staging sets e.g. MYAPP_INSPECT=plan,act
    /// //               and MYAPP_INSPECT_REDACT=type:ApiCredentials,param:api_key
    /// let plugin = InspectionPlugin::new()
    ///     .with_policy_from_env("MYAPP_INSPECT")
    ///     .with_redactions_from_env("MYAPP_INSPECT_REDACT");
    /// ```
    #[must_use]
    pub fn with_policy_from_env(self, var: &str) -> Self {
        let spec = env_spec(var);
        self.policy_from_spec(var, spec)
    }

    /// The testable half of [`with_policy_from_env`](Self::with_policy_from_env):
    /// everything after the environment read.
    fn policy_from_spec(self, var: &str, spec: Option<String>) -> Self {
        match spec {
            Some(spec) => self.with_policy(spec.parse().unwrap_or_else(|error| {
                panic!(
                    "environment variable {var} holds an invalid inspection policy spec: {error}"
                )
            })),
            None => self,
        }
    }

    /// Sets the starting [`RedactionRules`] from the environment variable
    /// `var`, when set.
    ///
    /// The variable holds a redaction spec — comma-separated `param:<binding>`
    /// and `type:<Type>` entries (the [`FromStr`] grammar on
    /// [`RedactionRules`]). As with
    /// [`with_policy_from_env`](Self::with_policy_from_env), the variable's
    /// name is the caller's, and an unset variable changes nothing. A set
    /// variable **adds** its rules to whatever
    /// [`with_redactions`](Self::with_redactions) already configured rather
    /// than replacing them: the environment can widen withholding but never
    /// drop a rule the binary baked in — the same one-way composition as the
    /// runtime [`InspectionAPI::add_redacted_param`] /
    /// [`add_redacted_type`](InspectionAPI::add_redacted_type) operations, and
    /// deliberately narrower than [`with_redactions`](Self::with_redactions),
    /// whose replacement semantics stay available to code.
    ///
    /// # Panics
    ///
    /// Panics if the variable is set but not valid Unicode or does not parse.
    /// A withholding rule that fails to parse must not be quietly dropped —
    /// that would leak exactly the values it was written to withhold.
    #[must_use]
    pub fn with_redactions_from_env(self, var: &str) -> Self {
        let spec = env_spec(var);
        self.redactions_from_spec(var, spec)
    }

    /// The testable half of
    /// [`with_redactions_from_env`](Self::with_redactions_from_env):
    /// everything after the environment read.
    fn redactions_from_spec(mut self, var: &str, spec: Option<String>) -> Self {
        if let Some(spec) = spec {
            let parsed: RedactionRules = spec.parse().unwrap_or_else(|error| {
                panic!(
                    "environment variable {var} holds an invalid inspection redaction spec: {error}"
                )
            });
            self.initial_redactions.params.extend(parsed.params);
            self.initial_redactions.types.extend(parsed.types);
        }
        self
    }

    /// Skips registering [`TracingInspectionSink`].
    ///
    /// Use when records should reach only explicitly registered listeners and
    /// not the tracing subscriber. To keep the sink registered but switch it
    /// off at run time instead, use
    /// [`InspectionAPI::disable_listener`]`(`[`INSPECTION_TRACING_LISTENER`]`)`.
    #[must_use]
    pub fn without_tracing_sink(mut self) -> Self {
        self.tracing_sink = false;
        self
    }
}

impl Plugin for InspectionPlugin {
    const ID: &'static str = "polaris::inspection";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Declared by hand rather than via `#[plugin]` because `build()` also
        // needs `&mut Server` to insert the API and middleware.
        PluginAccess::new()
            .provides::<InspectionSinkRegistry>(InspectionSinkRegistry::CONTRACT_VERSION)
            .provides::<InspectionAPI>(InspectionAPI::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        let api = InspectionAPI::new(self.initial_policy.clone(), self.initial_redactions.clone());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        if self.tracing_sink {
            registry.push_named(INSPECTION_TRACING_LISTENER, TracingInspectionSink);
        }
        let sink: Arc<dyn InspectionSink> = Arc::new(registry.fanout(api.shared()));

        server.insert_api(api);
        server.insert_resource(registry);

        if !server.contains_api::<MiddlewareAPI>() {
            server.insert_api(MiddlewareAPI::new());
        }
        // Installing the fan-out displaces any sink the caller put on the
        // context through `SystemContext::replace_inspection`. This plugin
        // replaces rather than chains, so warn instead of dropping the sink
        // silently — once, not once per run, since the middleware fires on
        // every graph execution.
        //
        // Chaining is a live option rather than an impossible one:
        // `SystemContext::inspection_arc()` (SC-3307, already landed) hands
        // back an owned `Arc` the fan-out could keep, where `inspection()` only
        // lends a borrow. If this plugin is ever moved onto it, **the identity
        // guard below has to move with it**. It compares the installed sink against `sink` itself, which
        // is only correct because replacing means the sink in force is always
        // this fan-out. A chaining version installs a *wrapper* around the
        // caller's sink, and the wrapper's address is not `sink`'s — so the
        // comparison would report a displacement on every single run, and worse,
        // the recipe it guards is not idempotent: each run would wrap whatever
        // is installed in a fresh link, deepening `record()` recursion without
        // bound and duplicating every record once per link on a context reused
        // across turns. The fix is to retain the handle actually installed and
        // compare against that. See "Chaining onto an installed sink" in
        // docs/reference/context.md.
        let displaced = Once::new();
        server
            .api::<MiddlewareAPI>()
            .expect("MiddlewareAPI should be present after initialization")
            .register_graph_execution("polaris::inspection", move |_info, ctx, next| {
                // Only a *foreign* sink is a displacement. A context that
                // already carries this same fan-out is one being reused across
                // runs — the sessions-per-turn shape — and re-installing it
                // replaces nothing. Comparing addresses keeps the one-shot
                // warning for the case it describes instead of spending it on
                // turn two of the first session.
                let displacing = ctx.inspection().is_some_and(|installed| {
                    !std::ptr::addr_eq(std::ptr::from_ref(installed), Arc::as_ptr(&sink))
                });
                if displacing {
                    displaced.call_once(|| {
                        tracing::warn!(
                            target: INSPECTION_TARGET,
                            "replacing an inspection sink installed via \
                             SystemContext::replace_inspection; register a listener on \
                             InspectionSinkRegistry instead, or drop InspectionPlugin \
                             to keep the manual sink",
                        );
                    });
                    // The warning is one-shot so it cannot flood, but the loss
                    // it reports recurs on every run and every context. A sink
                    // that is itself an audit control deserves a trace of each
                    // displacement, not only the first.
                    tracing::debug!(
                        target: INSPECTION_TARGET,
                        "displacing a foreign inspection sink for this run",
                    );
                }
                let _ = ctx.replace_inspection(Arc::clone(&sink));
                next.run(ctx)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FieldMapVisitor, set_default_and_rebuild};
    use polaris_system::param::inspect::{ParamKind, Phase};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    /// Listener handle that stores every record it receives.
    #[derive(Clone, Default)]
    struct SharedCollecting(Arc<Mutex<Vec<(ParamMeta, Inspection)>>>);

    impl SharedCollecting {
        fn records(&self) -> Vec<(ParamMeta, Inspection)> {
            self.0.lock().clone()
        }
    }

    impl InspectionSink for SharedCollecting {
        fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
            self.0.lock().push((meta, render()));
        }
    }

    fn meta_for(system: &'static str) -> ParamMeta {
        ParamMeta::new(system, "memory", "Memory", ParamKind::ResMut, Phase::Before)
    }

    /// A render closure that counts how many times formatting actually ran.
    fn counting_render(calls: &AtomicUsize) -> impl Fn() -> Inspection + '_ {
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Inspection::Text {
                value: "rendered".to_owned(),
                truncated: false,
            }
        }
    }

    fn fanout_with(
        policy: InspectionPolicy,
        listeners: &[SharedCollecting],
    ) -> (InspectionAPI, InspectionFanout) {
        let api = InspectionAPI::new(policy, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        for listener in listeners {
            registry.push(listener.clone());
        }
        let fanout = registry.fanout(api.shared());
        (api, fanout)
    }

    #[test]
    fn the_default_policy_is_off() {
        assert_eq!(InspectionPolicy::default(), InspectionPolicy::Off);
        assert_eq!(
            InspectionPlugin::default(),
            InspectionPlugin::new().with_policy(InspectionPolicy::Off),
            "the default plugin must start with recording off"
        );
    }

    #[test]
    fn policy_narrowing_matches_system_names() {
        assert!(!InspectionPolicy::Off.allows("plan"));
        assert!(InspectionPolicy::All.allows("plan"));
        let narrowed = InspectionPolicy::systems(["plan"]);
        assert!(narrowed.allows("plan"));
        assert!(!narrowed.allows("act"));
        assert!(
            !InspectionPolicy::Systems(HashSet::new()).allows("plan"),
            "an empty narrowing denies everything"
        );
    }

    #[test]
    fn narrowed_systems_reads_the_sealed_payload_without_destructuring_it() {
        // This accessor is the reason `Systems`' payload can be swapped without
        // a breaking release, so it is held to the contract independently of
        // the doctest that demonstrates it.
        let narrowed = InspectionPolicy::systems(["plan", "act"]);
        let mut named: Vec<&str> = narrowed
            .narrowed_systems()
            .expect("a narrowed policy must name its systems")
            .collect();
        named.sort_unstable();
        assert_eq!(named, ["act", "plan"]);

        // Neither unnarrowed mode names systems, and the two differ on
        // `allows` — which is why `None` must not be read as "records nothing".
        for policy in [InspectionPolicy::All, InspectionPolicy::Off] {
            assert!(
                policy.narrowed_systems().is_none(),
                "{policy:?} names no systems"
            );
        }
        assert!(InspectionPolicy::All.allows("plan"));
        assert!(!InspectionPolicy::Off.allows("plan"));

        // An empty narrowing is a narrowing: it answers `Some`, empty — the
        // one case where `None` and "no names" are genuinely different answers.
        let no_names = InspectionPolicy::Systems(HashSet::new());
        let empty: Vec<&str> = no_names
            .narrowed_systems()
            .expect("an empty narrowing still narrows")
            .collect();
        assert!(empty.is_empty());
    }

    #[test]
    fn off_means_nothing_is_formatted_even_with_listeners() {
        let listener = SharedCollecting::default();
        let (_api, fanout) = fanout_with(InspectionPolicy::Off, std::slice::from_ref(&listener));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(calls.load(Ordering::SeqCst), 0, "off must not format");
        assert!(listener.records().is_empty(), "off must not deliver");
    }

    #[test]
    fn no_listeners_means_nothing_is_formatted() {
        let (_api, fanout) = fanout_with(InspectionPolicy::All, &[]);

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn every_listener_receives_every_record_from_one_rendering() {
        let first = SharedCollecting::default();
        let second = SharedCollecting::default();
        let (_api, fanout) = fanout_with(InspectionPolicy::All, &[first.clone(), second.clone()]);

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        fanout.record(meta_for("act"), &counting_render(&calls));

        for listener in [&first, &second] {
            let records = listener.records();
            let systems: Vec<&str> = records.iter().map(|(meta, _)| meta.system).collect();
            assert_eq!(systems, vec!["plan", "act"]);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each record renders once, shared across listeners"
        );
    }

    #[test]
    fn narrowing_delivers_only_the_named_systems() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::Off, std::slice::from_ref(&listener));
        api.enable_only(["plan"]);

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        fanout.record(meta_for("act"), &counting_render(&calls));

        let records = listener.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0.system, "plan");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "excluded records never format"
        );
    }

    #[test]
    fn a_runtime_flip_takes_effect_on_the_next_record() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::Off, std::slice::from_ref(&listener));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        api.enable();
        fanout.record(meta_for("plan"), &counting_render(&calls));
        api.disable();
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            listener.records().len(),
            1,
            "only the enabled window delivers"
        );
        assert_eq!(api.policy(), InspectionPolicy::Off);
    }

    #[test]
    fn policy_round_trips_a_narrowed_policy() {
        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        let narrowed = InspectionPolicy::systems(["plan", "act"]);
        api.set_policy(narrowed.clone());
        assert_eq!(
            api.policy(),
            narrowed,
            "a narrowed policy must survive the write/read round trip intact"
        );
    }

    #[test]
    fn redactions_round_trip_through_the_api() {
        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        assert!(api.redactions().is_empty(), "nothing is redacted initially");
        let rules = RedactionRules::new()
            .redact_param("token")
            .redact_type("ApiCredentials");
        api.set_redactions(rules.clone());
        assert_eq!(api.redactions(), rules);
    }

    #[test]
    fn a_param_redaction_delivers_redacted_and_never_formats() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::All, std::slice::from_ref(&listener));
        api.set_redactions(RedactionRules::new().redact_param("memory"));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a covered value must never format"
        );
        let records = listener.records();
        assert_eq!(records.len(), 1, "the record itself must still deliver");
        assert_eq!(records[0].1, Inspection::Redacted);
    }

    #[test]
    fn a_type_redaction_delivers_redacted_and_never_formats() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::All, std::slice::from_ref(&listener));
        api.set_redactions(RedactionRules::new().redact_type("Memory"));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a type-covered value must never format"
        );
        assert_eq!(listener.records()[0].1, Inspection::Redacted);
    }

    #[test]
    fn type_redaction_rules_reach_into_container_spellings() {
        let rules = RedactionRules::new().redact_type("ApiCredentials");
        for spelling in [
            "Option<ApiCredentials>",
            "Vec<ApiCredentials>",
            "Option<credentials::ApiCredentials>",
            "HashMap<String, ApiCredentials>",
            "Wrapper<Inner<ApiCredentials>>",
        ] {
            assert!(
                rules.covers_type(spelling),
                "a rule on the sensitive type must cover {spelling}, the spelling \
                 recorded when a parameter declares it inside a container"
            );
        }
        assert!(
            !rules.covers_type("Option<OtherCredentials>"),
            "descending into a spelling must not drag in unrelated types"
        );
        assert!(
            RedactionRules::new()
                .redact_type("Vec<Token>")
                .covers_type("Vec<token::Token>"),
            "a rule naming a container still matches that spelling as a whole"
        );
    }

    #[test]
    fn a_container_declared_record_is_redacted_and_never_formats() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::All, std::slice::from_ref(&listener));
        api.set_redactions(RedactionRules::new().redact_type("Vault"));

        let calls = AtomicUsize::new(0);
        let meta = ParamMeta::new(
            "open_vault",
            "vault",
            "Option<vault::Vault>",
            ParamKind::Res,
            Phase::Before,
        );
        fanout.record(meta, &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a covered type must never format, however the parameter wraps it"
        );
        assert_eq!(listener.records()[0].1, Inspection::Redacted);
    }

    #[test]
    fn adding_redactions_keeps_rules_from_other_holders() {
        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        api.set_redactions(RedactionRules::new().redact_type("ApiCredentials"));

        // A second holder adding its own rule must not un-redact the first's.
        api.add_redacted_param("token");

        let rules = api.redactions();
        assert!(
            rules.covers_type("ApiCredentials"),
            "an addition must keep the rules already in force"
        );
        assert!(
            rules.covers_param("token"),
            "an addition must apply its own rule"
        );

        // `set_redactions` remains the wholesale door.
        api.set_redactions(RedactionRules::new().redact_param("token"));
        assert!(
            !api.redactions().covers_type("ApiCredentials"),
            "set_redactions must still replace the rule set wholesale"
        );
    }

    #[test]
    fn concurrent_redaction_additions_compose_without_lost_updates() {
        let api = Arc::new(InspectionAPI::new(
            InspectionPolicy::Off,
            RedactionRules::new(),
        ));
        let writers: Vec<_> = ["ApiCredentials", "SessionKey", "AccessToken"]
            .into_iter()
            .map(|name| {
                let api = Arc::clone(&api);
                std::thread::spawn(move || api.add_redacted_type(name))
            })
            .collect();

        for writer in writers {
            writer.join().expect("redaction writer must finish");
        }

        let rules = api.redactions();
        for name in ["ApiCredentials", "SessionKey", "AccessToken"] {
            assert!(
                rules.covers_type(name),
                "a concurrent addition must not lose the `{name}` rule"
            );
        }
    }

    /// The `&str`/`String` payload of a caught panic.
    ///
    /// Asserting on the message rather than `is_err()` keeps panic tests from
    /// passing when an unrelated failure produces an identical `Err`.
    fn panic_message(unwound: Result<(), Box<dyn std::any::Any + Send>>) -> String {
        let payload = unwound.expect_err("the panic must not be absorbed");
        payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_owned())
    }

    #[test]
    fn type_redaction_rules_match_across_path_spellings() {
        let rules = RedactionRules::new().redact_type("credentials::ApiCredentials");
        assert!(
            rules.covers_type("ApiCredentials"),
            "a path-spelled rule must cover the bare spelling"
        );
        assert!(
            rules.covers_type("auth::credentials::ApiCredentials"),
            "any path spelling of the same final segment is covered"
        );
        assert!(
            !rules.covers_type("OtherCredentials"),
            "a different type stays uncovered"
        );
        assert!(
            RedactionRules::new()
                .redact_type("Vec<Token>")
                .covers_type("Vec<token::Token>"),
            "paths inside generic arguments are stripped too"
        );
        assert!(
            RedactionRules::new()
                .redact_type("nested :: Deep")
                .covers_type("Deep"),
            "whitespace in a spelling is ignored"
        );
        assert_eq!(
            RedactionRules::new().redact_type("ApiCredentials"),
            rules,
            "rules normalize at insertion, so path spellings compare equal"
        );
    }

    #[test]
    fn type_redaction_rules_survive_whitespace_separated_spellings() {
        // Whitespace is the other way path-stripping can fail open. Dropping it
        // outright merges the identifiers it separated, and the merged token is
        // not a name any rule is written against, so the value renders.
        for (rule, spelling) in [
            ("Credentials", "Box<dyn Credentials>"),
            ("Credentials", "Box<dyn credentials::Credentials + Send>"),
            ("Token", "&'a Token"),
            ("Secret", "mut Secret"),
            ("Vault", "Option<&'static Vault>"),
            ("Deep", "Box<dyn nested :: Deep>"),
        ] {
            assert!(
                RedactionRules::new()
                    .redact_type(rule)
                    .covers_type(spelling),
                "a rule on `{rule}` must cover `{spelling}`: gluing the \
                 whitespace-separated identifiers together would hide the \
                 sensitive name from both the whole-spelling match and the \
                 descent, and the credential would render"
            );
        }

        assert!(
            !RedactionRules::new()
                .redact_type("dynCredentials")
                .covers_type("Box<dyn Credentials>"),
            "the glued spelling must not be what a working rule has to name"
        );
        assert!(
            !RedactionRules::new()
                .redact_type("Credentials")
                .covers_type("Box<dyn OtherCredentials>"),
            "keeping the identifiers apart must not make matching sloppier"
        );
    }

    #[test]
    fn path_stripping_normalizes_spellings_without_merging_identifiers() {
        for (spelling, stripped) in [
            // Nothing to do: borrowed straight through.
            ("Memory", "Memory"),
            ("Vec<Token>", "Vec<Token>"),
            // Paths collapse to their final segment.
            ("agent::memory::Memory", "Memory"),
            ("nested :: Deep", "Deep"),
            ("Vec<token::Token>", "Vec<Token>"),
            // Whitespace around punctuation carries nothing.
            ("Vec< Token >", "Vec<Token>"),
            ("HashMap<String, Token>", "HashMap<String,Token>"),
            // Whitespace between two identifiers is the one kind that does.
            ("Box<dyn Credentials>", "Box<dyn Credentials>"),
            ("Box<dyn credentials::Credentials>", "Box<dyn Credentials>"),
            ("&'a Token", "&'a Token"),
            ("mut Secret", "mut Secret"),
            ("Box<dyn Credentials + Send>", "Box<dyn Credentials+Send>"),
            // Degenerate spellings: a leading `::` truncates everything
            // written so far, and an empty spelling has no segment at all.
            // Both must still land on something a rule can match, because a
            // panic or a wrong answer here is a redaction that silently
            // stops covering.
            ("::credentials::Token", "Token"),
            ("Vec<::token::Token>", "Vec<Token>"),
            ("", ""),
        ] {
            assert_eq!(
                strip_type_paths(spelling).as_ref(),
                stripped,
                "`{spelling}` must normalize to `{stripped}`"
            );
        }
    }

    #[test]
    fn a_whitespace_separated_record_is_redacted_and_never_formats() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::All, std::slice::from_ref(&listener));
        api.set_redactions(RedactionRules::new().redact_type("Vault"));

        let calls = AtomicUsize::new(0);
        let meta = ParamMeta::new(
            "open_vault",
            "vault",
            "Box<dyn vault::Vault + Send>",
            ParamKind::Res,
            Phase::Before,
        );
        fanout.record(meta, &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a covered type must never format, however the parameter spaces out \
             its spelling"
        );
        assert_eq!(listener.records()[0].1, Inspection::Redacted);
    }

    #[test]
    fn a_rule_naming_a_container_does_not_follow_it_into_deeper_nesting() {
        // The documented negative behind "name the sensitive type, not a
        // container spelling": a container rule matches that spelling whole,
        // and descent finds type *names*, of which `Vec<Token>` is not one.
        let container_rule = RedactionRules::new().redact_type("Vec<Token>");
        assert!(
            container_rule.covers_type("Vec<Token>"),
            "the spelling the rule names is still covered"
        );
        assert!(
            !container_rule.covers_type("Option<Vec<Token>>"),
            "a container rule must not be assumed to follow the container into \
             deeper nesting — this is why the rustdoc says to name the \
             sensitive type itself"
        );
        assert!(
            !container_rule.covers_type("Token"),
            "nor does naming a container cover the type inside it"
        );

        // Naming the sensitive type instead covers every nesting of it.
        let type_rule = RedactionRules::new().redact_type("Token");
        for spelling in ["Token", "Vec<Token>", "Option<Vec<Token>>"] {
            assert!(
                type_rule.covers_type(spelling),
                "a rule on the type itself must cover {spelling}"
            );
        }
    }

    #[test]
    fn a_redacted_value_stays_withheld_when_a_listener_is_disabled() {
        // The two gates are independent: whichever listeners are still
        // receiving must receive the sentinel, and the value must not be
        // formatted for them.
        let named = SharedCollecting::default();
        let unnamed = SharedCollecting::default();
        const TOGGLEABLE: InspectionListenerName = InspectionListenerName::new("toggleable");
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push_named(TOGGLEABLE, named.clone());
        registry.push(unnamed.clone());
        let fanout = registry.fanout(api.shared());

        api.set_redactions(RedactionRules::new().redact_param("memory"));
        assert_eq!(api.disable_listener(TOGGLEABLE), Some(true));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a covered value must never format, disabled listener or not"
        );
        assert!(
            named.records().is_empty(),
            "a disabled listener must not even receive the sentinel"
        );
        let delivered = unnamed.records();
        assert_eq!(
            delivered.len(),
            1,
            "the enabled listener must still receive"
        );
        assert_eq!(
            delivered[0].1,
            Inspection::Redacted,
            "and must receive it withheld"
        );
    }

    #[test]
    fn replacing_the_rule_set_warns_about_every_rule_it_drops() {
        // Thread-local subscriber (a global install would clash with other
        // tests in this binary), plus an interest rebuild so a sibling test
        // that hit this callsite unsubscribed cannot have disabled it.
        let capture = FieldsCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        api.set_redactions(
            RedactionRules::new()
                .redact_type("ApiCredentials")
                .redact_param("token")
                .redact_type("SessionKey"),
        );
        assert!(
            capture.events().is_empty(),
            "installing rules over an empty set drops nothing and must stay quiet"
        );

        // Another holder replaces the set, keeping `token` and losing both
        // type rules. Pin the exact sorted diagnostic: `HashSet` iteration
        // order is arbitrary, and a warning that reorders between runs is one
        // nobody can diff or assert on.
        api.set_redactions(RedactionRules::new().redact_param("token"));

        let events = capture.events();
        assert_eq!(
            events.len(),
            1,
            "dropping a withholding rule must warn once"
        );
        assert_eq!(
            events[0]
                .get("polaris.inspection.method")
                .map(String::as_str),
            Some("set_redactions"),
        );
        assert_eq!(
            events[0]
                .get("polaris.inspection.dropped_rules")
                .map(String::as_str),
            Some(r#"["type `ApiCredentials`", "type `SessionKey`"]"#),
            "every dropped rule must be named and sorted, while the retained \
             `token` rule must stay absent from the diagnostic"
        );
    }

    #[test]
    fn the_api_debug_dump_is_sorted_and_names_only_disabled_listeners() {
        const ALPHA: InspectionListenerName = InspectionListenerName::new("alpha");
        const KILO: InspectionListenerName = InspectionListenerName::new("kilo");
        const MIKE: InspectionListenerName = InspectionListenerName::new("mike");
        const ZULU: InspectionListenerName = InspectionListenerName::new("zulu");

        let api = InspectionAPI::new(InspectionPolicy::Off, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        for name in [ZULU, ALPHA, MIKE, KILO] {
            registry.push_named(name, SharedCollecting::default());
        }
        assert_eq!(api.registered_listeners(), [ALPHA, KILO, MIKE, ZULU]);

        assert_eq!(api.disable_listener(ZULU), Some(true));
        assert_eq!(api.disable_listener(ALPHA), Some(true));
        assert_eq!(api.disable_listener(MIKE), Some(true));
        // An enabled name must not appear: the field is the exception list.
        assert_eq!(api.disable_listener(KILO), Some(true));
        assert_eq!(api.enable_listener(KILO), Some(false));

        let dump = format!("{api:?}");
        assert!(
            dump.contains(r#"disabled_listeners: {"alpha", "mike", "zulu"}"#),
            "the dump must list disabled listeners in sorted order and omit \
             enabled ones, so two dumps diff — got {dump}"
        );
    }

    #[test]
    fn unknown_listener_toggles_are_rejected_without_growing_the_registry() {
        const UNKNOWN: InspectionListenerName = InspectionListenerName::new("never-registered");
        const LATE: InspectionListenerName = InspectionListenerName::new("registers-later");
        let api = InspectionAPI::new(InspectionPolicy::Off, RedactionRules::new());

        assert_eq!(api.listener_enabled(UNKNOWN), None);
        assert_eq!(api.disable_listener(UNKNOWN), None);
        assert_eq!(api.enable_listener(UNKNOWN), None);
        assert!(
            api.toggles().read().is_empty(),
            "unknown lookups and toggles must not create state — got {:?}",
            api.toggles().read().keys().collect::<Vec<_>>()
        );

        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push_named(LATE, SharedCollecting::default());
        assert_eq!(api.registered_listeners(), [LATE]);
        assert_eq!(api.listener_enabled(LATE), Some(true));
        assert_eq!(api.disable_listener(LATE), Some(true));
        assert_eq!(api.listener_enabled(LATE), Some(false));
        assert_eq!(api.enable_listener(LATE), Some(false));
        assert_eq!(api.listener_enabled(LATE), Some(true));
    }

    #[test]
    fn setting_policy_and_redactions_hands_back_what_they_displaced() {
        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        api.set_policy(InspectionPolicy::systems(["plan"]));
        api.set_redactions(RedactionRules::new().redact_type("ApiCredentials"));

        // The operator recipe: open a window, then put back what was in force
        // rather than assuming `Off` and an empty rule set were.
        let before = api.enable();
        assert_eq!(before, InspectionPolicy::systems(["plan"]));
        let displaced_rules = api.set_redactions(RedactionRules::new());
        assert!(displaced_rules.covers_type("ApiCredentials"));

        assert_eq!(api.set_policy(before), InspectionPolicy::All);
        assert_eq!(api.policy(), InspectionPolicy::systems(["plan"]));
        let _ = api.set_redactions(displaced_rules);
        assert!(
            api.redactions().covers_type("ApiCredentials"),
            "restoring the displaced rule set must put the withholding back"
        );
    }

    #[test]
    fn adding_rules_never_warns() {
        let capture = FieldsCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        let (api, _fanout) = fanout_with(InspectionPolicy::Off, &[]);
        api.add_redacted_type("ApiCredentials");
        api.add_redacted_param("token");
        // Replacing a set with a strict superset of itself loses nothing.
        api.set_redactions(
            RedactionRules::new()
                .redact_type("ApiCredentials")
                .redact_param("token")
                .redact_type("SessionKey"),
        );

        assert!(
            capture.events().is_empty(),
            "accumulating rules withholds strictly more and must stay quiet — got {:?}",
            capture.events()
        );

        // Positive control. Everything above asserts an *absence*, which is
        // also what a silenced pipeline produces: `tracing` caches callsite
        // interest process-wide, so a capture that never receives anything
        // cannot tell "nothing was emitted" from "this callsite is off". Drop a
        // rule on purpose and require the warning to land, so the assertions
        // above can only pass while the path they are watching is live.
        api.set_redactions(RedactionRules::new().redact_param("token"));
        assert_eq!(
            capture.events().len(),
            1,
            "control: dropping a rule must warn, or the silence asserted above \
             proves nothing"
        );
    }

    #[test]
    fn a_path_spelled_record_is_redacted_by_a_bare_rule() {
        let listener = SharedCollecting::default();
        let (api, fanout) = fanout_with(InspectionPolicy::All, std::slice::from_ref(&listener));
        api.set_redactions(RedactionRules::new().redact_type("Memory"));

        let calls = AtomicUsize::new(0);
        let meta = ParamMeta::new(
            "plan",
            "memory",
            "agent::memory::Memory",
            ParamKind::ResMut,
            Phase::Before,
        );
        fanout.record(meta, &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a covered value must never format, however the parameter spells the path"
        );
        assert_eq!(listener.records()[0].1, Inspection::Redacted);
    }

    #[test]
    fn disabling_a_named_listener_stops_only_its_delivery() {
        const TOGGLEABLE: InspectionListenerName = InspectionListenerName::new("toggleable");
        let named = SharedCollecting::default();
        let unnamed = SharedCollecting::default();
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push_named(TOGGLEABLE, named.clone());
        registry.push(unnamed.clone());
        let fanout = registry.fanout(api.shared());

        assert_eq!(api.listener_enabled(TOGGLEABLE), Some(true));
        assert_eq!(api.disable_listener(TOGGLEABLE), Some(true));
        assert_eq!(api.listener_enabled(TOGGLEABLE), Some(false));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        assert!(
            named.records().is_empty(),
            "a disabled listener must receive nothing"
        );
        assert_eq!(
            unnamed.records().len(),
            1,
            "other listeners must keep receiving"
        );

        assert_eq!(api.enable_listener(TOGGLEABLE), Some(false));
        assert_eq!(api.listener_enabled(TOGGLEABLE), Some(true));
        fanout.record(meta_for("act"), &counting_render(&calls));
        assert_eq!(
            named.records().len(),
            1,
            "re-enabling must restore delivery"
        );
        assert_eq!(unnamed.records().len(), 2);
    }

    #[test]
    fn late_registration_becomes_discoverable_and_receives_records() {
        const LATE: InspectionListenerName = InspectionListenerName::new("late");
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        let fanout = registry.fanout(api.shared());

        assert_eq!(api.listener_enabled(LATE), None);
        assert!(api.registered_listeners().is_empty());

        // Register after the fan-out exists — the post-`build()` shape.
        let named = SharedCollecting::default();
        let unnamed = SharedCollecting::default();
        registry.push_named(LATE, named.clone());
        registry.push(unnamed.clone());
        assert_eq!(api.registered_listeners(), [LATE]);
        assert_eq!(api.listener_enabled(LATE), Some(true));

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        assert_eq!(
            named.records().len(),
            1,
            "a named sink pushed after the fan-out was installed must receive"
        );
        assert_eq!(
            unnamed.records().len(),
            1,
            "a sink pushed after the fan-out was installed must receive subsequent records"
        );

        assert_eq!(api.disable_listener(LATE), Some(true));
        fanout.record(meta_for("act"), &counting_render(&calls));
        assert_eq!(
            named.records().len(),
            1,
            "the checked toggle must stop a late-registered listener"
        );
        assert_eq!(unnamed.records().len(), 2);
    }

    #[test]
    fn duplicate_named_listeners_deliver_and_toggle_together() {
        const DUPLICATE: InspectionListenerName = InspectionListenerName::new("dup");
        let first = SharedCollecting::default();
        let second = SharedCollecting::default();
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push_named(DUPLICATE, first.clone());
        registry.push_named(DUPLICATE, second.clone());
        let fanout = registry.fanout(api.shared());

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        for listener in [&first, &second] {
            assert_eq!(
                listener.records().len(),
                1,
                "listeners sharing a name must both receive records"
            );
        }

        assert_eq!(api.disable_listener(DUPLICATE), Some(true));
        fanout.record(meta_for("act"), &counting_render(&calls));
        for listener in [&first, &second] {
            assert_eq!(
                listener.records().len(),
                1,
                "one toggle must address every listener under the name"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "when every listener is disabled the second record must stay lazy \
             and never invoke the render closure"
        );
    }

    #[test]
    fn a_listener_panic_unwinds_through_the_fanout() {
        struct PanickingListener;
        impl InspectionSink for PanickingListener {
            fn record(&self, _meta: ParamMeta, _render: &dyn Fn() -> Inspection) {
                panic!("listener exploded");
            }
        }

        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push(PanickingListener);
        let fanout = registry.fanout(api.shared());

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fanout.record(meta_for("plan"), &|| Inspection::Text {
                value: "rendered".to_owned(),
                truncated: false,
            });
        }));
        assert_eq!(
            panic_message(unwound),
            "listener exploded",
            "the listener's own panic must unwind through the fan-out, not be \
             absorbed — and not be masked by a panic inside the fan-out itself"
        );
    }

    #[test]
    fn concurrent_records_invoke_listeners_without_holding_the_fanout_locks() {
        // The fan-out consults its settings and snapshots its listener list
        // *before* invoking anyone, so two records arriving on different threads
        // must be able to sit inside `record` simultaneously. A lock held across
        // listener invocation would serialize them. Each invocation therefore
        // waits for the other to arrive, under a deadline, so the regression
        // fails this assertion instead of hanging the suite.
        struct OverlapListener {
            /// Invocations that have entered `record`.
            inside: Arc<AtomicUsize>,
            /// Invocations that saw another one inside at the same time.
            overlaps: Arc<AtomicUsize>,
        }

        impl InspectionSink for OverlapListener {
            fn record(&self, _meta: ParamMeta, _render: &dyn Fn() -> Inspection) {
                self.inside.fetch_add(1, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while self.inside.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                if self.inside.load(Ordering::SeqCst) >= 2 {
                    self.overlaps.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let inside = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push(OverlapListener {
            inside: Arc::clone(&inside),
            overlaps: Arc::clone(&overlaps),
        });
        let fanout = Arc::new(registry.fanout(api.shared()));

        let recorders: Vec<_> = ["plan", "act"]
            .into_iter()
            .map(|system| {
                let fanout = Arc::clone(&fanout);
                std::thread::spawn(move || {
                    fanout.record(meta_for(system), &|| Inspection::Text {
                        value: "rendered".to_owned(),
                        truncated: false,
                    });
                })
            })
            .collect();
        for recorder in recorders {
            recorder.join().expect("both records must complete");
        }

        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            2,
            "both listener invocations must observe the other inside `record`; \
             serialized invocations mean a fan-out lock is held across delivery"
        );
    }

    #[tokio::test]
    async fn build_provides_the_api_and_registry_with_recording_off() {
        let mut server = Server::new();
        server.add_plugins(InspectionPlugin::default());
        server.finish().await.expect("server must build");

        assert!(
            server.contains_resource::<InspectionSinkRegistry>(),
            "build() must insert InspectionSinkRegistry as a resource"
        );
        let api = server.api::<InspectionAPI>().expect("API must be inserted");
        assert_eq!(
            api.policy(),
            InspectionPolicy::Off,
            "recording defaults to off"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Tracing listener: field rendering
    //
    // The span-correlation and production-wiring scenario lives with the
    // per-step span middleware, in `tracing_plugin::instrument::graph`.
    // ─────────────────────────────────────────────────────────────────────────

    /// Collects the fields of every `polaris::inspection` event.
    #[derive(Clone, Default)]
    struct FieldsCapture(Arc<Mutex<Vec<HashMap<String, String>>>>);

    impl FieldsCapture {
        fn events(&self) -> Vec<HashMap<String, String>> {
            self.0.lock().clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FieldsCapture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
            if event.metadata().target() != "polaris::inspection" {
                return;
            }
            let mut visitor = FieldMapVisitor::default();
            event.record(&mut visitor);
            self.0.lock().push(visitor.0);
        }
    }

    #[test]
    fn the_tracing_listener_renders_sentinels_and_truncation() {
        // Thread-local subscriber (a global install would clash with other
        // tests in this binary), plus an interest rebuild so a sibling test
        // that hit this callsite unsubscribed cannot have disabled it.
        let capture = FieldsCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        let sink = TracingInspectionSink;
        sink.record(meta_for("plan"), &|| Inspection::Redacted);
        sink.record(meta_for("plan"), &|| Inspection::Opaque("no rendering"));
        sink.record(meta_for("plan"), &|| Inspection::Text {
            value: "cut".to_owned(),
            truncated: true,
        });

        let events = capture.events();
        assert_eq!(events.len(), 3, "each record must emit one event");
        let field = |index: usize, name: &str| events[index].get(name).map(String::as_str);
        // Values are emitted through `Debug`, so every rendering arrives quoted.
        assert_eq!(
            field(0, "polaris.inspection.value"),
            Some(r#""<redacted>""#),
            "a Redacted record must render as the redaction sentinel"
        );
        assert_eq!(
            field(0, "polaris.inspection.rendering"),
            Some("redacted"),
            "the variant must be reported in its own field"
        );
        assert_eq!(
            field(0, "polaris.inspection.truncated"),
            Some("false"),
            "the redaction sentinel is never truncated"
        );
        assert_eq!(
            field(1, "polaris.inspection.value"),
            Some(r#""<opaque: no rendering>""#),
            "an Opaque record must render its reason inside the sentinel"
        );
        assert_eq!(field(1, "polaris.inspection.rendering"), Some("opaque"));
        assert_eq!(
            field(1, "polaris.inspection.truncated"),
            Some("false"),
            "the opaque sentinel is never truncated"
        );
        assert_eq!(
            field(2, "polaris.inspection.value"),
            Some(r#""cut""#),
            "a Text record must pass its rendering through unchanged"
        );
        assert_eq!(field(2, "polaris.inspection.rendering"), Some("text"));
        assert_eq!(
            field(2, "polaris.inspection.truncated"),
            Some("true"),
            "a truncated rendering must be flagged on the event"
        );
    }

    #[test]
    fn a_rendered_value_cannot_forge_log_structure_or_the_redaction_sentinel() {
        // Thread-local subscriber (a global install would clash with other
        // tests in this binary), plus an interest rebuild so a sibling test
        // that hit this callsite unsubscribed cannot have disabled it.
        let capture = FieldsCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        // A hand-written `Debug` writes whatever bytes it likes, so treat the
        // rendering as attacker-influenced: newlines that would forge a second
        // log line, and the redaction sentinel spelled out verbatim.
        let sink = TracingInspectionSink;
        sink.record(meta_for("plan"), &|| Inspection::Text {
            value: "first\nlevel=ERROR forged=true".to_owned(),
            truncated: false,
        });
        sink.record(meta_for("plan"), &|| Inspection::Text {
            value: "<redacted>".to_owned(),
            truncated: false,
        });

        let events = capture.events();
        let field = |index: usize, name: &str| events[index].get(name).map(String::as_str);

        let forged = field(0, "polaris.inspection.value").expect("the value field must be present");
        assert!(
            !forged.contains('\n'),
            "a newline in a rendering must be escaped, not passed through into \
             the log — got {forged}"
        );
        assert_eq!(
            forged, r#""first\nlevel=ERROR forged=true""#,
            "the rendering must arrive quoted and escaped, as one field"
        );

        // The value renders to exactly what the sentinel renders to...
        assert_eq!(
            field(1, "polaris.inspection.value"),
            field(0, "polaris.inspection.value").and(Some(r#""<redacted>""#)),
            "sanity: this value is spelled exactly like the redaction sentinel"
        );
        // ...so `value` alone cannot answer "was this withheld". The dedicated
        // field can, because this listener writes it rather than the value.
        assert_eq!(
            field(1, "polaris.inspection.rendering"),
            Some("text"),
            "a real value spelled `<redacted>` must not be reported as redacted"
        );
    }

    #[test]
    fn a_filtered_out_target_stops_the_tracing_listener_formatting_at_all() {
        // The documented `RUST_LOG=...polaris::inspection=off` route promises
        // more than a quiet subscriber: the value must never reach a `String`
        // through this listener. That only holds if the filter is consulted
        // before the render closure runs.
        let capture = FieldsCapture::default();
        let subscriber = Registry::default()
            .with(capture.clone())
            .with(EnvFilter::new("info,polaris::inspection=off"));
        let _guard = set_default_and_rebuild(subscriber);

        let calls = AtomicUsize::new(0);
        TracingInspectionSink.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a filtered-out target must not format the value"
        );
        assert!(capture.events().is_empty(), "and must emit no event either");
        drop(_guard);

        // Positive control: the same record under the same listener, differing
        // only in that the target is not filtered out. Without this, a dead
        // callsite would satisfy both assertions above and the test would pass
        // while proving nothing about the filter.
        let unfiltered = FieldsCapture::default();
        let _guard = set_default_and_rebuild(Registry::default().with(unfiltered.clone()));
        TracingInspectionSink.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "control: an unfiltered target must format the value, or the zero \
             above is not attributable to the filter"
        );
        assert_eq!(
            unfiltered.events().len(),
            1,
            "control: an unfiltered target must emit the event"
        );
    }

    #[test]
    fn disabling_the_tracing_listener_stops_events_but_not_other_listeners() {
        // Thread-local subscriber (a global install would clash with other
        // tests in this binary), plus an interest rebuild so a sibling test
        // that hit this callsite unsubscribed cannot have disabled it.
        let capture = FieldsCapture::default();
        let subscriber = Registry::default().with(capture.clone());
        let _guard = set_default_and_rebuild(subscriber);

        // The plugin's own registration shape: the shipped sink under its
        // public name, next to an always-on consumer listener.
        let other = SharedCollecting::default();
        let api = InspectionAPI::new(InspectionPolicy::All, RedactionRules::new());
        let mut registry = InspectionSinkRegistry::new(api.toggles());
        registry.push_named(INSPECTION_TRACING_LISTENER, TracingInspectionSink);
        registry.push(other.clone());
        let fanout = registry.fanout(api.shared());

        let calls = AtomicUsize::new(0);
        fanout.record(meta_for("plan"), &counting_render(&calls));
        assert_eq!(
            capture.events().len(),
            1,
            "the tracing listener must emit while enabled"
        );

        assert_eq!(
            api.disable_listener(INSPECTION_TRACING_LISTENER),
            Some(true),
            "the shipped tracing listener must be registered under its public name"
        );
        fanout.record(meta_for("plan"), &counting_render(&calls));

        assert_eq!(
            capture.events().len(),
            1,
            "a disabled tracing listener must emit no further events"
        );
        assert_eq!(
            other.records().len(),
            2,
            "the other listener must keep receiving across the toggle"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Spec parsing
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn policy_spec_keywords_are_case_insensitive_and_empty_means_off() {
        for spec in ["", "   ", "off", "OFF", " Off "] {
            assert_eq!(
                spec.parse(),
                Ok(InspectionPolicy::Off),
                "spec {spec:?} must parse as Off"
            );
        }
        for spec in ["all", "ALL", " All "] {
            assert_eq!(
                spec.parse(),
                Ok(InspectionPolicy::All),
                "spec {spec:?} must parse as All"
            );
        }
    }

    #[test]
    fn policy_spec_names_narrow_and_tolerate_spacing_and_trailing_commas() {
        let policy: InspectionPolicy = " plan , act ,".parse().expect("two system names");
        assert_eq!(policy, InspectionPolicy::systems(["plan", "act"]));

        // A keyword is only a keyword when the spec is exactly that keyword:
        // inside a list, `all` is an ordinary system name.
        let policy: InspectionPolicy = "all,plan".parse().expect("a list containing `all`");
        assert_eq!(policy, InspectionPolicy::systems(["all", "plan"]));
    }

    #[test]
    fn policy_spec_rejects_what_would_silently_record_nothing() {
        assert_eq!(
            ",".parse::<InspectionPolicy>(),
            Err(InspectionSpecError::EmptyPolicy),
            "punctuation-only must not quietly mean Off"
        );
        // The classic forgotten comma: one entry spelled like two names.
        assert_eq!(
            "plan act".parse::<InspectionPolicy>(),
            Err(InspectionSpecError::MalformedSystem {
                entry: "plan act".to_owned()
            }),
            "a non-identifier entry must error, not become a name matching no system"
        );
        assert!(
            "plan-9".parse::<InspectionPolicy>().is_err(),
            "identifier validation must reject punctuation inside a name"
        );
    }

    #[test]
    fn redaction_spec_builds_the_same_rules_as_the_typed_builders() {
        let rules: RedactionRules = " param : token , type : ApiCredentials "
            .parse()
            .expect("one binding rule, one type rule, spacing tolerated");
        assert_eq!(
            rules,
            RedactionRules::new()
                .redact_param("token")
                .redact_type("ApiCredentials"),
            "the spec grammar must be a spelling of the typed builders, not a second rule system"
        );

        // Path-qualified spellings strip exactly as redact_type strips them.
        let rules: RedactionRules = "type:credentials::ApiCredentials"
            .parse()
            .expect("a path-qualified type rule");
        assert_eq!(
            rules,
            RedactionRules::new().redact_type("credentials::ApiCredentials")
        );
        assert!(rules.covers_type("ApiCredentials"));

        let empty: RedactionRules = "".parse().expect("an empty spec is a valid no-rule spec");
        assert!(empty.is_empty());
    }

    #[test]
    fn redaction_spec_rejects_every_fail_open_shape() {
        // Each of these, accepted leniently, would be a withholding rule that
        // withholds nothing. The parse must refuse them all, verbatim.
        for entry in [
            "token",           // no kind prefix: would have to be guessed at
            "parm:token",      // misspelled kind
            "param:",          // kind without a name
            "type:Vec<Token>", // container spelling: ambiguous under comma-splitting
            "param:a b",       // non-identifier binding name
        ] {
            assert_eq!(
                entry.parse::<RedactionRules>(),
                Err(InspectionSpecError::MalformedRedaction {
                    entry: entry.to_owned()
                }),
                "entry {entry:?} must be rejected"
            );
        }
    }

    #[test]
    fn env_spec_builders_apply_set_variables_and_leave_unset_ones_alone() {
        // The environment read itself (`env_spec`) is exercised through a
        // variable that is never set — read-only, so it cannot race the panic
        // machinery's own environment reads elsewhere in the binary. The
        // set-variable paths go through the builders' testable halves, which
        // is everything after that read.
        let untouched = InspectionPlugin::new()
            .with_policy(InspectionPolicy::All)
            .with_policy_from_env("POLARIS_TEST_INSPECTION_NEVER_SET")
            .with_redactions_from_env("POLARIS_TEST_INSPECTION_NEVER_SET");
        assert_eq!(
            untouched.initial_policy,
            InspectionPolicy::All,
            "an unset variable must change nothing, not reset to the default"
        );
        assert!(untouched.initial_redactions.is_empty());

        let plugin = InspectionPlugin::new()
            .with_redactions(RedactionRules::new().redact_type("BakedIn"))
            .policy_from_spec("VAR", Some("plan,act".to_owned()))
            .redactions_from_spec("VAR", Some("param:token,type:ApiCredentials".to_owned()));
        assert_eq!(
            plugin.initial_policy,
            InspectionPolicy::systems(["plan", "act"])
        );
        assert!(plugin.initial_redactions.covers_param("token"));
        assert!(plugin.initial_redactions.covers_type("ApiCredentials"));
        assert!(
            plugin.initial_redactions.covers_type("BakedIn"),
            "an environment spec adds rules; it must never drop one the binary baked in"
        );
    }

    #[test]
    fn a_malformed_env_spec_fails_the_boot_instead_of_guessing() {
        let unwound = std::panic::catch_unwind(|| {
            InspectionPlugin::new().policy_from_spec("MYAPP_INSPECT", Some("plan act".to_owned()))
        });
        let message = *unwound
            .expect_err("a malformed policy spec must panic, not narrow to a guess")
            .downcast::<String>()
            .expect("the panic carries a formatted message");
        assert!(
            message.contains("MYAPP_INSPECT"),
            "the panic must name the variable to fix, got: {message}"
        );

        let unwound = std::panic::catch_unwind(|| {
            InspectionPlugin::new()
                .redactions_from_spec("MYAPP_REDACT", Some("parm:token".to_owned()))
        });
        let message = *unwound
            .expect_err("a malformed redaction spec must panic, not withhold nothing")
            .downcast::<String>()
            .expect("the panic carries a formatted message");
        assert!(
            message.contains("MYAPP_REDACT") && message.contains("parm:token"),
            "the panic must name the variable and the offending entry, got: {message}"
        );
    }
}
