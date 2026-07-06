//! Migrant trap P2: bare `&mut T` no longer denotes a resource. `system!` must
//! reject it and point at `ResMut<T>` (or `&mut Vec<E>` for an event writer).

apex_core::system! {
    fn takes_ref_mut(res: &mut u32) {}
}

fn main() {}
