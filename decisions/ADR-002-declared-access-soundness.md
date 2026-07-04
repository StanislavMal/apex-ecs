# ADR-002 — Доступ = декларированный SystemParam (F3 soundness)

**Статус:** принят (кампания WAVE6B, 2026-07-04) · план-источник `plans/archive/WAVE6B_SOUNDNESS_DETERMINISM.md`

**Контекст.** Планировщик параллелит системы по `AccessDescriptor::conflicts_with`, выведенному из
ДЕКЛАРИРОВАННЫХ параметров системы. Публичный `SystemContext` из safe-кода раздавал
**НЕдекларированный** мутабельный доступ: `ctx.query::<Write<T>>()` (без бинда `ReadOnlyWorldQuery`,
в отличие от `World::query`), `ctx.resource_mut`, `ctx.event_writer`. Система с декларацией `Res<A>`,
дёргающая `ctx.resource_mut::<B>()`, и система с `ResMut<B>` — по декларациям не конфликтуют → бегут
параллельно → пишут B → **data race из safe-кода**. Тот же класс 🔴, что B1(в) закрыл для
`Query::new` (алиасящий `&mut` из safe-кода). Sound-движок не должен иметь safe-достижимую гонку ни
в одном публичном пути.

**Замер (метод-урок).** «Сотни сайтов» — была ошибка grep'а (`world.resource_mut` = эксклюзивный
`&mut World`, 140 шт., sound). Реальная дыра — только receiver `SystemContext`: ~14 ecs + 18 движок
≈ 32 сайта.

**Решение.** Убрать недекларированный мутабельный доступ с **благословенной** поверхности
`SystemContext`, оставив декларированный путь (SystemParam / `system!`-макрос, валидируемый
планировщиком):

- `SystemContext::query`/`query_changed` получают бинд `ReadOnlyWorldQuery` (F3.1). Декларированный
  (возможно мутабельный) путь — `#[doc(hidden)] query_unchecked`, зовут только `SystemParam`-имплы и
  `system!`-макрос (access валидирован планировщиком).
- `resource_mut`→`resource_mut_unchecked`, `try_resource_mut`→`try_resource_mut_unchecked`,
  `event_writer`→`event_writer_unchecked` + `#[doc(hidden)]` (F3.2). Read-аксессоры
  (`resource`/`try_resource`/`event_reader`) остаются благословенными.
- **Форма (a) — золотой путь** (доступ = параметр), НЕ (b) debug-валидация: (b) — философия
  runtime-флагов, от которой мы сознательно ушли к типам (B1(в)).
- **Выбор `#[doc(hidden)]`+`_unchecked`, а НЕ `pub(crate)` и НЕ форс-миграция input-систем на
  plain-fn.** `pub(crate)` требует роутинга макроса на `SystemParam::fetch` — лайфтайм-риск.
  Миграция apex-input на plain-fn: системы уже sound (декларируют доступ), input НЕ покрыт goldens
  → regression-риск; макро-миграция упирается в cross-module видимость (`system!` генерит приватные
  структуры) → inner-module хак = не чисто. `_unchecked` на ручных AutoSystem — честный/греппабельный
  сигнал «доступ декларирован в `type Resources`, валидирован планировщиком».
- **`commands()` НЕ трогали** (осознанно): deferred-аксессор (не live-`&mut` в мир); благословенный
  путь = `Commands` param; ASD-корректность undeclared-commands ловит D8b `has_deferred || uses_commands`.

**Последствия.**
- Недекларированный live-write (query/resource/event) больше недостижим с благословенной поверхности
  safe-кода. Access системы полностью выражен параметрами → планировщик может доверять `conflicts_with`.
- **Ноль нового unsafe, поведенчески идентично** (тот же `from_sub_world`, лишь bind + rename).
  Gate: apex-ecs workspace, apex-input тесты 14/0, clippy net-neutral, goldens движка byte-identical.
- **Остаток (опциональный polish, НЕ soundness-блокер):** миграция ручных AutoSystem'ов
  (apex-input и др.) на plain-fn/`system!` (access = параметр, mismatch невозможен) — требует
  vis-поддержки в `system!`-макросе ИЛИ inner-module + integration-тестов для input (regression-гард).
  Системы уже sound → это качество-кода.
