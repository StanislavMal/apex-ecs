use crate::{
    entity::Entity,
    system_param::{EventReader, EventWriter, Res, ResMut},
    World,
};

/// Представление на подмножество архетипов World'а.
///
/// Содержит индексы архетипов, которые соответствуют AccessDescriptor системы.
/// Не владеет данными — только ссылается на них через World.
///
/// # Row-level splits (5.7)
///
/// Если поле `row_ranges` не пусто, итерация ограничивается указанными
/// диапазонами строк `(arch_idx, start, end)`. Это позволяет нескольким
/// системам с одинаковым ArchetypeMask параллельно обрабатывать разные
/// строки одного архетипа.
///
/// # Безопасность
/// - SubWorld не владеет данными — World должен быть жив всё время использования.
/// - Разные SubWorld для разных систем в одном Stage не пересекаются по архетипам
///   (проверено compile() через AccessDescriptor).
/// - Structural changes запрещены во время выполнения систем.
pub struct SubWorld<'w> {
    /// Ссылка на оригинальный World (нужна для доступа к entity, registry, relations, resources)
    pub(crate) world: &'w World,
    /// Индексы архетипов, которые входят в этот SubWorld
    pub(crate) archetype_indices: &'w [usize],
    /// Опциональные ограничения строк для row-level splits.
    /// Каждый элемент: `(arch_idx, start, end)` — если arch_idx есть в `archetype_indices`,
    /// то итерация по нему ограничивается строками `[start, end)`.
    /// Если пусто — ограничений нет (все строки всех архетипов).
    pub(crate) row_ranges: &'w [(usize, usize, usize)],
}

impl<'w> SubWorld<'w> {
    #[inline]
    pub fn new(world: &'w World, archetype_indices: &'w [usize]) -> Self {
        Self {
            world,
            archetype_indices,
            row_ranges: &[],
        }
    }

    /// Создать SubWorld с row-level range ограничениями.
    ///
    /// # Safety
    /// Переданные срезы `archetype_indices` и `row_ranges` должны жить
    /// не меньше самого SubWorld. Внутренне lifetimes продлеваются через
    /// transmute, так как SubWorld не экспортирует эти ссылки наружу.
    #[inline]
    pub fn with_ranges(
        world: &'w World,
        archetype_indices: &[usize],
        row_ranges: &[(usize, usize, usize)],
    ) -> Self {
        unsafe {
            Self {
                world,
                archetype_indices: std::mem::transmute::<&[usize], &'w [usize]>(archetype_indices),
                row_ranges: std::mem::transmute::<&[(usize, usize, usize)], &'w [(usize, usize, usize)]>(row_ranges),
            }
        }
    }

    /// Количество архетипов в этом SubWorld.
    #[inline]
    pub fn archetype_count(&self) -> usize {
        self.archetype_indices.len()
    }

    /// Общее количество entity во всех архетипах этого SubWorld.
    pub fn entity_count(&self) -> usize {
        self.archetype_indices
            .iter()
            .map(|&idx| unsafe { (&*self.world.archetype_ptr(idx)).len() })
            .sum()
    }

    // ── Resource API ────────────────────────────────────────────

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world.resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self
                .world
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    // ── Event API ───────────────────────────────────────────────

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self.world.event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self.world.event_queue_ptr::<T>()
                .expect("event_writer: event queue not found");
            EventWriter::from_ptr(ptr)
        }
    }

    // ── Row-level parallel API (3.2) ─────────────────────────────

    /// Вернуть диапазон строк для архетипа `arch_idx`, если есть row_ranges.
    #[inline]
    fn arch_row_range(&self, arch_idx: usize) -> Option<(usize, usize)> {
        self.row_ranges.iter().find_map(|&(a, s, e)| {
            if a == arch_idx { Some((s, e)) } else { None }
        })
    }

    /// Последовательная итерация по всем entity в этом SubWorld.
    ///
    /// Если заданы row_ranges — итерация ограничена указанными диапазонами.
    #[inline]
    pub fn for_each_entity<F: FnMut(Entity)>(&self, mut f: F) {
        for &arch_idx in self.archetype_indices {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            if let Some((start, end)) = self.arch_row_range(arch_idx) {
                for row in start..end.min(entities.len()) {
                    f(entities[row]);
                }
            } else {
                for &entity in entities {
                    f(entity);
                }
            }
        }
    }

    /// Параллельная итерация по всем entity в этом SubWorld.
    ///
    /// Если заданы row_ranges — итерация ограничена указанными диапазонами.
    #[cfg(feature = "parallel")]
    pub fn par_for_each_entity<F: Fn(Entity) + Send + Sync>(&self, f: F) {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;

        let num_threads = rayon::current_num_threads();
        let chunks = compute_par_chunks(
            self.archetype_indices.iter().map(|&arch_idx| {
                let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
                if let Some((start, end)) = self.arch_row_range(arch_idx) {
                    let len = end.min(arch.len()).saturating_sub(start);
                    (arch_idx, len)
                } else {
                    (arch_idx, arch.len())
                }
            }),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            // Если есть row_ranges для этого arch_idx, дополнительно ограничиваем
            let effective_start = start;
            let effective_end = if let Some((rr_s, rr_e)) = self.arch_row_range(arch_idx) {
                end.min(rr_e)
            } else {
                end
            };
            for row in effective_start..effective_end {
                f(entities[row]);
            }
        });
    }

    /// Последовательная итерация по строкам архетипов SubWorld.
    ///
    /// Если заданы row_ranges — итерация ограничена указанными диапазонами.
    #[inline]
    pub fn for_each_row<F: FnMut(Entity, usize)>(&self, mut f: F) {
        for &arch_idx in self.archetype_indices {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            if let Some((start, end)) = self.arch_row_range(arch_idx) {
                for row in start..end.min(arch.len()) {
                    f(entities[row], row);
                }
            } else {
                for row in 0..arch.len() {
                    f(entities[row], row);
                }
            }
        }
    }

    /// Параллельная итерация по строкам архетипов SubWorld.
    ///
    /// Если заданы row_ranges — итерация ограничена указанными диапазонами.
    #[cfg(feature = "parallel")]
    pub fn par_for_each_row<F: Fn(Entity, usize) + Send + Sync>(&self, f: F) {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;

        let num_threads = rayon::current_num_threads();
        let chunks = compute_par_chunks(
            self.archetype_indices.iter().map(|&arch_idx| {
                let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
                if let Some((start, end)) = self.arch_row_range(arch_idx) {
                    let len = end.min(arch.len()).saturating_sub(start);
                    (arch_idx, len)
                } else {
                    (arch_idx, arch.len())
                }
            }),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            let effective_start = start;
            let effective_end = if let Some((rr_s, rr_e)) = self.arch_row_range(arch_idx) {
                end.min(rr_e)
            } else {
                end
            };
            for row in effective_start..effective_end {
                f(entities[row], row);
            }
        });
    }
}

unsafe impl Send for SubWorld<'_> {}
unsafe impl Sync for SubWorld<'_> {}
