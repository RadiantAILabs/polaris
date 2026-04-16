//! Code generation for `#[toolset]` on impl blocks.

use polaris_macro_utils::{PolarisCrate, resolve_crate_path};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, Generics, ImplItem, ImplItemFn, ItemImpl, Type};

use crate::common::{
    extract_doc_comments, generate_definition, generate_execute, parse_param, to_pascal_case,
    validate_context_params, validate_tool_signature, validate_toolset_method,
};

/// Generates a Toolset impl for an impl block with `#[tool]` methods.
pub(crate) fn generate_toolset(input: &ItemImpl) -> TokenStream {
    let pt = resolve_crate_path(PolarisCrate::Tools);

    let self_ty = &input.self_ty;
    let (impl_generics, _ty_generics, where_clause) = input.generics.split_for_impl();

    // Find all methods marked with #[tool]
    let mut tool_methods: Vec<ImplItemFn> = Vec::new();

    for item in &input.items {
        if let ImplItem::Fn(method) = item {
            let has_tool_attr = method.attrs.iter().any(|attr| attr.path().is_ident("tool"));
            if has_tool_attr {
                if let Some(err) = validate_tool_signature(&method.sig) {
                    return err;
                }
                if let Some(err) = validate_toolset_method(&method.sig) {
                    return err;
                }
                tool_methods.push(method.clone());
            }
        }
    }

    // Generate inner tool structs for each method
    let tool_struct_defs = generate_tool_structs(self_ty, &tool_methods, &input.generics);

    // Generate Toolset impl
    let tool_constructors: Vec<_> = tool_methods
        .iter()
        .map(|method| {
            let method_name = &method.sig.ident;
            let struct_name = format_ident!(
                "{}_{}Tool",
                type_name_str(self_ty),
                to_pascal_case(&method_name.to_string())
            );
            quote! {
                Box::new(#struct_name { inner: ::std::sync::Arc::clone(&__arc_self) })
            }
        })
        .collect();

    // Remove #[tool] and param doc/default/context attrs from the original impl
    let cleaned_items: Vec<_> = input
        .items
        .iter()
        .map(|item| {
            if let ImplItem::Fn(method) = item {
                let mut cleaned = method.clone();
                let is_tool = cleaned
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("tool"));
                cleaned.attrs.retain(|attr| !attr.path().is_ident("tool"));
                if is_tool {
                    for input in &mut cleaned.sig.inputs {
                        if let FnArg::Typed(pat_type) = input {
                            pat_type.attrs.retain(|attr| {
                                !attr.path().is_ident("doc")
                                    && !attr.path().is_ident("default")
                                    && !attr.path().is_ident("context")
                            });
                        }
                    }
                }
                ImplItem::Fn(cleaned)
            } else {
                item.clone()
            }
        })
        .collect();

    quote! {
        // Original impl block with attributes stripped
        impl #impl_generics #self_ty #where_clause {
            #(#cleaned_items)*
        }

        #tool_struct_defs

        impl #impl_generics #pt::Toolset for #self_ty #where_clause {
            fn tools(self) -> Vec<Box<dyn #pt::Tool>> {
                let __arc_self = ::std::sync::Arc::new(self);
                vec![
                    #(#tool_constructors),*
                ]
            }
        }
    }
}

fn generate_tool_structs(
    self_ty: &Type,
    methods: &[ImplItemFn],
    generics: &Generics,
) -> TokenStream {
    let structs: Vec<_> = methods
        .iter()
        .map(|method| generate_single_tool_struct(self_ty, method, generics))
        .collect();

    quote! { #(#structs)* }
}

fn generate_single_tool_struct(
    self_ty: &Type,
    method: &ImplItemFn,
    generics: &Generics,
) -> TokenStream {
    let pt = resolve_crate_path(PolarisCrate::Tools);
    let pm = resolve_crate_path(PolarisCrate::Models);

    let method_name = &method.sig.ident;
    let method_name_str = method_name.to_string();
    let struct_name = format_ident!(
        "{}_{}Tool",
        type_name_str(self_ty),
        to_pascal_case(&method_name_str)
    );

    let fn_description = extract_doc_comments(&method.attrs);
    let description_str = fn_description.as_deref().unwrap_or("");

    // Parse parameters (skip &self)
    let params: Vec<_> = method
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

    if let Some(err) = validate_context_params(&params) {
        return err;
    }

    let definition_code = generate_definition(&method_name_str, description_str, &params, &pt);
    let call_target = quote! { self.inner.#method_name };
    let execute_code = generate_execute(
        &method_name_str,
        &call_target,
        &params,
        &method.sig.output,
        &pt,
    );

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    // Collect existing where predicates to merge with the Send + Sync + 'static bound.
    let existing_predicates: Vec<_> = where_clause
        .map(|wc| wc.predicates.iter().collect())
        .unwrap_or_default();

    quote! {
        struct #struct_name #impl_generics
        where
            #self_ty: Send + Sync + 'static,
            #(#existing_predicates),*
        {
            inner: ::std::sync::Arc<#self_ty>,
        }

        impl #impl_generics #pt::Tool for #struct_name #ty_generics
        where
            #self_ty: Send + Sync + 'static,
            #(#existing_predicates),*
        {
            fn definition(&self) -> #pm::llm::ToolDefinition {
                #definition_code
            }

            fn execute<'__ctx>(
                &'__ctx self,
                __args: serde_json::Value,
                __ctx: &'__ctx #pt::ToolContext,
            ) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = Result<serde_json::Value, #pt::ToolError>> + Send + '__ctx>> {
                Box::pin(async move {
                    #execute_code
                })
            }
        }
    }
}

fn type_name_str(ty: &Type) -> String {
    if let Type::Path(type_path) = ty
        && let Some(segment) = type_path.path.segments.last()
    {
        return segment.ident.to_string();
    }
    panic!("#[toolset] impl target must be a path type")
}
