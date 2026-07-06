//! `#[derive(Bundle)]` supports structs only. On a union it must reject with
//! our own diagnostic (the `_ => …` arm covers both enums and unions).
#![allow(dead_code)]

use apex_core::prelude::Bundle;

#[derive(Bundle)]
union NotAStruct {
    a: u32,
    b: f32,
}

fn main() {}
