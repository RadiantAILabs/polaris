//! System execution primitives.
//!
//! A [`System`] is an async function that receives dependencies via parameter
//! injection from a [`SystemContext`] and produces
//! a typed output. All dependencies are declared as parameters, making data
//! flow explicit and systems composable across different execution contexts.
//!
//! See [`System`] for the full trait definition and examples.
//!
//! Use the [`#[system]`](macro@crate::system) attribute macro to define systems from
//! regular async functions:
//!
//! ```
//! use polaris_system::param::Res;
//! use polaris_system::resource::GlobalResource;
//! use polaris_system::system;
//!
//! struct Memory { context: String }
//! impl GlobalResource for Memory {}
//!
//! struct ReasoningResult { response: String }
//!
//! #[system]
//! async fn reason(memory: Res<Memory>) -> ReasoningResult {
//!     ReasoningResult { response: memory.context.clone() }
//! }
//! ```

use crate::param::{ParamError, SystemAccess, SystemContext};
use crate::resource::Output;
use std::any::{Any, TypeId, type_name};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

/// Errors that can occur during system execution.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    /// Failed to fetch a system parameter.
    #[error("parameter error: {0}")]
    ParamError(#[from] ParamError),

    /// The system encountered an error during execution.
    #[error("execution error: {0}")]
    ExecutionError(String),
}

/// A boxed future that is Send.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An async unit of computation that receives dependencies via
/// [`SystemParam`](crate::param::SystemParam) injection and produces a typed
/// [`Output`].
///
/// Most users define systems with the [`#[system]`](macro@crate::system) macro
/// rather than implementing this trait directly:
///
/// ```
/// use polaris_system::system::{IntoSystem, System};
/// use polaris_system::param::Res;
/// use polaris_system::resource::{GlobalResource, Output};
/// use polaris_system::system;
///
/// struct Config;
/// impl GlobalResource for Config {}
///
/// struct MyOutput;
/// impl MyOutput { fn new(_: &Config) -> Self { Self } }
///
/// #[system]
/// async fn my_system(config: Res<Config>) -> MyOutput {
///     MyOutput::new(&config)
/// }
///
/// // Automatically implements System via IntoSystem
/// let system = Box::new(my_system.into_system());
/// ```
///
/// ## Fallible Systems
///
/// Systems may return `Result<T, SystemError>` to indicate they can fail.
/// The `#[system]` macro detects this pattern and sets [`is_fallible`](System::is_fallible)
/// to `true` automatically. On success, `T` is stored in the context for
/// downstream `Out<T>` access. On error, the `SystemError` propagates to the
/// executor.
pub trait System: Send + Sync + 'static {
    /// The output type produced by this system.
    type Output: Output;

    /// Executes the system with the given context.
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] if parameter fetching fails or execution errors.
    fn run<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>>;

    /// Returns the system's name for debugging and tracing.
    fn name(&self) -> &'static str;

    /// Returns the access patterns for this system's parameters.
    ///
    /// Used by schedulers to detect conflicts between systems and enable
    /// safe parallel execution. The default implementation returns empty
    /// access (no conflicts).
    fn access(&self) -> SystemAccess {
        SystemAccess::default()
    }

    /// Returns whether this system can fail with a user-defined error.
    ///
    /// Systems that return `Result<T, SystemError>` are considered fallible.
    /// The `#[system]` macro emits `true` for such systems automatically.
    /// The default is `false` (infallible).
    fn is_fallible(&self) -> bool {
        false
    }
}

/// Object-safe wrapper for type-erased system execution.
///
/// This trait enables storing heterogeneous systems (with different output types)
/// in collections while preserving type information for runtime validation.
///
/// # Type Erasure Pattern
///
/// The [`System`] trait has an associated `Output` type, making it not object-safe.
/// `ErasedSystem` erases the output type while preserving:
/// - `TypeId` for runtime type validation
/// - Type name for debugging and error messages
///
/// All types implementing [`System`] automatically implement `ErasedSystem`
/// via a blanket implementation.
pub trait ErasedSystem: Send + Sync + 'static {
    /// Returns the system's name for debugging and tracing.
    fn name(&self) -> &'static str;

    /// Returns the access patterns for this system's parameters.
    fn access(&self) -> SystemAccess;

    /// Returns the [`TypeId`] of this system's output type.
    fn output_type_id(&self) -> TypeId;

    /// Returns the output type name for error messages.
    fn output_type_name(&self) -> &'static str;

    /// Executes the system, returning type-erased output.
    ///
    /// The returned `Box<dyn Any + Send + Sync>` contains the system's typed output,
    /// which can be downcast back to the concrete type using [`TypeId`].
    ///
    /// # Errors
    ///
    /// Returns [`SystemError`] if parameter fetching fails or execution errors.
    fn run_erased<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Box<dyn Any + Send + Sync>, SystemError>>;

    /// Returns whether this system can fail with a user-defined error.
    fn is_fallible(&self) -> bool;
}

/// Boxed type-erased system.
///
/// Use this type alias for storing heterogeneous systems in collections.
pub type BoxedSystem = Box<dyn ErasedSystem>;

/// A type-erased system whose output type remains visible at compile time.
///
/// `TypedSystem<O>` lets composition helpers name the output they require
/// without exposing the concrete system type or the marker used by
/// [`IntoSystem`]. Convert a system at the call site with [`erase`], then pass
/// the handle to an API that accepts boxed systems, such as
/// `Graph::system_boxed` in `polaris_graph`.
///
/// `O` is the system's [`Output`](System::Output), which for a fallible system
/// is the success type — a `Result<T, SystemError>` return erases to
/// `TypedSystem<T>`, matching how `Out<T>` reads a fallible system's value.
/// Fallibility travels alongside `O` and is readable via
/// [`is_fallible`](Self::is_fallible).
///
/// The handle is inspectable without being consumed:
/// [`name`](Self::name), [`output_type_name`](Self::output_type_name),
/// [`access`](Self::access), and [`is_fallible`](Self::is_fallible) mirror the
/// [`System`] methods of the same name, so a helper can log or branch on a
/// caller-supplied stage while still holding its compile-time `O`.
///
/// # Example
///
/// ```
/// use polaris_system::system::{TypedSystem, erase};
///
/// async fn produce() -> i32 { 42 }
///
/// fn accepts_i32(_system: TypedSystem<i32>) {}
///
/// accepts_i32(erase(produce));
/// ```
///
/// A mismatched output is rejected at compile time:
///
/// ```compile_fail
/// use polaris_system::system::{TypedSystem, erase};
///
/// async fn produce() -> String { String::new() }
///
/// fn accepts_i32(_system: TypedSystem<i32>) {}
///
/// accepts_i32(erase(produce));
/// ```
#[must_use = "a typed system must be added to a graph or converted into a boxed system"]
pub struct TypedSystem<O: Output> {
    inner: BoxedSystem,
    _output: PhantomData<O>,
}

impl<O: Output> TypedSystem<O> {
    /// Returns the erased system's name, for logging and tracing.
    ///
    /// Mirrors [`System::name`]. Lets a composition helper report which stage
    /// it was handed without consuming the handle — and so without giving up
    /// the compile-time `O` the handle exists to carry.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.inner.name()
    }

    /// Returns the name of the erased system's output type, for diagnostics.
    ///
    /// Mirrors [`ErasedSystem::output_type_name`]. This is the runtime spelling
    /// of `O`, useful in a helper's own error messages.
    #[must_use]
    pub fn output_type_name(&self) -> &'static str {
        self.inner.output_type_name()
    }

    /// Returns the erased system's declared resource and output access.
    ///
    /// Mirrors [`System::access`]. A helper can inspect what a caller-supplied
    /// stage reads and writes before deciding where to place it.
    #[must_use]
    pub fn access(&self) -> SystemAccess {
        self.inner.access()
    }

    /// Returns whether the erased system can fail with a user-defined error.
    ///
    /// Mirrors [`System::is_fallible`]. Fallibility is orthogonal to `O`: the
    /// [`#[system]`](macro@crate::system) macro extracts `T` from a
    /// `Result<T, SystemError>` return type as the system's output, so both
    /// `async fn() -> i32` and `async fn() -> Result<i32, SystemError>` erase
    /// to `TypedSystem<i32>` and are told apart by this flag. A composition
    /// helper that wires error handling reads it here rather than inferring it
    /// from the output type.
    ///
    /// # Example
    ///
    /// ```
    /// use polaris_system::system;
    /// use polaris_system::system::{SystemError, erase};
    ///
    /// #[system]
    /// async fn fallible_stage() -> Result<i32, SystemError> { Ok(7) }
    ///
    /// async fn infallible_stage() -> i32 { 7 }
    ///
    /// assert!(erase(fallible_stage).is_fallible());
    /// assert!(!erase(infallible_stage).is_fallible());
    /// ```
    #[must_use]
    pub fn is_fallible(&self) -> bool {
        self.inner.is_fallible()
    }
}

impl<O: Output> fmt::Debug for TypedSystem<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TypedSystem")
            .field("name", &self.name())
            .field("output_type", &self.output_type_name())
            .field("fallible", &self.is_fallible())
            .finish()
    }
}

/// Erases a system's concrete and marker types while retaining its output type.
///
/// This is useful at composition boundaries: callers erase their concrete
/// systems once, while the receiving helper can declare a compact
/// [`TypedSystem<O>`] parameter and still reject a system with the wrong
/// output type at compile time.
///
/// Accepts anything [`IntoSystem`] does — a bare async function, or a factory
/// function returning a [`System`], which is what the
/// [`#[system]`](macro@crate::system) macro generates.
///
/// # Example
///
/// ```
/// use polaris_system::system::{TypedSystem, erase};
///
/// // The helper names the output it needs, not the system type or marker.
/// fn add_stage(stage: TypedSystem<i32>) -> &'static str {
///     stage.name()
/// }
///
/// async fn predict() -> i32 { 7 }
///
/// assert!(add_stage(erase(predict)).contains("predict"));
/// ```
///
/// See [`TypedSystem`] for what the handle preserves and for the compile-time
/// rejection of a mismatched output.
pub fn erase<S, M>(system: S) -> TypedSystem<<S::System as System>::Output>
where
    S: IntoSystem<M>,
    S::System: 'static,
{
    TypedSystem {
        inner: Box::new(system.into_system()),
        _output: PhantomData,
    }
}

/// Converts a boxed or typed-erased system into a [`BoxedSystem`].
///
/// Graph construction APIs take `impl IntoBoxedSystem` rather than a concrete
/// [`BoxedSystem`] so existing call sites keep working while typed-erased
/// handles are accepted too. Three forms convert:
///
/// - `Box<S>` where `S` implements [`ErasedSystem`] — a boxed concrete system,
///   which by the blanket impl covers every `S: System`.
/// - [`BoxedSystem`] — an already-erased `Box<dyn ErasedSystem>`.
/// - [`TypedSystem<O>`] — a handle produced by [`erase`].
///
/// `Box<dyn System>` is deliberately *not* among them: [`System`] has an
/// associated `Output` type and so is not object safe, which is the reason
/// [`ErasedSystem`] and [`BoxedSystem`] exist.
pub trait IntoBoxedSystem {
    /// Converts this value into a type-erased boxed system.
    fn into_boxed_system(self) -> BoxedSystem;
}

impl<S: ErasedSystem> IntoBoxedSystem for Box<S> {
    fn into_boxed_system(self) -> BoxedSystem {
        self
    }
}

impl IntoBoxedSystem for BoxedSystem {
    fn into_boxed_system(self) -> BoxedSystem {
        self
    }
}

impl<O: Output> From<TypedSystem<O>> for BoxedSystem {
    fn from(system: TypedSystem<O>) -> Self {
        system.inner
    }
}

impl<O: Output> IntoBoxedSystem for TypedSystem<O> {
    fn into_boxed_system(self) -> BoxedSystem {
        self.into()
    }
}

impl<S: System> ErasedSystem for S {
    fn name(&self) -> &'static str {
        System::name(self)
    }

    fn access(&self) -> SystemAccess {
        System::access(self)
    }

    fn output_type_id(&self) -> TypeId {
        TypeId::of::<S::Output>()
    }

    fn output_type_name(&self) -> &'static str {
        type_name::<S::Output>()
    }

    fn run_erased<'a>(
        &'a self,
        ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Box<dyn Any + Send + Sync>, SystemError>> {
        Box::pin(async move {
            let output = self.run(ctx).await?;
            Ok(Box::new(output) as Box<dyn Any + Send + Sync>)
        })
    }

    fn is_fallible(&self) -> bool {
        <Self as System>::is_fallible(self)
    }
}

/// Converts a type into a [`System`].
///
/// Implemented for async functions (via [`FunctionSystem`]) and for factory
/// functions produced by the [`#[system]`](macro@crate::system) macro (via
/// [`SystemFnMarker`]). See [`System`] for a usage example.
pub trait IntoSystem<Marker>: Sized {
    /// The resulting system type.
    type System: System;

    /// Converts this into a system.
    fn into_system(self) -> Self::System;
}

/// Marker for the `#[system]` macro. Allows any function returning a [`System`]
/// (such as macro-generated factory functions) to implement [`IntoSystem`].
///
/// Marker required because Rust's coherence checker cannot prove that
/// `Fn() -> impl Future` and `FnOnce() -> impl System` are disjoint.
pub struct SystemFnMarker;

impl<F, S> IntoSystem<SystemFnMarker> for F
where
    F: FnOnce() -> S,
    S: System,
{
    type System = S;

    fn into_system(self) -> Self::System {
        self()
    }
}

/// A system wrapping an async function.
///
/// Created via [`IntoSystem`] for async functions.
pub struct FunctionSystem<F, Marker> {
    func: F,
    name: &'static str,
    _marker: PhantomData<fn() -> Marker>,
}

impl<F, Marker> FunctionSystem<F, Marker> {
    /// Creates a new function system with the given name.
    pub fn new(func: F, name: &'static str) -> Self {
        Self {
            func,
            name,
            _marker: PhantomData,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IntoSystem implementations for async functions with 0-8 parameters
// ─────────────────────────────────────────────────────────────────────────────

/// Marker type for function systems.
pub struct FunctionMarker;

// 0 parameters
impl<F, Fut, O> IntoSystem<(FunctionMarker,)> for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = O> + Send + 'static,
    O: Output,
{
    type System = FunctionSystem<F, (FunctionMarker,)>;

    fn into_system(self) -> Self::System {
        let full = type_name::<F>();
        let name = full.rsplit("::").next().unwrap_or(full);
        FunctionSystem::new(self, name)
    }
}

impl<F, Fut, O> System for FunctionSystem<F, (FunctionMarker,)>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = O> + Send + 'static,
    O: Output,
{
    type Output = O;

    fn run<'a>(
        &'a self,
        _ctx: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move { Ok((self.func)().await) })
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::SystemParam;
    use crate::resource::LocalResource;

    #[derive(Debug, PartialEq, Clone)]
    struct TestOutput {
        value: i32,
    }

    #[derive(Debug, PartialEq)]
    struct Counter {
        count: i32,
    }

    // Counter is a LocalResource - can be mutated via ResMut<Counter>
    impl LocalResource for Counter {}

    #[derive(Debug, PartialEq)]
    struct Config {
        multiplier: i32,
    }

    // Config is also LocalResource for these tests
    impl LocalResource for Config {}

    // ─────────────────────────────────────────────────────────────────────
    // Test: Zero parameter system
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn zero_param_system() {
        async fn produce() -> TestOutput {
            TestOutput { value: 42 }
        }

        let system = produce.into_system();
        let ctx = SystemContext::new();

        let result = system.run(&ctx).await.unwrap();
        assert_eq!(result.value, 42);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: Single parameter system (manual System impl)
    // This pattern is what the #[system] macro generates
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_param_system() {
        use crate::param::Res;

        // Manual System impl - the #[system] macro generates this pattern
        struct ReadCounterSystem;

        impl System for ReadCounterSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let counter = Res::<Counter>::fetch(ctx)?;
                    Ok(TestOutput {
                        value: counter.count,
                    })
                })
            }

            fn name(&self) -> &'static str {
                "read_counter"
            }
        }

        let system = ReadCounterSystem;
        let ctx = SystemContext::new().with(Counter { count: 10 });

        let result = system.run(&ctx).await.unwrap();
        assert_eq!(result.value, 10);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: Multiple parameter system
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn multi_param_system() {
        use crate::param::Res;

        struct ComputeSystem;

        impl System for ComputeSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let counter = Res::<Counter>::fetch(ctx)?;
                    let config = Res::<Config>::fetch(ctx)?;
                    Ok(TestOutput {
                        value: counter.count * config.multiplier,
                    })
                })
            }

            fn name(&self) -> &'static str {
                "compute"
            }
        }

        let system = ComputeSystem;
        let ctx = SystemContext::new()
            .with(Counter { count: 5 })
            .with(Config { multiplier: 3 });

        let result = system.run(&ctx).await.unwrap();
        assert_eq!(result.value, 15);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: System with mutable resource
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn mutable_resource_system() {
        use crate::param::ResMut;

        struct IncrementSystem;

        impl System for IncrementSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let mut counter = ResMut::<Counter>::fetch(ctx)?;
                    counter.count += 1;
                    Ok(TestOutput {
                        value: counter.count,
                    })
                })
            }

            fn name(&self) -> &'static str {
                "increment"
            }
        }

        let system = IncrementSystem;
        let ctx = SystemContext::new().with(Counter { count: 0 });

        let result = system.run(&ctx).await.unwrap();
        assert_eq!(result.value, 1);

        // Run again - should see incremented value
        let result2 = system.run(&ctx).await.unwrap();
        assert_eq!(result2.value, 2);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: System reading previous output
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn output_chain_system() {
        use crate::param::Out;

        struct DoubleOutputSystem;

        impl System for DoubleOutputSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let prev = Out::<TestOutput>::fetch(ctx)?;
                    Ok(TestOutput {
                        value: prev.value * 2,
                    })
                })
            }

            fn name(&self) -> &'static str {
                "double_output"
            }
        }

        let system = DoubleOutputSystem;
        let mut ctx = SystemContext::new();
        ctx.insert_output(TestOutput { value: 21 });

        let result = system.run(&ctx).await.unwrap();
        assert_eq!(result.value, 42);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: Missing resource error
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_resource_error() {
        use crate::param::Res;

        struct ReadMissingSystem;

        impl System for ReadMissingSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let counter = Res::<Counter>::fetch(ctx)?;
                    Ok(TestOutput {
                        value: counter.count,
                    })
                })
            }

            fn name(&self) -> &'static str {
                "read_missing"
            }
        }

        let system = ReadMissingSystem;
        let ctx = SystemContext::new(); // No Counter inserted

        let result = system.run(&ctx).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SystemError::ParamError(_)));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: System name
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn system_has_name() {
        async fn named_system() -> TestOutput {
            TestOutput { value: 0 }
        }

        let system = named_system.into_system();
        assert!(System::name(&system).contains("named_system"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Test: ErasedSystem trait
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn erased_system_output_type_id() {
        async fn typed_system() -> TestOutput {
            TestOutput { value: 42 }
        }

        let system = typed_system.into_system();
        let erased: &dyn ErasedSystem = &system;

        assert_eq!(erased.output_type_id(), TypeId::of::<TestOutput>());
    }

    #[test]
    fn erased_system_output_type_name() {
        async fn typed_system() -> TestOutput {
            TestOutput { value: 42 }
        }

        let system = typed_system.into_system();
        let erased: &dyn ErasedSystem = &system;

        assert!(erased.output_type_name().contains("TestOutput"));
    }

    #[test]
    fn erased_system_name_delegates_to_system() {
        async fn my_named_system() -> TestOutput {
            TestOutput { value: 0 }
        }

        let system = my_named_system.into_system();
        let erased: &dyn ErasedSystem = &system;

        assert!(erased.name().contains("my_named_system"));
    }

    #[tokio::test]
    async fn erased_system_run_returns_boxed_output() {
        async fn produce_value() -> TestOutput {
            TestOutput { value: 99 }
        }

        let system = produce_value.into_system();
        let erased: &dyn ErasedSystem = &system;
        let ctx = SystemContext::new();

        let boxed_result = erased.run_erased(&ctx).await.unwrap();

        // Downcast back to concrete type
        let concrete = boxed_result.downcast::<TestOutput>().unwrap();
        assert_eq!(concrete.value, 99);
    }

    #[tokio::test]
    async fn erased_system_run_propagates_errors() {
        use crate::param::Res;

        struct FailingSystem;

        impl System for FailingSystem {
            type Output = TestOutput;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    // Try to fetch a resource that doesn't exist
                    let _counter = Res::<Counter>::fetch(ctx)?;
                    Ok(TestOutput { value: 0 })
                })
            }

            fn name(&self) -> &'static str {
                "failing_system"
            }
        }

        let system = FailingSystem;
        let erased: &dyn ErasedSystem = &system;
        let ctx = SystemContext::new(); // No Counter

        let result = erased.run_erased(&ctx).await;
        assert!(result.is_err());
    }

    #[test]
    fn boxed_system_can_store_heterogeneous_systems() {
        async fn int_system() -> i32 {
            42
        }

        async fn string_system() -> String {
            "hello".to_string()
        }

        let sys1 = int_system.into_system();
        let sys2 = string_system.into_system();

        // Store as BoxedSystem (type-erased)
        let boxed1: BoxedSystem = Box::new(sys1);
        let boxed2: BoxedSystem = Box::new(sys2);

        // Can store in same collection
        let systems: Vec<BoxedSystem> = vec![boxed1, boxed2];

        // Type info preserved
        assert_eq!(systems[0].output_type_id(), TypeId::of::<i32>());
        assert_eq!(systems[1].output_type_id(), TypeId::of::<String>());
    }

    #[test]
    fn typed_system_erasure_preserves_runtime_metadata() {
        async fn int_system() -> i32 {
            42
        }

        let typed: TypedSystem<i32> = erase(int_system);
        let boxed: BoxedSystem = typed.into();

        assert_eq!(boxed.output_type_id(), TypeId::of::<i32>());
        assert_eq!(boxed.output_type_name(), type_name::<i32>());
        assert!(boxed.name().contains("int_system"));
        assert!(
            !boxed.is_fallible(),
            "an infallible system stays infallible through erasure"
        );
    }

    /// A manual `System` impl that declares fallibility, as a provider outside
    /// the `#[system]` macro would.
    struct ManualFallibleSystem;

    impl System for ManualFallibleSystem {
        type Output = i32;

        fn run<'a>(
            &'a self,
            _ctx: &'a SystemContext<'_>,
        ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
            Box::pin(async move { Err(SystemError::ExecutionError("boom".into())) })
        }

        fn name(&self) -> &'static str {
            "manual_fallible_system"
        }

        fn is_fallible(&self) -> bool {
            true
        }
    }

    #[test]
    fn typed_system_reports_fallibility_of_the_erased_system() {
        // The `#[system]` macro extracts `T` from `Result<T, SystemError>` as
        // the output type, so a fallible and an infallible system with the same
        // success type erase to the *same* `TypedSystem<O>`. Fallibility is
        // carried alongside `O`, not inside it.
        async fn infallible() -> i32 {
            42
        }

        let infallible: TypedSystem<i32> = erase(infallible);
        let fallible: TypedSystem<i32> = erase(|| ManualFallibleSystem);

        assert!(!infallible.is_fallible());
        assert!(fallible.is_fallible());

        // The flag survives conversion into a `BoxedSystem`, which is what
        // graph construction consumes.
        assert!(!BoxedSystem::from(infallible).is_fallible());
        assert!(BoxedSystem::from(fallible).is_fallible());
    }

    #[test]
    fn typed_system_accessors_mirror_the_erased_system() {
        use crate::param::Res;

        /// Declares real resource access, so `access()` parity is non-vacuous.
        struct ReadCounterSystem;

        impl System for ReadCounterSystem {
            type Output = i32;

            fn run<'a>(
                &'a self,
                ctx: &'a SystemContext<'_>,
            ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
                Box::pin(async move {
                    let counter = Res::<Counter>::fetch(ctx)?;
                    Ok(counter.count)
                })
            }

            fn name(&self) -> &'static str {
                "read_counter"
            }

            fn access(&self) -> SystemAccess {
                <Res<Counter> as SystemParam>::access()
            }
        }

        let direct = ReadCounterSystem;
        let typed: TypedSystem<i32> = erase(|| ReadCounterSystem);

        // Every accessor reports what the concrete system reports, and the
        // handle is still usable afterwards — nothing was consumed.
        assert_eq!(typed.name(), System::name(&direct));
        assert_eq!(typed.output_type_name(), type_name::<i32>());
        assert_eq!(typed.access(), System::access(&direct));
        assert_eq!(typed.is_fallible(), System::is_fallible(&direct));

        // Non-vacuous: `Res<Counter>` declares a read, so `access()` is
        // comparing real declarations rather than two empty sets.
        assert!(
            !typed.access().resources.is_empty(),
            "the fixture must declare resource access for this to be meaningful"
        );

        let boxed: BoxedSystem = typed.into();
        assert_eq!(boxed.name(), System::name(&direct));
    }

    #[test]
    fn typed_system_erasure_preserves_fallible_metadata() {
        let direct: BoxedSystem = Box::new(ManualFallibleSystem);
        let erased: BoxedSystem = erase(|| ManualFallibleSystem).into();

        assert_eq!(direct.is_fallible(), erased.is_fallible());
        assert_eq!(direct.name(), erased.name());
        assert_eq!(direct.output_type_id(), erased.output_type_id());
        assert_eq!(direct.output_type_name(), erased.output_type_name());
        assert_eq!(direct.access(), erased.access());
    }

    #[test]
    fn typed_system_debug_reports_name_output_and_fallibility() {
        async fn int_system() -> i32 {
            42
        }

        let typed: TypedSystem<i32> = erase(int_system);
        let rendered = format!("{typed:?}");

        assert!(
            rendered.starts_with("TypedSystem {"),
            "Debug should name the type, got: {rendered}"
        );
        assert!(
            rendered.contains("int_system"),
            "Debug should report the erased system's name, got: {rendered}"
        );
        assert!(
            rendered.contains(type_name::<i32>()),
            "Debug should report the output type, got: {rendered}"
        );
        assert!(
            rendered.contains("fallible: false"),
            "Debug should report fallibility, got: {rendered}"
        );
    }
}
