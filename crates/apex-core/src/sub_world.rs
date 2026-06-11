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
///
/// # Storage
/// `world`, `archetype_indices` и `row_ranges` хранятся как сырые указатели,
/// чтобы избежать UB продления lifetime и исключить конфликт `&World`/`&mut World`
/// при параллельном выполнении.
pub struct SubWorld<'w> {
    /// Сырой указатель на World. Всегда валиден пока SubWorld существует.
    world: *const World,
    /// Индексы архетипов, которые входят в этот SubWorld (сырой fat-указатель)
    archetype_indices: *const [usize],
    /// Опциональные ограничения строк для row-level splits.
    row_ranges: *const [(usize, usize, usize)],
    #[allow(dead_code)]
    _phantom: std::marker::PhantomData<&'w ()>,
}

impl<'w> SubWorld<'w> {
    #[inline]
    pub fn new(world: &World, archetype_indices: &[usize]) -> Self {
        Self {
            world: world as *const World,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: std::ptr::slice_from_raw_parts(
                std::ptr::null::<(usize, usize, usize)>(),
                0,
            ),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Создать SubWorld с row-level range ограничениями.
    ///
    /// # Safety
    /// Переданные срезы должны жить не меньше самого SubWorld.
    /// SubWorld хранит сырые указатели на данные.
    #[inline]
    pub fn with_ranges(
        world: &World,
        archetype_indices: &[usize],
        row_ranges: &[(usize, usize, usize)],
    ) -> Self {
        Self {
            world: world as *const World,
            archetype_indices: archetype_indices as *const [usize],
            row_ranges: row_ranges as *const [(usize, usize, usize)],
            _phantom: std::marker::PhantomData,
        }
    }

    /// Получить ссылку на World.
    #[inline]
    fn world_ref(&self) -> &World {
        unsafe { &*self.world }
    }

    /// Вернуть срез индексов архетипов.
    #[inline]
    fn archetype_indices_slice(&self) -> &[usize] {
        if self.archetype_indices.is_null() {
            return &[];
        }
        unsafe { &*self.archetype_indices }
    }

    /// Вернуть срез row_ranges.
    #[inline]
    fn row_ranges_slice(&self) -> &[(usize, usize, usize)] {
        if self.row_ranges.is_null() {
            return &[];
        }
        unsafe { &*self.row_ranges }
    }

    // ── Public API ──────────────────────────────

    #[inline]
    pub fn world(&self) -> &World {
        self.world_ref()
    }

    #[inline]
    pub fn archetype_indices(&self) -> &[usize] {
        self.archetype_indices_slice()
    }

    #[inline]
    pub fn row_ranges(&self) -> &[(usize, usize, usize)] {
        self.row_ranges_slice()
    }

    /// Количество архетипов в этом SubWorld.
    #[inline]
    pub fn archetype_count(&self) -> usize {
        self.archetype_indices_slice().len()
    }

    /// Общее количество entity во всех архетипах этого SubWorld.
    pub fn entity_count(&self) -> usize {
        let w = self.world_ref();
        self.archetype_indices_slice()
            .iter()
            .map(|&idx| unsafe { (&*w.archetype_ptr(idx)).len() })
            .sum()
    }

    // ── Resource API ────────────────────────────

    #[inline]
    pub fn resource<T: Send + Sync + 'static>(&self) -> Res<'_, T> {
        Res(self.world_ref().resource::<T>())
    }

    #[inline]
    pub fn resource_mut<T: Send + Sync + 'static>(&self) -> ResMut<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .resources
                .get_raw_ptr::<T>()
                .expect("resource_mut: resource not found");
            ResMut::from_ptr(ptr)
        }
    }

    // ── Event API ───────────────────────────────

    #[inline]
    pub fn event_reader<T: Send + Sync + 'static>(&self) -> EventReader<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .event_queue_ptr::<T>()
                .expect("event_reader: event type not registered");
            EventReader::new(&mut *ptr)
        }
    }

    #[inline]
    pub fn event_writer<T: Send + Sync + 'static>(&self) -> EventWriter<'_, T> {
        unsafe {
            let ptr = self
                .world_ref()
                .event_queue_ptr::<T>()
                .expect("event_writer: event queue not found");
            EventWriter::from_ptr(ptr)
        }
    }

    // ── Row-level parallel API ──────────────────

    #[inline]
    fn arch_row_range(&self, arch_idx: usize) -> Option<(usize, usize)> {
        self.row_ranges_slice().iter().find_map(
            |&(a, s, e)| {
                if a == arch_idx {
                    Some((s, e))
                } else {
                    None
                }
            },
        )
    }

    /// Последовательная итерация по всем entity в этом SubWorld.
    #[inline]
    pub fn for_each_entity<F: FnMut(Entity)>(&self, mut f: F) {
        let w = self.world_ref();
        for &arch_idx in self.archetype_indices_slice() {
            let arch = unsafe { &*w.archetype_ptr(arch_idx) };
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
    pub fn par_for_each_entity<F: Fn(Entity) + Send + Sync>(&self, f: F) {
        use crate::par_utils::compute_par_chunks;
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();
        let w = self.world_ref();
        let chunks = compute_par_chunks(
            self.archetype_indices_slice().iter().map(|&arch_idx| {
                let arch = unsafe { &*w.archetype_ptr(arch_idx) };
                if let Some((start, end)) = self.arch_row_range(arch_idx) {
                    let len = end.min(arch.len()).saturating_sub(start);
                    (arch_idx, len)
                } else {
                    (arch_idx, arch.len())
                }
            }),
            num_threads,
            w.chunk_config(),
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let w = self.world_ref();
            let arch = unsafe { &*w.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            // Чанки считаются в координатах ДИАПАЗОНА (0..len) — переводим в
            // глобальные строки архетипа смещением r_start (W3-4: раньше
            // смещение терялось и сплит-задача ходила по чужим строкам).
            let (r_start, r_end) = self.arch_row_range(arch_idx).unwrap_or((0, usize::MAX));
            let row_start = r_start + start;
            let row_end = (r_start + end).min(r_end).min(entities.len());
            for row in row_start..row_end {
                f(entities[row]);
            }
        });
    }

    /// Последовательная итерация по строкам архетипов SubWorld.
    #[inline]
    pub fn for_each_row<F: FnMut(Entity, usize)>(&self, mut f: F) {
        let w = self.world_ref();
        for &arch_idx in self.archetype_indices_slice() {
            let arch = unsafe { &*w.archetype_ptr(arch_idx) };
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
    pub fn par_for_each_row<F: Fn(Entity, usize) + Send + Sync>(&self, f: F) {
        use crate::par_utils::compute_par_chunks;
        use rayon::prelude::*;

        let num_threads = rayon::current_num_threads();
        let w = self.world_ref();
        let chunks = compute_par_chunks(
            self.archetype_indices_slice().iter().map(|&arch_idx| {
                let arch = unsafe { &*w.archetype_ptr(arch_idx) };
                if let Some((start, end)) = self.arch_row_range(arch_idx) {
                    let len = end.min(arch.len()).saturating_sub(start);
                    (arch_idx, len)
                } else {
                    (arch_idx, arch.len())
                }
            }),
            num_threads,
            w.chunk_config(),
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let w = self.world_ref();
            let arch = unsafe { &*w.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            // См. par_for_each_entity: чанк → глобальные строки через r_start.
            let (r_start, r_end) = self.arch_row_range(arch_idx).unwrap_or((0, usize::MAX));
            let row_start = r_start + start;
            let row_end = (r_start + end).min(r_end).min(entities.len());
            for row in row_start..row_end {
                f(entities[row], row);
            }
        });
    }
}

unsafe impl Send for SubWorld<'_> {}
unsafe impl Sync for SubWorld<'_> {}
