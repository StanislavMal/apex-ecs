# Кампания: API Golden Path — унификация публичного API + переписывание руководства (ApexForge_ECS)

> **Статус: 🔜 план (2026-07-05, анализ выполнен).** Источник истины кампании; статусы пунктов — ЗДЕСЬ.
> Метод анализа: 4 параллельных агента (руководство целиком ×2, инвентаризация pub-поверхности по коду,
> уроки API-дизайна по исходникам Bevy `C:\My\Projects\Rust_projects\bevy` — bevy_ecs **0.19.0-dev**).
> Планка: `docs/CONVENTIONS.md` §0.2a/§0.2b/§0.9 (анти-mimicry: Bevy-паттерн перенимаем только где он
> идиоматически верный Rust; козыри — relations/snapshot/dynamic query/детерминизм/cost-model — образцовы).
> Связанный реестр: `plans/TECH_DEBT.md` (S1–S4, A5, C6, F4, guide-broken). **Развилки §5 требуют
> решения пользователя ДО старта волн 2–4.** Брендинг: ядро будет называться **ApexForge_ECS**.

## 0. TL;DR

**Диагноз.** Функционально ядро после CORE_AUDIT — top-tier; но ПОВЕРХНОСТЬ API перегружена и
руководство ей врёт. Цифры: `World` — **87 pub fn** (world.rs целиком — 143); **10 публичных
query-типов / ~23 точки входа**; **11 способов итерации**; **7 путей отправки события / 6 чтения**;
**4 уровня параллелизм-конфига** (2 из них полурабочие); **7 способов регистрации систем** (но
`add_systems` уже единственный pub-вход — ✅). Руководство (4961 строка): **≥6 классов
некомпилирующихся примеров**, версия врёт (0.3.0 vs 0.1.0 в Cargo), 52+ внутренних шифра
(W2-0/волны/TD) в пользовательском доке, справочник §16 благословляет снятый F3-API, зоопарки без
таблиц выбора, «Полный пример» §15 не компилируется.

**Уроки Bevy (по исходникам, переносимые):** prelude = контракт золотого пути; единый Query-фасад
(`&self`-read через `D::ReadOnly`-подстановку / `&mut self`-write — один тип в голове); «доступ =
параметры» без бога-объекта SystemContext; трёхрегистровый нейминг (короткое = паника /
`get_` = Option/Result / `try_` = fallible; `unchecked` = всегда `unsafe fn`); ErrorHandler-ресурс
с Severity+контекстом (системная реализация нашего §0.2a); лестница EntityRef/EntityMut/EntityWorldMut;
rustdoc-стандарты (таблица сложности Query, `#[doc(alias)]`+`deprecated(since,note)`).

**НЕ копировать (мы лучше):** relations (двусторонний индекс vs их Vec-на-хуках, one-to-many only);
snapshot/MapEntities в ядре (у bevy_ecs нет вовсе); детерминированный спавн + cost-model (Bevy id
недетерминированы, executor без модели стоимости).

---

## 1. Находки: публичная поверхность API (полные таблицы — отчёт агента, ключевое здесь)

### 1.1 Query-зоопарк (→ C6, TECH_DEBT)
`Query` (7 конструкторов!) + `CachedQuery` + `QueryState` — **три копии одного метод-сета**
(for_each/par_for_each/for_each_chunk/iter/len — q:1589–1824 ≈ w:2706–2934). Плюс `DenseQuery`-трейт,
`DynQuery`/`DynQueryMut`, `QueryBuilder`/`QueryBuilderMut`, `Single`, `QueryParam`-маркер. Входы:
`World::query/query_mut/query_changed/query_mut_changed/query_builder(_mut)` + ctx-зеркала +
QueryState-пятёрка. Единый `Query<'w,'s>` (C6) схлопывает ~35 pub fn в ~12. `Single` есть в prelude,
но не в корневом реэкспорте (асимметрия).
**⚠ Словарь query-данных — ДВОЙНОЙ (решение волны 3, обсуждено 2026-07-05):** `Read<T>`/`Write<T>`
(маркер-структуры) И `&T`/`&mut T` (ссылки) — обе пары дают идентичный Item; плюс `Ref<T>` = алиас
`Read<T>` — **семантическая ловушка** (в Bevy `Ref` = read+change-detection, отдельная семантика). Это
зоопарк. **Золотой путь (Bevy):** ОДИН словарь — `&T`/`&mut T` для данных + НАСТОЯЩИЕ `Ref<T>`/`Mut<T>`
для change-detection; `Read`/`Write` → `#[deprecated]`-алиасы `&T`/`&mut T`, постепенная миграция сайтов
(сотни, но не больно через deprecate). **Не блокирует C6:** `WorldQuery::ReadOnly` (шаг 1, commit f9f191c)
проецирует КАЖДУЮ форму независимо от словаря; reference-формы уже спроецированы `&mut T→&'a T`
(refs→refs, без кросс-словарной путаницы).

### 1.2 Итерация — 11 способов
Целевая четвёрка на едином Query: `iter` / `for_each` / `par_for_each` / `for_each_chunk(+par)`.
`SubWorld::for_each_entity/par_for_each_entity/for_each_row/par_for_each_row` — транспорт
планировщика → pub(crate). DynQuery несогласован (read — `iter`, mut — только `for_each_mut`).

### 1.3 Спавн/структурные
Золотой путь: `spawn` / `spawn_batch` / `Commands::spawn→EntityCommands`. `spawn_many(_silent)` →
merge в `spawn_batch` (или deprecate); `spawn_reserved`+reserver-семейство → advanced-модуль
«entity reservation» (D8b); `insert_raw_pub` → **переименовать** (`insert_dyn`); `spawn_template` vs
`spawn_from_template` — рассогласованная пара → `spawn_template(name)` + `spawn_template_with(name,
params)`. Хорошее: spawn_at/insert_parts/bulk уже спрятаны ✅.

### 1.4 Ресурсы — 4 словаря
`World::resource/resource_mut/try_*` — Bevy-совместимо ✅; не хватает `init_resource`/`get_or_insert_with`.
`Resources` — **pub-ПОЛЕ мира** (см. A5, §1.11) + свой get/try_get словарь → pub(crate) целиком.
`ctx.*_unchecked` — safe fn мимо проверок (S2-класс). `SubWorld::resource_mut(&self)` — S1-класс
вообще без суффикса. `ResRead/ResWrite`-маркеры — из prelude убрать (макро-уровень).

### 1.5 События — 7 путей отправки, 6 чтения, 3 слоя flush
Золотой путь: `EventWriter`/`EventReader`-параметры (+F4 персистентные курсоры — TECH_DEBT) +
`World::send_event`. `Events::send_sync`-семейство — advanced (по сути S2-обход декларации);
низкоуровневый reader-API (`add_reader/advance_reader_mut/PeekGuard/PartialReadGuard` — **сейчас в
prelude!**) → pub(crate); `EventRegistry`-слой → pub(crate); у World один кадровый глагол
`advance_frame` (сейчас 6 pub-глаголов тиков/flush). `DelayedQueue` — не интегрирован с миром
(юзер сам держит и флашит) — дорастить или честно «utility».

### 1.6 Регистрация систем — фактически уже неплохо
`add_systems(stage, configs)` — единственный pub-вход ✅ (add_system/add_auto_system/add_par_system —
уже private ✅). Зоопарк в остатке: дубли `sys/seq/par/par_access` (свободные fn И ассоциированные на
SystemConfig) → merge; **ordering строковый** (`before("a","b")`) отдельно от конфигов, а `run_if` —
на конфиге → перенести `.before()/.after()/.chain()` на SystemConfig/кортежи (Bevy-идиома здесь
идиоматически верна, §0.9-чек пройден); `SystemBuilder` → pub(crate) после миграции; `par_for_each_
used_by_name` — имя-монстр, deprecate в пользу декларации. Стейл-док: заголовок scheduler lib.rs:12
зовёт несуществующий `ctx.for_each::<Self::Query,_>`.

### 1.7 Параллелизм-конфиг — 4 уровня, 2 полурабочих
`set_par_chunk_size` (свободная fn, глобальный atomic!) + `init_par_chunk_size_from_env` + env —
глобалка молча подмешивается в `ChunkConfig::default()` (порядок вызовов меняет состояние мира);
`World::set_chunk_config(ChunkConfig)`; `Scheduler::set_parallel_min_entities/auto_disable` —
поля-дубли ChunkConfig. **Целевая модель: `ChunkConfig` на World — единственный носитель**;
`ChunkConfig::from_env()` — один env-вход; Scheduler читает из мира; глобалки deprecate/pub(crate).

### 1.8 Тики/изменения
`Changed/Added` + ленивый `Mut` — ✅ Bevy-паритет. Но 6 pub-глаголов тиков на World (`tick/current_
tick/last_run_tick/set_last_run_tick/advance_change_tick/advance_frame`) — юзеру нужен один
`advance_frame`, остальное advanced/pub(crate). `query_changed(last_run: Tick)` с ручным тиком —
advanced (в системах база автоматическая).

### 1.9 Relations (козырь — дотянуть до образца)
Ядро ✅: add/remove/has_relation, batch, query_relation/query_wildcard, EntityCommands-сахар богат.
Дыры первокласности: `children_of` vs `get_relation_target` — несогласованные имена (→ `targets_of`/
`target_of`); иерархический сахар есть ТОЛЬКО на Commands (EntityRef без set_parent/add_child);
`iter_relations()` отдаёт сырые `(u32,u32,Entity)`; SubjectIndex/TargetIndex pub-методы → pub(crate);
нет relation-термов в QueryBuilder (S8); Bevy-урок для UX: декларативный `children![...]`-аналог
поверх наших relations.

### 1.10 Snapshot/serde
`WorldSerializer` — единый хаб ✅ (17 fn, структура «база+_with» консистентна). Регистрация раскидана
(5 методов World + дубль-слой Resources) → сжать рассказ; `make_serde_fns*` в корневом реэкспорте →
doc(hidden).

### 1.11 🔴 Сырая поверхность (НОВОЕ, → TECH_DEBT A5)
**`World.archetypes` и `World.resources` — pub-ПОЛЯ** (w:198/213): `world.archetypes.clear()` из
safe-кода ломает все инварианты. Это аудит-A5, который «делаем безусловно, волны 1/4» (§8.В1(а)) —
**выпал**. Туда же: `Scheduler::run_sequential(*mut World)` — pub с сырым указателем;
`Resources::get_raw_ptr`; `World::event_queue_ptr`; `MainWorld(pub World)`+unsafe Send/Sync;
`compute_archetype_indices`/`populate_type_names` (фазы compile — pub). Одна волна с S1/S2.

### 1.12 Prelude + нейминг
Prelude-лишнее: UnsafeWorldCell, EventCursor, PeekGuard/PartialReadGuard/EventIterator, RelationHookFn,
WorldQuerySystemAccess, AccessDescriptor, Resources, Dyn*-семейство, DenseQuery, ResRead/ResWrite/
Listen/Emit. Не хватает: **prelude в apex-scheduler нет вообще**; Single в корне lib.rs. Нейминг-разнобой:
`try_` vs `get_` vs `try_get` — три стиля; `_raw` — два смысла (указатели vs dyn-by-id); `_unchecked` —
safe fn мимо проверок (расходится с Rust-конвенцией); `insert_raw_pub`. **Словарь-цель (Р-5 решён):** короткое =
паника / `get_` = Option/Result lookup / `try_` = fallible-вариант ОПЕРАЦИИ (не lookup) / `_dyn` =
by-ComponentId / `unsafe fn` = мимо проверок (не `_unchecked`-safe) / `_mut` тотально. Зафиксировать
в CONVENTIONS ядра + механический sweep (волна 3).

---

## 2. Находки: руководство (реестр битого — полные таблицы в отчётах агентов)

### 2.1 BROKEN — классы некомпилирующихся примеров (≥6, сверх известного F3-guide)
1. **§6.7 + §16 (4451–4455): `ctx.resource_mut()/try_resource_mut()/event_writer()`** — методы сняты
   F3.2 (теперь `*_unchecked`+doc(hidden)); справочник благословляет снятый API.
2. **§6.8 Extract-примеры (2416–2443)**: маркеры (`Extract<QueryParam>`, `ResWrite`) использованы как
   plain-fn параметры — не компилируется ни одна из 3 витрин extract-паттерна.
3. **§15 «Полный пример» (4072–4090)**: компоненты без `#[derive(Component)]` — первое, что копирует
   новичок, не собирается.
4. **§4: `world.query_changed::<(…,Write<…>)>` (634–643)** — требует ReadOnly; правильный
   `query_mut_changed` в руководстве не упомянут ВООБЩЕ.
5. **§17 Lua: entity id как integer (4706–4762)** — E10 намеренно отверг голый индекс (id = строка
   "index:generation"); руководство учит анти-паттерну, despawn молча не сработает.
6. **§11: `load_directory` с 3 аргументами (3274) + миф про расширение `.prefab` (3140/3261/4514)** —
   код берёт только `*.prefab.json`; hot-reload по инструкции не заведётся.
7. Мелочь: `WaveSpawner::new(cfg)` (384) — system! не генерирует `new`; §13.4 сниппет с внешним `cmds`
   в `par_for_each` (3708) — не Fn; `while let` irrefutable (1023); `diff_snapshots` как свободная fn
   (2983); `produced_by(id, name)` — 2 аргумента вместо 1 (2277).
Известный класс F3-guide (~10 × `ctx.query::<…Write…>`) — сверх этого.

### 2.2 STALE — главное
Версия «0.3.0» (шапка) vs 0.1.0 (Cargo и футер!); §13.2 — формула чанка из другой эпохи (128/32-ярусов
нет; par_for_each давно `par_split_run_ranges`, не по-архетипный чанкинг); `set_par_chunk_size` в
таблице Scheduler (это свободная fn, у Scheduler её нет); §10 молчит про версию снапшота v2/migrate()/
ресурсы(E7)/MapEntities(E6) — а restore при version-mismatch ошибается; §16 прячет весь write-путь
запросов (нет query_mut/new_mut/query_builder_mut) и `set_deterministic_spawn`; §14.9 заголовочная
таблица противоречит собственной сноске (schedule 🔴 vs факт 1.45×); гнилые file:line-ссылки ×10+;
«SystemContext — read-only view» (2220) при живом commands().

### 2.3 Структура/педагогика
Нумерация §6 сломана (два §6.7; 6.4.1 после 6.6; в §4 нет 4.3.3); 52+ внутренних шифра (W*/CR-M*/П*/
волны/даты) — читается как changelog; Mut<T> объяснён 4 раза, apply_deferred — 4, flush событий — 4;
~230 строк бенч-архива в §14.7–14.9 (вкл. «коррекции честности» и признание UB-бага — внутренняя
кухня); два справочника Scheduler (§16 «API» и «API (v0.3)»); §16 стоит ПЕРЕД §17 и дублирует его
таблицы; «App API» в §16 — чужой крейт (apex-engine) без пометки; §2.4 Bevy-миграция — движковые
материи (Msaa/Camera/Handle) в доке ядра; нет quick start (первый цельный пример — §15, строка 4062!);
низкоуровневые подсекции событий (§5.2.2–5.2.4) идут ДО закрепления золотого пути.

### 2.4 INTERNAL-LEAK в доке
ConditionTree-хак «чистого OR» (1431); per-cursor события как полноправный слой; `matching_archetype_
ids`; `event_reserve_by_type`; APEX_MAIN_PROF (движковая диагностика!); §13.3 SubWorld row-API целиком;
§14.8 «применённые оптимизации»; §17.8 «публичное API для скриптинга» (интеграционные внутренности).

---

## 3. Целевая картина (что учит руководство после кампании)

**Один очевидный способ на задачу; advanced — явно маркирован и после базы.**

| Задача | Золотой путь | Advanced (маркировано) |
|---|---|---|
| Запрос в системе | параметр `Query<Q,F>` (единый тип C6) | `Single`, ручной тик, DynQuery (редактор/IPC) |
| Итерация | `iter`/`for_each`; `par_for_each` (CPU-bound) | `for_each_chunk` (dense) |
| Спавн | `spawn`/`spawn_batch`; из систем `Commands::spawn` | reservation-модуль (D8b), шаблоны |
| Ресурсы | `Res/ResMut`-параметры; `World::resource(_mut)`+`try_` | — |
| События | `EventReader/EventWriter`-параметры; `World::send_event` | delayed, sync-эскейпы |
| Регистрация | `add_systems(Stage, (a, b.after(a)).run_if(...))` | `system!` (stateful), AutoSystem |
| Иерархия | relations-сахар на Commands И EntityRef; `children![]`-аналог | dyn-relations, wildcard |
| Snapshot | `WorldSerializer` хаб + register_*-рассказ одним блоком | diff/prefab-пути |
| Параллелизм | ничего не делать (cost-model сам); `ChunkConfig` при нужде | диагностические env |
| Ошибки | ErrorHandler-ресурс + Severity (§0.2a системно, урок Bevy) | per-call `try_`-варианты |

Структура руководства-цели: §1 Quick start (20 строк до работающего мира) → концепции → золотой путь
по задачам → козыри (relations/snapshot/детерминизм/dynamic query — витринные главы) → advanced →
справочник (В КОНЦЕ, генерируемый/сверяемый) → Lua. Ноль внутренних шифров; версия из Cargo; бенчи —
одна актуальная таблица + ссылка на архив кампании.

---

## 4. Развилки — ✅ РЕШЕНЫ (2026-07-05)

> **✅ РЕШЕНЫ пользователем 2026-07-05 — критерий «самый правильный золотой путь AAA».**

- **Р-1 → params-as-access = ЗОЛОТОЙ ПУТЬ.** Обычные `fn` с типизированными параметрами (Query,
  Res/ResMut, Commands, EventReader/EventWriter, Local для состояния) — основной способ; вывод
  AccessDescriptor из типов параметров. `system!`-макрос — второй уровень для случаев, где он реально
  нужен; `SystemContext` уходит в advanced/`#[doc(hidden)]` целиком. **Мутабельный запрос = декларированный
  `Query<Q>`-параметр**; публичный `ctx.query_mut` НЕ вводим (закрывает F3-guide класс 1: примеры
  мигрируют на параметр, не на ctx). F6-ограничение «один query на system!» снимается самой моделью
  (несколько Query-параметров — норма). ⚠ Объём: это выходит за косметику — снять F6, доработать вывод
  access из параметров, депрекейтнуть SystemContext-write-поверхность. Полноценно, не полумерой (§0.2b).
- **Р-2 → только брендинг доков; крейты `apex-*` остаются.** Меняем: заголовки/футер/имя файла
  руководства, README, продуктовые упоминания «Apex ECS» → **ApexForge_ECS**. `use apex_core::…`,
  Lua-API, path-зависимости движка — НЕ трогаем. **crates.io: все имена свободны** (проверено 2026-07-05:
  `apexforge-ecs`, `apexforge`, `apex-ecs`, `apex-core`, `apex-scheduler`, `apex-serialization`,
  `apex-macros`, `apex-isolated`, `apex-scripting`, `apex-hot-reload`, `apex-graph` — все 404/FREE).
  Публикация планируется → нейминг/prelude/`deprecated`-дисциплина обязательны ДО первого релиза
  (после публикации ломать имена = major bump). Зарезервировать `apexforge-ecs` и ключевые `apex-*`
  имена стоит заранее (owner-squat) — решение о моменте резерва за пользователем.
- **Р-3 → ordering на конфигах ПРИНЯТ.** `.before()/.after()/.chain()` на SystemConfig/кортежах —
  золотой путь (Bevy-идиома, §0.9-чек пройден: идиоматически верный Rust, не мимикрия). Строковый
  `before("a","b")`-API остаётся для ДИНАМИЧЕСКОГО порядка (по имени, редактор/скрипты) — advanced.
- **Р-4 → лестница `EntityRef`/`EntityMut`/`EntityWorldMut` ПРИНЯТА** (уровни доступа в типах:
  read-only / мутация компонентов без структурных / полный). Вводим в волне 1 (S1 всё равно меняет
  сигнатуры entity-доступа) — `EntityMut` годится как QueryData с disjoint-доступом (урок Bevy).
- **Р-5 → канон `get_` = Option/Result; `try_` = fallible-вариант ОПЕРАЦИИ.** `get::<T>()`/`get_mut`/
  `get_entity` возвращают Option/Result (std/Bevy-идиома: `HashMap::get`, `slice::get`); короткое имя
  (`resource`, `entity`) = паника с внятным текстом; `try_` резервируется за «операция, которая может
  не выполниться» (`try_insert`, `try_despawn`), НЕ за lookup. Механический sweep: `try_resource`→
  оставить как есть (это lookup ресурса — здесь `try_` исторически = get; переименовать в справочнике
  осознанно) → **уточнение при волне 3**: привести к `get_`-семье для lookup, `try_` только для
  fallible-мутаций; `unsafe fn` вместо `_unchecked`-safe-обходов.

### 4.1 Blast-radius волны 1 (recon 2026-07-05)
Кросс-репо-риск МАЛЫЙ: write-query сайтов (query_mut/new_mut) ~**53 в ядре + 3 в движке**; поля
`World.archetypes`/`World.resources` движок **не трогает** (все `.resources` в движке — собственная
структура frame_graph) → приватизация A5 = чистая внутри-apex-core правка, ноль поломок движка.
`&mut self` на write-аксессорах правит те же ~53+3 сайта (компилятор найдёт). Вывод: волна 1 доводится
атомарно, goldens-гейт покрывает поведенческую эквивалентность.

---

## 5. Волны

**Волна 0 — развилки ✅ РЕШЕНЫ** (§4, 2026-07-05) + **спайк C6 ✅ ВЕРДИКТ: GO** (2026-07-05).

### Спайк C6 — результат (изолированный lifetime-прототип скомпилирован+исполнён)
**Вывод: единый `Query<'w, 's, D, F>` ОСУЩЕСТВИМ, риск низкий.** Кодовая база уже на 2/3 там:
`QueryState<Q>` = готовый `'s`-владелец (инкрементальный, привязка по world_id — как Bevy `QueryState`);
`CachedQuery<'w,Q>` = почти-view (уже несёт `ArchIndices::{Shared(Arc)|Borrowed(&'w)}`); отдельный
`Query<'w,Q,F>` c inline-`SmallVec`-стейтом — избыточный третий тип. Проверено прототипом:
- **View держит `UnsafeWorldCell<'w>`, НЕ `&'w World`** — иначе write-аксессор невыразим (rustc запрещает
  `&→&mut`-каст; это ровно S1 в миниатюре). Read через `&self`+`cell.world()`, write через
  `&mut self`+`cell.world_mut()`. `UnsafeWorldCell` в ядре уже есть (волна 6).
- **`StateSrc<'s> = {Owned(inline) | Borrowed(&'s)}`** — ОДИН view-тип покрывает ad-hoc (owned, сегодня
  `Query`), Arc-cache (сегодня `CachedQuery::new`) и per-system/SubWorld (borrowed). ⇒ **C6 лендится БЕЗ
  В3**: В3 позже лишь добавляет `Borrowed(&'s QueryState)` fast-path для plain-fn (не блокер).
- **`iter(&self)` через `type ReadOnly`-проекцию** (маппинг `&mut T→&T`) даёт read-итерацию даже на
  write-форме — «один тип в голове» (модель Bevy). У нашего `WorldQuery` `type ReadOnly` ПОКА НЕТ —
  надо добавить + impl на каждую форму (Read→Read, Write→Read, Maybe→Maybe, MaybeWrite→Maybe, кортежи
  поэлементно, фильтры→сами) — механически, но реально. `iter_mut/get_mut(&mut self)` = S1-фикс.

**Два подготовительных расширения трейта (входят в волну):** (а) `WorldQuery::type ReadOnly` + impls;
(б) `QueryState<D,F>` получает filter-параметр (сегодня CachedQuery — только `Q`, а `Query` — `(Q,F)`;
унифицировать на `<D, F=()>`).

**⇒ Уточнение секвенирования (Правило C конкретизировано):** S1-аксессоры (`&mut self`) и C6-merge —
ОДНА работа (unified view держит `UnsafeWorldCell` + `&self`-read/`&mut self`-write С САМОГО НАЧАЛА).
Поэтому **волны 1 и 2 сливаются**: A5/S2/S3 (не трогают Query-тип) идут первыми как «1a», затем
unified `Query<'w,'s>`+ReadOnly+sound-аксессоры как «1b/2» одним кросс-репо проходом (~53 ecs + 3 engine
сайта). Аксессоры НЕ трогаются дважды.

**Волна 1a — Soundness БЕЗ Query-типа (🔴).** ✅ ВЫПОЛНЕНА (2026-07-05, ветка `api-golden-path`).
Закрыты два реально достижимых из safe-кода прохода: **A5 pub-поля** `World.archetypes`/`resources` →
pub(crate) (commit b2a1ff5; потребители на `try_resource`/`insert_resource`, +2 serde-делегата) и
**S2** `ctx.fetch` → `fetch_unchecked` + `#[doc(hidden)]` (ADR-002-консистентно; пример мигрирован).
**Реклассификация по факту анализа** (§0.2b — не полумерить): (а) raw-МЕТОДЫ (`run_sequential(*mut)`→
`&mut World`, `get_raw_ptr`, `event_queue_ptr`, `compute_archetype_indices`, `populate_type_names`) —
футганы, НЕ UB-из-safe (нужен явный unsound-каст) → **харденинг поверхности волны 3**; (б) **S3/S4**
(`World::event_writer/event_reader(&self)` + `World: Sync`) — реализуемый race лишь при явном шеринге
`&World` меж-поток; чистый фикс = per-system курсоры (F4) → **волна 4** (rename-only был бы полумерой);
(в) `SubWorld::resource_mut/event_*(&self)` — ноль вызовов, `&SubWorld` наружу не отдаётся (недостижимо
из safe) → dead-code/харденинг **волны 3/4**. *Гейт волны 1a: workspace tests ✅, clippy net-neutral ✅,
движок all-targets, goldens byte-identical.*

**S1 ЧАСТЬ 2 ✅ (2026-07-05) — read/write accessor split (standalone, ДО C6).** Прагматично разведено с
C6-merge: soundness 🔴 не ждёт структурной схлопки. `&self`-аксессоры (`iter`/`for_each`/`par_for_each`/
`*_chunk`) → бинд `ReadOnlyWorldQuery`; добавлены `&mut self`-варианты `*_mut` (эксклюзив). `iter_raw` для
внутренних счётчиков; `CachedQuery::iter_mut` привязан к заёму (`'_`); `system!` биндит query `mut`. Полная
кросс-репо миграция write-итерации на `_mut`. Гейты все зелёные (детали — TECH_DEBT S1). Форвард-совместимо
с C6: сигнатуры `_mut`-аксессоров переживают merge в unified `Query<'w,'s>`. **NEXT = C6** (структурная
схлопка Query+CachedQuery+QueryState).

**Волна 1b/2 — Unified Query + sound-аксессоры (🔴 S1 + C6, ОДНА работа — спайк GO).** Единый
`Query<'w, 's, D, F>` держит `UnsafeWorldCell<'w>` + `StateSrc<'s>{Owned|Borrowed}`; `type ReadOnly`-
проекция на `WorldQuery` (+impls на все формы) → `iter(&self)` read даже на write-форме; `iter_mut/
get_mut(&mut self)` = S1-фикс сразу (аксессоры не трогаются дважды). `QueryState<D,F>` (+filter);
`CachedQuery`/inline-`Query` схлопываются в него, ~35→12 pub fn, 7 конструкторов → new/new_mut+unsafe-
эскейп; SubWorld-итерации pub(crate); Single в корневой реэкспорт; Ref-алиас deprecate; снять Clone с
write-view. Лендится БЕЗ В3 (owned-inline покрывает plain-fn; В3 позже = Borrowed(&'s) fast-path).
Атомарный кросс-репо проход (~53 ecs + 3 engine). PoC S1 перестаёт компилироваться. *Гейт: workspace+
движок+goldens byte-identical; Miri TB (cell+read/write+alias).*

**Волна 3 — Регистрация+конфиг+нейминг.** Ordering на конфигах (Р-3); merge sys/seq/par-дублей;
SystemBuilder → pub(crate); ChunkConfig-единая модель (§1.7), глобалки deprecate; naming-sweep по
словарю (§1.12: insert_raw_pub→insert_dyn, spawn_from_template→spawn_template_with, children_of/
get_relation_target→targets_of/target_of, par_for_each_used_by_name→deprecate); prelude-диета +
scheduler-prelude; `deprecated(since,note)`+`doc(alias)` на всех переименованиях (урок Bevy §8).

**Волна 4 — События+ошибки.** F4 (персистентные курсоры на State-слоте — TECH_DEBT); reader-API/
EventRegistry → pub(crate); send_sync → advanced; ErrorHandler-ресурс с Severity/контекстом (§0.2a
системно — вместо разрозненных warn_once там, где уместна политика уровня приложения).

**Волна 5 — Руководство** (после стабилизации API): переписывание под целевую структуру §3 —
quick start, перенумерация, справочник в конец (+полнота: query_mut-семейство, DynQuery, set_
deterministic_spawn, register_resource_serde/map_entities, v2/migrate), вычистка 52 шифров и
бенч-архива, починка ВСЕХ классов §2.1, §10-дописать (E6/E7/версии), §17-Lua под строковый id,
App API → в док движка, брендинг **ApexForge_ECS** (Р-2), версия из Cargo. Сверка примеров
компиляцией (выборочно — doctest-извлечение или примеры-крейт).

**Волна 6 — Relations-полировка** (козырь до образца): targets_of/target_of, EntityRef-сахар,
`children![]`-аналог, типизированный iter_relations, индексы pub(crate). Витринная глава руководства.

Порядок жёсткий только 0→1→2 (сигнатуры меняются — руководство переписывать один раз, В КОНЦЕ);
3/4/6 переставимы. Кампанию можно прервать после любой волны (нет полусмонтированных состояний).

## 6. Гейты (каждая волна)

`cargo test --workspace` (ядро) зелёный; clippy net-neutral; движок `cargo build --workspace` +
goldens **byte-identical** (текущий счётчик 649/0+9 ignored); затронут unsafe → целевой Miri TB;
API-переименования — только с `deprecated(since,note)` до следующего мажора (не ломать движок молча);
руководство волны 5 — примеры выборочно скомпилированы.

## 7. Ротация при закрытии (CLAUDE.md)

План → `plans/archive/`; решения Р-1..Р-5 → ADR ядра; статусы → TECH_DEBT (снять S1/S2/A5/C6/F4/
guide-broken; завести остатки); руководство = сам deliverable; фичи движку видимые (переименования) —
migration-заметка в CHANGELOG ядра; мерж --no-ff, ядро ДО движка.

---

## 8. Координация с CORE_AUDIT / TECH_DEBT — последовательность против двойной работы

Обе кампании живут в `plans/active/`. Общие файлы → жёсткий порядок, иначе переделываем дважды:

- **Правило A (главное). CORE_AUDIT «волна 7» (EN-миграция rustdoc/комментариев + декомпозиция
  `scheduler/lib.rs`) идёт ПОСЛЕ API-волн 1–4.** EN-миграция переписывает ВЕСЬ rustdoc — он должен
  документировать ФИНАЛЬНЫЕ имена/сигнатуры (после naming-sweep волны 3 и C6 волны 2), иначе перевод
  делается дважды. Декомпозиция scheduler/lib.rs — механический сплит ФИНАЛЬНОГО кода (после ordering/
  registration-рефактора волны 3), иначе сплит переигрывается. **User-facing литералы волны 7
  (compile_error!/panic/log)** можно EN-ить в любой момент (они меняются мало) — не блокирует.
- **Правило B. Переписывание руководства — ОДИН раз, в API-волне 5** (после стабилизации API волн
  1–4/6). Отменяет пункт CORE_AUDIT-волны 7 «руководство — сверка после всех волн» (дубль). Руководство
  — RU-док, от EN-миграции КОДА не зависит; зависит только от финального API.
- **Правило C. C6-спайк — ДО решения о стратегии S1-аксессоров.** Если спайк говорит «merge `Query<'w,'s>`
  осуществим и не огромен» → делать C6-структуру и S1 `&mut self`-аксессоры ОДНИМ проходом по call-sites
  (unified-тип с `&mut self` сразу), не трогать аксессоры дважды. Если спайк говорит «отложить/рискованно»
  → S1 standalone на 3 типах СЕЙЧАС (soundness 🔴 не ждёт); `&mut self` forward-совместим с будущим C6
  (сигнатура аксессора переживает merge) → не потеряно, лишь CachedQuery-аксессоры трогаются дважды.
- **Правило D. Naming-словарь (Р-5) нужен ДОМ + spec ДО sweep'а волны 3.** У apex-ecs НЕТ своего
  `docs/CONVENTIONS.md` (правила §0.2a/§0.2b/§0.9 живут в движковом `apex-engine/docs/CONVENTIONS.md`,
  ядро следует им по ADR-000). Завести `apex-ecs/docs/CONVENTIONS.md` с naming-словарём (§1.12) + prelude-
  политикой ПЕРЕД волной 3 — тогда sweep механически проверяем по споке.

**Порядок глобально (обе кампании):** C6-спайк → API-в1 (soundness) → API-в2 (C6, по спайку) →
[CONVENTIONS] → API-в3 (registration/naming) → API-в4 (события/ошибки) → API-в6 (relations) →
CORE_AUDIT-в7 (EN-миграция кода + декомпозиция scheduler + тест-кампании) → API-в5 (руководство).
Тест-кампании волны 7 (scripting/isolated/hot-reload/events/par) от API-порядка НЕ зависят — можно
вести параллельно в любой момент (они добавляют тесты, не трогают сигнатуры).
