//! A parameter named `ctx` collides with the context binding the generated
//! system body uses. Rejecting it at expansion keeps the collision from ever
//! becoming a silent capture of the wrong value, and replaces the confusing
//! type error the shadowing used to produce.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[system]
async fn shadows_context(ctx: Res<Counter>) {
    let _ = &ctx;
}

fn main() {}
