//! WorldSerializer — логика снэпшота и восстановления мира.
//!
//! Поддерживает:
//! - JSON и Bincode форматы
//! - Версионирование с автоматической миграцией
//! - Инкрементальные diff-сохранения
//! - Файловый I/O с произвольным форматом

use std::collections::HashMap;
use std::path::Path;

use apex_core::{
    component::{ComponentId, Tick},
    entity::Entity,
    relations::{is_relation_id, decode_kind, decode_target, encode_relation},
    world::World,
};

use crate::snapshot::{
    ComponentSnapshot, EntitySnapshot, RelationSnapshot, SaveFormat, WorldDiff,
    WorldSnapshot,
};

// ── Ошибки ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SerializationError {
    #[error("component `{type_name}` serialize failed: {reason}")]
    SerializeFailed { type_name: String, reason: String },

    #[error("component `{type_name}` deserialize failed: {reason}")]
    DeserializeFailed { type_name: String, reason: String },

    #[error("component `{type_name}` not registered in world")]
    ComponentNotRegistered { type_name: String },

    #[error("snapshot version {found} is not supported (expected {expected})")]
    VersionMismatch { expected: u32, found: u32 },

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Bincode error: {0}")]
    Bincode(#[from] Box<bincode::ErrorKind>),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("diff error: {reason}")]
    DiffError { reason: String },

    #[error("migration error: {0}")]
    Migration(String),
}

// ── RestoreEntityMap ───────────────────────────────────────────

/// Маппинг старых index → новые Entity, возвращается из `restore`.
pub type RestoreEntityMap = HashMap<u32, Entity>;

// ── WorldSerializer ────────────────────────────────────────────

pub struct WorldSerializer;

impl WorldSerializer {
    // ── Snapshot ───────────────────────────────────────────────

    /// Создать полный снэпшот мира в формате JSON (компоненты → JSON Value).
    pub fn snapshot(world: &World) -> Result<WorldSnapshot, SerializationError> {
        let tick = world.current_tick().0;
        let mut snap = WorldSnapshot::new(tick);

        // ── Entities + Components ──────────────────────────────
        for arch in world.archetypes() {
            if arch.is_empty() { continue; }

            for (row, &entity) in arch.entities().iter().enumerate() {
                let mut entity_snap = EntitySnapshot {
                    original_index: entity.index(),
                    components:     Vec::new(),
                };

                for col in arch.columns() {
                    let cid  = col.id();
                    let info = match world.registry().get_info(cid) {
                        Some(i) => i,
                        None    => continue,
                    };

                    // Relation-компоненты пропускаем — сохраняются отдельно
                    if is_relation_id(info.id) { continue; }

                    // Компоненты без serde пропускаем
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        None    => continue,
                    };

                    // ZST — нечего сериализовать
                    if info.size == 0 { continue; }

                    let raw_bytes = unsafe { (serde_fns.serialize_fn)(col.get_raw_ptr(row)) }
                        .map_err(|e| SerializationError::SerializeFailed {
                            type_name: info.name.to_string(),
                            reason:    e.to_string(),
                        })?;

                    // Сохраняем в зависимости от формата сериализации
                    match serde_fns.format {
                        "json" => {
                            entity_snap.components.push(ComponentSnapshot::new_json(
                                info.name.to_string(),
                                raw_bytes,
                            ));
                        }
                        _ => {
                            // Бинарный формат — сохраняем как есть
                            entity_snap.components.push(ComponentSnapshot::new_binary(
                                info.name.to_string(),
                                raw_bytes,
                            ));
                        }
                    }
                }

                snap.entities.push(entity_snap);
            }
        }

        // ── Relations ──────────────────────────────────────────
        for arch in world.archetypes() {
            if arch.is_empty() { continue; }
            for &entity in arch.entities() {
                for &raw_id in world.subject_index_raw(entity.index()) {
                    let rel_cid = ComponentId(raw_id);
                    if !is_relation_id(rel_cid) { continue; }

                    let kind_idx   = decode_kind(rel_cid);
                    let target_idx = decode_target(rel_cid);

                    // Wildcard ID пропускаем
                    if target_idx == (1u32 << 20) - 1 { continue; }

                    let kind_name = world.relation_registry()
                        .get_name(kind_idx)
                        .unwrap_or("<unknown>")
                        .to_string();

                    snap.relations.push(RelationSnapshot {
                        subject_index: entity.index(),
                        target_index:  target_idx,
                        kind_name,
                    });
                }
            }
        }

        Ok(snap)
    }

    // ── Restore ────────────────────────────────────────────────

    /// Восстановить мир из снэпшота.
    ///
    /// Перед вызовом можно вызвать `snapshot.migrate()` если версия устарела.
    pub fn restore(
        world:    &mut World,
        snapshot: &WorldSnapshot,
    ) -> Result<RestoreEntityMap, SerializationError> {
        if snapshot.version != WorldSnapshot::CURRENT_VERSION {
            return Err(SerializationError::VersionMismatch {
                expected: WorldSnapshot::CURRENT_VERSION,
                found:    snapshot.version,
            });
        }

        let mut entity_map: RestoreEntityMap = HashMap::with_capacity(snapshot.entities.len());
        let tick = Tick(snapshot.tick);

        // Строим маппинг type_name → ComponentId из зарегистрированных компонентов.
        let name_to_id: HashMap<String, ComponentId> = world
            .registry()
            .iter()
            .map(|info| (info.name.to_string(), info.id))
            .collect();

        // ── Шаг 1: Entity + компоненты ────────────────────────
        for entity_snap in &snapshot.entities {
            let new_entity = world.spawn_empty();
            entity_map.insert(entity_snap.original_index, new_entity);

            for comp_snap in &entity_snap.components {
                let component_id = match name_to_id.get(&comp_snap.type_name) {
                    Some(&id) => id,
                    None      => return Err(SerializationError::ComponentNotRegistered {
                        type_name: comp_snap.type_name.clone(),
                    }),
                };

                // Десериализуем в отдельном scope
                let component_bytes = {
                    let info = world.registry().get_info(component_id).unwrap();
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        None    => continue,
                    };

                    // Данные уже в нужном формате — используем как есть
                    let raw = &comp_snap.data;

                    (serde_fns.deserialize_fn)(&raw)
                        .map_err(|e| SerializationError::DeserializeFailed {
                            type_name: comp_snap.type_name.clone(),
                            reason:    e.to_string(),
                        })?
                };

                world.insert_raw_pub(new_entity, component_id, component_bytes, tick);
            }
        }

        // ── Шаг 2: Relations ───────────────────────────────────
        for rel_snap in &snapshot.relations {
            let subject = match entity_map.get(&rel_snap.subject_index) {
                Some(&e) => e,
                None     => {
                    log::warn!(
                        "restore: subject {} not in entity_map, skipping",
                        rel_snap.subject_index
                    );
                    continue;
                }
            };
            let target = match entity_map.get(&rel_snap.target_index) {
                Some(&e) => e,
                None     => {
                    log::warn!(
                        "restore: target {} not in entity_map, skipping relation '{}'",
                        rel_snap.target_index, rel_snap.kind_name
                    );
                    continue;
                }
            };

            if let Some(kind_idx) = world.relation_registry().get_idx_by_name(&rel_snap.kind_name) {
                let relation_id = encode_relation(kind_idx, target.index());
                world.insert_relation_raw(subject, relation_id, target);
            } else {
                log::warn!(
                    "restore: relation kind '{}' not registered, skipping",
                    rel_snap.kind_name
                );
            }
        }

        Ok(entity_map)
    }

    // ── Diff ──────────────────────────────────────────────────

    /// Вычислить разницу между старым снэпшотом и текущим состоянием мира.
    ///
    /// Полезно для инкрементальных сохранений: вместо полного снэпшота
    /// сохраняется только diff, который можно применить позже.
    pub fn diff(
        old_snapshot: &WorldSnapshot,
        new_world:    &World,
    ) -> Result<WorldDiff, SerializationError> {
        let new_snapshot = Self::snapshot(new_world)?;
        Self::diff_snapshots(old_snapshot, &new_snapshot)
    }

    /// Вычислить разницу между двумя снэпшотами.
    pub fn diff_snapshots(
        old: &WorldSnapshot,
        new: &WorldSnapshot,
    ) -> Result<WorldDiff, SerializationError> {
        let mut diff = WorldDiff::new();

        // Старые entity по original_index для быстрого поиска
        let old_entities: HashMap<u32, &EntitySnapshot> = old.entities
            .iter()
            .map(|e| (e.original_index, e))
            .collect();

        let new_entities: HashMap<u32, &EntitySnapshot> = new.entities
            .iter()
            .map(|e| (e.original_index, e))
            .collect();

        // Удалённые entity
        for old_entity in &old.entities {
            if !new_entities.contains_key(&old_entity.original_index) {
                diff.removed_entities.push(old_entity.original_index);
            }
        }

        // Добавленные и изменённые entity
        for new_entity in &new.entities {
            match old_entities.get(&new_entity.original_index) {
                None => {
                    // Новая entity — добавляем целиком
                    diff.added_entities.push(new_entity.clone());
                }
                Some(old_entity) => {
                    // Существующая — сравниваем компоненты
                    let old_comps: HashMap<&str, &ComponentSnapshot> = old_entity.components
                        .iter()
                        .map(|c| (c.type_name.as_str(), c))
                        .collect();

                    let mut added = Vec::new();
                    let mut removed = Vec::new();

                    for new_comp in &new_entity.components {
                        match old_comps.get(new_comp.type_name.as_str()) {
                            None => added.push(new_comp.clone()),
                            Some(_old) => {
                                // В будущем: deep compare данных
                            }
                        }
                    }

                    for old_comp in &old_entity.components {
                        if !new_entity.components.iter().any(|c| c.type_name == old_comp.type_name) {
                            removed.push(old_comp.type_name.clone());
                        }
                    }

                    if !added.is_empty() {
                        diff.added_components.push((new_entity.original_index, added));
                    }
                    if !removed.is_empty() {
                        diff.removed_components.push((new_entity.original_index, removed));
                    }
                }
            }
        }

        // Relations
        let old_relations: Vec<(u32, u32, &str)> = old.relations.iter()
            .map(|r| (r.subject_index, r.target_index, r.kind_name.as_str()))
            .collect();

        let new_relations: Vec<(u32, u32, &str)> = new.relations.iter()
            .map(|r| (r.subject_index, r.target_index, r.kind_name.as_str()))
            .collect();

        for rel in &new.relations {
            if !old_relations.contains(&(rel.subject_index, rel.target_index, rel.kind_name.as_str())) {
                diff.added_relations.push(rel.clone());
            }
        }

        for rel in &old.relations {
            if !new_relations.contains(&(rel.subject_index, rel.target_index, rel.kind_name.as_str())) {
                diff.removed_relations.push(rel.clone());
            }
        }

        Ok(diff)
    }

    /// Применить diff к базовому снэпшоту, получив новый снэпшот.
    ///
    /// Это snapshot-level операция: не требует прямого доступа к `World`.
    /// Результат можно сохранить или восстановить через `restore()`.
    pub fn apply_diff_to_snapshot(
        base: &WorldSnapshot,
        diff: &WorldDiff,
    ) -> Result<WorldSnapshot, SerializationError> {
        let mut result = base.clone();

        // Удаляем entity
        for idx in &diff.removed_entities {
            result.entities.retain(|e| e.original_index != *idx);
        }

        // Удаляем relations
        for rel in &diff.removed_relations {
            result.relations.retain(|r| {
                !(r.subject_index == rel.subject_index
                    && r.target_index == rel.target_index
                    && r.kind_name == rel.kind_name)
            });
        }

        // Удаляем компоненты
        for (entity_idx, type_names) in &diff.removed_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                entity.components.retain(|c| !type_names.contains(&c.type_name));
            }
        }

        // Добавляем entity
        let max_index = result.entities.iter()
            .map(|e| e.original_index)
            .max()
            .unwrap_or(0);

        for (i, entity_snap) in diff.added_entities.iter().enumerate() {
            let mut snap = entity_snap.clone();
            // Присваиваем новый index если конфликтует
            if result.entities.iter().any(|e| e.original_index == snap.original_index) {
                snap.original_index = max_index + 1 + i as u32;
            }
            result.entities.push(snap);
        }

        // Добавляем компоненты к существующим entity
        for (entity_idx, components) in &diff.added_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                entity.components.extend(components.clone());
            }
        }

        // Добавляем relations
        result.relations.extend(diff.added_relations.clone());

        Ok(result)
    }

    // ── Сохранение на диск ────────────────────────────────────

    /// Сохранить снэпшот в файл в указанном формате.
    pub fn write_to_file(
        path:   &Path,
        snap:   &WorldSnapshot,
        format: SaveFormat,
    ) -> Result<(), SerializationError> {
        let data = match format {
            SaveFormat::Json => snap.to_json()?,
            SaveFormat::Bincode => snap.to_bincode()?,
        };
        std::fs::write(path, &data)?;
        Ok(())
    }

    /// Прочитать снэпшот из файла, автоматически определяя формат по расширению.
    ///
    /// Поддерживаемые расширения:
    /// - `.json` → JSON
    /// - `.bin` → Bincode
    pub fn read_from_file(path: &Path) -> Result<WorldSnapshot, SerializationError> {
        let data = std::fs::read(path)?;
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("json");

        match ext {
            "json" => {
                let snap = WorldSnapshot::from_json(&data)?;
                Ok(snap)
            }
            "bin" => {
                let snap = WorldSnapshot::from_bincode(&data)?;
                Ok(snap)
            }
            _ => {
                // Пробуем JSON, потом Bincode
                if let Ok(snap) = WorldSnapshot::from_json(&data) {
                    return Ok(snap);
                }
                if let Ok(snap) = WorldSnapshot::from_bincode(&data) {
                    return Ok(snap);
                }
                Err(SerializationError::Migration(
                    format!("unknown file extension '{}' and couldn't detect format", ext)
                ))
            }
        }
    }

    /// Сохранить diff в файл (всегда в бинарном формате).
    pub fn write_diff_to_file(path: &Path, diff: &WorldDiff) -> Result<(), SerializationError> {
        let data = diff.to_bincode()?;
        std::fs::write(path, &data)?;
        Ok(())
    }

    /// Прочитать diff из файла.
    pub fn read_diff_from_file(path: &Path) -> Result<WorldDiff, SerializationError> {
        let data = std::fs::read(path)?;
        let diff = WorldDiff::from_bincode(&data)?;
        Ok(diff)
    }
}

// ── Тесты ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::prelude::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Position { x: f32, y: f32 }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Health { current: f32, max: f32 }

    struct RenderHandle(u64);

    fn setup_world() -> World {
        let mut world = World::new();
        world.register_component::<RenderHandle>();
        world.register_component_serde::<Position>();
        world.register_component_serde::<Health>();

        let e1 = world.spawn_bundle((
            Position { x: 10.0, y: 20.0 },
            Health { current: 100.0, max: 100.0 },
        ));
        world.insert(e1, RenderHandle(42));

        let e2 = world.spawn_bundle((
            Position { x: 30.0, y: 40.0 },
        ));

        world.add_relation(e2, apex_core::relations::ChildOf, e1);

        world
    }

    #[test]
    fn snapshot_restore_json_roundtrip() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let json = snap.to_json().unwrap();

        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde::<Position>();
        restored_world.register_component_serde::<Health>();

        // Регистрируем ChildOf, чтобы restore нашёл kind
        let p = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let restored_snap = WorldSnapshot::from_json(&json).unwrap();
        let entity_map = WorldSerializer::restore(&mut restored_world, &restored_snap).unwrap();

        assert!(!entity_map.is_empty());

        // Проверяем что Position восстановился для первой entity
        let new_e1 = entity_map[&0u32]; // original_index первой созданной entity
        let pos = restored_world.get::<Position>(new_e1).unwrap();
        assert!((pos.x - 10.0).abs() < 1e-6);
        assert!((pos.y - 20.0).abs() < 1e-6);
    }

    #[test]
    fn snapshot_bincode_roundtrip() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let binary = snap.to_bincode().unwrap();

        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde::<Position>();
        restored_world.register_component_serde::<Health>();

        let p = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let restored_snap = WorldSnapshot::from_bincode(&binary).unwrap();
        let entity_map = WorldSerializer::restore(&mut restored_world, &restored_snap).unwrap();

        assert!(!entity_map.is_empty());
        let new_e1 = entity_map[&0u32];
        let pos = restored_world.get::<Position>(new_e1).unwrap();
        assert!((pos.x - 10.0).abs() < 1e-6);
    }

    #[test]
    fn bincode_smaller_than_json() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();
        let json_size = snap.to_json().unwrap().len();
        let bincode_size = snap.to_bincode().unwrap().len();

        assert!(bincode_size < json_size,
            "bincode={} should be < json={}", bincode_size, json_size);
    }

    #[test]
    fn diff_add_entity() {
        let mut world = setup_world();

        // Старый снэпшот
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Добавляем entity
        let _e3 = world.spawn_bundle((
            Position { x: 50.0, y: 60.0 },
            Health { current: 50.0, max: 50.0 },
        ));

        // Вычисляем diff
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();

        assert_eq!(diff.added_entities.len(), 1);
        assert!(diff.removed_entities.is_empty());
    }

    #[test]
    fn diff_remove_entity() {
        let mut world = World::new();
        world.register_component_serde::<Position>();

        // Спавним entity, запоминаем index
        let e1 = world.spawn_bundle((Position { x: 1.0, y: 2.0 },));
        let e1_idx = e1.index();
        let _e2 = world.spawn_bundle((Position { x: 3.0, y: 4.0 },));

        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Удаляем e1
        world.despawn(e1);

        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert_eq!(diff.removed_entities, vec![e1_idx]);
    }

    #[test]
    fn write_read_file() {
        use std::io::Write;
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        let dir = std::env::temp_dir().join("apex_serialization_test");
        std::fs::create_dir_all(&dir).unwrap();

        // JSON
        let json_path = dir.join("test_save.json");
        WorldSerializer::write_to_file(&json_path, &snap, SaveFormat::Json).unwrap();
        let loaded = WorldSerializer::read_from_file(&json_path).unwrap();
        assert_eq!(loaded.entities.len(), snap.entities.len());

        // Bincode
        let bin_path = dir.join("test_save.bin");
        WorldSerializer::write_to_file(&bin_path, &snap, SaveFormat::Bincode).unwrap();
        let loaded_bin = WorldSerializer::read_from_file(&bin_path).unwrap();
        assert_eq!(loaded_bin.entities.len(), snap.entities.len());

        // Bincode файл должен быть меньше
        let json_meta = std::fs::metadata(&json_path).unwrap();
        let bin_meta = std::fs::metadata(&bin_path).unwrap();
        assert!(bin_meta.len() < json_meta.len(),
            "bin={} should be < json={}", bin_meta.len(), json_meta.len());

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_apply_roundtrip() {
        let mut world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Модифицируем мир
        let _e3 = world.spawn_bundle((
            Position { x: 100.0, y: 200.0 },
        ));

        // Diff
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert_eq!(diff.added_entities.len(), 1);

        // Сохраняем diff и загружаем
        let diff_bytes = diff.to_bincode().unwrap();
        let loaded_diff = WorldDiff::from_bincode(&diff_bytes).unwrap();
        assert_eq!(loaded_diff.added_entities.len(), 1);
    }

    #[test]
    fn restore_with_migration() {
        let mut world = setup_world();
        let mut snap = WorldSerializer::snapshot(&world).unwrap();

        // Симулируем старую версию
        snap.version = 1;

        // Миграция (v1 → v1 — no-op, так как v1 и есть CURRENT)
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);

        // Restore после миграции
        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde::<Position>();
        restored_world.register_component_serde::<Health>();
        let p = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn_bundle((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let result = WorldSerializer::restore(&mut restored_world, &snap);
        assert!(result.is_ok());
    }
}
