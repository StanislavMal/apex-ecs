//! apex-macros — procedural macros for Apex ECS.
//!
//! # `#[derive(Component)]`
//!
//! Implements the `Component` trait and adds a static registrar via
//! `linkme::distributed_slice`, invoked when `World::new()` is created.
//!
//! ```ignore
//! #[derive(Component, Debug, Clone)]
//! struct Position { x: f32, y: f32 }
//! ```
//!
//! # `#[derive(Bundle)]`
//!
//! Implements the `Bundle` trait for a struct with named fields.
//! Supports an arbitrary number of fields (no 8-component limit)
//! and nested Bundles (sub-Bundle fields are expanded recursively).
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
//!     base: PlayerBase,   // <— nested Bundle
//!     weapon: Weapon,
//!     armor: Armor,
//! }
//!
//! // Bundle tuples work too:
//! world.spawn((PlayerBase { pos, hp }, Weapon { .. }));
//! ```
//!
//! # `#[derive(Scriptable)]`
//!
//! Generates a `ScriptableRegistrar` trait implementation for a struct with named fields.
//!
//! ```ignore
//! #[derive(Clone, Copy, Scriptable)]
//! struct Position { x: f32, y: f32 }
//! ```
//!
//! - `ScriptableRegistrar::to_lua(&self, lua)` — struct → mlua::Table
//! - `ScriptableRegistrar::from_lua(val)` — mlua::Table → struct (Option)
//! - `ScriptableRegistrar::register_lua_type(lua)` — the `Position.new(x, y)` constructor in Lua
//! - `ScriptableRegistrar::field_names()` — the list of field names
//! - `ScriptableRegistrar::type_name_str()` — the type name as &'static str
//!
//! Also generates `IntoLua` and `FromLua` for the type,
//! so that the component's fields can be used with `table.set()` directly.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields, Type};

// ── Component derive ────────────────────────────────────────────

/// `#[derive(Component)]` — auto-registration via `linkme::distributed_slice`
/// and the `Component` trait implementation.
///
/// Generates:
/// - `impl Component for Type {}` (a trait with `Send + Sync + 'static` bounds)
/// - a static registrar in `COMPONENT_REGISTRARS`, invoked on `World::new()`
///
/// # `#[require(A, B, …)]` — required components (D2-4, analogous to Bevy 0.15+)
///
/// ```ignore
/// #[derive(Component)]
/// #[require(LocalTransform, GlobalTransform)]
/// struct MeshRenderer { /* … */ }
///
/// // the spawn pulls in the missing transforms with defaults on its own:
/// world.spawn((MeshRenderer::new(mesh, mat),));
/// ```
///
/// Required types must implement `Default`; an explicitly provided value
/// always wins over the default; requirements are transitive.
#[proc_macro_derive(Component, attributes(require))]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let registrar_ident = quote::format_ident!("__COMPONENT_REGISTRAR_{}", name);
    // F7: support generic components. `split_for_impl` yields the `<..>` for the
    // impl header, the type args, and the `where` clause.
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let is_generic = !input.generics.params.is_empty();

    // #[require(A, B, …)] — a comma-separated list of types (there may be several attributes).
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

    // Body of `Component::register_requires` — one `register_required::<Self, R>()` per `#[require]`.
    // Emit the method ONLY when there are requirements (otherwise the trait default applies = no requirements).
    let register_requires_impl = if required.is_empty() {
        quote! {}
    } else {
        let calls = required.iter().map(|ty| {
            quote! { registry.register_required::<#name #ty_generics, #ty>(); }
        });
        quote! {
            fn register_requires(registry: &mut ::apex_core::component::ComponentRegistry) {
                #( #calls )*
            }
        }
    };

    // Auto-registration at `World::new()` startup via `linkme::distributed_slice` (the linker collects
    // registrars from all crates). On wasm32 and under Miri linkme is not emitted — but that is only an
    // OPTIMIZATION (pre-registration): the component AND its `#[require]` are still registered LAZILY
    // on first use (`register` → `Component::register_requires`), so `#[require]`
    // works there too. TD-25 (wasm); Miri — the linkme distributed-slice causes overflow, same path.
    //
    // F7: for a GENERIC component a static registrar is impossible (there is no
    // concrete type for `get_or_register`), so we omit it — a generic
    // type is registered LAZILY on first use of a concrete substitution.
    let registrar = if is_generic {
        quote! {}
    } else {
        quote! {
            #[allow(non_upper_case_globals)]
            #[cfg(all(not(target_arch = "wasm32"), not(miri)))]
            #[::apex_core::linkme::distributed_slice(::apex_core::component::COMPONENT_REGISTRARS)]
            #[linkme(crate = ::apex_core::linkme)]
            static #registrar_ident: ::apex_core::component::ComponentRegistrarFn =
                |registry: &mut ::apex_core::component::ComponentRegistry| {
                    registry.get_or_register::<#name>();
                };
        }
    };

    let expanded = quote! {
        impl #impl_generics ::apex_core::component::Component for #name #ty_generics #where_clause {
            #register_requires_impl
        }

        #registrar
    };
    expanded.into()
}

// ── Bundle derive ───────────────────────────────────────────────

/// `#[derive(Bundle)]` — implements the `Bundle` trait for a struct.
///
/// Supports a struct with named fields and an arbitrary number of components
/// (unlike `impl_bundle!`, which is limited to 12).
///
/// Fields may be:
/// - components (any type with `Component` — automatically a Bundle)
/// - other Bundle structs (nesting, recursive expansion)
/// - Bundle tuples
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

    // F7: support generic bundles.
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let expanded = quote! {
        impl #impl_generics ::apex_core::Bundle for #name #ty_generics #where_clause {
            fn component_count() -> usize {
                0usize #( + <#field_types as ::apex_core::Bundle>::component_count() )*
            }

            // FIELD order (= traversal order of write_into_batch / write_data_into_batch); NO sorting.
            // col_indices for batch spawn is built from here, otherwise a component is written into the wrong column (UB).
            // The sorted archetype key is produced by the trait's default `component_ids`. The static one (by
            // field TYPES) requires no bundle value (§10.10, removes the make_bundle-probe footgun).
            fn static_component_ids(
                registry: &mut ::apex_core::ComponentRegistry,
                out: &mut ::apex_core::smallvec::SmallVec<[::apex_core::ComponentId; 8]>,
            ) {
                #(
                    <#field_types as ::apex_core::Bundle>::static_component_ids(registry, out);
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
                        "#[derive(Scriptable)] for an enum supports only data-less variants (a C-like enum). For an enum with data, implement ScriptableRegistrar manually.",
                    ));
                }
            }
            expand_c_like_enum(ident, &type_name, &e.variants)
        }

        Data::Union(_) => Err(syn::Error::new_spanned(
            ident,
            "#[derive(Scriptable)] does not support union",
        )),
    }
}

/// A struct with named fields → mlua::Table
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

    // Parameters for the Lua constructor: (f0, f1, ...): (Type0, Type1, ...)
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

/// A tuple struct (for example `struct Gravity(f32)`) → scalar
fn expand_tuple_struct(
    ident: &syn::Ident,
    type_name: &str,
    unnamed_fields: &syn::punctuated::Punctuated<syn::Field, syn::token::Comma>,
) -> syn::Result<TokenStream2> {
    let field_count = unnamed_fields.len();

    if field_count == 0 {
        return Err(syn::Error::new_spanned(
            ident,
            "#[derive(Scriptable)] does not support a tuple struct without fields",
        ));
    }

    let field_types: Vec<&Type> = unnamed_fields.iter().map(|f| &f.ty).collect();

    if field_count == 1 {
        // A single field → table { _value = ... } (consistent with the constructor)
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
        // Multiple fields → table with positional string keys "0","1",…
        // IMPORTANT: generate local variables via `f{i}` (f0, f1, …), NOT via
        // `syn::Index` — otherwise `let 0: T = …` produces a literal refutable pattern (E0005),
        // and `Self(0, 1)` constructs from LITERALS instead of variables.
        let local_idents: Vec<syn::Ident> = (0..field_count)
            .map(|i| syn::Ident::new(&format!("f{}", i), proc_macro2::Span::call_site()))
            .collect();

        // Field string keys — the same "0","1",… as in field_names() (consistency).
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

/// A unit struct (marker) → boolean true + registration as an empty table
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

/// A C-like enum → a namespace table in Lua
///
/// ```lua
/// -- Generated: TileKind = { Floor = 0, Wall = 1, Water = 2 }
/// TileKind.Floor  -- the number 0
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
