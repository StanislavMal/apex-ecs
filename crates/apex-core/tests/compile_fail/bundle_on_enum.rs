//! `#[derive(Bundle)]` supports structs only. On an enum it must reject with
//! our own diagnostic, not with a downstream `Bundle`-trait error.
#![allow(dead_code)]

use apex_core::prelude::Bundle;

#[derive(Bundle)]
enum NotAStruct {
    A,
    B,
}

fn main() {}
