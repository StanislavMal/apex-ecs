//! WorldSerializer — логика снэпшота и восстановления мира.
//!
//! Поддерживает:
//! - JSON и Bincode форматы
//! - Версионирование с автоматической миграцией
//! - Инкрементальные diff-сохранения
//! - Файловый I/O с произвольным форматом

use std::collections::{HashMap, HashSet};
use std::path::Path;

use apex_core::{
    component::ComponentId,
    entity::Entity,
    relations::ChildOf,
    world::World,
};

use crate::prefab::{PrefabChild, PrefabComponent, PrefabManifest};
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

    #[error("entity with index {index} not found in world")]
    EntityNotFound { index: u32 },
}

// ── RestoreEntityMap ───────────────────────────────────────────

/// Маппинг старых index → новые Entity, возвращается из `restore`.
pub type RestoreEntityMap = HashMap<u32, Entity>;

// ── WorldSerializer ────────────────────────────────────────────

pub struct WorldSerializer;

impl WorldSerializer {
    // ── Snapshot ───────────────────────────────────────────────

    /// Создать полный снэпшот мира. Обычная (де)сериализация без резолва внешних ссылок — для сцен с
    /// компонентами, ссылающимися на ассеты/entity, используйте [`snapshot_with`](Self::snapshot_with).
    pub fn snapshot(world: &World) -> Result<WorldSnapshot, SerializationError> {
        Self::snapshot_with(world, &mut apex_core::NoContext)
    }

    /// Снэпшот с **контекстом (де)сериализации** (TD-44): контекст-зависимые компоненты (Handle ассета,
    /// Entity-референс) резолвят ссылки через `ctx` (его реализует движок/редактор). Обычные компоненты
    /// `ctx` игнорируют ⇒ результат идентичен [`snapshot`](Self::snapshot).
    pub fn snapshot_with(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<WorldSnapshot, SerializationError> {
        Self::snapshot_with_filter(world, ctx, &|_| true)
    }

    /// [`snapshot_with`](Self::snapshot_with), restricted to the entities `keep` returns `true` for.
    /// The caller decides what belongs in the document — e.g. a scene editor keeps only entities that
    /// carry its stable-id component, excluding editor infrastructure (camera/grid) that shares the
    /// scene world but is not saved content. apex-ecs stays policy-free: `keep` is host-supplied.
    pub fn snapshot_with_filter(
        world: &World,
        ctx: &mut dyn apex_core::SerdeContext,
        keep: &dyn Fn(apex_core::Entity) -> bool,
    ) -> Result<WorldSnapshot, SerializationError> {
        let tick = world.current_tick().0;
        let mut snap = WorldSnapshot::new(tick);
        // Entity indices that made it into the snapshot — used to drop relations whose subject/target was
        // filtered out (e.g. a scene instance's inner nodes), which would otherwise warn + skip on restore.
        let mut kept: HashSet<u32> = HashSet::new();

        // ── Entities + Components ──────────────────────────────
        for arch in world.archetypes() {
            if arch.is_empty() { continue; }

            for (row, &entity) in arch.entities().iter().enumerate() {
                if !keep(entity) {
                    continue;
                }
                kept.insert(entity.index());
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

                    // Компоненты без serde пропускаем
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        None    => continue,
                    };

                    // ZST-маркер (Folder/Model/Hidden/…): байтов нет, но его ПРИСУТСТВИЕ и есть данные —
                    // пишем пустой снэпшот, чтобы restore переинсертил компонент. (Раньше `continue`
                    // молча терял маркер-компоненты на save/load.)
                    if info.size == 0 {
                        entity_snap
                            .components
                            .push(ComponentSnapshot::new_json(info.name.to_string(), Vec::new()));
                        continue;
                    }

                    let raw_bytes = unsafe { (serde_fns.serialize_fn)(col.get_raw_ptr(row), ctx) }
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
        // SubjectIndex — источник истины: записи чистятся при despawn,
        // поэтому iter_relations отдаёт только связи живых entity.
        for (subject_index, kind_idx, target) in world.iter_relations() {
            // Only keep relations between snapshotted entities — a relation to a filtered-out entity
            // (e.g. a scene instance's inner node) can't restore, so don't save it (avoids restore warns).
            if !kept.contains(&subject_index) || !kept.contains(&target.index()) {
                continue;
            }
            let kind_name = world.relation_registry()
                .get_name(kind_idx)
                .unwrap_or("<unknown>")
                .to_string();

            snap.relations.push(RelationSnapshot {
                subject_index,
                target_index: target.index(),
                kind_name,
            });
        }

        Ok(snap)
    }

    // ── Restore ────────────────────────────────────────────────

    /// Восстановить мир из снэпшота. Обычная (де)сериализация — для компонентов с внешними ссылками см.
    /// [`restore_with`](Self::restore_with).
    ///
    /// Перед вызовом можно вызвать `snapshot.migrate()` если версия устарела.
    pub fn restore(
        world:    &mut World,
        snapshot: &WorldSnapshot,
    ) -> Result<RestoreEntityMap, SerializationError> {
        Self::restore_with(world, snapshot, &mut apex_core::NoContext)
    }

    /// Восстановить мир из снэпшота с **контекстом (де)сериализации** (TD-44): контекст-зависимые
    /// компоненты резолвят внешние ссылки через `ctx`. Обычные компоненты `ctx` игнорируют.
    pub fn restore_with(
        world:    &mut World,
        snapshot: &WorldSnapshot,
        ctx:      &mut dyn apex_core::SerdeContext,
    ) -> Result<RestoreEntityMap, SerializationError> {
        if snapshot.version != WorldSnapshot::CURRENT_VERSION {
            return Err(SerializationError::VersionMismatch {
                expected: WorldSnapshot::CURRENT_VERSION,
                found:    snapshot.version,
            });
        }

        let mut entity_map: RestoreEntityMap = HashMap::with_capacity(snapshot.entities.len());
        // Mark all restored data as **freshly changed at load time**: bump the change tick and stamp the
        // inserted components with the new current tick. Otherwise restore stamped them with the SAVED
        // snapshot tick (older), which a change-detecting consumer (transform propagation, render
        // extract, picking) whose `last_run` is already past it would skip — e.g. transforms staying at
        // the world origin (and lights mis-aimed) after loading a scene into a long-running world. This
        // touches only the (rare) load path; the per-frame change-detection model is unchanged.
        world.advance_change_tick();
        let tick = world.current_tick();

        // Строим маппинг type_name → ComponentId из зарегистрированных компонентов.
        let name_to_id: HashMap<String, ComponentId> = world
            .registry()
            .iter()
            .map(|info| (info.name.to_string(), info.id))
            .collect();

        // ── Шаг 1: Entity + компоненты ────────────────────────
        for entity_snap in &snapshot.entities {
            let new_entity = world.spawn(());
            entity_map.insert(entity_snap.original_index, new_entity);

            for comp_snap in &entity_snap.components {
                let component_id = match name_to_id.get(&comp_snap.type_name) {
                    Some(&id) => id,
                    None      => return Err(SerializationError::ComponentNotRegistered {
                        type_name: comp_snap.type_name.clone(),
                    }),
                };

                // ZST-маркер: байтов нет, восстанавливаем по присутствию (пустой инсерт), не вызывая
                // deserialize_fn (данные пусты — парсить нечего). Парный к ZST-ветке в snapshot.
                if world.registry().get_info(component_id).map(|i| i.size).unwrap_or(0) == 0 {
                    world.insert_raw_pub(new_entity, component_id, Vec::new(), tick);
                    continue;
                }

                // Десериализуем в отдельном scope
                let component_bytes = {
                    let info = world.registry().get_info(component_id).unwrap();
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        // §0.2a (E8): the type is registered but has no serde
                        // functions, so its snapshot bytes are silently dropped
                        // on restore — the entity comes back missing this
                        // component. Surface it (throttled) so the data loss is
                        // visible; register serde for the type to fix.
                        None => {
                            apex_core::warn_once!(
                                "restore: component '{}' is registered without serde functions — snapshot data dropped, entity restored without it",
                                comp_snap.type_name,
                            );
                            continue;
                        }
                    };

                    // Данные уже в нужном формате — используем как есть
                    let raw = &comp_snap.data;

                    (serde_fns.deserialize_fn)(raw, ctx)
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
                world.add_relation_by_kind_idx(subject, kind_idx, target);
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
        Self::diff_with(old_snapshot, new_world, &mut apex_core::NoContext)
    }

    /// `diff` с **контекстом (де)сериализации** (TD-44): внутренний снэпшот строится через
    /// [`snapshot_with`](Self::snapshot_with), поэтому контекст-зависимые компоненты резолвят внешние
    /// ссылки и в инкрементальном сейве — консистентно с полным `snapshot_with` (нет тихого `NoContext`).
    pub fn diff_with(
        old_snapshot: &WorldSnapshot,
        new_world:    &World,
        ctx:          &mut dyn apex_core::SerdeContext,
    ) -> Result<WorldDiff, SerializationError> {
        let new_snapshot = Self::snapshot_with(new_world, ctx)?;
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
                    // Существующая — сравниваем компоненты с byte-level delta
                    let old_comps: HashMap<&str, &ComponentSnapshot> = old_entity.components
                        .iter()
                        .map(|c| (c.type_name.as_str(), c))
                        .collect();

                    let mut added = Vec::new();
                    let mut removed = Vec::new();
                    let mut modified = Vec::new();

                    for new_comp in &new_entity.components {
                        match old_comps.get(new_comp.type_name.as_str()) {
                            None => added.push(new_comp.clone()),
                            Some(old_comp) => {
                                // Byte-level delta: сравниваем данные компонента
                                if new_comp.data != old_comp.data {
                                    modified.push(new_comp.clone());
                                }
                                // Если данные совпадают — не включаем в diff
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
                    if !modified.is_empty() {
                        diff.modified_components.push((new_entity.original_index, modified));
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

        // Применяем modified компоненты (byte-level delta) — заменяем старые версии новыми
        for (entity_idx, components) in &diff.modified_components {
            if let Some(entity) = result.entities.iter_mut().find(|e| e.original_index == *entity_idx) {
                for new_comp in components {
                    if let Some(old) = entity.components.iter_mut().find(|c| c.type_name == new_comp.type_name) {
                        old.data = new_comp.data.clone();
                        old.format = new_comp.format;
                    }
                }
            }
        }

        // Добавляем relations
        result.relations.extend(diff.added_relations.clone());

        Ok(result)
    }

    // ── Сохранение на диск ────────────────────────────────────

    /// Atomically write `data` to `path` (§0.2a hygiene).
    ///
    /// A plain `fs::write` truncates the target first, so a crash or interrupt
    /// mid-write leaves a corrupt (partial) save. Instead write to a sibling
    /// `.tmp` file, flush it, then rename over the target — a reader never
    /// observes a half-written file, and rename is atomic within one directory.
    fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        // std::fs::rename replaces an existing destination atomically on both
        // Unix and Windows (MOVEFILE_REPLACE_EXISTING).
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp); // don't leak the temp on failure
            return Err(e);
        }
        Ok(())
    }

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
        Self::atomic_write(path, &data)?;
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
        Self::atomic_write(path, &data)?;
        Ok(())
    }

    /// Прочитать diff из файла.
    pub fn read_diff_from_file(path: &Path) -> Result<WorldDiff, SerializationError> {
        let data = std::fs::read(path)?;
        let diff = WorldDiff::from_bincode(&data)?;
        Ok(diff)
    }

    // ── Prefab export ─────────────────────────────────────────────

    /// Создать [`PrefabManifest`] из одной entity.
    ///
    /// Сериализуются только компоненты, зарегистрированные через
    /// `world.register_component_serde::<T>()`. Relation-компоненты пропускаются; ZST-маркеры
    /// сохраняются по присутствию (`value: null`) и переинсертятся при instantiate.
    pub fn entity_to_prefab(
        world: &World,
        entity: Entity,
    ) -> Result<PrefabManifest, SerializationError> {
        Self::entity_to_prefab_with(world, entity, &mut apex_core::NoContext)
    }

    /// `entity_to_prefab` с **контекстом (де)сериализации** (TD-44): компоненты с внешними ссылками
    /// резолвят их в префаб через `ctx` — консистентно со снэпшотами. Обычные компоненты `ctx` игнорируют.
    pub fn entity_to_prefab_with(
        world: &World,
        entity: Entity,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<PrefabManifest, SerializationError> {
        let location = world
            .entity_allocator()
            .get_location(entity)
            .ok_or(SerializationError::EntityNotFound { index: entity.index() })?;

        let arch = &world.archetypes()[location.archetype_id.as_usize()];
        let mut components = Vec::new();

        for col in arch.columns() {
            let cid: apex_core::component::ComponentId = col.id();
            let info = match world.registry().get_info(cid) {
                Some(i) => i,
                None => continue,
            };

            // Компоненты без serde пропускаем
            let serde_fns = match &info.serde {
                Some(s) => s,
                None => continue,
            };

            // ZST-маркер (Folder/Model/Hidden/…): данных нет, но присутствие значимо — пишем `null`,
            // чтобы instantiate переинсертил компонент (его deserialize_fn даёт unit из `null`). Раньше
            // `continue` молча терял маркеры на prefab/capture-пути (как и в снэпшотах).
            if info.size == 0 {
                components.push(PrefabComponent {
                    type_name: info.name.to_string(),
                    value: serde_json::Value::Null,
                });
                continue;
            }

            // Сериализуем сырые данные компонента в байты через контекст (TD-44) — консистентно со снэпшотами.
            let raw_bytes =
                unsafe { (serde_fns.serialize_fn)(col.get_raw_ptr(location.row as usize), ctx) }
                    .map_err(|e| SerializationError::SerializeFailed {
                        type_name: info.name.to_string(),
                        reason: e.to_string(),
                    })?;

            // Парсим JSON-байты в serde_json::Value
            let json_value: serde_json::Value = serde_json::from_slice(&raw_bytes)?;

            components.push(PrefabComponent {
                type_name: info.name.to_string(),
                value: json_value,
            });
        }

        Ok(PrefabManifest {
            name: format!("entity_{}", entity.index()),
            components,
            children: Vec::new(),
        })
    }

    /// Создать [`PrefabManifest`] из entity и всей её иерархии детей.
    ///
    /// Рекурсивно обходит детей через `ChildOf` relation, создавая
    /// вложенные `PrefabChild` записи.
    pub fn hierarchy_to_prefab(
        world: &World,
        root: Entity,
    ) -> Result<PrefabManifest, SerializationError> {
        Self::hierarchy_to_prefab_with(world, root, &mut apex_core::NoContext)
    }

    /// `hierarchy_to_prefab` с **контекстом (де)сериализации** (TD-44): контекст прокидывается во всю
    /// иерархию (каждый узел — через [`entity_to_prefab_with`](Self::entity_to_prefab_with)).
    pub fn hierarchy_to_prefab_with(
        world: &World,
        root: Entity,
        ctx: &mut dyn apex_core::SerdeContext,
    ) -> Result<PrefabManifest, SerializationError> {
        let mut manifest = Self::entity_to_prefab_with(world, root, ctx)?;

        // Рекурсивно собираем детей как ВСТРОЕННЫЕ (inline) поддеревья — так префаб самодостаточен:
        // один файл инстанцируется без предзагрузки под-префабов (раньше клалось только имя ребёнка, и
        // сам суб-манифест терялся ⇒ `instantiate` падал с `SubPrefabNotFound`).
        let children: Vec<Entity> = world.children_of(ChildOf, root).collect();
        for child in children {
            let child_manifest = Self::hierarchy_to_prefab_with(world, child, ctx)?;
            manifest.children.push(PrefabChild::Inline(child_manifest));
        }

        Ok(manifest)
    }
}

// ── Тесты ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::prelude::*;
    use serde::{Deserialize, Serialize};
    

    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Position { x: f32, y: f32 }

    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
    struct Health { current: f32, max: f32 }

    #[derive(Component)]
    #[allow(dead_code)]
    struct RenderHandle(u64);

    /// Zero-sized marker (like the editor's `Folder`/`Model`/`Hidden`): presence is the only state.
    #[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Marker;

    #[test]
    fn zst_marker_survives_snapshot_restore() {
        let mut world = World::new();
        world.register_component_serde_json::<Marker>();
        world.register_component_serde_json::<Position>();
        let e = world.spawn((Marker, Position { x: 1.0, y: 2.0 }));

        let snap = WorldSerializer::snapshot(&world).unwrap();
        let mut w2 = World::new();
        w2.register_component_serde_json::<Marker>();
        w2.register_component_serde_json::<Position>();
        let map = WorldSerializer::restore(&mut w2, &snap).unwrap();

        let restored = map[&e.index()];
        assert!(
            w2.get::<Marker>(restored).is_some(),
            "a zero-sized marker component must survive snapshot/restore by presence"
        );
        assert_eq!(w2.get::<Position>(restored).map(|p| p.x), Some(1.0));
    }

    /// §0.2a (E8): if a component's type is registered on the restore side but
    /// WITHOUT serde functions (e.g. version skew), its snapshot bytes are
    /// dropped and the entity comes back missing it. The drop must be loud
    /// (`warn_once!`, verified by the apex-core macro test) rather than silent;
    /// here we pin the data-loss contract — restore still succeeds, serde-backed
    /// components come back, the serde-less one is absent.
    #[test]
    fn restore_drops_component_registered_without_serde() {
        let mut world = World::new();
        world.register_component_serde_json::<Position>();
        world.register_component_serde_json::<Health>();
        let e = world.spawn((
            Position { x: 5.0, y: 6.0 },
            Health { current: 50.0, max: 100.0 },
        ));
        let snap = WorldSerializer::snapshot(&world).unwrap();

        // Restore side: Position has serde, Health is registered as a plain
        // component (no serde) — the E8 branch.
        let mut w2 = World::new();
        w2.register_component_serde_json::<Position>();
        w2.register_component::<Health>();
        let map = WorldSerializer::restore(&mut w2, &snap).unwrap();

        let restored = map[&e.index()];
        assert_eq!(
            w2.get::<Position>(restored),
            Some(&Position { x: 5.0, y: 6.0 }),
            "serde-backed component must restore",
        );
        assert!(
            w2.get::<Health>(restored).is_none(),
            "component registered without serde must be dropped on restore (E8)",
        );
    }

    fn setup_world() -> World {
        let mut world = World::new();
        world.register_component::<RenderHandle>();
        world.register_component_serde_json::<Position>();
        world.register_component_serde_json::<Health>();

        let e1 = world.spawn((
            Position { x: 10.0, y: 20.0 },
            Health { current: 100.0, max: 100.0 },
        ));
        world.insert(e1, RenderHandle(42));

        let e2 = world.spawn((
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
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();

        // Регистрируем ChildOf, чтобы restore нашёл kind
        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
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
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();

        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
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
        let _e3 = world.spawn((
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
        let e1 = world.spawn((Position { x: 1.0, y: 2.0 },));
        let e1_idx = e1.index();
        let _e2 = world.spawn((Position { x: 3.0, y: 4.0 },));

        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Удаляем e1
        world.despawn(e1);

        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert_eq!(diff.removed_entities, vec![e1_idx]);
    }

    #[test]
    fn write_read_file() {
        
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

    /// Atomic save: replacing an existing file works and leaves no `.tmp`
    /// sibling behind (the write goes through temp+rename).
    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        let dir = std::env::temp_dir().join("apex_serialization_atomic_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("save.json");

        // Pre-existing content that must be atomically replaced.
        std::fs::write(&path, b"STALE PARTIAL DATA").unwrap();
        WorldSerializer::write_to_file(&path, &snap, SaveFormat::Json).unwrap();

        let loaded = WorldSerializer::read_from_file(&path).unwrap();
        assert_eq!(loaded.entities.len(), snap.entities.len());
        assert!(
            !dir.join("save.json.tmp").exists(),
            "atomic write must not leave a .tmp file behind on success"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_apply_roundtrip() {
        let mut world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Модифицируем мир
        let _e3 = world.spawn((
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
    fn diff_byte_level_delta_modified_component() {
        

        let mut world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Находим entity с Health через итерацию по архетипам
        let health_id = world.registry().get_id::<Health>().unwrap();
        let e1 = *world.archetypes().iter()
            .filter(|a| a.column_index(health_id).is_some())
            .flat_map(|a| a.entities().iter())
            .next()
            .unwrap();
        *world.get_mut::<Health>(e1).unwrap() = Health { current: 50.0, max: 100.0 };

        // Diff — компонент должен попасть в modified_components, а НЕ в added_components
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert!(diff.added_entities.is_empty(), "нет новых entity");
        assert!(diff.added_components.is_empty(), "нет добавленных компонентов");
        assert_eq!(diff.modified_components.len(), 1, "один entity с изменённым компонентом");
        assert_eq!(diff.modified_components[0].1.len(), 1, "один изменённый компонент");
        assert_eq!(diff.modified_components[0].1[0].type_name, "apex_serialization::serializer::tests::Health");

        // Применяем diff к base snapshot — данные должны обновиться
        let new_snap = WorldSerializer::apply_diff_to_snapshot(&old_snap, &diff).unwrap();
        let health_snap = new_snap.entities.iter()
            .find(|e| e.original_index == diff.modified_components[0].0)
            .and_then(|e| e.components.iter().find(|c| c.type_name == "apex_serialization::serializer::tests::Health"))
            .unwrap();
        // Проверяем что данные изменились: старые данные (100.0) → новые (50.0)
        let health_str = String::from_utf8_lossy(&health_snap.data);
        assert!(health_str.contains("50.0"), "Health.current должен быть 50.0, получено: {health_str}");
    }

    #[test]
    fn diff_unchanged_component_excluded() {
        let world = setup_world();
        let old_snap = WorldSerializer::snapshot(&world).unwrap();

        // Ничего не меняем — diff должен быть пустым (все компоненты совпадают)
        let diff = WorldSerializer::diff(&old_snap, &world).unwrap();
        assert!(diff.is_empty(), "diff должен быть пустым для неизменённого мира");
        assert!(diff.modified_components.is_empty(), "нет изменённых компонентов");
    }

    #[test]
    fn restored_components_are_added_and_changed_for_a_prior_base() {
        // Picking/extract consumers gate rebuilds on Changed<T>/Added<T> with a base captured before a
        // scene load. Loading OVER a same-count scene leaves the cheap "count" signal unchanged, so
        // correctness hinges on restore making the loaded data look freshly Added+Changed against that
        // prior base (TD-52 restore-tick fix). Mirror of the picking BVH gate, minus the BVH.
        use apex_core::query::{Added, Changed, Query};

        let mut world = setup_world();
        let snap = WorldSerializer::snapshot(&world).unwrap();

        // Long-running editor: a consumer's base sits high before the load.
        for _ in 0..10 {
            world.advance_change_tick();
        }
        let base = world.current_tick();

        // File→Open over the current scene: clear + restore (same component set ⇒ same count).
        world.clear_entities();
        WorldSerializer::restore(&mut world, &snap).unwrap();

        let changed = Query::<(Changed<Position>,)>::new_with_tick(&world, base).iter().count();
        let added = Query::<(Added<Position>,)>::new_with_tick(&world, base).iter().count();
        assert!(changed > 0, "restored components must be Changed for a base predating the load");
        assert!(added > 0, "restored components must be Added for a base predating the load");
    }

    #[test]
    fn restore_with_migration() {
        let world = setup_world();
        let mut snap = WorldSerializer::snapshot(&world).unwrap();

        // Симулируем старую версию
        snap.version = 1;

        // Миграция (v1 → v1 — no-op, так как v1 и есть CURRENT)
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);

        // Restore после миграции
        let mut restored_world = World::new();
        restored_world.register_component::<RenderHandle>();
        restored_world.register_component_serde_json::<Position>();
        restored_world.register_component_serde_json::<Health>();
        let p = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        let c = restored_world.spawn((Position { x: 0.0, y: 0.0 },));
        restored_world.add_relation(c, apex_core::relations::ChildOf, p);

        let result = WorldSerializer::restore(&mut restored_world, &snap);
        assert!(result.is_ok());
    }

    /// TD-44: a component holding an **external reference** (here an id remapped by the host) is
    /// (de)serialised through a host-provided [`SerdeContext`] — the mechanism a scene editor uses to
    /// turn a `Handle<Asset>` into a stable path and back. apex-ecs stays asset-agnostic: the context
    /// type lives here (== the engine/editor in production); the core only threads `&mut dyn SerdeContext`.
    #[test]
    fn context_aware_component_resolves_via_serde_context() {
        use apex_core::{ComponentSerdeFns, SerdeContext};
        use std::any::Any;

        // Host-side context: remaps the external id by an offset (stands in for Handle↔path resolution).
        struct OffsetCtx {
            offset: u64,
        }
        impl SerdeContext for OffsetCtx {
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn Any {
                self
            }
        }

        #[derive(Component, Debug, PartialEq)]
        struct ExternalRef(u64);

        let fns = ComponentSerdeFns {
            serialize_fn: |ptr, ctx| {
                let r = unsafe { &*(ptr as *const ExternalRef) };
                let off = ctx.as_any().downcast_ref::<OffsetCtx>().map_or(0, |c| c.offset);
                Ok((r.0 + off).to_le_bytes().to_vec()) // store the host-resolved id
            },
            deserialize_fn: |bytes, ctx| {
                let off = ctx.as_any().downcast_ref::<OffsetCtx>().map_or(0, |c| c.offset);
                let stored = u64::from_le_bytes(bytes.try_into().unwrap());
                let r = ExternalRef(stored - off); // resolve back via the same context
                let size = std::mem::size_of::<ExternalRef>();
                let mut buf = vec![0u8; size];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &r as *const ExternalRef as *const u8,
                        buf.as_mut_ptr(),
                        size,
                    );
                }
                std::mem::forget(r);
                Ok(buf)
            },
            format: "bincode",
        };

        let mut world = World::new();
        world.register_component_serde_with::<ExternalRef>(fns.clone());
        let e = world.spawn((ExternalRef(7),));

        // Snapshot WITH a context that offsets by 1000 ⇒ the stored value is the resolved id (1007),
        // proving the context reached the component's serialize_fn.
        let mut sctx = OffsetCtx { offset: 1000 };
        let snap = WorldSerializer::snapshot_with(&world, &mut sctx).unwrap();
        let stored = u64::from_le_bytes(snap.entities[0].components[0].data[..8].try_into().unwrap());
        assert_eq!(stored, 1007, "serialize_fn saw the SerdeContext (resolved 7 → 1007)");

        // Restore WITH the same context ⇒ resolves back to 7 (context threaded both directions).
        let mut rworld = World::new();
        rworld.register_component_serde_with::<ExternalRef>(fns);
        let mut rctx = OffsetCtx { offset: 1000 };
        let map = WorldSerializer::restore_with(&mut rworld, &snap, &mut rctx).unwrap();
        let restored = map[&e.index()];
        assert_eq!(
            rworld.get::<ExternalRef>(restored),
            Some(&ExternalRef(7)),
            "restore_with threaded the context ⇒ external ref resolved back to 7"
        );
    }
}
