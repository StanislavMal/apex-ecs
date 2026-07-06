//! An unrecognised parameter shape (here `mut weird`, which is not
//! `name: Type`) hits the catch-all, which lists every valid parameter form.

apex_core::system! {
    fn bad(mut weird: u32) {}
}

fn main() {}
