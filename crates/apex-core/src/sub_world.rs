use crate::{
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
}

unsafe impl Send for SubWorld<'_> {}
unsafe impl Sync for SubWorld<'_> {}
