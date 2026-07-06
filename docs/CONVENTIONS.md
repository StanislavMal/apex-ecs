# CONVENTIONS — соглашения ядра apex-ecs (нейминг + prelude)

> **Статус:** живой документ · заведён 2026-07-05 (кампания API_GOLDEN_PATH, волна 3, координация §8
> Rule D; кампания закрыта 2026-07-06 — канон принят `decisions/ADR-004-api-golden-path-canon.md`,
> план в `plans/archive/API_GOLDEN_PATH.md`). Этот документ — ЖИВОЙ дом канона.
>
> **Область.** Здесь — соглашения, специфичные для ПУБЛИЧНОГО API ядра: словарь имён и политика
> prelude. Общие инженерные правила (§0.2a «громко, не молча», §0.2b «без полумер», §0.9 анти-mimicry,
> язык код=EN/доки=RU, git, статусы, ID) живут в движковом `apex-engine/docs/CONVENTIONS.md` — ядро
> следует им по [ADR-000]. Дублировать их здесь не нужно; этот файл лишь ДОПОЛНЯЕТ их для ядра.
>
> Публикация ядра на crates.io планируется (развилка Р-2). Поэтому нейминг/prelude/`deprecated`-
> дисциплина обязательны ДО первого релиза: после публикации ломать имена = major bump.

---

## 1. Словарь имён (Р-5)

Единый канон для всей публичной поверхности. Sweep волны 3 приводит имена к нему; расхождения —
баг. Опирается на идиому std/Bevy (`HashMap::get`, `slice::get` → `Option`; короткое имя → паника).

| Форма имени | Значение | Возврат | Примеры |
|---|---|---|---|
| **короткое** (`resource`, `entity`, `single`) | доступ, который ОБЯЗАН существовать | значение; **паника** с внятным текстом при отсутствии | `World::resource::<T>()`, `Query::single()` |
| **`get_`** (`get`, `get_mut`, `get_entity`) | **lookup**, который может не найти | `Option`/`Result` | `Query::get(e)`, `World::get_resource` |
| **`try_`** (`try_insert`, `try_despawn`) | **ОПЕРАЦИЯ**, которая может не выполниться | `Result`/`bool` | `Commands::try_insert` |
| **`_mut`** (тотально) | мутабельный вариант того же | — | `iter`/`iter_mut`, `get`/`get_mut`, `query`/`query_mut` |
| **`_dyn`** | доступ по runtime-`ComponentId`/имени (скриптинг/инспектор/IPC) | — | `insert_dyn`, `query_builder` |
| **`unsafe fn`** | обход проверок/инвариантов | — (контракт в `# Safety`) | `Query::new_unchecked` |

**Жёсткие следствия (то, что sweep исправляет):**

- **`try_` — НЕ lookup.** `try_` резервируется за «операция, которая может не выполниться»
  (`try_insert`, `try_despawn`). Lookup ресурса/entity — это `get_`-семья (либо короткое имя с
  паникой). Исторический `try_resource` (= lookup) при переименовании приводится к `get_`-канону
  (`get_resource`); короткое `resource()` = паника. Переход — через `#[deprecated]`-алиасы.
- **`_unchecked`-у место только на `unsafe fn`.** SAFE-функция с суффиксом `_unchecked`, обходящая
  проверки (декларацию доступа и т.п.), — расходится с Rust-конвенцией: суффикс обещает `unsafe`.
  Такие точки либо становятся `unsafe fn`, либо `#[doc(hidden)]`-эскейпами с явным контрактом
  (планировщик — единственный дозволенный вызыватель).
- **`_raw` двусмыслен — не использовать в новых именах.** Историческое `_raw` означало то
  указатели, то dyn-by-id. Указательные точки → `unsafe fn`/`pub(crate)`; by-id → `_dyn`.
  `insert_raw_pub` → `insert_dyn`.
- **Согласованные пары.** Ренейм-цели волны 3: `insert_raw_pub`→`insert_dyn`;
  `spawn_from_template`→`spawn_template_with` (пара к `spawn_template`); `children_of`→`targets_of`,
  `get_relation_target`→`target_of` (пара единичного/множественного); `par_for_each_used_by_name`
  → deprecate (декларативный порядок вместо имени-монстра).

## 2. Prelude — контракт золотого пути

Prelude = минимальный набор для золотого пути. Advanced/внутреннее/маркеры — НЕ в prelude
(импортируются явным путём).

**Входит:** `Component`/`Bundle`/derive; `Entity`; `Commands`; `Query`/`Single`; словарь форм
(`Read`/`Write`/`&T`/`&mut T`/`With`/`Without`/`Maybe`/`MaybeWrite`/`Changed`/`Added`/`Or`) +
`WorldQuery`/`ReadOnlyWorldQuery`/`ArchetypeFilter`; `Res`/`ResMut`; `EventReader`/`EventWriter`;
`World`/`SystemContext`; `add_systems`+конфиг-ordering; relations-сахар (`ChildOf`/`Owns`/
`RelationKind`); `default()`. **Плюс маркеры AutoSystem/`system!`** — это первоклассный путь
авторинга систем, не внутренняя кухня: `AutoSystem`/`SystemParam`/`ExclusiveSystem`,
`ResRead`/`ResWrite` (`type Resources`), `Listen`/`Emit` (`type Events`), `QueryParam`,
`CommandsParam`, `RemovedComponents`.

**НЕ входит (advanced / внутреннее):** `UnsafeWorldCell`; низкоуровневый event-API
(`EventCursor`/`PeekGuard`/`PartialReadGuard`/`EventIterator`/`add_reader`/`advance_reader_mut`) и
`DelayedQueue` (advanced-утилита, §1.5); `RelationHookFn`; `WorldQuerySystemAccess`/`AccessDescriptor`
(выводятся из параметров); `Resources` (pub(crate) после A5); `Dyn*`-семейство (скриптинг/IPC);
`DenseQuery` (chunk-путь — advanced); `SystemBuilder`; `QueryBuilder(Mut)`; `make_serde_fns*`.
Депрекейтнутые алиасы (`Ref`) — тоже вне prelude.

**Scheduler prelude.** У `apex-scheduler` prelude ЗАВОДИТСЯ (сегодня нет): `Scheduler`, `Stage`,
`SystemConfig`, ordering-хелперы золотого пути.

## 3. Дисциплина переименований (до публикации)

Любое публичное переименование волны 3:

1. Вводит НОВОЕ имя (канон §1).
2. Мигрирует ВСЕ консумеры (оба репо: ядро + движок) на новое имя — атомарно, как C6.
3. Оставляет СТАРОЕ имя `#[deprecated(since = "…", note = "use `<new>`")]` (+ `#[doc(alias)]` на
   новом, где помогает миграции). Старое имя после миграции — без вызовов ⇒ deprecate не даёт
   варнингов (net-neutral). Депрекейтнутый алиас НЕ реэкспортируется в prelude (re-export
   деп-item'а варнит — см. `Ref`).
4. Судьба алиаса:
   - **До первого релиза crates.io** (текущий статус ядра): раз все внутренние вызовы мигрированы,
     алиас никого не защищает — он снимается **напрямую, без major-bump**. НЕ по ходу каждого ренейма,
     а ОДНИМ финальным проходом «drop deprecated surface» перед релизом (иначе поздние волны добавят
     алиасы и чистка размажется — двойная работа). Сделано волной P (`plans/archive/API_GOLDEN_PATH.md`;
     ADR-004).
   - **После публикации**: алиас живёт до следующего мажора, удаляется отдельным breaking-коммитом.

[ADR-000]: ../decisions/ADR-000-format.md
