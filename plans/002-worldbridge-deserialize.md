# План 002 — фикс десериализации в WorldBridge

## Проблема

`WorldBridge::send_event<T>()` сериализовал событие (bincode) и отправлял как `BridgeEvent::Event`.
Но `apply_incoming()` на принимающей стороне только логировал warning и **молча выбрасывал данные**.
`send_event` был мёртвым кодом, который вводил в заблуждение.

## Решение

Добавлен общий реестр десериализаторов (`Arc<RwLock<HashMap<String, EventHandler>>>`) в `WorldBridge` и `CloneableBridge`.

### API

| Метод | Описание |
|---|---|
| `bridge.register_event::<T>(&mut world)` | Регистрирует `T` в `world` (через `add_event`) и сохраняет bincode-десериализатор |
| `bridge.send_event::<T>(&event)` | Сериализует и отправляет (без изменений) |
| `bridge.apply_incoming(&mut world)` | **Десериализует** зарегистрированные типы через обработчик; для незарегистрированных — warning |

### Где

`crates/apex-isolated/src/lib.rs`:
- `WorldBridge` — строка 41, поле `event_handlers`
- `WorldBridge::register_event::<T>()` — новый метод
- `WorldBridge::apply_incoming()` — теперь вызывает десериализатор
- `CloneableBridge` — аналогичные изменения
- `CloneableBridge::send_event()`, `CloneableBridge::register_event()` — новые методы

### Тесты

2 новых теста:
- `world_bridge_send_event_round_trip` — полный цикл send → apply → verification
- `world_bridge_send_event_missing_handler` — корректно предупреждает без паники

### Результат

- 12 тестов apex-isolated — все зелено
- 134+ тестов workspace — 0 failures
