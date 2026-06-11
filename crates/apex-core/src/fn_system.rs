//! Plain-fn системы (D2-1): обычная функция как система — Bevy-стиль 1:1.
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
//! Параметры — пользовательские типы с Bevy-семантикой (см. impl'ы
//! `SystemParam` в [`system_param`](crate::system_param)): `Res<T>` /
//! `ResMut<T>` / `Query<Q>` / `CachedQuery<Q>` / `EventReader<E>` /
//! `EventWriter<E>` / `&mut Commands`. ⚠ В отличие от `system!`, здесь
//! `Res<T>` — ресурс, а `&T`-компоненты живут ВНУТРИ `Query<…>` — ловушка
//! «`&T` = ресурс» макроса в plain-fn пути отсутствует.
//!
//! Access выводится статически (merge access'ов параметров), имя системы —
//! из имени функции. `system!` остаётся диалектом (системы со state там
//! удобнее Bevy `Local<T>`).
//!
//! Реализация — классический Bevy-паттерн «двойного Fn-баунда»: функция
//! обязана быть вызываемой и с декларированными типами параметров, и с их
//! `Item<'w>` для любого `'w` (для fn-item'ов это одно и то же благодаря
//! лайфтайм-генерализации элизии).

use crate::system_param::SystemParam;

/// Функция, чьи параметры — [`SystemParam`]. Реализуется автоматически для
/// `fn`/замыканий с 1..=12 параметрами; `Marker` — тип-дизамбигуатор
/// (`fn(P1, …)`), позволяющий одному типу функции матчить ровно один impl.
pub trait SystemParamFunction<Marker>: Send + Sync + 'static {
    /// Кортеж параметров (даже для одного параметра — `(P1,)`).
    type Param: SystemParam;

    /// Лайфтайм ресивера связан с лайфтаймом item — это позволяет HRTB-баунду
    /// `for<'w> &'w mut Func: FnMut(Item<'w>…)` резолвиться по диагонали.
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
                // Вызов через мономорфизированный шим: связывает HRTB-баунд
                // с конкретными Item-типами этого вызова (Bevy-паттерн —
                // напрямую `(self)(…)` компилятор по HRTB не резолвит).
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

/// Короткое имя системы из типа функции: `my_game::systems::movement` →
/// `movement` (отбрасывает путь модулей; generic-скобки сохраняются).
pub fn short_system_name<F: ?Sized>() -> &'static str {
    let full = std::any::type_name::<F>();
    // Срез до последнего `::` ВНЕ угловых скобок.
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
