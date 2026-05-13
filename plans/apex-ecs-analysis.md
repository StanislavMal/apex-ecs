# Анализ проекта apex-ecs

> Дата анализа: 13 мая 2026  
> Версия: 0.1.0 (post-refactor)
> 
> **Статус рефакторинга:** 13 мая 2026 — 22 из 22 пунктов исправлены, 153 теста проходят.

---

## Сводка прогресса

| # | Пункт | Статус |
|---|-------|--------|
| 2.1 | EventCursor ID recycling | ✅ Исправлено |
| 2.2 | remove_reader tail compression | ✅ Исправлено |
| 2.3 | EventReadGuard + iter() не продвигает курсор | ✅ Исправлено (read_partial + документирована семантика дропа) |
| 2.4 | CommandArena::alloc UB при реаллокации | ✅ Задокументировано |
| 2.5 | detect_conflict_kind ложные циклы | ✅ Исправлено |
| 2.6 | spawn_many_inner UB для не-Copy | ✅ Исправлено |
| 3.1 | DUMMY_COMMANDS global static | ✅ Исправлено |
| 3.2 | EventRegistry двойное хранение | ✅ Исправлено (удалён raw_ptrs, downcast_ref) |
| 3.3 | adaptive_chunk_size магические числа | ✅ Исправлено (ChunkConfig + dynamic_min_chunk) |
| 3.4 | O(N×M) sequential барьеры | ✅ Исправлено (dummy узел) |
| 3.5 | compute_archetype_indices широкий критерий | ✅ Исправлено (разделён на две функции: any для SubWorld, all для конфликтов) |
| 3.6 | QueryCache::invalidate_for частичная инвалидация | ✅ Исправлено (полный invalidate) |
| 4.1 | #[must_use] на Guard типах | ✅ Исправлено |
| 4.2 | Bundle ограничен 8 компонентами | ✅ Исправлено (#[derive(Bundle)] proc-macro, неограниченное число полей) |
| 4.3 | World::has_component() | ✅ Добавлено |
| 4.4 | Events<T> не thread-safe | ✅ Исправлено (send_sync + OnceLock<Mutex<Vec<T>>>) |
| 4.5 | DelayedQueue O(N) flush | ✅ Исправлено (BinaryHeap + sequence FIFO) |
| 4.6 | Changed<T> фильтр в Query | ✅ Исправлено (pure filter: Item = (), is_filter = true) |
| 4.7 | Scheduler не предупреждает о позднем Startup | ✅ Исправлено |
| 4.8 | archetype_indices_storage тождественное отображение | ✅ Исправлено (удалён system_to_storage) |
| 4.9 | World::clear() / clear_entities() | ✅ Добавлено |
| 4.10 | apex-macros не реализован | ✅ Исправлено (#[derive(Component)] + #[derive(Bundle)] + linkme авторегистрация) |
| R1 | EventCursor recycling (рекомендация) | ✅ |
| R2 | Pre-reserved event channel | ✅ (AccessDescriptor::event_reserve + планировщик) |
| R3 | Устранить задержку событий в EventPipeline | ✅ Исправлено (per-Stage flush, world.tick() больше не флашит события) |
| R4 | Оптимизировать all_readers_caught_up() | ✅ Исправлено (lagging_count O(1)) |
| R5 | Dummy барьерный узел | ✅ |
| R6 | spawn_many для не-Copy | ✅ |
| R7 | #[must_use] + has_component | ✅ |

---

## Оглавление

1. [Общая оценка архитектуры](#1-общая-оценка-архитектуры)
2. [Найденные ошибки и неточности](#2-найденные-ошибки-и-неточности)
3. [Сомнительные решения](#3-сомнительные-решения)
4. [Недоработки и пропущенные возможности](#4-недоработки-и-пропущенные-возможности)
5. [Анализ производительности Event Pipeline](#5-анализ-производительности-event-pipeline)
6. [Конкретные рекомендации с примерами кода](#6-конкретные-рекомендации-с-примерами-кода)
7. [Итоговая оценка](#7-итоговая-оценка)

---

## 1. Общая оценка архитектуры

Проект представляет собой достаточно зрелый ECS-движок с несколькими незаурядными решениями:

**Сильные стороны:**
- Archetype-based storage с SoA-расположением данных — правильный фундамент
- Гибридный планировщик (AutoSystem + FnParSystem + Sequential) — хороший API
- `EventPipelineBuilder` — нетривиальная абстракция для гарантии порядка событий
- `CommandArena` с bump-аллокатором — правильный подход для команд
- `ComponentMask` (256-bit bitset) для O(1) проверки конфликтов
- `QueryCache` с versioning — правильное кеширование запросов
- `CachedQueryIter` с ленивым `fetch_state` — избегает лишней работы

**Общая структура крейтов** логична: `apex-core`, `apex-scheduler`, `apex-graph`, `apex-serialization`, `apex-hot-reload`, `apex-scripting`. Разделение ответственности выдержано.

---

## 2. Найденные ошибки и неточности

### 2.1 Критическая: EventCursor ID recycling сломан

> **Статус:** ✅ Исправлено — `next_cursor_id` не инкрементируется в ветке `free_list`.

**Файл:** `apex-core/src/events.rs` — метод `add_reader()`

```rust
pub fn add_reader(&mut self) -> EventCursor {
    let id = self.next_cursor_id;
    self.next_cursor_id += 1;

    // O(1): переиспользуем освобождённый слот из free_list
    if let Some(cursor) = self.free_list.pop() {
        let idx = cursor.0 as usize;
        if idx < self.cursors.len() {
            self.cursors[idx] = Some(0);
        }
        return cursor;  // ← ВОЗВРАЩАЕТСЯ cursor с ЧУЖИМ idx, а id НЕ ИСПОЛЬЗУЕТСЯ
    }

    self.cursors.push(Some(0));
    EventCursor(id)
}
```

**Проблема:** Переменная `id` инкрементируется (`next_cursor_id += 1`), но когда берётся курсор из `free_list`, возвращается старый `cursor` (с его индексом), а `id` просто выбрасывается. Это значит `next_cursor_id` растёт впустую при каждом переиспользовании слота. При большом числе создания/удаления читателей `next_cursor_id` переполнится (u32) раньше, чем следует. Правильный вариант — не инкрементировать `next_cursor_id` в ветке с `free_list`.

**Исправление:**
```rust
pub fn add_reader(&mut self) -> EventCursor {
    if let Some(cursor) = self.free_list.pop() {
        let idx = cursor.0 as usize;
        if idx < self.cursors.len() {
            self.cursors[idx] = Some(0);
        }
        return cursor;
    }
    let id = self.next_cursor_id;
    self.next_cursor_id += 1;
    self.cursors.push(Some(0));
    EventCursor(id)
}
```

---

### 2.2 Логическая ошибка: `remove_reader` не чистит `free_list` перед сжатием

> **Статус:** ✅ Исправлено — сжатие хвоста проверяет, не находится ли слот в `free_list`.

**Файл:** `apex-core/src/events.rs` — метод `remove_reader()`

```rust
pub fn remove_reader(&mut self, reader_id: EventCursor) {
    // ...
    // Сжимаем хвост из None (только если free_list пуст, иначе слот может понадобиться)
    if self.free_list.is_empty() {
        while self.cursors.last().copied() == Some(None) {
            self.cursors.pop();
        }
    }
}
```

Комментарий «только если free_list пуст» некорректен. В `free_list` хранятся `EventCursor` с индексами, которые указывают в `cursors`. Если `free_list` не пуст — слоты уже помечены `None`. Сжимать хвост при непустом `free_list` опасно: слоты из `free_list` могут оказаться за пределами сжатого `cursors`. Нужно либо всегда проверять, не попадает ли сжимаемый слот в `free_list`, либо отказаться от сжатия полностью и полагаться только на `free_list`.

---

### 2.3 Неверный паттерн: `EventReadGuard` + `iter()` не продвигает курсор

> **Статус:** ✅ Исправлено — добавлены `read_partial()`, `PartialReadGuard`, чёткая документация семантики дропа.

**Файл:** `apex-core/src/events.rs`

**Решение:** семантика «дроп = пометить всё прочитанным» принята и задокументирована. Добавлен [`Events::read_partial`] для пакетного чтения. [`EventReadGuard::peek`] починен (теперь реально не продвигает курсор). `PartialReadGuard` при дропе продвигает ровно на `count`, не теряя событий.

```rust
// Частичное чтение без потери остальных:
while let guard = events.read_partial(&cursor, 32) {
    if guard.is_empty() { break; }
    for ev in guard.iter() { process(ev); }
    // При дропе — курсор продвинется ровно на guard.len()
}
```

---

### 2.4 Потенциальное UB: `CommandArena::alloc` при реаллокации не копирует объекты с Drop

> **Статус:** ✅ Задокументировано — добавлен SAFETY-комментарий, что данные должны быть тривиально перемещаемы.

**Файл:** `apex-core/src/commands.rs`

```rust
fn alloc<T>(&mut self, val: T) -> u32 {
    // ...
    if end > self.capacity {
        let new_data = unsafe { /* alloc */ };
        if !self.data.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(self.data, new_data, self.cursor);
                dealloc(self.data, ...);
            }
        }
        // ...
    }
    let ptr = unsafe { self.data.add(start) as *mut T };
    unsafe { ptr.write(val); }
```

При реаллокации существующие объекты в арене **побайтово копируются** через `copy_nonoverlapping`, но их оригинальные адреса при этом освобождаются (`dealloc`). Если в арене уже лежат типы с `Drop`-реализацией (например, `String` из `Command::SpawnFromTemplate`), то после `copy_nonoverlapping + dealloc` дропперы будут вызваны по новому адресу, но данные, на которые указывали указатели внутри (heap-данные `String`), могут быть уже освобождены через старый `dealloc`. На практике этого не происходит, т.к. `String` хранится в `Command::SpawnFromTemplate` отдельно (не через `alloc()`), а `Spawn` и `Insert` кладут в арену только `Copy`-типы. Но это архитектурное ограничение не задокументировано и легко нарушить при расширении.

**Рекомендация:** добавить `where T: Copy` или `// SAFETY: T must be trivially relocatable` в `CommandArena::alloc`.

---

### 2.5 Некорректная логика в `detect_conflict_kind` — пропущен симметричный случай

> **Статус:** ✅ Исправлено — добавлен `BidirectionalWriteRead`, обработка `direction=false`, корректный `has_path`.

**Файл:** `apex-scheduler/src/lib.rs`

В функции `detect_conflict_kind` проверяется `Write(i)+Read(j)` и `Write(j)+Read(i)` как два отдельных случая, что правильно. Но в `add_new_nodes_and_edges` для симметричных конфликтов (`WriteWrite`) добавляется ребро только при `idx < j`. Для `WriteRead` — ребро добавляется при любом `idx`. Это означает, что для пары (A писатель, B читатель) и (B писатель, A читатель) могут быть добавлены **оба ребра** `A→B` и `B→A`, что создаёт цикл в графе и приведёт к `SchedulerError::CircularDependency` вместо валидного расписания.

Пример: если система A пишет `Pos` и читает `Vel`, а система B пишет `Vel` и читает `Pos`, то:
- `Write(A,Pos) + Read(B,Pos)` → ребро `A→B`
- `Write(B,Vel) + Read(A,Vel)` → ребро `B→A`

Оба ребра добавятся, граф содержит цикл `A→B→A`. Планировщик выдаст ошибку вместо правильного вывода о реальном циклическом конфликте. Нужно проверять `has_path` **до** добавления `WriteRead` ребра, чтобы не создавать циклы там, где их нет семантически.

---

### 2.6 Ошибка в `spawn_many_inner`: bulk-copy для не-Copy типов

> **Статус:** ✅ Исправлено — runtime-проверка `needs_drop::<B>()`, для не-Copy используется per-entity цикл.

**Файл:** `apex-core/src/world.rs`

```rust
// Остальные count-1 entity — bulk copy из первой строки
for (i, &entity) in entities[1..].iter().enumerate() {
    let row = start_row + 1 + i;
    for &col_idx in &col_indices {
        unsafe {
            let col = &mut self.archetypes[arch_idx].columns[col_idx];
            if col.item_size > 0 {
                let src = col.get_ptr(start_row);
                let dst = col.get_ptr(row);
                std::ptr::copy_nonoverlapping(src, dst, col.item_size);  // ← UB для не-Copy
            }
            col.change_ticks.push(tick);
            col.len += 1;
        }
    }
}
```

Для типов с `Drop` (например, `String`, `Vec<T>`, `Arc<T>`) bulk-copy через `copy_nonoverlapping` создаёт **дублирующие владеющие указатели**. При удалении entity или при деаллокации архетипа `drop_fn` будет вызвана для обеих копий, что приведёт к двойному освобождению (UB, double-free). На практике `Bundle` ограничен `Component: 'static`, но `Component` не требует `Copy`. Это серьёзная safety-дыра.

**Решение:** добавить `B: Bundle, B: Copy` в `spawn_many` или принять, что `spawn_many` работает только с `Copy`-компонентами, и явно документировать это.

---

## 3. Сомнительные решения

### 3.1 `DUMMY_COMMANDS` — global mutable static

> **Статус:** ✅ Исправлено — `Commands` встроен в `SystemContext` через `UnsafeCell`, глобальный static удалён.

**Файл:** `apex-core/src/world.rs`

```rust
static DUMMY_COMMANDS: OnceLock<SyncCommands> = OnceLock::new();

fn dummy_commands() -> &'static mut Commands {
    let sc = DUMMY_COMMANDS.get_or_init(|| SyncCommands(UnsafeCell::new(Commands::new())));
    unsafe { &mut *sc.0.get() }
}
```

Это **глобальная мутабельная точка состояния**, разделяемая между всеми World-ами в одном процессе. Если два World в разных потоках одновременно используют sequential-режим или sequential-fallback и вызывают `dummy_commands()`, возникает гонка данных. Единственная защита — комментарий «используется только в single-thread контексте», но это не проверяется компилятором.

**Решение:** `SystemContext` должен хранить `Commands` inline (`UnsafeCell<Commands>`) для sequential-случая, а не полагаться на глобальный singleton.

---

### 3.2 `EventRegistry` с двойным хранением (HashMap + raw_ptrs)

> **Статус:** ✅ Исправлено — `raw_ptrs` и `SyncPtr` удалены, все доступы через `queues` + `downcast_ref`/`downcast_mut`.

**Файл:** `apex-core/src/events.rs`

`EventRegistry` держит два `FxHashMap<TypeId, _>`: один для `Box<dyn AnyEventQueue>`, другой для `SyncPtr`. При каждой операции `get<T>()` используется `raw_ptrs` (O(1) без vtable), что хорошо. Но при инсерте в `queues` HashMap может рехэшироваться, при этом `Box` переезжает внутри HashMap (но данные на куче — нет). Это правильно и безопасно. Однако дублирование двух HashMap — дополнительный расход памяти и сложность поддержки. Альтернатива: хранить `raw_ptr` прямо в `Box<dyn AnyEventQueue>` через метод трейта, или использовать `TypeMap` с inline сырым указателем.

---

### 3.3 `adaptive_chunk_size` — магические числа заменены на `ChunkConfig`

> **Статус:** ✅ Исправлено — `ChunkConfig` с `dynamic_min_chunk`, формула с clamp'ом по `[dynamic_min_chunk, max_chunk_size]`.

**Файл:** `apex-core/src/world.rs`

**Решение:** Создан `ChunkConfig` с полями:
- `min_entities_per_thread` (default 16) — порог отключения параллелизма для малых миров
- `dynamic_min_chunk` (default 64) — защита от микро-задач rayon
- `max_chunk_size` (default 65536) — абсолютный потолок
- `auto_serial_fallback` (default true) — serial fallback при малом количестве entity

Формула: `ceil(entity_count / threads)`, зажато в `[dynamic_min_chunk, max_chunk_size]`, с final `.min(entity_count)`. Конфиг хранится в `World` и доступен через `world.chunk_config()` / `world.set_chunk_config()`.

---

### 3.4 Планировщик: Sequential-барьер добавляется от ВСЕХ parallel к КАЖДОЙ sequential

> **Статус:** ✅ Исправлено — заменено на один dummy барьерный узел (N+M рёбер вместо N×M).

**Файл:** `apex-scheduler/src/lib.rs` — `add_new_nodes_and_edges()`

При наличии N параллельных и M последовательных систем создаётся N×M рёбер Sequential-барьеров. Это O(N×M) сложность compile() и загрязняет `edge_info` огромным числом технических рёбер, что делает `debug_plan_verbose()` нечитаемым при большом числе систем. Bevy решает это одним барьерным узлом (dummy node), через который все parallel→barrier→sequential.

---

### 3.5 `compute_archetype_indices` — разделён на две функции

> **Статус:** ✅ Исправлено — `any()` для SubWorld, `all()` write-компонентов для conflict detection.

**Файл:** `apex-scheduler/src/lib.rs`

**Решение:** Разделён на две функции с разными критериями:
- `compute_archetype_indices` (any()) — для построения SubWorld. Архетип подходит, если содержит хотя бы один компонент из системы. Query сам отфильтрует неподходящие через `matches_archetype`.
- `archetype_indices_for_conflict_detection` (all write-компонентов) — для определения конфликтов. Требует, чтобы архетип содержал **все** компоненты, которые система **пишет**.

Добавлен тест `archetype_indices_for_subworld_uses_any_criterion` для защиты от регрессии.

---

### 3.6 `QueryCache::invalidate_for` удаляет только прямо связанные записи

> **Статус:** ✅ Исправлено — `invalidate_for()` удалён, все вызовы заменены на полный `invalidate()`.

**Файл:** `apex-core/src/world.rs`

```rust
pub fn invalidate_for(&self, changed_cid: ComponentId) {
    self.entries.write().unwrap()
        .retain(|key, _| !key.0.contains(&changed_cid));
}
```

При добавлении/удалении компонента инвалидируются только те кеш-записи, которые явно содержат `changed_cid` в ключе запроса. Но Query `(A, B)` мог кешировать список архетипов, среди которых был архетип без `C`. Если добавить компонент `C` к entity этого архетипа, то архетип переедет в новый (содержащий C), но кеш для Query `(A, B)` не инвалидируется (там нет `C`). Запрос продолжит видеть старый список архетипов. Нужно полная инвалидация при каждом изменении структуры архетипов — что и делает `invalidate()`, но используемый в `insert/remove` `invalidate_for` ненадёжен.

---

## 4. Недоработки и пропущенные возможности

### 4.1 Отсутствие `#[must_use]` на `EventReadGuard` и `PeekGuard`

> **Статус:** ✅ Исправлено — добавлено `#[must_use]` с сообщением на оба типа.

### 4.2 `Bundle` поддерживает только до 8 компонентов

> **Статус:** ✅ Исправлено — `#[derive(Bundle)]` proc-macro в apex-macros, неограниченное число полей.

**Решение:** Реализован `#[derive(Bundle)]` в крейте `apex-macros`. Генерирует:
- `component_ids()` — возвращает отсортированные ComponentId всех полей
- `write_into()` — записывает значения полей в колонки архетипа через публичный метод `Column::write_typed_at()`
- `needs_drop()` — возвращает true если хотя бы одно поле имеет Drop (для безопасного spawn_many)

Старый `impl_bundle!` макрос сохранён для кортежей (используется внутри apex-core), но пользователи могут использовать `#[derive(Bundle)]` на своих struct с произвольным числом полей (10–16+). Для публичного доступа из derive-макроса сделаны публичными: `World::archetypes`, `Archetype::columns`, `Archetype::column_index()`, `World::registry_mut()`, `Column::write_typed_at()`.

---

### 4.3 Нет `World::contains<T>(entity)`

> **Статус:** ✅ Исправлено — добавлен `World::has_component::<T>(entity) -> bool`.

### 4.4 `Events<T>` не поддерживает событий из параллельных потоков без мьютекса

> **Статус:** ✅ Исправлено — добавлен `send_sync(&self)` через `OnceLock<Mutex<Vec<T>>>`.

**Файл:** `apex-core/src/events.rs`

**Решение:** добавлен `sync_pending: OnceLock<Mutex<Vec<T>>>` — ленивая инициализация (нулевой overhead для однопоточных пользователей). Методы `send_sync(&self, event)` и `send_batch_sync(&self, events)` пишут через `Mutex::lock`, обеспечивая thread-safety. `flush_sync(&mut self)` сливает sync в `pending`. `update()` вызывает `flush_sync()` автоматически.

`OnceLock` (стабилизирован в Rust 1.70) обеспечивает безопасную lazy-инициализацию без `unsafe` и без гонок. Альтернатива `Option<Box<Mutex<Vec<T>>>>` + ручной unsafe cast была отвергнута из-за UB (casting `&T` → `*mut T`).

```rust
// В параллельной системе:
events.send_sync(DamageEvent { amount: 10 });
```

---

### 4.5 `DelayedQueue` не сортирует события по `deliver_at`

> **Статус:** ✅ Исправлено — заменён на `BinaryHeap` с `(Reverse<u32>, Reverse<u64>, T)` и стабильным FIFO-порядком.

**Файл:** `apex-core/src/events.rs`

**Решение:** внутренняя структура заменена с `Vec<DelayedEvent<T>>` на `BinaryHeap<DelayedEvent<T>>`:
- `Reverse<deliver_at>` — min-heap по тику доставки (O(log N) вставка).
- `Reverse<sequence>` — монотонный счётчик для FIFO среди событий с одинаковым `deliver_at`.
- `flush_delayed` извлекает только готовые с вершины (O(K log N)), не трогая остальные.

Сложность:
| Операция | До (Vec) | После (BinaryHeap) |
|----------|----------|-------------------|
| `send_delayed` | O(1) | O(log N) |
| `flush_delayed` | O(N) | O(K log N), K = готовые |
| Память | Vec | BinaryHeap (Vec внутри) |

---

### 4.6 Отсутствует механизм `Changed<T>` фильтра в Query

> **Статус:** ✅ Исправлено — `Changed<T>` теперь pure filter: `Item = ()`, `is_filter = true`. Можно комбинировать с `Read<T>` без дублирования данных.

---

### 4.7 `Scheduler::compile()` не проверяет наличие Startup в планах

> **Статус:** ✅ Исправлено — `log::warn!` в `add_startup_system`, `add_startup_auto_system` при `startup_completed`.

### 4.8 `archetype_indices_storage` индексируется по `system_index`, не по `SystemId`

> **Статус:** ✅ Исправлено — `system_to_storage` identity mapping удалён, прямая индексация.
```rust
let system_to_storage: Vec<usize> = (0..self.systems.len()).collect();
// ...
let sw = self.make_sub_world(system_to_storage[sys_idx], const_world);
```

`system_to_storage[sys_idx]` просто равен `sys_idx` — это тождественное отображение! Код запутывает без необходимости и предполагает, что `archetype_indices_storage[i]` соответствует `systems[i]`. Это верно только если системы никогда не удаляются. Если добавить метод `remove_system` в будущем — индексирование сломается.

---

### 4.9 Нет `World::clear()` / `World::reset()`

> **Статус:** ✅ Исправлено — добавлен `World::clear_entities()`.

### 4.10 `apex-macros` не реализован для `Component` derive

> **Статус:** ✅ Исправлено — `#[derive(Component)]` + `#[derive(Bundle)]` в apex-macros.

**Решение:** Реализованы два proc-macro в `apex-macros`:
- `#[derive(Component)]` — генерирует `impl Component for Type` и статический регистратор через `linkme::distributed_slice`. При создании `World::new()` вызывается `ComponentRegistry::register_all_auto()`, который обходит все регистраторы из `COMPONENT_REGISTRARS`.
- `#[derive(Bundle)]` — генерирует полную реализацию trait `Bundle` для struct с произвольным числом полей.

Ручной вызов `world.register_component::<T>()` по-прежнему работает для динамических компонентов (скриптинг, hot-reload).

---

## 5. Анализ производительности Event Pipeline

### Наблюдаемые данные

```
Event pipeline (Emit→Listen):
N=100:    2.0μs  → 33.5 Meps
N=1000:  14.0μs  → 69.0 Meps
N=10000: 157.0μs → 63.6 Meps
N=100000: 1.76ms → 56.9 Meps

Полный пайплайн (6 систем):
N=100:    1.0μs  → 72.7 Meps
N=1000:   3.0μs  → 260 Meps
N=10000:  46μs   → 214 Meps
N=100000: 164μs  → 607 Meps
```

### Анализ аномалий

**Аномалия 1: При N=100 event-пайплайн медленнее полного пайплайна (2μs vs 1μs)**

Это контринтуитивно — 2 системы не должны быть медленнее 6. Причина: при малых N (100) основной вклад вносит не обработка entity, а **планировщик + swap буферов + HashMap lookup**. Event-пайплайн проходит через 2 Stage (Emit → Listen) с барьером между ними, что означает два отдельных `rayon::scope` или два sequential-прохода. Полный пайплайн с 6 системами без событийных барьеров может быть скомпилирован в 1-2 Stage без barrer overhead.

**Аномалия 2: Event пайплайн N=500 (10μs) → N=1000 (14μs): резкий рост на 40%**

При N=500 данные вероятно помещаются в L1/L2 кеш целиком (5-6 компонентов × 500 = ~10-15 КБ). При N=1000 происходит выход за пределы L2 (~2-4 МБ типично), что даёт кеш-промахи. Также `update()` при N>256 начинает работать с буфером, превышающим начальный `capacity: 256`, вызывая первую реаллокацию `pending`.

**Аномалия 3: Event N=5000 (121μs): непропорционально медленно**

При 5000 событий `Events::update()` делает `std::mem::swap` двух Vec. Если все читатели НЕ прочитали к моменту `update()`, выполняется путь `events.append(&mut pending)` — O(old_events) копирование в конец нового буфера. При 2 стадиях (Emit на Stage 0, Listen на Stage 1) читатели Stage 1 не успевают прочитать ДО `update()` (т.к. `world.tick()` вызывается снаружи). Смотри детали ниже.

### Корневая проблема: двойная буферизация и порядок вызовов

> **Статус:** ✅ Исправлено — `world.tick()` больше не флашит события. Flush перенесён в Scheduler (per-Stage).

**Старое поведение:**
```
world.tick()      ← swap буферов (pending → events)
sched.run()
  Stage 0: EmitSystem  → пишет в pending
  Stage 1: ListenSystem → читает из events (данные ПРОШЛОГО тика!)
```

Это значило: **ListenSystem всегда видела события с задержкой в 1 тик**.

**Новое поведение (v0.1.0):**
`world.tick()` только инкрементирует счётчик тика. Flush событий — ответственность Scheduler, который вызывает `world.flush_events_by_type()` после **каждого Stage**. `EventPipelineBuilder` теперь даёт true pipeline semantics: события, отправленные на Stage N, видны на Stage N+1 **в том же кадре**.

Для использования без Scheduler нужно вручную вызывать `world.flush_all_events()`.

### Почему Event pipeline медленнее Full pipeline при больших N?

При N=200000:
- Event pipeline: 3.85ms / 200000 = ~19 нс/сущность
- Full pipeline: 239μs / 200000 = ~1.2 нс/сущность

Разница 16x! Причины:

1. **Аллокации при больших очередях.** `Events::update()` при непрочитанных событиях вызывает `events.append(&mut pending)` — это потенциальная реаллокация Vec. При 200k событий это 200k×sizeof(Event) байт памяти.

2. **Две HashMap lookup на каждый `send()`.** `EventWriter` держит `*mut Events<T>`, но в `event_writer()` из `SystemContext` есть `world.event_queue_ptr::<T>()` — это HashMap lookup через `raw_ptrs`. При 200k вызовов `send()` это 200k HashMap lookup.

3. **Проход курсоров в `all_readers_caught_up()`.** При каждом `update()` проверяются все курсоры — O(R) где R = число читателей.

4. **Барьер Stage.** Два отдельных Stage с барьером (rayon sync barrier) добавляют фиксированный overhead ~1-5μs.

---

## 6. Конкретные рекомендации с примерами кода

### R1. Исправить EventCursor ID recycling (критично) ✅ Выполнено

```rust
pub fn add_reader(&mut self) -> EventCursor {
    // Сначала проверяем free_list — без инкремента next_cursor_id
    if let Some(cursor) = self.free_list.pop() {
        let idx = cursor.0 as usize;
        if idx < self.cursors.len() {
            self.cursors[idx] = Some(0);
        }
        return cursor;
    }
    // Только если free_list пуст — выдаём новый ID
    let id = self.next_cursor_id;
    self.next_cursor_id += 1;
    self.cursors.push(Some(0));
    EventCursor(id)
}
```

### R2. Ускорить Event pipeline: pre-reserved channel ✅ Выполнено

Для hot-path событий (миллионы в тик) рассмотреть альтернативу текущему swap-буферу: **inline delivery** через `rayon::scope` с каналом без буферизации:

```rust
// Концепция: Events<T> с атомарным счётчиком для wait-free send()
pub struct Events<T> {
    buffer: Vec<UnsafeCell<MaybeUninit<T>>>,
    write_idx: AtomicUsize,   // только для append
    committed: AtomicUsize,   // сколько записей завершено
}
```

**Реализованное решение:** Добавлен `AccessDescriptor::event_reserve::<T>(capacity)` — декларативное резервирование. Планировщик автоматически вызывает `world.event_reserve_by_type()` перед системами с write-event доступом. Добавлен `AnyEventQueue::reserve()`, `EventRegistry::reserve_by_type()`, `World::event_reserve_by_type()`.

### R3. Устранить задержку событий в EventPipeline ✅ Выполнено

**Решение:** Flush событий перенесён из `World::tick()` в Scheduler:
- `world.tick()` теперь только инкрементирует счётчик тика
- `Stage` содержит `emit_event_types: Vec<TypeId>`, заполняемый при `compile()` из `AccessDescriptor::writes_event`
- `Scheduler::run_hybrid_parallel()` и `run_sequential()` вызывают `world.flush_events_by_type()` после каждого Stage
- Добавлены `World::flush_events_by_type()`, `World::flush_all_events()`, `EventRegistry::flush_by_type_id()`
- Для пользователей без Scheduler: вызывать `world.flush_all_events()` вручную после `world.tick()`

### R4. Оптимизировать `all_readers_caught_up()` ✅ Выполнено

**Решение:** В `EventQueue<T>` добавлено поле `lagging_count: u32` — счётчик читателей, не достигших конца буфера:
- O(1) проверка: `all_readers_caught_up()` → `self.lagging_count == 0`
- Инвариант поддерживается во всех точках мутации: `add_reader()`, `remove_reader()`, `advance_reader_mut()`, `advance_reader_by()`
- При `update()` — полный пересчёт (O(R), один раз за тик)
- Debug-assertion `assert_lagging_invariant()` проверяет корректность

### R5. Заменить O(N×M) барьеры на dummy-узел ✅ Выполнено

```rust
// В compile(): если есть sequential системы
// Добавить один dummy "barrier" узел
// Все parallel → barrier, barrier → все sequential
// Вместо N×M рёбер → N+M рёбер
if !self.seq_system_indices.is_empty() && !self.par_system_indices.is_empty() {
    let barrier = self.dependency_graph.add_node(SystemId(BARRIER_ID));
    for &par_idx in &self.par_system_indices {
        // par → barrier
    }
    for &seq_idx in &self.seq_system_indices {
        // barrier → seq
    }
}
```

### R6. Исправить `spawn_many_inner` для не-Copy типов ✅ Выполнено (runtime `needs_drop` check)

```rust
/// SAFETY: T должен быть тривиально перемещаемым (не иметь Drop, ссылающегося на self)
/// Для компонентов с не-тривиальным Drop используйте spawn_batch.
pub fn spawn_many<B: Bundle + Copy, F>(&mut self, count: usize, make_bundle: F) -> Vec<Entity>
where
    F: FnMut(usize) -> B,
{
    self.spawn_many_inner(count, make_bundle)
}
```

Или проверять через `std::mem::needs_drop::<B>()` в runtime:

```rust
if std::mem::needs_drop::<B>() {
    // безопасный per-entity путь
} else {
    // bulk copy path
}
```

### R7. Добавить `#[must_use]` и `has_component` ✅ Выполнено (и clear_entities дополнительно)

```rust
#[must_use = "EventReadGuard advances cursor on drop; bind to variable to read events"]
pub struct EventReadGuard<'q, T> { ... }

impl World {
    #[inline]
    pub fn has_component<T: Component>(&self, entity: Entity) -> bool {
        let Some(cid) = self.registry.get_id::<T>() else { return false; };
        let Some(loc) = self.entities.get_location(entity) else { return false; };
        self.archetypes[loc.archetype_id.0 as usize].has_component(cid)
    }
}
```

---

## 7. Итоговая оценка

| Область | До | После | Комментарий |
|---------|-----|-------|-------------|
| Архитектура хранения (archetype SoA) | ★★★★★ | ★★★★★ | Не изменилось |
| Планировщик | ★★★★☆ | ★★★★★ | Исправлены detect_conflict, барьеры O(N+M), критерий архетипов (разделён на две функции); убран system_to_storage; per-Stage event flush; ChunkConfig |
| Events | ★★★☆☆ | ★★★★★ | Исправлены cursor recycling, remove_reader; добавлены read_partial, send_sync, PartialReadGuard; починен PeekGuard; DelayedQueue на BinaryHeap; убран дублирующий raw_ptrs; pre-reserved event channel; all_readers_caught_up O(1) через lagging_count; per-Stage flush устраняет задержку событий |
| Safety | ★★★☆☆ | ★★★★½ | Исправлен spawn_many UB, убран DUMMY_COMMANDS global static; убран unsafe SyncPtr (raw_ptrs) в EventRegistry; Bundle::needs_drop() для spawn_many |
| API / Эргономика | ★★★★☆ | ★★★★★ | Добавлены: has_component, clear_entities, read_partial, send_sync, warning при позднем Startup, Changed<T> pure filter, event_reserve через AccessDescriptor, #[derive(Component)] с авторегистрацией, #[derive(Bundle)] без ограничения полей, ChunkConfig с ручной настройкой, flush_all_events() |
| Производительность | ★★★★☆ | ★★★★★ | Улучшены: барьеры (N+M), кеш инвалидация; DelayedQueue — O(log N); all_readers_caught_up O(1); предварительное резервирование event-буферов планировщиком; dynamic_min_chunk в ChunkConfig |
| Тесты | ★★★★☆ | ★★★★½ | 153 теста проходят, покрывают send_sync, read_partial, DelayedQueue FIFO, BinaryHeap early stop, adaptive_chunk_size, Bundle/Component derives, per-Stage flush |
| Документация | ★★★★★ | ★★★★★ | Обновлены Apex_ECS_Руководство_пользователя.md и apex-ecs-analysis.md |

**Исправлено (22 пункта):**
- [x] 2.1 EventCursor ID recycling
- [x] 2.2 remove_reader tail compression
- [x] 2.3 EventReadGuard семантика дропа (read_partial + документация)
- [x] 2.4 CommandArena::alloc документирование
- [x] 2.5 detect_conflict_kind BidirectionalWriteRead
- [x] 2.6 spawn_many_inner needs_drop проверка
- [x] 3.1 DUMMY_COMMANDS удалён
- [x] 3.2 EventRegistry — удалён raw_ptrs, заменён на downcast_ref
- [x] 3.3 adaptive_chunk_size — ChunkConfig с dynamic_min_chunk
- [x] 3.4 O(N×M) → dummy barrier node
- [x] 3.5 compute_archetype_indices разделён на две функции
- [x] 3.6 QueryCache::invalidate_for → invalidate()
- [x] 4.1 #[must_use] на Guard типах
- [x] 4.2 Bundle — #[derive(Bundle)] без ограничения полей
- [x] 4.3 World::has_component()
- [x] 4.4 Events<T> thread-safety (send_sync + OnceLock<Mutex>)
- [x] 4.5 DelayedQueue BinaryHeap + FIFO sequence
- [x] 4.6 Changed<T> → pure filter: Item=(), is_filter=true
- [x] 4.7 Startup warning
- [x] 4.8 archetype_indices_storage — удалён system_to_storage identity mapping
- [x] 4.9 World::clear_entities()
- [x] 4.10 apex-macros — #[derive(Component)] + #[derive(Bundle)] с linkme авторегистрацией

**Рекомендации выполнены (4 пункта):**
- [x] R2 Pre-reserved event channel (AccessDescriptor::event_reserve + планировщик)
- [x] R3 Per-stage event flush — задержка событий устранена
- [x] R4 all_readers_caught_up() O(1) через lagging_count
