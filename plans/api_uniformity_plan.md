# План: Доработка единообразия API

## Текущие проблемы

### 1. CachedQuery не имеет par_for_each*
Файл: [`crates/apex-core/src/world.rs`](crates/apex-core/src/world.rs:859)

`CachedQuery` имеет только:
- `for_each(|entity, item|)` ✅
- `for_each_component(|item|)` ✅
- `len()` / `is_empty()` ✅

Отсутствуют:
- `par_for_each(|entity, item|)` ❌
- `par_for_each_component(|item|)` ❌

### 2. Скриптовый API (Rhai) не использует Query
Файлы: [`crates/apex-scripting/src/iterators.rs`](crates/apex-scripting/src/iterators.rs), [`crates/apex-scripting/src/rhai_api.rs`](crates/apex-scripting/src/rhai_api.rs)

Скриптовый `query()` — это отдельная реализация итератора (`RhaiQueryIter`), которая:
- Не использует `Query<Q>` из apex-core
- Имеет свою логику `build_arch_states()` (дублирование)
- Имеет свою логику `flush_writes()` (специфичная для Rhai)
- Работает через `Rc<RefCell<ScriptContext>>`

Это **не дублирование API**, а **другой уровень абстракции** — Rhai не имеет доступа к типам Rust, поэтому `query(["Read:Position", "Write:Velocity"])` — это динамический аналог `ctx.query::<(Read<Position>, Write<Velocity>)>()`.

Скриптовый API **не нужно менять** — он уже единообразен сам по себе.

### 3. SubWorld больше не используется пользователями
После удаления `for_each*` из `SubWorld`, он стал чисто внутренним типом для scheduler'а. Это правильно.

## План доработок

### Шаг 1: Добавить par_for_each* в CachedQuery

Добавить в [`CachedQuery`](crates/apex-core/src/world.rs:859):

```rust
// parallel variant (cfg parallel)
pub fn par_for_each_component<F>(&self, f: F)
where
    Q: Send,
    F: Fn(Q::Item<'_>) + Send + Sync,
{
    // chunk-level parallelism, adaptive_chunk_size
}

pub fn par_for_each<F>(&self, f: F)
where
    Q: Send,
    F: Fn(Entity, Q::Item<'_>) + Send + Sync,
{
    // chunk-level parallelism, adaptive_chunk_size
}

// non-parallel fallback (cfg not parallel)
pub fn par_for_each_component<F>(&self, f: F) { self.for_each_component(f); }
pub fn par_for_each<F>(&self, f: F) { self.for_each(f); }
```

### Шаг 2: Проверить скриптовый API

Скриптовый API:
- `query(["Read:Position", "Write:Velocity"])` → `RhaiQueryIter` — **не трогать**, это динамический API
- `read_resource("PhysicsConfig")` / `write_resource(...)` — **не трогать**
- `emit_event("DamageEvent", ...)` — **не трогать**
- `spawn_entity(...)` / `despawn(...)` — **не трогать**

Скрипты используют `SystemContext`? Нет — они используют `ScriptContext`, который имеет прямой доступ к `World`. Это другой уровень.

### Шаг 3: Проверить prelude

Убедиться, что `CachedQuery` экспортируется в prelude (уже есть в [`lib.rs:36`](crates/apex-core/src/lib.rs:36)).

### Шаг 4: Сборка и тесты

- `cargo build --features parallel` — проверить
- `cargo test --features parallel` — проверить
- `cargo run --release --features parallel --example perf` — проверить perf
- `cargo run --release --features parallel --example scripting` — проверить скрипты

## Итоговая матрица соответствия

| Метод | Query | CachedQuery | SubWorld | SystemContext | Rhai |
|-------|-------|-------------|----------|---------------|------|
| `for_each` | ✅ | ✅ | ❌ (internal) | ❌ (через query) | ✅ (query()) |
| `for_each_component` | ✅ | ✅ | ❌ | ❌ | ❌ (не нужно) |
| `par_for_each` | ✅ | ⬜ добавить | ❌ | ❌ | ❌ (Rhai sync) |
| `par_for_each_component` | ✅ | ⬜ добавить | ❌ | ❌ | ❌ (Rhai sync) |
| `len` / `is_empty` | ✅ | ✅ | ❌ | ❌ | ❌ |
| `iter` / `iter_components` | ✅ | ❌ (не нужно) | ❌ | ❌ | ❌ |
