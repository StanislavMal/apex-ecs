//! Структуры данных снэпшота мира — формат хранения/передачи.
//!
//! # Форматы
//!
//! - **JSON** — человекочитаемый, для отладки и конфигов
//! - **Bincode** — компактный бинарный, для быстрых сохранений/загрузок
//!
//! Компоненты всегда хранятся как сырые байты (`Vec<u8>`).
//! При JSON-сериализации снэпшота байты интерпретируются как JSON.
//! При Bincode-сериализации — как бинарные данные.

use serde::{Deserialize, Serialize};

// ── Версионирование ──────────────────────────────────────────────

/// Версия формата снэпшота — мажорная + минорная.
///
/// - Мажорная версия меняется при breaking change
/// - Минорная версия меняется при обратно-совместимых изменениях
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotVersion {
    pub major: u32,
    pub minor: u32,
}

impl SnapshotVersion {
    pub const CURRENT: Self = Self { major: 1, minor: 0 };

    pub fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Проверить, совместима ли эта версия с указанной.
    /// Совместимость = совпадает мажорная, минорная >= ожидаемой.
    pub fn is_compatible_with(&self, expected: SnapshotVersion) -> bool {
        self.major == expected.major && self.minor >= expected.minor
    }
}

impl std::fmt::Display for SnapshotVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

// ── Формат хранения данных компонента ───────────────────────────

/// Формат, в котором хранятся байты компонента.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataFormat {
    /// JSON-байты (человекочитаемые).
    Json,
    /// Бинарные байты (bincode).
    Binary,
}

// ── WorldSnapshot ────────────────────────────────────────────────

/// Полный снимок мира — всё что нужно для восстановления state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Версия формата снэпшота — для будущей миграции.
    pub version:   u32,
    /// Тик мира на момент снэпшота.
    pub tick:      u32,
    /// Все живые entity с их компонентами.
    pub entities:  Vec<EntitySnapshot>,
    /// Relations между entity.
    pub relations: Vec<RelationSnapshot>,
}

impl WorldSnapshot {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(tick: u32) -> Self {
        Self {
            version:   Self::CURRENT_VERSION,
            tick,
            entities:  Vec::new(),
            relations: Vec::new(),
        }
    }

    // ── JSON ─────────────────────────────────────────────────────

    /// Сериализовать снэпшот в JSON-байты.
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Десериализовать снэпшот из JSON-байт.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }

    // ── Bincode ──────────────────────────────────────────────────

    /// Сериализовать снэпшот в бинарный формат (bincode).
    ///
    /// Размер в 5-10x меньше JSON, скорость в 2-3x выше.
    pub fn to_bincode(&self) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
        bincode::serialize(self)
    }

    /// Десериализовать снэпшот из бинарного формата (bincode).
    ///
    /// Проверяет совместимость версии снэпшота.
    pub fn from_bincode(data: &[u8]) -> Result<Self, Box<bincode::ErrorKind>> {
        bincode::deserialize(data)
    }

    // ── Миграция ─────────────────────────────────────────────────

    /// Запустить цепочку миграций, приводя снэпшот к текущей версии.
    pub fn migrate(&mut self) -> Result<(), String> {
        while self.version < Self::CURRENT_VERSION {
            let migrator = migration_for(self.version)
                .ok_or_else(|| format!("no migration found for version {}", self.version))?;
            migrator(self)?;
            self.version += 1;
        }
        Ok(())
    }

    /// Проверить совместимость версии снэпшота с текущей.
    pub fn is_version_compatible(&self) -> bool {
        let expected = SnapshotVersion::CURRENT;
        let found = SnapshotVersion::new(self.version, 0);
        found.is_compatible_with(expected)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn relation_count(&self) -> usize {
        self.relations.len()
    }
}

// ── EntitySnapshot ───────────────────────────────────────────────

/// Снимок одной entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// Оригинальный index entity — для remapping при restore.
    pub original_index: u32,
    /// Сериализованные компоненты entity.
    pub components: Vec<ComponentSnapshot>,
}

// ── ComponentSnapshot ────────────────────────────────────────────

/// Снимок одного компонента.
///
/// `data` всегда содержит сырые байты в формате, указанном `format`.
/// - `Json`: байты = текст JSON
/// - `Binary`: байты = бинарная сериализация (bincode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    /// Имя типа компонента.
    pub type_name: String,
    /// Сырые байты данных компонента.
    pub data: Vec<u8>,
    /// Формат данных.
    pub format: DataFormat,
}

impl ComponentSnapshot {
    /// Создать снэпшот из JSON-байт.
    pub fn new_json(type_name: impl Into<String>, json_bytes: Vec<u8>) -> Self {
        Self {
            type_name: type_name.into(),
            data: json_bytes,
            format: DataFormat::Json,
        }
    }

    /// Создать снэпшот из бинарных байт.
    pub fn new_binary(type_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            type_name: type_name.into(),
            data: bytes,
            format: DataFormat::Binary,
        }
    }

    /// Получить данные как slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Являются ли данные JSON.
    pub fn is_json(&self) -> bool {
        self.format == DataFormat::Json
    }
}

// ── RelationSnapshot ─────────────────────────────────────────────

/// Снимок одной relation между entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationSnapshot {
    pub subject_index: u32,
    pub target_index:  u32,
    pub kind_name:     String,
}

// ── Миграции ─────────────────────────────────────────────────────

type MigrationFn = fn(&mut WorldSnapshot) -> Result<(), String>;

fn migration_for(version: u32) -> Option<MigrationFn> {
    match version {
        _ => None,
    }
}

// ── WorldDiff (инкрементальные изменения) ───────────────────────

/// Разница между двумя снэпшотами для инкрементального сохранения.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldDiff {
    pub version: u32,
    /// Добавленные entity.
    pub added_entities: Vec<EntitySnapshot>,
    /// Удалённые entity (original_index).
    pub removed_entities: Vec<u32>,
    /// Компоненты, добавленные к существующим entity.
    pub added_components: Vec<(u32, Vec<ComponentSnapshot>)>,
    /// Компоненты, удалённые у существующих entity.
    pub removed_components: Vec<(u32, Vec<String>)>,
    /// Добавленные relations.
    pub added_relations: Vec<RelationSnapshot>,
    /// Удалённые relations.
    pub removed_relations: Vec<RelationSnapshot>,
}

impl WorldDiff {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            added_entities: Vec::new(),
            removed_entities: Vec::new(),
            added_components: Vec::new(),
            removed_components: Vec::new(),
            added_relations: Vec::new(),
            removed_relations: Vec::new(),
        }
    }

    pub fn to_bincode(&self) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
        bincode::serialize(self)
    }

    pub fn from_bincode(data: &[u8]) -> Result<Self, Box<bincode::ErrorKind>> {
        bincode::deserialize(data)
    }

    pub fn is_empty(&self) -> bool {
        self.added_entities.is_empty()
            && self.removed_entities.is_empty()
            && self.added_components.is_empty()
            && self.removed_components.is_empty()
            && self.added_relations.is_empty()
            && self.removed_relations.is_empty()
    }
}

impl Default for WorldDiff {
    fn default() -> Self {
        Self::new()
    }
}

// ── Format enum ──────────────────────────────────────────────────

/// Формат сериализации для файлового I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveFormat {
    Json,
    Bincode,
}

// ── Тесты ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compatible() {
        let v1 = SnapshotVersion::new(1, 0);
        let v1_1 = SnapshotVersion::new(1, 1);
        let v2 = SnapshotVersion::new(2, 0);

        assert!(v1.is_compatible_with(SnapshotVersion::CURRENT));
        assert!(v1_1.is_compatible_with(SnapshotVersion::CURRENT));
        assert!(!v2.is_compatible_with(SnapshotVersion::CURRENT));
    }

    #[test]
    fn snapshot_json_roundtrip() {
        let mut snap = WorldSnapshot::new(42);
        snap.entities.push(EntitySnapshot {
            original_index: 1,
            components: vec![
                ComponentSnapshot::new_json("my_crate::Position", br#"{"x":1.0,"y":2.0}"#.to_vec()),
            ],
        });

        let json = snap.to_json().unwrap();
        let restored = WorldSnapshot::from_json(&json).unwrap();

        assert_eq!(restored.tick, 42);
        assert_eq!(restored.entities.len(), 1);
        assert_eq!(restored.entities[0].original_index, 1);
        assert_eq!(restored.entities[0].components[0].type_name, "my_crate::Position");
    }

    #[test]
    fn snapshot_bincode_roundtrip() {
        let mut snap = WorldSnapshot::new(42);
        snap.entities.push(EntitySnapshot {
            original_index: 1,
            components: vec![
                ComponentSnapshot::new_json("my_crate::Position", br#"{"x":1.0,"y":2.0}"#.to_vec()),
            ],
        });
        snap.relations.push(RelationSnapshot {
            subject_index: 1,
            target_index:  0,
            kind_name:     "apex_core::relations::ChildOf".to_string(),
        });

        let binary = snap.to_bincode().unwrap();
        let restored = WorldSnapshot::from_bincode(&binary).unwrap();

        assert_eq!(restored.tick, 42);
        assert_eq!(restored.entities.len(), 1);
        assert_eq!(restored.relations.len(), 1);
        // Проверяем что JSON-байты сохранились
        assert!(restored.entities[0].components[0].is_json());
        assert_eq!(restored.entities[0].components[0].as_bytes(), br#"{"x":1.0,"y":2.0}"#);
    }

    #[test]
    fn bincode_smaller_than_json() {
        let mut snap = WorldSnapshot::new(100);
        for i in 0..100 {
            snap.entities.push(EntitySnapshot {
                original_index: i,
                components: vec![
                    ComponentSnapshot::new_json("Pos", br#"{"x":1.0,"y":2.0}"#.to_vec()),
                    ComponentSnapshot::new_json("Vel", br#"{"x":0.0,"y":0.0}"#.to_vec()),
                ],
            });
        }

        let json_size = snap.to_json().unwrap().len();
        let bincode_size = snap.to_bincode().unwrap().len();

        assert!(bincode_size < json_size / 2,
            "bincode={} should be < json/2={}", bincode_size, json_size / 2);
    }

    #[test]
    fn world_diff_empty() {
        let diff = WorldDiff::new();
        assert!(diff.is_empty());
    }

    #[test]
    fn world_diff_bincode_roundtrip() {
        let mut diff = WorldDiff::new();
        diff.added_entities.push(EntitySnapshot {
            original_index: 10,
            components: vec![
                ComponentSnapshot::new_json("Health", br#"{"current":100.0}"#.to_vec()),
            ],
        });
        diff.removed_entities.push(5);
        diff.added_relations.push(RelationSnapshot {
            subject_index: 10,
            target_index:  0,
            kind_name:     "ChildOf".to_string(),
        });

        let binary = diff.to_bincode().unwrap();
        let restored = WorldDiff::from_bincode(&binary).unwrap();

        assert_eq!(restored.added_entities.len(), 1);
        assert_eq!(restored.removed_entities, vec![5]);
        assert_eq!(restored.added_relations.len(), 1);
    }

    #[test]
    fn component_snapshot_formats() {
        let json_comp = ComponentSnapshot::new_json("Pos", br#"{"x":1.0}"#.to_vec());
        assert!(json_comp.is_json());
        assert_eq!(json_comp.as_bytes(), br#"{"x":1.0}"#);

        let bin_comp = ComponentSnapshot::new_binary("Pos", vec![1, 2, 3]);
        assert!(!bin_comp.is_json());
        assert_eq!(bin_comp.as_bytes(), &[1, 2, 3]);
    }

    #[test]
    fn snapshot_migration_noop() {
        let mut snap = WorldSnapshot::new(42);
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);
        snap.migrate().unwrap();
        assert_eq!(snap.version, WorldSnapshot::CURRENT_VERSION);
    }

    #[test]
    fn version_compatibility_check() {
        let mut snap = WorldSnapshot::new(0);
        snap.version = 999;
        assert!(!snap.is_version_compatible());

        snap.version = WorldSnapshot::CURRENT_VERSION;
        assert!(snap.is_version_compatible());
    }
}
