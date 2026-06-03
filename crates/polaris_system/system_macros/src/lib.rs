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
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, ImplItem, ImplItemFn, ItemFn, ItemImpl, Lit, LitStr,
    Meta, Pat, PathArguments, ReturnType, Token, Type, parse_macro_input, punctuated::Punctuated,
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
    //
    // For the non-fallible case, the body is executed inside an inner `async move` block so
    // that any `return` statements in the body exit the inner block (yielding `#ret_type`)
    // rather than escaping the outer async block — which must return
    // `Result<#ret_type, SystemError>` to satisfy the `?` on fetch statements. Without this
    // isolation, writing `return x;` in an infallible system produces a confusing
    // type-mismatch error pointing at the `#[system]` macro site.
    let body_expr = if returns_result {
        quote!(#body)
    } else {
        quote!(::std::result::Result::Ok((async move #body).await))
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

// ─────────────────────────────────────────────────────────────────────────────
// #[plugin] attribute macro
// ─────────────────────────────────────────────────────────────────────────────

/// Parsed `#[plugin(...)]` arguments.
struct PluginArgs {
    id: LitStr,
    version: LitStr,
    provides: Vec<Type>,
}

impl syn::parse::Parse for PluginArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut id = None;
        let mut version = None;
        let mut provides = Vec::new();

        for meta in metas {
            match meta {
                Meta::NameValue(nv) if nv.path.is_ident("id") => {
                    id = Some(expect_lit_str(nv.value)?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("version") => {
                    version = Some(expect_lit_str(nv.value)?);
                }
                Meta::List(list) if list.path.is_ident("provides") => {
                    let types =
                        list.parse_args_with(Punctuated::<Type, Token![,]>::parse_terminated)?;
                    provides.extend(types);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unsupported `#[plugin]` argument; expected `id = \"...\"`, \
                         `version = \"x.y.z\"`, or `provides(Type, ...)`",
                    ));
                }
            }
        }

        let id =
            id.ok_or_else(|| syn::Error::new(input.span(), "`#[plugin]` requires `id = \"...\"`"))?;
        let version = version.ok_or_else(|| {
            syn::Error::new(input.span(), "`#[plugin]` requires `version = \"x.y.z\"`")
        })?;

        Ok(Self {
            id,
            version,
            provides,
        })
    }
}

/// Extracts a string literal from an attribute value expression.
fn expect_lit_str(expr: Expr) -> syn::Result<LitStr> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(lit), ..
    }) = expr
    {
        Ok(lit)
    } else {
        Err(syn::Error::new_spanned(expr, "expected a string literal"))
    }
}

/// Parses a `"major.minor.patch"` literal into its three components.
fn parse_version(lit: &LitStr) -> syn::Result<(u64, u64, u64)> {
    let raw = lit.value();
    let parts: Vec<&str> = raw.split('.').collect();
    let invalid = || syn::Error::new_spanned(lit, "version must be in `major.minor.patch` form");
    if parts.len() != 3 {
        return Err(invalid());
    }
    let major = parts[0].parse().map_err(|_| invalid())?;
    let minor = parts[1].parse().map_err(|_| invalid())?;
    let patch = parts[2].parse().map_err(|_| invalid())?;
    Ok((major, minor, patch))
}

/// Generates a `Plugin` impl from an `impl` block whose
/// `build` method declares its capability needs as typed parameters.
///
/// `#[plugin]` is to a plugin what [`macro@system`] is to a system: the `build` method's
/// parameter list is the single source of truth for what the plugin consumes, so the
/// declaration cannot drift from the access. The macro derives
/// `Plugin::access` from those parameters plus
/// the `provides(...)` attribute, and supplies the `ID`/`VERSION` constants.
///
/// # Usage
///
/// Apply it to `impl Plugin for YourPlugin`, omitting `ID`, `VERSION`, and `access`:
///
/// ```no_run
/// # use polaris_system::plugin;
/// # use polaris_system::plugin::{Contract, Extends, Plugin, Version};
/// # use polaris_system::server::Server;
/// # struct ModelRegistry { providers: u32 }
/// # impl Contract for ModelRegistry { const CONTRACT_VERSION: Version = Version::new(0, 1, 0); }
/// struct AnthropicPlugin;
///
/// #[plugin(id = "polaris::provider::anthropic", version = "0.1.0")]
/// impl Plugin for AnthropicPlugin {
///     // `Extends<ModelRegistry>` yields an infallible `&mut ModelRegistry`; the resolver
///     // guarantees a provider built first, so no `.expect("ModelsPlugin first")` is needed.
///     fn build(&self, mut registry: Extends<ModelRegistry>) {
///         registry.providers += 1;
///     }
/// }
/// ```
///
/// A provider plugin that inserts a new capability keeps a `&mut Server` parameter (the
/// inserts stay imperative) and declares what it provides via the attribute:
///
/// ```no_run
/// # use polaris_system::plugin;
/// # use polaris_system::plugin::{Contract, Plugin, Version};
/// # use polaris_system::server::Server;
/// # struct ModelRegistry;
/// # impl ModelRegistry { fn new() -> Self { Self } }
/// # impl Contract for ModelRegistry { const CONTRACT_VERSION: Version = Version::new(0, 1, 0); }
/// struct ModelsPlugin;
///
/// #[plugin(id = "polaris::models", version = "0.0.1", provides(ModelRegistry))]
/// impl Plugin for ModelsPlugin {
///     fn build(&self, server: &mut Server) {
///         server.insert_resource(ModelRegistry::new());
///     }
///     async fn ready(&self, _server: &mut Server) { /* freeze to global */ }
/// }
/// ```
///
/// Build parameters: `Requires<T>` → `&T`,
/// `Extends<T>` → `&mut T`,
/// `Optional<T>` → `Option<&T>`. Each `T` must
/// implement `Contract`; the version requirement is
/// the caret range of its contract version. Any other method (`ready`, `cleanup`,
/// `update`, `tick_schedules`, `dependencies`) is passed through unchanged.
#[proc_macro_attribute]
pub fn plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as PluginArgs);
    let mut item_impl = parse_macro_input!(item as ItemImpl);

    let ps = polaris_macro_utils::resolve_crate_path(polaris_macro_utils::PolarisCrate::System);

    let (major, minor, patch) = match parse_version(&args.version) {
        Ok(parts) => parts,
        Err(err) => return err.to_compile_error().into(),
    };

    // Rewrite the `build` method's typed parameters into the standard
    // `build(&self, server: &mut Server)` signature, collecting the build-param types so
    // their access declarations can be folded into the generated `access()`.
    let mut build_param_types: Vec<Type> = Vec::new();
    let mut found_build = false;
    for impl_item in &mut item_impl.items {
        if let ImplItem::Fn(method) = impl_item
            && method.sig.ident == "build"
        {
            found_build = true;
            match rewrite_build(method, &ps) {
                Ok(types) => build_param_types = types,
                Err(err) => return err.to_compile_error().into(),
            }
        }
    }

    if !found_build {
        return syn::Error::new_spanned(
            &item_impl,
            "`#[plugin]` requires a `build` method in the impl block",
        )
        .to_compile_error()
        .into();
    }

    let id_lit = &args.id;
    let provided = &args.provides;
    let provides_stmts = provided.iter().map(|ty| {
        quote! {
            access = access.provides::<#ty>(<#ty as #ps::plugin::Contract>::CONTRACT_VERSION);
        }
    });
    let access_stmts = build_param_types.iter().map(|ty| {
        quote! {
            <#ty as #ps::plugin::BuildParam>::contribute_access(&mut access);
        }
    });

    // Inject the generated trait items alongside the user's (rewritten) ones.
    let generated: ImplItem = syn::parse_quote! {
        const ID: &'static str = #id_lit;
    };
    item_impl.items.push(generated);
    let version_item: ImplItem = syn::parse_quote! {
        const VERSION: #ps::plugin::Version = #ps::plugin::Version::new(#major, #minor, #patch);
    };
    item_impl.items.push(version_item);
    let access_item: ImplItem = syn::parse_quote! {
        fn access(&self) -> #ps::plugin::PluginAccess {
            let mut access = #ps::plugin::PluginAccess::new();
            #(#provides_stmts)*
            #(#access_stmts)*
            access
        }
    };
    item_impl.items.push(access_item);

    quote!(#item_impl).into()
}

/// Rewrites a plugin `build` method in place: replaces its typed parameters with the
/// canonical `(&self, server: &mut Server)` signature and prepends the binding statements
/// that fetch each parameter from the server. Returns the build-param types (everything
/// that is not a raw `&Server`/`&mut Server`) so the caller can derive `access()`.
fn rewrite_build(method: &mut ImplItemFn, ps: &TokenStream2) -> syn::Result<Vec<Type>> {
    let mut bindings: Vec<TokenStream2> = Vec::new();
    let mut build_param_types: Vec<Type> = Vec::new();

    for arg in method.sig.inputs.iter().skip(1) {
        let FnArg::Typed(pat_type) = arg else {
            continue;
        };
        let pat = &pat_type.pat;
        let ty = &*pat_type.ty;

        if let Type::Reference(reference) = ty
            && type_is_server(&reference.elem)
        {
            // A raw `&mut Server` / `&Server` parameter — pass the server through so the
            // provide side can keep inserting resources imperatively. Only a reference
            // whose referent is `Server` is treated this way; any other reference falls
            // through to the build-param branch below, where it must implement
            // `BuildParam` (so e.g. a stray `&Config` fails with a clear trait bound
            // rather than silently binding to the server).
            if reference.mutability.is_some() {
                bindings.push(quote! { let #pat = &mut *_server; });
            } else {
                bindings.push(quote! { let #pat = &*_server; });
            }
        } else {
            // A typed build parameter (`Requires`/`Extends`/`Optional`): fetch it, panicking
            // with a named message if the resolver-guaranteed provider was somehow absent.
            bindings.push(quote! {
                let #pat = match <#ty as #ps::plugin::BuildParam>::fetch(&*_server) {
                    ::std::result::Result::Ok(value) => value,
                    ::std::result::Result::Err(err) => ::std::panic!(
                        "plugin `{}` could not resolve a build dependency: {}",
                        <Self as #ps::plugin::Plugin>::ID,
                        err
                    ),
                };
            });
            build_param_types.push(ty.clone());
        }
    }

    // Replace the parameter list with `(&self, _server: &mut Server)`.
    let receiver =
        method.sig.inputs.first().cloned().ok_or_else(|| {
            syn::Error::new_spanned(&method.sig, "plugin `build` must take `&self`")
        })?;
    let mut new_inputs: Punctuated<FnArg, Token![,]> = Punctuated::new();
    new_inputs.push(receiver);
    new_inputs.push(syn::parse_quote! { _server: &mut #ps::server::Server });
    method.sig.inputs = new_inputs;

    // Prepend the bindings to the original body.
    let original_stmts = std::mem::take(&mut method.block.stmts);
    let mut new_stmts: Vec<syn::Stmt> = Vec::new();
    for binding in bindings {
        new_stmts.push(syn::parse2(binding)?);
    }
    new_stmts.extend(original_stmts);
    method.block.stmts = new_stmts;

    Ok(build_param_types)
}

/// Returns `true` if `ty` names the `Server` type (bare or path-qualified, e.g.
/// `Server` or `polaris_system::server::Server`), so the `#[plugin]` macro can pass it
/// through as the imperative build handle rather than fetching it as a build parameter.
fn type_is_server(ty: &Type) -> bool {
    matches!(ty, Type::Path(type_path)
        if type_path.path.segments.last().is_some_and(|seg| seg.ident == "Server"))
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
