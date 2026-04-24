# API Simplification — Post-Refactoring Review

## 1. Единая точка входа для итерации — ✅ Query

**Было**: 3 overlapping API (SystemContext.for_each*, SubWorld.for_each*, Query.for_each*)
**Стало**: только `ctx.query::<Q>().for_each*()`

Удалённые методы:
- `ctx.for_each_component::<Q, _>(...)` — ❌ удалено
- `ctx.par_for_each_component::<Q, _>(...)` — ❌ удалено
- `ctx.for_each::<Q, _>(...)` — ❌ удалено
- `ctx.par_for_each::<Q, _>(...)` — ❌ удалено
- `sub_world.for_each_component::<Q, _>(...)` — ❌ удалено
- `sub_world.par_for_each_component::<Q, _>(...)` — ❌ удалено
- `sub_world.for_each::<Q, _>(...)` — ❌ удалено
- `sub_world.par_for_each::<Q, _>(...)` — ❌ удалено

Единственный путь:
```rust
// Sequential
ctx.query::<Q>().for_each_component(|item| { ... });
ctx.query::<Q>().for_each(|entity, item| { ... });

// Parallel
ctx.query::<Q>().par_for_each_component(|item| { ... });
ctx.query::<Q>().par_for_each(|entity, item| { ... });
```

## 2. Чистота API — ✅ SystemContext

`SystemContext` теперь содержит только:

| Метод | Назначение |
|-------|-----------|
| `query::<Q>()` | Создать Query для итерации |
| `query_changed::<Q>(last_run)` | Query с фильтром Changed |
| `resource::<T>()` | Чтение ресурса |
| `resource_mut::<T>()` | Запись ресурса |
| `event_reader::<T>()` | Чтение событий |
| `event_writer::<T>()` | Запись событий |
| `entity_count()` | Подсчёт entity |

Никаких дублирующих методов итерации.

## 3. Чистота API — ✅ SubWorld (internal)

`SubWorld` теперь только внутренний тип для scheduler'а:

| Метод | Назначение |
|-------|-----------|
| `resource::<T>()` | Для scheduler'а |
| `resource_mut::<T>()` | Для scheduler'а |
| `event_reader::<T>()` | Для scheduler'а |
| `event_writer::<T>()` | Для scheduler'а |
| `archetype_count()` | Для scheduler'а |
| `entity_count()` | Для scheduler'а |

SubWorld не экспортируется в prelude — это internal API.

## 4. Единообразие — ⚠️ Дублирование for_each / for_each_component

В `Query` есть 4 метода итерации:
- `for_each(|entity, item|)` — с Entity
- `for_each_component(|item|)` — без Entity
- `par_for_each(|entity, item|)` — параллельно с Entity
- `par_for_each_component(|item|)` — параллельно без Entity

**Проблема**: `for_each` и `for_each_component` — дублирование.
**Решение**: можно объединить — `for_each` всегда даёт Entity, пользователь пишет `|_, item|`.

## 5. Производительность — ⚠️ par_for_each пересоздаёт состояние

В `par_for_each_component` и `par_for_each` каждый чанк заново вызывает `Q::fetch_state()`.

**Проблема**: для N чанков на архетип `fetch_state` вызывается N раз вместо 1.
**Решение**: переделать на `(arch_idx, state, start, end)` — вычислять `state` один раз на архетип.

## 6. Единообразие — ⚠️ CachedQuery без par_for_each

`CachedQuery` имеет `for_each` и `for_each_component`, но не имеет `par_for_each*`.

**Решение**: добавить `par_for_each*` в `CachedQuery`.

## 7. Prelude — ✅

`prelude` экспортирует всё необходимое. SubWorld не в prelude.

## Итоговая оценка

| Критерий | Оценка |
|----------|--------|
| Чистота (нет дублирования) | ✅ 9/10 |
| Простота (один путь) | ✅ 10/10 |
| Единообразие (все методы есть везде) | ⚠️ 7/10 |
| Производительность | ⚠️ 7/10 |
| Миграция (обратная совместимость) | ✅ 10/10 |

## Что можно улучшить

1. Добавить `par_for_each*` в `CachedQuery`
2. Оптимизировать `par_for_each*` — fetch_state один раз на архетип
3. Объединить `for_each` и `for_each_component`
4. Убрать неиспользуемое поле `last_run` из `Query`
