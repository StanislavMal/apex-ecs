//! `#[derive(Scriptable)]` on an enum supports only data-less (C-like) variants.
//! A variant carrying data must be rejected, pointing at the manual impl.
#![allow(dead_code)]

use apex_scripting::Scriptable;

#[derive(Scriptable)]
enum WithData {
    Unit,
    Tuple(u32),
}

fn main() {}
