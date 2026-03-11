//! Code generation for `#[tool]` on standalone async functions.

use crate::common::{
    extract_doc_comments, generate_definition, generate_execute, parse_param, to_pascal_case,
    validate_standalone_tool, validate_tool_signature,
};
use polaris_macro_utils::{PolarisCrate, resolve_crate_path};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn};

/// Generates a Tool impl struct for a standalone `#[tool]` async function.
///
/// The macro consumes the original function and generates:
/// - A private `__tool_impl_<name>` async function with the original body
/// - A `<Name>Tool` struct implementing `Tool`
/// - A constructor `fn <name>() -> <Name>Tool`
pub(crate) fn generate_tool_fn(input: &ItemFn) -> TokenStream {
    if let Some(err) = validate_tool_signature(&input.sig) {
        return err;
    }
    if let Some(err) = validate_standalone_tool(&input.sig) {
        return err;
    }

    let pt = resolve_crate_path(PolarisCrate::Tools);
    let pm = resolve_crate_path(PolarisCrate::Models);

    let fn_name = &input.sig.ident;
    let fn_name_str = fn_name.to_string();
    let struct_name = format_ident!("{}Tool", to_pascal_case(&fn_name_str));
    let impl_fn_name = format_ident!("__tool_impl_{}", fn_name);

    let fn_description = extract_doc_comments(&input.attrs);
    let description_str = fn_description.as_deref().unwrap_or("");

    // Parse parameters (standalone functions have no &self)
    let params: Vec<_> = input
        .sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                parse_param(pat_type)
            } else {
                None
            }
        })
        .collect();

    let definition_code = generate_definition(&fn_name_str, description_str, &params, &pt);
    let call_target = quote! { #impl_fn_name };
    let execute_code =
        generate_execute(&fn_name_str, &call_target, &params, &input.sig.output, &pt);

    let vis = &input.vis;
    let block = &input.block;

    // Build the private impl function with cleaned params
    let cleaned_params: Vec<_> = input
        .sig
        .inputs
        .iter()
        .map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                let mut cleaned = pat_type.clone();
                cleaned.attrs.retain(|attr| {
                    !attr.path().is_ident("default") && !attr.path().is_ident("doc")
                });
                FnArg::Typed(cleaned)
            } else {
                arg.clone()
            }
        })
        .collect();

    let output = &input.sig.output;

    quote! {
        async fn #impl_fn_name(#(#cleaned_params),*) #output #block

        #vis struct #struct_name;

        impl #pt::Tool for #struct_name {
            fn definition(&self) -> #pm::llm::ToolDefinition {
                #definition_code
            }

            fn execute(
                &self,
                __args: serde_json::Value,
            ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = Result<serde_json::Value, #pt::ToolError>> + Send + '_>> {
                Box::pin(async move {
                    #execute_code
                })
            }
        }

        /// Creates an instance of the `#fn_name_str` tool.
        #[must_use]
        #vis fn #fn_name() -> #struct_name {
            #struct_name
        }
    }
}
