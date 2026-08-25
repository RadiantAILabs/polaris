//! Opt-in capture of system parameter values.
//!
//! Systems already declare *which* resources and outputs they touch through
//! their typed parameters; [`SystemAccess`](super::SystemAccess) exposes that
//! declaration structurally. This module adds the missing half: reading the
//! **values** behind those declarations at runtime.
//!
//! # Where capture happens
//!
//! Capture happens at parameter resolution, inside the body the
//! [`#[system]`](macro@crate::system) macro generates. That is the only point
//! where a parameter is still statically typed, which is what makes this
//! mechanism cheap and total:
//!
//! - no downcast and no reflection vtable — the concrete type is in scope;
//! - nothing for a resource author to implement, so it works on types from
//!   third-party crates that the caller does not own;
//! - a value whose [`ResMut`] borrow is held is still readable, because the
//!   capture *is* the borrow rather than a competing one.
//!
//! # Opting in
//!
//! A type becomes renderable by deriving [`Debug`]. Selection is per system and
//! per parameter, via the macro:
//!
//! ```
//! # use polaris_system::param::{Res, ResMut};
//! # use polaris_system::resource::{GlobalResource, LocalResource};
//! # use polaris_system::system;
//! #[derive(Debug)]
//! struct Config { verbose: bool }
//! # impl GlobalResource for Config {}
//!
//! #[derive(Debug)]
//! struct Memory { messages: Vec<String> }
//! # impl LocalResource for Memory {}
//!
//! #[system(inspect(memory))]
//! async fn process(config: Res<Config>, mut memory: ResMut<Memory>) {
//!     memory.messages.push(format!("verbose={}", config.verbose));
//! }
//! ```
//!
//! Naming a parameter whose type is not [`Debug`] is a compile error at that
//! parameter, not a silent omission.
//!
//! # Whether capture is live
//!
//! Selection is a compile-time concern; *activation* is a separate runtime one.
//! Nothing is captured unless a sink is installed on the context, and even then
//! the sink decides whether to render — [`InspectionSink::record`] receives the
//! value as a closure, so formatting is not paid for a record the sink drops.
//!
//! # Sensitive values
//!
//! A captured value renders through the type's **own** [`Debug`] impl, so the
//! standard redaction idiom composes: a hand-written `Debug` that masks a
//! secret field is honored by the capture path. Selection is the gate *this*
//! layer provides — do not name a parameter in `inspect(..)` whose derived
//! `Debug` would expose credentials or other sensitive data. Whether a selected
//! value is ever rendered is then the installed sink's call, and a policy layer
//! above may narrow further still; this layer assumes nothing about one.
//! [`Inspection::Redacted`] is the vocabulary for a sink or policy that
//! withholds a reachable value.
//!
//! This layer defines no telemetry vocabulary and stores nothing. Policy,
//! storage, and export belong to the plugin layer.

use std::fmt::{self, Debug, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::{ErrOut, ErrorContext, Out, Res, ResMut};
use crate::resource::{LocalResource, Output, Resource};

/// Default byte ceiling applied by [`Inspection::debug`].
///
/// Rendered values are capped because a `Debug` of accumulated state — a
/// conversation history, a diagnostics list — can reach megabytes, and an
/// observability path must not be able to exhaust memory on a large resource.
pub const DEFAULT_MAX_BYTES: usize = 4096;

/// A rendered parameter value.
///
/// Marked `#[non_exhaustive]`: further renderings may be added, so downstream
/// matches must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Inspection {
    /// A rendered value, possibly cut short at [`DEFAULT_MAX_BYTES`].
    Text {
        /// The rendered text.
        value: String,
        /// Whether `value` was cut short of the full rendering.
        truncated: bool,
    },
    /// The value was deliberately withheld as sensitive.
    ///
    /// Distinct from [`Opaque`](Self::Opaque): the value was reachable and a
    /// policy declined to render it.
    Redacted,
    /// The value could not be rendered, with a static reason.
    ///
    /// Distinct from a missing record: the parameter *was* captured, but no
    /// rendering was available for it.
    Opaque(&'static str),
}

/// A [`fmt::Write`] destination with a fixed byte budget.
///
/// Once the budget is exceeded, `write_str` refuses input with [`fmt::Error`],
/// which the formatting machinery propagates immediately — so a rendering that
/// would exceed the cap **stops mid-render** rather than materializing the
/// full value and trimming afterwards. A multi-megabyte accumulated resource
/// costs at most the cap, not its full size.
///
/// The writer is *fused*: after the first refusal every later call is refused
/// too, including a fragment small enough to fit budget left unused by the
/// `char`-boundary back-off. A `Debug` impl that swallows the error and keeps
/// writing (a contract violation) therefore cannot splice post-cut fragments
/// onto the truncated prefix — the retained text is always a prefix of the
/// true rendering, and the invariant `buf.len() <= max` holds regardless.
struct TruncatingWriter {
    buf: String,
    max: usize,
    truncated: bool,
}

impl fmt::Write for TruncatingWriter {
    fn write_str(&mut self, fragment: &str) -> fmt::Result {
        // Fused: nothing is accepted after a truncation. `truncated` doubles
        // as the fuse — it is only ever set on the refusal path below.
        if self.truncated {
            return Err(fmt::Error);
        }

        let remaining = self.max - self.buf.len();
        if fragment.len() <= remaining {
            self.buf.push_str(fragment);
            return Ok(());
        }

        // Keep what fits, backing off to a `char` boundary so the retained
        // prefix stays valid UTF-8.
        let mut end = remaining;
        while end > 0 && !fragment.is_char_boundary(end) {
            end -= 1;
        }
        self.buf.push_str(&fragment[..end]);
        self.truncated = true;
        Err(fmt::Error)
    }
}

impl Inspection {
    /// Renders `value` with [`Debug`], capped at [`DEFAULT_MAX_BYTES`].
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::inspect::Inspection;
    ///
    /// assert_eq!(
    ///     Inspection::debug(&vec![1, 2]),
    ///     Inspection::Text { value: "[1, 2]".into(), truncated: false },
    /// );
    /// ```
    #[must_use]
    pub fn debug<T: Debug + ?Sized>(value: &T) -> Self {
        Self::debug_capped(value, DEFAULT_MAX_BYTES)
    }

    /// Renders `value` with [`Debug`], stopping at `max_bytes`.
    ///
    /// This is the funnel every rendering path goes through, and it treats the
    /// `Debug` impl as untrusted code at a boundary:
    ///
    /// - Formatting **stops** once the cap is reached — the full value is never
    ///   materialized, so a huge resource costs at most `max_bytes`. The writer
    ///   refuses everything after the first truncation, so the retained text is
    ///   always a prefix of the true rendering.
    /// - A `Debug` impl that **panics** is absorbed with [`catch_unwind`] and
    ///   recorded as [`Opaque`](Self::Opaque) — observability must not be able
    ///   to take down the system it observes. (Best-effort: unwinding cannot be
    ///   caught under `panic = "abort"`.)
    /// - A `Debug` impl that returns [`fmt::Error`] without the writer having
    ///   refused input (a contract violation) is likewise recorded as
    ///   [`Opaque`](Self::Opaque) rather than trusted as partial text.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::inspect::Inspection;
    ///
    /// let rendered = Inspection::debug_capped(&vec![1, 2, 3], 8);
    /// assert_eq!(
    ///     rendered,
    ///     Inspection::Text { value: "[1, 2, 3".into(), truncated: true },
    /// );
    /// ```
    #[must_use]
    pub fn debug_capped<T: Debug + ?Sized>(value: &T, max_bytes: usize) -> Self {
        let mut writer = TruncatingWriter {
            buf: String::new(),
            max: max_bytes,
            truncated: false,
        };

        // AssertUnwindSafe: the closure touches `value` (by shared reference)
        // and `writer`. On panic the partially written buffer is discarded, so
        // the writer never crosses the boundary in a broken form. `value` can:
        // a `Debug` impl that mutates interior-mutable state and panics
        // mid-update leaves that state as-is, and the system body still runs
        // against it. The residual assumption is that `Debug` impls are
        // read-only or panic-atomic — the same assumption every caller of
        // `format!` on shared data already makes.
        let outcome = catch_unwind(AssertUnwindSafe(|| write!(writer, "{value:?}")));

        match outcome {
            Err(panic_payload) => {
                // The payload is untrusted too: `panic_any` can carry a value
                // whose own `Drop` panics, which would otherwise re-raise after
                // the catch and escape this boundary. Dropping it inside its
                // own guard keeps the absorption total.
                let _ = catch_unwind(AssertUnwindSafe(move || drop(panic_payload)));
                Self::Opaque("Debug implementation panicked")
            }
            Ok(Err(fmt::Error)) if !writer.truncated => {
                Self::Opaque("Debug implementation errored")
            }
            Ok(_) => Self::Text {
                value: writer.buf,
                truncated: writer.truncated,
            },
        }
    }

    /// Wraps an already-rendered string, capping it at `max_bytes`.
    ///
    /// Truncation lands on a `char` boundary, so the result is always valid
    /// UTF-8 even when the cap falls inside a multi-byte character. This means
    /// the retained text may be slightly shorter than `max_bytes`.
    ///
    /// The capture path never calls this — it exists for sinks and policy
    /// layers that produce their own text (a summary, a projection) and want
    /// the same bounded-size guarantee as rendered values.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::inspect::Inspection;
    ///
    /// let wrapped = Inspection::text_capped("summary".into(), 64);
    /// assert_eq!(
    ///     wrapped,
    ///     Inspection::Text { value: "summary".into(), truncated: false },
    /// );
    /// ```
    #[must_use]
    pub fn text_capped(mut value: String, max_bytes: usize) -> Self {
        if value.len() <= max_bytes {
            return Self::Text {
                value,
                truncated: false,
            };
        }

        let mut end = max_bytes;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);

        Self::Text {
            value,
            truncated: true,
        }
    }
}

/// Which parameter kind a record came from.
///
/// This recovers information [`SystemAccess`](super::SystemAccess) structurally
/// loses: [`Out`] and [`ErrOut`] both declare a read of the output channel, so
/// the access descriptor cannot tell them apart, while the macro sees the
/// wrapper the author actually wrote.
///
/// Marked `#[non_exhaustive]`: further kinds may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParamKind {
    /// A [`Res<T>`] parameter.
    Res,
    /// A [`ResMut<T>`] parameter.
    ResMut,
    /// An [`Out<T>`] parameter.
    Out,
    /// An [`ErrOut<T>`] parameter.
    ErrOut,
    /// The system's own return value, which is not a parameter.
    Return,
    /// A parameter whose wrapper the macro could not classify, such as one
    /// written through a type alias.
    Other,
}

/// When a value was captured, relative to the system body.
///
/// Marked `#[non_exhaustive]`: further phases may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Captured after resolution but before the body ran — the value going in.
    Before,
    /// Captured after the body ran — the value coming out.
    After,
}

/// Identifies what a record describes.
///
/// Every field is `&'static str` or a plain enum, so a record carries no borrow
/// of the value it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ParamMeta {
    /// Name of the system the record came from.
    ///
    /// Present so a record self-identifies without the sink needing to
    /// correlate against separately-carried execution metadata.
    pub system: &'static str,
    /// The parameter's binding name as written in the system signature, or
    /// `"return"` for [`ParamKind::Return`].
    pub param: &'static str,
    /// The `T` inside the wrapper (`Memory`, not `ResMut<'_, Memory>`), as
    /// declared at the parameter; the wrapper is dropped because
    /// [`kind`](Self::kind) already carries it.
    ///
    /// Invariant across wrapper and lifetime forms but not across paths:
    /// `Res<Deep>` and `Res<nested::Deep>` record different names for the
    /// same resource, and an alias records the alias. Falls back to the full
    /// declared type when the wrapper is unclassified
    /// ([`ParamKind::Other`]).
    pub type_name: &'static str,
    /// Which parameter kind this record came from.
    pub kind: ParamKind,
    /// When the value was captured.
    pub phase: Phase,
}

impl ParamMeta {
    /// Builds a record descriptor.
    ///
    /// The macro calls this rather than constructing the struct directly, since
    /// `ParamMeta` is `#[non_exhaustive]` and the generated code lives in the
    /// caller's crate. The five arguments are the record's identity, and this
    /// signature is stable: a future field will carry a default here and be
    /// set through a consuming `with_*` builder method (the
    /// [`SystemContext::with`](super::SystemContext::with) house style) rather
    /// than growing the arity.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::param::inspect::{ParamKind, ParamMeta, Phase};
    ///
    /// let meta = ParamMeta::new("plan", "memory", "Memory", ParamKind::ResMut, Phase::Before);
    /// assert_eq!(meta.param, "memory");
    /// ```
    #[must_use]
    pub fn new(
        system: &'static str,
        param: &'static str,
        type_name: &'static str,
        kind: ParamKind,
        phase: Phase,
    ) -> Self {
        Self {
            system,
            param,
            type_name,
            kind,
            phase,
        }
    }
}

/// Receives captured parameter values.
///
/// Install one on a context with
/// [`SystemContext::replace_inspection`](super::SystemContext::replace_inspection);
/// child contexts inherit it.
///
/// # Cost
///
/// `render` is a closure, not a rendered value. A sink that drops a record —
/// because policy excludes this system, this parameter, or this kind — simply
/// does not call it, and no formatting happens. Implementations should decide
/// *before* rendering.
///
/// [`record`](Self::record) is invoked **synchronously on the system's
/// execution path**, between parameter resolution and the body (or right after
/// it, for a return value). A sink must not block or perform I/O there — hand
/// the record off to a channel or buffer and drain it elsewhere. It must not
/// panic either: only panics inside the value's `Debug` impl are absorbed at
/// the render boundary — a panic thrown by `record` itself unwinds through the
/// system being observed.
///
/// # Example
///
/// ```
/// use polaris_system::param::inspect::{Inspection, InspectionSink, ParamMeta};
/// use std::sync::Mutex;
///
/// #[derive(Default)]
/// struct CollectingSink {
///     records: Mutex<Vec<(ParamMeta, Inspection)>>,
/// }
///
/// impl InspectionSink for CollectingSink {
///     fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection) {
///         // Only pay for rendering the parameters this sink wants.
///         if meta.param == "memory" {
///             let mut records = self.records.lock().expect("sink mutex poisoned");
///             records.push((meta, render()));
///         }
///     }
/// }
/// ```
pub trait InspectionSink: Send + Sync {
    /// Offers a captured value to the sink.
    ///
    /// Call `render` to obtain the value; skip it to decline the record without
    /// paying for formatting.
    fn record(&self, meta: ParamMeta, render: &dyn Fn() -> Inspection);
}

/// A system parameter that can render the value it resolved to.
///
/// Implemented by this crate for [`Res`], [`ResMut`], [`Out`] and [`ErrOut`]
/// whenever the wrapped type is [`Debug`]. These are blanket impls over
/// Polaris's *own* wrapper types, which is why opting in needs no trait impl
/// from the caller and works on types they do not own — the orphan rule never
/// enters into it.
///
/// Implementing this by hand is not expected. It exists as a trait so the
/// generated code can name a single function across all four wrappers.
pub trait InspectParam {
    /// Renders the resolved value.
    ///
    /// Takes `this` rather than `&self` — the same convention as
    /// [`Arc::clone`](std::sync::Arc::clone) — so it can never shadow an
    /// `inspect` method on the wrapped type reached through `Deref` (an
    /// iterator's [`Iterator::inspect`], say). Call it as
    /// `InspectParam::inspect(&param)`.
    fn inspect(this: &Self) -> Inspection;
}

impl<T: Resource + Debug> InspectParam for Res<'_, T> {
    fn inspect(this: &Self) -> Inspection {
        Inspection::debug(&**this)
    }
}

impl<T: LocalResource + Debug> InspectParam for ResMut<'_, T> {
    fn inspect(this: &Self) -> Inspection {
        Inspection::debug(&**this)
    }
}

impl<T: Output + Debug> InspectParam for Out<'_, T> {
    fn inspect(this: &Self) -> Inspection {
        Inspection::debug(&**this)
    }
}

impl<T: ErrorContext + Debug> InspectParam for ErrOut<'_, T> {
    fn inspect(this: &Self) -> Inspection {
        Inspection::debug(&**this)
    }
}

/// `Option<Out<T>>` is a supported parameter type, so it is inspectable too.
///
/// A `None` renders as [`Opaque`](Inspection::Opaque) rather than being dropped:
/// "the upstream system produced no `T`" is itself the observation worth having.
impl<T: Output + Debug> InspectParam for Option<Out<'_, T>> {
    fn inspect(this: &Self) -> Inspection {
        match this {
            Some(output) => InspectParam::inspect(output),
            None => Inspection::Opaque("output absent"),
        }
    }
}

/// Renders a bare value that is not wrapped in a system parameter.
///
/// Used by the generated code for a system's return value, which is a plain `T`
/// rather than one of the parameter wrappers. A blanket
/// `impl<T: Debug> InspectParam for T` cannot serve this purpose: it would
/// overlap the four wrapper impls, since a wrapper may itself be [`Debug`].
///
/// # Example
///
/// ```
/// use polaris_system::param::inspect::{Inspection, inspect_value};
///
/// assert_eq!(
///     inspect_value("done"),
///     Inspection::Text { value: "\"done\"".into(), truncated: false },
/// );
/// ```
#[must_use]
pub fn inspect_value<T: Debug + ?Sized>(value: &T) -> Inspection {
    Inspection::debug(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_under_cap_is_not_truncated() {
        let inspection = Inspection::text_capped("hello".into(), 16);
        assert_eq!(
            inspection,
            Inspection::Text {
                value: "hello".into(),
                truncated: false,
            }
        );
    }

    #[test]
    fn text_at_exactly_cap_is_not_truncated() {
        let inspection = Inspection::text_capped("abcd".into(), 4);
        assert_eq!(
            inspection,
            Inspection::Text {
                value: "abcd".into(),
                truncated: false,
            }
        );
    }

    #[test]
    fn text_over_cap_is_truncated() {
        let inspection = Inspection::text_capped("abcdefgh".into(), 4);
        assert_eq!(
            inspection,
            Inspection::Text {
                value: "abcd".into(),
                truncated: true,
            }
        );
    }

    #[test]
    fn truncation_backs_off_to_char_boundary() {
        // "€" is three bytes, so a cap of 4 lands mid-character and must back
        // off to 3 rather than split the encoding.
        let inspection = Inspection::text_capped("€€".into(), 4);
        assert_eq!(
            inspection,
            Inspection::Text {
                value: "€".into(),
                truncated: true,
            }
        );
    }

    #[test]
    fn truncation_to_zero_yields_empty_but_truncated() {
        // A cap smaller than the first character cannot retain anything, and
        // must still report that content was dropped.
        let inspection = Inspection::text_capped("€".into(), 1);
        assert_eq!(
            inspection,
            Inspection::Text {
                value: String::new(),
                truncated: true,
            }
        );
    }

    #[test]
    fn debug_renders_through_the_debug_impl() {
        #[derive(Debug)]
        struct Payload {
            #[expect(
                dead_code,
                reason = "read only through the derived Debug impl, which is what this asserts"
            )]
            count: u8,
        }

        assert_eq!(
            Inspection::debug(&Payload { count: 7 }),
            Inspection::Text {
                value: "Payload { count: 7 }".into(),
                truncated: false,
            }
        );
    }

    #[test]
    fn debug_capped_truncates_long_renderings() {
        let long = "x".repeat(DEFAULT_MAX_BYTES * 2);

        let Inspection::Text { value, truncated } = Inspection::debug(&long) else {
            panic!("debug of a String must render as text");
        };
        assert!(truncated, "a rendering past the cap must report truncation");
        assert!(value.len() <= DEFAULT_MAX_BYTES);
    }

    #[test]
    fn rendering_stops_at_the_cap_instead_of_rendering_everything() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Emits 1,000 fragments, counting how many the writer accepted.
        struct Endless<'a> {
            accepted: &'a AtomicUsize,
        }

        impl Debug for Endless<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for _ in 0..1_000 {
                    f.write_str("0123456789abcdef")?;
                    self.accepted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
        }

        let accepted = AtomicUsize::new(0);
        let Inspection::Text { value, truncated } = Inspection::debug(&Endless {
            accepted: &accepted,
        }) else {
            panic!("must render as text");
        };

        assert!(truncated);
        assert!(value.len() <= DEFAULT_MAX_BYTES);
        // 1,000 fragments (16 KiB) were on offer; the cap must have stopped the
        // rendering, not trimmed it afterwards.
        assert!(
            accepted.load(Ordering::Relaxed) < 1_000,
            "formatting continued past the cap: {} fragments accepted",
            accepted.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn writer_truncation_backs_off_to_char_boundary_mid_fragment() {
        /// Emits one 6-byte fragment of two 3-byte chars.
        struct Euros;

        impl Debug for Euros {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("€€")
            }
        }

        // A cap of 5 lands inside the second `€` and must back off to 3.
        assert_eq!(
            Inspection::debug_capped(&Euros, 5),
            Inspection::Text {
                value: "€".into(),
                truncated: true,
            }
        );
    }

    #[test]
    fn a_panicking_debug_impl_is_absorbed_as_opaque() {
        struct Panicky;

        impl Debug for Panicky {
            fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("deliberate");
            }
        }

        assert_eq!(
            Inspection::debug(&Panicky),
            Inspection::Opaque("Debug implementation panicked")
        );
    }

    #[test]
    fn a_panic_payload_with_a_panicking_drop_is_still_absorbed() {
        /// A payload whose own `Drop` panics — dropping it after the catch
        /// would re-raise and escape the boundary without the second guard.
        struct DropBomb;

        impl Drop for DropBomb {
            fn drop(&mut self) {
                panic!("deliberate panic from the payload's Drop");
            }
        }

        /// Smuggles the bomb across the unwind boundary via `panic_any`.
        struct Smuggler;

        impl Debug for Smuggler {
            fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
                std::panic::panic_any(DropBomb);
            }
        }

        assert_eq!(
            Inspection::debug(&Smuggler),
            Inspection::Opaque("Debug implementation panicked")
        );
    }

    #[test]
    fn a_zero_budget_retains_nothing_but_reports_truncation() {
        assert_eq!(
            Inspection::debug_capped("anything", 0),
            Inspection::Text {
                value: String::new(),
                truncated: true,
            }
        );
    }

    #[test]
    fn a_debug_that_ignores_refusals_still_cannot_exceed_the_cap() {
        /// Swallows every write refusal and keeps pushing — the documented
        /// `buf.len() <= max` invariant must hold anyway.
        struct Bulldozer;

        impl Debug for Bulldozer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for _ in 0..1_000 {
                    let _ = f.write_str("0123456789abcdef");
                }
                Ok(())
            }
        }

        let Inspection::Text { value, truncated } = Inspection::debug(&Bulldozer) else {
            panic!("must render as text");
        };
        assert!(truncated);
        assert!(
            value.len() <= DEFAULT_MAX_BYTES,
            "cap exceeded: {} bytes retained",
            value.len()
        );
    }

    #[test]
    fn nothing_is_accepted_after_truncation_even_if_it_fits() {
        /// Swallows the refusal of an oversized fragment, then offers one that
        /// would fit the budget the `char`-boundary back-off left unused — a
        /// fused writer must refuse it rather than splice it after the cut.
        struct GapFiller;

        impl Debug for GapFiller {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // 10 bytes against a cap of 8: the trailing `€` straddles the
                // cap, so the back-off retains 7 bytes and leaves 1 spare.
                let _ = f.write_str("0123456€");
                let _ = f.write_str("x");
                Ok(())
            }
        }

        assert_eq!(
            Inspection::debug_capped(&GapFiller, 8),
            Inspection::Text {
                value: "0123456".into(),
                truncated: true,
            }
        );
    }

    #[test]
    fn a_debug_impl_that_errors_without_cause_is_opaque_not_partial_text() {
        /// Returns `fmt::Error` although the writer refused nothing — a
        /// contract violation, so its partial output cannot be trusted.
        struct Lying;

        impl Debug for Lying {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("partial")?;
                Err(fmt::Error)
            }
        }

        assert_eq!(
            Inspection::debug(&Lying),
            Inspection::Opaque("Debug implementation errored")
        );
    }

    #[test]
    fn inspect_value_renders_bare_values() {
        assert_eq!(
            inspect_value(&42_u8),
            Inspection::Text {
                value: "42".into(),
                truncated: false,
            }
        );
    }
}
