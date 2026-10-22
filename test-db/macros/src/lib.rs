//! `#[test_db::test]`: an async test on its own copy of a cached template database.
use proc_macro::TokenStream;
use quote::quote;
use syn::{Ident, ItemFn, parse_macro_input};

/// Runs an async test against a fresh database copied from a template.
///
/// `#[test_db::test]` gives a database with every migration applied;
/// `#[test_db::test(seed)]` also loads `api/seed/local_seed.sql`. The test
/// function takes `pool: sqlx::MySqlPool` exactly like with `#[sqlx::test]`;
/// see `test_db::run`.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    let template = if args.is_empty() {
        quote!(::test_db::Template::Migrated)
    } else {
        let arg = parse_macro_input!(args as Ident);
        if arg != "seed" {
            return syn::Error::new(arg.span(), "expected `seed` or no argument")
                .to_compile_error()
                .into();
        }
        quote!(::test_db::Template::Seeded)
    };

    let function = parse_macro_input!(item as ItemFn);
    let Some(syn::FnArg::Typed(pool)) = function.sig.inputs.first() else {
        return syn::Error::new_spanned(&function.sig, "the test must take `pool: MySqlPool`")
            .to_compile_error()
            .into();
    };
    let attrs = &function.attrs;
    let vis = &function.vis;
    let name = &function.sig.ident;
    let body = &function.block;

    quote!(
        #(#attrs)*
        #[::core::prelude::v1::test]
        #vis fn #name() {
            ::test_db::run(#template, |#pool| async move #body);
        }
    )
    .into()
}
