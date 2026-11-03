//! `Option<Res<T>>` is only for global resources: optional reads of local
//! resources cannot be checked before a run, so they must not compile.

use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::system;

struct Memory;
impl LocalResource for Memory {}

#[system]
async fn reads_optional_local(memory: Option<Res<Memory>>) {
    let _ = memory;
}

fn main() {}
