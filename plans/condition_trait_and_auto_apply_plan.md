# Plan: Condition Trait + Auto-Apply Deferred (v0.3)

> Статус: **проектирование** | Дата: 2026-05-29

## Цель

Два улучшения в apex-scheduler (без изменения версии — остаёмся v0.3):

1. **Trait `Condition`** — условия с typed доступом к миру. Единый метод `run_if`, принимающий `impl Condition`. Планировщик знает какие данные читает условие → автоматически строит dependency edges.

2. **Auto-apply deferred** — `chain(&["spawner", "consumer"])` гарантирует не только порядок, но и видимость команд. `system!` макрос авто-детектит `cmd: Cmd` → `has_deferred` флаг → `compile()` сам вставляет sync points.

---

## Принципы

- **Один метод `run_if`** — пользователь не выбирает между opaque/typed. `Condition` trait разрешает и то, и другое.
- **Автоматизация** — всё что можно вывести из макроса или типов, выводится автоматически.
- **Ноль ручных действий** — `chain()` гарантирует и порядок, и видимость команд.
- **Ноль breakage** — 54 существующих теста проходят без изменений.

---

## Шаг 1: Trait `Condition`

### 1.1 Определение (`apex-scheduler/src/lib.rs`)

```rust
pub trait Condition: Send + Sync + 'static {
    fn check(&self, world: &World) -> bool;
    fn access(&self) -> AccessDescriptor { AccessDescriptor::new() }
}
```

### 1.2 Blanket impl для opaque closures

```rust
impl<F: Fn(&World) -> bool + Send + Sync + 'static> Condition for F {
    fn check(&self, world: &World) -> bool { self(world) }
}
```

`access()` = `AccessDescriptor::new()` → планировщик не видит конфликтов. Это 90% случаев — просто и быстро.

### 1.3 `.not()` на самом Condition

```rust
pub trait Condition {
    fn not(self) -> NotCondition<Self> where Self: Sized {
        NotCondition(self)
    }
}

struct NotCondition<C: Condition>(C);
impl<C: Condition> Condition for NotCondition<C> {
    fn check(&self, world: &World) -> bool { !self.0.check(world) }
    fn access(&self) -> AccessDescriptor { self.0.access() }
}
```

### 1.4 Tuple impls 1..12 — AND внутри кортежа

```rust
impl<A: Condition, B: Condition> Condition for (A, B) {
    fn check(&self, world: &World) -> bool {
        self.0.check(world) && self.1.check(world)
    }
    fn access(&self) -> AccessDescriptor {
        self.0.access().merge(&self.1.access())
    }
}
// + макрос для 3..12
```

### 1.5 `ConditionTree::Leaf` адаптация

`ConditionTree::Leaf` хранит `Box<dyn Fn(&World) -> bool>` — совместимо с `Condition::check()`.
`SystemConfig::run_if` создаёт `Leaf(Box::new(move |w| condition.check(w)))`.

---

## Шаг 2: Typed conditions в `conditions` модуле

Каждый built-in condition реализует `Condition::access()` — планировщик знает доступ.

### 2.1 `resource_exists<T>`

```rust
struct ResourceExists<T>(PhantomData<T>);
impl<T: Send + Sync + 'static> Condition for ResourceExists<T> {
    fn check(&self, w: &World) -> bool { w.has_resource::<T>() }
    fn access(&self) -> AccessDescriptor { AccessDescriptor::new().read::<T>() }
}
pub fn resource_exists<T: Send + Sync + 'static>() -> impl Condition + Clone {
    ResourceExists::<T>(PhantomData)
}
```

### 2.2 `resource_equals<T: PartialEq>`

```rust
struct ResourceEquals<T> { value: T }
impl<T: Send + Sync + 'static + PartialEq> Condition for ResourceEquals<T> {
    fn check(&self, w: &World) -> bool {
        w.try_resource::<T>().map(|r| *r == self.value).unwrap_or(false)
    }
    fn access(&self) -> AccessDescriptor { AccessDescriptor::new().read::<T>() }
}
// Clone когда T: Clone
```

### 2.3 `any_with_component<T: Component>`

```rust
struct AnyWithComponent<T>(PhantomData<T>);
impl<T: Component> Condition for AnyWithComponent<T> {
    fn check(&self, w: &World) -> bool {
        Query::<Read<T>>::new(w).iter().count() > 0
    }
    fn access(&self) -> AccessDescriptor { AccessDescriptor::new().read::<T>() }
}
```

### 2.4 Остальные (`run_until`, `every_n_frames`, `not`)

Адаптировать существующие `RunCondition`-возвращающие функции к `impl Condition`. `run_until` / `every_n_frames` — opaque (access = empty, не читают мир).

---

## Шаг 3: `SystemConfig` — накопление `condition_access`

### 3.1 Новые поля

```rust
pub struct SystemConfig {
    pub(crate) name: String,
    pub(crate) kind: SystemConfigKind,
    pub(crate) condition: ConditionTree,
    pub(crate) condition_access: AccessDescriptor,  // ★ NEW
    pub(crate) has_deferred: bool,                   // ★ NEW
}
```

### 3.2 `run_if` — единый метод

```rust
pub fn run_if<C: Condition>(mut self, condition: C) -> Self {
    self.condition_access.merge(&condition.access());  // ★ накапливаем
    let leaf = ConditionTree::Leaf(Box::new(move |w: &World| condition.check(w)));
    match &mut self.condition {
        ConditionTree::And(ref mut conds) => conds.push(leaf),
        _ => { let old = replace(self.condition, And(Vec::new())); /* push old + leaf */ }
    }
    self
}
```

### 3.3 `or_else` — аналогично

```rust
pub fn or_else<C: Condition>(mut self, condition: C) -> Self {
    self.condition_access.merge(&condition.access());
    let leaf = ConditionTree::Leaf(Box::new(move |w: &World| condition.check(w)));
    match &mut self.condition {
        ConditionTree::Or(ref mut conds) => conds.push(leaf),
        _ => { /* wrap in Or */ }
    }
    self
}
```

---

## Шаг 4: `register_system_config` — мерж condition_access в систему

```rust
fn register_system_config(&mut self, cfg: SystemConfig, ...) -> SystemId {
    match cfg.kind {
        SystemConfigKind::Auto(system, mut access) => {
            access.merge(&cfg.condition_access);  // ★ merge!
            self.systems.push(SystemDescriptor {
                // ...
                has_deferred: cfg.has_deferred,
            });
        }
        SystemConfigKind::Sequential(f) => {
            // sequential: condition_access не важен (уже &mut World)
            self.systems.push(SystemDescriptor {
                has_deferred: cfg.has_deferred,
                // ...
            });
        }
        SystemConfigKind::ParClosure { mut access, func } => {
            access.merge(&cfg.condition_access);  // ★ merge!
            self.systems.push(SystemDescriptor {
                has_deferred: cfg.has_deferred,
                // ...
            });
        }
    }
}
```

**Результат:** если `movement` имеет `run_if(conditions::resource_equals(GamePhase::Playing))`, планировщик видит `read<GameState>`. Система `toggle_pause` пишет `write<GameState>` → `detect_conflict_kind` находит `WriteRead` → разные Stage → автоматический порядок. **Ноль ручного `chain()` для этой ситуации.**

---

## Шаг 5: `has_deferred` auto-detect

### 5.1 `AutoSystem` trait

```rust
pub trait AutoSystem: Send + Sync {
    // ...
    /// Система использует Commands? Ставится system! макросом автоматически.
    const HAS_DEFERRED: bool = false;
}
```

### 5.2 `system!` макрос

При обнаружении `cmd: Cmd` в параметрах — генерирует:

```rust
impl AutoSystem for $fn_name {
    const HAS_DEFERRED: bool = true;
    // ...
}
```

### 5.3 `SystemConfig` — передача флага

`s(id, name, system)` проверяет `S::HAS_DEFERRED` и ставит `has_deferred: true`.

---

## Шаг 6: Auto-split в `split_at_apply_boundaries`

Добавляется автоматическая вставка split-точек для ordered пар с `has_deferred`:

```rust
fn split_at_apply_boundaries(
    ids: &[SystemId],
    systems: &[SystemDescriptor],
    explicit_orderings: &FxHashSet<(SystemId, SystemId)>,
) -> Vec<Vec<SystemId>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();

    for (i, &id) in ids.iter().enumerate() {
        let is_last = i + 1 == ids.len();
        current.push(id);

        let sys = systems.iter().find(|s| s.id == id);
        let manual_split = sys.map(|s| s.apply_deferred_after).unwrap_or(false);

        let auto_split = !is_last && {
            let next_id = ids[i + 1];
            sys.map(|s| s.has_deferred).unwrap_or(false)
                && explicit_orderings.contains(&(id, next_id))
        };

        if (manual_split || auto_split) && !is_last {
            groups.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() { groups.push(current); }
    groups
}
```

**Результат:** `chain(&["spawner", "camera"])` где `spawner` имеет `HAS_DEFERRED=true` → compile() видит `explicit_orderings.contains(&(spawner_id, camera_id))` + `has_deferred` → auto-split → camera видит заспавненные entity. **Ноль ручного `apply_deferred()`.**

---

## Шаг 7: `SystemBuilder` — удаление

`SystemBuilder<'a>` всё ещё существует в lib.rs (наследие v0.2). Удаляется полностью:
- struct `SystemBuilder<'a>` — удалить
- impl `SystemBuilder` — удалить
- Все методы `add_auto_system`, `add_system`, `add_par`, `add_par_access` — остаются как `pub(crate)` делегаты к `register_system_config`

Тесты используют `pub(crate)` методы, не ломаются.

---

## Шаг 8: Тесты и пример

### 8.1 Новые тесты

| Тест | Проверяет |
|---|---|
| `condition_trait_opaque_closure` | `\|w\| true` → `Condition` работает |
| `condition_trait_typed_resource_exists` | `conditions::resource_exists` → `access()` содержит `read<T>` |
| `condition_trait_tuple_and` | `(cond_a, cond_b)` → AND + мерж access |
| `condition_trait_not` | `.not()` → инвертирует результат, access тот же |
| `auto_apply_deferred_from_chain` | `chain()` с `has_deferred` → stage split |
| `auto_apply_deferred_no_split_without_chain` | Без chain → одна группа |
| `condition_access_causes_conflict` | typed condition → `WriteRead` конфликт → разные Stage |
| `condition_access_no_conflict_opaque` | opaque condition → нет конфликта → один Stage |

### 8.2 Обновление `scheduling_features.rs`

- `run_if(conditions::resource_equals(GamePhase::Playing))` — typed (было opaque)
- `chain(&["movement", "ai"])` — демонстрация auto-apply
- Tuple conditions: `.run_if((cond_a, cond_b))`

---

## Итоговая таблица

| # | Шаг | Где | Строк |
|---|---|---|---|
| 1 | Trait `Condition` + blanket + `.not()` | `lib.rs` | 20 |
| 2 | Tuple impls 1..12 | `lib.rs` | 25 |
| 3 | Typed `conditions::*` | `conditions.rs` | 35 |
| 4 | `condition_access` + `has_deferred` на `SystemConfig` | `config.rs` | 8 |
| 5 | `run_if` / `or_else` → `impl Condition` + мерж access | `config.rs` | 15 |
| 6 | `register_system_config` → мерж condition_access | `lib.rs` | 8 |
| 7 | `HAS_DEFERRED` на `AutoSystem` + system! макрос | `system_param.rs` + `system_macro.rs` | 15 |
| 8 | `sys()` конструктор → `S::HAS_DEFERRED` | `config.rs` | 3 |
| 9 | Auto-split в `split_at_apply_boundaries` | `lib.rs` | 10 |
| 10 | Удаление `SystemBuilder` | `lib.rs` | -100 |
| 11 | Тесты | `lib.rs` tests | 40 |
| 12 | Обновление примера | `scheduling_features.rs` | 10 |
| **Итого** | | | **~90 строк (нетто), 1-1.5 дня** |

---

## Что НЕ делаем (почему)

| Отказ | Причина |
|---|---|
| `fn`-алиасы `.and()`/`.or()` на `Condition` | Дублируют tuple-AND и `or_else`-OR. Держим один способ |
| `run_if` с обязательным `AccessDescriptor` | Opaque closure (`\|w\| ...`) — 90% случаев, не хотим бойлерплейта |
| `conditions::and_all((...))` | Tuple уже даёт AND |
| Смена версии на v0.4 | Пока нет крупных изменений |
