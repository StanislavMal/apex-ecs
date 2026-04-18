use crate::{
    component::{Component, ComponentId},
    entity::Entity,
    world::World,
};

pub struct QueryBuilder<'w> {
    world: &'w World,
    reads: Vec<ComponentId>,
    writes: Vec<ComponentId>,
    excludes: Vec<ComponentId>,
}

impl<'w> QueryBuilder<'w> {
    pub fn new(world: &'w World) -> Self {
        Self { world, reads: Vec::new(), writes: Vec::new(), excludes: Vec::new() }
    }

    pub fn read<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.reads.push(id);
        }
        self
    }

    pub fn write<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.writes.push(id);
        }
        self
    }

    pub fn exclude<T: Component>(mut self) -> Self {
        if let Some(id) = self.world.registry.get_id::<T>() {
            self.excludes.push(id);
        }
        self
    }

    /// Индексы архетипов, удовлетворяющих всем фильтрам
    pub fn matching_archetype_ids(&self) -> Vec<usize> {
        self.world
            .archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| self.matches_arch(arch))
            .map(|(i, _)| i)
            .collect()
    }

    #[inline]
    fn matches_arch(&self, arch: &crate::archetype::Archetype) -> bool {
        self.reads.iter().all(|id| arch.has_component(*id))
            && self.writes.iter().all(|id| arch.has_component(*id))
            && self.excludes.iter().all(|id| !arch.has_component(*id))
    }

    /// Итерация по компоненту T с учётом всех фильтров builder'а
    pub fn iter_one<T: Component>(
        &'w self,
    ) -> Box<dyn Iterator<Item = (Entity, &'w T)> + 'w> {
        let comp_id = match self.world.registry.get_id::<T>() {
            Some(id) => id,
            None => return Box::new(std::iter::empty()),
        };

        // Архетипы должны иметь T И удовлетворять всем фильтрам
        let arch_indices: Vec<usize> = self
            .world
            .archetypes
            .iter()
            .enumerate()
            .filter(|(_, arch)| arch.has_component(comp_id) && self.matches_arch(arch))
            .map(|(i, _)| i)
            .collect();

        Box::new(SingleComponentIter {
            world: self.world,
            arch_indices,
            comp_id,
            arch_cursor: 0,
            row_cursor: 0,
            _phantom: std::marker::PhantomData,
        })
    }
}

struct SingleComponentIter<'w, T> {
    world: &'w World,
    arch_indices: Vec<usize>,
    comp_id: ComponentId,
    arch_cursor: usize,
    row_cursor: usize,
    _phantom: std::marker::PhantomData<&'w T>,
}

impl<'w, T: Component> Iterator for SingleComponentIter<'w, T> {
    type Item = (Entity, &'w T);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let arch_idx = *self.arch_indices.get(self.arch_cursor)?;
            let arch = &self.world.archetypes[arch_idx];

            if self.row_cursor >= arch.len() {
                self.arch_cursor += 1;
                self.row_cursor = 0;
                continue;
            }

            let entity = arch.entities[self.row_cursor];
            let row = self.row_cursor;
            self.row_cursor += 1;

            let component = unsafe { arch.get_component::<T>(row, self.comp_id)? };
            return Some((entity, component));
        }
    }
}
