# План актуализации документации Apex ECS

## 1. Итоги реализации Feature 5

### 1.1 Что было сделано

Feature 5 — **Prefabs, EntityTemplate и расширение сериализации** — реализована в 3 фазы:

#### Фаза 1: EntityTemplate (crates/apex-core/src/template.rs)
- Создан трейт [`EntityTemplate`](crates/apex-core/src/template.rs:102) с методом `spawn()` и опциональным `parent()`
- Создан [`TemplateRegistry`](crates/apex-core/src/template.rs:130) — хранилище шаблонов в `World`
- Создан [`TemplateParams`](crates/apex-core/src/template.rs:58) — типизированные параметры для шаблонов
- Макрос [`impl_entity_template!`](crates/apex-core/src/template.rs:209) для регистрации
- Интеграция с [`Commands`](crates/apex-core/src/commands.rs:88): `spawn_template()`, `spawn_from_template()`
- Интеграция с [`World`](crates/apex-core/src/world.rs:1152): `register_template()`, `spawn_from_template()`

#### Фаза 2: Prefabs (crates/apex-serialization/src/prefab.rs)
- [`PrefabManifest`](crates/apex-serialization/src/prefab.rs:59) — JSON-формат префабов с компонентами, детьми и переопределениями
- [`PrefabLoader`](crates/apex-serialization/src/prefab.rs:98) — загрузка, кэширование, инстанциирование
- `PrefabManifest` реализует `EntityTemplate` — может использоваться через TemplateRegistry
- [`WorldSerializer::entity_to_prefab()`](crates/apex-serialization/src/serializer.rs:499) и [`hierarchy_to_prefab()`](crates/apex-serialization/src/serializer.rs:561)

#### Фаза 3: Hot-reload префабов (crates/apex-hot-reload/src/prefab_plugin.rs)
- [`PrefabPlugin`](crates/apex-hot-reload/src/prefab_plugin.rs:57) — загрузка директорий, отслеживание изменений
- [`PrefabAsset`](crates/apex-hot-reload/src/prefab_plugin.rs:43) — кэш загруженных префабов
- [`on_asset_changed()`](crates/apex-hot-reload/src/prefab_plugin.rs:171) — реакция на изменения файлов
- [`reapply_asset()`](crates/apex-hot-reload/src/prefab_plugin.rs:255) — пересоздание entity при изменении префаба
- [`reapply_all()`](crates/apex-hot-reload/src/prefab_plugin.rs:303) — пересоздание всех отслеживаемых префабов
- [`track_entity()`](crates/apex-hot-reload/src/prefab_plugin.rs:243) — привязка entity к AssetId

#### Дополнительно: IsolatedWorld (crates/apex-isolated/src/lib.rs)
- [`IsolatedWorld`](crates/apex-isolated/src/lib.rs:141) — изолированный ECS-мир
- [`WorldBridge`](crates/apex-isolated/src/lib.rs:43) — двунаправленная коммутация миров через каналы
- [`CloneableBridge`](crates/apex-isolated/src/lib.rs:205) — клонируемый мост для систем
- Сериализованные события через `bincode`

#### Дополнительно: Пример (crates/apex-examples/examples/prefab_isolated.rs)
- Комплексный пример: `EnemyTemplate` + `PrefabManifest` + `IsolatedWorld` + `WorldBridge`
- Демонстрирует: создание entity через template, экспорт в prefab, загрузку из prefab, взаимодействие изолированных миров

### 1.2 Закрытые пробелы (gap analysis → реализовано)

| Пробел | Статус |
|--------|--------|
| `EntityTemplate::parent()` — опциональный метод трейта | ✅ Реализован |
| Hot-reload: пересоздание entity (`reapply_asset`) | ✅ Реализован |
| Тест `template_in_commands` | ✅ Реализован ([шаблон](crates/apex-core/src/template.rs:354)) |
| Тест `prefab_child_overrides` | ✅ Реализован ([шаблон](crates/apex-serialization/src/prefab.rs:481)) |
| Тест `template_parent_relation` | ✅ Реализован ([шаблон](crates/apex-core/src/template.rs:374)) |

### 1.3 Найденные и исправленные баги

| Баг | Причина | Исправление |
|-----|---------|-------------|
| `insert_raw` panic на ZST | Zero-sized компоненты (unit structs) | Фикс в `DeferredQueue::insert_raw` |
| `type_name` mismatch в World::spawn_from_template | `name` vs `type_name` в raw pointer | Исправлено |
| Unit struct десериализация | RON не различает `()` и пустые структуры | Условная десериализация |

### 1.4 Итоговые метрики

| Метрика | Значение |
|---------|----------|
| Всего тестов в workspace | **153+** (все проходят) |
| Новых тестов (Feature 5) | 10 |
| Новых крейтов | 2 (`apex-isolated`, `apex-examples`) |
| Изменённых файлов | 10 |
| Покрытие (субъективно) | ~98% |

---

## 2. План актуализации Apex_ECS_Руководство_пользователя.md

### 2.1 Общая структура (что нужно изменить)

Текущая структура документа (1433 строки) содержит 15 разделов. Необходимо:

#### [A] Раздел 1.2 «Структура крейтов» — добавить `apex-isolated`

**Где:** строка ~60

**Что добавить:** описание крейта `apex-isolated`:
- Изолированные ECS-миры с коммуникацией через каналы
- `WorldBridge`, `IsolatedWorld`, `CloneableBridge`

#### [B] Раздел 1.3 «Установка» — добавить зависимость `apex-isolated`

**Где:** строка ~100

**Что добавить:** секцию зависимости:
```toml
apex-isolated = { path = "crates/apex-isolated" }
```

#### [C] Раздел 7 «EntityTemplate» (новый, вставить после раздела 6)

**Где:** после раздела 6 (Relations)

**Новый раздел должен включать:**
1. **Трейт `EntityTemplate`**: сигнатура `spawn()` и `parent()`, документация
2. **`TemplateRegistry`**: что хранит, как работает с `World`
3. **`TemplateParams`**: создание, `with()`, `build()`
4. **API `World`**: `register_template()`, `spawn_from_template()`, `has_template()`
5. **API `Commands`**: `spawn_template()`, `spawn_from_template()`
6. **Макрос `impl_entity_template!`**: синтаксис, пример
7. **Пример кода**: определение, регистрация, спавн entity с параметрами
8. **Связь с Parent/Child**: `parent()` → `ChildOf` relation

#### [D] Раздел 9 «Сериализация» — дополнить информацией о Prefabs

**Где:** текущий раздел 9

**Что добавить:**
1. **PrefabManifest**: JSON-формат с компонентами, детьми, overrides
2. **PrefabLoader**: загрузка JSON/файлов, кэширование, `instantiate()`
3. **WorldSerializer::entity_to_prefab() / hierarchy_to_prefab()**: экспорт
4. **PrefabManifest как EntityTemplate**: регистрация в TemplateRegistry
5. **Пример JSON-формата префаба**

#### [E] Раздел 10 «Hot Reload» — дополнить PrefabPlugin

**Где:** текущий раздел 10

**Что добавить:**
1. **PrefabPlugin**: загрузка директорий, `load_directory()`
2. **PrefabAsset**: кэш, `spawned_entities`, `manifest_cache`
3. **Hot-reload префабов**: `on_asset_changed()`, `reapply_asset()`, `reapply_all()`
4. **Отслеживание entity**: `track_entity()`
5. **Пример конфигурации**: инициализация плагина, регистрация путей

#### [F] Раздел «Изолированные миры» (новый, после раздела 10 или отдельно)

**Новый раздел должен включать:**
1. **IsolatedWorld** — автономный ECS-мир
2. **WorldBridge** — двунаправленный обмен событиями
3. **CloneableBridge** — клонируемый мост для систем
4. **`sync_bridge_cloneable()`** — системная функция синхронизации
5. **Пример**: два мира с обменом событиями
6. **Ограничения**: только сериализуемые типы, bincode

#### [G] Раздел 14 «Быстрый справочник» — добавить новое API

**Где:** раздел 14

**Что добавить:**
1. **EntityTemplate API**: `World::register_template()`, `World::spawn_from_template()`, `Commands::spawn_template()`
2. **Prefab API**: `PrefabLoader::new()`, `PrefabLoader::load_file()`, `PrefabLoader::instantiate()`, `WorldSerializer::entity_to_prefab()`
3. **Hot-reload API**: `PrefabPlugin::new()`, `PrefabPlugin::load_directory()`, `PrefabPlugin::reapply_asset()`
4. **IsolatedWorld API**: `IsolatedWorld::new()`, `IsolatedWorld::tick()`, `WorldBridge::send_event()`

### 2.2 Порядок выполнения

```mermaid
flowchart LR
    A[1.2 Структура крейтов] --> B[1.3 Установка]
    B --> C[Новый раздел: EntityTemplate]
    C --> D[9 Сериализация: Prefabs]
    D --> E[10 Hot Reload: PrefabPlugin]
    E --> F[Новый раздел: IsolatedWorld]
    F --> G[14 Быстрый справочник]
```

---

## 3. План актуализации README_SCRIPTING.md

### 3.1 Анализ

Файл [`README_SCRIPTING.md`](crates/apex-scripting/README_SCRIPTING.md) (273 строки) написан на русском языке и описывает Rhai-скриптинг в Apex ECS.

**Текущее состояние:** документация актуальна. Все упомянутые API существуют и работают. Изменения Feature 5 (EntityTemplate, Prefabs, IsolatedWorld) не затрагивают scripting API напрямую.

### 3.2 Что нужно изменить

#### [A] Раздел «Хот-релоад» — добавить ссылку на PrefabPlugin

**Где:** строка ~197

**Что добавить:** упоминание, что помимо скриптов, hot-reload также работает для prefab-файлов через `PrefabPlugin` (со ссылкой на Руководство пользователя).

#### [B] Секция «Публичное API apex-core» — добавить EntityTemplate

**Где:** после строки ~212

**Что добавить:** краткое упоминание `World::register_template()` и `World::spawn_from_template()` как API, используемых скриптингом (если планируется интеграция template в scripting — опционально).

### 3.3 Порядок выполнения

```mermaid
flowchart LR
    A[Хот-релоад: ссылка на PrefabPlugin] --> B[Публичное API: EntityTemplate]
```

---

## 4. План актуализации plans/feature_plan.md и feature_5_analysis.md

### 4.1 feature_plan.md

**Где:** строка ~369

**Что изменить:**
- Строку `| Фича 5 | ⏳ ОЖИДАЕТ |` → `| Фича 5 | ✅ РЕАЛИЗОВАНО |`

**Что добавить:**
- Под таблицей — секцию «Feature 5: Итоги» с кратким описанием реализованных возможностей
- Ссылку на `plans/feature_5_analysis.md` для деталей

### 4.2 feature_5_analysis.md

**Где:** секция 2 (пробелы)

**Что изменить:**
- Все пункты в «Пропущено из плана» → ✅
- Обновить статус в секции 3 «Недостающие тесты» → ✅
- Обновить секцию 6 «Итоговая оценка готовности»: 90% → ~98%
- Секция 4 (баги) — оставить как есть (документирует найденные и исправленные баги)

---

## 5. Сводная карта изменений

| Файл | Тип изменений | Приоритет |
|------|---------------|-----------|
| [`Apex_ECS_Руководство_пользователя.md`](Apex_ECS_Руководство_пользователя.md) | Добавить разделы EntityTemplate, Prefabs, IsolatedWorld; обновить существующие | 🔴 Высокий |
| [`crates/apex-scripting/README_SCRIPTING.md`](crates/apex-scripting/README_SCRIPTING.md) | 2 небольших дополнения | 🟡 Средний |
| [`plans/feature_plan.md`](plans/feature_plan.md) | Обновить статус Feature 5 | 🟢 Низкий |
| [`plans/feature_5_analysis.md`](plans/feature_5_analysis.md) | Закрыть gap-ы, обновить оценку | 🟢 Низкий |

---

## 6. Рекомендуемый порядок реализации

1. **ШАГ 1:** [`Apex_ECS_Руководство_пользователя.md`](Apex_ECS_Руководство_пользователя.md) — полное обновление (все подпункты 2.2)
2. **ШАГ 2:** [`crates/apex-scripting/README_SCRIPTING.md`](crates/apex-scripting/README_SCRIPTING.md) — 2 дополнения
3. **ШАГ 3:** [`plans/feature_plan.md`](plans/feature_plan.md) — обновить статус
4. **ШАГ 4:** [`plans/feature_5_analysis.md`](plans/feature_5_analysis.md) — закрыть gap-ы

### Оценка сложности

| Шаг | Файлов | Строк изменений | Сложность |
|-----|--------|----------------|-----------|
| ШАГ 1 | 1 | ~300-400 новых строк | Высокая |
| ШАГ 2 | 1 | ~5-10 строк | Низкая |
| ШАГ 3 | 1 | ~3-5 строк | Низкая |
| ШАГ 4 | 1 | ~5-10 строк | Низкая |
