//! `Option<ResMut<T>>` is not a system parameter.

use polaris_system::param::ResMut;
use polaris_system::resource::LocalResource;
use polaris_system::system;

struct Memory;
impl LocalResource for Memory {}

#[system]
async fn writes_optional_local(memory: Option<ResMut<Memory>>) {
    let _ = memory;
}

fn main() {}
