//! `replace_inspection` is `#[must_use]`: the sink it displaces belongs to
//! whoever installed it, so dropping that handle silently cuts them off.
//!
//! This fixture pins the attribute. Without it, displacing a sink is
//! expressible in statement position again and the "silent becomes loud"
//! guarantee is gone with nothing failing. A deliberate discard binds to `_`.

#![deny(unused_must_use)]

use polaris_system::param::SystemContext;
use polaris_system::param::inspect::{Inspection, InspectionSink, ParamMeta};
use std::sync::Arc;

struct Discard;

impl InspectionSink for Discard {
    fn record(&self, _meta: ParamMeta, _render: &dyn Fn() -> Inspection) {}
}

fn main() {
    let mut ctx = SystemContext::new();
    ctx.replace_inspection(Arc::new(Discard));
}
