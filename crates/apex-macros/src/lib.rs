//! apex-macros — процедурные макросы для Apex ECS.
//!
//! # `#[derive(Component)]`
//!
//! Добавляет статический регистратор через `linkme::distributed_slice`,
//! вызываемый при создании `World::new()`. Трейт `Component` реализуется
//! автоматически (blanket impl).
//!
//! ```ignore
//! #[derive(Component, Debug, Clone)]
//! struct Position { x: f32, y: f32 }
//! ```
//!
//! # `#[derive(Bundle)]`
//!
//! Реализует трейт `Bundle` для struct с именованными полями.
//! Поддерживает произвольное число полей (без ограничения в 8 компонентов).
//!
//! ```ignore
//! #[derive(Bundle)]
//! struct PlayerBundle {
//!     pos: Position,
//!     vel: Velocity,
//!     hp: Health,
//!     team: Team,
//!     // ... до 16+ полей
//! }
//! ```
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
//! - `ScriptableRegistrar::to_dynamic(&self)` — struct → rhai::Map
//! - `ScriptableRegistrar::from_dynamic(d)` — rhai::Map → struct (Option)
//! - `ScriptableRegistrar::register_rhai_type(engine)` — конструктор `Position(x, y)` в Rhai
//! - `ScriptableRegistrar::field_names()` — список имён полей
//! - `ScriptableRegistrar::type_name_str()` — имя типа как &'static str

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields, Type};

// ── Component derive ────────────────────────────────────────────

/// `#[derive(Component)]` — авторегистрация через `linkme::distributed_slice`.
///
/// Генерирует статический регистратор в `COMPONENT_REGISTRARS`, вызываемый
/// при `World::new()`. Трейт `Component` реализуется автоматически через
/// blanket impl для `T: Send + Sync + 'static`.
#[proc_macro_derive(Component)]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let registrar_ident = quote::format_ident!("__COMPONENT_REGISTRAR_{}", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        #[::apex_core::linkme::distributed_slice(::apex_core::component::COMPONENT_REGISTRARS)]
        #[linkme(crate = ::apex_core::linkme)]
        static #registrar_ident: ::apex_core::component::ComponentRegistrarFn =
            |registry: &mut ::apex_core::component::ComponentRegistry| {
                registry.get_or_register::<#name>();
            };
    };
    expanded.into()
}

// ── Bundle derive ───────────────────────────────────────────────

/// `#[derive(Bundle)]` — реализует трейт `Bundle` для struct.
///
/// Поддерживает struct с именованными полями и произвольное число компонентов
/// (в отличие от `impl_bundle!`, который ограничен 8).
#[proc_macro_derive(Bundle)]
pub fn derive_bundle(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields: Vec<&syn::Field> = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => n.named.iter().collect(),
            Fields::Unnamed(u) => u.unnamed.iter().collect(),
            Fields::Unit => vec![],
        },
        _ => {
            return syn::Error::new_spanned(
                name,
                "#[derive(Bundle)] supports only structs",
            )
            .to_compile_error()
            .into();
        }
    };

    let field_count = fields.len();

    let field_accessors: Vec<TokenStream2> = fields.iter().enumerate().map(|(i, f)| {
        if let Some(ident) = &f.ident {
            quote! { self.#ident }
        } else {
            let idx = syn::Index::from(i);
            quote! { self.#idx }
        }
    }).collect();

    let field_types: Vec<&syn::Type> = fields.iter().map(|f| &f.ty).collect();

    let expanded = quote! {
        impl ::apex_core::Bundle for #name {
            fn component_ids(
                &self,
                registry: &mut ::apex_core::ComponentRegistry,
            ) -> ::smallvec::SmallVec<[::apex_core::ComponentId; 8]> {
                let mut ids: ::smallvec::SmallVec<[::apex_core::ComponentId; 8]> =
                    ::smallvec::smallvec![
                        #( registry.get_or_register::<#field_types>() ),*
                    ];
                ids.sort_unstable();
                ids
            }

            fn write_into(
                self,
                world: &mut ::apex_core::World,
                archetype_id: ::apex_core::ArchetypeId,
                row: usize,
                tick: ::apex_core::Tick,
            ) {
                let arch_idx = archetype_id.as_usize();
                let arch = &mut world.archetypes[arch_idx];
                let mut col_idx = 0usize;
                #(
                    {
                        let cid = world.registry_mut().get_or_register::<#field_types>();
                        if let Some(ci) = arch.column_index(cid) {
                            unsafe {
                                arch.columns[ci].write_typed_at(
                                    #field_accessors,
                                    row + col_idx,
                                    tick,
                                );
                            }
                        }
                        col_idx += 1;
                    }
                )*
            }

            fn needs_drop() -> bool {
                false #( || ::std::mem::needs_drop::<#field_types>() )*
            }
        }
    };
    expanded.into()
}

// ── Scriptable derive ───────────────────────────────────────────

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
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => expand_named_struct(ident, &type_name, &f.named),
            Fields::Unnamed(f) => expand_tuple_struct(ident, &type_name, &f.unnamed),
            Fields::Unit => Err(syn::Error::new_spanned(
                ident,
                "#[derive(Scriptable)] не поддерживает struct без полей",
            )),
        },

        Data::Enum(e) => {
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