//! Shared utilities for tool macro code generation.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, ExprLit, FnArg, GenericArgument, Lit, Meta, Pat, PatType, PathArguments,
    ReturnType, Signature, Type,
};

/// Validates that a function signature is suitable for `#[tool]`.
///
/// Rejects non-async, generic, unsafe, and extern functions.
pub(crate) fn validate_tool_signature(sig: &Signature) -> Option<TokenStream> {
    if sig.asyncness.is_none() {
        return Some(
            syn::Error::new_spanned(sig.fn_token, "#[tool] requires an async function")
                .to_compile_error(),
        );
    }

    if let Some(unsafety) = &sig.unsafety {
        return Some(
            syn::Error::new_spanned(unsafety, "#[tool] cannot be applied to unsafe functions")
                .to_compile_error(),
        );
    }

    if let Some(abi) = &sig.abi {
        return Some(
            syn::Error::new_spanned(abi, "#[tool] cannot be applied to extern functions")
                .to_compile_error(),
        );
    }

    if !sig.generics.params.is_empty() {
        return Some(
            syn::Error::new_spanned(&sig.generics, "#[tool] does not support generic parameters")
                .to_compile_error(),
        );
    }

    None
}

/// Validates that a standalone `#[tool]` function has no receiver (`&self`).
pub(crate) fn validate_standalone_tool(sig: &Signature) -> Option<TokenStream> {
    if let Some(FnArg::Receiver(receiver)) = sig.inputs.first() {
        return Some(
            syn::Error::new_spanned(
                receiver,
                "#[tool] standalone functions cannot have a `self` receiver; \
                 use #[toolset] on an impl block instead",
            )
            .to_compile_error(),
        );
    }
    None
}

/// Validates that a `#[tool]` method inside a `#[toolset]` has `&self`.
pub(crate) fn validate_toolset_method(sig: &Signature) -> Option<TokenStream> {
    match sig.inputs.first() {
        Some(FnArg::Receiver(receiver)) => {
            if receiver.mutability.is_some() {
                return Some(
                    syn::Error::new_spanned(
                        receiver,
                        "#[tool] methods must take `&self`, not `&mut self`; \
                         toolset wraps self in Arc which only provides shared references",
                    )
                    .to_compile_error(),
                );
            }
            if receiver.reference.is_none() {
                return Some(
                    syn::Error::new_spanned(
                        receiver,
                        "#[tool] methods must take `&self`, not `self` by value; \
                         toolset wraps self in Arc which only provides shared references",
                    )
                    .to_compile_error(),
                );
            }
            None
        }
        _ => Some(
            syn::Error::new_spanned(
                sig.fn_token,
                "#[tool] methods in a #[toolset] must take `&self` as the first parameter",
            )
            .to_compile_error(),
        ),
    }
}

/// Parsed information about a single function parameter.
#[derive(Debug, Clone)]
pub(crate) struct ParamInfo {
    /// Parameter name.
    pub name: String,
    /// Parameter type.
    pub ty: Type,
    /// Description from doc comments.
    pub description: Option<String>,
    /// Default value expression from `#[default(expr)]`.
    pub default_expr: Option<TokenStream>,
    /// Whether this parameter is injected from [`ToolContext`] rather than LLM args.
    pub is_context: bool,
}

/// Extracts doc comment text from attributes.
pub(crate) fn extract_doc_comments(attrs: &[Attribute]) -> Option<String> {
    let mut docs = Vec::new();

    for attr in attrs {
        if attr.path().is_ident("doc")
            && let Meta::NameValue(meta) = &attr.meta
            && let Expr::Lit(ExprLit {
                lit: Lit::Str(lit_str),
                ..
            }) = &meta.value
        {
            docs.push(lit_str.value().trim().to_string());
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n"))
    }
}

/// Parses a typed function parameter into a [`ParamInfo`].
pub(crate) fn parse_param(pat_type: &PatType) -> Option<ParamInfo> {
    let name = if let Pat::Ident(pat_ident) = &*pat_type.pat {
        pat_ident.ident.to_string()
    } else {
        return None;
    };

    let ty = (*pat_type.ty).clone();
    let description = extract_doc_comments(&pat_type.attrs);
    let default_expr = extract_default_expr(&pat_type.attrs);
    let is_context = pat_type
        .attrs
        .iter()
        .any(|attr| attr.path().is_ident("context"));

    Some(ParamInfo {
        name,
        ty,
        description,
        default_expr,
        is_context,
    })
}

/// Validates that `#[context]` parameters use supported type shapes.
///
/// Rejects nested `Option<Option<T>>` context params: the outer `Option` already
/// expresses "optional context value," so wrapping it again is redundant and
/// ambiguous in semantics.
pub(crate) fn validate_context_params(params: &[ParamInfo]) -> Option<TokenStream> {
    for param in params.iter().filter(|p| p.is_context) {
        if let Some(inner) = unwrap_option_inner(&param.ty)
            && unwrap_option_inner(inner).is_some()
        {
            return Some(
                syn::Error::new(
                    param.ty.span(),
                    "#[context] parameter cannot be `Option<Option<T>>`; \
                     use `Option<T>` for an optional context value",
                )
                .to_compile_error(),
            );
        }
    }
    None
}

/// Extracts the default value from `#[default(expr)]`.
fn extract_default_expr(attrs: &[Attribute]) -> Option<TokenStream> {
    for attr in attrs {
        if attr.path().is_ident("default") {
            return Some(
                attr.parse_args::<TokenStream>()
                    .expect("#[default(...)] requires a valid expression"),
            );
        }
    }
    None
}

/// Checks if a return type is `Result<T, E>`.
pub(crate) fn is_result_type(return_type: &ReturnType) -> bool {
    if let ReturnType::Type(_, ty) = return_type
        && let Type::Path(type_path) = ty.as_ref()
        && let Some(segment) = type_path.path.segments.last()
    {
        return segment.ident == "Result";
    }
    false
}

/// Extracts `T` from `Option<T>`, returning `None` if the type is not `Option`.
fn unwrap_option_inner(ty: &Type) -> Option<&Type> {
    if let Type::Path(type_path) = ty
        && let Some(segment) = type_path.path.segments.last()
        && segment.ident == "Option"
        && let PathArguments::AngleBracketed(args) = &segment.arguments
        && args.args.len() == 1
        && let GenericArgument::Type(inner) = &args.args[0]
    {
        Some(inner)
    } else {
        None
    }
}

/// Converts a `snake_case` string to `PascalCase`.
pub(crate) fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + &chars.as_str().to_lowercase(),
            }
        })
        .collect()
}

/// Generates the `definition()` body for a tool, producing a `ToolDefinition`.
///
/// Only input parameters (non-`#[context]`) are included in the JSON schema.
pub(crate) fn generate_definition(
    fn_name: &str,
    description: &str,
    params: &[ParamInfo],
    pt: &TokenStream,
) -> TokenStream {
    // Only include input params in the schema — context params are invisible to the LLM
    let param_additions: Vec<_> = params
        .iter()
        .filter(|p| !p.is_context)
        .map(|param| {
            let param_name_str = &param.name;
            let desc_code = param
                .description
                .as_ref()
                .map(|d| quote! { param_info.description = Some(#d.to_string()); })
                .unwrap_or_else(|| quote! {});

            let default_code = if let Some(default_expr) = &param.default_expr {
                quote! {
                    param_info.required = false;
                    param_info.default_value = Some(serde_json::json!(#default_expr));
                }
            } else {
                quote! {}
            };

            // For Option<T>, use the inner type's schema and mark as not required
            if let Some(inner_type) = unwrap_option_inner(&param.ty) {
                quote! {
                    {
                        let mut param_info = <#inner_type as #pt::InputParam>::schema_info(#param_name_str);
                        param_info.required = false;
                        #desc_code
                        #default_code
                        param_info
                    }
                }
            } else {
                let param_type = &param.ty;
                quote! {
                    {
                        let mut param_info = <#param_type as #pt::InputParam>::schema_info(#param_name_str);
                        #desc_code
                        #default_code
                        param_info
                    }
                }
            }
        })
        .collect();

    let desc_builder = if description.is_empty() {
        quote! {}
    } else {
        quote! { .with_description(#description) }
    };

    quote! {
        let meta = #pt::FunctionMetadata::new(#fn_name)
            #desc_builder
            #(
                .add_parameter(#param_additions)
            )*;
        meta.to_tool_definition()
    }
}

/// Generates the `execute()` body for a tool.
///
/// `call_target` is the token stream for the function/method to call,
/// e.g. `quote! { #impl_fn_name }` or `quote! { self.inner.#method_name }`.
///
/// Input parameters are extracted from `__args` (LLM-provided JSON).
/// Context parameters (marked with `#[context]`) are extracted from
/// `__ctx` ([`ToolContext`]).
pub(crate) fn generate_execute(
    fn_name: &str,
    call_target: &TokenStream,
    params: &[ParamInfo],
    return_type: &ReturnType,
    pt: &TokenStream,
) -> TokenStream {
    let has_input_params = params.iter().any(|p| !p.is_context);

    let call_preamble = if has_input_params {
        quote! {
            let __call = #pt::FunctionCall::from_value(#fn_name, __args)?;
        }
    } else {
        quote! {}
    };

    let param_extractions: Vec<_> = params
        .iter()
        .map(|param| {
            let param_ident = format_ident!("{}", &param.name);
            let param_type = &param.ty;

            if param.is_context {
                // Context parameter — extract from ToolContext
                generate_context_extraction(&param_ident, param_type, pt)
            } else {
                // Input parameter — extract from LLM-provided JSON args
                generate_input_extraction(param, pt)
            }
        })
        .collect();

    let call_args: Vec<_> = params
        .iter()
        .map(|param| {
            let ident = format_ident!("{}", &param.name);
            quote! { #ident }
        })
        .collect();

    let result_handling = if is_result_type(return_type) {
        quote! {
            let __result = #call_target(#(#call_args),*).await;
            match __result {
                Ok(__value) => serde_json::to_value(__value)
                    .map_err(#pt::ToolError::SerializationError),
                Err(__err) => Err(__err),
            }
        }
    } else {
        quote! {
            let __result = #call_target(#(#call_args),*).await;
            serde_json::to_value(__result)
                .map_err(#pt::ToolError::SerializationError)
        }
    };

    quote! {
        #call_preamble
        #(#param_extractions)*
        #result_handling
    }
}

/// Generates extraction code for a `#[context]` parameter from `__ctx`.
///
/// - `Option<T>` → `None` when absent (no error)
/// - `T` → `ToolError::ResourceNotFound` when absent (requires `T: Clone`)
fn generate_context_extraction(
    param_ident: &proc_macro2::Ident,
    param_type: &Type,
    pt: &TokenStream,
) -> TokenStream {
    if let Some(inner_type) = unwrap_option_inner(param_type) {
        // Option<T> context param — None when absent
        quote! {
            let #param_ident: #param_type = __ctx.get::<#inner_type>().cloned();
        }
    } else {
        // Required context param — error when absent
        quote! {
            let #param_ident: #param_type = __ctx
                .get::<#param_type>()
                .cloned()
                .ok_or_else(|| #pt::ToolError::resource_not_found(
                    ::std::any::type_name::<#param_type>()
                ))?;
        }
    }
}

/// Generates extraction code for an input parameter from `__call` (LLM JSON args).
fn generate_input_extraction(param: &ParamInfo, pt: &TokenStream) -> TokenStream {
    let param_ident = format_ident!("{}", &param.name);
    let param_name_str = &param.name;
    let param_type = &param.ty;

    if let Some(inner_type) = unwrap_option_inner(&param.ty) {
        quote! {
            let #param_ident: #param_type = <#inner_type as #pt::FunctionParam>::extract_optional(&__call, #param_name_str)?;
        }
    } else if let Some(default_expr) = &param.default_expr {
        quote! {
            let #param_ident: #param_type = <#param_type as #pt::FunctionParam>::extract_optional(&__call, #param_name_str)?
                .unwrap_or(#default_expr);
        }
    } else {
        quote! {
            let #param_ident: #param_type = <#param_type as #pt::FunctionParam>::extract(&__call, #param_name_str)?;
        }
    }
}
