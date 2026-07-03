//! Regression tests for `#[derive(Scriptable)]` codegen (apex-macros).
//!
//! Covers two bugs fixed in the derive:
//!   * F7a — multi-field tuple structs must compile and roundtrip (previously the
//!     generated code used `syn::Index` locals, producing `let 0: T = …` /
//!     `Self(0, 1)`, which does not compile).
//!   * F7b — C-like enums with explicit discriminants must roundtrip against the
//!     REAL discriminant (previously to_lua emitted the discriminant while from_lua
//!     matched the ordinal index, breaking `enum E { A = 10 }`).

use apex_scripting::{Scriptable, ScriptableRegistrar};

// F7a: multi-field tuple struct.
#[derive(Debug, Clone, Copy, PartialEq, Scriptable)]
struct V(f32, f32);

// F7b: C-like enum with explicit, non-ordinal discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Scriptable)]
#[repr(i64)]
enum E {
    A = 10,
    B = 20,
}

#[test]
fn tuple_struct_multi_field_roundtrips() {
    let lua = mlua::Lua::new();

    // field_names() are the positional string keys "0","1" (consistent with the
    // table keys used in to_lua / from_lua).
    assert_eq!(V::field_names(), &["0", "1"]);
    assert_eq!(V::type_name_str(), "V");

    let original = V(1.5, -2.25);
    let value = original.to_lua(&lua).expect("to_lua");
    let back = V::from_lua(&value).expect("from_lua");
    assert_eq!(back, original);

    // register_lua_type installs a `V.new(a, b)` constructor whose table roundtrips.
    V::register_lua_type(&lua).expect("register_lua_type");
    let constructed: mlua::Value = lua
        .load("return V.new(3.0, 4.0)")
        .eval()
        .expect("V.new eval");
    assert_eq!(V::from_lua(&constructed), Some(V(3.0, 4.0)));
}

#[test]
fn c_enum_explicit_discriminant_roundtrips() {
    let lua = mlua::Lua::new();

    // to_lua emits the real discriminant, not the ordinal index.
    let a = E::A.to_lua(&lua).expect("to_lua A");
    assert_eq!(a.as_i64(), Some(10));
    let b = E::B.to_lua(&lua).expect("to_lua B");
    assert_eq!(b.as_i64(), Some(20));

    // from_lua matches the real discriminant → full roundtrip.
    assert_eq!(E::from_lua(&a), Some(E::A));
    assert_eq!(E::from_lua(&b), Some(E::B));

    // Unknown discriminant (e.g. ordinal 0, which no longer maps to A) → None.
    let zero = mlua::Value::Integer(0);
    assert_eq!(E::from_lua(&zero), None);

    // Lua namespace constants carry the real discriminants: E.A == 10, E.B == 20.
    E::register_lua_type(&lua).expect("register_lua_type");
    let ea: i64 = lua.load("return E.A").eval().expect("E.A");
    let eb: i64 = lua.load("return E.B").eval().expect("E.B");
    assert_eq!(ea, 10);
    assert_eq!(eb, 20);
}
