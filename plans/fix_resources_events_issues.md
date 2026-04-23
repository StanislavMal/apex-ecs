# План исправления проблем Resources + Events API

## Проблема 1 (Критическая): RefCell double-borrow при write_resource/emit_event внутри query()

### Корень
- [`RhaiQueryIter`](crates/apex-scripting/src/iterators.rs:113) хранит `ctx: Rc<RefCell<ScriptContext>>` и вызывает `ctx.borrow()` в `next()` и `flush_writes()`
- [`write_resource()`](crates/apex-scripting/src/rhai_api.rs:155) и [`emit_event()`](crates/apex-scripting/src/rhai_api.rs:165) вызывают `ctx.borrow()` через замыкания
- Если скрипт вызывает `write_resource()` внутри `for entity in query(...)`, то `RhaiQueryIter` уже держит `ctx.borrow()` → второй `ctx.borrow()` от `write_resource()` → **RefCell panic**

### Решение: Буферизация запросов на запись ресурсов и отправку событий

Аналогично тому, как сделано для spawn/despawn — накапливать запросы во время скрипта, применять после.

**Шаг 1.1**: Добавить в [`ScriptContext`](crates/apex-scripting/src/context.rs:65) буферы:
```rust
// Буфер отложенных записей ресурсов: (type_name, Dynamic)
pub(crate) deferred_resource_writes: RefCell<Vec<(&'static str, Dynamic)>>,
// Буфер отложенных событий: (type_name, Dynamic)
pub(crate) deferred_events: RefCell<Vec<(&'static str, Dynamic)>>,
```

**Шаг 1.2**: Изменить [`write_resource()`](crates/apex-scripting/src/context.rs:238) — вместо немедленной записи через `world_ptr.as_mut()`, пушить в `deferred_resource_writes`:
```rust
pub fn write_resource(&self, type_name: &str, value: &rhai::Dynamic) {
    self.deferred_resource_writes.borrow_mut()
        .push((type_name, value.clone()));
}
```

**Шаг 1.3**: Изменить [`emit_event()`](crates/apex-scripting/src/context.rs:250) — аналогично:
```rust
pub fn emit_event(&self, type_name: &str, value: &rhai::Dynamic) {
    self.deferred_events.borrow_mut()
        .push((type_name, value.clone()));
}
```

**Шаг 1.4**: Добавить метод [`apply_deferred_resources_and_events()`](crates/apex-scripting/src/context.rs:174) (рядом с `apply_deferred`):
```rust
pub(crate) fn apply_deferred_resources_and_events(&mut self) {
    let writes = std::mem::take(&mut *self.deferred_resource_writes.borrow_mut());
    let events = std::mem::take(&mut *self.deferred_events.borrow_mut());
    let world = unsafe { self.world_mut() };
    
    for (type_name, value) in writes {
        if let Some(binding) = self.resource_bindings.get(type_name) {
            (binding.write)(world, &value);
        }
    }
    for (type_name, value) in events {
        if let Some(binding) = self.event_bindings.get(type_name) {
            (binding.emit)(world, &value);
        }
    }
}
```

**Шаг 1.5**: В [`ScriptEngine::run()`](crates/apex-scripting/src/script_engine.rs:427) добавить вызов после скрипта, перед `clear_world_ptr()`:
```rust
// Применяем deferred ресурсы и события
self.ctx.borrow_mut().apply_deferred_resources_and_events();
```

**Шаг 1.6**: В [`ScriptContext::new()`](crates/apex-scripting/src/context.rs:97) инициализировать новые поля.

**Шаг 1.7**: В [`ScriptContext::set_world_ptr()`](crates/apex-scripting/src/context.rs:117) очищать новые буферы.

### Безопасность
- `deferred_resource_writes` и `deferred_events` — `RefCell<Vec<...>>`, не `Send` (как `deferred_spawns`)
- Применяются после скрипта, когда `RhaiQueryIter` уже дропнут → нет double-borrow
- `value.clone()` — `Dynamic::clone()` дешёвый (Rhai использует `Rc` внутри)

---

## Проблема 2 (Средняя): `world.send_event()` паникует если событие не зарегистрировано

### Корень
- [`world.send_event::<T>()`](crates/apex-core/src/world.rs:300) вызывает `self.events.get_mut::<T>()`, который паникует если `add_event::<T>()` не был вызван
- Паника внутри Rhai-замыкания превращается в `EvalAltResult::ErrorRuntime`

### Решение: Добавить `try_send_event` в `World`

**Шаг 2.1**: Добавить в [`World`](crates/apex-core/src/world.rs:299) публичный метод:
```rust
/// Безопасная версия send_event — возвращает false если событие не зарегистрировано.
pub fn try_send_event<T: Send + Sync + 'static>(&mut self, event: T) -> bool {
    if let Some(queue) = self.events.try_get_mut::<T>() {
        queue.send(event);
        true
    } else {
        false
    }
}
```

**Шаг 2.2**: Изменить [`register_event::<T>()`](crates/apex-scripting/src/script_engine.rs:317) — использовать `try_send_event` вместо `send_event`:
```rust
emit: |world: &mut World, value: &Dynamic| -> bool {
    if let Some(event) = T::from_dynamic(value) {
        if world.try_send_event(event) {
            true
        } else {
            log::warn!("emit_event: событие '{}' не зарегистрировано", T::type_name_str());
            false
        }
    } else {
        log::warn!("emit_event: не удалось конвертировать Dynamic в {}", T::type_name_str());
        false
    }
},
```

---

## Проблема 3 (Низкая): `read_resource` не проверяется в тесте 9

### Решение: Добавить проверку в тест

**Шаг 3.1**: В [`hot_reload_test.rs`](crates/apex-examples/examples/hot_reload_test.rs:460) изменить скрипт `read_res` чтобы он возвращал значение, и проверить его:
```rust
engine.load_script_str("read_res", r#"
fn run() {
    let g = read_resource("Gravity");
    log(`Gravity = ${g.value}`);
    if g.value != 9.8 {
        throw("Gravity mismatch");
    }
}
"#).expect("загрузка read_res");
```

---

## Порядок выполнения

1. **Шаг 1.1** — добавить поля в `ScriptContext`
2. **Шаг 1.6** — инициализация в `new()`
3. **Шаг 1.7** — очистка в `set_world_ptr()`
4. **Шаг 1.2** — изменить `write_resource()` на буферизацию
5. **Шаг 1.3** — изменить `emit_event()` на буферизацию
6. **Шаг 1.4** — добавить `apply_deferred_resources_and_events()`
7. **Шаг 1.5** — вызвать в `ScriptEngine::run()`
8. **Шаг 2.1** — добавить `try_send_event()` в `World`
9. **Шаг 2.2** — использовать `try_send_event` в `register_event()`
10. **Шаг 3.1** — усилить тест 9
11. `cargo check` — проверить сборку
12. `cargo run --example hot_reload_test` — проверить все тесты
