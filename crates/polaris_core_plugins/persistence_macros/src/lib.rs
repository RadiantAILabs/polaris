//! Procedural macros for persistence in `polaris_core_plugins`.
//!
//! This crate provides `#[derive(Storable)]` for marking resources as
//! eligible for persistence.

mod storable;

use proc_macro::TokenStream;

/// Derive macro for the `Storable` trait.
///
/// Generates an implementation of `polaris_core_plugins::persistence::Storable` for the
/// annotated struct, providing a stable storage key and schema version for
/// resource persistence.
///
/// # Attributes
///
/// - `key` (required): The stable storage key for this resource.
/// - `schema_version` (optional): The schema version. Defaults to `"1.0.0"`.
///
/// # Example
///
/// ```
/// use serde::{Serialize, Deserialize};
/// use polaris_core_plugins::persistence::Storable;
///
/// #[derive(Serialize, Deserialize, Storable)]
/// #[storable(key = "ConversationMemory", schema_version = "2.0.0")]
/// struct ConversationMemory {
///     messages: Vec<String>,
/// }
/// ```
#[proc_macro_derive(Storable, attributes(storable))]
pub fn derive_storable(input: TokenStream) -> TokenStream {
    storable::derive_storable(input)
}
