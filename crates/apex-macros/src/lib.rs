//! apex-macros — процедурные макросы для Apex ECS.
//!
//! # `#[derive(Scriptable)]`
//!
//! Генерирует реализацию трейта `ScriptableRegistrar` для struct с именованными полями.
//!
//! ```ignore
//! #[derive(Clone, Copy, Scriptable)]
//! struct Position { x: f32, y: f32 }
//! ```
//!
//! Генерируется:
//! - `ScriptableRegistrar::to_dynamic(&self)` — struct → rhai::Map
//! - `ScriptableRegistrar::from_dynamic(d)` — rhai::Map → struct (Option)
//! - `ScriptableRegistrar::register_rhai_type(engine)` — конструктор `Position(x, y)` в Rhai
//! - `ScriptableRegistrar::field_names()` — список имён полей
//! - `ScriptableRegistrar::type_name_str()` — имя типа как &'static str

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields, Type};

#[proc_macro_derive(Scriptable)]
pub fn derive_scriptable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_scriptable(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_scriptable(input: DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let type_name = ident.to_string();

    let named_fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => &f.named,
            _ => return Err(syn::Error::new_spanned(
                ident,
                "#[derive(Scriptable)] поддерживает только struct с именованными полями",
            )),
        },
        _ => return Err(syn::Error::new_spanned(
            ident,
            "#[derive(Scriptable)] поддерживает только struct",
        )),
    };

    let field_idents: Vec<&syn::Ident> = named_fields.iter()
        .map(|f| f.ident.as_ref().unwrap())
        .collect();

    let field_names: Vec<String> = field_idents.iter()
        .map(|i| i.to_string())
        .collect();

    let field_types: Vec<&Type> = named_fields.iter()
        .map(|f| &f.ty)
        .collect();

    let n_fields = field_idents.len();

    // to_dynamic() — собираем Map
    let to_dynamic_stmts = field_idents.iter().zip(field_names.iter()).map(|(fi, fn_)| {
        quote! {
            map.insert(
                #fn_.into(),
                <_ as apex_scripting::ScriptableField>::to_dynamic(&self.#fi),
            );
        }
    });

    // from_dynamic() — извлекаем из Map
    let from_dynamic_stmts = field_idents.iter()
        .zip(field_names.iter())
        .zip(field_types.iter())
        .map(|((fi, fn_), ft)| {
            quote! {
                let #fi: #ft = {
                    let v = map.get(#fn_)?;
                    <#ft as apex_scripting::ScriptableField>::from_dynamic(v)?
                };
            }
        });

    let struct_fields   = field_idents.iter().map(|fi| quote! { #fi });
    let field_names_arr = field_names.iter().map(|n| quote! { #n });

    // register_rhai_type() — конструктор через raw_fn (N динамических аргументов)
    let reg_inserts = field_names.iter().enumerate().map(|(i, fn_)| {
        quote! {
            map.insert(#fn_.into(), args[#i].clone());
        }
    });

    let expanded = quote! {
        impl apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str {
                #type_name
            }

            fn field_names() -> &'static [&'static str] {
                &[#(#field_names_arr),*]
            }

            fn to_dynamic(&self) -> rhai::Dynamic {
                let mut map = rhai::Map::new();
                #(#to_dynamic_stmts)*
                rhai::Dynamic::from_map(map)
            }

            fn from_dynamic(d: &rhai::Dynamic) -> ::std::option::Option<Self> {
                let lock = d.read_lock::<rhai::Map>()?;
                let map: &rhai::Map = &*lock;
                #(#from_dynamic_stmts)*
                ::std::option::Option::Some(Self { #(#struct_fields),* })
            }

            fn register_rhai_type(engine: &mut rhai::Engine) {
                let type_ids: ::std::vec::Vec<::std::any::TypeId> =
                    ::std::iter::repeat(::std::any::TypeId::of::<rhai::Dynamic>())
                        .take(#n_fields)
                        .collect();

                engine.register_raw_fn(
                    #type_name,
                    &type_ids,
                    |_ctx: rhai::NativeCallContext, args: &mut [&mut rhai::Dynamic]|
                        -> ::std::result::Result<rhai::Dynamic, ::std::boxed::Box<rhai::EvalAltResult>>
                    {
                        let mut map = rhai::Map::new();
                        #(#reg_inserts)*
                        ::std::result::Result::Ok(rhai::Dynamic::from_map(map))
                    },
                );
            }
        }
    };

    Ok(expanded)
}