//! Route macros for the API that keep utoipa optional (api's `openapi`
//! feature) and keep handler futures cheap to compile.
//!
//! - `#[path(...)]` takes `#[utoipa::path]`'s arguments. With `openapi` it
//!   expands to `#[utoipa::path]`; without, only the method and the path are
//!   read, for [`routes!`]. Either way an `async fn` handler returns a boxed
//!   future: axum's `Handler` and every layer above it are then compiled for
//!   one future type instead of one per handler.
//! - `routes!` without `openapi` gives the `(path, MethodRouter)` pairs that
//!   `api::openapi::OpenApiRouter::routes` registers with axum.
//! - `ToSchema` and `IntoParams` here derive nothing; they only accept the
//!   `#[schema]`, `#[into_params]` and `#[param]` attributes so the
//!   annotations compile without utoipa.

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::{ItemFn, LitStr, ReturnType, Token, parse_macro_input};

const METHODS: [&str; 8] = [
    "get", "post", "put", "delete", "patch", "head", "options", "trace",
];

#[proc_macro_attribute]
pub fn path(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = TokenStream2::from(args);
    let function = parse_macro_input!(item as ItemFn);
    let handler = boxed(function.clone());
    if cfg!(feature = "openapi") {
        return quote!(#[::utoipa::path(#args)] #handler).into();
    }
    let args_tokens = args.clone();
    let (method, path) = match method_and_path(args) {
        Ok(found) => found,
        Err(message) => {
            return syn::Error::new_spanned(&function.sig.ident, message)
                .to_compile_error()
                .into();
        }
    };
    let filter = format_ident!("{}", method.to_uppercase());
    let marker = format_ident!("__path_{}", function.sig.ident);
    let vis = &function.vis;
    // The bodies the annotation names stay in use, as with utoipa: no
    // unused imports or dead code that only show up without it.
    let mut bodies = Vec::new();
    body_types(args_tokens, &mut bodies);
    quote! {
        #handler
        const _: () = { #(let _ = ::core::marker::PhantomData::<#bodies>;)* };
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #vis struct #marker;
        impl #marker {
            pub const PATH: &'static str = #path;
            pub const METHOD: ::axum::routing::MethodFilter = ::axum::routing::MethodFilter::#filter;
        }
    }
    .into()
}

/// `async fn f(..) -> T { body }` as `fn f(..) -> Pin<Box<dyn Future<Output = T> + Send>>`.
/// Generic or non-async functions are left as they are.
fn boxed(mut function: ItemFn) -> ItemFn {
    if function.sig.asyncness.is_none() || !function.sig.generics.params.is_empty() {
        return function;
    }
    function.sig.asyncness = None;
    let output = match &function.sig.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, ty) => quote!(#ty),
    };
    function.sig.output = syn::parse_quote! {
        -> ::std::pin::Pin<::std::boxed::Box<
            dyn ::std::future::Future<Output = #output> + ::std::marker::Send + 'static
        >>
    };
    let body = &function.block;
    function.block = syn::parse_quote!({ ::std::boxed::Box::pin(async move #body) });
    function
}

/// The method ident and the `path = "..."` of `#[utoipa::path]` arguments.
fn method_and_path(args: TokenStream2) -> Result<(String, LitStr), &'static str> {
    let mut method = None;
    let mut path = None;
    for segment in split_commas(args) {
        match segment.as_slice() {
            [TokenTree::Ident(ident)] if METHODS.contains(&ident.to_string().as_str()) => {
                method = Some(ident.to_string());
            }
            [TokenTree::Ident(name), TokenTree::Punct(eq), value @ ..]
                if name == "path" && eq.as_char() == '=' =>
            {
                path = literal(value);
            }
            _ => {}
        }
    }
    match (method, path) {
        (Some(method), Some(path)) => Ok((method, path)),
        (None, _) => Err("expected an HTTP method (`get`, `post`, ...)"),
        (_, None) => Err("expected `path = \"...\"`"),
    }
}

/// The types after `body =`, `request_body =` and `content =` (also inside
/// `inline(...)`), at any depth of the arguments.
fn body_types(tokens: TokenStream2, types: &mut Vec<syn::Type>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (at, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) => body_types(group.stream(), types),
            TokenTree::Ident(key)
                if ["body", "request_body", "content"].contains(&key.to_string().as_str()) =>
            {
                let Some(TokenTree::Punct(eq)) = tokens.get(at + 1) else {
                    continue;
                };
                if eq.as_char() != '=' {
                    continue;
                }
                let mut depth = 0_i32;
                let value: TokenStream2 = tokens[at + 2..]
                    .iter()
                    .take_while(|token| match token {
                        TokenTree::Punct(p) if p.as_char() == '<' => {
                            depth += 1;
                            true
                        }
                        TokenTree::Punct(p) if p.as_char() == '>' => {
                            depth -= 1;
                            true
                        }
                        TokenTree::Punct(p) => p.as_char() != ',' || depth > 0,
                        _ => true,
                    })
                    .cloned()
                    .collect();
                if let Some(ty) = body_type(value) {
                    types.push(ty);
                }
            }
            _ => {}
        }
    }
}

fn body_type(value: TokenStream2) -> Option<syn::Type> {
    let tokens: Vec<TokenTree> = value.clone().into_iter().collect();
    if let [TokenTree::Ident(inline), TokenTree::Group(inner)] = tokens.as_slice()
        && inline == "inline"
    {
        return body_type(inner.stream());
    }
    // utoipa's own `Object` (any JSON object) is no Rust type.
    syn::parse2(value)
        .ok()
        .filter(|ty: &syn::Type| quote!(#ty).to_string() != "Object")
}

/// Top-level comma-separated segments (commas inside groups stay put).
fn split_commas(tokens: TokenStream2) -> Vec<Vec<TokenTree>> {
    let mut segments = vec![Vec::new()];
    for token in tokens {
        match &token {
            TokenTree::Punct(punct) if punct.as_char() == ',' => segments.push(Vec::new()),
            _ => segments.last_mut().expect("one segment").push(token),
        }
    }
    segments
}

/// A string literal, also one a `macro_rules!` passed in (an invisible group).
fn literal(tokens: &[TokenTree]) -> Option<LitStr> {
    match tokens {
        [TokenTree::Literal(_)] => syn::parse2(tokens.iter().cloned().collect()).ok(),
        [TokenTree::Group(group)] if group.delimiter() == Delimiter::None => {
            literal(&group.stream().into_iter().collect::<Vec<_>>())
        }
        _ => None,
    }
}

/// `routes!(a, b)`: one `(path, MethodRouter)` per handler, for
/// `OpenApiRouter::routes`.
#[proc_macro]
pub fn routes(input: TokenStream) -> TokenStream {
    let handlers =
        parse_macro_input!(input with Punctuated::<syn::Path, Token![,]>::parse_terminated);
    let pairs = handlers.iter().map(|handler| {
        let mut marker = handler.clone();
        let last = marker.segments.last_mut().expect("a handler name");
        last.ident = format_ident!("__path_{}", last.ident);
        quote!((#marker::PATH, ::axum::routing::on(#marker::METHOD, #handler)))
    });
    quote!([#(#pairs),*]).into()
}

#[proc_macro_derive(ToSchema, attributes(schema))]
pub fn to_schema(_: TokenStream) -> TokenStream {
    TokenStream::new()
}

#[proc_macro_derive(IntoParams, attributes(into_params, param))]
pub fn into_params(_: TokenStream) -> TokenStream {
    TokenStream::new()
}
