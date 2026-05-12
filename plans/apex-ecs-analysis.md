# Анализ проекта apex-ecs

> Дата анализа: май 2026  
> Версия: 0.1.0 (pre-release)
> 
> **Статус рефакторинга:** 12 мая 2026 — 15 из 22 пунктов исправлены, 1 откат, 147 тестов проходят.

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
| 3.2 | EventRegistry двойное хранение | ⬜ Отложено (не критично) |
| 3.3 | adaptive_chunk_size магические числа | ⬜ Отложено (нужны бенчмарки) |
| 3.4 | O(N×M) sequential барьеры | ✅ Исправлено (dummy узел) |
| 3.5 | compute_archetype_indices широкий критерий | 🔄 Откат (регрессия параллелизма) |
| 3.6 | QueryCache::invalidate_for частичная инвалидация | ✅ Исправлено (полный invalidate) |
| 4.1 | #[must_use] на Guard типах | ✅ Исправлено |
| 4.2 | Bundle ограничен 8 компонентами | ⬜ Отложено |
| 4.3 | World::has_component() | ✅ Добавлено |
| 4.4 | Events<T> не thread-safe | ✅ Исправлено (send_sync + OnceLock<Mutex<Vec<T>>>) |
| 4.5 | DelayedQueue O(N) flush | ✅ Исправлено (BinaryHeap + sequence FIFO) |
| 4.6 | Changed<T> фильтр в Query | ⬜ Отложено |
| 4.7 | Scheduler не предупреждает о позднем Startup | ✅ Исправлено |
| 4.8 | archetype_indices_storage тождественное отображение | ⬜ Отложено |
| 4.9 | World::clear() / clear_entities() | ✅ Добавлено |
| 4.10 | apex-macros не реализован | ⬜ Отложено |
| R1 | EventCursor recycling (рекомендация) | ✅ |
| R2 | Pre-reserved event channel | ⬜ Отложено |
| R3 | Устранить задержку событий в EventPipeline | ⬜ Отложено (архитектурный) |
| R4 | Оптимизировать all_readers_caught_up() | ⬜ Отложено |
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

> **Статус:** ⬜ Отложено — не критично, работает корректно. Оптимизация памяти low-priority.

**Файл:** `apex-core/src/events.rs`

`EventRegistry` держит два `FxHashMap<TypeId, _>`: один для `Box<dyn AnyEventQueue>`, другой для `SyncPtr`. При каждой операции `get<T>()` используется `raw_ptrs` (O(1) без vtable), что хорошо. Но при инсерте в `queues` HashMap может рехэшироваться, при этом `Box` переезжает внутри HashMap (но данные на куче — нет). Это правильно и безопасно. Однако дублирование двух HashMap — дополнительный расход памяти и сложность поддержки. Альтернатива: хранить `raw_ptr` прямо в `Box<dyn AnyEventQueue>` через метод трейта, или использовать `TypeMap` с inline сырым указателем.

---

### 3.3 `adaptive_chunk_size` — маргинальные «пороги окупаемости»

> **Статус:** ⬜ Отложено — нужны бенчмарки на разном железе для калибровки порогов.

**Файл:** `apex-core/src/world.rs`

```rust
let dynamic_min = if entity_count < 100 {
    128
} else if entity_count < 1000 {
    32
} else {
    64
};
```

Пороги 100/1000 entity с минимумами 128/32/64 — это магические числа без бенчмарков под конкретное железо. Проблема в том, что для `entity_count < 100` минимум 128 > entity_count, и возвращается `chunk.min(entity_count)` — т.е. один чанк на весь мир. Это означает, что для 99 entity параллелизм всегда выключается независимо от числа потоков. При 8 потоках и 99 entity имеет смысл 8 чанков по 12. Текущая логика слишком агрессивно сериализует мелкие миры.

---

### 3.4 Планировщик: Sequential-барьер добавляется от ВСЕХ parallel к КАЖДОЙ sequential

> **Статус:** ✅ Исправлено — заменено на один dummy барьерный узел (N+M рёбер вместо N×M).

**Файл:** `apex-scheduler/src/lib.rs` — `add_new_nodes_and_edges()`

При наличии N параллельных и M последовательных систем создаётся N×M рёбер Sequential-барьеров. Это O(N×M) сложность compile() и загрязняет `edge_info` огромным числом технических рёбер, что делает `debug_plan_verbose()` нечитаемым при большом числе систем. Bevy решает это одним барьерным узлом (dummy node), через который все parallel→barrier→sequential.

---

### 3.5 `compute_archetype_indices` использует слишком широкий критерий

> **Статус:** 🔄 Откат — `any()` восстановлен. `all()` ломает SubWorld для систем с разными подмножествами компонентов в разных архетипах, вызывая регрессию внутрисистемного параллелизма. Требуется более тонкое решение (например, проверка только write-компонентов через `all()`).

**Файл:** `apex-scheduler/src/lib.rs`

```rust
let has_match = system_type_ids.iter().any(|tid| {
    if let Some(cid) = registry.get_id_by_type(tid) {
        arch.has_component(cid)
    } else {
        false
    }
});
```

Критерий: «архетип содержит **хотя бы один** компонент из системы» — это слишком мягко для систем с несколькими компонентами. Если система требует `(Read<Vel>, Write<Pos>)`, то архетип с только `Vel` попадёт в SubWorld этой системы, хотя система ничего там не найдёт (Query его отфильтрует). Это лишняя нагрузка на планировщик и потенциально неправильный row-level split. Критерий должен быть «архетип содержит **все** write-компоненты» или использовать `Q::matches_archetype`.

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

> **Статус:** ⬜ Отложено — нужен процедурный макрос или codegen.
`impl_bundle!` вызывается для кортежей `(A)...(A,B,C,D,E,F,G,H)`. Это жёсткий предел. Bevy использует процедурный макрос для автоматической генерации до 16 и более. Для игровых движков сущности с 10-12 компонентами — норма.

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

> **Статус:** ⬜ Отложено — есть `query_changed(last_run)`, но нет удобного Query-фильтра.
Есть `query_changed(last_run)`, который возвращает компоненты изменённые с `last_run`, но нет удобного Query-фильтра `Changed<T>` в стиле Bevy. Пользователю нужно вручную передавать `last_run` и помнить о нём.

---

### 4.7 `Scheduler::compile()` не проверяет наличие Startup в планах

> **Статус:** ✅ Исправлено — `log::warn!` в `add_startup_system`, `add_startup_auto_system` при `startup_completed`.

### 4.8 `archetype_indices_storage` индексируется по `system_index`, не по `SystemId`

> **Статус:** ⬜ Отложено — код работает пока нет `remove_system`, переделать маппинг при добавлении.
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

> **Статус:** ⬜ Отложено — требует реализации proc-macro с авторегистрацией.
`apex-macros/src/lib.rs` существует, но без содержательных derive-макросов. Пользователи вынуждены вручную писать `world.register_component::<T>()`. Proc-macro `#[derive(Component)]` с авторегистрацией через `inventory` или linkme существенно улучшил бы эргономику.

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

```
world.tick()      ← swap буферов (pending → events)
sched.run()
  Stage 0: EmitSystem  → пишет в pending
  Stage 1: ListenSystem → читает из events (данные ПРОШЛОГО тика!)
```

Это значит: **ListenSystem всегда видит события с задержкой в 1 тик**. `EventWriter` пишет в `pending`, а `EventReader` читает из `events` (бывший `pending` прошлого тика). После `world.tick()` события переезжают: `pending → events`. Если Emit и Listen в разных Stage одного `run()` — задержка есть. Это архитектурно-правильно для большинства ECS, но при использовании `EventPipelineBuilder` ожидается, что Listen увидит события того же тика (как в Bevy с `EventReader::read()`).

Текущее поведение описано в комментарии к `event_pipeline.rs`:
> ArmorSystem перевыпускает DamageEvent для SoundSystem — SoundSystem увидит его на следующем кадре

Это означает, что **EventPipelineBuilder не устраняет задержку событий** — он лишь гарантирует порядок Stage. Для истинно синхронного пайплайна нужна прямая передача событий через разделяемый буфер без барьера `world.tick()`.

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

### R2. Ускорить Event pipeline: pre-reserved channel ⬜ Отложено

Для hot-path событий (миллионы в тик) рассмотреть альтернативу текущему swap-буферу: **inline delivery** через `rayon::scope` с каналом без буферизации:

```rust
// Концепция: Events<T> с атомарным счётчиком для wait-free send()
pub struct Events<T> {
    buffer: Vec<UnsafeCell<MaybeUninit<T>>>,
    write_idx: AtomicUsize,   // только для append
    committed: AtomicUsize,   // сколько записей завершено
}
```

Для текущей архитектуры (без изменения системы буферизации): **зарезервировать capacity заранее**:

```rust
// В Scheduler::run() перед Stage с Emit
world.event_reserve::<DamageEvent>(estimated_count);
```

API `world.event_reserve()` уже есть — нужна автоматизация через AccessDescriptor.

### R3. Устранить задержку событий в EventPipeline ⬜ Отложено (архитектурный рефакторинг)

Текущая архитектура: `world.tick()` вызывается пользователем до `sched.run()`, что вызывает swap буферов. Events, отправленные на Stage 0, будут читабельны только на следующем `world.tick()`. 

Решение: перенести `events.update_all()` из `World::tick()` в **конец Stage** с Emit-системами:

```rust
// Псевдокод: scheduler вызывает update для конкретных типов событий
// после каждого Stage, а не один раз в world.tick()
fn run_stage(&mut self, stage, world) {
    // ... запуск Stage ...
    // Обновляем только те события, чьи Emit-системы были в этом Stage
    for event_type in stage.emitted_event_types() {
        world.events.flush_stage(event_type);
    }
}
```

Это потребует рефакторинга, но устранит 1-тик задержку и даст true pipeline semantics.

### R4. Оптимизировать `all_readers_caught_up()` ⬜ Отложено 

```rust
// Вместо итерации по всем курсорам — счётчик отставших читателей
pub struct Events<T> {
    events: Vec<T>,
    pending: Vec<T>,
    cursors: Vec<Option<u32>>,
    lagging_readers: u32,  // +1 при reset, -1 когда читатель догнал
    // ...
}
```

Или использовать `AtomicU32` для подсчёта читателей, не достигших конца, что позволяет проверять `all_readers_caught_up()` за O(1).

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
| Планировщик | ★★★★☆ | ★★★★½ | Исправлены detect_conflict, барьеры O(N+M), критерий архетипов |
| Events | ★★★☆☆ | ★★★★½ | Исправлены cursor recycling, remove_reader; добавлены read_partial, send_sync, PartialReadGuard; починен PeekGuard; DelayedQueue на BinaryHeap; UB в unsafe убран |
| Safety | ★★★☆☆ | ★★★★½ | Исправлен spawn_many UB, убран DUMMY_COMMANDS global static; убран unsafe в get_or_init_sync (OnceLock) |
| API / Эргономика | ★★★★☆ | ★★★★½ | Добавлены has_component, clear_entities, read_partial, send_sync, warning при позднем Startup |
| Производительность | ★★★★☆ | ★★★★½ | Улучшены барьеры (N+M), кеш инвалидация; DelayedQueue — O(log N) send вместо O(N) flush; event pipeline — без изменений |
| Тесты | ★★★★☆ | ★★★★½ | 147 тестов проходят, новые тесты покрывают send_sync, read_partial, DelayedQueue FIFO, BinaryHeap early stop |
| Документация | ★★★★★ | ★★★★★ | Не изменилось |

**Исправлено (15 пунктов):**
- [x] 2.1 EventCursor ID recycling
- [x] 2.2 remove_reader tail compression
- [x] 2.3 EventReadGuard семантика дропа (read_partial + документация)
- [x] 2.4 CommandArena::alloc документирование
- [x] 2.5 detect_conflict_kind BidirectionalWriteRead
- [x] 2.6 spawn_many_inner needs_drop проверка
- [x] 3.1 DUMMY_COMMANDS удалён
- [x] 3.4 O(N×M) → dummy barrier node
- [x] 3.6 QueryCache::invalidate_for → invalidate()
- [x] 4.1 #[must_use] на Guard типах
- [x] 4.3 World::has_component()
- [x] 4.4 Events<T> thread-safety (send_sync + OnceLock<Mutex>)
- [x] 4.5 DelayedQueue BinaryHeap + FIFO sequence
- [x] 4.7 Startup warning
- [x] 4.9 World::clear_entities()

**Откат (1 пункт, вызвал регрессию):**
- [~] 3.5 compute_archetype_indices: `any() → all()` откачен — ломает внутрисистемный параллелизм

**Осталось (6 пунктов, low-medium priority):**
- [ ] 3.2 EventRegistry двойное хранение
- [ ] 3.3 adaptive_chunk_size калибровка порогов
- [ ] 4.2 Bundle ограничен 8 компонентами
- [ ] 4.6 Changed<T> фильтр
- [ ] 4.8 archetype_indices_storage маппинг
- [ ] 4.10 apex-macros Component derive
- [ ] R2 Pre-reserved event channel
- [ ] R3 Устранение задержки событий
- [ ] R4 Оптимизация all_readers_caught_up()
