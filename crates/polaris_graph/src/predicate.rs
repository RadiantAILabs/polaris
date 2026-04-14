//! Type-safe predicates for control flow decisions.
//!
//! Predicates evaluate conditions based on previous system outputs,
//! enabling type-safe control flow in graphs.
//!
//! # Architecture
//!
//! The predicate system follows the same type erasure pattern as systems:
//!
//! - [`Predicate<T, F>`] - Typed predicate that reads `Out<T>`
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

use polaris_system::param::SystemContext;
use polaris_system::resource::Output;
use std::any::{TypeId, type_name};
use std::fmt;
use std::marker::PhantomData;

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
/// use polaris_graph::predicate::{Predicate, BoxedPredicate, ErasedPredicate};
/// use polaris_system::param::SystemContext;
///
/// struct Score { value: i32 }
///
/// // Box a typed predicate into the trait object
/// let pred: BoxedPredicate = Box::new(Predicate::<Score, _>::new(|s| s.value > 50));
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

/// Type alias for boxed predicates stored in graph nodes.
pub type BoxedPredicate = Box<dyn ErasedPredicate>;

/// Object-safe trait for type-erased discriminators.
///
/// Discriminators are similar to predicates but return a string key
/// instead of a boolean, enabling multi-way branching in switch nodes.
///
/// # Examples
///
/// ```
/// use polaris_graph::predicate::{Discriminator, BoxedDiscriminator, ErasedDiscriminator};
/// use polaris_system::param::SystemContext;
///
/// struct RouterOutput { action: &'static str }
///
/// let disc: BoxedDiscriminator =
///     Box::new(Discriminator::<RouterOutput, _>::new(|out| out.action));
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
    fn discriminate(&self, ctx: &SystemContext<'_>) -> Result<&'static str, PredicateError>;

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

/// Type alias for boxed discriminators stored in switch nodes.
pub type BoxedDiscriminator = Box<dyn ErasedDiscriminator>;

/// A typed predicate that evaluates a condition on a previous system output.
///
/// `Predicate` wraps a closure that takes `&T` and returns `bool`,
/// where `T` is an output type from a previous system.
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
/// where `T` is an output type from a previous system.
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

impl<T, F> Discriminator<T, F>
where
    T: Output,
    F: Fn(&T) -> &'static str + Send + Sync + 'static,
{
    /// Creates a new discriminator from a closure.
    ///
    /// The closure receives an immutable reference to the output
    /// and should return a case key string.
    #[must_use]
    pub fn new(func: F) -> Self {
        Self {
            func,
            _marker: PhantomData,
        }
    }
}

impl<T, F> ErasedDiscriminator for Discriminator<T, F>
where
    T: Output,
    F: Fn(&T) -> &'static str + Send + Sync + 'static,
{
    fn discriminate(&self, ctx: &SystemContext<'_>) -> Result<&'static str, PredicateError> {
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
    fn boxed_predicate() {
        let pred: BoxedPredicate = Box::new(Predicate::<TestOutput, _>::new(|o| o.done));

        let mut ctx = SystemContext::new();
        ctx.insert_output(TestOutput {
            value: 0,
            done: true,
        });

        assert!(pred.evaluate(&ctx).unwrap());
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
    fn boxed_discriminator() {
        let disc: BoxedDiscriminator =
            Box::new(Discriminator::<RouterOutput, _>::new(|o| o.action));

        let mut ctx = SystemContext::new();
        ctx.insert_output(RouterOutput { action: "agent" });

        assert_eq!(disc.discriminate(&ctx).unwrap(), "agent");
    }
}
