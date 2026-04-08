//! Shared utilities for Polaris procedural macro crates.
//!
//! Provides crate-path resolution so that generated code emits correct
//! fully-qualified paths regardless of whether the consumer depends on
//! an individual Polaris crate or the `polaris` umbrella re-export.

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};

/// A Polaris crate that macro-generated code may reference.
pub enum PolarisCrate {
    /// `polaris_system`
    System,
    /// `polaris_tools`
    Tools,
    /// `polaris_models`
    Models,
    /// `polaris_core_plugins`
    CorePlugins,
}

impl PolarisCrate {
    /// Returns the `Cargo.toml` package name for this crate.
    fn as_str(&self) -> &'static str {
        match self {
            Self::System => "polaris_system",
            Self::Tools => "polaris_tools",
            Self::Models => "polaris_models",
            Self::CorePlugins => "polaris_core_plugins",
        }
    }
}

/// Returns a [`TokenStream`] path for the given Polaris crate.
///
/// Resolution order:
/// 1. Direct dependency (possibly renamed in `Cargo.toml`).
/// 2. Indirect access via the `polaris` umbrella crate (`polaris::<name>`).
/// 3. Fallback to the literal crate name (compile error will point the user
///    to the missing dependency).
pub fn resolve_crate_path(krate: PolarisCrate) -> TokenStream {
    let name = krate.as_str();

    match crate_name(name) {
        Ok(FoundCrate::Itself) => {
            let ident = format_ident!("{}", name);
            quote!(#ident)
        }
        Ok(FoundCrate::Name(found)) => {
            let ident = format_ident!("{}", found);
            quote!(#ident)
        }
        Err(_) => match crate_name("polaris-ai") {
            Ok(FoundCrate::Name(found)) => {
                let polaris = format_ident!("{}", found);
                let ident = format_ident!("{}", name);
                quote!(#polaris::#ident)
            }
            _ => {
                let ident = format_ident!("{}", name);
                quote!(#ident)
            }
        },
    }
}
