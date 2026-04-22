//! WorldSerializer — логика снэпшота и восстановления мира.

use std::collections::HashMap;

use apex_core::{
    component::{ComponentId, Tick},
    entity::Entity,
    relations::{is_relation_id, decode_kind, decode_target, encode_relation},
    world::World,
};

use crate::snapshot::{ComponentSnapshot, EntitySnapshot, RelationSnapshot, WorldSnapshot};

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
}

// ── RestoreEntityMap ───────────────────────────────────────────

/// Маппинг старых index → новые Entity, возвращается из `restore`.
pub type RestoreEntityMap = HashMap<u32, Entity>;

// ── WorldSerializer ────────────────────────────────────────────

pub struct WorldSerializer;

impl WorldSerializer {
    // ── Snapshot ───────────────────────────────────────────────

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
                    // component_id — pub(crate), доступ через pub accessor col.id()
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

                    // fn-pointer вызывается через скобки: (serde_fns.serialize_fn)(ptr)
                    let raw_bytes = unsafe { (serde_fns.serialize_fn)(col.get_raw_ptr(row)) }
                        .map_err(|e| SerializationError::SerializeFailed {
                            type_name: info.name.to_string(),
                            reason:    e.to_string(),
                        })?;

                    let json_val: serde_json::Value = serde_json::from_slice(&raw_bytes)?;

                    entity_snap.components.push(ComponentSnapshot {
                        type_name: info.name.to_string(),
                        data:      json_val,
                    });
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
        // Клонируем в HashMap<String, ComponentId> чтобы не держать borrow на registry
        // пока дальше мутируем world.
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

                // Десериализуем в отдельном scope чтобы borrow на registry
                // закончился до вызова world.insert_raw_pub()
                let component_bytes = {
                    let info = world.registry().get_info(component_id).unwrap();
                    let serde_fns = match &info.serde {
                        Some(s) => s,
                        None    => continue,
                    };
                    let raw = serde_json::to_vec(&comp_snap.data)?;
                    // fn-pointer вызывается через скобки
                    (serde_fns.deserialize_fn)(&raw)
                        .map_err(|e| SerializationError::DeserializeFailed {
                            type_name: comp_snap.type_name.clone(),
                            reason:    e.to_string(),
                        })?
                };

                // insert_raw — pub(crate), используем публичную обёртку insert_raw_pub
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
}