# ADR-007 — Авторская `Visibility` — компонент ядра (не рендера)

**Статус:** принят 2026-07-07 (кампания движка EDITOR_MODERNIZATION, волна 5). Планка — foundation-first:
авторские данные scene-graph живут в ядре, производное render-состояние — в рендере. Движковая сторона —
`apex-engine/decisions/ADR-016` (serde per-world) + `ADR-013` (honored Visibility, поправлен).

**Контекст.** Движок ввёл honored `Visibility{Inherited|Visible|Hidden}` в apex-render (ADR-013). Но это
**авторский** компонент (намерение пользователя «скрыть», как `LocalTransform` — намерение «где объект»),
а не render-специфичный GPU-тип. Render-агностичные инструменты — редактор (apex-editor зависит ТОЛЬКО от
apex-core) — не могли ставить/сериализовать ту же `Visibility`, что рендер honor-ит, и были вынуждены
держать собственный маркер + per-crate зеркало.

**Решение.** Перенести enum `Visibility` (Inherited/Visible/Hidden) в **apex-core** (`src/visibility.rs`,
Component + serde-derive), экспорт в prelude. Enum зависит только от `apex_core::Component` — перенос
чистый. **Derived-часть остаётся в apex-render:** `InheritedVisibility` (вычисляемое), `propagate_visibility`
(система), exclude-at-extract — они используют `MeshRenderer`/light-компоненты рендера. apex-render
**re-export-ит** `apex_core::Visibility` (`pub use`), так что весь её honored-visibility-код и внешние
`apex_render::Visibility`-потребители работают транзитивно, без изменений.

serde-derive НЕ форсирует сериализацию — snapshot пишет только компоненты с зарегистрированным serde
(per-world opt-in; движковая сторона — ADR-016). Игровой мир Visibility не регистрирует ⇒ дефолт
байт-идентичен.

**Последствия.** Авторская видимость — first-class компонент ядра, доступный любому render-агностичному
консументу без зависимости от рендера (редактор бросил свой `Hidden`-маркер + per-frame зеркало —
единая истина). Разделение чистое: авторский enum в ядре, производное `InheritedVisibility`/propagate в
рендере. apex-core тесты 265 зелёные; движковые goldens 651 байт-идентичны (перенос+re-export транзитивны).
Ядро пушить ДО движка.
