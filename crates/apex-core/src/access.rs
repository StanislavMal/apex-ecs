use std::any::TypeId;

// ComponentMask (512-bit O(1) conflict mask) removed in the wave-4 dead-code
// pass: `assign_masks` was never called, so the masks stayed EMPTY and
// `conflicts_with` always took the linear-scan fallback — the whole mask
// fast-path (struct, fields, assign_masks, conflicts_with_fast) was dead (C8).
//
// ArchetypeMask (фикс 1024 бита) удалена в CR-M4: единственным живым
// потребителем был exclude_mask в Query::new (избыточен после CR-M2 —
// Without корректно фильтруется в matches_archetype), а за пределом 1024
// архетипов set/get тихо вырождались в no-op (C-9).

/// Декларация Read/Write доступа системы к данным мира.
///
/// Хранит `TypeId`-векторы читаемых/записываемых компонентов и событий.
/// Конфликты между системами определяются линейным сравнением этих векторов
/// (число компонентов на систему мало).
///
/// Правила конфликтов — аналог Rust borrow checker:
/// - Write + Read  → конфликт
/// - Write + Write → конфликт
/// - Read  + Read  → нет конфликта (параллельны)
///
/// Также поддерживает декларацию доступа к событиям (events):
/// - `read_event<T>()` / `write_event<T>()` — декларация чтения/записи событий
/// - Два писателя одного типа событий конфликтуют (WriteWrite)
/// - Писатель и читатель одного типа событий конфликтуют (WriteRead)
/// - Два читателя одного типа событий — НЕ конфликтуют
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads: Vec<TypeId>,
    pub writes: Vec<TypeId>,
    /// Типы событий, которые система читает (TypeId, имя_типа).
    pub reads_event: Vec<(TypeId, &'static str)>,
    /// Типы событий, которые система пишет (TypeId, имя_типа).
    pub writes_event: Vec<(TypeId, &'static str)>,
    /// Флаг: система использует `par_for_each` внутри себя.
    /// Планировщик не будет чанковать такую систему через ASD
    /// (избегает oversubscribe rayon thread pool).
    pub uses_par_for_each: bool,
    /// Флаг: системе нужны ВСЕ entity (глобальный доступ).
    /// ASD-чанкование запрещено — система всегда получает полный SubWorld.
    pub needs_whole_world: bool,
    /// Флаг: у системы есть СОСТОЯНИЕ (`size_of::<S>() > 0` — state-системы
    /// `system!`, замыкания с захватами). ASD row-split запрещён (W3-4):
    /// несколько split-задач звали бы `run(&mut self)` ОДНОЙ системы
    /// конкурентно — гонка на состоянии. Ставится планировщиком при
    /// регистрации; параллельность МЕЖДУ системами не ограничивает.
    pub stateful: bool,
    /// Флаг: система МУТИРУЕТ ресурс (`ResMut`/`ResWrite`). ASD-декомпозиция ЗАПРЕЩЕНА (TD-37):
    /// тело plain-fn системы выполняется раз на чанк, поэтому мутация ресурса умножилась бы на
    /// число чанков (молчаливый баг только на масштабе) + гонка при параллельных чанках. Ставится
    /// `ResMut::access()`/`ResWrite::access()`; параллельность МЕЖДУ системами не ограничивает
    /// (её решает конфликт по `writes`-TypeId, куда ресурс тоже попадает).
    pub writes_resource: bool,
    /// Флаг: система использует `Commands` (отложенные структурные операции). ASD-декомпозиция
    /// ЗАПРЕЩЕНА (TD-37): не-entity-локальные команды дублировались бы по чанкам. Ставится
    /// `Commands`/`CommandsParam::access()`.
    pub uses_commands: bool,
    /// Типы и зарезервированные capacity для event-буферов.
    /// Планировщик вызывает `world.event_reserve::<T>(cap)` перед
    /// выполнением системы, чтобы избежать реаллокаций в hot-пути send().
    pub event_reserves: Vec<(TypeId, usize)>,
}

impl AccessDescriptor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.reads.contains(&tid) {
            self.reads.push(tid);
        }
        self
    }

    pub fn write<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        if !self.writes.contains(&tid) {
            self.writes.push(tid);
        }
        self
    }

    /// Декларировать чтение событий типа T.
    pub fn read_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        let name = std::any::type_name::<T>();
        if !self.reads_event.iter().any(|(id, _)| *id == tid) {
            self.reads_event.push((tid, name));
        }
        self
    }

    /// Пометить что система использует `par_for_each` внутри себя.
    /// Планировщик не будет дополнительно чанковать систему через ASD.
    pub fn par_for_each_used(mut self) -> Self {
        self.uses_par_for_each = true;
        self
    }

    /// Пометить что системе нужен доступ ко ВСЕМ entity (глобальный доступ).
    /// Планировщик не будет чанковать систему через ASD.
    pub fn whole_world(mut self) -> Self {
        self.needs_whole_world = true;
        self
    }

    /// Пометить что система МУТИРУЕТ ресурс (`ResMut`/`ResWrite`). Запрещает ASD-декомпозицию
    /// (тело системы выполнялось бы раз на чанк ⇒ мутация ресурса × число чанков). См. TD-37.
    pub fn resource_write(mut self) -> Self {
        self.writes_resource = true;
        self
    }

    /// Пометить что система использует `Commands`. Запрещает ASD-декомпозицию (не-entity-локальные
    /// команды дублировались бы по чанкам). См. TD-37.
    pub fn commands_used(mut self) -> Self {
        self.uses_commands = true;
        self
    }

    /// Декларировать запись событий типа T.
    pub fn write_event<T: 'static>(mut self) -> Self {
        let tid = TypeId::of::<T>();
        let name = std::any::type_name::<T>();
        if !self.writes_event.iter().any(|(id, _)| *id == tid) {
            self.writes_event.push((tid, name));
        }
        self
    }

    /// Зарезервировать capacity для event-буфера типа T.
    ///
    /// Избегает реаллокаций `Vec` при массовой отправке событий в цикле.
    /// Планировщик вызывает `world.event_reserve::<T>(capacity)` перед
    /// выполнением системы на основе этой декларации.
    pub fn event_reserve<T: 'static>(mut self, capacity: usize) -> Self {
        let tid = TypeId::of::<T>();
        self.event_reserves.push((tid, capacity));
        self
    }

    pub fn merge(mut self, other: &AccessDescriptor) -> Self {
        // O(N+M) дедупликация через HashSet вместо O(N²) contains+push
        Self::dedup_push(&mut self.reads, &other.reads);
        Self::dedup_push(&mut self.writes, &other.writes);
        Self::dedup_push_event(&mut self.reads_event, &other.reads_event);
        Self::dedup_push_event(&mut self.writes_event, &other.writes_event);
        // Передаём флаг par_for_each
        self.uses_par_for_each = self.uses_par_for_each || other.uses_par_for_each;
        self.needs_whole_world = self.needs_whole_world || other.needs_whole_world;
        self.stateful = self.stateful || other.stateful;
        self.writes_resource = self.writes_resource || other.writes_resource;
        self.uses_commands = self.uses_commands || other.uses_commands;
        // Сливаем резервирования событий (берём максимум по каждому типу)
        for &(tid, cap) in &other.event_reserves {
            if let Some(entry) = self.event_reserves.iter_mut().find(|(id, _)| *id == tid) {
                entry.1 = entry.1.max(cap);
            } else {
                self.event_reserves.push((tid, cap));
            }
        }
        self
    }

    /// O(N) проверка конфликта данных линейным сравнением write/read-векторов.
    ///
    /// Write(self) ∩ (Read(other) ∪ Write(other)) ≠ ∅, либо
    /// Write(other) ∩ Read(self) ≠ ∅. Число компонентов на систему мало,
    /// поэтому линейного скана достаточно.
    pub fn conflicts_with(&self, other: &AccessDescriptor) -> bool {
        if !self.writes.is_empty() || !other.writes.is_empty() {
            for w in &self.writes {
                if other.reads.contains(w) || other.writes.contains(w) {
                    return true;
                }
            }
            for w in &other.writes {
                if self.reads.contains(w) || self.writes.contains(w) {
                    return true;
                }
            }
        }
        // Нет writes — нет конфликта
        false
    }

    /// O(N+M) дедупликация — для малых наборов linear scan, для больших — HashSet.
    fn dedup_push(vec: &mut Vec<TypeId>, items: &[TypeId]) {
        if items.is_empty() {
            return;
        }
        // Порог: если суммарный размер < 8 — O(N²) дешевле аллокации HashSet
        if vec.len() + items.len() < 8 {
            for &item in items {
                if !vec.contains(&item) {
                    vec.push(item);
                }
            }
            return;
        }
        // Для больших наборов — HashSet как прежде
        let mut set: std::collections::HashSet<TypeId> = vec.iter().cloned().collect();
        for &item in items {
            if set.insert(item) {
                vec.push(item);
            }
        }
    }

    /// O(N+M) дедупликация для event-кортежей (TypeId, имя_типа).
    fn dedup_push_event(vec: &mut Vec<(TypeId, &'static str)>, items: &[(TypeId, &'static str)]) {
        if items.is_empty() {
            return;
        }
        if vec.len() + items.len() < 8 {
            for &item in items {
                if !vec.iter().any(|(id, _)| *id == item.0) {
                    vec.push(item);
                }
            }
            return;
        }
        let mut set: std::collections::HashSet<TypeId> = vec.iter().map(|(id, _)| *id).collect();
        for &item in items {
            if set.insert(item.0) {
                vec.push(item);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.reads_event.is_empty()
            && self.writes_event.is_empty()
    }
}

/// Создать `AccessDescriptor` декларативным синтаксисом.
///
/// Поддерживает `read<T>`, `write<T>`, `read_event<T>`, `write_event<T>`.
///
/// # Пример
///
/// ```ignore
/// access_desc!(read<Vel>, write<Pos>, read_event<DamageEvent>)
/// ```
#[macro_export]
macro_rules! access_desc {
    () => {
        $crate::AccessDescriptor::new()
    };
    ($($action:ident < $ty:ty >),+ $(,)?) => {{
        let mut desc = $crate::AccessDescriptor::new();
        $(
            desc = desc.$action::<$ty>();
        )+
        desc
    }};
}
