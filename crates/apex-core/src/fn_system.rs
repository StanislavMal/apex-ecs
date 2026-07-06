//! Plain-fn systems (D2-1): an ordinary function as a system — Bevy style 1:1.
//!
//! ```ignore
//! fn movement(time: Res<Time>, mut q: Query<(&Velocity, &mut Transform)>) {
//!     for (_, (vel, mut tf)) in q.iter() {
//!         tf.translation += vel.0 * time.dt;
//!     }
//! }
//!
//! sched.add_systems(StageLabel::Update, (movement, other_fn));
//! ```
//!
//! Parameters are user types with Bevy semantics (see the `SystemParam` impls
//! in [`system_param`](crate::system_param)): `Res<T>` / `ResMut<T>` /
//! `Query<Q>` / `CachedQuery<Q>` / `EventReader<E>` / `EventWriter<E>` /
//! `&mut Commands`. ⚠ Unlike `system!`, here `Res<T>` is a resource, and
//! `&T` components live INSIDE `Query<…>` — the macro's "`&T` = resource" trap
//! is absent on the plain-fn path.
//!
//! Access is inferred statically (merging the parameters' accesses), and the
//! system name comes from the function name. `system!` remains a dialect
//! (stateful systems are more convenient there with Bevy's `Local<T>`).
//!
//! The implementation is the classic Bevy "double Fn-bound" pattern: the
//! function must be callable both with the declared parameter types and with
//! their `Item<'w>` for any `'w` (for fn-items these are the same thanks to
//! elision's lifetime generalization).

use crate::system_param::SystemParam;

/// A function whose parameters are [`SystemParam`]. Implemented automatically
/// for `fn`s/closures with 1..=12 parameters; `Marker` is a disambiguator type
/// (`fn(P1, …)`) that lets one function type match exactly one impl.
pub trait SystemParamFunction<Marker>: Send + Sync + 'static {
    /// The parameter tuple (even for a single parameter — `(P1,)`).
    type Param: SystemParam;

    /// The receiver's lifetime is tied to the item's lifetime — this lets the
    /// HRTB bound `for<'w> &'w mut Func: FnMut(Item<'w>…)` resolve diagonally.
    fn run<'w>(&'w mut self, item: <Self::Param as SystemParam>::Item<'w>);
}

macro_rules! impl_system_param_function {
    ( $( ($P:ident, $p:ident) ),+ ) => {
        impl<Func, $($P),+> SystemParamFunction<fn($($P),+)> for Func
        where
            Func: Send + Sync + 'static,
            $( $P: SystemParam, )+
            for<'w> &'w mut Func:
                FnMut($($P),+) + FnMut($(<$P as SystemParam>::Item<'w>),+),
        {
            type Param = ($($P,)+);

            #[inline]
            fn run<'w>(&'w mut self, item: <Self::Param as SystemParam>::Item<'w>) {
                // Call through a monomorphized shim: it binds the HRTB bound
                // to this call's concrete Item types (Bevy pattern — the
                // compiler cannot resolve `(self)(…)` directly via HRTB).
                #[allow(non_snake_case, clippy::too_many_arguments)]
                fn call_inner<$($P),+>(
                    mut f: impl FnMut($($P),+),
                    $($p: $P),+
                ) {
                    f($($p),+)
                }
                let ($($p,)+) = item;
                call_inner(self, $($p),+)
            }
        }
    };
}

impl_system_param_function!((P1, p1));
impl_system_param_function!((P1, p1), (P2, p2));
impl_system_param_function!((P1, p1), (P2, p2), (P3, p3));
impl_system_param_function!((P1, p1), (P2, p2), (P3, p3), (P4, p4));
impl_system_param_function!((P1, p1), (P2, p2), (P3, p3), (P4, p4), (P5, p5));
impl_system_param_function!((P1, p1), (P2, p2), (P3, p3), (P4, p4), (P5, p5), (P6, p6));
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7)
);
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7),
    (P8, p8)
);
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7),
    (P8, p8),
    (P9, p9)
);
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7),
    (P8, p8),
    (P9, p9),
    (P10, p10)
);
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7),
    (P8, p8),
    (P9, p9),
    (P10, p10),
    (P11, p11)
);
impl_system_param_function!(
    (P1, p1),
    (P2, p2),
    (P3, p3),
    (P4, p4),
    (P5, p5),
    (P6, p6),
    (P7, p7),
    (P8, p8),
    (P9, p9),
    (P10, p10),
    (P11, p11),
    (P12, p12)
);

/// Short system name from a function type: `my_game::systems::movement` →
/// `movement` (drops the module path; generic brackets are kept).
pub fn short_system_name<F: ?Sized>() -> &'static str {
    let full = std::any::type_name::<F>();
    // Slice up to the last `::` OUTSIDE angle brackets.
    let mut depth = 0usize;
    let bytes = full.as_bytes();
    let mut cut = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b':' => {
                cut = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    &full[cut..]
}
