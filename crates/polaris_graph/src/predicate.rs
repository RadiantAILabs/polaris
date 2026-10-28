//! Type-safe predicates for control flow decisions.
//!
//! Predicates evaluate conditions based on previous system outputs,
//! enabling type-safe control flow in graphs.
//!
//! # Architecture
//!
//! The predicate system follows the same type erasure pattern as systems:
//!
//! - [`Predicate<T, F>`] - Typed predicate that reads `Out<T>` (`()` is implicit)
//! - [`ErasedPredicate`] - Object-safe trait for type-erased storage
//! - [`BoxedPredicate`] - Type alias for boxed predicates
//!
//! # Example
//!
//! ```
//! use polaris_graph::predicate::{Predicate, ErasedPredicate};
//! use polaris_system::param::SystemContext;
//!
//! struct ReasoningResult {
//!     needs_tool: bool,
//! }
//!
//! // Create a typed predicate
//! let predicate = Predicate::<ReasoningResult, _>::new(|result| result.needs_tool);
//!
//! // Evaluate against a context
//! let mut ctx = SystemContext::new();
//! ctx.insert_output(ReasoningResult { needs_tool: true });
//!
//! let result = predicate.evaluate(&ctx).unwrap();
//! assert!(result);
//! ```

use crate::node::NodeName;
use polaris_system::param::SystemContext;
use polaris_system::resource::Output;
use std::any::{Any, TypeId, type_name};
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

/// Errors that can occur during predicate evaluation.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::{Predicate, PredicateError, ErasedPredicate};
/// use polaris_system::param::SystemContext;
///
/// struct MyOutput { value: i32 }
///
/// let pred = Predicate::<MyOutput, _>::new(|output| output.value > 5);
///
/// // Evaluating without the required output produces an error
/// let ctx = SystemContext::new();
/// let result = pred.evaluate(&ctx);
/// assert!(matches!(result, Err(PredicateError::OutputNotFound { .. })));
/// ```
#[derive(Debug, Clone)]
pub enum PredicateError {
    /// The required output type was not found in the context.
    OutputNotFound {
        /// The name of the expected output type.
        type_name: &'static str,
    },
    /// An error occurred while accessing the context.
    ContextError(String),
}

impl fmt::Display for PredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PredicateError::OutputNotFound { type_name } => {
                write!(f, "output not found: {type_name}")
            }
            PredicateError::ContextError(msg) => {
                write!(f, "context error: {msg}")
            }
        }
    }
}

impl std::error::Error for PredicateError {}

/// Object-safe trait for type-erased predicates.
///
/// This trait enables storing heterogeneous predicates in graph nodes
/// while preserving type information for debugging.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::{BoxedPredicate, ErasedPredicate};
/// use polaris_system::param::SystemContext;
///
/// struct Score { value: i32 }
///
/// // Box a typed predicate into the trait object
/// let pred = BoxedPredicate::new::<Score>(|s| s.value > 50);
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output(Score { value: 75 });
/// assert!(pred.evaluate(&ctx).unwrap());
/// ```
pub trait ErasedPredicate: Send + Sync {
    /// Evaluates the predicate against the current context.
    ///
    /// # Errors
    ///
    /// Returns an error if the required output is not found or
    /// if there's a context access error.
    fn evaluate(&self, ctx: &SystemContext<'_>) -> Result<bool, PredicateError>;

    /// Returns the [`TypeId`] of the input type this predicate reads.
    fn input_type_id(&self) -> TypeId;

    /// Returns the name of the input type for error messages.
    fn input_type_name(&self) -> &'static str;
}

impl fmt::Debug for dyn ErasedPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErasedPredicate")
            .field("input_type", &self.input_type_name())
            .finish()
    }
}

/// A type-erased, boxed predicate stored in graph nodes.
///
/// Construct one from a typed closure with [`BoxedPredicate::new`] without
/// naming the concrete [`Predicate<T, F>`] type — the way a runtime graph
/// builder assembles control flow from a registry of closures it only knows
/// by input type at construction time, not at the call site.
///
/// Derefs to `dyn ErasedPredicate`, so `.evaluate(..)`, `.input_type_id()` and
/// `.input_type_name()` are called directly on it exactly as they were on the
/// former `Box<dyn ErasedPredicate>` type alias.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::BoxedPredicate;
/// use polaris_system::param::SystemContext;
///
/// struct Score { value: i32 }
///
/// let pred = BoxedPredicate::new::<Score>(|s| s.value > 50);
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output(Score { value: 75 });
/// assert!(pred.evaluate(&ctx).unwrap());
/// ```
pub struct BoxedPredicate(Box<dyn ErasedPredicate>);

impl BoxedPredicate {
    /// Builds a boxed predicate from a typed closure reading `&T`.
    #[must_use]
    pub fn new<T>(func: impl Fn(&T) -> bool + Send + Sync + 'static) -> Self
    where
        T: Output,
    {
        Self(Box::new(Predicate::<T, _>::new(func)))
    }

    /// Unwraps into the underlying trait object.
    pub(crate) fn into_inner(self) -> Box<dyn ErasedPredicate> {
        self.0
    }
}

impl Deref for BoxedPredicate {
    type Target = dyn ErasedPredicate;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl fmt::Debug for BoxedPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<T, F> From<Predicate<T, F>> for BoxedPredicate
where
    T: Output,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn from(predicate: Predicate<T, F>) -> Self {
        Self(Box::new(predicate))
    }
}

impl From<Box<dyn ErasedPredicate>> for BoxedPredicate {
    fn from(predicate: Box<dyn ErasedPredicate>) -> Self {
        Self(predicate)
    }
}

/// Object-safe trait for type-erased discriminators.
///
/// Discriminators are similar to predicates but return a string key
/// instead of a boolean, enabling multi-way branching in switch nodes.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::{BoxedDiscriminator, ErasedDiscriminator};
/// use polaris_system::param::SystemContext;
///
/// struct RouterOutput { action: &'static str }
///
/// let disc = BoxedDiscriminator::new::<RouterOutput, _>(|out| out.action);
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output(RouterOutput { action: "tool" });
/// assert_eq!(disc.discriminate(&ctx).unwrap(), "tool");
/// ```
pub trait ErasedDiscriminator: Send + Sync {
    /// Evaluates the discriminator against the current context.
    ///
    /// Returns the case key to match against switch node cases.
    ///
    /// # Errors
    ///
    /// Returns an error if the required output is not found or
    /// if there's a context access error.
    fn discriminate(&self, ctx: &SystemContext<'_>) -> Result<NodeName, PredicateError>;

    /// Returns the [`TypeId`] of the input type this discriminator reads.
    fn input_type_id(&self) -> TypeId;

    /// Returns the name of the input type for error messages.
    fn input_type_name(&self) -> &'static str;
}

impl fmt::Debug for dyn ErasedDiscriminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErasedDiscriminator")
            .field("input_type", &self.input_type_name())
            .finish()
    }
}

/// A type-erased, boxed discriminator stored in switch nodes.
///
/// Construct one from a typed closure with [`BoxedDiscriminator::new`]
/// without naming the concrete [`Discriminator<T, F>`] type — the way a
/// runtime graph builder assembles a switch from a registry of closures it
/// only knows by input type at construction time, not at the call site.
///
/// Derefs to `dyn ErasedDiscriminator`, so `.discriminate(..)`,
/// `.input_type_id()` and `.input_type_name()` are called directly on it
/// exactly as they were on the former `Box<dyn ErasedDiscriminator>` type
/// alias.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::BoxedDiscriminator;
/// use polaris_system::param::SystemContext;
///
/// struct RouterOutput { action: &'static str }
///
/// let disc = BoxedDiscriminator::new::<RouterOutput, _>(|out| out.action);
///
/// let mut ctx = SystemContext::new();
/// ctx.insert_output(RouterOutput { action: "tool" });
/// assert_eq!(disc.discriminate(&ctx).unwrap(), "tool");
/// ```
pub struct BoxedDiscriminator(Box<dyn ErasedDiscriminator>);

impl BoxedDiscriminator {
    /// Builds a boxed discriminator from a typed closure reading `&T` and
    /// returning a case key — a literal, an owned `String`, or any other
    /// `impl Into<NodeName>`.
    #[must_use]
    pub fn new<T, R>(func: impl Fn(&T) -> R + Send + Sync + 'static) -> Self
    where
        T: Output,
        R: Into<NodeName>,
    {
        Self(Box::new(Discriminator::<T, _>::new(func)))
    }

    /// Unwraps into the underlying trait object.
    pub(crate) fn into_inner(self) -> Box<dyn ErasedDiscriminator> {
        self.0
    }
}

impl Deref for BoxedDiscriminator {
    type Target = dyn ErasedDiscriminator;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl fmt::Debug for BoxedDiscriminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<T, F, R> From<Discriminator<T, F>> for BoxedDiscriminator
where
    T: Output,
    F: Fn(&T) -> R + Send + Sync + 'static,
    R: Into<NodeName>,
{
    fn from(discriminator: Discriminator<T, F>) -> Self {
        Self(Box::new(discriminator))
    }
}

impl From<Box<dyn ErasedDiscriminator>> for BoxedDiscriminator {
    fn from(discriminator: Box<dyn ErasedDiscriminator>) -> Self {
        Self(discriminator)
    }
}

/// A typed predicate that evaluates a condition on a previous system output.
///
/// `Predicate` wraps a closure that takes `&T` and returns `bool`,
/// where `T` is an output type from a previous system. `Predicate<(), F>` is
/// the unit exception: because `()` represents no data flow and is never
/// stored as an output, the closure receives an implicit `&()`.
///
/// # Type Parameters
///
/// - `T`: The output type to read from the context
/// - `F`: The predicate closure type
///
/// # Example
///
/// ```
/// use polaris_graph::predicate::Predicate;
///
/// struct Counter { value: i32 }
///
/// let is_done = Predicate::<Counter, _>::new(|c| c.value >= 10);
/// ```
pub struct Predicate<T, F> {
    func: F,
    _marker: PhantomData<fn() -> T>,
}

impl<T, F> Predicate<T, F>
where
    T: Output,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    /// Creates a new predicate from a closure.
    ///
    /// The closure receives an immutable reference to the output
    /// and should return `true` or `false`.
    #[must_use]
    pub fn new(func: F) -> Self {
        Self {
            func,
            _marker: PhantomData,
        }
    }
}

impl<T, F> ErasedPredicate for Predicate<T, F>
where
    T: Output,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn evaluate(&self, ctx: &SystemContext<'_>) -> Result<bool, PredicateError> {
        // `()` is the absence of data flow, so unit predicates are constant
        // control-flow closures rather than reads from the output channel.
        // The safe `Any` downcast succeeds only when `T` is exactly `()`.
        let unit = ();
        if let Some(input) = (&unit as &dyn Any).downcast_ref::<T>() {
            return Ok((self.func)(input));
        }
        let output = ctx
            .get_output::<T>()
            .map_err(|_| PredicateError::OutputNotFound {
                type_name: type_name::<T>(),
            })?;
        Ok((self.func)(&output))
    }

    fn input_type_id(&self) -> TypeId {
        TypeId::of::<T>()
    }

    fn input_type_name(&self) -> &'static str {
        type_name::<T>()
    }
}

impl<T, F> fmt::Debug for Predicate<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Predicate")
            .field("input_type", &type_name::<T>())
            .finish()
    }
}

/// A typed discriminator that returns a case key based on a previous system output.
///
/// `Discriminator` wraps a closure that takes `&T` and returns `&'static str`,
/// where `T` is an output type from a previous system. For
/// `Discriminator<(), F>`, the closure receives an implicit `&()` because unit
/// represents no data flow and is never stored as an output.
///
/// # Type Parameters
///
/// - `T`: The output type to read from the context
/// - `F`: The discriminator closure type
///
/// # Example
///
/// ```
/// use polaris_graph::predicate::Discriminator;
///
/// struct RouterOutput { action: &'static str }
///
/// let router = Discriminator::<RouterOutput, _>::new(|o| o.action);
/// ```
pub struct Discriminator<T, F> {
    func: F,
    _marker: PhantomData<fn() -> T>,
}

impl<T, F, R> Discriminator<T, F>
where
    T: Output,
    F: Fn(&T) -> R + Send + Sync + 'static,
    R: Into<NodeName>,
{
    /// Creates a new discriminator from a closure.
    ///
    /// The closure receives an immutable reference to the output and should
    /// return a case key — a `&'static str` literal, an owned `String`, or
    /// any other `impl Into<NodeName>`.
    #[must_use]
    pub fn new(func: F) -> Self {
        Self {
            func,
            _marker: PhantomData,
        }
    }
}

impl<T, F, R> ErasedDiscriminator for Discriminator<T, F>
where
    T: Output,
    F: Fn(&T) -> R + Send + Sync + 'static,
    R: Into<NodeName>,
{
    fn discriminate(&self, ctx: &SystemContext<'_>) -> Result<NodeName, PredicateError> {
        // Keep unit discriminators consistent with unit predicates: `()` is
        // implicit and never needs to be deposited as a system output.
        let unit = ();
        if let Some(input) = (&unit as &dyn Any).downcast_ref::<T>() {
            return Ok((self.func)(input).into());
        }
        let output = ctx
            .get_output::<T>()
            .map_err(|_| PredicateError::OutputNotFound {
                type_name: type_name::<T>(),
            })?;
        Ok((self.func)(&output).into())
    }

    fn input_type_id(&self) -> TypeId {
        TypeId::of::<T>()
    }

    fn input_type_name(&self) -> &'static str {
        type_name::<T>()
    }
}

impl<T, F> fmt::Debug for Discriminator<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discriminator")
            .field("input_type", &type_name::<T>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct TestOutput {
        value: i32,
        done: bool,
    }

    #[test]
    fn predicate_evaluate_true() {
        let pred = Predicate::<TestOutput, _>::new(|output| output.value > 5);

        let mut ctx = SystemContext::new();
        ctx.insert_output(TestOutput {
            value: 10,
            done: false,
        });

        let result = pred.evaluate(&ctx).unwrap();
        assert!(result);
    }

    #[test]
    fn predicate_evaluate_false() {
        let pred = Predicate::<TestOutput, _>::new(|output| output.value > 5);

        let mut ctx = SystemContext::new();
        ctx.insert_output(TestOutput {
            value: 3,
            done: false,
        });

        let result = pred.evaluate(&ctx).unwrap();
        assert!(!result);
    }

    #[test]
    fn predicate_missing_output() {
        let pred = Predicate::<TestOutput, _>::new(|_| true);
        let ctx = SystemContext::new();

        let result = pred.evaluate(&ctx);
        assert!(matches!(result, Err(PredicateError::OutputNotFound { .. })));
    }

    #[test]
    fn unit_predicate_uses_implicit_unit_input() {
        let pred = Predicate::<(), _>::new(|()| true);
        let ctx = SystemContext::new();

        assert!(
            pred.evaluate(&ctx).expect("unit input is always available"),
            "the predicate closure receives implicit unit without a context output"
        );
    }

    #[test]
    fn boxed_predicate() {
        let pred = BoxedPredicate::new::<TestOutput>(|o| o.done);

        let mut ctx = SystemContext::new();
        ctx.insert_output(TestOutput {
            value: 0,
            done: true,
        });

        assert!(pred.evaluate(&ctx).unwrap());
    }

    /// A hand-written `ErasedPredicate`, not built via the `Predicate<T, F>`
    /// typed wrapper — the shape a caller's own erased implementation takes.
    struct AlwaysDone;

    impl ErasedPredicate for AlwaysDone {
        fn evaluate(&self, _ctx: &SystemContext<'_>) -> Result<bool, PredicateError> {
            Ok(true)
        }

        fn input_type_id(&self) -> TypeId {
            TypeId::of::<TestOutput>()
        }

        fn input_type_name(&self) -> &'static str {
            "TestOutput"
        }
    }

    #[test]
    fn boxed_predicate_from_box_dyn_erased_predicate() {
        let boxed: Box<dyn ErasedPredicate> = Box::new(AlwaysDone);
        let pred: BoxedPredicate = boxed.into();

        let ctx = SystemContext::new();
        assert!(pred.evaluate(&ctx).unwrap());
    }

    /// A hand-written `ErasedDiscriminator`, not built via the
    /// `Discriminator<T, F>` typed wrapper.
    struct FixedKey;

    impl ErasedDiscriminator for FixedKey {
        fn discriminate(&self, _ctx: &SystemContext<'_>) -> Result<NodeName, PredicateError> {
            Ok("fixed".into())
        }

        fn input_type_id(&self) -> TypeId {
            TypeId::of::<TestOutput>()
        }

        fn input_type_name(&self) -> &'static str {
            "TestOutput"
        }
    }

    #[test]
    fn boxed_discriminator_from_box_dyn_erased_discriminator() {
        let boxed: Box<dyn ErasedDiscriminator> = Box::new(FixedKey);
        let disc: BoxedDiscriminator = boxed.into();

        let ctx = SystemContext::new();
        assert_eq!(disc.discriminate(&ctx).unwrap(), "fixed");
    }

    #[tokio::test]
    async fn custom_erased_predicate_boxed_builds_working_loop() {
        use crate::executor::GraphExecutor;
        use crate::graph::Graph;
        use polaris_system::param::Out;

        #[derive(Debug, Clone)]
        struct Counter {
            value: i32,
        }

        #[polaris_system::system]
        async fn init_counter() -> Counter {
            Counter { value: 0 }
        }

        #[polaris_system::system]
        async fn increment(counter: Out<Counter>) -> Counter {
            Counter {
                value: counter.value + 1,
            }
        }

        /// A hand-written `ErasedPredicate`, boxed the way custom
        /// implementations were boxed before `BoxedPredicate` became a
        /// newtype: `Box::new(..) as Box<dyn ErasedPredicate>` converted with
        /// `.into()`.
        struct DoneAtThree;

        impl ErasedPredicate for DoneAtThree {
            fn evaluate(&self, ctx: &SystemContext<'_>) -> Result<bool, PredicateError> {
                let counter =
                    ctx.get_output::<Counter>()
                        .map_err(|_| PredicateError::OutputNotFound {
                            type_name: "Counter",
                        })?;
                Ok(counter.value >= 3)
            }

            fn input_type_id(&self) -> TypeId {
                TypeId::of::<Counter>()
            }

            fn input_type_name(&self) -> &'static str {
                "Counter"
            }
        }

        let termination: BoxedPredicate =
            (Box::new(DoneAtThree) as Box<dyn ErasedPredicate>).into();

        let mut graph = Graph::new();
        graph.add_system(init_counter);
        graph.add_loop_boxed("counter_loop", termination, |g| {
            g.add_system(increment);
        });

        let mut ctx = SystemContext::new();
        let executor = GraphExecutor::new();
        let result = executor
            .execute(&graph, &mut ctx, None, None)
            .await
            .expect("loop should complete");

        // The final `increment` call's return value is taken out of `ctx` and
        // into the result (`ExecutionResult::output`), not left queryable on
        // `ctx` — see `ctx.outputs_mut().take_last()` in `executor/mod.rs`.
        let counter = result
            .output::<Counter>()
            .expect("the loop's last iteration produces the final Counter");
        assert_eq!(
            counter.value, 3,
            "loop should run until the predicate is satisfied"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Discriminator tests
    // ─────────────────────────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct RouterOutput {
        action: &'static str,
    }

    #[test]
    fn discriminator_returns_key() {
        let disc = Discriminator::<RouterOutput, _>::new(|output| output.action);

        let mut ctx = SystemContext::new();
        ctx.insert_output(RouterOutput { action: "tool" });

        let result = disc.discriminate(&ctx).unwrap();
        assert_eq!(result, "tool");
    }

    #[test]
    fn discriminator_different_keys() {
        let disc = Discriminator::<RouterOutput, _>::new(|output| output.action);

        let mut ctx = SystemContext::new();
        ctx.insert_output(RouterOutput { action: "respond" });
        assert_eq!(disc.discriminate(&ctx).unwrap(), "respond");

        ctx.insert_output(RouterOutput { action: "clarify" });
        assert_eq!(disc.discriminate(&ctx).unwrap(), "clarify");
    }

    #[test]
    fn discriminator_missing_output() {
        let disc = Discriminator::<RouterOutput, _>::new(|_| "test");
        let ctx = SystemContext::new();

        let result = disc.discriminate(&ctx);
        assert!(matches!(result, Err(PredicateError::OutputNotFound { .. })));
    }

    #[test]
    fn unit_discriminator_uses_implicit_unit_input() {
        let disc = Discriminator::<(), _>::new(|()| "default");
        let ctx = SystemContext::new();

        assert_eq!(
            disc.discriminate(&ctx)
                .expect("unit input is always available"),
            "default"
        );
    }

    #[test]
    fn boxed_discriminator() {
        let disc = BoxedDiscriminator::new::<RouterOutput, _>(|o| o.action);

        let mut ctx = SystemContext::new();
        ctx.insert_output(RouterOutput { action: "agent" });

        assert_eq!(disc.discriminate(&ctx).unwrap(), "agent");
    }
}
