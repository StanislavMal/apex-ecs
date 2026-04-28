# План 001 — ctx.try_resource / ctx.try_resource_mut

## Задача

Добавить безопасные (не паникующие) аналоги `ctx.resource()` / `ctx.resource_mut()` на `SystemContext`.

## Где

`crates/apex-core/src/world.rs` — impl блок `SystemContext<'w>`

## Что сделано

1. **`ctx.try_resource::<T>() -> Option<Res<'_, T>>`** — делегирует в `world.try_resource::<T>()`, заворачивает в `Res`
2. **`ctx.try_resource_mut::<T>() -> Option<ResMut<'_, T>>`** — использует `resources.get_raw_ptr::<T>()` (тот же unsafe-паттерн, что у существующего `ctx.resource_mut()`)

## Тесты

4 теста в `#[cfg(test)] mod tests`:

| Тест | Проверяет |
|---|---|
| `system_context_try_resource_some` | `Some(Res)` когда ресурс вставлен |
| `system_context_try_resource_none` | `None` когда ресурс отсутствует |
| `system_context_try_resource_mut_some` | `Some(ResMut)` когда ресурс вставлен |
| `system_context_try_resource_mut_none` | `None` когда ресурс отсутствует |

## Результат

- 54 теста — всё зелено
- workspace компилируется с нуля
