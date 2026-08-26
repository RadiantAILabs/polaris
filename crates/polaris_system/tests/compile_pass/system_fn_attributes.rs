//! Attributes on a `#[system]` function are forwarded rather than silently
//! discarded: doc comments land on the generated struct, `cfg` gates every
//! generated item, and lint attributes scope the generated `run` method.
#![deny(unused_variables)]

use polaris_system::system;

/// Documented, so the doc comment must forward to the generated struct.
#[system]
#[expect(
    unused_variables,
    reason = "proves lint attributes reach the generated run method: without \
              forwarding, the file-level deny above turns this into an error"
)]
async fn lint_scoped() {
    let forwarded = 1;
}

// An inner `#![expect(..)]` at the top of the body parses as an inner
// attribute of the function; forwarding must re-emit it in outer position on
// the generated `run` method, where it still scopes the body.
#[system]
async fn inner_lint_scoped() {
    #![expect(
        unused_variables,
        reason = "proves inner-style lint attributes forward to the generated run method"
    )]
    let forwarded = 1;
}

// The gated system must compile to nothing: struct, impl, and factory all
// have to sit behind the cfg, or the same-name items below would collide.
#[system]
#[cfg(any())]
async fn gated() {}

struct GatedSystem;

fn gated() -> GatedSystem {
    GatedSystem
}

// `cfg_attr` must gate like `cfg` once its condition holds: this expands to
// `cfg(any())` on every generated item, so the same-name items below would
// collide if any generated item escaped the forwarding.
#[system]
#[cfg_attr(all(), cfg(any()))]
async fn conditionally_gated() {}

struct ConditionallyGatedSystem;

fn conditionally_gated() -> ConditionallyGatedSystem {
    ConditionallyGatedSystem
}

fn main() {
    let _ = lint_scoped();
    let _ = inner_lint_scoped();
    let _ = gated();
    let _ = conditionally_gated();
}
