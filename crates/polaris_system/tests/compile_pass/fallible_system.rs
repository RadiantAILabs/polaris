use polaris_system::param::{Res, SystemContext};
use polaris_system::prelude::SystemError;
use polaris_system::resource::LocalResource;
use polaris_system::system;
use polaris_system::system::System;

struct Counter {
    count: i32,
}

impl LocalResource for Counter {}

#[derive(Debug)]
struct CounterOutput {
    value: i32,
}

/// A system returning `Result<T, SystemError>` should compile with `T` as the output type.
#[system]
async fn fallible_read(counter: Res<Counter>) -> Result<CounterOutput, SystemError> {
    if counter.count < 0 {
        return Err(SystemError::ExecutionError(
            "negative count".to_string(),
        ));
    }
    Ok(CounterOutput {
        value: counter.count,
    })
}

fn main() {
    use std::any::TypeId;
    use polaris_system::system::ErasedSystem;

    let system = fallible_read();

    // The output type should be `CounterOutput`, NOT `Result<CounterOutput, SystemError>`.
    assert_eq!(system.output_type_id(), TypeId::of::<CounterOutput>());
    assert_ne!(
        system.output_type_id(),
        TypeId::of::<Result<CounterOutput, SystemError>>()
    );
}
