//! PrefabManifest — файловые префабы (JSON-формат).
//!
//! Позволяет описывать entity и их иерархии в JSON-файлах,
//! загружать их через [`PrefabLoader`] и спавнить в [`World`].
//!
//! # Формат
//!
//! ```json
//! {
//!   "name": "Monster",
//!   "components": [
//!     { "type_name": "crate::Health",  "value": { "current": 100, "max": 100 } },
//!     { "type_name": "crate::Name",    "value": "Monster" }
//!   ],
//!   "children": [
//!     { "prefab": "Weapon", "overrides": [{ "type_name": "crate::Name", "value": "Sword" }] },
//!     { "prefab": "Helmet" }
//!   ]
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;

use apex_core::{
    component::{ComponentId, Tick},
    entity::Entity,
    relations::ChildOf,
    world::World,
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::serializer::SerializationError;

// ── Structures ────────────────────────────────────────────────────

/// Описание одного компонента в префабе.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefabComponent {
    /// Полное имя типа (должно совпадать с `ComponentInfo.name`).
    pub type_name: String,
    /// JSON-значение компонента.
    pub value:     serde_json::Value,
}

/// Ребёнок в иерархии префаба.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefabChild {
    /// Имя под-префаба (должен быть загружен в `PrefabLoader`).
    pub prefab:    String,
    /// Переопределения компонентов для этого ребёнка.
    #[serde(default)]
    pub overrides: Vec<PrefabComponent>,
}

/// Манифест префаба — JSON-файл, описывающий entity и его детей.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefabManifest {
    /// Имя префаба (для отладки и поиска).
    pub name:       String,
    /// Компоненты корневой entity.
    pub components: Vec<PrefabComponent>,
    /// Дочерние entity (рекурсивно).
    #[serde(default)]
    pub children:   Vec<PrefabChild>,
}

// ── Errors ────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PrefabError {
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("component `{type_name}` not registered in world")]
    ComponentNotRegistered { type_name: String },

    #[error("component `{type_name}` has no serde (de)serializer")]
    ComponentNotSerializable { type_name: String },

    #[error("sub-prefab `{name}` not found in cache")]
    SubPrefabNotFound { name: String },

    #[error("serialization error: {0}")]
    Serialization(#[from] SerializationError),
}

// ── PrefabLoader ──────────────────────────────────────────────────

/// Загрузчик и кеш префабов.
///
/// Хранит загруженные манифесты в памяти и предоставляет метод
/// [`instantiate`](PrefabLoader::instantiate) для создания entity.
pub struct PrefabLoader {
    /// Кеш загруженных манифестов (имя → манифест).
    cache: FxHashMap<String, PrefabManifest>,
}

impl PrefabLoader {
    pub fn new() -> Self {
        Self { cache: FxHashMap::default() }
    }

    /// Загрузить манифест из JSON-строки.
    pub fn load_json(&mut self, json: &str) -> Result<&PrefabManifest, PrefabError> {
        let manifest: PrefabManifest = serde_json::from_str(json)?;
        let name = manifest.name.clone();
        self.cache.insert(name.clone(), manifest);
        // Гарантированно существует — только что вставили
        Ok(self.cache.get(&name).unwrap())
    }

    /// Загрузить манифест из файла.
    pub fn load_file(&mut self, path: &Path) -> Result<&PrefabManifest, PrefabError> {
        let content = std::fs::read_to_string(path)?;
        self.load_json(&content)
    }

    /// Получить манифест из кеша по имени.
    pub fn get(&self, name: &str) -> Option<&PrefabManifest> {
        self.cache.get(name)
    }

    /// Есть ли префаб в кеше?
    pub fn has(&self, name: &str) -> bool {
        self.cache.contains_key(name)
    }

    /// Количество загруженных префабов.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    // ── Instantiate ──────────────────────────────────────────────

    /// Создать entity из префаба (рекурсивно, с учётом children).
    ///
    /// * `overrides` — переопределения компонентов корневой entity.
    /// * `parent` — если указан, entity становится ребёнком через `ChildOf`.
    pub fn instantiate(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
        parent: Option<Entity>,
    ) -> Result<Entity, PrefabError> {
        let entity = self.spawn_entity(world, manifest, overrides)?;

        // Parent relation
        if let Some(parent_entity) = parent {
            world.add_relation(entity, ChildOf, parent_entity);
        }

        // Children
        for child in &manifest.children {
            let child_manifest = self.cache.get(&child.prefab).ok_or_else(|| {
                PrefabError::SubPrefabNotFound { name: child.prefab.clone() }
            })?;

            self.instantiate(world, child_manifest, &child.overrides, Some(entity))?;
        }

        Ok(entity)
    }

    /// Внутренний метод: создаёт одну entity с компонентами из manifest + overrides.
    fn spawn_entity(
        &self,
        world: &mut World,
        manifest: &PrefabManifest,
        overrides: &[PrefabComponent],
    ) -> Result<Entity, PrefabError> {
        let entity = world.spawn_empty();
        let tick = world.current_tick();

        // Строим HashMap overrides для быстрого поиска
        let override_map: HashMap<&str, &serde_json::Value> = overrides
            .iter()
            .map(|c| (c.type_name.as_str(), &c.value))
            .collect();

        // Объединяем: overrides заменяют компоненты из manifest
        let mut seen: HashMap<&str, &serde_json::Value> = HashMap::new();
        for comp in &manifest.components {
            seen.entry(&comp.type_name).or_insert(&comp.value);
        }
        // Overrides перезаписывают
        for (type_name, value) in &override_map {
            seen.insert(type_name, value);
        }

        // Применяем компоненты
        for (type_name, json_value) in &seen {
            let component_id = match world.component_id_by_name(type_name) {
                Some(id) => id,
                None => return Err(PrefabError::ComponentNotRegistered {
                    type_name: (*type_name).to_string(),
                }),
            };

            let info = world.registry().get_info(component_id).unwrap();
            let serde_fns = match &info.serde {
                Some(s) => s,
                None => return Err(PrefabError::ComponentNotSerializable {
                    type_name: (*type_name).to_string(),
                }),
            };

            // Сериализуем JSON Value → JSON строка → байты → deserialize_fn
            let json_bytes = serde_json::to_vec(json_value)?;
            let component_bytes = (serde_fns.deserialize_fn)(&json_bytes)
                .map_err(|e| PrefabError::Serialization(
                    SerializationError::DeserializeFailed {
                        type_name: (*type_name).to_string(),
                        reason: e.to_string(),
                    }
                ))?;

            world.insert_raw_pub(entity, component_id, component_bytes, tick);
        }

        Ok(entity)
    }
}

impl Default for PrefabLoader {
    fn default() -> Self { Self::new() }
}

// ── EntityTemplate для PrefabManifest ─────────────────────────────

impl apex_core::template::EntityTemplate for PrefabManifest {
    fn spawn(&self, world: &mut World, params: &apex_core::template::TemplateParams) -> Entity {
        // Создаём временный PrefabLoader для instantiate
        let loader = PrefabLoader::new();
        // Пытаемся преобразовать params в overrides
        let overrides: Vec<PrefabComponent> = Vec::new(); // params → overrides (сложно без type_id → name)
        // Поскольку мы не можем легко преобразовать TemplateParams в PrefabComponent
        // (нужен обратный маппинг TypeId → имя), используем instantiate без overrides
        loader.instantiate(world, self, &overrides, None)
            .expect("PrefabManifest::spawn failed")
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::prelude::*;
    use serde::{Deserialize, Serialize};

    // Тестовые компоненты
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Health {
        current: f32,
        max: f32,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Name(String);

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Position {
        x: f32,
        y: f32,
        z: f32,
    }

    fn setup_world() -> World {
        let mut world = World::new();
        world.register_component_serde::<Health>();
        world.register_component_serde::<Name>();
        world.register_component_serde::<Position>();
        world
    }

    #[test]
    fn prefab_json_roundtrip() {
        let manifest = PrefabManifest {
            name: "Test".to_string(),
            components: vec![
                PrefabComponent {
                    type_name: "apex_core::Health".to_string(),
                    value: serde_json::json!({ "current": 100.0, "max": 100.0 }),
                },
            ],
            children: vec![],
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let restored: PrefabManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.name, "Test");
        assert_eq!(restored.components.len(), 1);
        assert_eq!(restored.components[0].type_name, "apex_core::Health");
    }

    #[test]
    fn prefab_instantiate_single() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        let json = r#"{
            "name": "Test",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 100.0, "max": 100.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Goblin" }
            ]
        }"#;

        let manifest = loader.load_json(json).unwrap().clone();
        let entity = loader.instantiate(&mut world, &manifest, &[], None).unwrap();

        let health = world.get::<Health>(entity).unwrap();
        assert_eq!(health.current, 100.0);
        assert_eq!(health.max, 100.0);

        let name = world.get::<Name>(entity).unwrap();
        assert_eq!(name.0, "Goblin");
    }

    #[test]
    fn prefab_instantiate_hierarchy() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        // Parent prefab
        loader.load_json(r#"{
            "name": "Parent",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Parent" }
            ],
            "children": [
                { "prefab": "Child" }
            ]
        }"#).unwrap();

        // Child prefab
        loader.load_json(r#"{
            "name": "Child",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Child" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Parent").unwrap();
        let parent = loader.instantiate(&mut world, manifest, &[], None).unwrap();

        let parent_name = world.get::<Name>(parent).unwrap();
        assert_eq!(parent_name.0, "Parent");

        // Ищем ребёнка через ChildOf
        let children: Vec<Entity> = world.children_of(ChildOf, parent).collect();
        assert_eq!(children.len(), 1);

        let child_name = world.get::<Name>(children[0]).unwrap();
        assert_eq!(child_name.0, "Child");
    }

    #[test]
    fn prefab_with_overrides() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Monster",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 50.0, "max": 50.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Monster" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Monster").unwrap();
        let overrides = vec![
            PrefabComponent {
                type_name: "apex_serialization::prefab::tests::Health".to_string(),
                value: serde_json::json!({ "current": 200.0, "max": 200.0 }),
            },
        ];

        let entity = loader.instantiate(&mut world, manifest, &overrides, None).unwrap();

        let health = world.get::<Health>(entity).unwrap();
        assert_eq!(health.current, 200.0);  // override
        assert_eq!(health.max, 200.0);

        let name = world.get::<Name>(entity).unwrap();
        assert_eq!(name.0, "Monster");  // default
    }

    #[test]
    fn prefab_component_not_registered() {
        let mut world = World::new();  // Без регистрации Health
        let mut loader = PrefabLoader::new();

        let json = r#"{
            "name": "Test",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Health", "value": { "current": 100.0, "max": 100.0 } }
            ]
        }"#;

        let manifest = loader.load_json(json).unwrap().clone();
        let result = loader.instantiate(&mut world, &manifest, &[], None);

        assert!(result.is_err());
        match result {
            Err(PrefabError::ComponentNotRegistered { .. }) => {} // expected
            _ => panic!("expected ComponentNotRegistered error"),
        }
    }

    #[test]
    fn prefab_sub_prefab_not_found() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Parent",
            "components": [],
            "children": [
                { "prefab": "NonExistent" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Parent").unwrap();
        let result = loader.instantiate(&mut world, manifest, &[], None);

        assert!(result.is_err());
        match result {
            Err(PrefabError::SubPrefabNotFound { .. }) => {} // expected
            _ => panic!("expected SubPrefabNotFound error"),
        }
    }

    #[test]
    fn prefab_loader_cache() {
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{"name": "A", "components": []}"#).unwrap();
        loader.load_json(r#"{"name": "B", "components": []}"#).unwrap();

        assert!(loader.has("A"));
        assert!(loader.has("B"));
        assert!(!loader.has("C"));
        assert_eq!(loader.len(), 2);
    }

    #[test]
    fn prefab_instantiate_with_position() {
        let mut world = setup_world();
        let mut loader = PrefabLoader::new();

        loader.load_json(r#"{
            "name": "Mover",
            "components": [
                { "type_name": "apex_serialization::prefab::tests::Position", "value": { "x": 1.0, "y": 2.0, "z": 3.0 } },
                { "type_name": "apex_serialization::prefab::tests::Name", "value": "Mover" }
            ]
        }"#).unwrap();

        let manifest = loader.get("Mover").unwrap();
        let entity = loader.instantiate(&mut world, manifest, &[], None).unwrap();

        let pos = world.get::<Position>(entity).unwrap();
        assert_eq!(pos.x, 1.0);
        assert_eq!(pos.y, 2.0);
        assert_eq!(pos.z, 3.0);
    }
}
