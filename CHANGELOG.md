# Changelog

## [0.1.0] — 2026-05-18

### Core Stabilization — Phase 0

Выпуск v0.1 замораживает публичный API apex-core. Breaking changes после этого
релиза — только с major-версией.

### Added

- **`ComponentMask` расширен до 512 бит (8 × u64)**. `access.rs` — потолок в 256
  компонентов был реалистичным ограничением для полноценного игрового движка.
  Размер маски 64 байта = одна кэш-линия, zero-cost.
- **`FixedUpdate` добавлен в `StageLabel`**. `stage.rs` — для физики и
  детерминированных систем с фиксированным шагом. Стандартный порядок:
  `Startup → First → PreUpdate → FixedUpdate → Update → PostUpdate → Last`.
- **`PrefabManifest::spawn` с `TemplateParams` — конвертация в overrides**.
  `template.rs` + `prefab.rs` — `TemplateParam` теперь опционально объявляет
  `component_type_name()`, и параметры автоматически сериализуются в JSON
  для переопределения компонентов в префабах.
- **Тесты для `archetype.rs`** — 17 тестов колонок, архетипов и чанков.
- **Тесты для `commands.rs`** — 14 тестов команд, арены и edge-кейсов.
- **`TemplateParams::json_overrides_iter()`** — итератор по предсериализованным
  JSON-переопределениям.

### Changed

- `TemplateParam::Value` теперь требует `Serialize`.
- `TemplateParam` — добавлен метод `component_type_name()` с default `""`.
- `TemplateParams` хранит дополнительно `type_names` и `json_overrides`.
- `ComponentMask::set/get/word_idx/bit_idx` принимают `u16` вместо `u8`.
- `AccessDescriptor::assign_masks` — параметр `HashMap<TypeId, u16>`.
- Приоритеты `StageLabel` сдвинуты: `Update=4`, `PostUpdate=5`, `Last=6`, `Custom=7`.

### Fixed

- `PrefabManifest::spawn` теперь использует `TemplateParams` для генерации
  overrides компонентов вместо игнорирования параметров.

### API Stability

- `apex-core` v0.1 — публичный API заморожен.
- `apex-scheduler` v0.1 — `StageLabel` расширен без удаления вариантов.
- Семвер: breaking changes только с major ≥ 1.0.
