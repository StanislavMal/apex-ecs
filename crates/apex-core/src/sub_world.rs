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
}

impl<'w> SubWorld<'w> {
    #[inline]
    pub fn new(world: &'w World, archetype_indices: &'w [usize]) -> Self {
        Self { world, archetype_indices }
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
        EventReader(unsafe { self.world.events::<T>() })
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

    /// Последовательная итерация по всем entity в этом SubWorld.
    ///
    /// Позволяет эффективно обойти все entity, принадлежащие системе,
    /// без необходимости создавать Query.
    ///
    /// # Пример
    ///
    /// ```ignore
    /// sub_world.for_each_entity(|entity| {
    ///     println!("entity = {}", entity);
    /// });
    /// ```
    #[inline]
    pub fn for_each_entity<F: FnMut(Entity)>(&self, mut f: F) {
        for &arch_idx in self.archetype_indices {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            for &entity in arch.entities() {
                f(entity);
            }
        }
    }

    /// Параллельная итерация по всем entity в этом SubWorld.
    ///
    /// Разбивает entity на чанки используя `compute_par_chunks` и
    /// обрабатывает их параллельно через rayon.
    ///
    /// Доступна только с feature `parallel`.
    ///
    /// # SAFETY
    /// - Замыкание `f` не должно делать structural changes (spawn/despawn).
    /// - Доступ к компонентам через `world.get::<T>(entity)` безопасен,
    ///   так как SubWorld гарантирует отсутствие конфликтов.
    #[cfg(feature = "parallel")]
    pub fn par_for_each_entity<F: Fn(Entity) + Send + Sync>(&self, f: F) {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;

        let num_threads = rayon::current_num_threads();
        let chunks = compute_par_chunks(
            self.archetype_indices.iter().map(|&arch_idx| {
                let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
                (arch_idx, arch.len())
            }),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            for row in start..end {
                f(entities[row]);
            }
        });
    }

    /// Последовательная итерация по строкам архетипов SubWorld.
    ///
    /// Замыкание получает `(Entity, архетип, row)` — это позволяет
    /// читать компоненты напрямую из колонок архетипа.
    #[inline]
    pub fn for_each_row<F: FnMut(Entity, usize)>(&self, mut f: F) {
        for &arch_idx in self.archetype_indices {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            for row in 0..arch.len() {
                f(entities[row], row);
            }
        }
    }

    /// Параллельная итерация по строкам архетипов SubWorld.
    ///
    /// Замыкание получает `(Entity, row)` — row это строка в архетипе,
    /// по которой можно читать/писать компоненты.
    #[cfg(feature = "parallel")]
    pub fn par_for_each_row<F: Fn(Entity, usize) + Send + Sync>(&self, f: F) {
        use rayon::prelude::*;
        use crate::par_utils::compute_par_chunks;

        let num_threads = rayon::current_num_threads();
        let chunks = compute_par_chunks(
            self.archetype_indices.iter().map(|&arch_idx| {
                let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
                (arch_idx, arch.len())
            }),
            num_threads,
        );

        chunks.par_iter().for_each(|&(arch_idx, start, end)| {
            let arch = unsafe { &*self.world.archetype_ptr(arch_idx) };
            let entities = arch.entities();
            for row in start..end {
                f(entities[row], row);
            }
        });
    }
}

unsafe impl Send for SubWorld<'_> {}
unsafe impl Sync for SubWorld<'_> {}
