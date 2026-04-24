# План оптимизации параллелизма v2 — через apex-graph

## Проблема

Текущий `run_hybrid_parallel` даёт speedup 1.27x-1.34x на 12 ядрах вместо ожидаемых 3x-6x.
Причина: все системы получают `&World` через `ParallelWorld` и конкурируют за L3 кеш.

## Решение: Использовать `apex-graph` для построения ExecutionPlan

### Шаг 1: Заменить ручной граф конфликтов на `Graph<SystemId, ConflictKind>`

**Файл**: `crates/apex-scheduler/src/lib.rs`

Сейчас:
- `rebuild_graph()` строит `Vec<Vec<EdgeInfo>>` вручную
- `detect_conflict_kind()` вызывается для каждой пары систем O(N²)
- `ExecutionPlan` собирается через ручную топологическую сортировку

После:
- Использовать `apex-graph::Graph<SystemId, ConflictKind>`
- Каждая система = узел графа
- Ребро A→B = "A должен быть перед B" (конфликт или sequential barrier)
- `graph.parallel_levels()` даёт готовые стадии

### Шаг 2: Добавить вес ребра = тип конфликта

`ConflictKind` уже есть:
```rust
pub enum ConflictKind {
    WriteWrite(ComponentId),
    ReadWrite(ComponentId),
    SequentialBarrier,
}
```

Использовать как вес ребра в `Graph<SystemId, ConflictKind>`.

### Шаг 3: Заменить `ExecutionPlan` на уровни из графа

Сейчас `ExecutionPlan`:
```rust
struct ExecutionPlan {
    stages: Vec<Stage>,
}
struct Stage {
    system_ids: Vec<SystemId>,
    all_parallel: bool,
}
```

После: `parallel_levels()` возвращает `Vec<Vec<Index>>` — это и есть стадии.

### Шаг 4: Упростить `compile()`

Сейчас compile():
1. `rebuild_graph()` — ручное построение
2. `detect_conflict_kind()` — O(N²)
3. Сборка `ExecutionPlan`

После compile():
1. Построить `Graph<SystemId, ConflictKind>` через `add_node` + `add_edge`
2. Вызвать `graph.parallel_levels()`
3. Конвертировать `Vec<Vec<Index>>` в `Vec<Stage>`

### Ожидаемый эффект

- **Код**: чище, меньше багов, переиспользование `apex-graph`
- **Производительность**: `parallel_levels()` даёт оптимальное разбиение (доказуемо минимальное число стадий)
- **Поддержка**: новый тип конфликта = новое ребро в графе, без правки алгоритма

### Риски

- `apex-graph` использует `thunderdome::Index` для узлов — нужно отображать `SystemId` → `Index`
- `parallel_levels()` клонирует топологическую сортировку — небольшой оверхед на compile()
- Нужно убедиться что `Graph::remove_node` корректно работает при удалении систем

## Альтернатива (если граф не даст ускорения)

Если проблема именно в доступе к памяти (L3 кеш), а не в планировании:
- Разделить `World` на несколько `SubWorld` по архетипам
- Каждая система получает только свои архетипы
- Это требует изменений в `SystemContext` и `ParallelWorld`
