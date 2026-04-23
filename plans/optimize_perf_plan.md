# План оптимизации производительности Apex ECS

## 1. Межсистемный параллелизм (speedup 1.08x–1.39x → цель 3x–6x)

### Диагностика

Текущая реализация [`run_hybrid_parallel`](crates/apex-scheduler/src/lib.rs:841) использует `rayon::scope` для параллельного запуска систем в одном Stage. Проблема может быть в:

1. **`ParallelWorld`** — оборачивает `*const World`, и каждая система получает `&World` через `unsafe { world_ref.get() }`. Если `SystemContext::new()` или `for_each_component` внутри системы использует блокировки (например, `Mutex` на уровне архетипов), параллелизм деградирует.

2. **Rayon work-stealing** — если задачи слишком мелкие, оверхед rayon перевешивает.

3. **Cache thrashing** — даже с разными архетипами, если Column'ы расположены в памяти близко, возникает false sharing на уровне кеш-линий.

### Шаги

#### Шаг 1.1: Добавить логирование времени в run_hybrid_parallel
- Измерить время `rayon::scope` vs суммарное время систем
- Если `scope` занимает ~столько же, сколько последовательный запуск — проблема в rayon
- Если `scope` быстрее, но системы внутри медленные — проблема в `SystemContext`/`for_each_component`

**Файл:** [`crates/apex-scheduler/src/lib.rs`](crates/apex-scheduler/src/lib.rs:841)
**Метод:** `run_hybrid_parallel`
**Изменения:** Добавить `Instant::now()` до и после `rayon::scope`, вывести в `debug!` или `println!`

#### Шаг 1.2: Проверить ParallelWorld на блокировки
- `ParallelWorld::get()` возвращает `&World` — без блокировок
- `SystemContext::for_each_component` вызывает `Query::for_each_component` — нужно проверить, нет ли там `Mutex` или `RefCell`
- Посмотреть [`Query::for_each_component`](crates/apex-core/src/query.rs:350) — если там есть внутренняя изменяемость через `UnsafeCell`, это может вызывать проблемы

**Файл:** [`crates/apex-core/src/query.rs`](crates/apex-core/src/query.rs:350)
**Проверить:** Нет ли `RefCell`/`Mutex` в `Query` или `WorldQuery::fetch_state`

#### Шаг 1.3: Rayon scope vs par_iter
- Текущий код: `rayon::scope` + `scope.spawn` для каждой системы
- Альтернатива: собрать замыкания в `Vec<Box<dyn FnOnce() + Send>>` и запустить через `par_iter()`
- `rayon::scope` хорош для вложенных задач, но `par_iter` может дать лучший load balancing

**Файл:** [`crates/apex-scheduler/src/lib.rs`](crates/apex-scheduler/src/lib.rs:884)
**Изменения:** Заменить `rayon::scope` + `spawn` на `Vec<impl FnOnce() + Send>` + `par_bridge()` или `par_iter()`

#### Шаг 1.4: Увеличить parallel_threshold
- Сейчас `parallel_threshold = 2` (строка 293)
- Для CPU-bound нагрузки с 12 ядрами имеет смысл запускать параллельно даже 2 системы
- Проверить, не срабатывает ли порог ложно

**Файл:** [`crates/apex-scheduler/src/lib.rs`](crates/apex-scheduler/src/lib.rs:293)
**Проверить:** `stage_ids.len() < self.parallel_threshold` — при 2 системах порог не должен блокировать параллелизм

#### Шаг 1.5: Написать микро-тест параллелизма
- Создать тест, который запускает 2 системы с тяжёлой CPU-bound нагрузкой на **разных архетипах**
- Измерить время `rayon::scope` напрямую, без `bench_seq_par`
- Сравнить с последовательным запуском

**Файл:** [`crates/apex-examples/examples/perf.rs`](crates/apex-examples/examples/perf.rs:1027)
**Добавить:** Микро-тест в `bench_parallel_scheduler` или отдельную функцию

---

## 2. has_relation (396–434 ns/op → цель 20–50 ns/op)

### Диагностика

Текущая реализация [`SubjectIndex::has`](crates/apex-core/src/relations.rs:187):
1. Проверяет `entity_index < entries.len()` — O(1)
2. Вызывает `SubjectEntry::has` — проверяет `kind_mask` (битовая маска, O(1)), затем `binary_search` по `SmallVec<[u32; 4]>` (O(log n))

Проблема: `binary_search` на `SmallVec` — быстрый, но 396 ns/op для хеш-лукапа это много. Возможные причины:
- `SmallVec<[u32; 4]>` — если relations > 4, аллоцируется на куче
- `has_kind_slow` для kind_idx >= 64 — линейный поиск
- Вызов `decode_kind` и `is_relation_id` на каждый вызов

### Шаги

#### Шаг 2.1: Заменить SmallVec на FxHashSet для relations
- `binary_search` на SmallVec — O(log n), но для частых вставок/удалений лучше HashSet
- `FxHashSet<u32>` даст O(1) lookup
- Для малых размеров (< 4) можно оставить SmallVec, для больших — HashSet

**Файл:** [`crates/apex-core/src/relations.rs`](crates/apex-core/src/relations.rs:94)
**Изменения:** Заменить `SmallVec<[u32; 4]>` на `FxHashSet<u32>` или гибрид SmallVec + HashSet

#### Шаг 2.2: Оптимизировать has_kind_slow
- kind_idx >= 64 — сейчас линейный поиск по всем relations
- Добавить `Vec<u64>` для битовых масок при kind_idx >= 64, или использовать `FxHashSet<u32>` для хранения kind_idx

**Файл:** [`crates/apex-core/src/relations.rs`](crates/apex-core/src/relations.rs:109)
**Изменения:** Заменить линейный поиск на HashSet lookup

#### Шаг 2.3: Inline has() и убрать лишние декодирования
- `has()` вызывает `has_kind()`, затем `binary_search`
- `has_kind()` внутри снова декодирует kind_idx
- Можно объединить в один проход

**Файл:** [`crates/apex-core/src/relations.rs`](crates/apex-core/src/relations.rs:148)
**Изменения:** Inline и объединить проверки

#### Шаг 2.4: Написать микро-бенчмарк для has_relation
- Измерить чистое время `SubjectIndex::has` без оверхеда `World::has_relation`
- Сравнить разные реализации (SmallVec vs HashSet)

**Файл:** [`crates/apex-bench/benches/benchmark.rs`](crates/apex-bench/benches/benchmark.rs) или [`crates/apex-examples/examples/perf.rs`](crates/apex-examples/examples/perf.rs)

---

## 3. Structural changes — insert (68–79 ns/op → цель 40–50 ns/op)

### Диагностика

Текущая реализация [`World::insert`](crates/apex-core/src/world.rs:416):
1. Получить `component_id` — O(1)
2. Получить `location` entity — O(1)
3. Проверить, есть ли уже компонент — O(1)
4. Если нет — `find_or_create_archetype_with` — поиск/создание нового архетипа
5. `move_entity` — перемещение entity между архетипами (копирование всех компонентов)
6. `write_component` — запись нового компонента
7. `set_location` — обновление location

Основная стоимость: **`move_entity`** — копирование всех компонентов entity из старого архетипа в новый.

### Шаги

#### Шаг 3.1: Оптимизировать move_entity
- Сейчас копирует все компоненты по одному через `Column::copy_to`
- Можно использовать `memcpy` для всего ряда целиком, если компоненты тривиально копируемые
- Добавить флаг `is_copy` в `ComponentInfo`

**Файл:** [`crates/apex-core/src/archetype.rs`](crates/apex-core/src/archetype.rs)
**Изменения:** В `move_entity` — для Copy-типов использовать `ptr::copy_nonoverlapping` всего ряда

#### Шаг 3.2: Кешировать find_or_create_archetype_with
- Сейчас при каждом insert ищется/создаётся новый архетип
- Добавить кеш: `(source_archetype_id, component_id) → target_archetype_id`
- Для batch insert (много entity с одним и тем же компонентом) это даст значительное ускорение

**Файл:** [`crates/apex-core/src/world.rs`](crates/apex-core/src/world.rs:436)
**Изменения:** Добавить `FxHashMap<(ArchetypeId, ComponentId), ArchetypeId>` в World

#### Шаг 3.3: Batch insert
- Добавить метод `insert_batch` для массовой вставки одного компонента многим entity
- Оптимизация: найти целевой архетип один раз, переместить все entity за один проход

**Файл:** [`crates/apex-core/src/world.rs`](crates/apex-core/src/world.rs:416)
**Изменения:** Добавить `insert_batch(entity: &[Entity], component: T)`

#### Шаг 3.4: Оптимизировать despawn
- Сейчас despawn удаляет entity из архетипа и сдвигает последний элемент на место удалённого
- 19–21 ns/op — уже неплохо, но можно улучшить
- Добавить batch despawn

**Файл:** [`crates/apex-core/src/world.rs`](crates/apex-core/src/world.rs)
**Изменения:** Добавить `despawn_batch(entities: &[Entity])`

---

## Порядок выполнения

### Фаза 1: Диагностика (1–2 дня)
1. [ ] Шаг 1.1 — добавить логирование в `run_hybrid_parallel`, запустить бенчмарк
2. [ ] Шаг 1.2 — проверить Query на блокировки
3. [ ] Шаг 1.5 — написать микро-тест параллелизма
4. [ ] Шаг 2.4 — написать микро-бенчмарк has_relation
5. [ ] Шаг 3.1 — профилировать insert (какая часть времени уходит на move_entity)

### Фаза 2: Исправление параллелизма (2–3 дня)
1. [ ] Шаг 1.3 — заменить `rayon::scope` на `par_iter`
2. [ ] Шаг 1.4 — проверить parallel_threshold
3. [ ] Запустить бенчмарк, проверить speedup

### Фаза 3: has_relation (1 день)
1. [ ] Шаг 2.1 — заменить SmallVec на FxHashSet
2. [ ] Шаг 2.2 — оптимизировать has_kind_slow
3. [ ] Шаг 2.3 — inline has()
4. [ ] Запустить бенчмарк

### Фаза 4: Structural changes (2–3 дня)
1. [ ] Шаг 3.1 — memcpy для Copy-типов в move_entity
2. [ ] Шаг 3.2 — кеш find_or_create_archetype_with
3. [ ] Шаг 3.3 — batch insert
4. [ ] Шаг 3.4 — batch despawn
5. [ ] Запустить бенчмарк

---

## Критерии успеха

| Метрика | Текущее значение | Цель |
|---------|-----------------|------|
| Межсистемный speedup (2 CPU-bound, изол.) | 1.28x (100k) / 1.08x (1M) | > 3x |
| Межсистемный speedup (3 CPU-bound, изол.) | 1.39x (100k) / 1.19x (1M) | > 4x |
| has_relation TRUE | 396 ns/op | < 50 ns/op |
| has_relation FALSE | 410 ns/op | < 50 ns/op |
| insert component | 68–79 ns/op | < 50 ns/op |
| despawn | 19–21 ns/op | < 15 ns/op |
