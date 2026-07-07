# ADR-006 — IsolatedWorld exchange protocol + единая snapshot-схема + `WorldRegistrar` в ядре

**Статус:** принят 2026-07-07 (кампания EDITOR_GOLDEN_PATH, волны 1–3). Планка —
[[apex-foundation-first-principle]] + §0.9 (использовать козыри relations/IsolatedWorld/snapshot, не
мимикрировать). Закрывает долг **В4** и **string-table** (реестр ядра, `plans/TECH_DEBT.md`).

**Контекст.** `IsolatedWorld` давал только *изоляцию* (мир владеет своими сущностями); переноса
сущностей между мирами не было. Оба реальных потребителя вручную изобретали недостающую половину:
рендер штампует `MainEntity`-обратную-ссылку, редактор держит `EditorIdMap` + свой snapshot/restore +
командный лог. Ремаппинг-машинерия уже существовала в сериализаторе (`map_entity_refs`), но не была
подключена к isolated. Параллельно снапшот держал **две рассинхронизированные версионные схемы** (u32
`version` + вестигиальный `SnapshotVersion{major,minor}`), гейт загрузки был строгим `==`, а `migrate()`
вызывался ТОЛЬКО в `read_from_file` — редактор грузил сцены мимо миграции (любой бамп версии молча ломал
on-disk сцены). `type_name` писался на каждый инстанс (без интернирования).

**Решение.**
1. **Одна версионная схема снапшота.** Удалён `SnapshotVersion`; авторитетно `WorldSnapshot::version: u32`
   + миграционная цепочка. Гейт `restore` — ЧЕРЕЗ миграцию, не `==`: `restore_impl` мигрирует
   не-текущую версию (аллокация только когда версия отстаёт; текущая — allocation-free), будущую —
   явная `VersionMismatch`. ВСЕ потребители (в т.ч. редактор через `restore_with`/`from_json`)
   участвуют в миграции — не только байт-путь `read_from_file`.
2. **Wire format v3 — string-table.** Version-gated `WireSnapshotV3` интернирует `type_name` в таблицу
   (`name_idx: u32`), version-peek по ведущему полю; in-memory `WorldSnapshot` не трогается; v2-байты
   грузятся. Закрывает долг string-table.
3. **Протокол обмена (`apex-isolated::exchange`)** поверх СУЩЕСТВУЮЩЕЙ snapshot-машинерии, НЕ вторая
   реализация: `export_entities`/`export_world` (snapshot, опц. фильтр поддерева по `ChildOf`-замыканию),
   `import` (restore — spawn свежих + ремап ссылок), `apply_back` (`restore_merge_with` — merge:
   переиспользует сущности, что `resolve(old_index)` мапит по внешнему ключу, остальные spawn'ит;
   named-компоненты overwrite, не-названные — не трогаются; change-tick бампается ⇒ консументы видят).
4. **`WorldRegistrar`** — replayable schema-рецепт (register serde/relation/event, `apply`/`new_world`).
   Миры, что обмениваются snapshot'ами, делят ОДНУ схему — дрейф двух ручных списков = молчаливая потеря
   данных. **Живёт в `apex-core`** (чистый `World`-примитив, ноль isolation-семантики); `apex-isolated`
   ре-экспортит. Редактор (deps apex-core, не apex-isolated) использует его напрямую для play-форка.
5. **Soundness/гигиена.** Убран ложный `unsafe impl Sync` (ни один потребитель не алиасит
   `&IsolatedWorld` кросс-поточно — рендер передаёт владение); мост bounded + backpressure-политика +
   телеметрия (было unbounded). Miri targeted по мосту.

**Последствия.** Перенос поддерева между мирами с ремапом ссылок — first-class, покрыт кросс-поточными
тестами (`cross_thread.rs`). Редактор больше не минует миграцию — бамп версии безопасен для on-disk
сцен. `apply_back` — построенный и тестированный примитив; его продуктовый потребитель (agent-preview
рассмотрен и отклонён — см. движковый ADR по editor-world-model) — будущий multi-user/streaming
(EDIT-M11). Рендер оставлен на `MainEntity`-модели осознанно (extract-таргет, не snapshot-таргет —
ручная регистрация ×35 by-design, D9-класс различия). `WorldRegistrar` в ядре — единый примитив для
любого World-консумента.
