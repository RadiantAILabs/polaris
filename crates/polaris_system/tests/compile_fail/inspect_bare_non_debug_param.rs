//! Bare `inspect` selects every parameter, so it imposes the `Debug` bound on
//! all of them: a single non-`Debug` parameter must fail to compile, with the
//! error at that parameter — not at the attribute, and not at the parameters
//! that do satisfy the bound.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Renderable {
    _count: i32,
}

impl LocalResource for Renderable {}

struct NotDebug {
    _value: i32,
}

impl LocalResource for NotDebug {}

#[system(inspect)]
async fn inspect_all(fine: Res<Renderable>, broken: Res<NotDebug>) {
    let _ = (&fine, &broken);
}

fn main() {}
