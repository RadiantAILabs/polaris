//! A tuple parameter is a legal `SystemParam` but has no `InspectParam` impl.
//! Selecting it must fail at that parameter — a legal-but-uninspectable
//! parameter kind is always rejected, never silently unrecorded.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

#[derive(Debug)]
struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[derive(Debug)]
struct Gauge {
    level: i32,
}

impl LocalResource for Gauge {}

#[system(inspect(pair))]
async fn tuple_param(pair: (Res<'_, Counter>, Res<'_, Gauge>)) {
    let _ = &pair;
}

fn main() {}
