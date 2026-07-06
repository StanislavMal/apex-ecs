//! `#[derive(Scriptable)]` does not support unions.
#![allow(dead_code)]

use apex_scripting::Scriptable;

#[derive(Scriptable)]
union U {
    a: u32,
    b: f32,
}

fn main() {}
