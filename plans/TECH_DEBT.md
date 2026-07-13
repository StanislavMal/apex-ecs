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

- **MIRI-CD ✅ ЗАКРЫТ (обнаружено + исправлено 2026-07-06, CORE_POLISH; был PRE-EXISTING, НЕ регрессия
  1.2) — Miri UB в change-detection query-пути под планировщиком.** Было: `SubWorld::from_raw(world:
  &World)` сохранял сырой указатель, НАСЛЕДУЯ borrow-tag этой `&World`; исполнитель затем мутировал тик
  World через sibling `&mut World` (`world.rs:338` `tick()` — foreign write), что дизаблило tag →
  последующий `SubWorld::world()` = **UB под Stacked И Tree Borrows**. Ловилось
  `unified_system::changed_in_system_detects_only_mutated`; подтверждено pre-existing на b48ffd0. Не
  замечено раньше: Miri гоняли на apex-**core** lib, а путь — в apex-**scheduler** integration.
  **Фикс:** `from_raw`/`from_raw_with_ranges` берут сырой `*const World` (не `&World`) — указатель без
  borrow-tag; исполнители строят SubWorld из ТОГО ЖЕ raw-ptr (`world_ptr`/`const_ptr`), из которого
  берут транзиентные `&mut *ptr`-записи, поэтому записи (дети raw-tag) его не дизаблят. `run_sequential`
  переписан с долгоживущего `&mut *world_ptr` на raw-ptr + транзиентные reborrow'ы (долгоживущий `&mut`
  делал каждое SubWorld-чтение foreign-доступом). **Гейты:** Miri SB+TB чисты (unified_system 5/0,
  co_stage 0-UB, d8b параллельный 5/0); scheduler+core 12 ok, workspace 42 ok, clippy net-neutral;
  **движок собран + goldens 649/0/9 БАЙТ-ИДЕНТИЧНЫ** (фикс семантически прозрачен). Scheduler-CD-путь
  теперь в Miri-политике. **§0.2a — громкий баг найден и закрыт.**

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
- **S3 ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 2.1) — `World::event_writer/event_reader(&self)` = гонка
  из safe-кода.** Было: `world.rs:977/990` мутировали event-очереди (продвижение read-курсора / push)
  через `&World`; `World: Sync` → safe-код мог шарить `&World` между потоками и звать метод на двух
  потоках сразу (оба `&mut *ptr` на одну очередь) = data race из safe. **Сделано (канон ADR-002, как
  S1/S2/F3):** благословенные `event_reader`/`event_writer` → `&mut self` (эксклюзив делает гонку
  невыразимой); добавлены `event_reader_unchecked`/`event_writer_unchecked(&self)` + `#[doc(hidden)]` —
  escape-hatch для путей с законным `&World`. Единственный такой потребитель — `Extract<Listen<E>>::fetch`
  (`system_param.rs`, MainWorld через `Res`) — мигрирован на `event_reader_unchecked` (sound:
  последовательная extract-стадия, декларированный `Listen<E>`, нет конкурентного доступа). Потребители
  `&mut World` (тесты, пример basic.rs) не тронуты (auto-`&mut`). **SubWorld::event_reader/event_writer**
  (`sub_world.rs`) оставлены `&self` (dead-code + недостижимы из safe: `SubWorld` строится лишь через
  `unsafe fn from_raw`, чей контракт гарантирует не-алиасинг по декларированному доступу) — инвариант
  soundness ЗАЖУРНАЛИРОВАН в doc-комментарии (зеркалит `SubWorld::resource_mut`). Rename-only без
  `&mut`-пути был бы полумерой §0.2b. **Тест:** trybuild compile-fail `event_mut_needs_exclusive_world`
  (E0596 на `&World`-мутацию reader И writer). Гейты: 260 core + весь workspace (102 scheduler, все
  integration) зелёные; clippy net-neutral (4 warn — pre-existing в serialization/bench); Miri TB чист
  (event 22, persistent/Extract, events_lag_threads 3 — multi-thread, ноль UB/гонок).
- **S4 ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 2.2) — недекларированный `ctx.event_reader` мутировал
  реестр курсоров вне conflict-детекции.** Было: `SystemContext::event_reader` благословлён как «read»,
  но `EventReader::new`→`add_reader` пишет в реестр курсоров (push/realloc) — недекларированный ctx-путь
  планировщик не видел (асимметрия: `event_writer`/`resource_mut` уже были `_unchecked` по F3.2, только
  reader отставал). **Сделано (симметрично S2/F3.2, ADR-002):** `SystemContext::event_reader` →
  `event_reader_unchecked` + `#[doc(hidden)]`; благословенный путь чтения событий из системы — ТОЛЬКО
  параметр `EventReader<E>` (персистентный курсор F4, декларация видна планировщику) или `Listen<E>`.
  Внутренние декларированные потребители переведены на `_unchecked`: `Listen<E>::fetch`,
  `EventReader<E>::fetch` (stateless fallback), `system!`-макро (4 сайта event-reader — теперь
  симметричны уже-`_unchecked` event-writer/resource-mut снипетам). Движок: apex-input AutoSystem'ы
  (4 сайта) → `event_reader_unchecked` (декларируют `type Events = Listen<…>` → validated; тот же
  принятый ADR-002-остаток, что оставил apex-input на AutoSystem+`_unchecked` для writer/resource —
  reader был последним не-`_unchecked` держателем). Руководство §5.2.1/§6.7 + таблицы World/ctx
  обновлены. Гейты: apex-core + весь workspace (scheduler 7 ok, scripting 8 E2E) зелёные; clippy
  net-neutral; Miri TB чист (event/persistent); движок `check --all-targets` ✅. Связан с F4b (волна 2.3
  — макро-путь всё ещё транзиентный курсор).

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
- **F4b ✅ ЗАКРЫТ ПОЛНОСТЬЮ (2026-07-06, CORE_POLISH волна 2.3) — `system!`/AutoSystem event-reader
  теперь ПЕРСИСТЕНТЕН (per-system курсор), паритет с plain-fn (F4).** Было: `system!` генерит
  `AutoSystem` (отдельный трейт, не `SystemParamFunction`), который НЕ прокидывал `SystemParam::State`
  → свежий курсор каждый ран → FixedUpdate-catchup читал дубли. Промежуточная переоценка (тот же день)
  сочла полное закрытие несоразмерным, но пользователь потребовал золотой путь → сделано ПРАВИЛЬНОЕ
  архитектурное решение. **Ключевой инсайт:** курсор не обязан жить в пользовательской структуре (оба
  прежних блокера были про неё) — он живёт в **раннере системы** (адаптер AutoSystem), который и так
  переживает кадры, ровно как plain-fn держит `SystemParam::State` в замыкании. **Сделано (паттерн
  существующего `deferred_cmds`):** новый `EventCursors` (per-system store, TypeId-keyed) во владении
  адаптера; `SystemContext.event_cursors: Option<*mut EventCursors>` (зеркало `deferred_cmds`, тот же
  single-task soundness-инвариант) + `with_event_cursors` (unsafe builder) + благословенный
  `event_reader_persistent_auto` (берёт слот из store, персистентно; фолбэк на transient без store);
  оба адаптера (`AutoSystemAdapter` lib.rs + `Adapter` config.rs) владеют `EventCursors` и ставят ptr
  через `addr_of_mut!` перед `inner.run`; макро-снипеты event-reader → `event_reader_persistent_auto`
  (0 ripple по ~35 impl AutoSystem — курсор НЕ в структуре). **Инвариант single-task:** event-читающая
  система форсится `stateful` (`!reads_event.is_empty()` в обоих registration-путях) → нет ASD row-split
  → ровно один таск формирует `&mut *ptr` (иначе shared-курсор через чанки = дубли per-chunk — это
  ЧИНИТ и латентный баг query+event row-split). **Тесты:** `macro_event_reader.rs` — per-frame (батч
  ровно раз) + **catchup (3 рана в кадре без flush → батч ровно раз, НЕ per-run)** = доказательство
  персистентности. Гейты: workspace зелёный, clippy net-neutral, **Miri TB чист на raw-ptr пути**
  (macro_event_reader + unified_system parallel), движок build + **goldens 649/0/9 байт-идентичны**
  (форс-stateful для event-читателей рендер не сдвинул). **Смежно: S3/S4 закрыты (волна 2.1/2.2).**
- **D6-полное ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 1.2) — per-system `last_run` (Bevy-паритет
  change-окон).** Было: окно change-detection per-execution-stage (`stage_last_run`); корректно только
  для solo-gated систем. Дыра: run_if-gated система, ДЕЛЯЩАЯ стадию с ungated-системой (та держит
  стадию живой, окно per-stage тикает), при возобновлении теряла `Changed<T>`, сделанные пока она
  пропускала кадры. **Сделано (Bevy `SystemMeta::last_run`):** `SystemContext.last_run` per-system
  (`with_last_run`/`last_run()`); `ctx.query`/`query_unchecked`/`Query`-SystemParam берут per-system
  baseline; `Scheduler::system_last_run` map + `system_window`/`advance_system_windows` (обновление
  ТОЛЬКО после фактического рана, не при skip); заврайрено во ВСЕХ 3 исполнителях (run_sequential,
  run_hybrid_parallel fallback+parallel, run_stage_parallel inline+rayon-task через `AsdTask.last_run`).
  Аддитивно: дефолт = world-тик → для every-frame систем поведение байт-идентично (goldens не сдвигаются).
  Тест `co_stage_gated_reader_sees_changed_from_pause` (при per-stage окне seen=0, при per-system seen=1).
  Гейты: 102 scheduler-lib + parity/determinism зелёные, clippy net-neutral.
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
- **B6 ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 0.4) — standalone Commands (PLACEHOLDER) молча терял
  chained insert/with_children.** Было: цепочка `standalone.spawn(x).insert(y)` (нет резервера → id =
  PLACEHOLDER) молча теряла `y` (Insert на PLACEHOLDER). **Сделано (§0.2a — громко, не молча):**
  `EntityCommands::assert_bound(op)` (`#[track_caller]`) — паника с actionable-сообщением на ВСЕХ
  цепочечных методах, использующих отложенный id (insert/remove/add_relation/set_parent/add_child/
  add_children/remove_parent/clear_children/with_children/despawn). `cmd.entity(real)` не затронут
  (реальный id); `cmd.spawn((full,bundle))` без цепочки работает standalone (компоненты в самой
  Spawn-команде); `ChildSpawner` защищён транзитивно (`with_children` уже проверил родителя →
  резервер есть → дети реальны). Тесты: `standalone_spawn_chained_insert_panics`/`_with_children_panics`
  (should_panic), `world_attached_spawn_chained_insert_works`, `standalone_spawn_full_bundle_still_works`.
  Гейты: 260 core-тестов, clippy net-neutral.
- **В4 ✅ ЗАКРЫТ (2026-07-07, EDITOR_GOLDEN_PATH волна 3) — IsolatedWorld дотянут до дифференциатора.**
  Решение §10.6 выполнено полностью: (1) **soundness** — ложный `unsafe impl Sync` удалён (E12-остаток;
  ни один потребитель не алиасит `&IsolatedWorld` — рендер передаёт владение; `Send` с исправленным
  SAFETY); (2) **bounded-каналы + телеметрия** — `BridgeConfig{capacity, overflow}` +
  `BridgeOverflow::{Block, DropLoud}` вместо безусловного unbounded; `BridgeStats` (sent/dropped/
  high_water); дроп/сериализ-фейл считаются и громко (§0.2a); (3) **протокол обмена + Entity-ремаппинг** —
  модуль `exchange` (`export_world`/`export_entities` субтри по ChildOf / `import` fresh+remap E6 /
  `apply_back` merge-by-key / `transfer_entities`) ПОВЕРХ snapshot-машинерии (не вторая реализация;
  `WorldSerializer::restore_merge_with` — apply-back примитив); (4) **schema-рецепт** — `WorldRegistrar`
  (component-serde/event/relation-kind, replay на любой мир) убирает ручную дупликацию регистраций
  (рендер ×35, editor Play). Кросс-поточные тесты новых способностей (transfer remaps relations,
  shared-schema round-trip, apply-back onto original); Miri чист на мосту (атомики+Send, single+threaded).
  Потребитель apply-back — preview-транзакции агента (EDITOR_GOLDEN_PATH волна 5.2, движок).
- **§10.8 ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 3 фаза A) — Lua-путь пересажен на общий DynQuery.**
  Решение (волна 6 п.4): скриптинг «обязан встать на общий, валидируемый ядром механизм». **Сделано:**
  `apex-scripting` собственный unsafe-путь через `_meta`/`columns_raw` УДАЛЁН (`build_arch_states`/`ArchState`/
  `ComponentState` вырезаны). `query()` снимает матч-энтити вперёд через read-`DynQuery`
  (`collect_matching_entities`); чтение per-`next()` — `DynItem::get_ptr(id)`; запись в `commit` —
  `DynQueryMut::get_mut(entity)` (E10 через allocator-lookup) + `DynItemMut::get_mut_ptr(id)` (S7-гейт +
  change-tick стампится ядром). `binding.read/write` fn-указатели сохранены — сменился лишь ИСТОЧНИК ptr.
  `&mut World` в commit — через `ScriptContext::world_ptr_mut` (raw-ptr escape-hatch модуля; gather-фаза
  освобождает ctx/Lua-займы до материализации `&mut World`; commit идёт между Lua-итерациями). `query_cache`
  УДАЛЁН (§0.2b-переоценка: новый путь всё равно строит per-entity снимок; кэшировать+клонировать
  `Vec<Entity>` не дешевле пересборки — архетипов единицы; второй источник истины устранён). Descriptor-парсер
  расширен: `Changed:X`/`Added:X` → `changed_id`/`added_id` (S8-фильтры, симметрично `With:`; значение — через
  отдельный `Read:X`). Гейты: apex-scripting 6 unit + 9 E2E (incl. flagship commit-persist + forgery-reject
  stale-entity + Changed-фильтр применяется) + весь workspace зелёный, clippy `--all-targets` net-neutral;
  Miri неприменим (mlua FFI) — покрыт E2E. **Трейд-офф (документирован в коде):** write-set в commit берётся из
  `_meta.writes` (script-authored) → S7-гейт тавтологичен в фазе A; scheduler-visible enforcement — фаза B
  (скрипт-системы декларируют доступ). Память безопасна независимо (id-резолв + allocator-lookup ⇒ ни OOB, ни
  type-confusion). Коммиты S7/S8 — [ниже]; миграция — этот коммит.
  **Обновление (фаза B ✅, 2026-07-07, `decisions/ADR-005`):** трейд-офф закрыт — Lua `system{}`-системы
  декларируют доступ планировщику, S7-гейт стал СОДЕРЖАТЕЛЬНЫМ (enforcement по декларированным ids). Приём
  `world_ptr_mut` (raw `&mut World` из `&self`) УДАЛЁН и заменён на звучный declared-cell путь
  (`QueryBuilderMut::from_declared_cell(&World)`, запись только в interior-mutable колонки, `&mut World` не
  формируется) — B3a переоценка §0.2b (иначе UB под Tree Borrows в конкурентной стадии).

## 🟢 Бездомные — чистота/эргономика/док-честность

- **DIFF-REORDER 🟢 (ADR-008, 2026-07-13) — дифф снапшотов слеп к чистой перестановке детей.**
  `WorldSnapshotDiff` сравнивает relations как МНОЖЕСТВО рёбер `(subject, kind, target)`: sibling-
  перестановка (ADR-008) не меняет набор рёбер ⇒ пустой relation-дифф, `apply_diff` оставляет старый
  порядок. Полные снапшоты round-trip'ят порядок корректно (target-major эмит); слеп только
  дифф/patch-путь. Loud-нота в `serializer.rs` (у HashSet-сравнения). Закрытие = вынести порядок в
  дифф (per-target список позиций или ordered-рёбра) — брать при первом реальном потребителе
  диффов иерархий (сейчас дифф используется для компонент-датой, не для порядка).

- **ErrorHandler world-less хвост 🟢 (API_GOLDEN_PATH волна 4, 2026-07-05).** Системный §0.2a
  `ErrorHandler` (per-World поле, `anomaly!`-макрос, режимы Warn/Panic/Silent/Custom + счётчики; ecs
  `754ed44`) охватил 9 сайтов с World в scope (world.rs ×6, commands.rs ×2, serializer restore ×1).
  **Остаются на `warn_once!`** (нет World в scope → политику не достать): `TemplateParams::set`
  (`template.rs`), `WorldBridge`/`CloneableBridge` кросс-world send-дропы (`apex-isolated`, смежно §В4),
  script-query незарег. компоненты (`apex-scripting/iterators.rs` ×4), `DynItem`/`DynItemMut`
  type-mismatch (`query.rs` ×3). Апгрейд потребует прокидывать `&ErrorHandler` в эти пути ИЛИ ввести
  процесс-глобальный фолбэк (частичная мимикрия Bevy-глобала — против нашей per-world модели §0.9).
  Осознанный фокусный охват, не полумера: golden-path мутаций покрыт; хвост — по спросу.
  **Обновление (2026-07-06, CORE_POLISH волна 3 фаза A):** script-query незарег.-компоненты
  (`apex-scripting/iterators.rs` ×4 `warn_once!`) переведены на `anomaly!` — миграция на DynQuery дала World
  в scope (`collect_matching_entities`/`build_entity_table`). Остаются на `warn_once!`: `TemplateParams::set`,
  `WorldBridge`/`CloneableBridge` (apex-isolated), `DynItem`/`DynItemMut` type-mismatch (`query.rs` ×3).

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
- **string-table снапшота ✅ ЗАКРЫТ (2026-07-07, EDITOR_GOLDEN_PATH волны 1–2) — snapshot wire format v3.**
  Было: `serializer.rs` писал `info.name.to_string()` на каждый инстанс (полный Rust-путь ~30-50 байт ×
  инстансы). **Волна 1 (предзадача):** свёл две рассинхронизированные версионные схемы к ОДНОЙ — удалил
  вестигиальный `SnapshotVersion{major,minor}` + `is_version_compatible` (0 живых потребителей); u32
  `version` + миграционная цепочка = единственный источник; `restore_with` мигрирует централизованно
  (старую версию — клоном, будущую — reject), редакторский `from_json`+`restore_with` путь больше не
  минует миграцию; `WorldDiff::version` унифицирован с snapshot-версией и проверяется в
  `apply_diff_to_snapshot`; префабы получили версию + migrate-on-load. **Волна 2:** приватный модуль
  `wire` — `WireSnapshotV3` со `string_table: Vec<String>` + `name_idx: u32`; in-memory `WorldSnapshot`
  не тронут (интернирование чисто wire); `to/from_json`/`bincode` конвертят на границе + version-peek
  дисптчит v3/legacy; `CURRENT_VERSION` 2→3 (миграция v2→3 = no-op). Тесты: имя интернируется 1×,
  v2-фикстуры (JSON+bincode) грузятся, corrupt-index reject, fuzz через v3. Движок: editor-host тест
  адаптирован к интернированному on-disk JSON (name_idx через string_table).
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
- **§1.4-хвосты ✅ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 0.5) — гигиена.** Все 4:
  (1) `EventCursor(pub u32)` → поле `pub(crate)` (внешних потребителей нет, `events.rs:770`);
  (2) `by_id` `FxHashMap<u32,_>` → `Vec<ComponentInfo>` (ids плотные с 0: `register` — единственный
  аллокатор, `push` + инкремент, никогда не удаляются; O(1) индекс, без хеша, `debug_assert` плотности;
  `iter()` теперь в id-порядке = детерминированнее); (3) `MainWorld unsafe Send+Sync` — journaled
  инвариант (нужен: resource-хранилище Send+Sync-bounded → без impl extract не компилируется; sound:
  MainWorld живёт только в sequential extract-стадии, concurrent-доступа нет; load-bearing —
  extract держать последовательным); (4) `TargetIndex::remove` O(N)-скан — коммент-компромисс
  (O(N) в fan-in target'а; малый для типичных relations; SmallVec inline бьёт вторичный индекс;
  деградация только на патологическом huge-fan-in). Гейты: 260 core-тестов, clippy net-neutral.
- **DynQueryMut item-гейт: S7 ✅ + S8 ✅ ЗАКРЫТЫ (2026-07-06, CORE_POLISH волна 3 фаза A).**
  **S7 ✅:** `DynItemMut::get_mut/get_mut_ptr` теперь ГЕЙТЯТ по декларированным `writes` — id не в
  `writes` → `None` + `anomaly!` (`refuse_undeclared_write`, Severity::Warn, World в scope через новое
  поле `DynItemMut.world`). Декларация write больше не декоративна: скриптинг/agent-IPC/редактор поверх
  DynQueryMut не могут писать недекларированный компонент (read-declared компонент даёт только shared
  read). `DynItemMut` получил поля `writes: &[ComponentId]` + `world: &World`; оба конструктора
  (`for_each_mut`/`get_mut`) прокидывают их. Тест `dyn_write_gate_refuses_undeclared_write` (write:Hp +
  read:Mana → get_mut::<Mana>/get_mut_ptr(mana) = None, Mana не тронута; get::<Mana> работает). Гейты:
  260 core + workspace зелёные, clippy net-neutral, Miri TB чист (dyn_write 9). Нулевой blast radius
  (движок DynItemMut не потребляет). **S8 ✅:** динамический билдер получил Changed/Added-термы —
  `changed[_id/_name]::<T>()` / `added[_id/_name]::<T>()` + `since(last_run)`-baseline (дефолт
  `world.last_run_tick()`) на ОБОИХ билдерах (Query/QueryBuilderMut). Реализованы как per-row
  post-фильтр (`DynTerms::row_passes_change` через `col.get_tick`/`get_added_tick` +
  `is_newer_than`), симметрично relation-фильтру; терм добавляет id в `withs` (компонент обязан
  присутствовать). Заврайрено в `DynIter::next`/`DynQuery::get`/`count`/`is_empty` +
  `DynQueryMut::for_each_mut`/`get_mut`/`count`. Инспектору редактора теперь доступен change-polling
  без полных сканов; скриптингу — реактивные запросы. Тесты `dyn_changed_filter_matches_typed`
  (== типизированный `Changed<T>`) + `dyn_added_filter` (added-tick, не мутация). Гейты: 263 core +
  workspace зелёные, clippy net-neutral, Miri TB чист (dyn 21).

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
- **D8b-overflow ✅ БОЛЬШЕЙ ЧАСТЬЮ ЗАКРЫТ (2026-07-06, CORE_POLISH волна 1.1) — эскроу-margin +
  громкий фолбэк.** Было: overflow блока в кадре/спайке → тихий недетерминированный путь тот кадр.
  **Сделано:** сеемый блок = `block_size_for` (peak×2) + эскроу (половина блока, `seed_size_for`) —
  rank-детерминированный приватный хвост; спайк в пределах эскроу (~до 3× peak) остаётся
  детерминированным. За пределами блок+эскроу — фолбэк как прежде, но ГРОМКИЙ (§0.2a):
  `BlockCursor.overflowed` → `warn_once!` + счётчик `deterministic_overflow_count`. Эскроу-хвост
  реклеймится (id-пространство ограничено). Тесты `escrow_keeps_spike_within_margin_deterministic`/
  `overflow_beyond_escrow_is_loud_and_still_correct`; Miri чист (20 entity-тестов); addendum ADR-001;
  руководство §6.6a. **Остаток (принято §0.2b):** спайк за пределами эскроу недетерминирован тот кадр
  (громкий) — бесконечный детерминированный overflow не окупается.
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
