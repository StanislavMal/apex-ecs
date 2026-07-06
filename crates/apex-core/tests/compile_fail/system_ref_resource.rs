//! Migrant trap P2: bare `&T` no longer denotes a resource (in Bevy `&T` is a
//! query component). `system!` must reject it and point at `Res<T>`.

apex_core::system! {
    fn takes_ref(res: &u32) {}
}

fn main() {}
