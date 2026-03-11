use polaris_system::prelude::SystemError;
use polaris_system::system;
use polaris_system::system::System;

#[derive(Debug)]
struct MyOutput {
    value: i32,
}

/// A fallible system with no parameters should also work.
#[system]
async fn fallible_no_params() -> Result<MyOutput, SystemError> {
    Ok(MyOutput { value: 42 })
}

fn main() {
    use std::any::TypeId;
    use polaris_system::system::ErasedSystem;

    let system = fallible_no_params();
    assert_eq!(system.output_type_id(), TypeId::of::<MyOutput>());
}
