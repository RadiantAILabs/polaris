# system_macros

Procedural macros for the `polaris_system` crate.

## Overview

This crate provides the `#[system]` attribute macro that transforms async functions into `System` implementations, solving Rust's lifetime limitations with Higher-Ranked Trait Bounds (HRTB) and async functions.

## The Problem

When defining systems with lifetime-parameterized parameters like `Res<'_, T>`, Rust's type system cannot express the relationship between input lifetimes and async return types:

```rust
// Fails with E0582: "lifetime 'w in return type doesn't appear in input types"
for<'w> F: Fn(Res<'w, T>) -> BoxFuture<'w, O>
```

## The Solution

The `#[system]` macro generates a struct that implements `System` directly, bypassing the HRTB limitation:

```rust
use polaris_system_macros::system;

#[system]
async fn read_counter(counter: Res<Counter>) -> Output {
    Output { value: counter.count }
}
```

This generates:

```rust
struct ReadCounterSystem;

impl System for ReadCounterSystem {
    type Output = Output;

    fn run<'a>(&'a self, ctx: &'a SystemContext<'_>)
        -> BoxFuture<'a, Result<Self::Output, SystemError>>
    {
        Box::pin(async move {
            let counter = Res::<Counter>::fetch(ctx)?;
            Ok({ Output { value: counter.count } })
        })
    }

    fn name(&self) -> &'static str {
        "read_counter"
    }
}

fn read_counter() -> ReadCounterSystem {
    ReadCounterSystem
}
```

## Fallible Systems

Systems that return `Result<T, SystemError>` are considered fallible. The macro detects this return type and:

1. Extracts `T` as the system's `Output` type (not `Result<T, SystemError>`)
2. Sets `is_fallible()` to return `true` (infallible systems return `false`)

```rust
#[system]
async fn reason(llm: Res<LLM>) -> Result<ReasoningResult, SystemError> {
    let response = llm.generate().await
        .map_err(|err| SystemError::ExecutionError(err.to_string()))?;
    Ok(ReasoningResult { action: response.action })
}
```

On success, `T` is stored in the context for downstream `Out<T>` access. On error, the `SystemError` propagates to the executor.

## Generated Code

For a function `foo_bar`, the macro generates:

| Generated Item | Description |
|----------------|-------------|
| `FooBarSystem` | Unit struct (PascalCase from snake_case) |
| `impl System for FooBarSystem` | The `System` trait implementation |
| `fn foo_bar() -> FooBarSystem` | Factory function returning the system |

The generated `System` implementation includes:

- `run()` — fetches each parameter via `SystemParam::fetch()` and executes the function body
- `name()` — returns the original function name as a `&'static str`
- `access()` — merges access declarations from all parameters
- `is_fallible()` — returns `true` if the return type is `Result<T, SystemError>`, `false` otherwise

## Requirements

- Functions must be `async`
- Parameters must be simple identifiers (no patterns)
- Currently designed for use within the `polaris_system` crate (uses `crate::` paths)

## License

Apache-2.0
