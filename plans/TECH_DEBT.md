# apex-ecs — Реестр технического долга (ядро)

> **Назначение.** Живой реестр ОТКРЫТОГО долга ядра apex-ecs: сознательные упрощения, слабые/сырые
> места, отложенные с обоснованием пункты. Правило **§0.2a** (громко, не молча): любое такое место —
> либо чиним по ходу, либо заносим сюда **в тот же заход** с severity и планом. Правило **§0.2b**
> (без полумер): запись здесь — отложенный по приоритету долг, а НЕ лицензия на упрощённое решение;
> когда долг берётся в работу, он закрывается полным решением уровня эталона, не заплаткой.
>
> **Один факт — одно место.** Этот файл держит СТАТУС + указатель; детали живут в источнике
> (`plans/active/CORE_AUDIT.md` §-раздел, `decisions/ADR-*`, архив кампании). Реестр движка —
> отдельный (`apex-engine/plans/TECH_DEBT.md`, ключи `TD-NN`); ключи ядра — аудит-ID (A/B/C/D/E/F-nn)
> и лейблы кампаний. **Коллизия имён:** «C6» здесь = аудит-C6 ядра (query-zoo), НЕ «C6/TD-78»
> движка (ROADMAP). Всегда квалифицировать репозиторием.
>
> **Severity:** 🔴 high (soundness/UB/корректность) · 🟡 med (фиделити/перф/масштаб/§0.2a-громкость) ·
> 🟢 low (чистота/эргономика/док-честность).
>
> **Происхождение.** Реестр заведён 2026-07-04 при закрытии кампаний CORE_AUDIT/PARALLELISM —
> собрал «бездомные» пункты (числились в реестре находок аудита, но при ротации остались бы только
> в смертных планах) + находки объективной верификации волн 6/6б (сессия 2026-07-04, лейблы S1–S8).

---

## 🔴 Soundness — недоделки borrow-модели (верификация 2026-07-04)

> B1(в) (волна 6) выразила эксклюзивность в типах на КОНСТРУКТОРАХ запроса; аксессоры и часть
> `World`/`SystemContext`-поверхности остались лазейками. Заявка «soundness в типах» до конца НЕ
> выполнена. Приоритет — первым заходом.

- **S1 ✅ ЗАКРЫТ ЦЕЛИКОМ — ЧАСТЬ 1 (2026-07-05, commit e79ad1e) + ЧАСТЬ 2 (2026-07-05) — read/write-аксессоры.**
  **Часть 1 ✅ (документированный PoC закрыт):** `Query::get`/`single` возвращали ОДИН дублируемый item;
  `new_mut::<Write<T>>().get(e)` дважды → два `Mut<T>` на одну строку (aliasing из safe). → `get`/`single`
  требуют `Q: ReadOnlyWorldQuery` (`&self`), write — через `get_mut`/`single_mut` (`&mut self`, эксклюзив).
  Ноль миграции. `CachedQuery` НЕ Clone (верификатор ошибся — `derive(Clone)` на `ArchIndices`, не на
  CachedQuery) → того вектора нет.
  **Часть 2 ✅ (концурентный вектор, read/write split):** `for_each`/`iter`/`par_for_each`(`&self`) выдавали
  `Mut` транзиентно → single-thread sound, но `CachedQuery`/`Query: Sync` ⇒ два scoped-потока с `&q`
  писали одни строки (data race из safe; тот же корень, что S3 `World: Sync`). **Сделано (Bevy read/write
  split):** `&self`-аксессоры (`iter`/`for_each`/`par_for_each`/`*_chunk`) требуют `Q: ReadOnlyWorldQuery`
  (+`F` у типизированного `Query`); добавлены `&mut self`-варианты `iter_mut`/`for_each_mut`/
  `par_for_each_mut`/`for_each_chunk_mut`/`par_for_each_chunk_mut` (эксклюзив → нельзя шарить между потоками).
  `IntoIterator for &Query` гейтнут ReadOnly, `&mut Query` даёт writes; `CachedQuery::iter_mut` возвращает
  привязанный к заёму итератор (`'_`, не `'w`) — повторный `iter_mut` не алиасит. Внутренние счётчики
  (`len`/`is_empty`/`single_impl`) через приватный `iter_raw`. `system!`-макрос биндит query-параметры `mut`
  (`#[allow(unused_mut)]`). Внешняя миграция write-итерации на `_mut` — по обоим репо (bench/scheduler/ecs-
  examples + engine app/render/picking/editor-host/examples). **Гейты:** workspace tests зелёные (244 core);
  clippy net-neutral; движок `check --all-targets` чист; goldens **649/0/9 byte-identical**; Miri TB чист на
  split + chunk-mut + write-путях. (Смежное:
  `SubWorld::resource_mut/event_*(&self)` недостижимо из safe → волна 3/4 dead-code, не S1.)
- **A5 ✅ ЗАКРЫТ ЦЕЛИКОМ (pub-ПОЛЯ 2026-07-05 commit b2a1ff5; raw-остаток верифицирован закрытым
  2026-07-06, кампания CORE_POLISH волна 0.1) — сырая pub-поверхность хранилища.**
  `World.archetypes`/`World.resources` были **pub-ПОЛЯ** (`world.rs:198/213`): `world.archetypes.clear()`
  из safe-кода ломал инварианты. **→ pub(crate)**; потребители (scripting/isolated/hot-reload/
  serialization/examples) переведены на `World::try_resource`/`insert_resource`; +`World::
  snapshot_resources_serde`/`restore_resource_serde` для сериализатора. **ОСТАТОК (харденинг поверхности)
  — верификация по коду 2026-07-06 показала, что ВСЕ 5 пунктов уже удовлетворены** (реестр отставал):
  `run_sequential(&mut World)` (`apex-scheduler/src/executor.rs:98`), `Resources::get_raw_ptr` `pub(crate)`
  (`resources.rs:142`), `World::event_queue_ptr` `pub(crate)` (`world.rs:967`), `compute_archetype_indices`
  `pub(crate)` (`apex-scheduler/src/compile.rs:184`), `populate_type_names` `pub(crate)`
  (`apex-scheduler/src/registration.rs:978`). Долг закрыт: сырых `*mut World`-методов и pub raw-поверхности
  из safe нет.
- **S2 ✅ ЗАКРЫТ (2026-07-05) — `SystemContext::fetch::<P>()` safe-обход F3/ADR-002.** `ctx.fetch::<P>()`
  фетчил ЛЮБОЙ SystemParam без сверки декларации → недекларированный live-write. **→ `fetch_unchecked`
  + `#[doc(hidden)]`** (ADR-002-консистентно с `query_unchecked`/`resource_mut_unchecked`); единственный
  потребитель — пример `system_param.rs` — мигрирован. Полное устранение (params-as-args, `fetch` не
  нужен) — Р-1/волна 3.
- **S3 🟡 → CORE_POLISH волна 2.1 — `World::event_writer/event_reader(&self)` = гонка из safe-кода.**
  `world.rs:977/990` мутируют event-очереди через `&World`; `World: Sync` → два потока с `&World` в safe.
  **Реализуемо лишь при явном меж-поточном шеринге `&World`** (планировщик использует UnsafeWorldCell, не
  `&World`) → футган-класс. Чистый фикс = `&mut self`+`_unchecked` по канону ADR-002 → **CORE_POLISH
  волна 2.1** (rename-only был бы полумерой §0.2b; используется примером basic.rs И EventReader-SystemParam).
- **S4 🟡 → CORE_POLISH волна 2.2 — недекларированный `ctx.event_reader` мутирует реестр курсоров вне
  conflict-детекции.** `ctx.event_reader` (`world.rs:2531`) благословлён как «read», но
  `EventReader::new`→`add_reader` пишет в реестр курсоров (push/realloc, `events.rs:163-178`). Для
  ДЕКЛАРИРОВАННЫХ читателей гонку закрывает F2 (`SharedEventReaders`), но недекларированный ctx-путь
  планировщик не видит. Связан с F4.

---

## 🟡 Бездомные пункты реестра аудита (числились, но не сделаны/не отложены явно)

- **F4 ✅ ЗАКРЫТ для golden-path (2026-07-05, ветка api-golden-path).** `EventReader`-параметр (plain-fn,
  Р-1) держал `type State = ()` → каждый `get_param` звал `add_reader()` → свежий курсор с нуля →
  FixedUpdate-катчап читал дубли (rustdoc `Removed<T>` «без дублей/пропусков» — была ложь). **Фикс:**
  `State = EventReaderState<E>` (хранит `Option<EventCursor>`); `get_param` создаёт/переиспользует
  персистентный курсор через `SystemContext::event_reader_persistent`; `EventReader` получил
  `persistent`-флаг (Drop НЕ освобождает курсор — им владеет state). **§0.9-решение (не мимикрия):** НЕ
  переписывать `Events` на Bevy-лоссовую local-cursor модель (у Bevy события истекают за 2 кадра;
  `run_if`-gated читатель теряет их) — курсор в НАШЕЙ no-loss registry. Бонус: no-loss ТЕПЕРЬ активен
  для system-читателей (раньше transient-курсор терялся к `update()` → пропущенный читатель молча терял
  события). Цена no-loss — сериализация читателей одного события (`SharedEventReaders`) — оставлена
  осознанно. Гейты: workspace + clippy net-neutral + движок check + Miri TB (F4-тест + 23 event) +
  goldens byte-identical. Регресс-тест `persistent_event_reader_no_duplicate_reads`.
- **F4b 🟢 → CORE_POLISH волна 2.3 — AutoSystem/`system!`-путь чтения событий НЕ персистентен.**
  `ctx.event_reader::<E>()` внутри `AutoSystem::run` / `system!`-тела даёт свежий курсор (не через
  `SystemParam::State`) → тот же FixedUpdate-дубль для AutoSystem-читателей. AutoSystem — второй уровень
  (Р-1: plain-fn = golden path). Фикс потребовал бы хранить курсоры в AutoSystem-инстансе (macro-хирургия
  `system!`). Golden-path (plain-fn) закрыт F4; это — хвост. **Смежно: S3/S4** (footgun-гонки `&self`
  event-мутации) F4 НЕ закрывает (по-прежнему registry + `ctx.event_reader` для standalone).
- **D6-полное 🟡 → CORE_POLISH волна 1.2 — per-system `last_run` (Bevy-паритет change-окон).** Окно по-прежнему
  per-execution-stage (`stage_last_run: Vec<Tick>`, `apex-scheduler/src/lib.rs:~740-747, 2449-2454`);
  волна 2 закрыла только run_if-кейс. Значился в исходном scope волны 6б, исчез при формировании плана.
- **B5 ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 0.3) — утечка Entity-резерваций при drop/clear
  Commands.** Было: `Commands::drop`/`clear()` дропали payload, но непотреблённые `spawn().id()`-
  резервации (продвинули high-water / съели lease-слоты) не возвращались → утечка id-пространства
  (TD-40 починил только счётчик). **Сделано (честный возврат, §0.2b):** общий канал `AbandonQueue`
  (Arc, разделён аллокатором и всеми резерверами) с atomic-флагом `pending` — `EntityReserver::abandon`
  кладёт индексы (rare drop/clear-путь), `EntityAllocator::flush` дренит их в free-list ПОСЛЕ роста
  records, БЕЗ gen-инкремента (никогда не были живы — та же логика, что `reclaim_block_tail`).
  Флаг держит горячий fast-path flush lock-free. `Commands::abandon_queued_reservations` собирает
  `Command::Spawn`-резервации (не PLACEHOLDER) на drop/clear + `warn_once!` (нет World в scope →
  не ErrorHandler). Тесты: `abandoned_reservations_are_reclaimed_on_flush` (entity),
  `dropped_/cleared_commands_reservations_are_reclaimed` (commands). Гейты: 256 core-тестов, clippy
  net-neutral, Miri чист (19 reserv + 28 commands тестов, ноль UB/гонок).
- **B6 🟡 → CORE_POLISH волна 0.4 — standalone Commands (PLACEHOLDER) молча теряет chained insert/with_children.**
  `EntityCommands::insert` ставит `Insert{entity: PLACEHOLDER}` (`commands.rs:~809-813, 338`).
  Смягчено косвенно (spawn аллоцирует id на apply; insert-на-PLACEHOLDER попадает в A9-warn), но
  данные chained-insert теряются. Принятое решение §3 («паника в EntityCommands при PLACEHOLDER»)
  не исполнено и не пересмотрено письменно.
- **В4 🟡 — IsolatedWorld не дотянут до дифференциатора (осознанно ОТЛОЖЕН за scope CORE_POLISH —
  кандидат на следующую кампанию-дифференциатор).** Решение §10.6 = ДА (Entity-ремаппинг между
  мирами, bounded-каналы с телеметрией, кросс-поточные тесты). Сделано: только громкость дропа (E12,
  волна 4) + кросс-поточные тесты (волна 7, `apex-isolated/tests/cross_thread.rs`). Каналы моста
  unbounded, ремаппинга нет — эти два пункта бездомны намеренно: это отдельный дифференциатор (§0.9),
  не хвост полиша; берётся отдельной кампанией, не CORE_POLISH. Прямое требование §0.9 (козырь-прототип
  хуже отсутствия козыря).
- **§10.8 🟡 → CORE_POLISH волна 3 (фаза A) — Lua-путь не пересажен на общий DynQuery.** Решение (волна 6 п.4): скриптинг «обязан
  встать на общий, валидируемый ядром механизм». Факт: `apex-scripting/src` — ноль `DynQuery`;
  `iterators.rs` — прежний собственный unsafe-путь через `_meta` (`:~301, 426`; E1-валидация есть,
  но доступ свой). DynQuery (волна 6) сделан и назвал скриптинг консумером, но миграция не выполнена.

## 🟢 Бездомные — чистота/эргономика/док-честность

- **ErrorHandler world-less хвост 🟢 (API_GOLDEN_PATH волна 4, 2026-07-05).** Системный §0.2a
  `ErrorHandler` (per-World поле, `anomaly!`-макрос, режимы Warn/Panic/Silent/Custom + счётчики; ecs
  `754ed44`) охватил 9 сайтов с World в scope (world.rs ×6, commands.rs ×2, serializer restore ×1).
  **Остаются на `warn_once!`** (нет World в scope → политику не достать): `TemplateParams::set`
  (`template.rs`), `WorldBridge`/`CloneableBridge` кросс-world send-дропы (`apex-isolated`, смежно §В4),
  script-query незарег. компоненты (`apex-scripting/iterators.rs` ×4), `DynItem`/`DynItemMut`
  type-mismatch (`query.rs` ×3). Апгрейд потребует прокидывать `&ErrorHandler` в эти пути ИЛИ ввести
  процесс-глобальный фолбэк (частичная мимикрия Bevy-глобала — против нашей per-world модели §0.9).
  Осознанный фокусный охват, не полумера: golden-path мутаций покрыт; хвост — по спросу.

- **ParallelPolicy ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 0.2) — реализован `Fixed`-откат.**
  Был: тип-политика обещана планом PARALLELISM §3.1, не реализована (пороги — захардкоженные `const`),
  комментарий `lib.rs` обещал несуществующий `ParallelPolicy::Fixed` — ложь в коде (§0.2a).
  **Сделано (§0.2b — полный откат, не правка комментария):** `pub enum ParallelPolicy { CostModel, Fixed }`
  + `Scheduler::set_parallel_policy`/`parallel_policy`. `Fixed` — SEQ/PAR из entity-порогов, EMA не
  читается (детерминированный, измерение-независимый путь для патологии EMA); явный пол — жёсткий гейт
  под обеими политиками. Развилка вынесена в чистую `Scheduler::stage_prefers_seq`; юнит-тест
  `parallel_policy_fixed_vs_cost_model`; комментарий исправлен; addendum к ADR-003.
  Гейты: 101 scheduler-тест зелёный, clippy net-neutral.
- **guide-broken ✅ ЗАКРЫТ (2026-07-06, волна 5 API_GOLDEN_PATH).** Руководство
  `Apex_ECS_Руководство_пользователя.md` переписано под финальный API (ecs `5ab4096`..`5323d34`). Все
  классы §2.1 починены: (1) для_each-write→`for_each_mut`, ctx-write→`*_unchecked`; (2) §6.7+§16
  ctx.resource_mut/event_writer→advanced `*_unchecked`; (3) §6.8 Extract→`fetch_unchecked`; (4) §15
  `#[derive(Component)]`; (5) `query_mut_changed` задокументирован; (6) §17 Lua id = строка
  "index:generation"; (7) §11 `load_directory` 2 арг + `*.prefab.json`. STALE-пласт вычищен (версия
  0.1.0, ChunkConfig-модель, §10 v2/migrate/resources/MapEntities, write-путь запросов). Вычищены ВСЕ
  внутренние шифры; брендинг ApexForge_ECS. Гейт: grep снятых имён = только migration-заметки; сниппеты
  золотого пути компилируются. **Уточнение:** §2.1-класс F3.1 (`ctx.query::<…Write>`) оказался не
  «~10 битых» — `run_if_cond`/`conditions` НЕ сняты (снят тест-`SystemBuilder`), §6.0a был корректен.
- **string-table снапшота 🟢 → CORE_POLISH волна 0.6 — `type_name` per-instance.** `serializer.rs:142` пишет
  `info.name.to_string()` на каждый инстанс компонента (E7-формат v2 string-table не включил).
  Выигрыш — только в РАЗМЕРЕ сейва (редкий путь), отдельный focused-заход.
- **D9 ✅ ЗАКРЫТ (2026-07-06) — РЕШЕНИЕ: не фолд, а дифференциальный parity-гейт.** Прежняя рамка
  «свести `run_sequential` и `run_hybrid_parallel` в один путь под byte-identical goldens» — **оказалась
  НЕВЕРНОЙ целью** при разборе кода. Установленные факты: (1) движок использует ТОЛЬКО `run()`
  (→`run_hybrid_parallel`); `run_sequential` вызывается 0 раз в движке, ~97 раз в примерах/тестах ядра —
  это (а) **чистый sequential-базлайн перф-A/B** (perf.rs, parallel_diagnostics.rs; методология
  cost-model-кампании) и (б) простой reference-исполнитель для тестов; (2) расхождение per-stage тел
  (общий `Commands`-буфер vs per-system D8b-слоты + cost-model + ASD) — **осознанное, привязано к ЦЕЛИ
  пути**, не дрейф: единый буфер = честный «чистый seq» замер, per-system слоты = детерминизм под
  параллелизмом. **Форс-слияние испортило бы seq-базлайн (D8b/cost-model-оверхед в замере) и сменило бы
  id-семантику ради НУЛЯ выигрыша в корректности — регресс актива, не улучшение (§0.2b наоборот).**
  Настоящий риск D9 — не дублирование, а **тихий дрейф наблюдаемого поведения** между путями (сегодня
  ловился только тем тестом, что случайно взял нужный путь). **Правильное решение (§0.2a — громко):**
  оставить оба исполнителя (by design), добавить **дифференциальный parity-харнесс** — репрезентативные
  schedule'ы (spawn+move, run-condition gating, растущий мир, multi-stage ordering, форс-ASD-row-split
  N=512) гоняются через ОБА исполнителя, ассерт идентичности семантического состояния (счётчики,
  отсортированные значения компонентов, ресурсы; НЕ сырые id — стратегии аллокации разные легитимно).
  Любой будущий дрейф → ГРОМКИЙ фейл, без дорогого кросс-репо goldens-гейта. Файл:
  `apex-scheduler/tests/executor_parity.rs` (5 тестов, incl. форс-параллельный ASD-путь через
  `set_chunk_config`). Co-location из волны 7 (всё в `executor.rs`) сохранена. `run_stage_parallel` —
  по-прежнему ASD-под-хелпер hybrid'а (не top-level копия).
- **§1.4-хвосты 🟢 → CORE_POLISH волна 0.5 — гигиена, не сделана волной 4.** `EventCursor(pub u32)` — всё ещё pub
  (`events.rs:761`); `by_id` — FxHashMap, не Vec (`component.rs:337`); `MainWorld unsafe impl
  Send+Sync` — «проверить необходимость» не журналировано (`world.rs:~2112-2113`); `TargetIndex::remove`
  O(N)-скан без обещанного коммента-компромисса (`relations.rs:~372-388`).
- **DynQueryMut item-гейт 🟢 (S7/S8) → CORE_POLISH волна 3 (фаза A).** `DynItemMut::get_mut/get_mut_ptr` (`query.rs:~2711,2725`) не
  проверяют вхождение id в декларированные `writes` — декларация влияет только на матчинг архетипов,
  `AliasedWrite`-проверка при lending декоративна; для agent-IPC/скриптинга политику «что можно
  писать» придётся enforcить слоем выше (S7). Нет Changed/Added-термов у динамического билдера —
  инспектору для change-polling остаются полные сканы (S8). Гейтить в item либо явно задокументировать.

---

## Крупные отложенные развилки (с обоснованием §0.2b — дом здесь)

- **C6 ✅ ЗАКРЫТ (2026-07-05, commit ecs `8301699`) — консолидация query-зоопарка** (аудит-C6 ядра, НЕ
  движка). `Query`(inline-owned) + `CachedQuery`(Arc-cache) + view-часть `QueryState`(borrowed) →
  ЕДИНЫЙ `Query<'w, 's, D, F>` поверх ленивой per-archetype fetch-машины; источник индексов —
  приватный `StateSrc {Owned|Shared|Borrowed}`. **Реализация отклонилась от спайка:** view держит
  `&'w World` (НЕ `UnsafeWorldCell`) — write идёт через сырые указатели колонок, не `world_mut()`, cell
  не нужен (Miri TB чист; `&World`-модель уже доказана S1-part-2). Публичный `'s` разнесён (Bevy-паритет,
  форвард-фит В3), но элидированные fn-параметры поглощают его ⇒ **0 правок в движке** (не 3). Read/write
  split (S1) сохранён. Убраны `CachedQuery`/`CachedQueryIter`/`ArchIndices`; `SubWorld::world()`→`&'w`;
  `Single` в корневой реэкспорт; `Ref` deprecate+из prelude вон; удалён мёртвый SubWorld row-iteration
  кластер. Нетто −597 строк. Гейты: workspace 245 core / clippy net-neutral / движок all-targets /
  goldens 649/0/9 / Miri TB. **Остаток здесь же (НЕ закрыт C6):** В3 Phase B/C (реальный кэш-State
  через `Borrowed(&'s QueryState)` fast-path — инфраструктура `'s` готова) и leaf-sizing Ş2b.
  Детали — API §5 «Волна 1b/2».
- **Ş5 / Ş6 / В2 🟡 — перф-спайки кампании PARALLELISM (ROI-gated).** **Ş5** dense-by-default для
  infallible-запросов (vs-Legion итерация 0.59–0.68×; goldens-рискован — семантика change-стампинга
  per-deref vs range, байт-identity как жёсткий гейт). **Ş6** heavy_compute leaf-sizing (0.83× vs
  Bevy; pow-of-2-дисбаланс `par_split_run_ranges` на малых N; профайлер → num_threads-way split под
  A/B-гардом). **В2** packed/SoA storage (fragmented_iter/relations структурно; ROI-гейт =
  many_foxes-доказательство, НЕ микробенч — ставит под удар структурные победы add_remove/despawn/get).
  Детали: ADR-003, архив PARALLELISM §4.5/4.6/§8.В2.
- **D8b-overflow 🟢 → CORE_POLISH волна 1.1 — детерминированный overflow блока (фронтир).** При overflow блока в кадре
  система падает в недетерминированный путь тот кадр, затем блок растёт. Rank-ordered
  детерминированный overflow — по спросу (после прогрева не наступает). Детали: ADR-001, entity.rs:183-185.
- **Волна 7 🔶 ЧАСТИЧНО (2026-07-06) — EN-миграция ✅ + декомпозиция ✅ + тест-кампании ⏳.**
  **EN-миграция ✅ ЗАКРЫТА ЦЕЛИКОМ:** 90 `.rs`-файлов / 5513 вхождений кириллицы (комментарии/rustdoc/
  строковые литералы) → **ноль кириллицы в `*.rs` всего apex-ecs** (grep-подтверждено; правило движка
  §10.2). Все крейты + 14 примеров зелёные, clippy net-neutral, примеры запускаются без паник.
  **Декомпозиция `scheduler/lib.rs` ✅:** 6856→1090 строк — вынос `mod tests`→`tests.rs` + сплит
  `impl Scheduler`→`registration/compile/executor/debug.rs` (child-модули видят приватные поля родителя;
  16 методов→`pub(crate)` для кросс-модульного/тестового доступа). **D9-фолд ОТЛОЖЕН** (см. запись D9
  выше — co-location сделана, фолд = поведенческий рефактор под goldens-гейтом).
  **✅ ТЕСТ-КАМПАНИИ ЗАКРЫТЫ (2026-07-06)** — 7 коммитов, +8 тест-таргетов, весь workspace зелёный
  (40 `test result: ok`), clippy `--all-targets` net-neutral (core/scheduler/isolated 0):
  (1) **макро trybuild** — 10 compile-fail фикстур на НАШИ `compile_error!`/`syn::Error` (derive
  Bundle enum/union; Scriptable enum-with-data/union/пустой tuple-struct; `system!` `&T`/`&mut T`-resource,
  two-query, world+params, unsupported-param) — `apex-core/tests/trybuild.rs` + `apex-scripting/tests/trybuild.rs`;
  (2) **serialization** — полная эквивалентность round-trip (компоненты+relations+ресурс, JSON+bincode)
  + детерминированный fuzz (64 мира) + 4 typed error-path — `apex-serialization/tests/roundtrip.rs`;
  (3) **isolated** — 6 настоящих кросс-поточных (мост миров через `std::thread`, no-loss под контенцией,
  IsolatedWorld на воркере) — `apex-isolated/tests/cross_thread.rs`;
  (4) хвост: hot-reload (temp-фильтр inline + реальный OS-watch E2E), scripting E2E (query/commit/spawn/
  despawn/resource/event/error/set_active, 8 тестов), events (multi-frame gated no-loss + concurrent
  send_sync), par-пути core (par_for_each[_mut]/_chunk[_mut] полнота+ровно-один-раз+эквивалентность seq).
  **D9 ✅ ЗАКРЫТ** (2026-07-06) — дифференциальный parity-гейт `run_sequential`↔`run` вместо
  рискованного фолда (см. запись D9). **Волна 7 → ✅ ЗАВЕРШЕНА ПОЛНОСТЬЮ** (EN + декомпозиция +
  тест-кампании + D9); открытого долга волны 7 не осталось. Дом волны — CORE_AUDIT §9.
