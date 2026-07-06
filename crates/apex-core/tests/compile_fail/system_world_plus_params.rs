//! U.1: `world: &mut World` is an exclusive system (FULL access) and cannot be
//! combined with other parameters. `system!` rejects the mix loudly.

apex_core::system! {
    fn bad(world: &mut World, extra: u32) {}
}

fn main() {}
