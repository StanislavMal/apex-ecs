# apex-ecs — План реализации оставшихся пунктов

> Документ составлен: май 2026  
> Версия проекта: 0.1.0 (post-refactor, 153 теста проходят)  
> Статус: 5 нереализованных пунктов + 1 требует нового подхода (3.5 откат)

---

## Содержание

1. [3.3 — Калибровка `adaptive_chunk_size`](#33--калибровка-adaptive_chunk_size)
2. [4.2 — Bundle более 8 компонентов](#42--bundle-более-8-компонентов)
3. [4.10 — `apex-macros`: derive-макрос `Component`](#410--apex-macros-derive-макрос-component)
4. [R3 — Устранение задержки событий в EventPipeline](#r3--устранение-задержки-событий-в-eventpipeline)
5. [R4 — Оптимизация `all_readers_caught_up()`](#r4--оптимизация-all_readers_caught_up)
6. [3.5 — Новый подход к `compute_archetype_indices`](#35--новый-подход-к-compute_archetype_indices-после-отката)
7. [Порядок реализации и зависимости](#порядок-реализации-и-зависимости)

---

## 3.3 — Калибровка `adaptive_chunk_size`

### Проблема

Текущий код в `apex-core/src/world.rs`:

```rust
let dynamic_min = if entity_count < 100 {
    128
} else if entity_count < 1000 {
    32
} else {
    64
};
```

При `entity_count < 100` минимум 128 > entity_count → всегда один чанк → параллелизм отключён для малых миров, даже при 8 потоках. Пороги 100/1000 выбраны произвольно без бенчмарков.

### Решение

Заменить магические пороги на формулу, учитывающую реальное число потоков. Добавить возможность ручной калибровки через параметры в `SchedulerConfig`.

#### Шаг 1 — Ввести структуру конфигурации планировщика

**Файл:** `apex-scheduler/src/lib.rs` (или новый `apex-scheduler/src/config.rs`)

```rust
/// Конфигурация стратегии параллельного чанкования.
/// Передаётся в `Scheduler::new_with_config()`.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Минимальное число entity на поток, ниже которого параллелизм не выгоден.
    /// Default: 16. Калибруется бенчмарками под целевое железо.
    pub min_entities_per_thread: usize,

    /// Максимальный размер чанка (ограничитель роста при huge worlds).
    /// Default: 4096.
    pub max_chunk_size: usize,

    /// Если true — всегда использовать один чанк для N < min_entities_per_thread * threads.
    /// Если false — всегда разбивать на threads чанков (даже мелких).
    pub auto_serial_fallback: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            min_entities_per_thread: 16,
            max_chunk_size: 4096,
            auto_serial_fallback: true,
        }
    }
}
```

#### Шаг 2 — Переписать `adaptive_chunk_size`

**Файл:** `apex-core/src/world.rs`

Убрать функцию `adaptive_chunk_size` или переписать её сигнатуру:

```rust
/// Вычисляет оптимальный размер чанка для параллельной итерации.
///
/// Логика:
/// 1. Если entity_count < min_entities_per_thread * thread_count — один чанк (serial fallback).
/// 2. Иначе — делим на thread_count, округляем вверх, зажимаем в [1, max_chunk_size].
pub fn adaptive_chunk_size(
    entity_count: usize,
    thread_count: usize,
    config: &ChunkConfig,
) -> usize {
    if entity_count == 0 {
        return 1;
    }
    let serial_threshold = config.min_entities_per_thread.saturating_mul(thread_count);
    if config.auto_serial_fallback && entity_count < serial_threshold {
        // Весь мир — один чанк, идёт в sequential
        return entity_count;
    }
    let raw = (entity_count + thread_count - 1) / thread_count;
    raw.clamp(1, config.max_chunk_size)
}
```

Преимущества:
- При 8 потоках и 99 entity: threshold = 16×8 = 128 > 99 → serial (как и сейчас, но явно).
- При 8 потоках и 200 entity: threshold = 128 < 200 → chunk = ceil(200/8) = 25. Раньше было 128 → один чанк.
- `min_entities_per_thread = 16` — типичная cache line / SIMD-порция, легко обоснована.

#### Шаг 3 — Добавить бенчмарки для калибровки

**Файл:** `apex-core/benches/chunk_size.rs` (новый файл)

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

fn bench_chunk_variants(c: &mut Criterion) {
    let mut group = c.benchmark_group("adaptive_chunk_size");
    for &n in &[50usize, 100, 500, 1000, 5000, 10000, 100000] {
        for &threads in &[1usize, 2, 4, 8] {
            group.bench_with_input(
                BenchmarkId::new(format!("t{threads}"), n),
                &(n, threads),
                |b, &(n, t)| {
                    b.iter(|| {
                        // Запускаем реальный параллельный query с данным chunk_size
                        // (заглушка — заменить на реальный World::par_query_bench)
                        let chunk = adaptive_chunk_size(n, t, &ChunkConfig::default());
                        criterion::black_box(chunk)
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, bench_chunk_variants);
criterion_main!(benches);
```

Запуск: `cargo bench -p apex-core --bench chunk_size`  
По результатам скорректировать `min_entities_per_thread` в `ChunkConfig::default()`.

#### Шаг 4 — Пробросить `ChunkConfig` в Scheduler и World

В `Scheduler::new_with_config(config: SchedulerConfig)` добавить поле `chunk_config: ChunkConfig`. При вызове `make_sub_world` / параллельных итераций передавать `chunk_config` в `World::par_iter`.

#### Тесты

```rust
#[test]
fn chunk_size_serial_fallback() {
    let cfg = ChunkConfig { min_entities_per_thread: 16, max_chunk_size: 4096, auto_serial_fallback: true };
    // 8 потоков, 99 entity — ниже порога (128) → serial (один чанк)
    assert_eq!(adaptive_chunk_size(99, 8, &cfg), 99);
    // 8 потоков, 200 entity — выше порога → 25
    assert_eq!(adaptive_chunk_size(200, 8, &cfg), 25);
    // Краевой: 0 entity
    assert_eq!(adaptive_chunk_size(0, 8, &cfg), 1);
}

#[test]
fn chunk_size_respects_max() {
    let cfg = ChunkConfig { min_entities_per_thread: 1, max_chunk_size: 100, auto_serial_fallback: false };
    // 1000 entity, 1 поток → без ограничения было бы 1000, но clamp до 100
    assert_eq!(adaptive_chunk_size(1000, 1, &cfg), 100);
}
```

---

## 4.2 — Bundle более 8 компонентов

### Проблема

`impl_bundle!` раскрывается для кортежей `(A)...(A,B,C,D,E,F,G,H)`. Жёсткий предел 8. Для игровых сущностей (персонаж с Transform, Mesh, Material, Health, Velocity, Collider, AI, Team, Inventory…) нужно 10–16.

### Решение

Реализовать процедурный макрос `#[derive(Bundle)]` в крейте `apex-macros`. Это устраняет предел полностью и синергирует с задачей 4.10.

> **Зависимость:** задача 4.2 выгодно объединить с 4.10 — они оба требуют `apex-macros`. Реализовывать совместно.

#### Шаг 1 — Подготовить крейт `apex-macros`

`apex-macros/Cargo.toml`:

```toml
[lib]
proc-macro = true

[dependencies]
syn = { version = "2", features = ["full"] }
quote = "1"
proc-macro2 = "1"
```

#### Шаг 2 — Определить трейт `Bundle` с необходимыми ассоциированными методами

**Файл:** `apex-core/src/bundle.rs`

Трейт должен предоставлять всё, что нужно макросу для генерации:

```rust
pub trait Bundle: Sized + 'static {
    /// Возвращает список TypeId компонентов (для регистрации и построения архетипа).
    fn component_ids(registry: &mut ComponentRegistry) -> Vec<ComponentId>;

    /// Записывает компоненты в колонки архетипа по переданным индексам.
    /// `row` — строка (entity index) внутри архетипа.
    ///
    /// # Safety
    /// `columns[i]` должен соответствовать `component_ids()[i]`.
    unsafe fn write_into_columns(self, columns: &mut [Column], row: usize, tick: u32);

    /// Возвращает true если хотя бы один компонент имеет Drop (нужно для spawn_many).
    fn needs_drop() -> bool;
}
```

#### Шаг 3 — Реализовать derive-макрос `Bundle`

**Файл:** `apex-macros/src/bundle.rs`

```rust
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

pub fn derive_bundle_impl(input: DeriveInput) -> TokenStream {
    let name = &input.ident;
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
            Fields::Unnamed(f) => f.unnamed.iter().collect::<Vec<_>>(),
            Fields::Unit => vec![],
        },
        _ => panic!("#[derive(Bundle)] поддерживает только struct"),
    };

    // Генерируем имена полей (для named) или индексы (для tuple struct)
    let field_accessors: Vec<TokenStream> = fields.iter().enumerate().map(|(i, f)| {
        if let Some(ident) = &f.ident {
            quote! { self.#ident }
        } else {
            let idx = syn::Index::from(i);
            quote! { self.#idx }
        }
    }).collect();

    let field_types: Vec<&syn::Type> = fields.iter().map(|f| &f.ty).collect();
    let field_count = fields.len();

    quote! {
        impl ::apex_core::bundle::Bundle for #name {
            fn component_ids(registry: &mut ::apex_core::ComponentRegistry) -> Vec<::apex_core::ComponentId> {
                vec![
                    #( registry.get_or_register::<#field_types>() ),*
                ]
            }

            unsafe fn write_into_columns(
                self,
                columns: &mut [::apex_core::Column],
                row: usize,
                tick: u32,
            ) {
                assert_eq!(columns.len(), #field_count, "Bundle: несоответствие числа колонок");
                let mut _i = 0usize;
                #(
                    {
                        let col = &mut columns[_i];
                        col.write(row, #field_accessors, tick);
                        _i += 1;
                    }
                )*
            }

            fn needs_drop() -> bool {
                #( ::std::mem::needs_drop::<#field_types>() )||*
            }
        }
    }
}
```

**Файл:** `apex-macros/src/lib.rs`

```rust
use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput};

mod bundle;
mod component; // для задачи 4.10

#[proc_macro_derive(Bundle)]
pub fn derive_bundle(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    bundle::derive_bundle_impl(input).into()
}
```

#### Шаг 4 — Удалить `impl_bundle!`, перевести весь код на derive

Старые макросы `impl_bundle!` для кортежей `(A)..(A,...,H)` **удалить** — они не нужны, так как движок не имеет пользователей. Все внутренние использования (тесты, примеры, сам код) перевести на `#[derive(Bundle)]`. Это устраняет дублирование и упрощает поддержку.

#### Шаг 5 — Обновить `World::spawn` и `spawn_many`

`spawn` должен принимать `impl Bundle`. Проверить, что все вызовы в тестах компилируются как с кортежами, так и с derive-структурами.

#### Тесты

```rust
#[derive(Bundle)]
struct PlayerBundle {
    pos: Position,
    vel: Velocity,
    health: Health,
    mesh: MeshHandle,
    material: MaterialHandle,
    collider: Collider,
    ai: AiState,
    team: Team,
    inventory: Inventory,
    name: EntityName,  // 10 компонентов — раньше невозможно
}

#[test]
fn bundle_derive_10_components() {
    let mut world = World::new();
    let entity = world.spawn(PlayerBundle {
        pos: Position { x: 0.0, y: 0.0 },
        // ...
    });
    assert!(world.has_component::<Position>(entity));
    assert!(world.has_component::<EntityName>(entity));
}

#[test]
fn bundle_derive_needs_drop() {
    #[derive(Bundle)]
    struct WithString { name: EntityName, pos: Position }
    // EntityName содержит String → needs_drop = true
    assert!(WithString::needs_drop());
}
```

---

## 4.10 — `apex-macros`: derive-макрос `Component`

### Проблема

Пользователи вынуждены вручную вызывать `world.register_component::<T>()` для каждого компонента. Нет `#[derive(Component)]`.

### Решение

Реализовать `#[derive(Component)]` с авторегистрацией через [`linkme`](https://crates.io/crates/linkme) — статические distributed slices, собираемые линкером без runtime overhead. Это надёжнее `inventory` на большинстве платформ (включая WASM с осторожностью).

#### Шаг 1 — Добавить зависимость `linkme`

`apex-core/Cargo.toml`:

```toml
[dependencies]
linkme = "0.3"
```

`apex-macros/Cargo.toml`:

```toml
[dependencies]
linkme = "0.3"
# ... syn, quote, proc-macro2
```

#### Шаг 2 — Определить distributed slice для авторегистраторов

**Файл:** `apex-core/src/component.rs`

```rust
use linkme::distributed_slice;

/// Тип функции-регистратора: принимает мутабельный реестр, регистрирует компонент.
pub type ComponentRegistrarFn = fn(&mut ComponentRegistry);

/// Глобальный список всех авторегистраторов компонентов.
/// Заполняется линкером из всех крейтов, использующих #[derive(Component)].
#[distributed_slice]
pub static COMPONENT_REGISTRARS: [ComponentRegistrarFn] = [..];

impl ComponentRegistry {
    /// Регистрирует все компоненты, объявленные через #[derive(Component)].
    /// Вызывать один раз при создании World.
    pub fn register_all_auto(&mut self) {
        for registrar in COMPONENT_REGISTRARS {
            registrar(self);
        }
    }
}
```

#### Шаг 3 — Реализовать derive-макрос `Component`

**Файл:** `apex-macros/src/component.rs`

```rust
use proc_macro2::TokenStream;
use quote::quote;
use syn::DeriveInput;

pub fn derive_component_impl(input: DeriveInput) -> TokenStream {
    let name = &input.ident;
    // Уникальное имя статического регистратора (избегаем коллизий имён)
    let registrar_ident = quote::format_ident!("__COMPONENT_REGISTRAR_{}", name);

    quote! {
        // Маркерная реализация трейта Component (если не реализован вручную)
        impl ::apex_core::Component for #name {}

        // Статический регистратор, собираемый линкером
        #[::linkme::distributed_slice(::apex_core::component::COMPONENT_REGISTRARS)]
        #[linkme(crate = ::linkme)]
        static #registrar_ident: ::apex_core::component::ComponentRegistrarFn =
            |registry: &mut ::apex_core::ComponentRegistry| {
                registry.get_or_register::<#name>();
            };
    }
}
```

**Файл:** `apex-macros/src/lib.rs` (дополнение):

```rust
#[proc_macro_derive(Component)]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    component::derive_component_impl(input).into()
}
```

#### Шаг 4 — Вызов авторегистрации при создании World

**Файл:** `apex-core/src/world.rs`

```rust
impl World {
    pub fn new() -> Self {
        let mut registry = ComponentRegistry::new();
        registry.register_all_auto();  // ← регистрирует все #[derive(Component)]
        Self {
            registry,
            // ...
        }
    }
}
```

#### Шаг 5 — Документировать ограничения

В `README.md` и в doc-комментарии к `#[derive(Component)]` явно написать:

- Авторегистрация работает только если крейт с компонентом **слинкован** в финальный бинарь (не только в зависимостях `[dev-dependencies]`). LTO с `codegen-units = 1` не должен выбрасывать `distributed_slice`.
- На WASM: `linkme` поддерживает `wasm32-unknown-unknown` начиная с версии 0.3.x — проверить при таргетинге WASM.
- Ручной вызов `world.register_component::<T>()` остаётся рабочим для динамических компонентов (скриптинг, горячая перезагрузка).

#### Тесты

```rust
#[derive(Component, Debug, PartialEq)]
struct AutoRegistered {
    value: i32,
}

#[test]
fn component_auto_registration() {
    // World::new() вызывает register_all_auto()
    let world = World::new();
    // AutoRegistered должен быть зарегистрирован без ручного вызова
    assert!(world.registry.get_id::<AutoRegistered>().is_some());
}

#[test]
fn component_manual_registration_still_works() {
    let mut world = World::new();
    // Повторная регистрация idempotent — не паникует, возвращает тот же ID
    let id1 = world.register_component::<AutoRegistered>();
    let id2 = world.register_component::<AutoRegistered>();
    assert_eq!(id1, id2);
}
```

---

## R3 — Устранение задержки событий в EventPipeline

### Проблема

Текущая архитектура:

```
world.tick()           ← swap буферов (pending → events)
sched.run()
  Stage 0: EmitSystem  → пишет в Events::pending
  Stage 1: ListenSystem → читает из Events::events (данные ПРОШЛОГО тика)
```

`ListenSystem` видит события с задержкой **1 тик**. `EventPipelineBuilder` гарантирует порядок Stage, но не устраняет задержку. Это задокументировано, но семантически неправильно для синхронных пайплайнов.

### Решение

Перенести `events.update()` из `World::tick()` в конец Stage с Emit-системами. Для этого Scheduler должен знать, какие типы событий эмитирует каждый Stage, и вызывать точечный flush после завершения Stage.

#### Архитектурное решение: per-Stage event flush

Ключевой принцип: **`world.tick()` больше не трогает события**. Flush событий — ответственность Scheduler после каждого Stage.

#### Шаг 1 — Ввести метод `World::flush_events_by_type`

**Файл:** `apex-core/src/world.rs`

```rust
impl World {
    /// Вызывает `update()` для конкретных типов событий (по TypeId).
    /// Используется Scheduler для flush после Stage с Emit-системами.
    pub fn flush_events_by_type(&mut self, type_ids: &[TypeId]) {
        for tid in type_ids {
            if let Some(queue) = self.event_registry.get_mut_by_typeid(*tid) {
                queue.update();
            }
        }
    }

    /// Flush ВСЕХ событий (используется в World::tick() если Scheduler не используется).
    pub fn flush_all_events(&mut self) {
        self.event_registry.flush_all();
    }
}
```

#### Шаг 2 — Собирать информацию об event-write доступе в Stage при компиляции

**Файл:** `apex-scheduler/src/lib.rs`

При `Scheduler::compile()` для каждого Stage собирать множество `write_event_types: HashSet<TypeId>` из `AccessDescriptor` всех систем Stage.

```rust
#[derive(Debug)]
struct CompiledStage {
    system_indices: Vec<usize>,
    /// TypeId событий, которые системы этого Stage могут писать (emit).
    /// После выполнения Stage — flush именно этих типов.
    emit_event_types: Vec<TypeId>,
}
```

При `compile()`:

```rust
for stage in &mut compiled_stages {
    let mut emit_types = HashSet::new();
    for &sys_idx in &stage.system_indices {
        for tid in &self.systems[sys_idx].access.event_write_types {
            emit_types.insert(*tid);
        }
    }
    stage.emit_event_types = emit_types.into_iter().collect();
}
```

#### Шаг 3 — Вызывать per-Stage flush в `Scheduler::run()`

**Файл:** `apex-scheduler/src/lib.rs`

```rust
pub fn run(&mut self, world: &mut World) {
    for stage in &self.compiled_stages {
        self.execute_stage(stage, world);

        // После Stage — flush только тех событий, которые Stage мог эмитировать.
        // Следующий Stage увидит их без задержки.
        if !stage.emit_event_types.is_empty() {
            world.flush_events_by_type(&stage.emit_event_types);
        }
    }
}
```

#### Шаг 4 — Убрать flush событий из `World::tick()`

**Файл:** `apex-core/src/world.rs`

```rust
impl World {
    /// Продвигает глобальный tick. НЕ делает flush событий — это делает Scheduler.
    /// Для использования без Scheduler вызывайте `flush_all_events()` вручную.
    pub fn tick(&mut self) {
        self.current_tick = self.current_tick.wrapping_add(1);
        // ← flush_all_events() УДАЛЁН отсюда
    }
}
```

**Важно:** Пользователи, использующие `World` без `Scheduler` (тесты, кастомные рантаймы), должны вручную вызывать `world.flush_all_events()` — это документируется в doc-комментарии к `World::tick()`.

#### Шаг 5 — Обновить `AccessDescriptor`

**Файл:** `apex-core/src/access.rs` (или `apex-scheduler/src/lib.rs`)

```rust
#[derive(Default, Clone, Debug)]
pub struct AccessDescriptor {
    pub reads: ComponentMask,
    pub writes: ComponentMask,
    pub event_read_types: Vec<TypeId>,
    pub event_write_types: Vec<TypeId>,  // ← уже есть (или добавить)
    pub event_reserves: Vec<(TypeId, usize)>,
}
```

Системы должны декларировать event-write через `AccessDescriptor::event_write::<T>()` — аналогично уже реализованному `event_reserve`.

#### Тесты

```rust
#[test]
fn event_pipeline_no_frame_delay() {
    let mut world = World::new();
    world.register_component::<Position>();

    let mut sched = Scheduler::new();
    // Stage 0: эмит события
    sched.add_system(|events: EventWriter<DamageEvent>| {
        events.send(DamageEvent { amount: 42 });
    });
    // Stage 1: чтение того же тика
    let received = Arc::new(AtomicI32::new(0));
    let received_clone = received.clone();
    sched.add_system(move |events: EventReader<DamageEvent>| {
        for ev in events.read() {
            received_clone.fetch_add(ev.amount, Ordering::SeqCst);
        }
    });
    sched.compile().unwrap();

    world.tick();
    sched.run(&mut world);  // Stage 0 emit → flush → Stage 1 read

    // БЕЗ задержки: ListenSystem видит событие ТОГО ЖЕ тика
    assert_eq!(received.load(Ordering::SeqCst), 42);
}

#[test]
fn world_without_scheduler_requires_manual_flush() {
    let mut world = World::new();
    let mut writer = world.get_event_writer::<DamageEvent>();
    writer.send(DamageEvent { amount: 10 });

    // БЕЗ flush — события не видны
    let reader = world.get_event_reader::<DamageEvent>();
    assert_eq!(reader.iter().count(), 0);

    world.flush_all_events();

    // ПОСЛЕ flush — видны
    let reader = world.get_event_reader::<DamageEvent>();
    assert_eq!(reader.iter().count(), 1);
}
```

---

## R4 — Оптимизация `all_readers_caught_up()`

### Проблема

Текущая реализация:

```rust
pub fn all_readers_caught_up(&self) -> bool {
    let event_count = self.events.len() as u32;
    self.cursors.iter().flatten().all(|&pos| pos >= event_count)
}
```

O(R) при каждом вызове, где R = число читателей. При R=100 и вызове каждый тик — 100 итераций при потенциально горячем пути (проверка перед swap буферов в `update()`).

### Решение

Хранить счётчик «отстающих читателей» (`lagging_count: u32`), обновляемый инкрементально при операциях с курсорами. `all_readers_caught_up()` → O(1).

#### Шаг 1 — Добавить поле в `EventQueue<T>`

**Файл:** `apex-core/src/events.rs`

```rust
pub struct EventQueue<T> {
    events: Vec<T>,
    pending: Vec<T>,
    cursors: Vec<Option<u32>>,
    free_list: Vec<EventCursor>,
    next_cursor_id: u32,
    sync_pending: OnceLock<Mutex<Vec<T>>>,
    /// Число активных читателей, у которых позиция курсора < events.len().
    /// Инвариант: lagging_count == cursors.iter().flatten().filter(|&&p| p < events.len()).count()
    lagging_count: u32,
}
```

#### Шаг 2 — Поддерживать инвариант во всех точках мутации

Нужно обновлять `lagging_count` в:

**`add_reader()`** — новый читатель добавляется с позицией 0. Если `events.len() > 0` → он отстающий:

```rust
pub fn add_reader(&mut self) -> EventCursor {
    let cursor = /* existing logic */;
    // Если в events есть данные — новый читатель сразу отстаёт
    if !self.events.is_empty() {
        self.lagging_count += 1;
    }
    cursor
}
```

**`remove_reader()`** — если читатель был отстающим (pos < events.len()) → декремент:

```rust
pub fn remove_reader(&mut self, reader_id: EventCursor) {
    let idx = reader_id.0 as usize;
    if let Some(Some(pos)) = self.cursors.get(idx) {
        if (*pos as usize) < self.events.len() {
            self.lagging_count = self.lagging_count.saturating_sub(1);
        }
    }
    // ... rest of existing logic
}
```

**Продвижение курсора** (в `EventReadGuard::drop` / `read_partial`) — при достижении `events.len()`:

```rust
fn advance_cursor(&mut self, cursor_id: EventCursor, new_pos: u32) {
    let idx = cursor_id.0 as usize;
    if let Some(slot) = self.cursors.get_mut(idx).and_then(|s| s.as_mut()) {
        let old_pos = *slot;
        let event_count = self.events.len() as u32;
        // Был отстающим, стал догнавшим?
        if old_pos < event_count && new_pos >= event_count {
            self.lagging_count = self.lagging_count.saturating_sub(1);
        }
        *slot = new_pos;
    }
}
```

**`update()`** — после swap буферов все курсоры нужно пересчитать:

```rust
pub fn update(&mut self) {
    self.flush_sync(); // sync_pending → pending
    // swap: events ← pending
    std::mem::swap(&mut self.events, &mut self.pending);
    self.pending.clear(); // pending теперь пуст

    // После swap: все курсоры отстают от новых events
    // Пересчитываем lagging_count за O(R)
    let new_event_count = self.events.len() as u32;
    self.lagging_count = self.cursors.iter()
        .flatten()
        .filter(|&&pos| pos < new_event_count)
        .count() as u32;
}
```

> **Замечание:** `update()` всё равно O(R) — но это вызывается 1 раз за тик. Зато `all_readers_caught_up()` (может вызываться многократно) становится O(1).

#### Шаг 3 — Переписать `all_readers_caught_up()`

```rust
#[inline]
pub fn all_readers_caught_up(&self) -> bool {
    self.lagging_count == 0
}
```

#### Шаг 4 — Добавить debug-assertion для проверки инварианта

```rust
#[cfg(debug_assertions)]
fn assert_lagging_invariant(&self) {
    let actual = self.cursors.iter()
        .flatten()
        .filter(|&&p| (p as usize) < self.events.len())
        .count() as u32;
    assert_eq!(
        self.lagging_count, actual,
        "lagging_count инвариант нарушен: stored={}, actual={}",
        self.lagging_count, actual
    );
}
```

Вызывать в `update()`, `add_reader()`, `remove_reader()` в debug-сборках.

#### Тесты

```rust
#[test]
fn all_readers_caught_up_o1() {
    let mut queue: EventQueue<i32> = EventQueue::new();
    let c1 = queue.add_reader();
    let c2 = queue.add_reader();

    queue.send(1);
    queue.send(2);
    queue.update();

    assert!(!queue.all_readers_caught_up()); // оба отстают

    // c1 читает всё
    let _guard = queue.read(&c1);
    drop(_guard);
    assert!(!queue.all_readers_caught_up()); // c2 ещё отстаёт

    // c2 читает всё
    let _guard = queue.read(&c2);
    drop(_guard);
    assert!(queue.all_readers_caught_up()); // все догнали
}

#[test]
fn lagging_count_invariant_on_add_remove() {
    let mut queue: EventQueue<i32> = EventQueue::new();
    queue.send(1);
    queue.update();

    let c = queue.add_reader(); // добавляем после send — отстаёт
    assert_eq!(queue.lagging_count, 1);

    queue.remove_reader(c);
    assert_eq!(queue.lagging_count, 0);
}
```

---

## 3.5 — Новый подход к `compute_archetype_indices` (после отката)

### Контекст

Исходная попытка заменить `any()` на `all()` вызвала регрессию: системы с `(Read<Vel>, Write<Pos>)` переставали видеть архетипы, содержащие только часть компонентов в разных SubWorld. Откат к `any()` восстановил поведение.

### Правильное решение

Применять разные критерии для разных целей использования архетипов:

- **Для row-level split (SubWorld boundaries):** `any()` — правильно. Архетип попадает в SubWorld системы, если содержит хотя бы один компонент.
- **Для conflict detection (определение конфликтующих систем):** проверять write-компоненты через `all()` только для WriteWrite-пар.
- **Для Query matching (runtime фильтрация):** `Q::matches_archetype()` — уже реализовано, не трогать.

Таким образом, проблема 3.5 — это **не проблема `compute_archetype_indices`**, а проблема того, что одна функция использовалась для двух разных задач с разными инвариантами.

#### Шаг 1 — Разделить `compute_archetype_indices` на две функции

**Файл:** `apex-scheduler/src/lib.rs`

```rust
/// Возвращает индексы архетипов для SubWorld системы.
/// Критерий: `any()` — архетип содержит хотя бы один компонент из системы.
/// Используется для построения SubWorld (row-level split).
fn archetype_indices_for_subworld(
    system_type_ids: &[TypeId],
    archetypes: &[Archetype],
    registry: &ComponentRegistry,
) -> Vec<usize> {
    archetypes.iter().enumerate()
        .filter(|(_, arch)| system_type_ids.iter().any(|tid| {
            registry.get_id_by_type(tid)
                .map(|cid| arch.has_component(cid))
                .unwrap_or(false)
        }))
        .map(|(i, _)| i)
        .collect()
}

/// Возвращает индексы архетипов, к которым система имеет WRITE-доступ.
/// Критерий: `all()` write-компоненты — для точного определения конфликтов.
/// Используется ТОЛЬКО в conflict detection.
fn archetype_indices_for_conflict_detection(
    write_type_ids: &[TypeId],
    archetypes: &[Archetype],
    registry: &ComponentRegistry,
) -> Vec<usize> {
    if write_type_ids.is_empty() {
        return vec![];
    }
    archetypes.iter().enumerate()
        .filter(|(_, arch)| write_type_ids.iter().all(|tid| {
            registry.get_id_by_type(tid)
                .map(|cid| arch.has_component(cid))
                .unwrap_or(false)
        }))
        .map(|(i, _)| i)
        .collect()
}
```

#### Шаг 2 — Использовать правильную функцию в каждом месте

В `make_sub_world`: использовать `archetype_indices_for_subworld` (как раньше, `any()`).

В `detect_conflict_kind` / `add_new_nodes_and_edges`: использовать `archetype_indices_for_conflict_detection` для определения пересечений write-архетипов между системами.

#### Шаг 3 — Тест на регрессию параллелизма

```rust
#[test]
fn no_false_conflict_vel_pos_systems() {
    // SystemA: Read<Vel>, Write<Pos>
    // SystemB: Read<Pos>, Write<Vel>
    // Реальный конфликт: A читает Vel (B пишет Vel), B читает Pos (A пишет Pos)
    // → должен быть WriteRead конфликт в обе стороны, но не ложный цикл
    let mut sched = Scheduler::new();
    sched.add_par_system(system_a);
    sched.add_par_system(system_b);
    // Не должно быть CircularDependency, должен быть LinearOrder
    let result = sched.compile();
    assert!(result.is_ok(), "Ложный цикл в conflict detection: {:?}", result);
}
```

---

## Порядок реализации и зависимости

```
Зависимости между задачами:

4.10 (apex-macros Component derive)
  └── 4.2 (Bundle derive) — оба требуют apex-macros, реализовывать совместно

3.3 (adaptive_chunk_size) — независима, можно начинать в любой момент

R4 (all_readers_caught_up O(1)) — независима, изолированные изменения в events.rs

R3 (per-Stage event flush) — требует изменений в Scheduler и World::tick()
  └── Рекомендуется реализовать ПОСЛЕ R4 (меньше конфликтов в events.rs)

3.5 (compute_archetype_indices) — изолировано в scheduler, независима
```

### Рекомендуемый порядок спринтов

| Спринт | Задачи | Обоснование |
|--------|--------|-------------|
| 1 | R4, 3.3 | Изолированные, низкий риск, быстрая победа |
| 2 | 3.5 | Изолировано в scheduler, нет внешних зависимостей |
| 3 | 4.10 + 4.2 | Совместно — оба в apex-macros, синергия |
| 4 | R3 | Наибольший риск (breaking change в World::tick), последним |

### Чеклист перед мержем каждой задачи

- [ ] Все 153 существующих теста проходят
- [ ] Добавлены тесты из этого документа
- [ ] `cargo clippy -- -D warnings` без новых предупреждений
- [ ] `cargo doc` без broken intra-doc links
- [ ] CHANGELOG.md обновлён (одной строкой, напр. «Event flush moved from World::tick() to Scheduler»)

---

*Документ подготовлен для команды разработки apex-ecs. Вопросы по архитектурным решениям — к автору анализа.*
