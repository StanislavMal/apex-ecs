# Apex ECS — Рефакторинг: `system!` макрос + эргономика AutoSystem

> **Цель:** Уменьшить boilerplate при объявлении систем с 8-10 строк до 2-6 строк,
> сохранив все текущие преимущества: compile-time access-проверку, именованную диагностику
> конфликтов, быструю компиляцию.
>
> **Принцип:** macro_rules!, не proc-macro. Никаких новых зависимостей.
> Генерирует тот же `impl AutoSystem for X`, что пишется сейчас руками.
>
> **Статус:** ✅ Фаза 1 завершена. Вариант А + Вариант Б работают.

---

## 1. Результаты реализации

### Что сделано

| Шаг | Описание | Статус |
|-----|----------|--------|
| 1 | Default `type Events = ()` в трейте | ❌ Отменено (см. отклонения) |
| 1b | 1-element tuple impl для WorldQuery, ResourceAccessList, EventAccessList | ✅ `query.rs:468`, `system_param.rs:262,315` |
| 2 | `system!` макрос (macro_rules!) | ✅ `system_macro.rs` (~530 строк) |
| 3 | Реэкспорт в `lib.rs` + prelude | ✅ `apex-core/src/lib.rs:13,59`, `apex-app/src/lib.rs:34` |
| 5 | Обновление `minimal.rs` | ✅ Все 5 систем на `system!` |
| — | `compile_error!` для нераспознанных параметров | ✅ Добавлен catch-all arm |
| — | Bare type query (`q: Write<T>`) | ✅ Сверх плана |

### Метрики приёмки

- ✅ `cargo test -p apex-core` — 106 тестов зелёные
- ✅ `cargo test -p apex-app` — 19 тестов зелёные
- ✅ `cargo build --example minimal` — собирается
- ✅ `system!` с 0, 1, 2, 3+ параметрами разных типов (протестировано через minimal.rs)
- ✅ Вариант Б (struct + поля + `s: &mut Self`)
- ⚠️ События (синтаксис есть, не протестирован на реальном примере)
- ⚠️ `cargo clippy` / `cargo fmt` — pre-existing issues в других крейтах

---

## 2. Отклонения от исходного плана

### 2.1 Default `type Events = ()` — НЕ СДЕЛАНО

**Причина:** `associated_type_defaults` — unstable feature (#29661). На stable Rust 1.94.0
не компилируется.

**Решение в макросе:** `type Events = (...)` всегда генерируется явно.
Функционально эквивалентно, разница только в том, что ручной `impl AutoSystem`
всё ещё требует `type Events = ();`.

**Будущее:** ждать стабилизации `associated_type_defaults` или перейти на nightly.

### 2.2 Именование структур — отличается

| План | Факт |
|------|------|
| `fn movement(...)` → `struct MovementSystem` | `fn movement_system(...)` → `struct movement_system` |

**Причина:** macro_rules! не умеет преобразовывать snake_case → CamelCase.
`paste` crate не добавлялся (принцип: без новых зависимостей).

**Решение:** имя функции = имя структуры. Атрибут `#[allow(non_camel_case_types)]`
подавляет warning. Пользователь может использовать PascalCase в имени функции:
`fn MovementSystem(...)`.

### 2.3 Variant B: `s: &mut Self` вместо `self: &mut Self`

| План | Факт |
|------|------|
| `fn run(self: &mut Self, ...)` | `fn run(s: &mut Self, ...)` |

**Причина:** `self` — keyword, не может быть захвачен как `$slf:ident` в macro_rules!.
Из-за hygiene правил, пользовательский `self.field` в теле не может сослаться на
макро-сгенерированный `self` в `fn run(&mut self, ...)`.

**Решение:** пользователь выбирает ЛЮБОЕ имя для state-accessor'а (например `s`, `state`).
Макрос генерирует `let s = &mut *self;` в начале тела `fn run`.
Это имя захватывается как `$slf:ident` и имеет call-site hygiene → доступно в теле.

### 2.4 Event reader: `EventReader<T>` вместо `&[E]`

| План | Факт |
|------|------|
| `let events: &[E] = ctx.event_reader::<E>().read().as_slice()` | `let events = ctx.event_reader::<E>();` |

**Причина:** API `EventReader` не совпал с предположениями плана (нет `.read().as_slice()`).
Возвращается сам `EventReader`, пользователь вызывает `.iter()` для получения `&[E]`.

### 2.5 Event writer: ограничение 1 на систему

План допускал несколько `&mut Vec<E>`, но macro_rules! не может конкатенировать
идентификаторы (без `paste` crate). Буфер использует фиксированное имя `__system_ev_buf`.

### 2.6 Hygiene: `ctx` и `self` передаются как metavariables

Для обхода hygiene-проблем в рекурсивных macro_rules!:
- `ctx` передаётся как `$ctx:ident` через все уровни рекурсии `__system_impl!`
- `s` (state accessor) передаётся как `@slf: [ $slf ]` через accumulator

---

## 3. Текущий синтаксис (актуальный)

### Вариант А — без состояния

```rust
system! {
    fn movement_system(
        q: (Read<Velocity>, Write<Position>),  // query tuple
        keys: &Input<KeyCode>,                  // resource read
    ) {
        for (_, (vel, pos)) in q.iter() {
            if keys.pressed(KeyCode::A) { pos.x -= vel.x; }
        }
    }
}
// Использование: app.add_system(Update, movement_system);
```

### Вариант А — без query (type Query = ())

```rust
system! {
    fn exit_on_escape(
        keys: &Input<KeyCode>,   // resource read
        exit: &mut Exit,         // resource write
    ) {
        if keys.just_pressed(KeyCode::Escape) {
            exit.is_requested = true;
        }
    }
}
```

### Вариант Б — с состоянием

```rust
system! {
    struct WaveSpawner {
        wave: u32 = 1,
        enemies_spawned: u32 = 0,
    }

    fn run(
        s: &mut Self,      // state accessor (любое имя)
        cmd: Cmd,           // commands
        dt: &Time,          // resource read
    ) {
        if s.wave <= 5 {
            cmd.spawn((Enemy, Position::default()));
            s.enemies_spawned += 1;
        }
    }
}
// Генерирует: struct + impl Default с указанными значениями + impl AutoSystem
// Использование: app.add_system(Update, WaveSpawner::default());
```

### Все поддерживаемые типы параметров

| Параметр | Associated type | Let-биндинг |
|----------|----------------|-------------|
| `q: (Read<A>, Write<B>)` | `type Query = (Read<A>, Write<B>)` | `let q = ctx.query::<Self::Query>();` |
| `q: Read<A>` (bare type) | `type Query = (Read<A>)` | `let q = ctx.query::<Self::Query>();` |
| `name: &T` | `ResRead<T>` | `let name: &T = &*ctx.resource::<T>();` |
| `name: &mut T` | `ResWrite<T>` | `let name: &mut T = &mut *ctx.resource_mut::<T>();` |
| `name: &[E]` | `Listen<E>` | `let name = ctx.event_reader::<E>();` |
| `name: &mut Vec<E>` | `Emit<E>` | `let mut __system_ev_buf = Vec::new(); let name = &mut __system_ev_buf;` + flush после тела |
| `name: Cmd` | *(none)* | `let name: &mut Commands = ctx.commands();` |

---

## 4. Что осталось на будущее

| Задача | Приоритет | Оценка |
|--------|-----------|--------|
| Несколько `&mut Vec<E>` в одной системе | Medium | 1-2h (нужен paste crate или генерация уникальных имён счётчиком) |
| Тесты компиляции для Listen/Emit | Medium | 30min |
| Default `type Events` в трейте | Low | Ждать stable Rust |
| CamelCase-именование структур | Low | Ждать paste или proc-macro |
| Обновление остальных примеров (basic, event_pipeline, perf) | Low | 30min |
| Документирование макроса в rustdoc | Low | 30min |
