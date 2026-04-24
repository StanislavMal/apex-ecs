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

    match &input.data {
        // ── Struct с именованными полями ────────────────────────
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => expand_named_struct(ident, &type_name, &f.named),
            Fields::Unnamed(f) => expand_tuple_struct(ident, &type_name, &f.unnamed),
            Fields::Unit => Err(syn::Error::new_spanned(
                ident,
                "#[derive(Scriptable)] не поддерживает struct без полей",
            )),
        },

        // ── C-like enum (варианты без данных) ───────────────────
        Data::Enum(e) => {
            // Проверяем, что все варианты без полей
            for variant in &e.variants {
                if !variant.fields.is_empty() {
                    return Err(syn::Error::new_spanned(
                        &variant.ident,
                        "#[derive(Scriptable)] для enum поддерживает только варианты без данных (C-like enum). Для enum с данными реализуйте ScriptableRegistrar вручную.",
                    ));
                }
            }
            expand_c_like_enum(ident, &type_name, &e.variants)
        }

        Data::Union(_) => Err(syn::Error::new_spanned(
            ident,
            "#[derive(Scriptable)] не поддерживает union",
        )),
    }
}

/// Struct с именованными полями → rhai::Map
fn expand_named_struct(
    ident: &syn::Ident,
    type_name: &str,
    named_fields: &syn::punctuated::Punctuated<syn::Field, syn::token::Comma>,
) -> syn::Result<TokenStream2> {
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

    let to_dynamic_stmts = field_idents.iter().zip(field_names.iter()).map(|(fi, fn_)| {
        quote! {
            map.insert(
                #fn_.into(),
                <_ as apex_scripting::ScriptableField>::to_dynamic(&self.#fi),
            );
        }
    });

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

    // Параметры Dynamic для register_fn: arg_0: Dynamic, arg_1: Dynamic, ...
    let reg_arg_names: Vec<syn::Ident> = (0..n_fields)
        .map(|i| syn::Ident::new(&format!("arg_{}", i), proc_macro2::Span::call_site()))
        .collect();
    let reg_params = reg_arg_names.iter().map(|a| quote! { #a: rhai::Dynamic });
    let reg_inserts = reg_arg_names.iter().zip(field_names.iter()).map(|(a, fn_)| {
        quote! {
            map.insert(#fn_.into(), #a);
        }
    });

    Ok(quote! {
        impl apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str { #type_name }
            fn field_names() -> &'static [&'static str] { &[#(#field_names_arr),*] }

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
                engine.register_fn(#type_name, |#(#reg_params),*| -> rhai::Dynamic {
                    let mut map = rhai::Map::new();
                    #(#reg_inserts)*
                    rhai::Dynamic::from_map(map)
                });
            }
        }
    })
}

/// Tuple struct (например `struct Gravity(f32)`) → scalar или Array
fn expand_tuple_struct(
    ident: &syn::Ident,
    type_name: &str,
    unnamed_fields: &syn::punctuated::Punctuated<syn::Field, syn::token::Comma>,
) -> syn::Result<TokenStream2> {
    let field_count = unnamed_fields.len();

    if field_count == 0 {
        return Err(syn::Error::new_spanned(
            ident,
            "#[derive(Scriptable)] не поддерживает tuple struct без полей",
        ));
    }

    let field_types: Vec<&Type> = unnamed_fields.iter().map(|f| &f.ty).collect();

    if field_count == 1 {
        // Одиночное поле → скалярное значение (не Map)
        let ft = &field_types[0];
        Ok(quote! {
            impl apex_scripting::ScriptableRegistrar for #ident {
                fn type_name_str() -> &'static str { #type_name }
                fn field_names() -> &'static [&'static str] { &["0"] }

                fn to_dynamic(&self) -> rhai::Dynamic {
                    <#ft as apex_scripting::ScriptableField>::to_dynamic(&self.0)
                }

                fn from_dynamic(d: &rhai::Dynamic) -> ::std::option::Option<Self> {
                    let v = <#ft as apex_scripting::ScriptableField>::from_dynamic(d)?;
                    ::std::option::Option::Some(Self(v))
                }

                fn register_rhai_type(engine: &mut rhai::Engine) {
                    engine.register_fn(#type_name, |a: rhai::Dynamic| -> rhai::Dynamic { a });
                }
            }
        })
    } else {
        // Несколько полей → rhai::Array
        let to_dynamic_stmts = (0..field_count).map(|i| {
            let fi = syn::Index::from(i);
            let ft = field_types[i];
            quote! {
                arr.push(<#ft as apex_scripting::ScriptableField>::to_dynamic(&self.#fi));
            }
        });

        let from_dynamic_stmts = (0..field_count).map(|i| {
            let fi = syn::Index::from(i);
            let ft = field_types[i];
            quote! {
                let #fi: #ft = {
                    let v = arr.get(#i)?;
                    <#ft as apex_scripting::ScriptableField>::from_dynamic(v)?
                };
            }
        });

        let struct_fields = (0..field_count).map(|i| {
            let fi = syn::Index::from(i);
            quote! { #fi }
        });

        let reg_arg_names: Vec<syn::Ident> = (0..field_count)
            .map(|i| syn::Ident::new(&format!("arg_{}", i), proc_macro2::Span::call_site()))
            .collect();
        let reg_params = reg_arg_names.iter().map(|a| quote! { #a: rhai::Dynamic });
        let reg_inserts = reg_arg_names.iter().map(|a| {
            quote! {
                arr.push(#a);
            }
        });

        Ok(quote! {
            impl apex_scripting::ScriptableRegistrar for #ident {
                fn type_name_str() -> &'static str { #type_name }
                fn field_names() -> &'static [&'static str] { &[#(#field_types),*] }

                fn to_dynamic(&self) -> rhai::Dynamic {
                    let mut arr = rhai::Array::new();
                    #(#to_dynamic_stmts)*
                    rhai::Dynamic::from_array(arr)
                }

                fn from_dynamic(d: &rhai::Dynamic) -> ::std::option::Option<Self> {
                    let lock = d.read_lock::<rhai::Array>()?;
                    let arr: &rhai::Array = &*lock;
                    #(#from_dynamic_stmts)*
                    ::std::option::Option::Some(Self(#(#struct_fields),*))
                }

                fn register_rhai_type(engine: &mut rhai::Engine) {
                    engine.register_fn(#type_name, |#(#reg_params),*| -> rhai::Dynamic {
                        let mut arr = rhai::Array::new();
                        #(#reg_inserts)*
                        rhai::Dynamic::from_array(arr)
                    });
                }
            }
        })
    }
}

/// C-like enum → конвертация в i64
fn expand_c_like_enum(
    ident: &syn::Ident,
    type_name: &str,
    variants: &syn::punctuated::Punctuated<syn::Variant, syn::token::Comma>,
) -> syn::Result<TokenStream2> {
    let variant_idents: Vec<&syn::Ident> = variants.iter().map(|v| &v.ident).collect();

    // Собираем match-ветки вида: 0 => Some(Self::Floor), 1 => Some(Self::Wall), ...
    // Используем i64 значения, чтобы тип совпадал с d.as_int() -> Option<i64>
    let from_dynamic_arms: Vec<TokenStream2> = variant_idents.iter().enumerate().map(|(i, v)| {
        let vi = i as i64;
        quote! { #vi => ::std::option::Option::Some(Self::#v) }
    }).collect();

    // Регистрируем константные функции: TileKind_Floor, TileKind_Wall, ...
    let reg_fns: Vec<TokenStream2> = variant_idents.iter().enumerate().map(|(i, v)| {
        let vi = i as i64;
        let fn_name = format!("{}_{}", type_name, v.to_string());
        quote! {
            engine.register_fn(#fn_name, || -> rhai::Dynamic { rhai::Dynamic::from_int(#vi) });
        }
    }).collect();

    Ok(quote! {
        impl apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str { #type_name }

            fn field_names() -> &'static [&'static str] { &[] }

            fn to_dynamic(&self) -> rhai::Dynamic {
                rhai::Dynamic::from_int(*self as i64)
            }

            fn from_dynamic(d: &rhai::Dynamic) -> ::std::option::Option<Self> {
                let val: i64 = d.as_int().ok()?;
                match val {
                    #(#from_dynamic_arms),*,
                    _ => ::std::option::Option::None,
                }
            }

            fn register_rhai_type(engine: &mut rhai::Engine) {
                #(#reg_fns)*
            }
        }
    })
}