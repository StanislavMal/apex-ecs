//! Структуры данных снэпшота мира — формат хранения/передачи.

use serde::{Deserialize, Serialize};

/// Полный снимок мира — всё что нужно для восстановления state.
///
/// Содержит только сериализуемые компоненты. Non-serializable (runtime) данные
/// не включаются — их нужно восстанавливать отдельной логикой после restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Версия формата снэпшота — для будущей миграции.
    pub version: u32,
    /// Тик мира на момент снэпшота.
    pub tick:    u32,
    /// Все живые entity с их компонентами.
    pub entities: Vec<EntitySnapshot>,
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

    /// Сериализовать снэпшот в JSON-байты.
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Десериализовать снэпшот из JSON-байт.
    pub fn from_json(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// Снимок одной entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// Оригинальный index entity — используется для remapping при restore.
    /// Generation не сохраняется — при restore будет выдан новый generation.
    pub original_index: u32,
    /// Сериализованные компоненты entity.
    pub components: Vec<ComponentSnapshot>,
}

/// Снимок одного компонента.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    /// Имя типа компонента — используется для поиска ComponentId при restore.
    /// Формат: результат `std::any::type_name::<T>()`.
    pub type_name: String,
    /// Сериализованные байты — JSON (или другой формат).
    /// `serde_json::Value` для human-readable хранения без лишних escaping.
    pub data: serde_json::Value,
}

/// Снимок одной relation между entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationSnapshot {
    /// original_index subject entity.
    pub subject_index: u32,
    /// original_index target entity.
    pub target_index:  u32,
    /// Имя типа RelationKind: `std::any::type_name::<R>()`.
    pub kind_name:     String,
}