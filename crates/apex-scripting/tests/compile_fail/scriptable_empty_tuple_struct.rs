//! `#[derive(Scriptable)]` rejects a field-less tuple struct: there is nothing
//! to serialise into a Lua table. (A unit struct `struct Marker;` is fine — it
//! maps to `true` — so the trap is specifically the `()` form.)
#![allow(dead_code)]

use apex_scripting::Scriptable;

#[derive(Scriptable)]
struct Gravity();

fn main() {}
