# CORE_AUDIT — глубокий аудит ядра apex-ecs (июль 2026): находки + план работ

> **Назначение.** Полный аудит ядра ECS на соответствие «золотому пути» проекта
> (apex-engine: CLAUDE.md, docs/CONVENTIONS.md §0.2a/§0.2b/§0.9): корректность/soundness,
> сырые места, производительность, профессионализм. Итог — план работ волнами.
> **Дата:** 2026-07-03. **Статус:** 🔄 в работе. Все 10 развилок §10 РЕШЕНЫ 2026-07-03
> (критерий — золотой путь, см. §10). **Волны 0-4 ✅ СМЕРЖЕНЫ и ЗАПУШЕНЫ** (0-3 soundness/
> корректность/panic-safety; 4 = громкость §0.2a + гигиена + мёртвый код). **Волна 5 🔄 в работе**
> (перф + ложные Changed; A13 ✅ get_mut→Mut; ветка `core-audit-wave5`; см. «Журнал волн»).

## Журнал волн (ход исполнения)

**Волна 0 ✅** (`138f0c1`): удалён мусор из корня репо (results.txt UTF-16 / perf.txt /
apex-benc.txt) + .gitignore; план заведён в репо ядра; Miri-компонент установлен.

**Волна 1 ✅ — SOUNDNESS (достижимый UB из safe-кода), 11 фиксов + регресс-тесты:**
- `19df31f` B1 (CommandArena выравнивание align>8 — glam-типы) + B3 (mem::zeroed для
  не-ZST RelationKind → compile-ассерт ZST / захват kind).
- `0e1d2c1` A1 (дубликат компонента в bundle → panic+dedup в choke point) + B8 (insert_raw
  валидация размера) + A9-leak (drop T на мёртвой entity) + B4 (template Arc, UAF при
  само-перерегистрации).
- `87ec817` C1 (SubWorld лайфтаймы: безопасный new/with_ranges + unsafe from_raw для
  планировщика) + D10 (SystemBuilder держит &'a mut Scheduler, не *mut).
- `6b9b4ad` C7 (WorldQuery → unsafe trait с контрактом).
- `de474c5` E2 (IsolatedWorld мост: clone-then-drain, UAF при само-удалении ресурса; unsafe
  убран целиком).
- `4787d6e` E1 (скриптинг: валидация `_meta` из Lua — bounds+identity+ре-резолв col; закрыта
  UB-дыра песочницы; первый тест крейта).
- `2dff9b7` B2 (единая разделяемая ячейка аренды `Arc<RwLock<Arc<ReserveLease>>>` — устаревший
  снимок аренды невозможен; двойная выдача Entity закрыта).
- **C2 перенесена в волну 3** (co-located с В1(б) runtime borrow-чекером — полная детекция
  write-write+write-read, см. §4).
- **Гейт волны 1:** весь воркспейс зелёный (тесты + примеры), clippy net-neutral (только 3
  pre-existing warning в apex-bench), frag_world 5 архетипов, criterion без регресса
  (commands_spawn 395µs, despawn 246µs). **Miri подтвердил B1 (арена) и B2 (lease) —
  UB-чисто.** ⚠ **Блокер Miri-гейта для волны 3:** `linkme-0.3.36` (авто-регистрация
  компонентов) даёт `subtract with overflow` в distributed_slice под Miri на ЛЮБОМ тесте,
  строящем `World` → полный Miri-сьют не зелёный. Волна 3 обязана добавить `cfg(miri)`-путь,
  форсирующий ленивую регистрацию (как wasm-путь TD-25), прежде чем делать Miri обязательным
  гейтом.
- **✅ Goldens движка ПРОЙДЕНЫ** (2026-07-03): `cargo test -p apex-render --lib
  --features visual_tests` = **656 passed / 0 failed, байт-идентично** против всей ветки
  (волна 1 + волна 2). Изменения ядра поведение рендера не изменили. Движок собирается
  против ветки (apex-render build ок). Ветка готова к мержу/пушу (`core-audit-fixes`).

**Волна 2 ✅ — КОРРЕКТНОСТЬ поведения (ВЫПОЛНЕНА 2026-07-03; 20 фиксов + регресс-тесты; 3
агента параллелили независимые крейты). Гейт: воркспейс+примеры зелёные, clippy net-neutral,
движок собирается, goldens 656/656 байт-идентично.**
Дополнительно к 6 флагманам (ниже): D5 (ложный CircularDependency при обратном before) ·
D7 (states: on_enter(initial) в Update + отказ двойного init_state) · D2 (empty-access система
исполняется раз, не по чанкам) · D6 (change-окно не двигается на полностью run_if-пропущенной
стадии) · E3 (Lua Box::leak → String-ключи) · E10 (Lua entity id «index:generation» с
generation) · E4 (reapply префаба сохраняет N инстансов с parent/overrides) · E9 (watch_config
регистрирует loader до первичной загрузки + настоящий дебаунс) · F7 (derive Scriptable
tuple/enum компилируется+roundtrip) · F3 (Ctx-система = whole-world; whole-world конфликтует со
всеми в конфликт-детекте) · F6 (второй query-параметр в system! = compile_error) · F2 (гонка
Listen‖Listen: конкурентные читатели одного события сериализуются — SharedEventReaders) ·
D4 (Filtered-индексы обновляются между стадиями в кадре) · C5 (задокументирован reparent-
контракт; редактор уже компенсирует).
**Отложено (не correctness): D9** (дедуп 3 копий исполнителя стадии — код-качество) ·
**F4** (персистентные per-system курсоры событий — архитектурно, в волну 6 с borrow-моделью В1;
F2 закрыл лишь гонку, семантика катчапа FixedUpdate/Removed<T> остаётся). **C2** — в волне 3.

Флагманские 🔴 (6 фиксов + регресс-тесты):
- `8c878f0` F1 (Events: при отстающем читателе новые события терялись для всех — layout
  `[old,new]` без сдвига курсоров).
- `707fcb8` D1 (FixedUpdate: 2-я+ exec-стадия старвалась — обе исполнителя (run_hybrid_parallel
  + run_sequential) дренируют группу FixedUpdate раз и гоняют `(A;B)×N`).
- `f47d481` B9+C4 (ChildOf эксклюзивен → фикс torn-write мульти-родителя; self/cycle-рёбра
  отвергаются в add_relation_by_kind_idx + depth-guard в propagate против зависания).
- `0e20a70` A7 (паника в хуке больше не заклинивает диспетчер хуков навсегда — RAII-guard).
- `c1a4880` E5 (циклы Ref-префабов → CycleDetected вместо stack overflow; + depth-лимит).
- **Гейт волны 2 (ФИНАЛЬНЫЙ, пройден):** весь воркспейс + примеры зелёные, clippy net-neutral,
  движок собирается против ветки, **goldens движка 656/656 байт-идентично** (рендер не изменён).
  Все correctness/soundness-пункты волны 2 закрыты; отложены только D9 (косметика) и F4
  (архитектурно, волна 6). **Волны 0-2 СМЕРЖЕНЫ в main** (`b3fb9ed`, локально, не запушено —
  пуш делает пользователь).

**Волна 3 ✅ — PANIC-SAFETY + формальный UB (ВЫПОЛНЕНА 2026-07-03; ветка `core-audit-wave3` от
main; гейт пройден, готова к мержу `--no-ff` в main):**
- `a614c04` — снят блокер Miri↔linkme: `cfg(miri)` → ленивая регистрация компонентов (как wasm),
  distributed_slice под Miri не итерируется; тесты со `World` проходят Miri. **Вторая Miri-нота
  (не наш баг):** `crossbeam-epoch`/rayon даёт ложное Stacked-Borrows-срабатывание на
  `par`-тестах → гейт идёт через `-Zmiri-tree-borrows` (crossbeam проходит; реальный UB ловит).
- `06e7145` A3 — Resources в `UnsafeCell` (провенанс `*mut` от cell, не отмытый из `&T`; N19
  двойной Box убран). **Подтверждено: Miri Tree Borrows находил A3 как реальный UB, после фикса
  тест зелёный.**
- **ОСТАТОК волны 3 — ВЫПОЛНЕН (фокус-сессия 2026-07-03, 6 коммитов + регресс-тесты):**
  - `5d03d93` **A2** ✅ — `change_ticks/added_ticks: Vec<TickCell>` (TickCell=`UnsafeCell<Tick>`,
    `#[repr(transparent)]`+unsafe Sync); ticks_ptr/stamp_range/set_change_tick пишут с
    cell-провенансом. Регресс `a2_write_stamps_change_tick_soundly` (Write hot-path + dense
    stamp_range) — **Miri Tree Borrows зелёный** (до фикса — тот же UB-класс, что A3).
  - `47b6471` **A8** ✅ — swap_remove_and_drop: swap→shrink len/тики→ТОЛЬКО потом drop (значение
    вне живого диапазона). Паника в Drop больше не даёт double-drop. Регресс с catch_unwind,
    Miri-зелёный.
  - `1376391` **A6** ✅ — `BulkSpawnRollback` drop-guard в spawn_many_inner/spawn_bundles_bulk:
    unwind обрезает `arch.entities` до `start_row` (== `col.len`), нет ghost-строк над
    неинициализированной памятью. Регресс `spawn_many_panic_rolls_back_batch`, Miri-зелёный.
    (B-Н17 «арена apply panic-guard» — CommandArena::apply не запускает Drop чужих команд при
    панике одной; фактического double-drop нет, отдельного фикса не потребовалось.)
  - `2fe0134` **D3** ✅ — `AsdTask.ptr: SendPtr<dyn ParSystem>` (не `SystemDescriptor`): целим в
    сам trait-object (ZST у split-систем) — конкурентные `&mut *ptr` не алиасят реальных байт.
    `SendPtr<T: ?Sized>`. Регресс `asd_row_split_no_descriptor_aliasing` (детерминир. 4-thread
    pool + chunk-config) — **Miri Tree Borrows зелёный** (`-Zmiri-ignore-leaks` — только для
    rayon-teardown-утечек, не наш UB).
  - `7874453` **C2** ✅ — самоконфликт формы закрыт ВСЕГДА-включённой проверкой при
    конструировании Query (как Bevy): новый `WorldQuery::fill_data_access` (роли данных;
    фильтры/Entity/`()` — no-op) + `assert_no_self_alias` в `new_with_tick`/
    `new_within_archetypes`. Паникует на `Query<(&mut T,&mut T)>`/`(Read<T>,Write<T>)`; легальные
    формы (distinct/shared+shared/With/Changed/Maybe) не трогает. Регресс
    `c2_rejects_self_aliasing_query_shapes`.
  - `b3a6ca4` **F5** — вердикт Miri: **НЕ UB**. `advance_reader_mut` мутирует только курсор
    (`cursors`/`lagging_count`), не буфер `events` → location-precise Tree Borrows не конфликтует
    с живыми `&T`. Все 21 events-теста (вкл. `event_iterator_collect_holds_references`, держащий
    `&i32` после дропа итератора) зелены под TB. Инвариант сделан «громким» (§0.2a) в SAFETY-
    комментах Drop/advance_reader_mut.
  - **В1(б) (cross-query runtime borrow-registry) — ОТЛОЖЕН в В1(в)** (⚠ дизайн-решение
    2026-07-03): per-ComponentId registry на уровне view-заёма ДАЁТ ЛОЖНЫЕ СРАБАТЫВАНИЯ на
    ASD row-split планировщика (несколько конкурентных `Query<Write<T>>` над НЕПЕРЕСЕКАЮЩИМИСЯ
    строками одного компонента → все берут T-exclusive → паника на легальном параллелизме).
    Granularity per-ComponentId не выражает per-row дизъюнктность; корректная модель — per-row/
    UnsafeWorldCell (В1(в), migration-волна). Concrete-🔴 C2 (ради которого В1(б) был co-located)
    закрыт лучше — always-on construction-check (см. выше). `q.get(e)` дважды (A4) требует
    guard-типа на возврате get_mut → тоже В1(в). Итог: В1(б) как отдельный дев-инструмент не
    делаем (был бы §0.2b-полумерой с планировщик-футганом); детекция write-write/write-read
    закрывается типами в В1(в).
  - **Pre-existing UB, найденные ПЕРВЫМ полным Miri-прогоном apex-core --lib (чиним по одному):**
    - `ae6f44b` **insert_raw alignment** — dead-path дропал `T` через `drop_in_place` по
      `Vec<u8>`-указателю (align 1); для `align>1` компонента (Arc/Box/glam) — unaligned reference
      UB. Фикс: дроп через выровненную scratch-аллокацию `(size, align)`. Miri: "unaligned
      reference (required 8, found 2)" в `insert_raw_on_dead_entity_drops_value_not_leaks`.
    - `5a3d053` **events get_raw_ptr** — `EventRegistry::get_raw_ptr` отмывал `*mut Events<T>` из
      shared `downcast_ref`, запись через него (EventWriter push / add_reader `next_cursor_id+=1`)
      = write-through-frozen UB (класс A3). Фикс: очереди в `EventQueueCell(UnsafeCell<Box<dyn
      AnyEventQueue>>)`, провенанс из cell. Miri: "write ... forbidden ... Frozen" в
      `removed_events_emitted_for_remove_and_despawn`.
    - `97e4e4c` **transform test-compat** (не UB) — `transform_components_auto_registered` требует
      linkme-авторегистрацию, отключённую под Miri/wasm (ленивая регистрация, TD-25) → падал по
      linkme, не по багу. Помечен `#[cfg_attr(any(miri, target_arch="wasm32"), ignore)]`.
  - **Гейт волны 3 (ПРОЙДЕН):** воркспейс `cargo test --workspace` зелёный (29 групп, 0 падений);
    clippy net-neutral (только 3 pre-existing в apex-bench); **Miri Tree Borrows на apex-core
    --lib: 211 passed, 0 failed, 0 UB, 1 ignored** (`-Zmiri-disable-isolation -Zmiri-tree-borrows`).
    ⚠ На выходе процесса Miri ругается "main thread terminated without waiting for all remaining
    threads" из-за rayon-теста `dense_par_chunk` (глобальный threadpool паркует рабочие потоки и
    не джойнит их) — это НЕ наш UB (как и crossbeam-false-positive из SB), снимается
    `-Zmiri-ignore-leaks`. Гейт зачтён по отсутствию UB (все 211 тестов зелены). Движок собирается
    + goldens 656/656 байт-идентично.

**Волна 4 🔄 — §0.2a ГРОМКОСТЬ + гигиена + мёртвый код (В РАБОТЕ; ветка `core-audit-wave4` от
main):**
- `6f562c3` **A11** ✅ — checked column alloc size на realloc-пути.
- `003e45b` **A12** ✅ — `len`/`is_empty` честны к row-ranges и построчным фильтрам.
- **Warn-волна §0.2a (ВЫПОЛНЕНА; единый `warn_once!` — лог один раз на call-site, AtomicBool-
  латч, стиль Bevy; re-export из apex-core для всех крейтов):**
  - `4e376b8` **A9/A10/B7/B10** ✅ — apex-core: `warn_once!` + громкие misuse-пути. A9 —
    insert/insert_raw/insert_parts/remove_raw/remove на мёртвой entity (no-op → warn, значение
    дропается не течёт). A10 — `spawn_at` на УЖЕ живую entity (осиротил бы строку): debug =
    panic, release = громкий refuse+drop bundle; reserved-but-unflushed id (location-less)
    проходят. B7 — `Commands::despawn` на уже-мёртвую (двойной/каскад) громко. B10 —
    `Commands::spawn_template` с незарег. именем + `TemplateParams::set` с ошибкой сериализации.
    Регресс `loudness_wave4`: throttle single-emission, A10 refusal, B7 clean double-despawn,
    B10 no-op.
  - `25cea06` **E8 (serde)** ✅ — restore дропает компонент, зарег. БЕЗ serde-fns (version skew) —
    теперь warn. Регресс `restore_drops_component_registered_without_serde`.
  - `86c6c82` **E12 (isolated)** ✅ — WorldBridge/CloneableBridge громко о дропе события на
    ошибке bincode-serialize и о закрытом канале (был `let _ = send`). Регресс: closed-channel +
    serialize-failure дропают без паники.
  - `f6883a3` **E8 (scripting)** ✅ — Lua-query по незарег. data/With → пустой результат громко;
    незарег. Without (over-match) громко; исчезнувший binding mid-iter громко. Тестов нет
    (харнесс скриптинга — волна 7).
- **Гигиена (ВЫПОЛНЕНА):**
  - `bf441c3` — лишний unsafe ×4 (`&*archetypes.as_ptr().add(i)` → safe indexing в par-циклах);
    QueryCache poison-толерантность (`unwrap_or_else(into_inner)` — append-only кэш не бьётся
    паникой); `Pipeline::build` с незарег. системой — громкий `log::error`+skip вместо opaque
    unwrap-паники (регресс-тест).
  - `fb7630a` — атомарная запись сейвов (`WorldSerializer::atomic_write` temp+fsync+rename);
    `PrefabManifest::spawn` — громкая паника с именем префаба+причиной (trait возвращает Entity;
    try-путь = волна 6), `instantiate` get_info→Result. 2 регресс-теста.
  - `96a1d01` **E7** — `migrate()` на load-пути (`read_from_file`: read→migrate→restore); старый
    снапшот апгрейдится на загрузке или падает громко. Регресс-тест.
  - `4ca4865` — крейт-wide `allow(clippy::missing_safety_doc)` СНЯТ; 24 точечных `# Safety`-дока
    на каждую экспортируемую `unsafe fn` (Column/Archetype примитивы, WorldQuery/DenseQuery,
    from_ptr, ParallelWorld::get). Docs-only.
- **Мёртвый код (ВЫПОЛНЕН; каждое удаление под полный сьют, ~760 строк):**
  - `d2230ed` — SparseSet (весь storage-модуль, 0 консумеров, §10.3); ComponentMask+read/write_mask+
    assign_masks+conflicts_with_fast (C8 — assign_masks НИКОГДА не звался ⇒ маски всегда EMPTY ⇒
    conflicts_with всегда линейный fallback; поведение сохранено); Scheduler::
    archetype_indices_for_conflict_detection (C8, 0 консумеров, лживый rustdoc).
  - `cf822fd` **§10.5** — row-split 5.7 (prepare_sub_worlds/make_sub_world/*_storage/sub_worlds_dirty):
    ДОКАЗАНО мёртвым — storage читался ТОЛЬКО из make_sub_world, у которого НЕТ вызовов; prepare_sub_worlds
    молол вхолостую каждый рост мира. ЖИВОЙ row-split (ASD/run_stage_parallel через
    `SubWorld::from_raw_with_ranges`) НЕ тронут.
  - `51883b4` — allow(dead_code) sweep: SendPtr::as_mut/as_ref (deref прямой `&mut *task.ptr.0`),
    stale-allow min_entities_for_parallelism (жив), мёртвое поле parallel_threshold. Оставлены
    (не мёртвые): ParSystem static access()/name() (trait-API), AsdTask, add_par_system* (test-only).
  - QueryBuilder НЕ тронут (волна 6, §10.4).
- **ГЕЙТ волны 4 (ПРОЙДЕН ЦЕЛИКОМ 2026-07-03):** `cargo test --workspace` зелёный (0 падений);
  `cargo clippy --workspace` net-neutral (только 3 pre-existing apex-bench: unused_mut ×2 +
  type_complexity ×1; библиотеки чисты, снятый `allow(missing_safety_doc)` варнингов не дал);
  движок собирается (`cargo build --workspace` apex-engine ок); **Miri Tree Borrows apex-core
  --lib = 214 passed, 0 failed, 0 UB, 1 ignored** (`-Zmiri-disable-isolation -Zmiri-tree-borrows
  -Zmiri-ignore-leaks`); **goldens движка 656/656 байт-идентично** (рендер не изменён). Волны 0-4
  СМЕРЖЕНЫ и ЗАПУШЕНЫ (пользователь, 2026-07-03).

**Волна 5 🔄 — ПЕРФ + ложные Changed (ветка `core-audit-wave5` от main; правило кампании: каждый
пункт — A/B в одном прогоне, НЕ лендить неподтверждённую сложность):**
- `43a8fde` (apex-ecs) + `0abe27b` (apex-engine) **A13 ✅** — `World::get_mut`/`get_mut_by_id`/
  `EntityMut::get_mut` теперь возвращают `Mut<T>` с ЛЕНИВОЙ change-detection: тик стампится только
  при реальной мутации (DerefMut/set_changed), не на доступ. Раньше стамп был ЖАДНЫЙ → ложные
  `Changed<T>` для read-only get_mut (лишняя работа extract/propagate/picking каждый кадр).
  `Mut<T>` дерефается в `&mut T`: мутирующие сайты получили `mut`-биндинг (compiler-driven правка
  rustc-подсказками), read-only сайты не тронуты и теперь корректно НЕ метят Changed. Регресс-тест
  `a13_get_mut_is_lazy_about_change_detection`. Конструирование Mut — cell-провенанс как A2.
  **Гейт A13:** workspace зелёный; **goldens 656/656 байт-идентично** (A13 не изменил рендер —
  жадный стамп был чистым ложным срабатыванием); **Miri ЦЕЛЕВОЙ = 9 passed, 0 UB, 12.78с** (см.
  ниже про политику).
- **Miri-политика (решение пользователя 2026-07-03):** точечная правка (тип-обёртка, не трогает
  Column/Archetype/storage/планировщик) → Miri ТОЛЬКО на затронутых тестах (`-- <фильтры имён>`,
  ~секунды), а не полный `--lib` (~25 мин из-за rayon `dense_par_chunk`). Полный прогон — лишь при
  правке unsafe-ФУНДАМЕНТА (storage/Column/archetype/ASD/events/Resources). Детали в памяти
  `apex-miri-targeted-run`.
- **split-par_for_each (§7 headline) ✅ ВОССТАНОВЛЕН — эмпирика ОПРОВЕРГЛА предв. анализ:** предв.
  анализ ожидал нейтральности (равномерные бенчи), НО прямой A/B показал ВЫИГРЫШ. Реализован
  `par_utils::par_split_run_ranges` — рекурсивный divide-and-conquer поперёк архетипов через
  `rayon::join` (делит объединённое пространство строк пополам до листа=`adaptive_chunk_size`,
  process-замыкание инкапсулирует fetch → один runner для Query И CachedQuery). A/B в одном
  прогоне (дрейф-иммунно, `par_skew`-харнесс, ×3 воспроизв.): **uniform 7000 = +6.6…7.7%**
  (интервалы РАЗДЕЛЕНЫ), **skew ~7000 = +2…3%**, **heavy_compute 1000 = нейтрально** (±2% шум).
  КОРЕНЬ выигрыша — НЕ гранулярность (она та же), а бинарное `join`-дерево work-steal'ит
  эффективнее `par_iter`-над-коллекцией. Никогда не хуже: для `N<serial_threshold(192)` split =
  один лист = последовательный проход (0 overhead). Движок `par_for_each` НЕ использует → goldens
  вне риска. Корректность: `par_split_write_matches_sequential` + `visits_every_entity_once` на
  скошенном мире; целевой Miri TB на split-путь = 0 UB. compute_par_chunks остаётся (chunk/
  SubWorld-пути). Метод `par_for_each` теперь split; отдельного `par_for_each_split` НЕТ (слит).
- **Остаток волны 5:** пулинг per-frame буферов планировщика (schedule 42.1 vs bevy 40.8) · ленивая
  entity в par · bulk без двойного копирования · string-table снапшота · O(R²) диффы — каждый с
  отдельным A/B.
> **Охват:** все крейты воркспейса apex-ecs на HEAD `4ff7a0a` (apex-core 18.2k строк,
> apex-scheduler 7.2k, apex-serialization 2.2k, apex-scripting 1.9k, apex-graph, apex-isolated,
> apex-hot-reload, apex-macros, apex-bench, apex-examples; ~36k строк).
> **Метод:** 6 параллельных агентов глубокого чтения (каждый файл зоны целиком, все unsafe-блоки
> реестром, сверка с bevy_ecs-эталоном), верификация каждого утверждения памяти по коду
> (file:line), прогон полного тест-сьюта / clippy / criterion 3-way (apex+bevy 0.18+legion 0.4
> в одном процессе) / frag_world-стража / примеров. Код НЕ менялся (сессия анализа).

## 0. TL;DR

- **Тесты/clippy/бенчи/примеры здоровы:** весь воркспейс зелёный (0 падений), clippy
  библиотечных крейтов чист, примеры ядра 13/13 выполняются до конца (perf-демо вне прогона),
  criterion-standing совпадает с PERF_CAMPAIGN_2026-06 (6 побед / 6 паритетов / 4 отставания
  vs Bevy 0.18), frag_world в норме (5 архетипов, extract-цикл 211µs).
- **Но найдено 13 подтверждённых 🔴-дефектов уровня UB/потери данных**, большинство достижимы
  из safe-кода: дубликат компонента в bundle → drop по null; выравнивание CommandArena (align>8
  = все glam-типы); stale-lease → двойная выдача Entity; `mem::zeroed` для не-ZST RelationKind;
  UAF шаблона и моста IsolatedWorld; UB из Lua через подделку `_meta`; зависание
  propagate_transforms на циклах ChildOf; потеря событий при отстающем читателе; гонка курсоров
  Listen‖Listen; старвация FixedUpdate-систем; возврат TD-37-футгана через пустой access;
  дыра access у `Ctx` в `system!`.
- **Системная тема:** мутабельность через `&self`/`&World` без `UnsafeCell`/borrow-модели —
  формальный UB по правилам Rust (Miri уронит) и реальные дыры API (`q.get(e)` дважды = два
  `&mut` на один компонент; `Query<(&mut T, &mut T)>` компилируется). Ядро — «крепкий
  internal-core, не-sound как публичная библиотека». Нужна стратегическая развилка (см. §8.В1).
- **«Потерянная перф-работа» закрыта расследованием** (§7): потеря задокументирована ещё
  2026-06-17; col-кэш переимплементирован и эмпирически отвергнут; **реально потерян и стоит
  восстановления только adaptive rayon-split `par_for_each`**; закоммиченный код не
  регрессировал (свежие числа = стендинг кампании).
- **Гигиена:** ноль TODO/println в либах — чисто; но ~90/126 файлов с кириллицей (≈6.3k строк;
  движок мигрирован, ядро нет), пласт мёртвого кода (SparseSet, row-split 5.7, assign_masks,
  QueryBuilder), 3 файла-мусора в корне репо.
- План — 7 волн (§9): soundness → корректность → panic-safety/формальный UB → громкость §0.2a
  → перф → API/архитектура → EN/тесты/доки.

---

## 1. Сводка находок

Идентификаторы: буква = зона аудита (A world/storage, B entity/commands/relations/template,
C query/parallel/transform, D scheduler/graph, E isolated/serialization/hot-reload/scripting,
F macros/events). Уверенность: ✓ = подтверждено доказательным рассуждением/сценарием по коду,
? = подозрение (что подтвердить — в разделе подсистемы).

### 1.1 🔴 Критичные (UB / потеря данных / зависание)

| ID | Проблема | Место | Категория | Impact | Ув. |
|----|----------|-------|-----------|--------|-----|
| A1 | Дубликат компонента в bundle (компонент в кортеже И во вложенном Bundle) → колонка-фантом `len=0,data=null` → drop по null при despawn (release), порча данных | world.rs:2865-2872 (нет dedup), 1634-1654; archetype.rs:407-427 | soundness | UB из safe-кода; существующий тест создаёт порченый архетип, но не деспавнит | ✓ |
| B1 | CommandArena выравнивание: база align 8, а T с align 16 (glam Quat/Vec4/Mat4 — любой `cmd.spawn((Transform,…))` движка) → misaligned write/read | commands.rs:52-54, 70-73 | soundness | латентный UB на всём Commands-пути движка | ✓ |
| B2 | Stale ReserveLease: Commands держит Arc старой аренды через flush → повторная выдача одного Entity (две живые entity с одним id). Планировщик спасается только НЕдокументированным `*cmds = Commands::new()` | entity.rs:119-131, 209-236; scheduler lib.rs:2441, 2829 | soundness/баг | коррупция мира при переиспользовании Commands (легальный паттерн руководства) | ✓ |
| B3 | `mem::zeroed::<R>()` в relation-командах при неэнфорснутом ZST: `RelationKind` не требует ZST → kind с `&'static str` = нулевая ссылка = UB | commands.rs:449-491 | soundness | UB из safe-кода | ✓ |
| B4 | `spawn_from_template`: raw ptr на Box в HashMap + `template.spawn(&mut World)` → UAF при перерегистрации шаблона из своего же spawn | world.rs:3111-3123 | soundness | UAF (низкая вероятность, тривиальный фикс) | ✓ |
| C1 | SubWorld::new/with_ranges не привязывают входные лайфтаймы к `'w` → `SubWorld<'static>` из временных ссылок, dangling из safe-кода | sub_world.rs:42-71 | soundness | UB из safe-кода | ✓ |
| C2 | Нет проверки самоконфликта формы: `Query<(&mut T, &mut T)>`, `(Read<T>, Write<T>)` компилируются → алиасные `&mut` в одном item | query.rs:1204+ (Query::new без валидации) | soundness | UB из safe-кода (Bevy паникует) | ✓ |
| C4 | propagate_transforms: циклы ChildOf = вечный подъём/DFS без visited (зависание); мульти-родитель (модель разрешает!) = два потока пишут одну строку GlobalTransform (torn write 64Б) | transform.rs:520-528, 634-681; relations.rs:92-109, 602-620 | soundness/баг | зависание кадра; data race из safe-кода | ✓ |
| E1 | Lua-скрипт вызывает UB через подделку `_meta` (arch/row/col из Lua-таблицы → unsafe запись без валидации): OOB, type confusion, запись в чужую entity | apex-scripting/iterators.rs:253-305 | soundness | дыра в песочнице скриптинга | ✓ |
| E2 | UAF моста: `*const CloneableBridge` внутрь resources + Action с `&mut World` может удалить/заменить ресурс под собой | apex-isolated/lib.rs:373-385 | soundness | UAF | ✓ |
| F1 | `Events::update()`: при любом отстающем читателе НОВЫЕ события теряются для ВСЕХ (layout `[new,old]` + сдвиг курсоров на +new → новые позади всех курсоров) | events.rs:237-290 | баг | потеря данных; lag-ветка не покрыта ни одним тестом | ✓ |
| F2 | Гонка курсоров: `EventReader::new`→`add_reader` мутирует `Events::cursors` через raw ptr; Listen+Listen НЕ конфликт → две системы-читателя в одном пар-уровне пушат в один Vec | system_param.rs:150-195; world.rs:2038-2046; scheduler:3401-3434 | soundness | data race | ✓ |
| D1 | FixedUpdate: каждая exec-стадия дренирует аккумулятор → системы 2-й+ exec-стадии (любой конфликт physics→collision) НЕ исполняются никогда; семантика A×N,B×N вместо (A;B)×N | scheduler lib.rs:2790-2798, 2941-2949; fixed.rs:76-88 | баг | тихо «мёртвые» системы у пользователя | ✓ |
| D2 | Возврат TD-37-футгана: система с пустым access (`par()`, ZST-замыкание) → `SystemArchetypes::All` → entity_count = весь мир → ASD дробит → тело×чанки (пример из СОБСТВЕННОГО rustdoc config.rs:11 умножается) | scheduler lib.rs:2524-2528, 2619-2687 | баг | умножение сайд-эффектов | ✓ |
| F3 | `system!`: параметр `Ctx` не вносит НИЧЕГО в access, а SystemContext даёт resource_mut/event_writer/query произвольного Q → планировщик параллелит с чем угодно → молчаливые гонки | system_macro.rs:289-300; world.rs:1993-2011 | soundness | data race через макро-API | ✓ |

### 1.2 🔴/🟡 Системные (формальный UB, стратегические)

| ID | Проблема | Место | Impact | Ув. |
|----|----------|-------|--------|-----|
| A2 | Мутация change/added-тиков через ptr из `&self` БЕЗ UnsafeCell (set_change_tick/stamp_range) — формальный UB (Stacked/Tree Borrows), Miri уронит | archetype.rs:93-97, 108-114, 484-488 + потребители | латентный мискомпил-риск, блокер Miri-гейта | ✓ |
| A3 | `Resources::get_raw_ptr`: `&T→*mut T`+запись — тот же класс (коммент про «паттерн UnsafeCell» неверен — UnsafeCell там нет) | resources.rs:84-91 | то же | ✓ |
| A4 | Safe API раздаёт алиасящиеся `&mut`: `world.query::<Write<T>>(&self)` дважды; `q.get(e)` дважды; `event_reader/writer(&self)`→`&mut Events`; sub_world resource_mut | world.rs:1563, 797-816; query.rs:1382-1392; sub_world.rs:137-170 | эксклюзивность держится на дисциплине планировщика, не выражена в типах | ✓ |
| A5 | `pub` поля `World::archetypes`, `Archetype::{columns,entities,…}` — safe-код снаружи ломает все инварианты хранилища | world.rs:191; archetype.rs:397-403 | UB одним `entities.pop()` | ✓ |
| D3 | ASD row-split материализует алиасящиеся `&mut SystemDescriptor` в конкурентных тасках («не читаем байты» не снимает UB по правилам языка); U4 SendPtr+IndexMut — Stacked Borrows | scheduler lib.rs:2558-2569, 2708-2731 | блокер Miri; переработать на UnsafeCell/raw | ✓ |
| C7 | `WorldQuery` — публичный unsealed трейт: safe `has_row_filter()` с ложью снаружи → `unwrap_unchecked` на None → UB. Фикс: `unsafe trait` | query.rs:30-123 | UB-контракт наружу | ✓ |

### 1.3 🟡 Важные (баги поведения, §0.2a, panic-safety)

| ID | Проблема | Место | Ув. |
|----|----------|-------|-----|
| A6 | Panic-safety spawn_many/spawn_bundles_bulk: паника `make_bundle(i)` → `entities.len>col.len` → чтение неинициализированной памяти дальнейшими запросами (при catch_unwind) | world.rs:944-961, 1069-1081 | ✓ |
| A7 | Паника в пользовательском хуке навсегда отключает ВСЕ хуки (`hook_dispatch_active` не снимается) + очередь растёт молча | world.rs:551-608 | ✓ |
| A8 | `swap_remove_and_drop`: паника в Drop компонента → double-drop | archetype.rs:224-241 | ✓ |
| A9 | `insert_raw` на мёртвой entity ТЕЧЁТ T (байты дропаются без drop_fn); insert/remove на мёртвой — молчаливый no-op | world.rs:1137-1140, 1196-1199, 1335-1338 | ✓ |
| A10 | `spawn_at`/`spawn_reserved` повторно на живую entity → осиротевшая строка-дубль в итерации (нет ни assert, ни ветки) + B: generation-mismatch/чужой мир → ghost-строка (set_location молча no-op) | world.rs:851-892; entity.rs:388-399 | ✓ |
| A11 | `Column::grow/reserve`: realloc без checked_mul (первая ветка проверена, соседняя нет — §0.2b) | archetype.rs:288, 326 | ✓ |
| A12 | `CachedQuery::len/is_empty` игнорируют match-фильтр/row_ranges/построчные фильтры — врут для SubWorld-суперсетов, Without и Changed | world.rs:2384-2397; query.rs:1545-1551 | ✓ |
| A13 | `World::get_mut` стампит тик безусловно (vs ленивый `Mut` на DerefMut) → ложные Changed | world.rs:1533-1535 | ✓ |
| B5 | Осиротевшие резервации текут навсегда (drop/clear Commands без apply не возвращает слоты) | entity.rs:218-239 | ✓ |
| B6 | Standalone Commands (PLACEHOLDER): `.insert()` молча дропает компонент, `with_children` молча распадается | commands.rs:313-330, 765-768, 868-877 | ✓ |
| B7 | Молчаливые no-op команд на мёртвых entity (Despawn игнорирует false; §0.2a) | commands.rs:679-681; world.rs:1135-1140 | ✓ |
| B8 | `insert_raw`: нет валидации `data.len()` vs item_size → OOB-read из safe API; ZST-tick не стампится | world.rs:689-697, 1189-1238 | ✓ |
| B9 | ChildOf не эксклюзивен: `set_parent` ДОБАВЛЯЕТ второго родителя; self-relation разрешён (связано с C4) | commands.rs:786-791; relations.rs:590-620 | ✓ |
| B10 | Template-слой: `spawn_template("опечатка")` молча None; ошибка serde параметра молча теряет override | commands.rs:682-684; template.rs:146-148 | ✓ |
| C5 | Reparent не диртит трансформы: `add_relation(child, ChildOf, p2)` без мутации LocalTransform → GlobalTransform stale навсегда | transform.rs:472-498 | ? (проверить, компенсирует ли редактор движка) |
| C6 | Зоопарк query-API: Query/CachedQuery/QueryState/DenseQuery/QueryBuilder(мёртв); plain-fn системы НЕ используют QueryState — пересборка state на КАЖДЫЙ вызов системы | query.rs:1220-1264, 1779-1833 | ✓ |
| D4 | Протухание Filtered-индексов внутри кадра после sequential-стадий (recompute только в ASD-ветке) + run_sequential: один SubWorld на весь прогон; thread_commands OOB из rayon-воркера | scheduler lib.rs:2827-2910, 2393-2394; world.rs:1968-1970 | ✓ |
| D5 | WriteWrite + явный обратный `before()` = ложный CircularDependency (подавление explicit-порядком только для Bidirectional-ветки) | scheduler lib.rs:2246-2276 | ✓ |
| D6 | Change-окно стадии двигается при полностью пропущенной run_if-стадии → потеря Changed за период паузы | scheduler lib.rs:2416, 2798 | ✓ |
| D7 | `on_enter(initial)` не срабатывает для Update+ систем (вопреки rustdoc); двойной `init_state` молча ломает переходы | states.rs:66-91 | ✓ |
| D8 | Недетерминизм: порядок Custom-стадий = итерация FxHashMap (wasm32 иной); Commands per-thread → порядок apply/Entity ID варьирует между запусками (противоречит позиционированию independent()=replay) | scheduler lib.rs:1758, 1799, 2917-2919 | ✓ |
| D9 | Тройной копипаст исполнителя стадии (~120 строк; поведение УЖЕ разъехалось = D4) | scheduler lib.rs:2407-2451, 2799-2840, 2870-2911 | ✓ |
| F4 | EventReader = свежий курсор с позиции 0 на каждый запуск: FixedUpdate-катчап читает дубли; rustdoc `Removed<T>` «без дублей и пропусков» неверен для системного пути | system_param.rs:150 | ✓ |
| F5 | `EventIterator::Drop` создаёт `&mut Events` при живых `&T` наружу — кандидат UB (нужен Miri) | events.rs:556-583, 635-643 | ? |
| F6 | Два query-параметра `system!` молча биндятся к ОДНОМУ объединённому join-запросу | system_macro.rs:325, 469, 513, 653 | ✓ |
| F7 | derive без generics (все 3); Scriptable multi-field tuple = НЕкомпилирующийся код (мёртвая ветка с рождения); C-enum с явными дискриминантами ломает roundtrip | macros lib.rs:117, 182, 457-494, 581-600 | ✓ |
| E3 | `Box::leak` на КАЖДЫЙ write_resource/emit_event из Lua — линейная утечка | lua_api.rs:178, 190 | ✓ |
| E4 | `reapply_asset` схлопывает N инстансов префаба в 1, теряет parent/overrides | prefab_plugin.rs:249-298 | ✓ |
| E5 | Цикл префабов Ref→Ref = stack overflow | prefab.rs:166-214 | ✓ |
| E6 | Snapshot: НЕТ ремапа Entity-ссылок внутри компонентов (нет MapEntities-аналога) — ключ к статусу «snapshot=дифференциатор» | serializer.rs (системно) | ✓ |
| E7 | Снапшот не включает ресурсы/события (rustdoc обещает «всё»); migrate() НЕ вызывается на restore-пути (версионирование распылено) | snapshot.rs:59-63, 20-44; serializer.rs:530-558 | ✓ |
| E8 | restore: тип зарегистрирован, но без serde-fns → тихий continue (дроп данных снапшота); Lua-query незарег. типа → молча 0; незарег. Without молча отброшен | serializer.rs:259-262; scripting/iterators.rs:125-142 | ✓ |
| E9 | Debounce FileWatcher — фикция (poll_interval не действует на RecommendedWatcher/Windows); ошибка первичной загрузки молча отключает reload навсегда; неатомарная запись сейва | watcher.rs:32-53; plugin.rs:157-181; serializer.rs:512-523 | ✓ |
| E10 | despawn из Lua по голому index без generation → чужая entity; auto_commit теряет последнюю entity при break; Lua chunk перевыполняется каждый кадр | lua_api.rs:128-143; iterators.rs:320-331; script_engine.rs:404-407 | ✓ |
| E11 | apply_diff_to_snapshot: переиндексация added_entities рвёт added_relations/components | serializer.rs:469-489 | ✓ |
| E12 | `unsafe impl Sync for IsolatedWorld` с ложным обоснованием; unbounded-каналы моста без backpressure/warn; молчаливый дроп события при ошибке bincode | isolated/lib.rs:186-190, 61-62, 117-120 | ✓ |
| D10 | SystemBuilder — сырой `*mut Scheduler` без лайфтайма (pub-тип) | scheduler lib.rs:541-544 | ✓ |
| C8 | assign_masks/ComponentMask/conflicts_with_fast — мёртвый pub-API с лживым rustdoc (не вызывается нигде); + archetype_indices_for_conflict_detection мертва | access.rs:11-74, 247-280; scheduler lib.rs:1965-2000 | ✓ |

### 1.4 🟢 Мелочи (сводно; детали в разделах подсистем)

Storage/world: молчаливый tick-skip при row вне диапазона; SparseSet — мёртвый код с багом
iter_mut (крадёт dense) и O(n) remove; Resources двойная Box-индирекция; ComponentSerdeError
без `impl Error`; by_id HashMap→Vec; spawn_batch двойная буферизация + устаревший rustdoc;
QueryCache RwLock `.unwrap()` (poison); крейт-wide `allow(missing_safety_doc)`;
`make_bundle(0)` вызывается дважды (probe); filter_map-hardening col_indices;
`MainWorld: unsafe impl Send+Sync` — проверить необходимость.
Commands/entity: high-water u32 без стража; panic-safety apply (утечка арены); группировка
по адресу fn-указателя (ICF-хрупкость → TypeId); двойное копирование бандлов bulk-пути;
`*cmds = Commands::new()` в планировщике теряет арену/capacity; TargetIndex::remove O(N)
(осознанный компромисс — зафиксировать); TemplateParams тяжёлые.
Query: лишний unsafe `&*archetypes.as_ptr().add(i)` ×4; линейный row_range-скан; вечный
re-fill_ids при незарег. Maybe; Single validate→fetch двойное исполнение; static_assert на
Component: Send+Sync.
Scheduler: pipeline.rs:133 голый unwrap; `compile().expect` в run(); мёртвый row-split 5.7
(prepare_sub_worlds работает ВПУСТУЮ каждый рост мира); 13 `allow(dead_code)`; RunUntil/
EveryNFrames считают оценки, не кадры; populate_type_names ранний выход; диагностика
"system_N"; паника системы в пар-уровне не документирована (Startup повторится).
Events: EventCursor(pub u32); двойной remove_reader → дубль free_list; clear() слоты-сироты;
flush_all дубль; doc-ложь «не Sync»; DelayedQueue wrapping_add.
Serde/прочее: type_name String на инстанс (string-table v2); O(R²) diff; kind_name "<unknown>"
молча; UnknownFormat-вариант; кеш префабов сироты при переименовании; prefab spawn O(k²)
moves + HashMap-недетерминизм; EntityTemplate::spawn паникует expect.
Гигиена репо: results.txt (UTF-16 дамп) / perf.txt / apex-benc.txt закоммичены в корень;
кириллица ~90/126 файлов ≈6.3k строк; TODO/println в либах — НОЛЬ (чисто).

---

## 2. Хранилище: World / Archetype / Column / Resources

**Что есть.** Классический archetype-SoA: append-only `Vec<Archetype>`, Column = raw-буфер +
параллельные Vec тиков (change+added), идентичность архетипа — сортированный `ArchetypeKey` с
zero-copy lookup, переходы по add/remove-edges, swap_remove с релокацией вытесненной entity.
Change-detection двухтиковая с wrapping-сравнением и клампом MAX_CHANGE_AGE. Хуки — отложенная
очередь с re-entrancy-гвардом; required components поверх неё. Многие классические грабли уже
закрыты с регресс-тестами (отравление QueryCache, тики при swap_remove, col_indices
decl-order, ABA generation, tick-wrap).

**Что не так.** (1) A1 — единственный найденный в хранилище сценарий UB, достижимый из
обычного пользовательского кода. (2) Системно: тики и ресурсы мутируются через указатели,
выведенные из `&` (A2/A3) — работает, но формальный UB и блокер Miri. (3) Panic-safety
структурных путей слабая (A6/A7/A8). (4) Молчаливые no-op/утечки на мёртвых entity (A9/A10).

**Решения.**
- **A1 (S):** после сортировки ids в `spawn_at`/`spawn_many_inner`/`spawn_bundles_bulk` —
  `ids.dedup()`; при обнаружении дубликата `panic!("Bundle contains duplicate component <name>")`
  (семантика Bevy; «громко» по §0.2a, «кто выигрывает» не определяем). Плюс
  `debug_assert!(component_ids.windows(2).all(|w| w[0]!=w[1]))` в `Archetype::new`.
  Регресс-тест: spawn с дублем → паника с диагностикой; НЕ-дубли (тест
  `bundle_mixed_tuple_of_components_and_bundles`) — работают как раньше.
- **A2 (M):** `change_ticks/added_ticks: Vec<TickCell>` где
  `#[repr(transparent)] struct TickCell(UnsafeCell<Tick>)` + `unsafe impl Sync`. Сигнатуры
  `set_change_tick(&self)`/`stamp_range(&self)` не меняются — меняется законность записи.
  Альтернатива: тики в той же raw-аллокации, что данные (больше работы, выигрыш локальности —
  отложить до packed/SoA-кампании §8.В2).
- **A3 (S/M):** `FxHashMap<TypeId, UnsafeCell<Box<dyn Any + Send + Sync>>>` (и убрать двойную
  Box-индирекцию через трейт — она бесполезна).
- **A4/A5 — стратегическая развилка §8.В1** (borrow-модель). До неё — минимум (S): rustdoc-
  контракт на `World::query`/`event_reader`/`resource_mut` («write-формы из `&World` требуют
  внешней гарантии эксклюзивности») + `pub(crate)` на поля `World::archetypes` и
  `Archetype::*` (потребители движка — через существующие accessors; объём M).
- **A6 (M):** drop-guard в spawn_many/bulk (struct с Drop, обрезающий `arch.entities` до
  фактически записанных строк при unwind) — паттерн BundleSpawner Bevy.
- **A7 (S):** RAII-guard на `hook_dispatch_active` + `log::error` о недоставленных хуках.
- **A8 (S):** паттерн Vec::swap_remove — `ptr::read` в локал → копировать last → уменьшить
  len/тики → дропнуть локал последним.
- **A9 (S):** `insert_raw` на мёртвой — прогнать `drop_fn` + warn; `insert/remove` — warn
  (throttled) или возврат bool (унифицировать с remove).
- **A10 (S):** checked-`set_location` (bool) + в `spawn_at` debug_assert «нет локации» +
  громкая ветка в release; `EntityReserver` хранит world_id + debug_assert в apply.
- **A11 (S):** `checked_mul` + общий приватный `realloc_to(new_cap)` вместо двух копий.
- **A12 (S):** len/is_empty honor row_ranges + match_verified; для построчных фильтров —
  переименовать в честную семантику (`len_upper_bound`) или считать iter().count().
- **A13 (M):** `get_mut` возвращает `Mut<T>` (1:1 Bevy, ленивый стамп) — сайты движка
  переживут через Deref; свип call-sites обязателен.
- **SparseSet (S):** удалить целиком (мёртвый код с багом iter_mut) — держать полу-контейнер
  в pub API запрещает §0.2b. РЕШЕНО (§10.3): удалить; sparse-storage, если понадобится, —
  осознанным дизайном с тестами, не реанимацией.

**Гейт:** Drop-счётчик-тесты («ровно N дропов» на insert-поверх/despawn/move/spawn_many),
catch_unwind-тесты паник (hook/make_bundle/Drop), Miri-smoke зелёный на spawn/insert/query/
despawn, полный сьют + goldens движка байт-в-байт.

## 3. Entity-аллокатор / Commands / Relations / Template

**Что есть.** Аллокатор — generational-index + lock-free lease-резервация (дизайн продуманнее
Bevy: переиспользование слотов в reserve); W3-3 ретирование на gen==MAX (ABA исключён); live-
счётчик O(1). Commands — typed-enum ≤48Б (compile-страж) + bump-арена payload'ов, группировка
insert-бёрстов и spawn-бёрстов (bulk), EntityCommands/with_children 1:1 Bevy, строгий FIFO.
Relations — пара вне идентичности архетипа, SubjectIndex+TargetIndex, add_relation без
archetype move, каскад итеративным стеком (циклы/self-relation в каскаде НЕ зацикливаются —
проверено), generation-честность с тестами.

**Что не так.** Три 🔴 (B1 арена-align, B2 stale-lease, B3 zeroed-ZST) + UAF шаблона (B4);
PLACEHOLDER-семантика standalone Commands теряет данные молча (B6); ChildOf неэксклюзивен —
источник и UX-бага (два родителя), и 🔴 C4 в propagate.

**Решения.**
- **B1 (S-M):** арена трекает `max_align`; `alloc<T>` при `align_of::<T>() > max_align` —
  реаллокация с новым Layout (offsets валидны: каждый кратен своему align). Минимум на первый
  коммит: стартовый align 16 + `const`-ассерт `align_of::<T>() <= 16` с внятной ошибкой.
  Тест: spawn glam-типов (align 16) через Commands под Miri.
- **B2 (M):** единая ячейка аренды `Arc<ArcSwap<ReserveLease>>` (или RwLock), общая для
  аллокатора и всех EntityReserver — flush публикует новую аренду атомарно, stale невозможен
  по построению. ВМЕСТЕ с этим: планировщик переходит с `*cmds = Commands::new()` на
  `cmds.clear()` с сохранением арены/capacity (перф-бонус) — сейчас именно пересоздание
  случайно маскирует B2. Тест: переиспользуемый Commands + несколько apply при despawn-чёрне,
  ассерт уникальности id.
- **B3 (S):** `const { assert!(size_of::<R>() == 0) }` в трёх relation-командах (значение
  kind не читается) либо баунд `R: Default` + `R::default()`.
- **B4 (S):** `templates: HashMap<String, Arc<dyn EntityTemplate>>`; `get_arc().clone()` —
  заём отпущен до spawn.
- **B5 (S→M):** warn при drop/clear Commands с непотреблёнными резервациями; честный возврат
  (bitmap потребления в lease) — вторым шагом при появлении кейса.
- **B6 (S):** паника в EntityCommands-методах при PLACEHOLDER («standalone Commands не
  поддерживает chaining — привяжите reserver») — дешевле и честнее remap'а.
- **B7/B10 (S):** warn-волна §0.2a (см. волну 4).
- **B8 (S):** `assert_eq!(data.len(), info.size)` в insert_raw + стамп тика для ZST.
- **B9 + C4 (M, один заход):** `RelationKind::EXCLUSIVE: bool` (ChildOf=true) — add_relation
  заменяет существующую пару вида (O(1) через kind_mask); reject self-relation с warn;
  анти-цикл guard для каскадных/эксклюзивных видов (подъём с лимитом; цикл → warn + отказ)
  И страховочный visited-IndexStamp в propagate (переиспользовать scratch) + warn при
  повторном визите; предохранитель глубины в seed-подъёме. Тесты: цикл A→B→A (не виснет,
  warn), второй set_parent заменяет первого, despawn самопетли.
- **C5 — РЕШЕНО анализом 2026-07-03: документировать контракт, БЕЗ скрытого хука.** Проверка
  движка: команда редактора `Reparent` (apex-editor/src/commands/reparent.rs) УЖЕ корректно
  компенсирует — пересчитывает LocalTransform для сохранения мировой позиции (это диртит его →
  propagate отрабатывает). Слепой core on_add-хук был бы ХУЖЕ: он не знает трансформ нового
  родителя, поэтому мог бы лишь «тупо диртить», навязывая семантику ДРЕЙФА
  (`new_parent · old_local`) ВСЕМ вызывающим вместо их выбора preserve-world vs keep-local; к
  тому же диртил бы каждый ChildOf-add на спавне иерархий (лишний оверхед). Правильный
  golden-path: rustdoc `add_relation` явно проговаривает, что reparent сущности с трансформом
  НЕ пересчитывает LocalTransform автоматически — вызывающий решает (мутировать local для
  preserve-world, либо оставить как есть). §0.2a-громкость через документацию.

**Гейт:** зеркальный decl≠id регресс-тест для `spawn_bundles_bulk` (сейчас есть только для
spawn_many — логика продублирована, тест обязан быть продублирован); Miri на арену; тесты B2;
frag_world/criterion без регресса (арена и lease на горячем пути).

## 4. Query-подсистема / параллелизм / transform

**Что есть.** `WorldQuery` (state = raw-указатели per-archetype) + 4 фасада (Query, CachedQuery
с инкрементальным глобальным кэшем, per-system QueryState, DenseQuery) + рудиментарный
QueryBuilder. Горячие пути продуманы: ленивая entity, archetype-level fast-path
`has_row_filter` (контракт согласован по всем in-crate формам — проверено), инкрементальные
кэши без инвалидаций (append-only модель), match_verified. Параллелизм — предвычисленные
чанки + rayon; дизъюнктность соблюдена, state re-fetch на воркере. propagate_transforms —
dirty-корни + widen-then-descend + параллельная прямая запись (алгоритм соответствует памяти).

**Что не так.** C1/C2 — прямые safe-UB дыры; C4 — циклы/мульти-родитель (см. §3); C6 —
главный структурный перф/DX-долг: plain-fn системы пересобирают state запроса на каждый вызов,
а «правильный» QueryState живёт отдельным типом; adaptive rayon-split par_for_each потерян
(§7).

**Решения.**
- **C1 (S):** `pub fn new(world: &'w World, indices: &'w [usize]) -> Self` — привязать
  лайфтаймы (raw-указатели внутри остаются для Send/Sync); with_ranges — аналогично либо
  `unsafe fn`.
- **C2 → перенесена в волну 3, co-located с В1(б)** (решение при реализации 2026-07-03):
  полная детекция требует различать роли по позициям формы, но `AccessDescriptor::merge`
  дедуплицирует writes ⇒ write-write (`Query<(&mut T, &mut T)>`) в слитом дескрипторе невидим.
  Компайл-тайм-проверка только write-read — полумера (§0.2b) и потребовала бы проброса
  `Q: WorldQuerySystemAccess` через все конструкторы Query (широкий риск). Runtime
  borrow-флаги per-ComponentId (В1(б), волна 3) ловят И write-read, И write-write из ЛЮБОГО
  safe-пути (queries/get_mut) единообразно — это и есть полное решение.
- **C7 (S):** `pub unsafe trait WorldQuery` — согласованность has_row_filter/fetch_item
  становится частью unsafe-контракта. + макро-тест-переборщик «для каждой in-crate формы:
  has_row_filter()==false ⇒ fetch_item везде Some».
- **A12/A13, наход. 11 (лишний unsafe ×4), Single, row_range-скан, re-fill_ids** — россыпь
  S-фиксов в волне 4.
- **C6 (L, поэтапно — §8.В3):** единый пользовательский `Query<'w,'s>` поверх per-system
  QueryState, кэшируемого планировщиком в слоте системы (Bevy-модель); CachedQuery — во
  внутренности; DenseQuery — методами (`iter_chunks`); QueryBuilder — развилка §10.4
  (дорастить до dynamic query для редактора/скриптинга ИЛИ удалить). Это одновременно главный
  перф-выигрыш горячего пути систем (устраняет пересборку state на вызов) и главная DX-чистка.

**Гейт:** par_for_each получает ПРЯМЫЕ тесты (par≡seq на Write, par+Changed, par+row_ranges) —
сейчас их НОЛЬ в apex-core; Miri на query/dense/transform-тесты; criterion parity-гард.

## 5. Scheduler / graph

**Что есть.** Инкрементальный граф конфликтов → уровни Кана → exec-стадии по label +
apply_deferred → hybrid-исполнение с ASD row-чанкованием чистых систем (TD-37 гейты) +
per-stage change-окна (TD-52, ключ по exec-index — подтверждено) + independent() с
детерминированной сериализацией (подтверждено). Графовые алгоритмы корректны и
детерминированы.

**Что не так.** D1 (FixedUpdate-старвация — «тихо ломает прод»), D2 (возврат TD-37 через
пустой access), D4-D7 (корректность окон/состояний/индексов), D3 (формальный UB ASD-указателей),
D8 (недетерминизм против собственного позиционирования), D9 (тройной копипаст, уже давший
расхождения D4), мёртвая машинерия (row-split 5.7 работает впустую), per-frame аллокации
(D-Н15; ровно то, что PERF_CAMPAIGN §3.1B предлагала пулить).

**Решения.**
- **D1 (M):** дренаж аккумулятора ОДИН раз на кадр перед первой FixedUpdate-стадией; внешний
  шаговый цикл `for step in 0..steps { for stage in fixed_stages {…} }` — семантика (A;B)×N
  как Bevy. Тест: две конфликтующие FixedUpdate-системы (сегодня упадёт).
- **D2 (S+M):** системы с пустым компонентным access / `SystemArchetypes::All` → безусловно
  single-task (им нечего дробить по строкам); плюс громкий контракт: debug_assert/warn при
  `ctx.commands()`/`ctx.query` из системы, чей access этого не декларирует (связка с F3).
- **F3 (S, минимум):** `Ctx` в system! ⇒ `NEEDS_WHOLE_WORLD` + консервативный full-access.
  Целево (волна 6): убрать resource_mut/event_writer/произвольный query из публичного
  SystemContext (доступ только через декларированные SystemParam).
- **D9+D4 (M, одна связка):** единый `run_stage_sequentially()` вместо трёх копий + recompute
  архетип-индексов в конце КАЖДОЙ ветки стадии; SubWorld per-stage в run_sequential;
  `commands()` — кламп/инвариант thread_idx.
- **D5 (S):** для симметричных конфликтов сперва смотреть explicit_orderings (в любом
  направлении), ориентировать ребро по нему.
- **D6 (M):** не двигать `stage_last_run[idx]`, если ни одна система стадии не исполнилась.
  Полное решение — per-system last_run (Bevy-паритет) — оценить в волне 6.
- **D7 (S):** skip-флаг первого прогона в apply_state_transition; guard двойного init_state
  (warn + return).
- **D3 (M):** ASD-таски — без материализации `&mut SystemDescriptor`: UnsafeCell-обёртка/raw-
  вызов; SendPtr-цикл — split_at_mut или указатели без повторного IndexMut. Гейт — Miri.
- **D8 (M, если replay всерьёз — §10.7):** сортировка Custom-стадий по имени; команды
  per-SYSTEM (не per-thread), применение в порядке систем стадии.
- **D10 (S):** `SystemBuilder<'a> { scheduler: &'a mut Scheduler }`.
- **Мёртвое (S):** удалить prepare_sub_worlds/make_sub_world/storage/sub_worlds_dirty
  (row-split 5.7 не работает и жрёт время на каждом росте мира) — либо решение §10.5 довести;
  удалить/подключить archetype_indices_for_conflict_detection и assign_masks (C8); зачистить
  13 `allow(dead_code)`; pipeline.rs unwrap → ошибки.
- **Перф (M):** кэш стадийных метаданных в плане (не глубокая копия каждый run), пул
  sys_infos/tasks/skipped, `Commands::clear()` (см. B2), разделяемый all_indices. Ожидание:
  закрыть часть разрыва schedule 42.1 vs 40.8 (сегодняшний прогон — уже почти паритет).
- **Декомпозиция (M, волна 7):** lib.rs 5897 строк → registration.rs / compile.rs /
  executor.rs / debug.rs + вынос inline-тестов (~2400 строк) в tests/.

**Гейт:** новые тесты D1/D2/D5/D6/D7 + Miri scheduler (после D3) + criterion schedule не хуже.

## 6. Events / macros / serialization / isolated / hot-reload / scripting

**Events.** Архитектура зрелая (double-buffer, курсоры O(1) lagging, RAII-гварды, DelayedQueue),
НО центральный механизм «отстающий читатель» сломан (F1) и не покрыт ни одним тестом, а
реестр курсоров гоняется через raw ptr без синхронизации (F2).
**Решение (M, одна переработка):** (1) F1 — layout `[old_remainder…, new…]` без сдвига
курсоров (или сквозной счётчик как Bevy); (2) F2+F4 — курсор-per-system: персистентный
EventCursor в слоте системы у планировщика, `Events` на пути чтения только читается (Bevy-
модель state-in-system). Это чинит гонку, дубли FixedUpdate-катчапа и делает rustdoc-обещание
`Removed<T>` истинным. Мелочи E5-пакетом (приватный EventCursor, guard двойного remove_reader,
doc-фиксы). Гейт: тесты lag-ветки (old+new, порядок, caught-up при чужом lag), Miri events
(F5), стресс send_sync из потоков.

**Macros.** derive-трио аккуратное, но без generics (F7); Scriptable содержит мёртвую
некомпилирующуюся ветку и enum-рассинхрон; system! — гигиеничен кроме `apex_core::Commands`
(→`$crate`), M2-футгана двух query и F3 (Ctx). **Решения (S каждый):** compile_error guard на
второй query-параметр; `$crate::`; `format_ident!("f{}",i)` для tuple-Scriptable + тест;
`#ident::#variant as i64` для enum; generics — `split_for_impl` + lazy-регистрация (M);
trybuild ui-тесты на compile_error-тексты.

**Serialization** — лучшая подсистема зоны, свежие коммиты качественные. Дотянуть до
«образцового дифференциатора»: E6 MapEntities-аналог (`map_entities_fn` в ComponentSerdeFns +
автопрогон после шага 1 restore — карта уже есть; M/L), E7 ресурсы в снапшоте (opt-in
`register_resource_serde`, формат v2 со string-table type_name; M) + свести версионирование к
одному механизму read→migrate→restore (сейчас migrate() мёртв на пути загрузки), E8/E11 —
громкость и remap диффа, атомарная запись tmp+rename (S).

**IsolatedWorld** — прототип, не дифференциатор: E2 UAF (фикс: Clone-bridge через Arc-каналы,
unsafe исчезает — S), ложный Sync (S), нет Entity-ремаппинга между мирами, нет НИ ОДНОГО
кросс-поточного теста (единственный смысл крейта), мёртвые ассерты в существующих тестах.
Дотягивание до дифференциатора — отдельный заход волны 6: ремаппинг/протокол обмена + тесты
std::thread. Развилка приоритета — §10.6.

**Hot-reload:** E4 (per-instance контекст reapply — M), E9-пакет (честный debounce в poll(),
регистрация loader'а независимо от первичной ошибки, tmp+rename — S/M), тесты watcher/plugin
с нуля.

**Scripting — самое слабое звено ядра** (ноль тестов, максимум unsafe): E1 (валидация _meta:
ре-резолв col_idx + bounds + entity/generation-check, либо непрозрачный userdata-токен — M),
E3 leak (интерн — S), E10-пакет (64-битный id с generation в Lua; flush pending на break;
chunk исполнять один раз при load — S/M), E8-громкость. Плюс тест-кампания с нуля (round-trip
query→commit, forged _meta отклоняется, deferred spawn/despawn, ошибка Lua, hot-reload,
sandbox-изоляция). Развилка «вкладываться vs заморозить» — §10.8.

---

## 7. Перф-расследование: «потерянная работа» и текущий стендинг

**Хронология потери (по git + PERF_CAMPAIGN_2026-06.md §3.1A, подтверждено этим аудитом):**
сессия 2026-06-17 (IS_ARCHETYPAL, arch_cols/fetch_state_cached, par_for_each→rayon-split,
heavy_compute-редизайн, «закрытия» fragmented/or_iter) велась в незакоммиченном дереве и
потеряна безвозвратно (ни коммита, ни stash, ни dangling — проверено тогда же). Закоммиченный
пласт ЖИВ: `87b6e2a` has_row_filter/fetch_item_unchecked fast-path, `de322f4` enum-shrink ≤48Б,
match_verified, lazy-entity, despawn_recursive O(n)-фикс, col_indices UB-фикс.

**Статус каждой потерянной единицы:**
- `IS_ARCHETYPAL` — **не восстанавливать**: роль закрыта живым has_row_filter fast-path
  (это и есть archetype-level filter).
- `arch_cols` (кэш индексов колонок) — **не восстанавливать**: был честно переимплементирован
  2026-06-17 и эмпирически отвергнут (wash: индексация кэша ≈ стоимость 2-элементного скана;
  замеры дрейф-доминированы). Урок зафиксирован в PERF_CAMPAIGN §3.1A: «не делать наивный кэш
  повторно». Структурный путь — packed/SoA (§8.В2).
- **`par_for_each` → adaptive rayon-split — ЕДИНСТВЕННОЕ, что реально стоит восстановить.**
  Подтверждено: в коде живёт фикс-чанкинг `compute_par_chunks` (par_utils.rs:13,
  query.rs:1504/1517, world.rs:2339/2357) — гранулярность заморожена до rayon. Потерянный
  вариант (legion-стиль `rayon::iter::split` c work-stealing) по записям давал 660µs vs bevy
  707 на heavy_compute и был архитектурно правильным фундаментом. Восстанавливать
  ЭМПИРИЧЕСКИ (не по памяти): реализовать split-producer заново → A/B на heavy_compute/
  wide_iter в одном прогоне → лендить только при воспроизводимом ≥нейтральном результате.
- heavy_compute-редизайн бенч-тел — уже не нужен (текущее тело стабильно, см. числа ниже).
- **Бимодальность par_for_each** (good ~660µs / bad ~3ms) — расследование 2026-06-17
  исключило 8 гипотез, корень сужен до per-element data-path ECS-query, НЕ найден (нужен
  сэмплирующий/HW-профайлер, на Windows-CLI недоступен). В сегодняшнем прогоне heavy_compute
  стабилен (567µs, без выбросов ×4) — возможно, бимодальность была связана с исправленным
  col_indices-UB. Держать под наблюдением; отдельно не атаковать без появления симптома.

**Свежий 3-way стендинг (этот аудит, 2026-07-03, один процесс — ratio дрейф-иммунны):**

| Бенч | apex | bevy 0.18 | legion 0.4 | Вердикт |
|------|:----:|:---------:|:----------:|---------|
| simple_insert | 324 µs | **307** | 192 | 🟡 −5% |
| simple_iter | 8.81 / dense **6.41** | 8.82 | 5.89 | паритет |
| fragmented_iter | 182 ns | **142** | 176 | 🔴 ×1.28 (бьём legion) |
| schedule | 42.1 µs | 40.8 | **30.1** | ≈паритет (был −8%) |
| heavy_compute | 567 µs | 551 | **436** | паритет |
| add_remove | **530 µs** | 897 | 2914 | 🟢 ×1.7 |
| commands_spawn | **447 µs** | 492 | — | 🟢 |
| commands_insert | 539 µs | **486** | — | 🟡 −10% |
| despawn | **244 µs** | 291 | 504 | 🟢 |
| despawn_recursive | **23.5 µs** | 53.0 | — | 🟢 ×2.25 |
| get_component | 37.3 µs | 36.3 | 51.5 | паритет |
| changed_iter | **7.26 µs** | 7.31 | — | паритет+ |
| events | **13.9 µs** | 22.4 | — | 🟢 ×1.6 |
| relations | 638 µs | 632 | — | паритет |
| wide_iter | 3.63 µs | 3.55 | 2.22 | паритет |
| propagate | 322 µs | — | — | apex-страж, норма |

Совпадает со стендингом кампании — **регресса от потери нет**; отставания те же и прежней
природы (fragmented — диффузный prologue, structural путь = packed/SoA; commands_insert —
archetype-move bound, bulk-перенос уже отвергнут замером; simple_insert — цена change-ticks).
frag_world-страж: 5 архетипов, extract-цикл 211µs — норма.

**План возврата (волна 5):** (1) восстановить split-par_for_each по процедуре выше;
(2) пулинг per-frame буферов планировщика (D-Н15 — ровно PERF_CAMPAIGN §3.1B); (3) ленивая
entity в par-путях (сейчас только в seq); (4) B-Н16 bulk-спавн без двойного копирования.
Каждый пункт — отдельный A/B-замер в одном прогоне; правило кампании: не лендить
неподтверждённую сложность.

---

## 8. Стратегические развилки (волна 6, каждая — отдельное решение)

**В1. Borrow-модель публичного API (A4/A5, C-1).** Сейчас эксклюзивность write-доступа — чистая
дисциплина планировщика; safe-код может получить два `&mut` на компонент. Три уровня:
(а) **S** — честные rustdoc-контракты + `pub(crate)` поля (делаем безусловно, волна 1/4);
(б) **M** — runtime borrow-флаги per-ComponentId (атомарные счётчики, debug-режим,
shipyard-стиль) — ловит нарушение в тестах/дев-сборках, не ломает API;
(в) **L** — Bevy-модель: `UnsafeWorldCell`, конструкторы write-запросов из `&mut World`,
unsafe-эскейп для планировщика, `get_mut/iter_mut` через `&mut self`. Ломает API движка —
объём сравним с adoption-проходом. Рекомендация: (а)+(б) сейчас; (в) — совместить с В3, ибо
оба ломают одни и те же сигнатуры (один migration-проход движка, не два).

**В2. Packed/SoA storage (legion-стиль, один блок на архетип).** Единственный структурный
ответ на fragmented_iter (×1.28) и кандидат-фикс исторической бимодальности. Большой рефактор
Column/Archetype; A2 (тики в той же аллокации) — естественная часть. Делать ТОЛЬКО при
подтверждённом ROI на тихой машине (урок arch_cols); сейчас — не блокер (микро-шейп 26×20
нерепрезентативен, реальные нагрузки в паритете).

**В3. Консолидация query-зоопарка (C6).** Единый `Query<'w,'s>` поверх per-system QueryState
в слоте системы; главный горячий выигрыш plain-fn пути. Связана с В1(в) — решать вместе.

**В4. Snapshot/IsolatedWorld до статуса дифференциаторов.** MapEntities (E6) + ресурсы (E7) +
version-путь; IsolatedWorld: Entity-ремаппинг, bounded-каналы, кросс-поточные тесты. Это
прямое требование анти-mimicry §0.9: наши козыри обязаны быть образцовыми, а не прототипами.

---

## 9. План работ волнами

Нумерация НЕ означает «строго друг за другом» — волны 1-2 можно вести параллельно по разным
крейтам; внутри волны порядок = приоритет. Каждая волна — один или несколько коммитов ядра;
пуш apex-ecs ДО движка; после КАЖДОЙ волны — гейт.

**Волна 0 ✅ — инфраструктура гейтов** (выполнено `138f0c1`; Miri-smoke как обязательный гейт
отложен до волны 3 из-за блокера linkme↔Miri, см. Журнал волн). Мусор удалён, план в репо,
Miri установлен.

**Волна 1 — SOUNDNESS: достижимый UB из safe-кода (P0; S/M-фиксы).**
A1 dup-bundle → panic+dedup · B1 арена-align · B3 zeroed-ZST assert · B2 stale-lease
(+`cmds.clear()` в планировщике) · B4 template Arc · C1 SubWorld лайфтаймы · B8 insert_raw
size-assert · E1(Lua _meta валидация) · E2 (Clone-bridge) · D10 SystemBuilder лайфтайм ·
C7 unsafe trait WorldQuery.
Зависимости: B2 вместе с B-Н15. **C2 перенесена в волну 3** (co-located с В1(б) runtime
borrow-чекером — там полная детекция write-write+write-read, см. §4).
*Гейт: новые регресс-тесты на каждый пункт (вкл. Miri на арену/dup-bundle), полный сьют,
clippy net-neutral, criterion+frag_world без регресса, goldens движка байт-идентичны.*

**Волна 2 — КОРРЕКТНОСТЬ поведения (P0/P1).**
D1 FixedUpdate-старвация · D2+F3 пустой access/Ctx → single-task+full-access · F1 lag-ветка
Events · F2+F4 курсор-per-system · B9+C4+C5 ChildOf EXCLUSIVE + анти-цикл + reparent-dirty ·
D9+D4 единый исполнитель стадии + recompute индексов · D5 explicit-порядок для симметричных ·
D6 окно при пропущенной стадии · D7 states-фиксы · A7 hook-guard · E4 reapply per-instance ·
E5 цикл префабов · E9 watch_config-регистрация · E3 Lua leak · F6 guard двух query ·
F7 Scriptable tuple/enum · E10 Lua id c generation.
*Гейт: тесты на каждый пункт (вкл. «сегодня упадут»: две FixedUpdate-системы, цикл ChildOf,
lag-reader); примеры ядра 14/14 до конца; goldens движка.*

**Волна 3 — PANIC-SAFETY + формальный UB (P1).**
A6 drop-guard spawn-путей · A8 swap_remove порядок · A2 TickCell(UnsafeCell) · A3 Resources
UnsafeCell · D3 ASD без алиасящихся &mut · F5 EventIterator (по вердикту Miri) · B-Н17 арена
panic-guard · **В1(б) runtime borrow-флаги per-ComponentId (debug-режим)** — инструмент,
делающий aliasing-нарушения ловимыми тестами до целевой модели В1(в) · catch_unwind-тест-пакет.
*Предусловие гейта: снять блокер linkme↔Miri — `cfg(miri)` форсирует ленивую регистрацию
компонентов (как wasm-путь TD-25), иначе distributed_slice даёт overflow на любом тесте с
`World`. Гейт: Miri-smoke становится ОБЯЗАТЕЛЬНЫМ гейтом коммита ядра (расширенный сьют:
query/dense/transform/events/commands); goldens движка.*

**Волна 4 — §0.2a ГРОМКОСТЬ + мёртвый код (P2).**
Warn-волна: B7/B10/A9/A10/E8/E12(bincode-drop)/S15/S23/D-«молчаливые» — единый стиль
(throttled `log::warn` с entity/типом и причиной). Мёртвое (решения §10.3/§10.5): удалить
SparseSet, row-split 5.7 (prepare_sub_worlds/make_sub_world/storage), assign_masks+
ComponentMask, archetype_indices_for_conflict_detection, 13 allow(dead_code), протухшие
атрибуты. QueryBuilder НЕ удаляется — дорастает в волне 6 (§10.4). Гигиена: A11
checked_mul, A12 len/is_empty, лишний unsafe ×4, QueryCache poison, pipeline unwrap,
prefab expect→Result, атомарная запись сейвов, версионирование migrate()-путь, крейт-wide
allow(missing_safety_doc) → точечные SAFETY-доки.
*Гейт: clippy строже (снятые allow), сьют, grep-чек «молчаливых no-op» по чек-листу аудита.*

**Волна 5 — ПЕРФ (P2; каждый пункт с A/B в одном прогоне).**
Восстановление split-par_for_each (§7) · пулинг буферов планировщика + кэш метаданных стадий ·
ленивая entity в par · B-Н16 bulk без двойного копирования · A13 get_mut→Mut (ложные Changed
— тоже перф) · string-table снапшота (E-S21) · O(R²) диффы.
*Гейт: criterion 3-way — нет регресса ни в одной группе >5% (шум), целевые группы улучшены
воспроизводимо; frag_world; many_foxes движка A/B; goldens.*

**Волна 6 — АРХИТЕКТУРА/API (P3; все развилки решены — §10).**
**В1(в)+В3 одним migration-проходом** (UnsafeWorldCell + `&mut World`-конструкторы write-путей
+ единый `Query<'w,'s>` поверх per-system QueryState) · В4 дифференциаторы: snapshot
(E6 MapEntities + E7 ресурсы/версии) И IsolatedWorld (ремаппинг, bounded-каналы,
кросс-поточные тесты) · QueryBuilder → полноценный dynamic query (консумеры: инспектор
редактора, скриптинг, agent-IPC) · D8 детерминизм (per-system command-буферы + сортировка
Custom-стадий) · F7 generics в derive · make_bundle → честный `Bundle::static_component_ids` ·
D6-полное (per-system last_run — оценить по факту после D6-минимума) · SystemContext ужатие
(F3 целевое).
*Гейт: migration-проход движка одним заходом, adoption-стиль; полный сьют оба репо; goldens;
руководство ядра актуализировано.*

**Волна 7 — EN-МИГРАЦИЯ + ТЕСТ-КАМПАНИЯ + ДОКИ (P3/P4).**
Кириллица ≈6.3k строк: (1) user-facing литералы (compile_error!/panic/expect/log) — сразу;
(2) rustdoc/комментарии по-крейтово (core→scheduler→остальные), механические коммиты в
.git-blame-ignore-revs. Тест-дыры по реестрам агентов (scripting с нуля; isolated
кросс-поточные + живые ассерты; serialization round-trip relations/error-path/fuzz;
hot-reload watcher; events lag/threads; макро trybuild; par-пути core). Декомпозиция
scheduler/lib.rs. Rustdoc-полировка + CHANGELOG. Руководство пользователя ядра — сверка
после всех волн.
*Гейт: grep-ноль кириллицы в *.rs (решено: да, §10.2); tests count↑; руководство компилируется.*

---

## 10. Развилки — РЕШЕНИЯ (приняты 2026-07-03; критерий — золотой путь §0.2a/б, §0.9, §0.10)

1. **Borrow-модель → ДА, целевая модель (в) целиком; поэтапно (а)→(б)→(в).**
   (а) rustdoc-контракты + `pub(crate)` поля — волны 1/4; (б) runtime borrow-флаги
   per-ComponentId в debug — волна 3 (инструмент, делающий нарушения ловимыми до целевой
   модели); (в) UnsafeWorldCell + `&mut World`-конструкторы write-путей — волна 6, ОДНИМ
   migration-проходом с В3. Обоснование: «крепкий internal-core, не-sound как библиотека» —
   это полумера (§0.2b); ядро — публичная поверхность движка, редактора и скриптинга,
   AAA-планка = soundness, выраженная в типах. Анти-mimicry-чек пройден: паттерн Bevy тут не
   компенсация отсутствующих у нас фич, а идиоматически корректный Rust — перенимаем.
2. **Кириллица в ядре → ДА, ноль кириллицы в *.rs apex-ecs** (≈6.3k строк, волна 7;
   user-facing литералы — раньше, волной 4). Код экосистемы един; правило движка
   распространяется на ядро дословно. `.md`-документы остаются русскими.
3. **SparseSet → УДАЛИТЬ** (волна 4). Мёртвый код с подтверждённым багом iter_mut и без
   единого потребителя — ровно то, что §0.2b запрещает держать в pub API. Будущий
   sparse-storage (если появится ROI) — осознанный дизайн с нуля, не реанимация.
4. **QueryBuilder → ДОРАСТИТЬ до полноценного dynamic query** (волна 6). Консумеры уже
   существуют: инспектор редактора (запросы по именам типов), agent-IPC/MCP, скриптинг —
   который сегодня строит СВОЙ динамический путь поверх ядра (после E1-фикса обязан встать
   на общий, валидируемый ядром механизм). Удаление было бы анти-стратегично: editor/agent —
   наш дифференциатор, dynamic query — его фундамент.
5. **Row-split 5.7 → УДАЛИТЬ мёртвую машинерию** (волна 4). Фича не работает сегодня и жрёт
   время на каждом росте мира; реальный row-split УЖЕ делает ASD (run_stage_parallel).
   «Одинаковые системы делят строки архетипа» — без консумера и без замеренного ROI;
   §0.2b требует доводить нужное, а не всё, что когда-то начато.
6. **IsolatedWorld → ДОТЯГИВАТЬ до дифференциатора** (волна 6: Entity-ремаппинг/протокол
   обмена, bounded-каналы с телеметрией, кросс-поточные тесты, живые ассерты). Прямое
   следствие §0.9: IsolatedWorld назван структурным козырем проекта — козырь-прототип хуже
   отсутствия козыря. Замораживание противоречило бы стратегии анти-mimicry.
7. **Детерминизм (D8) → ДА, заявленная цель, не nice-to-have** (волна 6: per-system
   command-буферы в порядке систем стадии + сортировка Custom-стадий по имени).
   `independent()` уже ПРОДАЁТ детерминизм для replay/netcode — недетерминированные Entity ID
   делают это обещание ложью (§0.2a). Бонус: детерминизм — дешёвая страховка
   воспроизводимости golden-тестов движка.
8. **Scripting → ВКЛАДЫВАТЬСЯ, с гейтом на расширение.** Довести СУЩЕСТВУЮЩЕЕ до полного
   стандарта: soundness/утечки/громкость (E1/E3/E8/E10 — волны 1-2), тест-кампания с нуля +
   EN (волна 7), пересадка на общий dynamic query (волна 6, п.4). НОВЫЕ фичи — только вместе
   с реальным консумером редактора (gameplay-скрипты/assistant). Это полный §0.2b для того,
   что есть, без спекулятивного роста.
9. **FixedUpdate → ДА, семантика (A;B)×N** (Bevy/стандартный game-loop; волна 2 = фикс D1).
   Слом «A×N, потом B×N» безопасен: текущее поведение никем не может осмысленно
   использоваться — вторая exec-стадия просто мертва (старвация). Физический конвейер
   (integrate→collide→resolve на каждый шаг) корректен только в новой семантике.
10. **make_bundle-контракт → ЧЕСТНОЕ решение**: `Bundle::static_component_ids` без
    конструирования значения (волна 6; derive/кортежи знают состав статически, ручные impl —
    документированный fallback на probe). До волны 6 — временная rustdoc-пометка о двойном
    вызове `make_bundle(0)` (волна 2, строка в A-Н11). Полумера «только задокументировать»
    отклонена: контракт «замыкание должно быть чистым» — скрытый футган.

---

## Приложение: верификация утверждений памяти агента (код ↔ память)

| Утверждение памяти | Вердикт |
|---|---|
| Lease-аллокатор (TD-39), ретирование gen==MAX (W3-3) | ✓ подтверждено (entity.rs:89-239, 357-368) + найдена дыра B2 |
| Страж size_of::<Command>()≤48, spawn_bundles_bulk, FIFO | ✓ (commands.rs:197-200, 557-589) |
| col_indices decl-order фикс + регресс-тест | ✓ для spawn_many (world.rs:928, тест :3778); bulk-путь БЕЗ зеркального теста |
| despawn_recursive делегация в despawn | ✓ (relations.rs:775-790) |
| Relations: 2 индекс-вставки, каскад стеком, warn на мёртвых | ✓ (relations.rs:227-409, 590-620; world.rs:1369-1457) |
| TD-37 гейты ASD | ✓ частично — дыра пустого access (D2) |
| independent() детерминированный | ✓ (lib.rs:1396-1421, 2199-2242) |
| TD-52 per-stage окно по exec-index | ✓ (lib.rs:2334-2344) + 2 оговорки (D6) |
| has_row_filter/fetch_item_unchecked контракт | ✓ согласован по всем in-crate формам |
| match_verified | ✓ корректен во всех трёх режимах |
| par_for_each на rayon::iter::split | ✗ ПАМЯТЬ ВРЁТ — в коде фикс-чанкинг (работа потеряна, §7) |
| propagate widen-then-descend + parallel write | ✓ алгоритм; ✗ контракт дизъюнктности нарушаем циклами/мульти-родителем (C4) |
| «Часть перф-работы потеряна» | ✓ подтверждено и закрыто расследованием §7 |
