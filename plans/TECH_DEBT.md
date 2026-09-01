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

## 🟡 §0.2a-громкость — снапшот молчит о том, что выбросил

- **SNAP-REL 🟡 — `snapshot_with_filter` МОЛЧА выбрасывает связь, чей subject или target не прошёл
  фильтр** (`crates/apex-serialization/src/serializer.rs`, ветка
  `if !kept.contains(&subject_index) || !kept.contains(&target_index) { continue; }`).
  Заведено 2026-09-01 движком: там это стоило 🔴-дефекта, открытого два месяца (apex-engine TD-123,
  ADR-076). Комментарий в коде честно называет причину — «a relation to a filtered-out entity can't
  restore, so don't save it (avoids restore warns)» — и в этом же и дефект: **избавление от варнинга
  на восстановлении куплено тишиной на сохранении.**

  Что это стоило потребителю: редактор фильтрует из снапшота под-узлы развёрнутой модели (их
  регенерирует ссылка на файл). Авторский объект, привязанный к КОСТИ модели, терял связь на
  сохранении — беззвучно — и возвращался в корень сцены с трансформом, значившим что-то только в
  кадре кости. Все двери отвечали `Ok`; неправильность появлялась через один цикл save/load.

  **Чего НЕ надо делать:** сохранять такие связи (восстановить их действительно некому — target
  отфильтрован намеренно). Фильтр прав; неправа тишина.

  **План (§0.2a):** снапшот обязан СЧИТАТЬ выброшенное и отдавать это в `WorldSnapshot` —
  сколько связей и какого вида (`kind_name`) не поехало. Тогда потребитель может решить сам:
  редактору это нужно, чтобы сказать «эта связь не сохранится» ДО сохранения, а не после загрузки.
  Число, а не лог: ядро не знает, чей это дефект.

  **Статус движка:** apex-engine больше от этого не зависит (ADR-076 хранит место внутри модели
  адресом, а не связью), но следующий потребитель наступит на то же место.

---

## 🔴 Soundness — недоделки borrow-модели (верификация 2026-07-04)

- **MIRI-ER ✅ ЗАКРЫТ (2026-07-16, заход «running is reading», ADR-010) — Miri UB в
  `EventReader::read` устранён.** Было: `ptr: events as *const Events<T>` (указатель, унаследовавший
  shared-borrow-tag от `&mut`) + мутация через `as_mut` в `read()` ⇒ UB под Stacked/Tree Borrows
  (класс MIRI-CD). Фикс: `EventReader.ptr` = `*mut Events<T>` ПРЯМЫМ кастом из `&mut` (провенанс
  записи сохранён); `iter`/`read`/`Drop` через него. Закрыт совместно с семантическим редизайном
  ридеров (**ADR-010 «запуск = прочтение»**: drop-advance персистентного курсора — ран-но-не-читавшая
  система не может удержать retention и вызвать пере-доставку; `Listen::fetch` →
  `event_reader_persistent_auto`; starvation-warn ≥64 тиков, `Events::retained_ticks()`).
  Триггер: движковый баг «клавиша A при фокусе списка спамит каждый кадр» (ui_virtual_list, найден
  юзером). Верификация: Miri чист на репро (`removed_events_emitted`) + событийных тестах (5/5);
  ядро 44 тест-группы + clippy 0; движок: apex-input на `_auto`-курсорах, регресс-тест спама зелен
  БЕЗ per-site дренажа (откачен), goldens 720/0 байт-идентичны.

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

## Блок движкового аудита PERF_ECONOMY (ключи PE-C*) — ✅ ЗАКРЫТ ЦЕЛИКОМ

> Происхождение: четырёхсторонний код-аудит движка 2026-08-05 (кампания PERF_ECONOMY,
> `apex-engine/plans/archive/PERF_ECONOMY.md` §1) вскрыл ядровой пласт «покадровая цена не
> пропорциональна изменённому». **Закрыт кампанией CORE_ECONOMY 2026-08-06 одним днём,
> волны 0–4** (план → `plans/archive/CORE_ECONOMY.md`; решения → ADR-011/ADR-012).

- **PE-C1 ✅ ЗАКРЫТ (2026-08-06, волна 2)** — changed-фаза `propagate_transforms` скипает
  архетипы по агрегату колонки (ADR-011). `propagate_static` (10k узлов, ничего не движется):
  2.96 µs → **112.5 нс (×26)**; горячий кейс без изменений.

- **PE-C2 ✅ ЗАКРЫТ (2026-08-06, волна 1, ADR-011)** — поколонные `max_change_tick`/
  `max_added_tick` (AtomicU32 relaxed, только raise) + `WorldQuery::skip_archetype`
  (Changed/Added/кортежи/Or) + дин-путь. `changed_iter_static` 2.84 µs → **15.6 нс (×182)**,
  frag ×7.5; write-путь ≤2 % (полоса шума). Miri чист; goldens движка байт-идентичны.
  Движковый TD-207 (потребительская сторона) переоценить: ядро теперь скипает само.

- **PE-C3 ✅ ЗАКРЫТ (2026-08-06, волна 4)** — `any_with_component` = `!is_empty()`
  (стоп на первом совпадении) вместо `count()`.

- **PE-C4 ✅ ЗАКРЫТ (2026-08-06, волна 4)** — слоты `stage_cmds` переиспользуются
  (`Commands::reset_for_reuse`: capacity очереди/арены живёт, резервер отвязывается — D8b
  без утечки id-блоков). ⚠ Расширение на capacity очереди ВНУТРИ `apply()` (`drain`)
  измеренно стоило +5–8 % на insert-тяжёлых кадрах и откатано (комментарий в коде).

- **PE-C5 ✅ ЗАКРЫТ (2026-08-06, волна 4)** — identity-вектора `0..n` заменены кэшем
  `scratch_arch_indices` (растёт монотонно; 4 сайта executor.rs).

- **PE-C6 ✅ ЗАКРЫТ (2026-08-06, волна 3, ADR-012)** — `World::resource_mut` → ленивый
  `ResMut` (взятие ≠ изменение; Bevy-паритет), `ResMut::set_if_neq`/`into_inner`;
  `FixedTime`/`NextState`/`StateTransitions` не «изменены» на холостом кадре. Тип-миграция
  обоих репо (движок: свип 109 файлов, all-features-гейт).

- **PE-C7 ✅ ЗАКРЫТ (2026-08-06, волна 4)** — rustdoc интервала клампа переписан формулой:
  тик двигается ПО СТАДИЯМ (+граница кадра) ⇒ `≈ 2²⁶/(fps·(stages+1))` ≈ 39 ч @60 FPS/7
  стадий; горизонт wrap ≈ 52 дня (не «3 дня»/«99 дней @250Hz»).

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
- **BUNDLE-RESOLVE-0830 ✅ ЗАКРЫТ ТЕМ ЖЕ ЗАХОДОМ (2026-08-30) — композиция бандла выводилась НА
  КАЖДЫЙ спавн, и цена росла с ШИРИНОЙ бандла.** Найдено разложением ячейки `relations` (0.73×):
  её половины — spawn, link, walk — поодиночке на паритете с bevy, а целое нет; лестница по ширине
  бандла назвала виновника — **три лишних компонента стоили apex 5313 мкс против 1252 у bevy
  (4.2×)** на 100k одиночных спавнов, `spawn 4 компонента` 0.56×. Причина в исходнике: `spawn_at`
  на каждый вызов делал по хэш-поиску `TypeId`→`ComponentId` на компонент в `static_component_ids`,
  сортировку, хэш отсортированного списка id в `get_or_create_archetype`, а затем внутри
  `write_into` ещё по поиску на компонент плюс скан `column_index` — **девять хэш-поисков на спавн
  четырёхкомпонентного бандла** ради факта, принадлежащего ТИПУ.
  **Фикс — эталонной формы** (`Bundles`/`BundleInfo` у bevy, `bundle/info.rs:373,428`):
  пер-мировой `BundleCache`, ключ `TypeId::of::<B>()`, значение — порядок объявления, ключ
  архетипа, сам архетип и индексы колонок; `Bundle: Sized + 'static` (у эталона та же сигнатура).
  Одиночный спавн пишет через `write_into_batch` с готовыми индексами. Стало: `spawn 4 компонента`
  11021 → **7166 мкс (0.96× на рунге 100k)**, а главное — ЦЕНА ШИРИНЫ (разность внутри одного
  прогона, поэтому устойчивая) упала 5313 → **1972** при bevy 2016: паритет. Абсолют зависит от
  рунга: новая ячейка компаратора на 10k записала apex 788.0 против bevy 598.2 (**0.76×**). Остаток
  — уже не ширина, а пер-спавновая константа (три касания таблицы сущностей + по два `Vec::push`
  тиков и подъём агрегатов `max_change_tick`/`max_added_tick` на колонку; агрегаты — плата за
  `changed_iter_static` ×232, которой у эталона нет). Названо числом, ячейка `spawn_wide` держит.
  **ОСТАТОК РАЗОБРАН ТЕМ ЖЕ ЗАХОДОМ И ОКАЗАЛСЯ НЕ УСТАНОВЛЕННЫМ — точить его не стали.**
  Гипотеза «мы платим за рост колонок поотдельности, а bevy выделяет СТРОКУ целиком»
  проверена рунгом «тёплый архетип» (популяция уже есть, колонки не растут) и **опровергнута**:
  рост стоит нам 213 мкс против 299 у bevy на 10k (том же бандле, что в ячейке). А само ратио
  оказалось ИНСТРУМЕНТ-ЗАВИСИМЫМ: зонд на ТОМ ЖЕ бандле и том же рунге (10k) даёт
  apex 124.2 против bevy 126.2 нс/спавн (**1.02×**), criterion — 78.8 против 59.8 (**0.76×**).
  Наше собственное число устойчиво между инструментами, эталонное — нет (criterion гоняет тысячи
  итераций на переиспользуемых страницах, зонд — пятнадцать на холодных; аллокаторное состояние
  разное). Поэтому **ценность ячейки `spawn_wide` — охрана НАШЕГО числа от регресса**,
  а не утверждение о ратио; читать её столбец «×bevy» как величину, за которой надо гнаться, нельзя.
  Что УСТОЙЧИВО через оба инструмента и все рунги: пер-компонентная добавка у нас 19.1 нс
  против 16.5 у bevy (было 4.2× разницы — стало 16 %), а однокомпонентный спавн — паритет
  (0.99–1.05×). Остаток не объявлен закрытым и не объявлен долгом: он **не установлен**.
  **Попутно закрыта дыра:** один вывод был выписан ТРИЖДЫ (`spawn_at`, `spawn_many_inner`,
  `spawn_bundles_bulk`), и каждая копия кончалась `filter_map`, молча укорачивающим `col_indices`;
  короткий `col_indices` заставил бы кортежный `write_into_batch` писать компонент в чужую колонку
  (UB). Теперь вывод один и отсутствующая колонка отказывает громко (§0.2a).
  **Гейты на дефекте:** ячейка `spawn_wide` (одиночный спавн широкого бандла — такой не мерила ни
  одна: `simple_insert` пакетный, `commands_spawn` идёт `spawn_bundles_bulk`, узкие прячут дефект);
  тесты `bundle_layout_is_resolved_per_world_not_per_type` (два мира с разным порядком
  `ComponentId`) и `spawn_and_spawn_many_share_one_layout`. Подробности —
  `plans/active/CORE_AUDIT.md`, раздел 2026-08-30.

- **BENCH-EVENTS-0830 ✅ ЗАКРЫТ ТЕМ ЖЕ ЗАХОДОМ (2026-08-30) — ячейка `events_frame_loop` мерила
  раскладку кода, а не `Events` против `Messages`; рунг переставлен туда, где живёт сигнал.** criterion устойчиво (±0.7 % CI) даёт
  apex 160.7 мкс против bevy 104.6. Но те же операторы, собранные в другом бинаре, дают обратное:
  зонд `apex-bench --bin events_shapes` с чередованием A/B внутри одного окна показывает на рунге
  ячейки (8 событий/кадр) apex 10.9–11.1 против bevy 11.5–12.5 нс/кадр — **мы впереди**. А если
  позвать из того же зонда СТРУКТУРЫ САМОЙ ЯЧЕЙКИ, получается 11.81 против 10.81 (0.92×). Одни и
  те же операторы, три места сборки, разброс вердикта в полтора раза.
  **Что устойчиво через все прогоны:** пер-событийная цена apex **~1.8× дешевле** (0.53 против
  0.93 нс/событие — мы кладём 8 байт, bevy кладёт `MessageInstance` = id + сообщение, 16), а
  пер-кадровый свободный член на этой точности не разрешается вовсе (между двумя прогонами ОДНОГО
  бинаря 9.28 против 6.26 у нас и 8.01 против 10.15 у bevy).
  **Класс тот же, что BENCH-GAUGE-0830 и движковый TD-386:** число, чей разброс между
  эквивалентными сборками больше измеряемой дельты, не свидетельство. **Код `Events` не тронут
  сознательно** — ячейка указала бы в путь, где мы и так впереди по единственной устойчивой
  величине; чинился инструмент.
  **Подтверждено на дефекте В ЭТОМ ЖЕ заходе, а не рассуждением.** Правка `BundleCache` тронула
  ТОЛЬКО `world.rs` — и компаратор выдал `events_frame_loop/apex` 134.0 -> 198.5 мкс (+36 %,
  REGRESSION), при том что в том же прогоне bevy-двойник не сдвинулся (104.2), а `events/apex` —
  тот же тип, те же буферы, одноразовая пачка — стал БЫСТРЕЕ (13.4 -> 12.4). Зонд, собранный
  отдельным бинарём, позвал ТЕ ЖЕ структуры ячейки после правки: **10.37 нс/кадр против 11.81 до
  неё**, то есть код стал быстрее ровно там, где criterion объявил 36 % регресса. Ложный красный
  ценой в целый прогон — это и есть цена одноточечной ячейки на переломе.
  **Сделано (чинилась ЯЧЕЙКА, а не код):** рунг `events_frame_loop` переставлен на
  **1000 кадров x 512 событий** — там пер-событийная работа в полсотни раз выше пер-кадрового
  члена, и меряется величина, которая между движками ДЕЙСТВИТЕЛЬНО различается и устойчива
  (apex 1.75–1.79× впереди в независимых прогонах). Пер-кадровый член не потерян: заведена
  отдельная ячейка **`events_frame_idle`** (10k кадров x 1 событие) — там ротация и курсоры это
  ~80 % кадра, и регресс `Events::update` виден именно в ней. Гарантия честности
  (`events_relations_fairness`) больше не носит захардкоженную сумму: она ВЫВОДИТ ожидаемое из
  собственной формы бенча (`event_count()`) и проверяет ОБА рунга — константа пережила бы
  перестановку рунга и продолжила бы утверждать форму, которой уже нет.
  **Что записали переставленные рунги:** `events_frame_loop` (1000 x 512) — apex 288.6 мкс против
  bevy 510.1, **1.77× впереди** (ровно устойчивая величина лестницы); `events_frame_idle`
  (10k x 1) — apex 47.9 против bevy 34.8, **0.73×**: пер-кадровая ротация с курсорами у нас на
  ~1.3 нс/кадр дороже, и это цена нашей семантики (`update` сливает `sync_pending`, обнуляет
  курсоры и пересчитывает `lagging_count` — то, чем куплено «отставший читатель ничего не теряет»;
  у bevy сообщения просто истекают через две ротации, его `update` = swap + clear + счётчик).
  Оставлено сознательно и названо числом. Ячейка `events` (одноразовая пачка 10k) судится с
  оговоркой BENCH-GAUGE-0830 — её bevy-эталон качается.

- **LADDER-SPAWN-0830 / LADDER-CMD-0830 ✅ ЗАВЕДЕНЫ (2026-08-30) — два прибора, которых не было:
  лестницы атрибуции ВНУТРИ `World::spawn` и внутри пути `Commands`.** Обе ячейки
  (`spawn_wide`, `commands_insert`) дают ОДНО число на путь, делающий шесть разных работ, а такое
  число не называет, какую из них чинить (CONVENTIONS §2, урок 25). Приборы —
  `apex-bench --bin spawn_ladder` и `--bin commands_ladder`, форма как у движкового
  `APEX_EXTRACT_LADDER`: один штамп на ПРОХОД, читать разности. Полное описание, включая ЧТО в них
  пришлось починить, чтобы им верить, — `apex-engine/docs/TESTING.md`. Коротко три вещи, каждая
  проверена собственным симптомом: **ступени чередуются** (первая версия мерила блоками и печатала
  отрицательные разности при разбросе 66 % — дрейф машины доставался ступени, которая в этот момент
  бежала); **верхняя ступень — настоящий вызов**, копия сверяется с ним строкой `copy drift`
  (у обеих ≤4 %) и гейтами `ladder_copy_is_observationally_the_real_spawn` /
  `commands_ladder_copy_matches_the_real_path`, которые требуют, чтобы копия оставляла мир,
  НЕОТЛИЧИМЫЙ от построенного производственным путём; **разброс межквартильный**, потому что один
  проход из десяти платит ОС за страничные промахи. `spawn_ladder` вдобавок поворачивается по
  ШИРИНЕ бандла — параметру, который ячейка держит фиксированным, — и печатает для каждой ступени
  пару «на сущность / на компонент». Планка эталона снимается чередованием с bevy ВНУТРИ окна:
  ранняя форма зонда, бравшая абсолют bevy отдельным блоком, читала 0.47× там, где чередование
  читает 0.65×. Гейты обеих лестниц проверены на дефекте (шесть инъекций, каждая покраснела на
  своей строке; инъекция «производство перестало поднимать агрегат added-тика» валит fidelity-гейт
  лестницы ВМЕСТЕ с четырьмя гейтами `Added<T>` — то есть копия сторожит и оригинал).

- **ALLOC-LEASE-0830 ✅ ЗАКРЫТ ТЕМ ЖЕ ЗАХОДОМ (2026-08-30) — `allocate` брал ЗАМОК и клонировал
  `Arc` на КАЖДУЮ сущность, чтобы прочитать лизу, которую сам же публикует.** Найдено первой же
  лестницей: `entities.allocate` — **30 нс, 46 % спавна четырёхкомпонентного бандла и больше, чем
  ВЕСЬ спавн у bevy** (23–25 нс). Цена пер-сущностная (наклон по ширине 0.5 нс/компонент), поэтому
  она и была главной статьёй в узких бандлах: планка была 0.65× на ширине 1 и подтягивалась к
  0.94× только к ширине 8, где её разбавляет пер-компонентная работа.
  **Заявленная планом гипотеза «платим за ТРИ касания таблицы сущностей против двух у bevy»
  ОПРОВЕРГНУТА тем же прибором**: касания 1 и 3 (`get_location`, `set_location`) стоят в пределах
  шума (−0.9 и +1.8 нс на сущность), `ensure_record` — 0.2. Предмет был не в касаниях.
  Причина в исходнике: `allocate(&mut self)` ходил за лизой ТЕМ ЖЕ путём, каким обязан ходить
  РЕЗЕРВЕР через `&self` — `cell.read()` (RwLock) + `Arc::clone` + `Arc::drop` + безусловный
  `cursor.fetch_sub` + `high_water.fetch_add`: **шесть атомарных read-modify-write на сущность**
  ради значения, которого эта же структура единственный писатель.
  **Фикс:** аллокатор держит СВОЮ ручку на текущую лизу (`owned_lease`) — тот же `Arc`, что лежит
  в ячейке, не снимок с неё; `refresh_lease` — единственный писатель ячейки — ставит ОБА поля из
  одного значения, поэтому разойтись они не могут. Ячейка не тронута: резерверы читают её как
  читали, и именно чтение ячейки делает для них невозможным устаревший снимок (B2). Плюс курсор
  теперь ЧИТАЕТСЯ перед декрементом: исчерпанная лиза — установившийся режим любого мира, который
  спавнит больше, чем деспавнит, а безусловный декремент был атомарной операцией на сущность,
  которая никогда ничего не могла вернуть (поднять курсор может только `refresh_lease`, а ему
  нужен `&mut self`, — значит увиденная исчерпанной лиза не может стать непустой под этим вызовом).
  Шесть атомарных операций → одна. **Стало:** ступень `allocate` 30 → **7 нс**, спавн бандла
  ячейки 66.0 → **46.4 нс**, планка по ширинам 1/2/4/8 (чередованием с bevy)
  0.65/0.78/0.87/0.94 → **1.36/1.39/1.17/1.03**. Инвариант «ручка ЕСТЬ содержимое ячейки»
  утверждается `debug_assert` прямо в `refresh_lease` (то есть в каждом debug-прогоне каждого
  вызывающего `flush`) и гейтом `the_allocators_lease_handle_is_the_shared_one`, который проверяет
  не только `ptr_eq`, но и что переиспользование РЕАЛЬНО приходит через эту ручку: устаревшая
  ручка с живым курсором продолжала бы выдавать id, и один `ptr_eq` этого не заметил бы.
  Инъекция «`refresh_lease` не обновляет ручку» валит восемь тестов сущностей.

- **CMD-RECORD-0830 ✅ ЗАКРЫТ ТЕМ ЖЕ ЗАХОДОМ (2026-08-30) — половина ЗАПИСИ в `Commands` стоила 2.85× эталона, и это было ЕДИНСТВЕННОЕ
  место, где отложенная вставка нам дорога.** Названо числом второй лестницей; предмет тот, что
  раньше выводился ВЫЧИТАНИЕМ двух ячеек (`commands_insert` 0.89× при `add_remove_component` 1.58×
  впереди), а вычитание — не измерение. Что измерено, всё внутри одного окна, 10k вставок:
  прямой `World::insert` **35.2 нс против 43.6 у bevy (1.24× впереди)**; половина ПРИМЕНЕНИЯ
  **36.0 против 48.2 (тоже впереди)**; половина ЗАПИСИ **21.2 против 7.4 — 2.85× ПОЗАДИ**.
  Итог отложенного пути 57.2 против 55.6, то есть машинерия стоит нам 22.0 нс против их 12.0.
  Внутри записи: `arena.alloc` 3.2 нс, `queue.push` **16.1 нс**. Два варианта, снятые чередованием
  друг с другом, делят эти 16.1 пополам и НАЗЫВАЮТ обе половины:
  в **зарезервированную** очередь — 13.2 (то есть **6.7 нс — это перерост аллокации**: `apply`
  забирает очередь через `into_iter` и выбрасывает её ёмкость, так что 10k `push` каждый раз
  растят Vec от нуля через ~19 перевыделений); в **12-байтовую** запись вместо 48-байтовой — 5.7
  (то есть **~7.5 нс — это ширина самой записи**).
  **Форма решения известна и это тот же урок, что BUNDLE-RESOLVE-0830 в другой проекции:** три
  функциональных указателя, которые несёт КАЖДАЯ команда `Insert` (`apply`, `drop`, `cid_fn`), —
  свойство ТИПА, а хранятся на КОМАНДУ; то же у `Spawn` (`apply`, `apply_batch`, `drop`). Пер-Commands
  таблица вида «тип → тройка указателей» + `u32`-индекс в команде сжимает `Insert` и `Spawn` до 16
  байт, но размер enum держат ещё `InsertRaw` (несёт `Vec<u8>`) и `SpawnFromTemplate`; чтобы
  страж `size_of::<Command>() <= 48` можно было опустить до 24, их полезную нагрузку тоже надо
  убрать в `Box` (редкие пути, цена там не платится). Вторая половина — ёмкость: `apply` обязан
  оставлять аллокацию очереди себе, а `drain(..)` уже измерялся и оказался ХУЖЕ (+5–8 %, PE-C4)
  из-за drop-guard в его итераторе; годная форма — вычитывание команд сырым указателем с
  `set_len(0)` ДО прохода (сохраняет и аллокацию, и итерацию бампом указателя; на панике течёт,
  но не роняет владение дважды). ⚠ Ёмкость **не проверяется ячейкой**: `commands_insert` создаёт
  `Commands::new()` на каждый прогон, поэтому выигрыш от сохранённой ёмкости виден только там, где
  `Commands` переживает кадр (планировщик), — мерить придётся зондом с рукой «переиспользованный
  `Commands`», иначе правка будет выглядеть бесплатной и бесполезной одновременно.
  **ВЗЯТО И ЗАКРЫТО тем же заходом, обе половины.**
  *(1) Ширина.* Три функциональных указателя `Insert` и три у `Spawn` сведены в ОДИН
  `&'static InsertVtable` / `&'static SpawnVtable` — пер-типовая константа
  (`trait InsertMeta { const VTABLE }` + блэнкет-impl, мономорфизуется в `&'static` на месте
  вызова). Широкие редкие полезные нагрузки (`InsertRaw` с `Vec<u8>`, `SpawnFromTemplate` со
  `String`+`TemplateParams`) убраны в `Box` целиком. **`size_of::<Command>()` 48 → 32**, страж
  опущен до 32 и объясняет, ЧТО его держит (пара subject/target у relation-команд; ниже 32 —
  ещё ~4 нс, не взято). Группировка спавн-бёрста теперь сравнивает АДРЕСА вtable вместо
  указателей на функции: у разных `B` константы различаются содержимым, линкеру нечего слить.
  *(2) Ёмкость.* `apply` больше не отдаёт аллокацию очереди итератору. Живая очередь меняется
  местами с запасным буфером (`Commands::spare`), команды вычитываются сырым указателем после
  `set_len(0)` (сохраняет и аллокацию, и итерацию бампом — в отличие от `drain(..)`, который
  измерялся в PE-C4 и был ХУЖЕ на 5–8 % из-за drop-guard в его итераторе). Запасной буфер, а не
  обход на месте: применяемая команда может дойти до произвольного кода (`Command::Apply` несёт
  замыкание с `&mut World`), и реэнтрантный `push` обязан попасть в очередь, которую этот проход
  не обходит — ровно то, что раньше гарантировал `mem::take`.
  **ЧИСЛА (зонд, чистое окно, 10k вставок, ns/вставку):** запись НА ВТОРОМ КАДРЕ 21.2 → **7.5**
  при bevy 7.4 (**0.99×, паритет**); обход очереди в половине применения 12.4 → **2.8**;
  применение целиком 36.7 при bevy 49.3 (**1.34× впереди**); отложенный путь целиком
  **1.20× впереди** эталона на тёплом `Commands` и 1.03× на холодном.
  **Что ОСТАЛОСЬ и почему это не «полумера»:** ХОЛОДНАЯ запись (первый в жизни `Commands`) —
  18.5 против 7.4, и это не та величина, которую бежит движок: планировщик переиспользует
  `Commands` слотами (`reset_for_reuse`), то есть каждый кадр после первого — тёплый. Цена
  холодного пути — рост ДВУХ буферов (очередь + арена) там, где у эталона один: полезная нагрузка
  лежит в очереди рядом с командой. Слияние арены в очередь — запись переменной длины, отдельный
  предмет; названо и не взято.
  ⚠ **Ячейка `commands_insert` этой правки НЕ ВИДИТ** и видеть не может: она заводит
  `Commands::new()` на каждый прогон, то есть меряет ровно холодный путь. Поэтому купленное
  сторожит не она, а гейт `a_commands_that_applied_once_keeps_its_queues_room` — на ЁМКОСТИ, а не
  на времени (гейт на несколько наносекунд был бы гейтом, чей вердикт есть его собственный
  разброс). Проверено на дефекте: инъекция «apply снова выбрасывает аллокацию» валит его;
  инъекция «новый вариант понёс нагрузку inline» валит страж 32 байт; инъекция «бёрст спавна
  перестал различать тип» валит `entity_commands_with_children_wires_childof`.

- **BENCH-GAUGE-0830 ✅ ЗАКРЫТ ТЕМ ЖЕ ЗАХОДОМ (2026-08-30) — вердикт о машине опирался на ЛЮБОЙ
  ОДИН эталон, и один неустойчивый бенчмарк заткнул весь гейт.** Найдено использованием: движковая
  сессия (PS.14) прогнала сравнительные бенчи ядра, чтобы подтвердить отсутствие регресса, и
  получила «NOT COMPARABLE» — уехал `events/bevy`. Три независимых прогона: **−28 %, −36 %, −30 %**,
  всегда он один, при том что остальные ~20 эталонных чисел стояли в пределах нескольких процентов
  (и сами между собой этот бенчмарк разбрасывает 18.7–21.0 мкс, то есть ~12 % — величина, сравнимая
  с допуском 15 %, которым его судят). Класс тот же, что у движкового TD-386: **метрика, чей
  собственный разброс подходит к допуску, не может быть судьёй.**
  **Правило переписано на НАБОР эталонов** (`tools/bench_baseline.ps1`): о машине судит МЕДИАННЫЙ
  дрейф эталонов (робастная статистика — один дикий ряд её не двигает) плюс кворум (четверть
  эталонов); машина, которая действительно уехала, двигает МНОГО рядов сразу. Одинокий беглец
  дисквалифицирует **свою группу**, поимённо и вслух, а остальные судятся. Прогон, оставивший
  группу несудимой, возвращает 3, а не 0: зелёный по 23 группам — настоящий ответ, но он не смеет
  выглядеть как прогон, который проверил всё.
  **Проверено на дефекте и на данных, а не рассуждением.** Три ветки правила подняты собственной
  ручкой инструмента на РЕАЛЬНОМ выводе criterion: `-ReferenceTolerance 0.01` → «NOT COMPARABLE,
  медианный эталон уехал» (машинная ветка); `0.15` (дефолт) → «23 группы судимы, `events` — нет»;
  `0.50` → «no regression». Инъекция: обе защиты инертны ⇒ инструмент печатает чистое
  «no regression» на прогоне, где почти каждый эталон ушёл за допуск — ровно тот ложный зелёный,
  ради которого защита и стоит.
  **Что теперь известно про ядро (тот же прогон, дефолтный допуск):** регресса нет в 23 группах из
  24; `propagate/apex` −6.9 %, `simple_insert` −9.7 %, `changed_iter_frag` −10.7 %,
  `relations` −4.3 %; `schedule/apex` +5.2 % (три прогона подряд дали +14.1 / +9.6 / +5.2 % —
  разброс больше самой дельты, поэтому дельта не свидетельство), `events_frame_loop/apex` +16.2 %
  при −2.7 % на соседнем прогоне того же бинаря — та же неустойчивость, обе внутри правила 20 %.
  Группа `events` не судима, пока её эталон качается.

- **BENCH-REGRESS-0824 ✅ ЗАКРЫТ (2026-08-27) — три утраченные победы возвращены; виноват был ОДИН
  коммит и ОДНА строка в нём.** Было: schedule ×2.5, wide_iter ×2.2, fragmented_iter ×2.06 к
  собственным записанным числам; запись «No change» уже сравнивала с регрессировавшим байзлайном. Найдено 2026-08-24 свежим прогоном бенчей (criterion, features
  bevy+legion, HEAD `87e1278`; шок-числа подтверждены двумя прогонами):
  | группа | apex сейчас | apex записан (sh_final 2026-07-04) | bevy сейчас/записан | вердикт |
  |---|--:|--:|--:|---|
  | schedule | **66.4-66.8 µs** | 26.6 µs | 38.5-39.2 / 38.6 | был ПОБЕДОЙ 1.45×, стал 0.58× |
  | wide_iter | **7.90-7.92 µs** | 3.49 µs | 3.63-3.77 / 3.55 | был паритет 0.98×, стал 0.46× |
  | fragmented_iter | **360.8 ns** | 175 ns | 134.4 / 139 | 0.70× → 0.37× |
  Числа bevy/legion со старыми записями совпадают → регрессировала apex-сторона, не машина/стенд.
  Не-регрессировавшее подтверждено тем же прогоном: changed_iter_static **15.2 нс (×232 vs bevy)**,
  changed_iter_frag ×9.8, get_component 1.09×, simple_iter паритет (chunked 1.37×), heavy_compute
  подтянулся 0.83→0.92×, relations стабильно 0.74-0.76×, propagate улучшился 318→287 µs.
  **Окно регресса:** sh_final (2026-07-04) → CORE_ECONOMY волна 4 (2026-08-06, «schedule 67.6 No
  change» — байзлайн уже был плохим). Кандидаты в окне: волна 7 (декомпозиция scheduler), C6
  (консолидация query-зоопарка — Owned/Shared/Borrowed fetch-машина как раз меняет форму итерации),
  PE-C*. **Класс-урок:** «падение >20 % блокирует мерж» — локальная дисциплина без локального
  байзлайн-СНИМКА не срабатывает: сравнивать было не с чем, и «No change» узаконил регресс.
  **План:** (1) бисекция по трём группам в окне; вернуть ≥ записанных чисел; (2) ✅ **СДЕЛАНО
  2026-08-24** — `tools/bench_baseline.ps1` + закоммиченный снимок
  `tools/baselines/core_bench.baseline.json` (25 групп; читает `estimates.json` самого criterion,
  повторный прогон не нужен); (3) обновить §14.8 руководства по свежему стендингу (там до сих пор
  прогон 2026-06-17).
  **Про (2), две вещи, без которых снимок был бы вредным:**
  — **Байзлайн НЕ выдаёт себя за цель.** Снимок фиксирует РЕГРЕССИРОВАВШЕЕ состояние (schedule
  65.9 µs), поэтому рядом с ним лежат `targets_ns` — записанные победы (26.6 µs / 3.49 µs / 175 нс),
  и компаратор печатает столбец `target` с пометкой `owed 2.5x`, а зелёный вердикт сопровождает
  словами «no slide, not arrived». Иначе гейт узаконил бы регресс — ровно то, что уже сделала
  запись «No change».
  — **Эталон = проверка МАШИНЫ.** bevy/legion меряются в тех же группах и двигаются только когда
  двигается машина. Компаратор смотрит на них ПЕРВЫМИ: ушли за 15 % — прогон не про ядро, а про
  машину, и сравнение отказывается (код 3), а не объявляется регрессом. Это тот же приём, которым
  сессия обмера и доказала, что регресс настоящий (bevy совпал со старыми записями, apex — нет).
  **Гейт проверен на дефекте в обе стороны** (инъекция в criterion-выход): apex +30 % → код 1
  (регресс); bevy +40 % → код 3 (не сравнивается). Пороги: 20 % на наши группы (правило мержа
  ядра), 15 % на эталон. Движковый контекст:
  кадр движка платит schedule каждым тиком каждого мира; wide_iter — форма UI-мира
  (`apex-engine/plans/active/PERF_SUPERIORITY.md`, Ф5).

  ---
  **ЗАКРЫТИЕ (2026-08-27).**

  **Виновник назван бисекцией, не чтением кода.** `git bisect run` по окну (165 коммитов, 7 шагов;
  судья — `schedule/apex` с порогом 45 µs, но все три группы логировались на КАЖДОМ шаге, поэтому
  две другие локализовались бесплатно) → **`777a5da` «feat(core): PE-C2 — per-column tick
  aggregates»**. Он регрессировал все три группы разом: 28.9→66.8 µs / 3.63→7.74 µs /
  174.5→335.9 нс. **Инструмент откалиброван ОБОИМИ концами** прежде, чем ему поверили: HEAD
  воспроизвёл записанное регрессировавшее состояние, а сам sh_final-коммит на сегодняшней машине
  дал 29.5 / 3.61 / 157.8 — то есть записанные победы настоящие, а не артефакт той машины.

  **Корень — не инструкция, а её непрозрачность.** Абляция (снять ОДНУ строку `raise`) вернула все
  три числа точно. Relaxed-load на x86 это `mov`; цена в том, что АТОМИК в построчной петле
  запрещает векторизацию всей петли записи. Измерены и отвергнуты ещё два варианта: условный атомик
  (векторизатор отказывается от петли, несущей атомик на ЛЮБОЙ ветке) и простой байт-мемо (запись по
  инвариантному адресу стоит тех же 15–18 %).

  **Фикс** — дополнение к ADR-011 (развилка Д-1 «точность через `Mut`» СОХРАНЕНА, переехало место
  подъёма): вопрос «штампилась ли строка» задаётся один раз на выходе из архетипа и задаётся ТОЧНО
  (штамп кладёт `this_run` в тик строки ⇒ ответ есть «есть ли строка с тиком `this_run`», с
  O(1)-выходом «агрегат уже не старее»). `fetch_item<const SCOPED: bool>` — обещание ВЫЗЫВАЮЩЕГО,
  что item не переживёт состояние; подъём делает ДЕСТРУКТОР `ScopedState`, потому что замыкание
  умеет выйти раскруткой. Детали и отвергнутые альтернативы — в дополнении к `decisions/ADR-011`.

  | группа | регресс | стало | записанная цель | vs Bevy было → стало |
  |---|--:|--:|--:|---|
  | schedule/apex | 65 919 нс | **26 336** (−60 %) | 26 600 — **взята** | 0.58× → **1.50×** |
  | wide_iter/apex | 7 897 нс | **3 686** (−52 %) | 3 490 — +5.6 % | 0.46× → **0.99×** |
  | fragmented_iter/apex | 356.9 нс | **175.4** (−49 %) | 175 — **ровно** | 0.37× → **0.72×** |
  | changed_iter_static/apex | 15.0 нс | **14.6** | покупка PE-C2 цела (×242 к Bevy) | — |

  **Как доказано, что больше ничего не сломано — и почему НЕ байзлайном.** Компаратор на полном
  3-way прогоне ОТКАЗАЛСЯ сравнивать (код 3): эталоны уехали (`events/bevy` +51 %,
  `simple_insert/legion` +17 %) — машина не та, на которой снят байзлайн 2026-08-24. Гейт сработал
  как задуман. Поэтому контролем стал не трёхдневный байзлайн, а **тот же код без правки на этой же
  машине**: правка отложена в stash, полный 3-way прогон повторён, обе руки мерят bevy/legion. Между
  руками машина уехала на 2–11.5 % по эталонам, и КАЖДЫЙ «SLOWER» на apex сопровождался таким же
  дрейфом эталона — включая группы, которых правка физически не касается (`simple_insert` +12.4 %,
  `commands_spawn` +8.4 %, `events` +8.5 %: спавн, команды, события). Три целевые группы уехали на
  −49…−60 %, то есть в 5–10 раз за полосу дрейфа.
  Единственная группа НА ПУТИ правки, где apex обогнал дрейф эталона (`changed_iter_frag` +12.9 %
  при эталоне +4.1 %), домерена **чередующимся** A/B (FIX/CTL/FIX/CTL, соседние во времени):
  реальная цена — **+2 %** (`changed_iter` 8117/8048 против 7876/7891; `changed_iter_frag`
  421.6/420.4 против 413.5/413.2; `changed_iter_static` ±0). Источник назван: `Mut` подрос на байт
  (`defer_agg`), а `changed_iter` строит его 1000 раз за итерацию через `world.get_mut`.

  **Гейты фикса:** apex-core 302 теста (+4 новых), весь workspace 44 группы, clippy net-neutral
  (4 pre-existing), Miri TB 22/22 чист включая rayon-путь, Miri SB 11/11 чист на однопоточных
  (единственная SB-жалоба — внутри crossbeam-epoch, зависимости rayon). **Каждый новый тест проверен
  НА СВОЁМ дефекте, и на каждом дефекте падает ровно один:** консервативный подъём валит
  `a_write_iteration_that_stamps_nothing_leaves_the_archetype_skippable`; подъём не-в-деструкторе —
  `a_panic_mid_archetype_still_settles_the_aggregate`; молчащий flush — шесть существующих тестов
  (`write_query_marks_changed`, четыре transform-теста, `bevy_ref_syntax_query`).

  **Байзлайн переснят** на восстановленные числа (`tools/baselines/core_bench.baseline.json`;
  `targets_note` больше не говорит «REGRESSED») — иначе гейт продолжал бы охранять регресс.

  **Класс-урок (он же причина, по которой это прожило три недели): ГЕЙТ НАД ЧАСТЬЮ КЛАССА не
  охраняет класс.** PE-C2 честно измерил write-путь — на `simple_iter`/`heavy_compute`/`propagate`
  — и записал «остаток ≤2 %». Но настоящая форма write-пути (плотная петля `for_each_mut`) живёт в
  ТЕХ группах, которые он не гонял. Правило: перф-заявление о КЛАССЕ путей доказывают прогоном
  ВСЕГО класса; выбор подмножества — это и есть та дыра, в которую уходит победа.
  **Второй урок, оплаченный сегодня:** подозрительное число сравнивают с контролем, СОСЕДНИМ ВО
  ВРЕМЕНИ, а не с записью трёхдневной давности — машина за трое суток уезжает на 11 %, а это больше
  половины порогов, которыми мы судим.
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
