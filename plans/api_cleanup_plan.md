# План очистки API Apex ECS v0.1.0

## Переименования

| Было | Стало | Причина |
|------|-------|---------|
| `TrackedEventQueue<T>` | **`Events<T>`** | короче, идиоматичнее (Bevy) |
| `ResourceMap` | **`Resources`** | симметрично `Events` |
| `resource()`, `resource_mut()` | оставить | паника как `Res<T>` в Bevy |

## Удаление (без обратной совместимости)

- `EventQueue<T>` alias — удалить
- `advance_reader()` — deprecated no-op
- `read_and_advance()` — заменён на `read()`
- `iter_previous()`, `iter_current()`, `iter_all()`, `len_previous()`

## EventReader

```rust
pub struct EventReader<'w, T: Send + Sync + 'static> {
    events: &'w Events<T>,
    cursor: EventCursor,
}

impl EventReader {
    // SAFE: сам вызывает add_reader(), unsafe не нужен пользователю
    pub fn new(events: &'w mut Events<T>) -> Self {
        let cursor = events.add_reader();
        Self { events, cursor }
    }
    
    pub fn iter(&self) -> &[T] { self.events.iter(&self.cursor) }
    pub fn read(&mut self) -> EventReadGuard<'_, T> { self.events.read(&self.cursor) }
    pub fn len(&self) -> usize { self.iter().len() }
    pub fn is_empty(&self) -> bool { self.iter().is_empty() }
}
```

## План работ

- [x] Анализ codebase
- [x] Создание плана
- [x] **Задача 1**: Рефакторинг ядра — ✅ `cargo check -p apex-core` успешен
- [x] **Задача 2**: Обновление зависимых крейтов — ✅ `cargo check` успешен, `cargo run --example hot_reload_test` пройден
- [ ] **Задача 3**: Обновление документации
- [ ] Проверка сборки
