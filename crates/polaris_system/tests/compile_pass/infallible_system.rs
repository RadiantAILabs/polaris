use polaris_system::param::Res;
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

#[system]
async fn read_counter(counter: Res<Counter>) -> CounterOutput {
    CounterOutput {
        value: counter.count,
    }
}

fn main() {
    // Verify the macro generates a callable factory function.
    let _system = read_counter();
}
