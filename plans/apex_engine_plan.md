# Apex Engine — план разработки полноценного игрового движка

> На основе детального анализа всех крейтов ядра Apex ECS:
> `apex-core` (access, archetype, commands, component, entity, events, query, relations, resources, sub_world, system_param, template, transform, world),
> `apex-scheduler`, `apex-graph`, `apex-hot-reload`, `apex-serialization`, `apex-scripting`, `apex-isolated`.

---

## Часть I. Детальная оценка текущего ядра

### Полная карта возможностей

| Подсистема | Файл | Состояние | Ключевые особенности |
|---|---|---|---|
| **Access / Bitmask** | `access.rs` | ✅ Отлично | ComponentMask 256-бит, ArchetypeMask 1024-бит, O(1) конфликты, event-декларации |
| **Archetype Storage** | `archetype.rs` | ✅ Отлично | Column-store, Change ticks, sparse индексация |
| **Commands / Arena** | `commands.rs` | ✅ Отлично | Chunk-based bump arena, нет per-command Box, typed function pointers, `insert_raw` по ComponentId |
| **World** | `world.rs` | ✅ Отлично | QueryCache с версионированием, write hooks, ChunkConfig, TemplateRegistry встроен |
| **Query** | `query.rs` | ✅ Хорошо | WorldQuery trait, Changed<T>, Maybe<T>, component_arch_index для O(1) поиска |
| **Relations** | `relations.rs` | ✅ Отлично | Flecs-архитектура, encode_relation в ComponentId, SubjectIndex с kind_mask, Sparse/Dense auto-upgrade, cascade_delete, despawn_recursive, children_of, wildcard query |
| **Events** | `events.rs` | ✅ Хорошо | Per-stage flush, EventReader/EventWriter, EventPipeline |
| **Resources** | `resources.rs` | ✅ Хорошо | TypeId-based, Send+Sync |
| **Scheduler** | `scheduler/lib.rs` | ✅ Отлично | AutoSystem, ConflictKind с диагностикой, ASD-чанкование, инкрементальный граф |
| **Templates** | `template.rs` | ✅ Готово | EntityTemplate trait, TemplateParams по TypeId, impl_entity_template! макрос, интегрирован в Commands |
| **Serialization** | `serialization/` | ✅ Хорошо | PrefabManifest (JSON), PrefabLoader с кешем, overrides, иерархия через ChildOf, snapshot |
| **Hot-Reload** | `hot-reload/` | ✅ Хорошо | FileWatcher (notify), HotReloadPlugin, JsonConfigLoader, PrefabPlugin с reapply_asset/reapply_all, debounce |
| **Scripting (Lua)** | `scripting/` | ✅ Готово | ScriptEngine на mlua (Lua 5.4), ScriptableRegistrar, #[derive(Scriptable)], query/spawn/despawn/resources/events, commit/auto-commit, With/Without фильтры, sandbox _ENV, hot-reload .lua, inspect(), log_levels, Read-protection metatable |
| **SubWorld / Isolated** | `sub_world.rs`, `isolated/` | ✅ Готово | Изолированные миры для тестов и параллелизма |
| **Graph** | `graph/` | ✅ Готово | DAG, топосорт — используется планировщиком |
| **Transform** | `transform.rs` | ✅ Есть | Базовый трансформ |

### Что уже лучше конкурентов прямо сейчас

Это важно понимать, чтобы не потерять при разработке движка:

1. **AutoSystem с автовыводом access** — в Bevy это source of bugs, здесь compile-time гарантии
2. **ConflictKind с именованной диагностикой** — Bevy выдаёт panic без контекста, здесь WriteWrite(Position), EventWriteRead(DamageEvent)
3. **Commands через bump arena** — нет per-command Box allocation, при 10k+ команд выигрыш значительный
4. **Relations a-la Flecs** — встроены в ядро, а не надстройка; SubjectIndex с kind_mask, wildcard query, cascade delete — это уровень выше Bevy/Unity
5. **EntityTemplate + PrefabManifest** — два уровня шаблонов (программный + файловый), уже интегрированы с Commands и hot-reload
6. **PrefabPlugin.reapply_asset()** — пересоздание entity при изменении файла без перезапуска
7. **Scripting с change ticks** — изменения из Lua скриптов корректно видны Changed<T>; auto-commit, sandbox _ENV, With/Without фильтры — уровень выше типичных embedded scripting систем

### Известные ограничения

1. **ComponentMask ограничен 256 компонентами** — для движка с материалами, светом, UI, физикой это может стать потолком. Нужно расширить до 512 заранее
2. **Lua однопоточный** — ScriptEngine нельзя использовать в ParSystem; ScriptEngine не Send (Rc<RefCell<>>)
3. **QueryCache инвалидируется целиком** при любом структурном изменении — приемлемо сейчас, но потребует внимания при > 1000 архетипов
4. **PrefabManifest::spawn** с TemplateParams — обратный маппинг TypeId→name не реализован, overrides через params не работают (есть TODO в коде)
5. **Transform** базовый — нет иерархической пропагации GlobalTransform
6. **AssetId** в hot-reload — простой u32 без типизации; при строительстве AssetServer нужно добавить typed Handle<T>

---

## Часть II. Уроки из конкурентов

### Bevy — что взять, что не повторять

| Проблема Bevy | Как Apex уже решает / должен решить |
|---|---|
| Compile time 3–5 минут | Уже: минимальные derive-макросы. Дополнительно: чёткие границы crate |
| `#[derive(Component)]` bloat | Уже: только TypeId регистрация. Держать этот подход |
| Непонятный SystemParam | Уже: AutoSystem с явными `type Query`, `type Resources`, `type Events` |
| Тяжёлая `bevy_reflect` | Уже: лёгкая рефлексия через ComponentInfo::serde. НЕ делать её центральной |
| God-плагины | Решить: типизированные интерфейсы плагинов с чёткими хуками |
| Нестабильный API | Заморозить core API на v0.1, forward-compatibility слой |
| ECS как единственная парадигма | Уже: Relations + ChildOf = граф сцены. Дополнить: FSM из коробки |
| Отсутствие иерархического despawn | Уже: `despawn_recursive(ChildOf, root)` — встроен |

### Unity — что взять
- Инспектор по компонентам через сериализацию (основа уже есть в ComponentInfo::serde)
- Play/Stop/Pause без перезапуска процесса (PrefabPlugin.reapply_all() — уже есть механизм)
- Понятие "Scene" как файла

### Godot — что взять
- Hot-reload скриптов (уже есть в apex-scripting + apex-hot-reload)
- Сигналы = EventPipeline (уже есть)
- Простой скриптовый язык (Lua работает через mlua, нужно улучшить эргономику)

---

## Часть III. Архитектура Apex Engine

### Структура crate-ов

```
apex-ecs/           ← существующее ядро (API заморожен)
  ├── apex-core
  ├── apex-scheduler
  ├── apex-graph
  ├── apex-hot-reload
  ├── apex-serialization
  ├── apex-scripting
  ├── apex-macros
  └── apex-isolated

apex-engine/        ← новые crate движка
  ├── apex-app          — App-builder, Plugin trait, жизненный цикл
  ├── apex-window       — winit, управление окном
  ├── apex-input        — клавиатура, мышь, геймпад, ActionMap
  ├── apex-render       — wgpu, RenderGraph, материалы, меши
  ├── apex-asset        — AssetServer, типизированный Handle<T>, лоадеры
  ├── apex-scene        — Scene файлы, SceneManager, GlobalTransform
  ├── apex-audio        — kira, AudioEmitter, AudioListener
  ├── apex-physics      — rapier3d интеграция
  ├── apex-ui           — retained UI, taffy layout
  └── apex-editor       — egui редактор (dev-only feature)
```

### Принцип "слоёного лука" — без нарушения зависимостей

```
┌─────────────────────────────────────────┐
│             apex-editor                 │  dev-only, feature flag
├─────────────────────────────────────────┤
│   apex-ui  │  apex-physics  │  apex-net │  опциональные модули
├─────────────────────────────────────────┤
│    apex-render    │    apex-audio        │  обязательные для игры
├─────────────────────────────────────────┤
│    apex-asset     │    apex-scene        │  данные и иерархия
├─────────────────────────────────────────┤
│    apex-input     │    apex-window       │  платформа
├─────────────────────────────────────────┤
│              apex-app                   │  App, Plugin, жизненный цикл
├─────────────────────────────────────────┤
│ apex-core │ apex-scheduler │ apex-graph  │  неизменяемое ядро ECS
└─────────────────────────────────────────┘
```

### Желаемый финальный API

```rust
App::new()
    .add_plugin(WindowPlugin::default())
    .add_plugin(RenderPlugin::default())
    .add_plugin(AudioPlugin::default())
    .add_plugin(PhysicsPlugin::default())
    .add_systems(Update, (movement_system, animation_system))
    .add_systems(FixedUpdate, physics_step_system)
    .run();
```

### Главный цикл — физика и рендер разделены

```
Fixed tick (60 Hz):
    Input::poll() → Events flush → Physics step → Game Logic → Commands::apply()

Variable tick (vsync):
    Interpolate transforms → Extract ECS → Render → Audio
```

---

## Часть IV. Пошаговый план разработки

### Фаза 0 — Стабилизация ядра (1–2 недели)

Перед строительством движка — укрепить фундамент.

- [ ] **Расширить ComponentMask до 512 компонентов** (8×u64) — обратная совместимость через type alias
- [ ] **Починить PrefabManifest::spawn с TemplateParams** — реализовать обратный маппинг TypeId→name для overrides (есть TODO в коде)
- [ ] **Добавить GlobalTransform** — иерархическая пропагация из Transform + родительского GlobalTransform; система пропагации только для changed entity (Change Detection уже есть)
- [ ] **Покрыть тестами** ключевые пути: world.rs, archetype.rs, relations.rs, commands.rs — минимум 80% веток
- [ ] **Зафиксировать публичный API apex-core как v0.1** — никаких breaking changes без major

### Фаза 1 — App и Plugin система (1 неделя)

**apex-app** — точка входа всего движка.

- [ ] `Plugin` trait — `fn build(&self, app: &mut App)` — единственный метод
- [ ] `App` struct — владеет `World`, `Scheduler`, вектором плагинов
- [ ] `StageLabel` расширить: `PreUpdate`, `Update`, `PostUpdate`, `FixedUpdate`, `PreRender`, `Render`
- [ ] `App::add_systems(label, systems)` — делегирует в Scheduler
- [ ] `App::run()` — запускает main loop через winit EventLoop
- [ ] Типизированные Plugin интерфейсы: каждый плагин декларирует что он добавляет в `World` (ресурсы, системы, стадии) — никаких God-плагинов

### Фаза 2 — Окно и ввод (1 неделя)

**apex-window + apex-input**

- [ ] `WindowPlugin` — winit EventLoop, ресурс `Window { width, height, title }`
- [ ] winit события → Apex Events через существующий EventWriter (WindowResized, WindowClosed)
- [ ] `Input<KeyCode>`, `Input<MouseButton>` — ресурсы, обновляются в PreUpdate
- [ ] `MouseDelta`, `ScrollDelta` как Events
- [ ] `gilrs` для геймпада — `Input<GamepadButton>`, `Axis<GamepadAxis>`
- [ ] **`ActionMap`** — именованные действия вместо raw keycodes:
  ```rust
  ActionMap::new()
      .bind(Action::Jump, KeyCode::Space)
      .bind(Action::Jump, GamepadButton::South)
  ```
  Это критично: в Bevy смена кнопки требует рефакторинга всех систем

### Фаза 3 — Asset Pipeline (2 недели)

**apex-asset** — развитие существующего `apex-hot-reload`.

`AssetRegistry` и `HotReloadPlugin` уже есть — нужно надстроить над ними типизированный `AssetServer`.

- [ ] `Handle<T>` — `Arc`-based typed handle; drop = автоматический unload (ref-counted)
- [ ] `AssetServer` — централизованная загрузка, дедупликация по пути, `AssetRegistry` внутри
- [ ] `AssetLoader<T>` trait — пользователь пишет лоадер; стандартная реализация = расширение `ConfigLoader`
- [ ] Async загрузка — не блокировать game thread; `tokio::fs` или `smol`
- [ ] `AssetEvent<T>` — события загрузки/выгрузки/изменения (мост на существующий hot-reload)
- [ ] **Переиспользовать `PrefabPlugin`** — `AssetLoader<PrefabManifest>` делегирует в него
- [ ] Встроенные лоадеры: PNG/JPEG/KTX2 (Texture), GLTF (Mesh + Material), WAV/OGG (AudioClip), JSON/RON (Prefab)
- [ ] Release mode: **Asset Packing** — все assets в один `.pak`, `AssetServer` прозрачно читает из него

### Фаза 4 — Рендер (4–8 недель) ← Ключевая фаза

**apex-render** — самая сложная часть. Ошибки здесь дорого стоят.

**Принципы:**
- Рендер **полностью отделён** от ECS — читает через `SubWorld` (уже есть), не пишет в основной мир
- **RenderGraph** — DAG рендер-пасов, каждый пас независимая задача (можно использовать `apex-graph`)
- **Bindless** с первого дня (wgpu 0.20+) — один порядок производительности над Unity/Godot
- **ExtractSystem** — специальная стадия `PreRender`: копирует данные ECS → RenderWorld за O(changed)

**Структура:**
```
RenderWorld              — параллельный мир только для рендера
ExtractSystem            — ECS → RenderWorld (Transform, MeshHandle, MaterialHandle)
RenderGraph              — DAG пасов (использует apex-graph)
GpuResourceCache         — текстуры, буферы, шейдеры на GPU (keyed by Handle<T>)
MaterialSystem           — материалы как assets с hot-reload
BatchingSystem           — группировка по материалу для instancing
```

**Этапы:**
- [ ] wgpu инициализация, surface, swapchain → ресурс `GpuDevice` в World
- [ ] Mesh и Material как Asset types через `AssetServer`
- [ ] `ExtractSystem` — копирует `Transform` + `Handle<Mesh>` + `Handle<Material>` → RenderWorld; только changed entity благодаря Change Detection из ядра
- [ ] Forward renderer: Depth prepass → Opaque → Transparent
- [ ] PBR-lite материал: albedo, roughness, metallic, emissive
- [ ] Directional light + Shadow map (basic)
- [ ] **Instancing** — меши с одинаковым `Handle<Material>` батчатся автоматически через `wgpu::draw_indexed_indirect`
- [ ] **Frustum culling** на CPU — SIMD-friendly AABB через `glam`
- [ ] **Skinned mesh** (анимация) — позже, после базового рендера работает

**Что не делать:**
- Не начинать с собственного шейдерного языка
- Не делать OpenGL backend — только wgpu (Metal/Vulkan/DX12/WebGPU)
- Не смешивать RenderWorld и GameWorld

### Фаза 5 — Сцены и иерархия (1–2 недели)

**apex-scene** — расширение `apex-serialization`.

`ChildOf` relation уже есть в ядре, `PrefabManifest` уже поддерживает иерархию. Нужно надстроить:

- [ ] `GlobalTransform` компонент — вычисляется из `Transform` + родительский `GlobalTransform`
- [ ] Система пропагации — только для dirty entity (Changed<Transform>); уже есть механизм в ядре
- [ ] `Scene` = файл с набором prefab-ов + начальных ресурсов; загрузка = `AssetLoader<Scene>`
- [ ] `SceneManager` ресурс — `load_scene(handle)`, `unload_scene()`, аддитивная загрузка
- [ ] **Переиспользовать `PrefabLoader` + `PrefabPlugin`** — Scene = именованная коллекция prefab-ов

### Фаза 6 — Звук (1–2 недели)

**apex-audio**

- [ ] Backend: `kira` (Rust, async, production-ready)
- [ ] `AudioClip` как Asset type (загружается через `AssetServer`)
- [ ] Компоненты: `AudioEmitter { handle: Handle<AudioClip>, volume, looping, spatial }`; `AudioListener`
- [ ] 3D spatial audio — позиция AudioEmitter → параметры kira
- [ ] `AudioChannel` ресурс — независимые каналы SFX / Music / Voice с volume
- [ ] `AudioSystem` в PostUpdate — синхронизирует Transform AudioEmitter с kira

### Фаза 7 — Физика (2–4 недели)

**apex-physics** — интегрировать `rapier3d`, не писать свою.

- [ ] `RigidBody`, `Collider` компоненты → регистрируются в `rapier3d::PhysicsWorld` ресурсе
- [ ] `PhysicsPlugin` запускает step в `FixedUpdate` (гарантированный шаг — уже есть StageLabel)
- [ ] Sync системы: `SyncToRapier` (перед step) + `SyncFromRapier` (после step)
- [ ] `RayCastRequest` / `RayCastResult` через Commands + Events (не блокирующий API)
- [ ] `CollisionEvent { entity_a, entity_b, kind }` → Apex Events
- [ ] Переиспользовать `ChildOf` relation для составных коллайдеров

### Фаза 8 — UI (2–4 недели)

**apex-ui**

Два слоя:
- **egui** — редактор и debug UI; уже хорошо интегрируется с wgpu
- **Retained game UI** — для HUD, меню, диалогов

- [ ] `UiNode` компонент с `taffy` flexbox layout
- [ ] `UiPass` в RenderGraph — поверх игры, после основного рендера
- [ ] События UI → Apex Events (`ButtonPressed`, `InputChanged`, `SliderMoved`)
- [ ] Встроенные виджеты: Button, Label, Image, Panel, ScrollView, TextInput
- [ ] **Переиспользовать Relations** — `ChildOf` для иерархии UI-нод (бесплатно)
- [ ] Hot-reload UI-скинов через `HotReloadPlugin` (уже есть)

### Фаза 9 — Улучшение скриптинга (1–2 недели)

**apex-scripting** — доработка существующего.

**Уже сделано (миграция Rhai → Lua v0.1):**

- [x] **Lua 5.4 через `mlua`** — миграция с Rhai завершена, 16 API-функций
- [x] `#[derive(Scriptable)]` — named struct, tuple struct, unit struct (маркеры), C-like enum
- [x] `query({"Read:X", "Write:Y", "With:Z", "Without:W"})` — 4 режима доступа + кэш запросов
- [x] `commit(entity)` + `engine.set_auto_commit(true)` — явный и авто-режим
- [x] `spawn_entity`, `despawn`, `read_resource`, `write_resource`, `emit_event`
- [x] Sandbox `_ENV` — изоляция скриптов (только разрешённые функции)
- [x] Read-компонент `__newindex` metatable — предупреждает о попытке модификации
- [x] `inspect(table)`, `log_debug/warn/error` — отладка и логирование
- [x] Hot-reload `.lua` файлов с debounce 50ms
- [x] 13 автоматических тестов, покрывающих весь API

**Что осталось:**

- [ ] **WASM-скрипты через `wasmtime`** — для сложной логики с sandbox (опционально)
- [ ] **Сквозные ID для `spawn_entity`** — сейчас spawn отложенный, индекс не возвращается; нужен механизм временных ID с маппингом после apply
- [ ] **`EntityTemplate` + `PrefabManifest` из Lua** — вызов `world.spawn_from_template("Orc")` из скриптов
- [ ] **Несколько ScriptEngine в одном World** — каждый со своим Lua VM, независимые скрипты для разных подсистем

### Фаза 10 — Редактор (4–8 недель)

**apex-editor** — финальная часть, строится на всём предыдущем.

**Ключевой принцип:** редактор — это просто игра с дополнительным плагином.
```rust
App::new()
    .add_plugin(GamePlugin)
    .add_plugin(EditorPlugin)  // ← только это отличает debug-сборку
    .run();
```

- [ ] **Viewport** — рендер игры в egui texture, показывается как панель
- [ ] **Инспектор** — выбор entity → список компонентов → редактирование через `ComponentInfo::serde`; уже есть сериализация компонентов!
- [ ] **Иерархия сцены** — дерево entity с `ChildOf` relations (встроено в ядро)
- [ ] **Asset browser** — файловый браузер с превью; переиспользует `AssetRegistry`
- [ ] **Play / Pause / Stop** — fork world state → run → restore; `World::snapshot()` уже есть в `apex-serialization`!
- [ ] **Gizmo** — стрелки Transform, bounding boxes — отдельный RenderPass
- [ ] **Console** — вывод `log::`, Lua REPL (ScriptEngine уже работает)
- [ ] **Prefab editing** — открыть `.prefab.json` как отдельную сцену; `PrefabPlugin.reapply_asset()` уже есть
- [ ] **Live hot-reload в Play mode** — `HotReloadPlugin.apply_changes()` вызывается каждый кадр (уже архитектурно готово)

---

## Часть V. Поперечные принципы

### 1. Максимально переиспользовать существующее

Это самое важное. Многие вещи уже сделаны лучше чем кажется:

| Нужно для движка | Уже есть в ядре |
|---|---|
| Иерархия сцены (Parent/Children) | `ChildOf` relation + `children_of()` + `despawn_recursive()` |
| Play/Stop без рестарта | `WorldSerializer::snapshot()` + `restore()` |
| Hot-reload prefab-ов | `PrefabPlugin.reapply_asset()` + `reapply_all()` |
| Asset watching | `HotReloadPlugin` + `FileWatcher` + `AssetRegistry` |
| Программные шаблоны | `EntityTemplate` + `TemplateParams` + `Commands::spawn_template()` |
| Файловые шаблоны | `PrefabManifest` + `PrefabLoader` |
| Скриптинг | `ScriptEngine` + `#[derive(Scriptable)]` + hot-reload .lua |
| Граф задач | `apex-graph` DAG — использовать для RenderGraph |
| Изолированные миры | `apex-isolated` — для редактора и тестов |

### 2. Нулевая стоимость в горячих путях

- Query iteration, render batching — без dynamic dispatch
- Commands — уже без per-command Box благодаря bump arena
- ExtractSystem — только Changed entity

### 3. Диагностируемость из коробки

- `ConflictKind` уже показывает ПОЧЕМУ системы конфликтуют
- `APEX_LOG=trace` — каждый frame: какие системы, сколько entity, сколько времени
- Все `unwrap()` в публичном API → именованные ошибки через `thiserror` (уже используется)
- Debug overlay в редакторе: archetype count, entity count, frame time per system

### 4. Детерминизм

- Физика на фиксированном шаге (FixedUpdate stage уже в планировщике)
- `RngSeed` ресурс — `StdRng::seed_from_u64`
- Replay готов: зафиксированные шаги + детерминированный seed

### 5. WASM с первого дня

- `apex-core` не использует `std::thread::spawn` — уже хорошо
- wgpu поддерживает WebGPU — рендер в браузере
- `wasm32-unknown-unknown` target в CI

### 6. Минимальные зависимости

| Категория | Выбор | Причина |
|---|---|---|
| Математика | `glam` | Уже в проекте (SIMD), bytemuck для GPU |
| Async | `tokio` | Asset loading |
| Физика | `rapier3d` | Лучшая Rust физика |
| Звук | `kira` | Async, production-ready |
| Scripting | `mlua` (Lua 5.4) | Миграция с Rhai завершена |
| UI layout | `taffy` | flexbox |
| Редактор | `egui` | Простая интеграция с wgpu |
| Окно | `winit` | Стандарт |
| Геймпад | `gilrs` | Стандарт |

---

## Часть VI. Корректировки оригинального плана

После детального изучения всех крейтов — вот что изменилось по сравнению с первоначальным планом:

### Убрать из плана

- **"Добавить Parent/Children компоненты"** — НЕ нужно. `ChildOf` relation + `children_of()` + `despawn_recursive()` уже есть в ядре и работают. Bevy-style Parent/Children — лишняя дублирующая система
- **"Система пропагации трансформов с нуля"** — нужно только добавить `GlobalTransform` и систему; механика Change Detection уже работает
- **"Asset watching с нуля"** — `HotReloadPlugin` + `FileWatcher` + `AssetRegistry` уже есть. Нужно только надстроить типизированный `Handle<T>` и `AssetServer`
- **"Play/Stop через отдельный механизм"** — `WorldSerializer::snapshot()` + `restore()` уже есть в `apex-serialization`

### Добавить в план (не было в первой версии)

- **Починка PrefabManifest::spawn с TemplateParams** (Фаза 0) — в коде есть TODO, это реальный баг
- **ActionMap** (Фаза 2) — именованные действия вместо raw keycodes; это важнее чем кажется
- **Lua backend через mlua** (Фаза 9) — миграция с Rhai завершена; осталась полировка и WASM-опция
- **Явное переиспользование apex-graph для RenderGraph** — DAG уже написан
- **Sandbox для скриптов** (Фаза 9) — уже частично реализован, нужно довести

### Изменить приоритет

- **AssetServer (Фаза 3) перед рендером (Фаза 4)** — рендер зависит от типизированных Handle<Mesh>, Handle<Material>; без AssetServer рендер-код будет костылями
- **Скриптинг (Фаза 9) перед редактором (Фаза 10)**, не одновременно — редактор использует скриптинговый REPL

---

## Часть VII. Метрики успеха

| Метрика | Цель |
|---|---|
| 100k entity с Position+Velocity+Handle<Mesh> | < 2ms за frame (60Hz) |
| Время загрузки сцены средней сложности | < 500ms |
| Холодная сборка всего `apex-engine/` | < 90 секунд |
| Горячая пересборка после изменения одной системы | < 5 секунд |
| Время hot-reload .lua скрипта | < 50ms (debounce 50ms реализован) |
| Платформы | Windows, Linux, macOS, WebAssembly |
| Покрытие тестами `apex-core` | > 80% |

---

## Часть VIII. Временная дорожная карта

```
Неделя 1-2:  Фаза 0  — стабилизация ядра, GlobalTransform, фикс PrefabParams
Неделя 3:    Фаза 1  — App + Plugin система
Неделя 4:    Фаза 2  — окно, ввод, ActionMap
Неделя 5-6:  Фаза 3  — AssetServer над существующим hot-reload
Месяц 2-3:   Фаза 4  — рендер (forward, PBR-lite, instancing, frustum culling)
Месяц 4:     Фаза 5  — сцены (над существующим PrefabLoader)
Месяц 4:     Фаза 6  — звук (kira)
Месяц 5:     Фаза 7  — физика (rapier3d)
Месяц 5-6:   Фаза 8  — UI (egui + retained)
Месяц 6:     Фаза 9  — скриптинг улучшения (Lua backend)
Месяц 7-8:   Фаза 10 — редактор
Месяц 9+:    Deferred renderer, анимации, сеть, мобильные платформы
```

---

## Приложение: Ключевые зависимости

```toml
[workspace.dependencies]
# Платформа
winit       = "0.30"
wgpu        = "0.20"
gilrs       = "0.10"

# Математика (уже частично в проекте)
glam        = { version = "0.28", features = ["bytemuck"] }

# Физика
rapier3d    = "0.21"

# Звук
kira        = "0.9"

# Скриптинг
mlua        = { version = "0.10", features = ["lua54", "vendored"] }

# UI
egui        = "0.28"
taffy       = "0.5"

# Async
tokio       = { version = "1", features = ["rt-multi-thread", "fs"] }

# Asset loading
image       = "0.25"
gltf        = "1.4"
rodio       = "0.17"   # для WAV/OGG декодирования до kira

# Уже в проекте — не менять
rustc-hash  = "1.1"
smallvec    = "1.11"
rayon       = "1.8"
serde       = { version = "1", features = ["derive"] }
notify      = "6"
thunderdome = "*"
thiserror   = "*"
```
