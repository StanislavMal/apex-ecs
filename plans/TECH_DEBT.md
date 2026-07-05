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

- **S1 🔴 — write-аксессоры принимают `&self` → алиасящий `&mut` из safe-кода (доказано PoC).**
  `Query::get/get_mut/iter/for_each/single` берут `&self` (`crates/apex-core/src/query.rs:~1589-1673`;
  комментарий у `get_mut` «`&self` достаточно» неверен); то же у `CachedQuery`
  (`world.rs:~2706,2934`), который вдобавок `#[derive(Clone)]` (`world.rs:2559`) + Send/Sync →
  клонируемый write-запрос гоняется из двух потоков. PoC (три строки safe-кода): `new_mut::<Write<T>>`
  → двойной `q.get(e)` → два алиасящих `&mut T` на одну строку. Тот же класс:
  `SubWorld::resource_mut(&self)` (`sub_world.rs:190`) — вообще без `_unchecked`-суффикса. *Золотой
  путь:* `&mut self` на write-аксессорах (модель Bevy `get_mut`/`iter_mut`), снять `Clone` с
  write-варианта CachedQuery; read-аксессоры остаются `&self` (Bevy-трюк: read-методы подставляют
  `Q::ReadOnly`). Гейт: PoC перестаёт компилироваться; Miri TB; goldens byte-identical.
- **A5 🔴 — сырая pub-поверхность хранилища (аудит-A5, «делаем безусловно волны 1/4» — ВЫПАЛ;
  найден повторно API-инвентаризацией 2026-07-05).** `World.archetypes: Vec<Archetype>` и
  `World.resources: Resources` — **pub-ПОЛЯ** (`world.rs:198/213`): `world.archetypes.clear()` из
  safe-кода ломает все инварианты хранилища. Туда же: `Scheduler::run_sequential(*mut World)` — pub с
  сырым указателем; `Resources::get_raw_ptr` (`resources.rs:~159`); `World::event_queue_ptr`;
  `compute_archetype_indices`/`populate_type_names` (фазы compile — pub). *Фикс:* pub(crate) + фасады
  для легальных консумеров (движок через существующие аксессоры). Одна волна с S1/S2
  (`plans/active/API_GOLDEN_PATH.md` волна 1).
- **S2 🔴 — `SystemContext::fetch::<P>()` — safe-обход F3/ADR-002.** `ctx.fetch::<ResWrite<B>>()`
  (`world.rs:2466`) — публичный документированный safe-метод к `_unchecked`-семейству без декларации
  доступа → недекларированный live-write, обесценивает `#[doc(hidden)]`-стратегию ADR-002 (в ADR не
  учтён). *Путь:* гейтить `fetch` по декларации либо перевести в `_unchecked`/`#[doc(hidden)]`.
- **S3 🟡 — `World::event_writer/event_reader(&self)` = гонка из safe-кода.** `world.rs:~913-933`
  мутируют event-очереди через `&World`; `World: Sync` (unsafe impl для `ResourceCell`
  `resources.rs:17` / `EventQueueCell` `events.rs:1053`) → два потока с `&World` в safe-коде. Тот же
  класс, что ADR-002 закрыл на `SystemContext`, но на самом `World` оставлен.
- **S4 🟡 — недекларированный `ctx.event_reader` мутирует реестр курсоров вне conflict-детекции.**
  `ctx.event_reader` (`world.rs:2427`) благословлён как «read», но `EventReader::new`→`add_reader`
  пишет в реестр курсоров (push/realloc, `events.rs:163-178`). Для ДЕКЛАРИРОВАННЫХ читателей гонку
  закрывает F2 (`SharedEventReaders`), но недекларированный ctx-путь планировщик не видит. Связан с F4.

---

## 🟡 Бездомные пункты реестра аудита (числились, но не сделаны/не отложены явно)

- **F4 🟡 — per-system event-курсоры не персистентны.** `EventReader` как SystemParam:
  `type State = ()` (`system_param.rs:479`), каждый `fetch` зовёт `Events::add_reader()` →
  курсор `Some(0)` (`events.rs:167`) → свежий курсор с нуля каждый запуск. Следствие: FixedUpdate-
  катчап читает дубли; rustdoc `Removed<T>` «без дублей и пропусков» (`events.rs:~1002-1004`) — ложь.
  **Инфраструктура готова:** В3 Phase A (`SystemParam::State`/`get_param`, `system_param.rs:418`) —
  ровно тот per-system слот, для которого F4 проектировался (сейчас `State` не использует НИКТО).
  F4 теперь дёшев. Дважды переадресован (волна 2→6→6б) и оба раза выпал. Детали: CORE_AUDIT §1.3/§6.
  **Дом исполнения: `plans/active/API_GOLDEN_PATH.md` волна 4** (события+ошибки — курсоры на State-слоте).
- **D6-полное 🟡 — per-system `last_run` (Bevy-паритет change-окон).** Окно по-прежнему
  per-execution-stage (`stage_last_run: Vec<Tick>`, `apex-scheduler/src/lib.rs:~740-747, 2449-2454`);
  волна 2 закрыла только run_if-кейс. Значился в исходном scope волны 6б, исчез при формировании плана.
- **B5 🟡 — утечка Entity-резерваций при drop/clear Commands без apply.** `Commands::drop`
  (`commands.rs:~925`) и `clear()` (`commands.rs:~761-786`) дропают payload арены, но не варнят о
  непотреблённых резервациях и не возвращают слоты (тест-свидетель
  `len_counts_only_located_ignoring_orphaned_reservations`, `entity.rs:742`). TD-40 починил счётчик,
  не утечку id-пространства. Решение §3 («warn при drop/clear + честный возврат») не исполнено.
- **B6 🟡 — standalone Commands (PLACEHOLDER) молча теряет chained insert/with_children.**
  `EntityCommands::insert` ставит `Insert{entity: PLACEHOLDER}` (`commands.rs:~809-813, 338`).
  Смягчено косвенно (spawn аллоцирует id на apply; insert-на-PLACEHOLDER попадает в A9-warn), но
  данные chained-insert теряются. Принятое решение §3 («паника в EntityCommands при PLACEHOLDER»)
  не исполнено и не пересмотрено письменно.
- **В4 🟡 — IsolatedWorld не дотянут до дифференциатора.** Решение §10.6 = ДА (Entity-ремаппинг между
  мирами, bounded-каналы с телеметрией, кросс-поточные тесты). Сделано: только громкость дропа (E12,
  волна 4). Каналы моста unbounded, ремаппинга нет. Тестовая часть — в scope волны 7; ремаппинг/
  bounded-каналы бездомны. Прямое требование §0.9 (козырь-прототип хуже отсутствия козыря).
- **§10.8 🟡 — Lua-путь не пересажен на общий DynQuery.** Решение (волна 6 п.4): скриптинг «обязан
  встать на общий, валидируемый ядром механизм». Факт: `apex-scripting/src` — ноль `DynQuery`;
  `iterators.rs` — прежний собственный unsafe-путь через `_meta` (`:~301, 426`; E1-валидация есть,
  но доступ свой). DynQuery (волна 6) сделан и назвал скриптинг консумером, но миграция не выполнена.

## 🟢 Бездомные — чистота/эргономика/док-честность

- **ParallelPolicy 🟡 — обещан планом, не реализован; комментарий в коде лжёт.** План PARALLELISM §3.1
  обещал тип-политику (`ParallelPolicy`) с `Fixed`-фолбэком для отключения cost-model при патологии
  EMA. Не реализовано: пороги — захардкоженные `const`, ручки нет (только `parallel_min_entities` как
  пол). Комментарий `apex-scheduler/src/lib.rs:451-452` утверждает «`ParallelPolicy::Fixed` remains
  available» — **ложь в коде** (§0.2a). *Минимум:* исправить комментарий (не обещать несуществующее)
  + записать, что cost-model всегда on после прогрева. *Полно:* реализовать `Fixed`-откат. См. ADR-003.
- **guide-broken 🟡 — ≥7 классов некомпилирующихся/ложных примеров руководства** (аудит руководства
  2026-07-05; полный реестр со строками — `plans/active/API_GOLDEN_PATH.md` §2). Классы: (1) F3.1:
  ~10 × `ctx.query::<...Write...>` (нужна развилка Р-1 плана: публичный `ctx.query_mut` vs
  `system!`-only); (2) F3.2: §6.7+§16 учат снятым `ctx.resource_mut/try_resource_mut/event_writer`;
  (3) §6.8: все 3 Extract-примера используют маркеры как plain-fn параметры; (4) §15 «Полный пример»
  без `#[derive(Component)]`; (5) §4 `query_changed` с Write (а `query_mut_changed` не задокументирован
  вовсе); (6) §17 Lua: entity id как integer (E10 отверг — id строка "index:generation");
  (7) §11: `load_directory` 3 аргумента + миф про расширение `.prefab` (код берёт только
  `*.prefab.json`). Плюс STALE-пласт: версия «0.3.0» vs 0.1.0, §13.2 формула чанка другой эпохи,
  §10 молчит про v2/migrate/E6/E7, §16 прячет write-путь запросов и `set_deterministic_spawn`,
  ~10 гнилых file:line-ссылок. *Фикс = волна 5 кампании API_GOLDEN_PATH* (переписывание руководства
  ПОСЛЕ стабилизации API — иначе дважды). Cost-model-секции (§1/§13.1.1/§13.1.3/§14.4/§14.9-нота)
  уже исправлены 2026-07-05.
- **string-table снапшота 🟢 — `type_name` per-instance.** `serializer.rs:142` пишет
  `info.name.to_string()` на каждый инстанс компонента (E7-формат v2 string-table не включил).
  Выигрыш — только в РАЗМЕРЕ сейва (редкий путь), отдельный focused-заход.
- **D9 🟢 — тройной копипаст исполнителя стадии.** `run_sequential`/`run_stage_parallel`/
  `run_hybrid_parallel` (`apex-scheduler/src/lib.rs:~2526/2686/2987`) — три копии, поведение уже
  разъезжалось (D4-класс). Волна 7 содержит декомпозицию scheduler/lib.rs — D9 туда поимённо.
- **§1.4-хвосты 🟢 — гигиена, не сделана волной 4.** `EventCursor(pub u32)` — всё ещё pub
  (`events.rs:761`); `by_id` — FxHashMap, не Vec (`component.rs:337`); `MainWorld unsafe impl
  Send+Sync` — «проверить необходимость» не журналировано (`world.rs:~2112-2113`); `TargetIndex::remove`
  O(N)-скан без обещанного коммента-компромисса (`relations.rs:~372-388`).
- **DynQueryMut item-гейт 🟢 (S7/S8).** `DynItemMut::get_mut/get_mut_ptr` (`query.rs:~2711,2725`) не
  проверяют вхождение id в декларированные `writes` — декларация влияет только на матчинг архетипов,
  `AliasedWrite`-проверка при lending декоративна; для agent-IPC/скриптинга политику «что можно
  писать» придётся enforcить слоем выше (S7). Нет Changed/Added-термов у динамического билдера —
  инспектору для change-polling остаются полные сканы (S8). Гейтить в item либо явно задокументировать.

---

## Крупные отложенные развилки (с обоснованием §0.2b — дом здесь)

- **C6 🟡 — консолидация query-зоопарка** (аудит-C6 ядра, НЕ движка). Единый `Query<'w,'s>` поверх
  per-system QueryState (Query/CachedQuery/QueryState → один тип). Переоценён в волне 6б: атомарный
  кросс-репо рефактор ВСЕГО query-usage (движок не собирается до конца миграции), риск лайфтаймов
  'w/'s, перф маргинален/плоский (arch-индексы уже zero-copy из SubWorld). Ценность = API-качество.
  **Spike-first:** изолированный lifetime-спайк `Query<'w,'s>` → атомарная миграция. Детали: CORE_AUDIT
  §8.В3, шапка архива WAVE6B. Здесь же живут В3 Phase B/C (реальный кэш-State) и leaf-sizing Ş2b.
  **→ Взят в scope кампании `plans/active/API_GOLDEN_PATH.md` (волна 1b/2):** инвентаризация 2026-07-05
  усилила ROI по обучаемости — три копии метод-сета, ~35→12 pub fn, 7 конструкторов → 2+эскейп.
  **✅ Спайк 2026-07-05: GO** — прототип `Query<'w,'s,D,F>` (UnsafeWorldCell + `StateSrc{Owned|Borrowed}`
  + `type ReadOnly`-проекция) скомпилирован+исполнён; лендится БЕЗ В3; сливается с S1-аксессорами в
  одну работу. Детали — API §5 «Спайк C6 — результат».
- **Ş5 / Ş6 / В2 🟡 — перф-спайки кампании PARALLELISM (ROI-gated).** **Ş5** dense-by-default для
  infallible-запросов (vs-Legion итерация 0.59–0.68×; goldens-рискован — семантика change-стампинга
  per-deref vs range, байт-identity как жёсткий гейт). **Ş6** heavy_compute leaf-sizing (0.83× vs
  Bevy; pow-of-2-дисбаланс `par_split_run_ranges` на малых N; профайлер → num_threads-way split под
  A/B-гардом). **В2** packed/SoA storage (fragmented_iter/relations структурно; ROI-гейт =
  many_foxes-доказательство, НЕ микробенч — ставит под удар структурные победы add_remove/despawn/get).
  Детали: ADR-003, архив PARALLELISM §4.5/4.6/§8.В2.
- **D8b-overflow 🟢 — детерминированный overflow блока (фронтир).** При overflow блока в кадре
  система падает в недетерминированный путь тот кадр, затем блок растёт. Rank-ordered
  детерминированный overflow — по спросу (после прогрева не наступает). Детали: ADR-001, entity.rs:183-185.
- **Волна 7 🟡 — EN-миграция + тест-кампании + декомпозиция.** ~95 `.rs`-файлов с кириллицей
  (комментарии/rustdoc; user-facing литералы — раньше) → ноль кириллицы в `*.rs` (правило движка
  дословно, §10.2). Тест-дыры: scripting с нуля, isolated кросс-поточные + живые ассерты,
  serialization round-trip relations/error-path, hot-reload watcher, events lag/threads, макро
  trybuild, par-пути core. Декомпозиция `scheduler/lib.rs` (~5.9k строк) → registration/compile/
  executor/debug + вынос inline-тестов. Планируемая волна CORE_AUDIT §9 — дом есть, дублируется здесь
  как индекс.
