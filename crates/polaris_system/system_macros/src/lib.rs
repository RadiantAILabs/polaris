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
use quote::{format_ident, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemFn, ItemImpl, Lit,
    LitStr, Meta, Pat, PathArguments, ReturnType, Token, Type, parse_macro_input,
    punctuated::Punctuated,
};

/// Parsed arguments of the `#[system(..)]` attribute.
///
/// The only argument today is `inspect`, which selects the parameters whose
/// values are captured for observability. Selection is compile-time: the
/// `Debug` bound required to render a value lands on exactly the parameters
/// named here, so an un-annotated system imposes no bound on its parameters.
#[derive(Default)]
struct SystemArgs {
    /// Set by bare `inspect`, selecting every parameter.
    inspect_all: bool,
    /// Parameter names listed in `inspect(..)`.
    inspect_params: Vec<Ident>,
    /// Set by `return` inside `inspect(..)`, selecting the return value.
    inspect_return: bool,
}

impl SystemArgs {
    /// Returns whether the parameter bound to `name` was selected.
    fn selects(&self, name: &Ident) -> bool {
        self.inspect_all
            || self
                .inspect_params
                .iter()
                .any(|param| unraw(param) == unraw(name))
    }
}

/// Returns the identifier's name with any raw prefix stripped.
///
/// Rust resolves `r#memory` and `memory` to the same name, so selection and
/// validation must compare the two spellings as equal — `Ident` equality is
/// rawness-sensitive and would not.
fn unraw(ident: &Ident) -> String {
    let name = ident.to_string();
    match name.strip_prefix("r#") {
        Some(stripped) => stripped.to_owned(),
        None => name,
    }
}

impl syn::parse::Parse for SystemArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let mut args = Self::default();
        if input.is_empty() {
            return Ok(args);
        }

        let keyword: Ident = input.parse()?;
        if keyword != "inspect" {
            return Err(syn::Error::new_spanned(
                &keyword,
                format!("unknown `#[system]` argument `{keyword}`; expected `inspect`"),
            ));
        }
        let keyword_span = keyword.span();

        // Bare `inspect` selects every parameter. Every one of them must then be
        // `Debug`, which is why the parenthesized form exists.
        if input.is_empty() {
            args.inspect_all = true;
            return Ok(args);
        }

        if input.peek(Token![,]) {
            return Err(syn::Error::new(
                keyword_span,
                "bare `inspect` cannot be combined with other arguments; \
                 it already selects every parameter",
            ));
        }

        let list;
        syn::parenthesized!(list in input);
        while !list.is_empty() {
            // `return` is a keyword, so it can never collide with a parameter
            // name — which is why it, rather than an ident like `ret`, marks the
            // return value.
            if list.peek(Token![return]) {
                let token = list.parse::<Token![return]>()?;
                if args.inspect_return {
                    return Err(syn::Error::new(
                        token.span,
                        "duplicate `return` in `inspect(..)`",
                    ));
                }
                args.inspect_return = true;
            } else {
                let requested: Ident = list.parse()?;
                // A repeated name is usually a typo of a different parameter —
                // report it rather than silently deduplicating.
                if args
                    .inspect_params
                    .iter()
                    .any(|param| unraw(param) == unraw(&requested))
                {
                    return Err(syn::Error::new_spanned(
                        &requested,
                        format!("duplicate parameter `{requested}` in `inspect(..)`"),
                    ));
                }
                args.inspect_params.push(requested);
            }

            if list.is_empty() {
                break;
            }
            list.parse::<Token![,]>()?;
        }

        if !input.is_empty() {
            return Err(input.error("unexpected tokens after `inspect(..)`"));
        }

        if args.inspect_params.is_empty() && !args.inspect_return {
            return Err(syn::Error::new(
                keyword_span,
                "`inspect(..)` needs at least one parameter name or `return`; \
                 write bare `inspect` to select every parameter",
            ));
        }

        Ok(args)
    }
}

/// Returns the name of a parameter type's outermost wrapper, unwrapping a
/// single `Option<_>` layer so `Option<Out<T>>` classifies as `Out`.
///
/// Returns `None` when the type is not a path (a tuple or reference, say), which
/// the caller reports as an unclassified kind rather than guessing.
fn wrapper_ident(ty: &Type) -> Option<String> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    let name = segment.ident.to_string();
    if name != "Option" {
        return Some(name);
    }

    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Some(name);
    };
    arguments
        .args
        .iter()
        .find_map(|argument| match argument {
            GenericArgument::Type(inner) => wrapper_ident(inner),
            _ => None,
        })
        .or(Some(name))
}

/// Renders a type back to a compact display string.
///
/// Re-emitted tokens stringify with spaces around punctuation (`ResMut <
/// crate :: Memory >`), so the angle brackets, commas, and path separators are
/// closed up here. This is done at expansion time rather than with
/// `stringify!` so the recorded form is deterministic instead of depending on
/// token-spacing rules. Only the separators path-shaped types produce are
/// closed up; a non-path type reaching this (`Out<[u8; 4]>`, say) may keep
/// stray spaces (`[u8 ; 4]`).
fn tokens_display(tokens: &TokenStream2) -> String {
    tokens
        .to_string()
        .replace(" :: ", "::")
        .replace(":: ", "::")
        .replace(" ::", "::")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
}

/// Renders a parsed type back to a compact display string.
fn type_display(ty: &Type) -> String {
    tokens_display(&quote!(#ty))
}

/// Returns the resource type inside a recognized wrapper — the `T` of
/// `Res<T>`, `ResMut<T>`, `Out<T>`, `ErrOut<T>`, unwrapping one `Option<..>`
/// layer, and skipping lifetime arguments (`Res<'_, T>` yields `T`).
///
/// Returns `None` for unclassified types, which the caller reports with the
/// full declared type instead.
fn inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let first_type = arguments.args.iter().find_map(|argument| match argument {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })?;

    match segment.ident.to_string().as_str() {
        "Option" => inner_type(first_type),
        "Res" | "ResMut" | "Out" | "ErrOut" => Some(first_type),
        _ => None,
    }
}

/// Builds the `ParamKind` expression for a parameter type.
fn param_kind_expr(ty: &Type, ps: &TokenStream2) -> TokenStream2 {
    let variant = match wrapper_ident(ty).as_deref() {
        Some("Res") => quote!(Res),
        Some("ResMut") => quote!(ResMut),
        Some("Out") => quote!(Out),
        Some("ErrOut") => quote!(ErrOut),
        _ => quote!(Other),
    };
    quote!(#ps::param::inspect::ParamKind::#variant)
}

/// Builds the capture statement emitted after a selected parameter's fetch.
///
/// The whole record is behind an `Option` check on the context, and the value
/// is passed as a closure, so an inactive sink costs one discriminant read and
/// a declined record costs no formatting.
fn capture_stmt(
    ps: &TokenStream2,
    system_name: &str,
    param_name: &Ident,
    param_type: &Type,
    kind: &TokenStream2,
) -> TokenStream2 {
    let param_str = param_name.to_string();

    // Record the inner resource type (`Memory`, not `ResMut<'_, Memory>`): the
    // kind already names the wrapper, and the inner type is spelling-invariant
    // where the declared form is not. Unclassified wrappers fall back to the
    // full declared type.
    let type_str = inner_type(param_type).map_or_else(|| type_display(param_type), type_display);

    // Span the rendering call at the parameter's own type. `InspectParam` is the
    // only bound this feature adds, so when a selected parameter is not `Debug`
    // the resulting error points at that parameter rather than at the attribute.
    let render = quote_spanned! { param_type.span() =>
        &|| #ps::param::inspect::InspectParam::inspect(&#param_name)
    };

    quote! {
        if let ::std::option::Option::Some(__polaris_sink) =
            #ps::param::SystemContext::inspection(ctx)
        {
            #ps::param::inspect::InspectionSink::record(
                __polaris_sink,
                #ps::param::inspect::ParamMeta::new(
                    #system_name,
                    #param_str,
                    #type_str,
                    #kind,
                    #ps::param::inspect::Phase::Before,
                ),
                #render,
            );
        }
    }
}

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
/// # Parameter Inspection
///
/// The attribute's one optional argument, `inspect(..)`, selects parameters —
/// and, via the `return` keyword, the return value — whose values are captured
/// for observability when a sink is installed on the context:
///
/// ```
/// # use polaris_system::param::ResMut;
/// # use polaris_system::resource::LocalResource;
/// # use polaris_system_macros::system;
/// # #[derive(Debug)]
/// # struct Memory { turns: u32 }
/// # impl LocalResource for Memory {}
/// #[system(inspect(memory, return))]
/// async fn advance(mut memory: ResMut<Memory>) -> u32 {
///     memory.turns += 1;
///     memory.turns
/// }
/// # let _ = advance();
/// ```
///
/// Bare `#[system(inspect)]` selects every parameter (but not the return
/// value). Every selected parameter's type must be `Debug`; a non-`Debug`
/// selection is a compile error at that parameter. Capture is live only when
/// a sink is installed via `SystemContext::replace_inspection` — see the
/// `polaris_system::param::inspect` module for the full mechanism, cost
/// model, and sensitive-value guidance.
///
/// # Attribute Forwarding
///
/// Attributes written on the function are forwarded rather than silently
/// discarded: doc comments land on the generated struct, `cfg`/`cfg_attr`
/// gate every generated item, and lint attributes (`allow`, `expect`, `warn`,
/// `deny`, `forbid`) scope the generated `run` method, which contains the
/// function body. Any other attribute has no meaningful target after
/// expansion and is a compile error at that attribute.
///
/// The rejection applies to attributes left for `#[system]` itself to handle.
/// Another attribute macro still composes when written **above** `#[system]`:
/// attribute macros expand outside-in, so it transforms the original function
/// before `#[system]` ever sees it.
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
pub fn system(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as SystemArgs);
    let input = parse_macro_input!(item as ItemFn);

    // Validate: must be async
    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input.sig.fn_token, "system functions must be async")
            .to_compile_error()
            .into();
    }

    // Validate: no generics. The generated struct and factory carry no
    // generic parameters, so accepting them would silently discard the
    // signature element (an unused parameter) or fail with an unspanned
    // resolution error inside the body (a used one).
    if let Some(where_clause) = &input.sig.generics.where_clause {
        return syn::Error::new_spanned(
            where_clause,
            "system functions cannot have a `where` clause; the generated system \
             struct is not generic",
        )
        .to_compile_error()
        .into();
    }
    if !input.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.sig.generics,
            "system functions cannot be generic; the generated system struct \
             would discard the generic parameters",
        )
        .to_compile_error()
        .into();
    }

    // Partition the function's attributes for forwarding. The macro replaces
    // the function with generated items, so an attribute not re-emitted would
    // be silently discarded — the same failure class as a discarded generic.
    // Doc comments describe the system and land on the generated struct;
    // `cfg`/`cfg_attr` must gate every generated item, or a disabled system
    // would leave orphaned items behind; lint attributes scope the function
    // body, which lives in the generated `run` method. Anything else (another
    // attribute macro, `#[inline]`, ...) has no meaningful target after
    // expansion and is rejected rather than dropped.
    let mut doc_attrs: Vec<syn::Attribute> = Vec::new();
    let mut cfg_attrs: Vec<syn::Attribute> = Vec::new();
    let mut lint_attrs: Vec<syn::Attribute> = Vec::new();
    for attr in &input.attrs {
        let path = attr.path();
        let target = if path.is_ident("doc") {
            &mut doc_attrs
        } else if path.is_ident("cfg") || path.is_ident("cfg_attr") {
            &mut cfg_attrs
        } else if ["allow", "expect", "warn", "deny", "forbid"]
            .iter()
            .any(|lint| path.is_ident(lint))
        {
            &mut lint_attrs
        } else {
            return syn::Error::new_spanned(
                attr,
                "unsupported attribute on a `#[system]` function: the macro replaces \
                 the function with generated items, so only `doc` (forwarded to the \
                 generated struct), `cfg`/`cfg_attr` (forwarded to every generated \
                 item), and lint attributes (forwarded to the generated `run` method) \
                 are supported",
            )
            .to_compile_error()
            .into();
        };
        // Re-emit in outer position regardless of how the attribute was
        // written: `#![allow(..)]` at the top of the body parses as an inner
        // attribute of the function, and the body's new home is a method.
        let mut attr = attr.clone();
        attr.style = syn::AttrStyle::Outer;
        target.push(attr);
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

    // Span for the return-value rendering call, so selecting `return` on a
    // non-`Debug` return type errors at the type itself — the same precise,
    // local diagnostic a non-`Debug` parameter gets.
    let ret_span = match &input.sig.output {
        ReturnType::Type(_, ty) => ty.span(),
        ReturnType::Default => input.sig.ident.span(),
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

            // The generated body binds the context as `ctx` and its own
            // locals under a `__polaris` prefix. A parameter reusing either
            // name would shadow them — today that is a confusing type error,
            // and it must never become a silent capture of the wrong value.
            if unraw(param_name) == "ctx" {
                return syn::Error::new_spanned(
                    param_name,
                    "parameter name `ctx` collides with the context binding \
                     the generated system body uses; rename the parameter",
                )
                .to_compile_error()
                .into();
            }
            if unraw(param_name).starts_with("__polaris") {
                return syn::Error::new_spanned(
                    param_name,
                    "parameter names beginning with `__polaris` are reserved \
                     for identifiers the `#[system]` macro generates",
                )
                .to_compile_error()
                .into();
            }

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

            // Capture goes immediately after the fetch, so the recorded value is
            // the one the body is about to see. For `ResMut` this reads through
            // the live borrow rather than competing with it.
            let capture = if args.selects(param_name) {
                let kind = param_kind_expr(param_type, &ps);
                capture_stmt(&ps, &fn_name_str, param_name, param_type, &kind)
            } else {
                TokenStream2::new()
            };

            fetch_stmts.push(quote! {
                #fetch_stmt
                #capture
            });
            param_names.push(param_name.clone());
            param_types.push(param_type.clone());
        }
    }

    // Report a name in `inspect(..)` that matches no parameter, rather than
    // silently capturing nothing.
    for requested in &args.inspect_params {
        if !param_names
            .iter()
            .any(|name| unraw(name) == unraw(requested))
        {
            return syn::Error::new_spanned(
                requested,
                format!("no parameter named `{requested}` in this system"),
            )
            .to_compile_error()
            .into();
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
    //
    // A fallible body gets the same isolation when `inspect(return)` is selected: inlined,
    // an explicit `return Ok(v)` would exit the outer async block past the capture site
    // below and silently skip the record. The inner block turns that `return` into the
    // block's value instead. (`?` behaves identically in both forms: the annotation on
    // `__polaris_result` gives the block the same `Result<#ret_type, SystemError>` target
    // the outer block provided.) Without `inspect(return)` the body stays inlined, so the
    // expansion of existing systems is unchanged.
    let body_expr = if returns_result {
        if args.inspect_return {
            quote!((async move #body).await)
        } else {
            quote!(#body)
        }
    } else {
        quote!(::std::result::Result::Ok((async move #body).await))
    };

    // `inspect(.., return)` records the produced value. Only the success value is
    // recorded: a failure carries no output, and the error already travels the
    // graph's own error flow. Inspection stays read-only on the output channel —
    // it observes the value on its way out and never writes one.
    let ret_type_str = tokens_display(&ret_type);
    let body_expr = if args.inspect_return {
        let render = quote_spanned! { ret_span =>
            &|| #ps::param::inspect::inspect_value(__polaris_output)
        };
        quote! {
            {
                let __polaris_result: ::std::result::Result<#ret_type, #ps::system::SystemError> =
                    #body_expr;
                if let ::std::result::Result::Ok(ref __polaris_output) = __polaris_result
                    && let ::std::option::Option::Some(__polaris_sink) =
                        #ps::param::SystemContext::inspection(ctx)
                {
                    #ps::param::inspect::InspectionSink::record(
                        __polaris_sink,
                        #ps::param::inspect::ParamMeta::new(
                            #fn_name_str,
                            "return",
                            #ret_type_str,
                            #ps::param::inspect::ParamKind::Return,
                            #ps::param::inspect::Phase::After,
                        ),
                        #render,
                    );
                }
                __polaris_result
            }
        }
    } else {
        body_expr
    };

    // The function's own docs, when present, lead the generated struct's docs
    // with the canned provenance line demoted to a trailing paragraph.
    let canned_doc = "System struct generated by the `#[system]` macro.";
    let struct_doc = if doc_attrs.is_empty() {
        quote! { #[doc = #canned_doc] }
    } else {
        quote! {
            #(#doc_attrs)*
            #[doc = ""]
            #[doc = #canned_doc]
        }
    };

    // Generate the struct and System impl
    // Note: Uses `::polaris_system::` paths for use within polaris_system crate.
    // The macro is re-exported from polaris_system via `pub use polaris_system_macros::system;`
    let expanded = quote! {
        #struct_doc
        #(#cfg_attrs)*
        #vis struct #struct_name;

        #(#cfg_attrs)*
        impl #ps::system::System for #struct_name {
            type Output = #ret_type;

            #(#lint_attrs)*
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
        #(#cfg_attrs)*
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
