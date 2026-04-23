//! Rhai-совместимые итераторы для `query()` и `reader()`.
//!
//! # Дизайн query-итератора
//!
//! Каждый элемент итератора — `rhai::Dynamic::from_map(Map)` со структурой:
//! ```text
//! {
//!     "entity":    <entity_index: i64>,
//!     "Position":  #{ x: 1.0, y: 2.0 },
//!     "Velocity":  #{ x: 0.5, y: 0.0 },
//! }
//! ```
//!
//! После скрипта, если были Write-компоненты, `flush_writes()` записывает
//! изменённые Dynamic-значения обратно в Column через binding.write.
//!
//! # Формат дескрипторов
//!
//! `query(["Read:Position", "Write:Velocity"])` — массив строк вида `"Mode:TypeName"`.
//! Парсится в `QueryDesc` в `parse_query_descs()`.

use std::{cell::RefCell, rc::Rc};

use apex_core::{
    archetype::Archetype,
    component::ComponentId,
    entity::Entity,
    world::World,
};
use rhai::Dynamic;

use crate::context::ScriptContext;

// ── QueryDesc ──────────────────────────────────────────────────

/// Разобранный дескриптор одного компонента в запросе.
#[derive(Clone, Debug)]
pub struct QueryDesc {
    /// Имя типа компонента
    pub type_name: String,
    /// true = Write (мутабельный), false = Read
    pub write:     bool,
}

/// Разобрать массив строк вида `["Read:Position", "Write:Velocity"]` в `Vec<QueryDesc>`.
///
/// Поддерживаемые форматы:
/// - `"Read:Position"` / `"Write:Position"` — явный режим
/// - `"Position"` — Read по умолчанию
/// - `"Read<Position>"` / `"Write<Position>"` — альтернативный синтаксис
pub fn parse_query_descs(arr: &rhai::Array) -> Vec<QueryDesc> {
    arr.iter()
        .filter_map(|d| {
            let s = d.clone().into_string().ok()?;
            parse_one_desc(&s)
        })
        .collect()
}

fn parse_one_desc(s: &str) -> Option<QueryDesc> {
    let s = s.trim();

    // Формат "Read:TypeName" или "Write:TypeName"
    if let Some(rest) = s.strip_prefix("Write:").or_else(|| s.strip_prefix("write:")) {
        return Some(QueryDesc { type_name: rest.trim().to_string(), write: true });
    }
    if let Some(rest) = s.strip_prefix("Read:").or_else(|| s.strip_prefix("read:")) {
        return Some(QueryDesc { type_name: rest.trim().to_string(), write: false });
    }

    // Формат "Write<TypeName>" или "Read<TypeName>"
    if let Some(rest) = s.strip_prefix("Write<").or_else(|| s.strip_prefix("write<")) {
        let name = rest.trim_end_matches('>').trim().to_string();
        return Some(QueryDesc { type_name: name, write: true });
    }
    if let Some(rest) = s.strip_prefix("Read<").or_else(|| s.strip_prefix("read<")) {
        let name = rest.trim_end_matches('>').trim().to_string();
        return Some(QueryDesc { type_name: name, write: false });
    }

    // Без префикса — Read по умолчанию
    Some(QueryDesc { type_name: s.to_string(), write: false })
}

// ── ArchState ──────────────────────────────────────────────────

/// Состояние одного архетипа в query-итераторе.
#[derive(Clone)]
struct ArchState {
    /// Индекс архетипа в `world.archetypes`
    arch_idx: usize,
    /// Количество entity в архетипе
    len: usize,
    /// Binding info для каждого запрошенного компонента
    /// (component_id, type_name, is_write, col_index_in_arch)
    components: Vec<ComponentState>,
}

#[derive(Clone)]
struct ComponentState {
    col_idx:   usize,
    type_name: String,
    write:     bool,
    comp_id:   ComponentId,
}

// ── RhaiQueryIter ──────────────────────────────────────────────

/// Rhai-совместимый итератор результатов `query()`.
///
/// Регистрируется через `engine.register_iterator::<RhaiQueryIter>()`.
#[derive(Clone)]
pub struct RhaiQueryIter {
    ctx:             Rc<RefCell<ScriptContext>>,
    arch_states:     Vec<ArchState>,
    arch_cursor:     usize,
    row_cursor:      usize,
    /// Накопленные Write-изменения: (arch_idx, row, type_name, Dynamic)
    /// Сбрасываются в Column после завершения итерации через flush_writes()
    pending_writes:  Vec<(usize, usize, String, Dynamic)>,
}

impl RhaiQueryIter {
    /// Создать итератор для заданного набора дескрипторов.
    ///
    /// Перебирает все архетипы мира и отбирает те, которые содержат
    /// ВСЕ запрошенные компоненты.
    pub fn new(ctx: Rc<RefCell<ScriptContext>>, descs: Vec<QueryDesc>) -> Self {
        let arch_states = {
            let ctx_ref   = ctx.borrow();
            let world     = ctx_ref.world_ref();
            build_arch_states(world, &ctx_ref, &descs)
        };

        Self {
            ctx,
            arch_states,
            arch_cursor:    0,
            row_cursor:     0,
            pending_writes: Vec::new(),
        }
    }

    /// Применить все накопленные Write-изменения обратно в Column.
    ///
    /// Вызывается автоматически при Drop итератора или явно из `rhai_api`.
    pub fn flush_writes(&mut self) {
        if self.pending_writes.is_empty() {
            return;
        }

        let ctx_ref = self.ctx.borrow();
        // SAFETY: итератор завершён (flush_writes вызывается в Drop или
        // после исчерпания итератора), никаких shared borrow на Column нет.
        let world_ref = ctx_ref.world_ref();
        let world_ptr = world_ref as *const World;

        for (arch_idx, row, type_name, dynamic) in self.pending_writes.drain(..) {
            let binding = match ctx_ref.binding(&type_name) {
                Some(b) => b,
                None    => continue,
            };

            // SAFETY: arch_idx и row валидны — получены из того же world.
            unsafe {
                let world = world_ptr.as_ref().unwrap_unchecked();
                let arch  = &world.archetypes()[arch_idx];
                if let Some(col_idx) = arch.column_index(binding.id) {
                    // Получаем указатель на данные в колонке
                    let col = &world.archetypes()[arch_idx].columns_raw()[col_idx];
                    let ptr = col.get_raw_ptr(row) as *mut u8;
                    (binding.write)(ptr, &dynamic);
                }
            }
        }
    }
}

impl Drop for RhaiQueryIter {
    fn drop(&mut self) {
        self.flush_writes();
    }
}

// ── Iterator impl ──────────────────────────────────────────────

impl Iterator for RhaiQueryIter {
    type Item = Dynamic;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Клонируем arch_state чтобы избежать borrow conflict
            // между immutable borrow (arch_state) и mutable borrow (self.build_item)
            let arch_state = self.arch_states.get(self.arch_cursor)?;
            let arch_idx   = arch_state.arch_idx;
            let len        = arch_state.len;
            let components = arch_state.components.clone();

            if self.row_cursor >= len {
                self.arch_cursor += 1;
                self.row_cursor  = 0;
                continue;
            }

            let row = self.row_cursor;
            self.row_cursor += 1;

            let item = self.build_item(arch_idx, &components, row);
            return Some(item);
        }
    }
}

// ── Построение элементов ───────────────────────────────────────

impl RhaiQueryIter {
    fn build_item(&mut self, arch_idx: usize, components: &[ComponentState], row: usize) -> Dynamic {
        let ctx_ref  = self.ctx.borrow();
        let world    = ctx_ref.world_ref();
        let arch     = &world.archetypes()[arch_idx];

        let mut map = rhai::Map::new();

        // Entity index как i64
        let entity: Entity = arch.entities()[row];
        map.insert("entity".into(), Dynamic::from_int(entity.index() as rhai::INT));

        for comp in components {
            let binding = match ctx_ref.binding(&comp.type_name) {
                Some(b) => b,
                None    => continue,
            };

            // SAFETY: col_idx и row валидны в пределах этого arch
            let dynamic = unsafe {
                let col = &arch.columns_raw()[comp.col_idx];
                let ptr = col.get_raw_ptr(row);
                (binding.read)(ptr)
            };

            // Для Write-компонентов запоминаем arch_idx и row чтобы flush мог найти колонку
            if comp.write {
                // Помещаем clone в map, оригинал в pending_writes
                let clone = dynamic.clone();
                self.pending_writes.push((arch_idx, row, comp.type_name.clone(), dynamic));
                map.insert(
                    comp.type_name.to_lowercase().into(),
                    clone,
                );
            } else {
                map.insert(
                    comp.type_name.to_lowercase().into(),
                    dynamic,
                );
            }
        }

        Dynamic::from_map(map)
    }
}

// ── Вспомогательные функции ────────────────────────────────────

fn build_arch_states(world: &World, ctx: &ScriptContext, descs: &[QueryDesc]) -> Vec<ArchState> {
    // Разрешаем имена типов → ComponentId через binding реестр
    let resolved: Vec<Option<(ComponentId, &QueryDesc)>> = descs.iter()
        .map(|d| ctx.binding(&d.type_name).map(|b| (b.id, d)))
        .collect();

    // Если хотя бы одно имя не разрешено — возвращаем пустой итератор
    if resolved.iter().any(|r| r.is_none()) {
        return Vec::new();
    }
    let resolved: Vec<(ComponentId, &QueryDesc)> = resolved.into_iter()
        .map(|r| r.unwrap())
        .collect();

    world.archetypes()
        .iter()
        .enumerate()
        .filter_map(|(arch_idx, arch)| {
            if arch.is_empty() { return None; }

            // Архетип должен содержать ВСЕ запрошенные компоненты
            let components: Vec<ComponentState> = resolved.iter()
                .filter_map(|(cid, desc)| {
                    let col_idx = arch.column_index(*cid)?;
                    Some(ComponentState {
                        col_idx,
                        type_name: desc.type_name.clone(),
                        write:     desc.write,
                        comp_id:   *cid,
                    })
                })
                .collect();

            if components.len() != resolved.len() {
                return None;
            }

            Some(ArchState {
                arch_idx,
                len: arch.len(),
                components,
            })
        })
        .collect()
}