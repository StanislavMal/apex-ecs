# Changelog

## [Unreleased]

### Changed — волна P ADR-004: `Read<T>`/`Write<T>` УДАЛЕНЫ (2026-07-10, пред-релизно — без deprecation)

- **Единственный словарь запросов — канон ADR-004 Р-5: `&T` / `&mut T`.** Маркер-типы
  `query::Read<T>`/`query::Write<T>` удалены целиком (root- и prelude-экспорты сняты);
  их `WorldQuery`-реализации перенесены в `&'a T`/`&'a mut T` (инверсия делегации —
  семантика бит-в-бит та же: `&mut T` даёт `Mut<T>` со change-tick-штампом на `DerefMut`,
  «нетрекаемого» мутабельного спецификатора не существует).
- **Associated-type/`QueryState`-позиции** пишутся с `'static`: `&'static T` —
  каноничное написание, заём перепривязывается к лайфтайму мира в `WorldQuery::Item<'_>`.
- **`system!`: одиночный запрос — ВСЕГДА 1-кортеж** `q: (&mut T,)`; кортежи запросов
  принимают `&T`/`&mut T` (макрос дописывает `'static` через `__q_static!`). Bare
  `q: &T`/`q: &mut T` остаётся громкой compile-ошибкой (ловушка «ресурс», P2) — иначе
  «ресурсное» намерение тихо становилось бы пустым запросом.
- **Типизированный system-ordering:** `.before(target)`/`.after(target)` на конфиге
  принимают САМУ систему (fn-item или `AutoSystem`-значение) через новый трейт
  `OrderTarget<Marker>` — имя ребра выводится из типа и не может разойтись с
  зарегистрированным. Строковые цели принимаются тем же методом; динамический
  строковый API `Scheduler::before/after/chain` не тронут (Р-3).
- `Ref<T>` НЕ введён — имя остаётся зарезервированным под будущий read-change-detection
  тип; его добавление аддитивно и переделок не потребует.

### Changed — ревизия публичного API (2026-06-12, движок не публиковался — без deprecation)

- **`Scheduler::add_systems(label, …)` — единственный вход регистрации систем.**
  Удалён публичный зоопарк из 15 методов (`add_auto_system[_to_stage]`,
  `add_system[_to_stage]`, `add_par[_to_stage]`, `add_par_access[_to_stage]`,
  `add_startup_*`, `add_exclusive_system[_to_stage|_startup]`, `set_default_stage`,
  `par_for_each_used(id)`, `add_dependency(id,id)`, `system_access(id)`,
  `set_run_if[_cond]`) — всё выражается `add_systems` + конструкторами
  `sys`/`seq`/`par`/`par_access`/`SystemConfig::exclusive` + bare-идентификаторами;
  порядок — `.chain()`/`.before()`/`.after()` на конфигах (по именам — для
  динамики); `par_for_each` — декларативно через `SystemConfig::par_for_each_used()`.
- **`staged(label, f)` → `scoped(f)`**: скоуп-условия отделены от стадий (стадия
  теперь всегда явная в `add_systems`). Попутно закрыты ДВА латентных бага:
  (1) путь `add_systems` молча терял scope-условия (документированный паттерн
  §6.0b не работал); (2) scope-условие никогда не сбрасывалось и «прилипало»
  ко всем последующим регистрациям. Регрессионный тест
  `scoped_condition_applies_to_add_systems_and_does_not_leak`.
- **`App` (apex-app)**: удалены `add_system`/`add_system_with`/`AppSystemBuilder`/
  `add_startup_system`/`add_sequential_system`/`add_exclusive_system` —
  остался единый `add_systems` (формы те же, что у Scheduler).
- **`World::query_typed::<Q>()` → `World::query::<Q>()`** (Bevy 1:1);
  динамический билдер `World::query()` → `World::query_builder()`.
- **Удалён `World::try_send_event`** — дубль `send_event` (всегда возвращал true).
- `add_systems(Startup, …)` после завершения Startup-этапа теперь предупреждает
  в лог (раньше предупреждали только удалённые `add_startup_*`).

### Changed — API golden path (2026-07-05, ядро не публиковалось — без deprecation-цикла)

- **Ренеймы под naming-канон** (`docs/CONVENTIONS.md`): `World::insert_raw_pub` →
  `insert_dyn`; `children_of` → `targets_of`, `get_relation_target` → `target_of`
  (World, SystemContext, RelationRegistry); `World/Commands::spawn_from_template` →
  `spawn_template_with` (пара к `spawn_template`); императивный
  `Scheduler::par_for_each_used_by_name` → декларативный
  `SystemConfig::par_for_each_used()`. `Ref<T>`-алиас удалён (был semantic-trap
  синоним `Read<T>`; имя зарезервировано под будущий change-detection-тип).
  Переходные `#[deprecated]`-алиасы удалены пред-релизно (все внутренние вызовы
  мигрированы; ядро не публиковалось — снос без major-bump).
- **`Query`-зоопарк схлопнут в единый `Query<'w, 's, D, F>`** (`CachedQuery` и
  view-часть `QueryState` убраны); read/write разделены по типам (`&self`-read
  требует `ReadOnlyWorldQuery`, `*_mut`-write эксклюзивны).
- **Событийная поверхность сужена**: `EventRegistry` → `pub(crate)`;
  `Events::{send_sync,send_batch_sync,flush_sync}` → `#[doc(hidden)]` (golden-path —
  `EventWriter<T>`-параметр).
- **`ErrorHandler` (§0.2a)**: per-World политика осознанных дропов
  (`Warn`/`Panic`/`Silent`/`Custom` + счётчики аномалий), `world.set_error_mode(..)`,
  `APEX_ERROR_MODE`.

### Added

- **Самодостаточные иерархические префабы (2026-06-22).** `PrefabChild` стал
  `#[serde(untagged)] enum { Ref { prefab, overrides } | Inline(PrefabManifest) }`:
  ребёнок — либо ссылка на именованный под-префаб (как раньше), либо **встроенное**
  поддерево. `WorldSerializer::hierarchy_to_prefab[_with]` теперь встраивает детей
  inline (раньше клалось только ИМЯ ребёнка, а суб-манифест терялся ⇒ `instantiate`
  падал с `SubPrefabNotFound`). Итог: иерархический префаб — один self-contained
  файл, инстанцируется через `PrefabLoader` без предзагрузки под-префабов. Формат
  обратно совместим со старыми файлами-ссылками. Тест
  `hierarchy_to_prefab_is_self_contained`; пример `prefab_isolated` теперь
  инстанцирует экспортированную иерархию (round-trip).
- **`for e in reader.read()` — прямая итерация по событиям (1:1 Bevy, TD-24
  движка, 2026-06-12).** `EventReadGuard` получил `IntoIterator` (владеющий
  `EventIterator<'q, T>`: отдаёт `&T`, advance курсора до конца буфера на drop —
  семантика guard'а сохранена, `break` пропускает остаток) и `IntoIterator`
  по ссылке (`for e in &guard` без потребления). `len()`/`is_empty()` доступны
  на guard'е через `Deref<[T]>`. `EventIterator`/`EventReadGuard` экспортированы
  из корня и prelude.

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
