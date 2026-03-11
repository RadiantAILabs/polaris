//! Procedural macros for the `polaris_system` crate.
//!
//! This crate provides the `#[system]` attribute macro for defining
//! system components in the Polaris framework.
//!
//! # Example
//!
//! ```
//! use polaris_system::param::Res;
//! use polaris_system::resource::GlobalResource;
//! use polaris_system_macros::system;
//!
//! # struct Counter { value: i32 }
//! # impl GlobalResource for Counter {}
//! # struct CounterOutput { value: i32 }
//! #[system]
//! async fn read_counter(counter: Res<'_, Counter>) -> CounterOutput {
//!     CounterOutput { value: counter.value }
//! }
//!
//! // Use the generated system:
//! let system = read_counter();
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    FnArg, GenericArgument, ItemFn, Pat, PathArguments, ReturnType, Type, parse_macro_input,
};

/// Transforms an async function into a System implementation.
///
/// The macro generates a struct that implements `System`, allowing async functions
/// with lifetime-parameterized parameters (like `Res<'_, T>`) to work correctly.
///
/// # Usage
///
/// ```
/// # use polaris_system::param::Res;
/// # use polaris_system::resource::GlobalResource;
/// # use polaris_system_macros::system;
/// # struct MyResource { field: i32 }
/// # impl GlobalResource for MyResource {}
/// # struct MyOutput { value: i32 }
/// #[system]
/// async fn my_system(res: Res<'_, MyResource>) -> MyOutput {
///     MyOutput { value: res.field }
/// }
///
/// // Fallible systems can return Result<T, SystemError>.
/// // The macro extracts T as the output type and propagates errors.
/// # use polaris_system::system::SystemError;
/// # fn do_something(_r: &MyResource) -> Result<i32, Box<dyn std::error::Error + Send + Sync>> { Ok(0) }
/// #[system]
/// async fn fallible_system(res: Res<'_, MyResource>) -> Result<MyOutput, SystemError> {
///     let value = do_something(&res).map_err(|err| SystemError::ExecutionError(err.to_string()))?;
///     Ok(MyOutput { value })
/// }
///
/// // Creates a system:
/// let system = my_system();
/// ```
///
/// # Generated Code
///
/// For an async function like:
/// ```
/// # use polaris_system::param::Res;
/// # use polaris_system::resource::GlobalResource;
/// # use polaris_system_macros::system;
/// # struct Counter { count: i32 }
/// # impl GlobalResource for Counter {}
/// # struct Output { value: i32 }
/// #[system]
/// async fn read_counter(counter: Res<'_, Counter>) -> Output {
///     Output { value: counter.count }
/// }
/// ```
///
/// The macro generates:
/// ```
/// # use polaris_system::param::{Res, SystemContext, SystemParam, SystemAccess};
/// # use polaris_system::system::{System, SystemError, BoxFuture};
/// # use polaris_system::resource::GlobalResource;
/// # struct Counter { count: i32 }
/// # impl GlobalResource for Counter {}
/// # struct Output { value: i32 }
/// struct ReadCounterSystem;
///
/// impl System for ReadCounterSystem {
///     type Output = Output;
///
///     fn run<'a>(&'a self, ctx: &'a SystemContext<'_>)
///         -> BoxFuture<'a, ::core::result::Result<Self::Output, SystemError>>
///     {
///         Box::pin(async move {
///             let counter = Res::<Counter>::fetch(ctx)?;
///             Ok({ Output { value: counter.count } })
///         })
///     }
///
///     fn name(&self) -> &'static str {
///         "read_counter"
///     }
///     
///     fn access(&self) -> SystemAccess {
///         SystemAccess::new()
///     }
/// }
///
/// fn read_counter() -> ReadCounterSystem {
///     ReadCounterSystem
/// }
/// ```
#[proc_macro_attribute]
pub fn system(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    // Validate: must be async
    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input.sig.fn_token, "system functions must be async")
            .to_compile_error()
            .into();
    }

    // Auto-detect crate path (works with both `polaris_system` and `polaris` umbrella).
    let ps = polaris_macro_utils::resolve_crate_path(polaris_macro_utils::PolarisCrate::System);

    let fn_name = &input.sig.ident;
    let fn_name_str = fn_name.to_string();
    let struct_name = format_ident!("{}System", to_pascal_case(&fn_name_str));
    let body = &input.block;
    let vis = &input.vis;

    // Extract return type (default to () if not specified).
    // If the return type is `Result<T, SystemError>`, extract `T` as the output type
    // and let the body's Result propagate directly (no extra `Ok()` wrapping).
    let (ret_type, returns_result) = match &input.sig.output {
        ReturnType::Type(_, ty) => {
            if let Some(ok_type) = extract_result_system_error(ty) {
                (quote!(#ok_type), true)
            } else {
                (quote!(#ty), false)
            }
        }
        ReturnType::Default => (quote!(()), false),
    };

    // Extract parameters and generate fetch calls + access merges
    let mut fetch_stmts = Vec::new();
    let mut param_names = Vec::new();
    let mut param_types = Vec::new();

    for arg in &input.sig.inputs {
        if let FnArg::Typed(pat_type) = arg {
            // Get parameter name
            let param_name = if let Pat::Ident(pat_ident) = &*pat_type.pat {
                &pat_ident.ident
            } else {
                return syn::Error::new_spanned(
                    &pat_type.pat,
                    "system parameters must be simple identifiers",
                )
                .to_compile_error()
                .into();
            };

            // Get parameter type (strip the lifetime for the fetch call)
            let param_type = &pat_type.ty;

            // Generate: let param_name = ParamType::fetch(ctx)?;
            // We need to handle the mutability
            let is_mut = if let Pat::Ident(pat_ident) = &*pat_type.pat {
                pat_ident.mutability.is_some()
            } else {
                false
            };

            let fetch_stmt = if is_mut {
                quote! {
                    let mut #param_name = <#param_type as #ps::param::SystemParam>::fetch(ctx)?;
                }
            } else {
                quote! {
                    let #param_name = <#param_type as #ps::param::SystemParam>::fetch(ctx)?;
                }
            };

            fetch_stmts.push(fetch_stmt);
            param_names.push(param_name.clone());
            param_types.push(param_type.clone());
        }
    }

    // Generate access merge statements for each parameter type
    let access_merges: Vec<_> = param_types
        .iter()
        .map(|param_type| {
            quote! {
                access.merge(&<#param_type as #ps::param::SystemParam>::access());
            }
        })
        .collect();

    // When the function returns `Result<T, SystemError>`, the body already produces a Result,
    // so we use it directly. Otherwise, wrap in `Ok()`.
    let body_expr = if returns_result {
        quote!(#body)
    } else {
        quote!(::std::result::Result::Ok(#body))
    };

    // Generate the struct and System impl
    // Note: Uses `::polaris_system::` paths for use within polaris_system crate.
    // The macro is re-exported from polaris_system via `pub use polaris_system_macros::system;`
    let expanded = quote! {
        /// System struct generated by the `#[system]` macro.
        #vis struct #struct_name;

        impl #ps::system::System for #struct_name {
            type Output = #ret_type;

            fn run<'a>(
                &'a self,
                ctx: &'a #ps::param::SystemContext<'_>,
            ) -> #ps::system::BoxFuture<'a, ::std::result::Result<Self::Output, #ps::system::SystemError>> {
                ::std::boxed::Box::pin(async move {
                    #(#fetch_stmts)*
                    #body_expr
                })
            }

            fn name(&self) -> &'static str {
                #fn_name_str
            }

            fn access(&self) -> #ps::param::SystemAccess {
                let mut access = #ps::param::SystemAccess::new();
                #(#access_merges)*
                access
            }

            fn is_fallible(&self) -> bool {
                #returns_result
            }
        }

        /// Creates an instance of the system.
        #vis fn #fn_name() -> #struct_name {
            #struct_name
        }
    };

    expanded.into()
}

/// If `ty` is `Result<T, SystemError>`, returns `Some(T)`.
///
/// This allows the `#[system]` macro to detect fallible systems and avoid
/// double-wrapping the return value in `Ok()`. The error type is checked
/// by its last path segment being `SystemError`.
fn extract_result_system_error(ty: &Type) -> Option<Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };

    let last_segment = type_path.path.segments.last()?;
    if last_segment.ident != "Result" {
        return None;
    }

    let PathArguments::AngleBracketed(angle_args) = &last_segment.arguments else {
        return None;
    };

    if angle_args.args.len() != 2 {
        return None;
    }

    // Check that the error type's last segment is `SystemError`.
    let GenericArgument::Type(err_type) = &angle_args.args[1] else {
        return None;
    };

    let Type::Path(err_path) = err_type else {
        return None;
    };

    let err_last_segment = err_path.path.segments.last()?;
    if err_last_segment.ident != "SystemError" {
        return None;
    }

    // Extract the Ok type.
    let GenericArgument::Type(ok_type) = &angle_args.args[0] else {
        return None;
    };
    Some(ok_type.clone())
}

/// Converts `snake_case` to `PascalCase`.
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect()
}
