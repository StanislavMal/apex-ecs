//! apex-macros — процедурные макросы для Apex ECS.
//!
//! # `#[derive(Component)]`
//!
//! Реализует трейт `Component` и добавляет статический регистратор через
//! `linkme::distributed_slice`, вызываемый при создании `World::new()`.
//!
//! ```ignore
//! #[derive(Component, Debug, Clone)]
//! struct Position { x: f32, y: f32 }
//! ```
//!
//! # `#[derive(Bundle)]`
//!
//! Реализует трейт `Bundle` для struct с именованными полями.
//! Поддерживает произвольное число полей (без ограничения в 8 компонентов)
//! и вложенные Bundle (поля-под-Bundle рекурсивно разворачиваются).
//!
//! ```ignore
//! #[derive(Bundle)]
//! struct PlayerBase {
//!     pos: Position,
//!     hp: Health,
//! }
//!
//! #[derive(Bundle)]
//! struct ArmedPlayer {
//!     base: PlayerBase,   // <— вложенный Bundle
//!     weapon: Weapon,
//!     armor: Armor,
//! }
//!
//! // Кортежи Bundle тоже работают:
//! world.spawn((PlayerBase { pos, hp }, Weapon { .. }));
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
//! - `ScriptableRegistrar::to_lua(&self, lua)` — struct → mlua::Table
//! - `ScriptableRegistrar::from_lua(val)` — mlua::Table → struct (Option)
//! - `ScriptableRegistrar::register_lua_type(lua)` — конструктор `Position.new(x, y)` в Lua
//! - `ScriptableRegistrar::field_names()` — список имён полей
//! - `ScriptableRegistrar::type_name_str()` — имя типа как &'static str
//!
//! Также генерирует `IntoLua` и `FromLua` для типа,
//! чтобы поля компонента могли использоваться с `table.set()` напрямую.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields, Type};

// ── Component derive ────────────────────────────────────────────

/// `#[derive(Component)]` — авторегистрация через `linkme::distributed_slice`
/// и реализация трейта `Component`.
///
/// Генерирует:
/// - `impl Component for Type {}` (трейт с границами `Send + Sync + 'static`)
/// - статический регистратор в `COMPONENT_REGISTRARS`, вызываемый при `World::new()`
///
/// # `#[require(A, B, …)]` — required components (D2-4, аналог Bevy 0.15+)
///
/// ```ignore
/// #[derive(Component)]
/// #[require(LocalTransform, GlobalTransform)]
/// struct MeshRenderer { /* … */ }
///
/// // спавн сам дотягивает недостающие трансформы дефолтами:
/// world.spawn((MeshRenderer::new(mesh, mat),));
/// ```
///
/// Требуемые типы обязаны реализовывать `Default`; явно заданное значение
/// всегда выигрывает у дефолта; требования транзитивны.
#[proc_macro_derive(Component, attributes(require))]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let registrar_ident = quote::format_ident!("__COMPONENT_REGISTRAR_{}", name);

    // #[require(A, B, …)] — список типов через запятую (атрибутов может быть несколько).
    let mut required: Vec<Type> = Vec::new();
    for attr in &input.attrs {
        if attr.path().is_ident("require") {
            match attr.parse_args_with(
                syn::punctuated::Punctuated::<Type, syn::Token![,]>::parse_terminated,
            ) {
                Ok(types) => required.extend(types),
                Err(e) => return e.to_compile_error().into(),
            }
        }
    }

    // Тело `Component::register_requires` — по одному `register_required::<Self, R>()` на каждый `#[require]`.
    // Эмитим метод ТОЛЬКО когда требования есть (иначе работает дефолт трейта = нет требований).
    let register_requires_impl = if required.is_empty() {
        quote! {}
    } else {
        let calls = required.iter().map(|ty| {
            quote! { registry.register_required::<#name, #ty>(); }
        });
        quote! {
            fn register_requires(registry: &mut ::apex_core::component::ComponentRegistry) {
                #( #calls )*
            }
        }
    };

    let expanded = quote! {
        impl ::apex_core::component::Component for #name {
            #register_requires_impl
        }

        // Авторегистрация на старте `World::new()` через `linkme::distributed_slice` (линкер собирает
        // регистраторы из всех крейтов). На wasm32 linkme не эмитится — но это лишь ОПТИМИЗАЦИЯ
        // (пре-регистрация): компонент И его `#[require]` всё равно регистрируются ЛЕНИВО при первом
        // использовании (`register` → `Component::register_requires`), поэтому `#[require]` работает и
        // на wasm. TD-25.
        #[allow(non_upper_case_globals)]
        #[cfg(not(target_arch = "wasm32"))]
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
/// (в отличие от `impl_bundle!`, который ограничен 12).
///
/// Поля могут быть:
/// - компонентами (любой тип с `Component` — автоматически является Bundle)
/// - другими Bundle-структурами (вложенность, рекурсивное разворачивание)
/// - кортежами Bundle
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
            fn component_count() -> usize {
                0usize #( + <#field_types as ::apex_core::Bundle>::component_count() )*
            }

            fn component_ids(
                &self,
                registry: &mut ::apex_core::ComponentRegistry,
            ) -> ::apex_core::smallvec::SmallVec<[::apex_core::ComponentId; 8]> {
                let mut ids: ::apex_core::smallvec::SmallVec<[::apex_core::ComponentId; 8]> =
                    ::apex_core::smallvec::SmallVec::new();
                #(
                    ::apex_core::Bundle::push_component_ids(&#field_accessors, registry, &mut ids);
                )*
                ids.sort_unstable();
                ids
            }

            // Порядок ПОЛЕЙ (= порядок обхода write_into_batch / write_data_into_batch); БЕЗ сортировки.
            // col_indices для batch-спавна строится отсюда, иначе компонент пишется в чужую колонку (UB).
            fn push_component_ids(
                &self,
                registry: &mut ::apex_core::ComponentRegistry,
                out: &mut ::apex_core::smallvec::SmallVec<[::apex_core::ComponentId; 8]>,
            ) {
                #(
                    ::apex_core::Bundle::push_component_ids(&#field_accessors, registry, out);
                )*
            }

            fn write_into(
                self,
                world: &mut ::apex_core::World,
                archetype_id: ::apex_core::ArchetypeId,
                row: usize,
                tick: ::apex_core::Tick,
            ) {
                #(
                    ::apex_core::Bundle::write_into(#field_accessors, world, archetype_id, row, tick);
                )*
            }

            fn write_data_into_batch(
                self,
                world: &mut ::apex_core::World,
                archetype_id: ::apex_core::ArchetypeId,
                row: usize,
                tick: ::apex_core::Tick,
                col_indices: &[usize],
            ) {
                let mut _offset = 0usize;
                #(
                    let _cnt = <#field_types as ::apex_core::Bundle>::component_count();
                    ::apex_core::Bundle::write_data_into_batch(
                        #field_accessors, world, archetype_id, row, tick, &col_indices[_offset.._offset + _cnt]
                    );
                    _offset += _cnt;
                )*
            }

            fn needs_drop() -> bool {
                false #( || <#field_types as ::apex_core::Bundle>::needs_drop() )*
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
            Fields::Unit => expand_unit_struct(ident, &type_name),
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

/// Struct с именованными полями → mlua::Table
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

    let to_lua_stmts = field_idents.iter().zip(field_names.iter()).map(|(fi, fn_)| {
        quote! {
            t.set(#fn_, self.#fi.clone())?;
        }
    });

    let from_lua_stmts = field_idents.iter()
        .zip(field_names.iter())
        .zip(field_types.iter())
        .map(|((fi, fn_), ft)| {
            quote! {
                let #fi: #ft = t.get(#fn_).ok()?;
            }
        });

    let struct_fields = field_idents.iter().map(|fi| quote! { #fi });
    let field_names_arr = field_names.iter().map(|n| quote! { #n });

    // Параметры для конструктора Lua: (f0, f1, ...): (Type0, Type1, ...)
    let reg_arg_names: Vec<syn::Ident> = (0..n_fields)
        .map(|i| syn::Ident::new(&format!("f{}", i), proc_macro2::Span::call_site()))
        .collect();
    let reg_inserts = reg_arg_names.iter().zip(field_names.iter()).map(|(a, fn_)| {
        quote! {
            t.set(#fn_, #a)?;
        }
    });

    Ok(quote! {
        impl ::apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str { #type_name }
            fn field_names() -> &'static [&'static str] { &[#(#field_names_arr),*] }

            fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                let t = lua.create_table()?;
                #(#to_lua_stmts)*
                Ok(mlua::Value::Table(t))
            }

            fn from_lua(val: &mlua::Value) -> ::std::option::Option<Self> {
                let t = val.as_table()?;
                #(#from_lua_stmts)*
                ::std::option::Option::Some(Self { #(#struct_fields),* })
            }

            fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
                let t = lua.create_table()?;
                t.set("new", lua.create_function(|lua, (#(#reg_arg_names),*): (#(#field_types),*)| {
                    let t = lua.create_table()?;
                    #(#reg_inserts)*
                    Ok(t)
                })?)?;
                lua.globals().set(#type_name, t)
            }
        }

        impl mlua::IntoLua for #ident {
            fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                <Self as ::apex_scripting::ScriptableRegistrar>::to_lua(&self, lua)
            }
        }

        impl mlua::FromLua for #ident {
            fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
                <Self as ::apex_scripting::ScriptableRegistrar>::from_lua(&value)
                    .ok_or_else(|| mlua::Error::runtime(
                        ::std::format!("cannot convert Lua value to {}", #type_name)
                    ))
            }
        }
    })
}

/// Tuple struct (например `struct Gravity(f32)`) → scalar
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
        // Одиночное поле → таблица { _value = ... } (консистентно с конструктором)
        let ft = &field_types[0];
        Ok(quote! {
            impl ::apex_scripting::ScriptableRegistrar for #ident {
                fn type_name_str() -> &'static str { #type_name }
                fn field_names() -> &'static [&'static str] { &["0"] }

                fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                    let t = lua.create_table()?;
                    t.set("_value", self.0.clone())?;
                    Ok(mlua::Value::Table(t))
                }

                fn from_lua(val: &mlua::Value) -> ::std::option::Option<Self> {
                    let t = val.as_table()?;
                    let inner: #ft = t.get("_value").ok()?;
                    ::std::option::Option::Some(Self(inner))
                }

                fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
                    let t = lua.create_table()?;
                    t.set("new", lua.create_function(|lua, v: #ft| {
                        let t = lua.create_table()?;
                        t.set("_value", v)?;
                        Ok(t)
                    })?)?;
                    lua.globals().set(#type_name, t)
                }
            }

            impl mlua::IntoLua for #ident {
                fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                    <Self as ::apex_scripting::ScriptableRegistrar>::to_lua(&self, lua)
                }
            }

            impl mlua::FromLua for #ident {
                fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
                    <Self as ::apex_scripting::ScriptableRegistrar>::from_lua(&value)
                        .ok_or_else(|| mlua::Error::runtime(
                            ::std::format!("cannot convert Lua value to {}", #type_name)
                        ))
                }
            }
        })
    } else {
        // Несколько полей → таблица с позиционными строковыми ключами "0","1",…
        // ВАЖНО: генерировать локальные переменные через `f{i}` (f0, f1, …), а НЕ через
        // `syn::Index` — иначе `let 0: T = …` даёт литеральный refutable-паттерн (E0005),
        // а `Self(0, 1)` конструирует из ЛИТЕРАЛОВ вместо переменных.
        let local_idents: Vec<syn::Ident> = (0..field_count)
            .map(|i| syn::Ident::new(&format!("f{}", i), proc_macro2::Span::call_site()))
            .collect();

        // Строковые ключи полей — те же "0","1",… что и в field_names() (согласованность).
        let field_keys: Vec<String> = (0..field_count).map(|i| i.to_string()).collect();

        let to_lua_stmts = (0..field_count).zip(field_keys.iter()).map(|(i, key)| {
            let fi = syn::Index::from(i);
            quote! {
                t.set(#key, self.#fi.clone())?;
            }
        });

        let from_lua_stmts = local_idents.iter()
            .zip(field_types.iter())
            .zip(field_keys.iter())
            .map(|((local, ft), key)| {
                quote! {
                    let #local: #ft = t.get(#key).ok()?;
                }
            });

        let struct_fields = local_idents.iter().map(|local| quote! { #local });

        let field_names_arr = field_keys.iter().map(|n| quote! { #n });

        let reg_arg_names = &local_idents;
        let reg_inserts = reg_arg_names.iter().zip(field_keys.iter()).map(|(a, key)| {
            quote! {
                t.set(#key, #a)?;
            }
        });

        Ok(quote! {
            impl ::apex_scripting::ScriptableRegistrar for #ident {
                fn type_name_str() -> &'static str { #type_name }
                fn field_names() -> &'static [&'static str] { &[#(#field_names_arr),*] }

                fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                    let t = lua.create_table()?;
                    #(#to_lua_stmts)*
                    Ok(mlua::Value::Table(t))
                }

                fn from_lua(val: &mlua::Value) -> ::std::option::Option<Self> {
                    let t = val.as_table()?;
                    #(#from_lua_stmts)*
                    ::std::option::Option::Some(Self(#(#struct_fields),*))
                }

                fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
                    let t = lua.create_table()?;
                t.set("new", lua.create_function(|lua, (#(#reg_arg_names),*): (#(#field_types),*)| {
                        let t = lua.create_table()?;
                        #(#reg_inserts)*
                        Ok(t)
                    })?)?;
                    lua.globals().set(#type_name, t)
                }
            }

            impl mlua::IntoLua for #ident {
                fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                    <Self as ::apex_scripting::ScriptableRegistrar>::to_lua(&self, lua)
                }
            }

            impl mlua::FromLua for #ident {
                fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
                    <Self as ::apex_scripting::ScriptableRegistrar>::from_lua(&value)
                        .ok_or_else(|| mlua::Error::runtime(
                            ::std::format!("cannot convert Lua value to {}", #type_name)
                        ))
                }
            }
        })
    }
}

/// Unit struct (маркер) → boolean true + регистрация как empty table
fn expand_unit_struct(
    ident: &syn::Ident,
    type_name: &str,
) -> syn::Result<TokenStream2> {
    Ok(quote! {
        impl ::apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str { #type_name }
            fn field_names() -> &'static [&'static str] { &[] }

            fn to_lua(&self, _lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                Ok(mlua::Value::Boolean(true))
            }

            fn from_lua(_val: &mlua::Value) -> ::std::option::Option<Self> {
                ::std::option::Option::Some(Self)
            }

            fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
                let t = lua.create_table()?;
                lua.globals().set(#type_name, t)
            }
        }

        impl mlua::IntoLua for #ident {
            fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                <Self as ::apex_scripting::ScriptableRegistrar>::to_lua(&self, lua)
            }
        }

        impl mlua::FromLua for #ident {
            fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
                <Self as ::apex_scripting::ScriptableRegistrar>::from_lua(&value)
                    .ok_or_else(|| mlua::Error::runtime(
                        ::std::format!("cannot convert Lua value to {}", #type_name)
                    ))
            }
        }
    })
}

/// C-like enum → таблица-неймспейс в Lua
///
/// ```lua
/// -- Генерируется: TileKind = { Floor = 0, Wall = 1, Water = 2 }
/// TileKind.Floor  -- число 0
/// ```
fn expand_c_like_enum(
    ident: &syn::Ident,
    type_name: &str,
    variants: &syn::punctuated::Punctuated<syn::Variant, syn::token::Comma>,
) -> syn::Result<TokenStream2> {
    let variant_idents: Vec<&syn::Ident> = variants.iter().map(|v| &v.ident).collect();
    let variant_names: Vec<String> = variant_idents.iter().map(|v| v.to_string()).collect();

    // Match on the REAL discriminant (`Self::V as i64`), not the ordinal index —
    // otherwise a C-enum with explicit discriminants (`enum E { A = 10 }`) breaks
    // roundtrip: to_lua emits 10 but from_lua would only match ordinal 0.
    let from_lua_arms: Vec<TokenStream2> = variant_idents.iter().map(|v| {
        quote! { x if x == (#ident::#v as i64) => ::std::option::Option::Some(Self::#v) }
    }).collect();

    // Lua-namespace constants use the same real discriminants so `E.A == 10`.
    let reg_entries: Vec<TokenStream2> = variant_idents.iter().zip(variant_names.iter()).map(|(v, n)| {
        quote! {
            t.set(#n, #ident::#v as i64)?;
        }
    }).collect();

    Ok(quote! {
        impl ::apex_scripting::ScriptableRegistrar for #ident {
            fn type_name_str() -> &'static str { #type_name }

            fn field_names() -> &'static [&'static str] { &[] }

            fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                // Emit the real discriminant (matches from_lua and the Lua constants).
                Ok(mlua::Value::Integer((*self as i64) as mlua::Integer))
            }

            fn from_lua(val: &mlua::Value) -> ::std::option::Option<Self> {
                let v: i64 = val.as_i64()?;
                match v {
                    #(#from_lua_arms),*,
                    _ => ::std::option::Option::None,
                }
            }

            fn register_lua_type(lua: &mlua::Lua) -> mlua::Result<()> {
                let t = lua.create_table()?;
                #(#reg_entries)*
                lua.globals().set(#type_name, t)
            }
        }

        impl mlua::IntoLua for #ident {
            fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
                <Self as ::apex_scripting::ScriptableRegistrar>::to_lua(&self, lua)
            }
        }

        impl mlua::FromLua for #ident {
            fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
                <Self as ::apex_scripting::ScriptableRegistrar>::from_lua(&value)
                    .ok_or_else(|| mlua::Error::runtime(
                        ::std::format!("cannot convert Lua value to {}", #type_name)
                    ))
            }
        }
    })
}
