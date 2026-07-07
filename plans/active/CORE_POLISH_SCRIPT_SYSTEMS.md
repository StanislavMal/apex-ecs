# Кампания CORE_POLISH + SCRIPT_SYSTEMS — хвосты TECH_DEBT, безусловный детерминизм, скриптинг на золотом пути

> **Статус: 🚧 в работе (старт 2026-07-06). Волны 0, 1 и 2 ✅ ЗАКРЫТЫ.** Волна 0: 0.1–0.5 сделаны,
> 0.6 string-table переоценён §0.2b (отдельный заход, 🟢 открыт). Волна 1: 1.1 D8b-overflow (эскроу),
> 1.2 D6-полное (per-system change-окна), 1.3 детерминизм-гейт. Волна 2 (event-модель): 2.1 S3
> (`World::event_*`→`&mut self`+`_unchecked`), 2.2 S4 (`ctx.event_reader`→`_unchecked`, blessed=param),
> 2.3 F4b (⚠ переоценка §0.2b — macro-персистентность неисполнима чисто, golden path=plain-fn). Goldens
> 649/0/9 байт-идентичны на всех трёх волнах. Далее — волна 3 (скриптинг/DynQuery). Источник истины
> кампании. Статусы пунктов — ЗДЕСЬ; `plans/TECH_DEBT.md` держит указатель сюда для взятых в
> работу пунктов. Ветка: `core-polish-script-systems` (заведена 2026-07-06 от HEAD
> `api-golden-path`, НЕ от main — main отстаёт на 71 коммит и не содержит фундамента волны 7,
> D9 и самого реестра TECH_DEBT; ветвление от main потеряло бы основу кампании).
> Планка: `docs/CONVENTIONS.md` §0.2a (громко, не молча), §0.2b (без полумер / письменная
> переоценка), §0.9 (анти-mimicry — козыри: relations, no-loss events, IsolatedWorld, детерминизм).
> Канон API: `decisions/ADR-004` (Р-1 plain-fn = golden path). Эталон-исходник Bevy:
> `C:\My\Projects\Rust_projects\bevy` (читать/грепать, не вспоминать).
>
> **Гейты каждого шага (без исключений):** workspace-тесты apex-ecs зелёные · clippy
> `--all-targets` net-neutral · Miri (targeted по затронутым unsafe-путям; полный `--lib` — при
> правках unsafe-фундамента) · движок: `cargo build --workspace` + полный
> `cargo test -p apex-render --lib --features visual_tests`, goldens **byte-identical** ·
> пушить ядро ДО движка · коммиты `git commit -F <файл>`.

## 0. TL;DR

Четыре волны, строго по возрастанию риска — сперва честность и уже принятые решения, затем
фундамент (детерминизм, event-модель), затем скриптинг ПОВЕРХ готового фундамента:

- **Волна 0 — честность/гигиена ✅:** закрыть всё из `plans/TECH_DEBT.md`, что либо уже фактически
  сделано (реестр отстал от кода), либо имеет принятое-но-неисполненное решение (B5/B6), либо
  является ложью в коде (§0.2a: ParallelPolicy). Малый риск, высокая отдача. **Итог: 5/6 закрыты
  кодом; string-table переоценён §0.2b (полноценная эволюция формата, не «минор») → отдельный заход.**
- **Волна 1 — детерминизм до безусловной гарантии ✅:** D8b-overflow (эскроу-margin + громкий
  фолбэк) + D6-полное (per-system change-окна, Bevy-паритет) + run-to-run детерминизм-гейт. Всё
  закрыто, goldens 649/0/9 байт-идентичны. Побочно: записан pre-existing Miri UB query-пути (MIRI-CD).
- **Волна 2 — event-модель:** распространить модель персистентных per-system курсоров (F4)
  на остальные пути — S3/S4/F4b. Предпосылка волны 3.
- **Волна 3 — скриптинг на золотой путь:** §10.8 (Lua → DynQuery, закрывает S7/S8), затем
  Lua-системы как первоклассные системы планировщика с динамическими декларациями доступа.
  Дизайн-развилка «Arc<Mutex<VM>>» **отклонена** — см. §5 (дизайн-нота, при ротации → ADR).

Вне scope кампании (осознанно, §0.2b): В4 IsolatedWorld (ремаппинг+bounded — кандидат на
СЛЕДУЮЩУЮ кампанию-дифференциатор), В3 Phase B/C (кэш-QueryState), перф-спайки Ş5/Ş6/В2
(ROI-gated, только с профилем), кросс-платформенный/FP-детерминизм (ADR-001 — не заявляем).

---

## 1. Волна 0 — честность и быстрые закрытия TECH_DEBT

> Принцип волны: каждый пункт — АТОМАРНЫЙ коммит со своим тестом; никакой пункт не тянет
> архитектурных решений (всё уже решено ранее — только исполнение и актуализация реестра).

### 0.1 Актуализация реестра — A5-остаток фактически ЗАКРЫТ кодом (только запись)

Верификация 2026-07-06 показала: ВСЕ пункты «остатка A5» уже сделаны в коде, запись в
`plans/TECH_DEBT.md` устарела:

| Пункт записи A5 | Факт в коде |
|---|---|
| `run_sequential(*mut World)` → `&mut World` | уже `&mut World` — `apex-scheduler/src/executor.rs:98` |
| `Resources::get_raw_ptr` — сырая pub-поверхность | уже `pub(crate)` — `apex-core/src/resources.rs:142` |
| `World::event_queue_ptr` | уже `pub(crate)` — `apex-core/src/world.rs:967` |
| `compute_archetype_indices` (фаза compile) | уже `pub(crate)` — `apex-scheduler/src/compile.rs:184` |
| `populate_type_names` | уже `pub(crate)` — `apex-scheduler/src/registration.rs:978` |

**Как:** закрыть запись A5 в TECH_DEBT (✅ с датой и ссылкой на верификацию), одновременно
перенаправить стейл-ссылки реестра на мёртвые волны («→ волна 3/4» ротированных кампаний) на
эту кампанию: S3/S4 → волна 2 здесь, B5/B6 → волна 0 здесь, §10.8/S7/S8 → волна 3 здесь.
**Гейт:** ноль ссылок из TECH_DEBT на несуществующие дома.

### 0.2 ParallelPolicy — реализовать `Fixed` (сейчас комментарий в коде лжёт, §0.2a) ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `pub enum ParallelPolicy { CostModel, Fixed }` + `set_parallel_policy`/`parallel_policy`;
> `Fixed` минует EMA (entity-пороги + явный пол); развилка вынесена в чистую `stage_prefers_seq`
> (юнит-тест `parallel_policy_fixed_vs_cost_model`); лживый комментарий исправлен; addendum ADR-003.
> Гейты: 101 scheduler-тест, clippy net-neutral.

**Проблема:** `apex-scheduler/src/lib.rs:475-476` — комментарий обещает
«`ParallelPolicy::Fixed` … remains available as a fallback», типа `ParallelPolicy` в коде НЕТ;
cost-model нельзя отключить при патологии EMA (план PARALLELISM §3.1 обещал ручку). Правка
только комментария — полумера (§0.2b): фолбэк был обещан пользователю модели.

**Как (полное решение):**
- `pub enum ParallelPolicy { CostModel, Fixed }` (default = `CostModel`) +
  `Scheduler::set_parallel_policy()`.
- `Fixed` = только существующий entity-пол (`parallel_min_entities`), минуя EMA-гейты
  (`T_STAGE_SEQ_NS` и связанные пороги cost-model, `lib.rs:~451-478`); точки ветвления —
  в диспатче стадий (см. `executor.rs`, решение seq/par).
- Комментарий переписать под реальность; addendum к `decisions/ADR-003` (одна строка статуса:
  «Fixed реализован, кампания CORE_POLISH волна 0»).
- **Тесты:** unit — под `Fixed` стадия с «холодной»/патологической EMA всё равно уходит в
  параллель при N ≥ порога и в seq при N < порога; под `CostModel` поведение неизменно
  (существующие тесты диспатча зелёные без правок).

### 0.3 B5 — утечка Entity-резерваций при drop/clear `Commands` без apply ✅ 2026-07-06

> **✅ ЗАКРЫТО.** Общий `AbandonQueue` (Arc: аллокатор + резерверы) с atomic-флагом `pending`
> (fast-path flush остаётся lock-free); `EntityReserver::abandon` кладёт индексы на drop/clear,
> `EntityAllocator::flush` дренит в free-list без gen-инкремента (как `reclaim_block_tail`);
> `Commands::abandon_queued_reservations` + `warn_once!`. Тесты entity+commands; Miri чист
> (19+28); 256 core-тестов; clippy net-neutral. Уточнение к дизайну: `reclaim_block_tail`
> напрямую переиспользовать нельзя (Drop не имеет `&mut World`) — тот же ПРИНЦИП (возврат без
> gen-bump), но через общий канал drop→flush.

**Проблема:** `Commands::drop` (`apex-core/src/commands.rs:~925`) и `clear()` (`:~761-786`)
дропают payload, но молча теряют непотреблённые id-резервации (слоты не возвращаются,
id-пространство растёт). Решение принято ранее («warn + честный возврат»), не исполнено.

**Как:**
- При `drop`/`clear` с непустыми неприменёнными резервациями: **вернуть слоты** в free-list
  арены БЕЗ gen-инкремента (механизм уже существует — `reclaim_block_tail`-семейство из D8b,
  `entity.rs`; переиспользовать, не дублировать).
- **Громкость:** у `Commands` нет `&World` в scope → `ErrorHandler` недостижим; использовать
  `warn_once!`-класс с точным текстом (сколько резерваций потеряно-возвращено). Это НЕ
  ErrorHandler-хвост (тот остаётся отдельной 🟢-записью), а минимально честная громкость.
- **Тесты:** (а) `spawn().id()` → drop без apply → следующий `reserve` переиспользует те же
  слоты (id-пространство не растёт); (б) `clear()` аналогично; (в) существующий свидетель
  `len_counts_only_located_ignoring_orphaned_reservations` (`entity.rs:742`) обновить под новую
  семантику (резервации больше не «orphaned»).

### 0.4 B6 — standalone `Commands`: PLACEHOLDER молча теряет chained insert ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `EntityCommands::assert_bound(op)` (`#[track_caller]`) — паника с actionable-текстом
> на ВСЕХ цепочечных методах (insert/remove/add_relation/set_parent/add_child/add_children/
> remove_parent/clear_children/with_children/despawn). Полнее плана: не только insert/with_children —
> любой отложенный op на PLACEHOLDER. `cmd.entity(real)` и `spawn((full_bundle))` не затронуты;
> `ChildSpawner` защищён транзитивно. Тесты should_panic + позитивные; 260 core-тестов, clippy
> net-neutral.

**Проблема:** `EntityCommands::insert` на standalone-Commands ставит
`Insert{entity: PLACEHOLDER}` (`commands.rs:~809-813, 338`) — данные chained-insert теряются
(A9-warn ловит косвенно). Принятое решение «паника в EntityCommands при PLACEHOLDER» не
исполнено и письменно не пересмотрено.

**Как:** исполнить принятое решение — `panic!` с внятным EN-сообщением в `EntityCommands::insert`
/`with_children` при PLACEHOLDER-id («standalone Commands cannot chain inserts on spawn();
apply the Commands to a World or use world-attached Commands»). Паника — потому что это
детерминированная ошибка программирования (не data-driven путь).
**Тесты:** `#[should_panic]`-тест на insert-после-spawn у standalone; позитивный тест — тот же
код через world-attached Commands работает.

### 0.5 §1.4-хвосты — гигиена (4 микропункта одним заходом) ✅ 2026-07-06

> **✅ ЗАКРЫТО.** (1) `EventCursor` поле → `pub(crate)`; (2) `by_id` `FxHashMap`→`Vec<ComponentInfo>`
> (ids плотные, O(1) индекс, `iter()` детерминирован по id); (3) `MainWorld Send+Sync` journaled
> (нужен для resource-хранилища; sound через sequential extract); (4) `TargetIndex::remove`
> коммент-компромисс O(N)-fan-in. 260 core-тестов, clippy net-neutral.

| Пункт | Где | Как |
|---|---|---|
| `EventCursor(pub u32)` — pub-поле | `events.rs:770` | поле → приватное; конструктор/геттер `pub(crate)`; проверить, что внешних потребителей нет (grep обоих репо) |
| `by_id` — FxHashMap вместо Vec | `component.rs:337` | `ComponentId` — плотный индекс; заменить на `Vec<Option<…>>`; бенч не нужен (структурно очевидно), тесты реестра зелёные |
| `MainWorld unsafe impl Send+Sync` | `world.rs:~2112-2113` | провести аудит необходимости, ЖУРНАЛИРОВАТЬ вывод в doc-комментарии над impl (инвариант, из-за которого это sound); если не нужен — удалить impl |
| `TargetIndex::remove` O(N)-скан | `relations.rs:~372-388` | добавить обещанный коммент-компромисс (почему O(N) приемлем: размер типичного target-списка, где сломается) — самодостаточный инвариант, без ссылок на живые планы |

### 0.6 string-table снапшота — `type_name` per-instance (размер сейва) ⚠ ПЕРЕОЦЕНКА §0.2b 2026-07-06

**Проблема:** `apex-serialization/src/serializer.rs` пишет `info.name.to_string()` на КАЖДЫЙ
инстанс компонента (полный Rust-путь ~30-50 байт × число инстансов).

**⚠ ПЕРЕОЦЕНКА (§0.2b — полное решение ИЛИ переоценка, не полумера).** При реализации вскрылось,
что «формат-минор с чтением предыдущей версии» — НЕ минорная правка, а полноценная эволюция
формата, несоразмерная «быстрому закрытию» волны 0 и с реальным риском для on-disk сцен редактора.
Разбор по коду:
- **Единственный size-критичный путь — bincode** (JSON = debug/human-readable, не про размер).
  bincode ПОЗИЦИОННЫЙ: `#[serde(default)]`/`skip_serializing_if` игнорируются, любое изменение
  структуры → несовместимость с прежними байтами (подтверждено комментарием `snapshot.rs:78-80`
  «Bincode v1 is not compatible with v2»). Значит аддитивный serde-трюк (Option-поле + default)
  работает ТОЛЬКО для JSON — мимо цели.
- **Чистое решение — version-gated wire-типы + version-peek:** приватные `WireSnapshotV3`
  (`string_table: Vec<String>` + компоненты/релейшены/ресурсы с `name_idx: u32`), конверсии
  in-memory↔wire, диспетч на чтении по ведущему полю `version` (первое и в bincode-LE, и в JSON).
  In-memory `WorldSnapshot` НЕ меняется (string-table — чисто wire-концерн) — сериализатор/
  десериализатор/diff не трогаются. Правильный дизайн, но:
- **Блокер — версионная машинерия в двух представлениях:** `WorldSnapshot::CURRENT_VERSION` (u32)
  И `SnapshotVersion::CURRENT` (major/minor) — рассинхронизированы уже сейчас; бамп 2→3 должен
  пройти через обе + `migrate()` (2→3 no-op на уровне struct) + `restore()`-чек версии +
  `is_version_compatible` (major-сравнение → v2 стал бы «несовместим», хотя должен грузиться через
  migrate). Untangle двух схем — отдельная задача, не «хвост гигиены».

**РЕШЕНИЕ: вынести в отдельный focused-заход «snapshot format v3 (string-table)»**, НЕ в волну 0.
Дизайн выше — готовый спек. Предзадача: свести две версионные схемы (u32 `version` vs
`SnapshotVersion`) к одной. Тесты будущего захода: v3 round-trip; чтение v2-фикстуры (байты старого
формата → restore); «N инстансов одного типа → имя в файле ровно раз»; fuzz/roundtrip волны 7
зелёные. Запись в TECH_DEBT остаётся ОТКРЫТОЙ (🟢) с этим спеком.

### Гейт волны 0 ✅ ПРОЙДЕН (2026-07-06)

Полные гейты шапки; TECH_DEBT актуализирован (A5 ✅, ParallelPolicy ✅, B5 ✅, B6 ✅, §1.4 ✅ —
каждый со ссылкой на коммит; string-table ⚠ переоценён-отложен с дизайн-спеком, остаётся 🟢 открыт);
ноль записей долга-упрощения (переоценка string-table — не упрощение, а честный перенос полного
решения в соразмерный заход, §0.2b). **Итоговые гейты:** apex-core 260 lib + весь workspace зелёный,
clippy net-neutral, Miri чист на затронутых unsafe-путях (B5); **движок `cargo build --workspace` ✅,
goldens `visual_tests` 649/0/9 БАЙТ-ИДЕНТИЧНЫ** (B6-паника и by_id-порядок движок не задели). 7 коммитов
на ветке `core-polish-script-systems` (НЕ запушено — пуш по решению юзера).

---

## 2. Волна 1 — детерминизм до безусловной гарантии

> Дом решений: ADR-001 (D8b). Текущая граница: гарантия «безусловна в steady-state» — при
> overflow блока система падает в недетерминированный путь ТОТ кадр. Цель волны — убрать
> оговорку «steady-state» и закрыть D6-полное (change-окна), плюс поставить детерминизм под
> постоянный тест-гейт.

### 1.1 D8b-overflow — rank-ordered детерминированный overflow блока ✅ 2026-07-06

> **✅ ЗАКРЫТО (упрощение дизайна vs план — к лучшему).** Реализация: эскроу — НЕ отдельный
> сегмент/резерв, а **margin на сеемый блок** (`seed_size_for` = `block_size_for` peak×2 + половина
> блока). Механизм тот же, что у блока (доказан контентон-фри и rank-детерминированным), поэтому
> отдельный «эскроу-пул» не нужен — overflow в margin автоматически тянет приватные детерминированные
> id. За пределами блок+эскроу: `BlockCursor.overflowed`-флаг → `warn_once!` + счётчик
> `deterministic_overflow_count` (имя `deterministic_overflow_fallbacks` из плана → короче). Реклейм
> эскроу-хвоста = существующий `reclaim_block_tail` (не дублируется). Спайк-first подтверждён
> тестами: `escrow_keeps_spike_within_margin_deterministic` (спайк 300 > блок 256, ≤ seed 384 →
> детерминирован; без эскроу 3 системы гоняли бы 44 overflow-id на общем reserve) +
> `overflow_beyond_escrow_is_loud_and_still_correct` (500 > 384 → счётчик > 0). Miri чист (20
> entity), 101 scheduler + все d8b, clippy net-neutral. Addendum ADR-001 + руководство §6.6a.

**Дизайн (спайк-first, как D8b) — исходный план (реализация упростила эскроу до block-margin):**
- **Эскроу-пул:** при посеве блоков стадии (`World::reserve_entity_block`, main-поток,
  rank-порядок) планировщик дополнительно выделяет **rank-упорядоченный эскроу-резерв**
  (доля от суммы блоков, адаптивная по `system_spawn_history`, как сами блоки). Overflow
  системы ранга R берёт слоты из СВОЕГО эскроу-сегмента приватным счётчиком — без
  cross-thread contention, id остаются rank-детерминированными и eager.
- **Второй уровень (эскроу тоже исчерпан):** громкий недетерминированный фолбэк КАК СЕЙЧАС,
  но с телеметрией (§0.2a): счётчик `deterministic_overflow_fallbacks` в диагностике
  планировщика + `warn` с именем системы; следующий кадр блок И эскроу растут.
- Семантика reuse эскроу-хвоста — та же, что у блоков (`reclaim_block_tail`, без
  gen-инкремента): id-пространство ограничено под churn.
- **Переоценка §0.2b:** если спайк покажет, что эскроу не удерживает инвариант
  «eager = final = детерминированный» без contention — письменная переоценка здесь, а не
  тихая полумера.

**Тесты:** форс-overflow (стартовый блок 1-2 слота, спайк спавна) → присвоение id идентично
между многократными прогонами (методология «40/40» спайка D8b); телеметрия срабатывает только
при исчерпании эскроу; goldens движка не трогаются (дефолт OFF неизменен).
**Док:** руководство §6.6a — граница гарантии обновляется («безусловна, включая спайки в
пределах эскроу; фолбэк громкий»); ADR-001 — addendum-строка.

### 1.2 D6-полное — per-system `last_run` (Bevy-паритет change-окон) ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `SystemContext.last_run` per-system (`with_last_run`, дефолт = world-тик →
> every-frame системы байт-идентичны, goldens не сдвигаются); `ctx.query`/`query_unchecked`/`Query`-
> SystemParam берут per-system baseline; `Scheduler::system_last_run` + `system_window`/
> `advance_system_windows` (апдейт только после рана, не при skip); заврайрено в 3 исполнителях
> (`run_sequential`, `run_hybrid_parallel`, `run_stage_parallel` через `AsdTask.last_run` в rayon-таск).
> Тест `co_stage_gated_reader_sees_changed_from_pause` (co-stage gated reader видит Changed из паузы —
> per-stage окно давало 0). 102 scheduler-lib + parity/determinism зелёные; clippy net-neutral.
> Осталось: Miri по change-detection + engine goldens (гейт волны 1).

**Проблема:** окно change-detection — per-execution-stage
(`stage_last_run: Vec<Tick>`, `apex-scheduler/src/lib.rs:~740-747, 2449-2454`); волна 2
CORE_AUDIT закрыла только run_if-кейс. Система, пропущенная планировщиком/условием, видит
изменения относительно чужого окна.

**Как:** `last_run: Tick` per-system (слот в per-system state — инфраструктура В3 Phase A уже
есть); при диспатче система получает СВОЁ окно `(system_last_run, current_tick)`; обновление —
после фактического рана (skip НЕ двигает окно). Сверить с эталоном построчно:
`bevy_ecs/src/system/function_system.rs` (`SystemMeta::last_run`) — это тот случай, где
Bevy-модель идиоматически верна (не мимикрия).
**Тесты:** (а) система под `run_if`, спящая N кадров, при пробуждении видит накопленные
изменения ровно один раз; (б) две системы разных стадий не влияют на окна друг друга;
(в) `executor_parity.rs` расширить change-detection-сценарием (seq ↔ hybrid — одинаковые
наблюдаемые change-сеты).

### 1.3 Run-to-run детерминизм-гейт (постоянный, не разовый спайк) ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `apex-scheduler/tests/determinism.rs` — 5 гейтов: concurrent_spawn (3 системы
> гоняются), spawn_despawn_churn (25 кадров reuse), conditional_spawn (run_until-гейт), event_driven
> (события→спавн), escrow_spike (спайк в пределах эскроу, валидирует 1.1). Каждый — id-ЧУВСТВИТЕЛЬНЫЙ
> снапшот дважды в одном процессе с `set_deterministic_spawn(true)`, ассерт байт-идентичности
> (сильнее parity-гейта D9: тот только семантика seq↔par). Дешёвый CI-гейт против дрейфа детерминизма.

**Как:** новый тест-таргет `apex-scheduler/tests/determinism.rs`:
- Репрезентативные schedule'ы (переиспользовать матрицу `executor_parity.rs`: spawn+move,
  run_if-gating, растущий мир, multi-stage, форс-ASD-row-split, + spawn/despawn-churn и
  события) гоняются **дважды в одном процессе** с `set_deterministic_spawn(true)`.
- Ассерт: детерминированные снапшоты (bincode-сериализация волны 7 — уже детерминирована,
  подтверждено) **байт-идентичны** между прогонами; плюс прямой ассерт на равенство
  присвоенных Entity id (это сильнее семантического равенства — проверяем именно гарантию
  ADR-001).
- Это дешёвый гейт (in-process, без кросс-репо goldens) — любой будущий дрейф детерминизма
  становится ГРОМКИМ фейлом CI (§0.2a), как parity-гейт D9 для seq↔par.

### Гейт волны 1 ✅ ПРОЙДЕН (2026-07-06)

Полные гейты шапки + Miri на затронутых unsafe-путях `entity.rs` (block-reserver — targeted-прогон
по entity/reserv-тестам: 20 passed, 0 UB). **Итог:** apex-scheduler 102 lib + все integration
(determinism 5, d8b 5 incl. escrow, parity 5) + workspace зелёный; clippy net-neutral; **движок
`cargo build --workspace` ✅, goldens `visual_tests` 649/0/9 БАЙТ-ИДЕНТИЧНЫ** (per-system change-window
и эскроу рендер не сдвинули — эскроу opt-in, per-system дефолт = world-тик). TECH_DEBT: D8b-overflow
✅, D6-полное ✅. **Побочная находка (§0.2a):** pre-existing Miri UB в change-detection query-пути
(`unified_system`, обе модели borrow) — НЕ регрессия 1.2 (подтверждено на b48ffd0); записан 🔴 MIRI-CD,
заведена отдельная задача, вне scope кампании.

**Волна 1 ✅ ЗАКРЫТА.** Далее — волна 2 (event-модель S3/S4/F4b).

---

## 3. Волна 2 — event-модель: per-system курсоры везде (S3/S4/F4b)

> F4 (закрыт 2026-07-05) дал модель: персистентный per-system курсор в `SystemParam::State`
> (`EventReaderState<E>`), no-loss registry — НАША модель, не Bevy-лоссовая (§0.9). Волна
> распространяет её на оставшиеся лазейки. Порядок внутри волны: 2.1 → 2.2 → 2.3
> (2.3 зависит от решений 2.2).

### 2.1 S3 — `World::event_writer/event_reader(&self)` = гонка из safe-кода ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `event_reader`/`event_writer` → `&mut self` (благословенный эксклюзив);
> `event_reader_unchecked`/`event_writer_unchecked(&self)` + `#[doc(hidden)]` (escape-hatch ADR-002).
> Единственный `&World`-потребитель — `Extract<Listen<E>>::fetch` — мигрирован на `_unchecked`
> (sound: sequential extract, декларированный `Listen<E>`). `SubWorld`-варианты оставлены `&self`
> (dead-code, недостижимы из safe через `unsafe fn from_raw` — инвариант зажурналирован). Тест
> trybuild `event_mut_needs_exclusive_world` (E0596). Гейты: workspace зелёный, clippy net-neutral,
> Miri TB чист (вкл. multi-thread `events_lag_threads`). Движок S3 не задет (там только `ctx.event_reader`
> = шаг 2.2). Детали — TECH_DEBT S3.

**Проблема:** `world.rs:977/990` мутируют event-очереди через `&World`; `World: Sync` → два
потока с `&World` в safe-коде — data race (тот же корень, что закрытый S1-part-2).

**Как (канон ADR-002, как S1/S2/F3):** благословенная поверхность → `&mut self`;
`&self`-варианты → `event_reader_unchecked`/`event_writer_unchecked` с `#[doc(hidden)]`
(частично уже есть: `event_writer_unchecked`, `world.rs:2581` — довести симметрию).
Потребители: пример `basic.rs`, `EventReader`/`EventWriter`-SystemParam (идут через
декларированный путь — проверить, что не задеты), `SubWorld`-варианты (`sub_world.rs:208/219` —
по записи S1 недостижимы из safe; верифицировать и либо гейтнуть так же, либо журналировать
почему sound). Rename-only без `&mut`-пути был бы полумерой — не делаем.
**Тесты:** compile-fail (trybuild) на `&World`-мутацию событий из safe; существующие 23+
event-теста зелёные.

### 2.2 S4 — недекларированный `ctx.event_reader` мимо conflict-детекции ✅ 2026-07-06

> **✅ ЗАКРЫТО.** `SystemContext::event_reader` → `event_reader_unchecked` + `#[doc(hidden)]`
> (симметрия с уже-`_unchecked` writer/resource). Благословенный путь чтения из системы = параметр
> `EventReader<E>`/`Listen<E>`. Переведены на `_unchecked`: `Listen`/`EventReader` `fetch`, 4 сайта
> `system!`-макро, 4 сайта apex-input AutoSystem (движок; декларируют `Listen<…>` — ADR-002-остаток).
> Руководство §5.2.1/§6.7 + таблицы обновлены. Гейты: workspace + scripting E2E зелёные, clippy
> net-neutral, Miri TB чист, движок check ✅. Детали — TECH_DEBT S4.

**Проблема:** `ctx.event_reader` (`world.rs:2531`) благословлён как «read», но
`EventReader::new` → `add_reader` пишет в реестр курсоров (push/realloc,
`events.rs:163-178`). Планировщик этот путь не видит.

**Как (Р-1: plain-fn = единственный golden path):** `ctx.event_reader` →
`ctx.event_reader_unchecked` + `#[doc(hidden)]` (симметрично S2/F3.2); благословенный путь
чтения событий из системы — ТОЛЬКО `EventReader<E>`-SystemParam (персистентный курсор F4,
декларация видна планировщику). Потребители ctx-пути мигрируются на параметр. Руководство —
секция advanced обновляется (та же формула, что для `resource_mut_unchecked`).

### 2.3 F4b — AutoSystem/`system!`-путь чтения событий → ПЕРСИСТЕНТЕН ✅ 2026-07-06

> **✅ ЗАКРЫТО ПОЛНОСТЬЮ (золотой путь, по требованию пользователя).** Первая переоценка сочла полное
> закрытие несоразмерным, но верный архитектурный ход снимает оба блокера: курсор живёт НЕ в
> пользовательской структуре (об неё бились блокеры), а в **раннере системы** — адаптере AutoSystem,
> который и так переживает кадры (как plain-fn держит `SystemParam::State` в замыкании). Реализация
> по паттерну существующего `deferred_cmds`: `EventCursors` (per-system, TypeId-keyed) во владении
> адаптера; `SystemContext.event_cursors: Option<*mut EventCursors>` + `with_event_cursors` +
> благословенный `event_reader_persistent_auto`; макро-снипеты → персистентный путь (0 ripple по ~35
> impl AutoSystem). Инвариант: event-читатель форсится `stateful` → single-task (нет row-split → нет
> aliasing; чинит и латентный query+event-баг). Тесты `macro_event_reader.rs`: per-frame + **catchup
> (3 рана без flush → батч ровно раз)**. Гейты: workspace, clippy net-neutral, Miri TB чист (raw-ptr),
> движок build + goldens 649/0/9 байт-идентичны. Детали — TECH_DEBT F4b.

**Проблема:** `ctx.event_reader` внутри `system!`-тела даёт свежий курсор → FixedUpdate-катчап
читает дубли (тот же баг, что F4 закрыл для plain-fn).

**Как:** после 2.2 `system!`-тела, читающие события, обязаны декларировать
`EventReader<E>`-параметр (макро уже поддерживает параметры) — тогда курсор персистентен
через `SystemParam::State` автоматически, и отдельная macro-хирургия хранения курсоров в
`AutoSystem` НЕ нужна. Если grep потребителей покажет `system!`-тела с `ctx.event_reader`,
которые нельзя выразить параметром — письменная переоценка (§0.2b) с хранением
state в AutoSystem-инстансе.
**Тест:** зеркало `persistent_event_reader_no_duplicate_reads` для `system!`-пути.

### Гейт волны 2 ✅ ПРОЙДЕН (2026-07-06)

Полные гейты шапки + Miri TB по event-путям (core events 22, persistent-reader, multi-thread
`events_lag_threads` 3 — ноль UB/гонок). **Итог:** apex-core 260 lib + весь workspace (scheduler 102 +
integration, scripting 8 E2E) зелёный; clippy `--all-targets` net-neutral (4 pre-existing в
serialization/bench); **движок `check --all-targets` ✅, goldens `visual_tests` 649/0/9 БАЙТ-ИДЕНТИЧНЫ**
(S3/S4 рендер не трогают, apex-input rename семантически тождествен). TECH_DEBT: S3 ✅ (2.1), S4 ✅ (2.2),
F4b ✅ полной реализацией персистентных курсоров (2.3, коммит c76e11c). Руководство §5.2.1/§6.7 +
таблицы World/ctx отражают финальную поверхность.
Коммиты: core `5150970`/`a70abf2`/`095dcd7` + engine `ced2f31` (ветка api-golden-path). НЕ запушено.

**Волна 2 ✅ ЗАКРЫТА.** Далее — волна 3 (скриптинг: фаза A DynQuery/S7/S8, фаза B Lua-системы).

---

## 4. Волна 3 — скриптинг на золотой путь (§10.8 + S7/S8 + первоклассные скрипт-системы)

> Контекст-решение волны 6 CORE_AUDIT (§10.8): скриптинг «обязан встать на общий, валидируемый
> ядром механизм» — `DynQuery` сделан и назвал скриптинг консумером, миграция не выполнена.
> Сегодня `apex-scripting/src/iterators.rs` — собственный unsafe-путь через `_meta`
> (`:~301, 426`), а `ScriptEngine::run(dt, &mut world)` — монолит вне планировщика
> (`script_engine.rs:383`). Дизайн-развилка стажёра (Arc<Mutex<VM>> + `Res<ScriptEngine>`)
> отклонена — §5.

### Фаза A — `iterators.rs` → DynQuery/DynQueryMut; S7/S8 становятся несущими

> **Статус (2026-07-06): ФАЗА A ✅ ЗАКРЫТА.** S7 ✅ + S8 ✅ (core-фундаменты, коммиты `5e50661`/`8e1f410`) +
> миграция ✅ (этот коммит). S7 — `DynItemMut::get_mut/get_mut_ptr` гейтят по декларированным `writes`. S8 —
> Changed/Added-термы у обоих динамических билдеров. **Миграция (все 4 пункта сделаны):**
> (1) `iterators.rs` пересажен с `_meta`/`columns_raw` на DynQuery(read)/DynQueryMut(write): `query()`
> снимает матч-энтити вперёд (`collect_matching_entities`), per-`next()` чтение — `DynItem::get_ptr(id)`,
> `commit` — `DynQueryMut::get_mut(entity)` (E10 через allocator) + `DynItemMut::get_mut_ptr(id)` (S7-гейт +
> change-tick); `build_arch_states`/`ArchState`/`ComponentState` УДАЛЕНЫ; `&mut World` — через новый
> `ScriptContext::world_ptr_mut` (raw escape-hatch, gather-фаза освобождает займы до `&mut World`).
> (2) 4 `warn_once!` → `anomaly!` (World в scope). (3) `query_cache` УДАЛЁН — §0.2b-переоценка: снимок
> per-entity строится в любом случае, кэш+клон `Vec<Entity>` не дешевле пересборки (архетипов единицы),
> второй источник истины устранён (структурный аргумент решающий, микробенч не нужен). (4) парсер:
> `Changed:X`/`Added:X` → `changed_id`/`added_id` (фильтры как `With:`; значение — через `Read:X`).
> **Трейд-офф:** write-set commit'а из `_meta.writes` → S7-гейт тавтологичен в фазе A (реальный enforcement —
> фаза B); память безопасна независимо (id-резолв + allocator-lookup). Гейты: 6 unit + 9 E2E (flagship
> commit-persist, forgery-reject stale-entity, Changed-фильтр применяется) + workspace зелёные, clippy
> `--all-targets` net-neutral (4 pre-existing serialization/bench), Miri неприменим (mlua FFI, покрыт E2E).
> §10.8 + script-часть ErrorHandler-хвоста закрыты в TECH_DEBT. **ДАЛЬШЕ — фаза B (Lua-системы =
> первоклассные системы планировщика; см. ниже).**

- **Миграция доступа:** парсер дескрипторов (`parse_one_desc`, `iterators.rs:100-114`)
  остаётся; сборка архетипов/чтение/запись — через `DynQuery`/`DynQueryMut`
  (`apex-core/src/query.rs:2718/2922`) вместо собственного `_meta`-пути. Собственный
  unsafe-путь УДАЛЯЕТСЯ (не сосуществует — иначе два источника истины).
- **S7 — write-гейт item'а:** `DynItemMut::get_mut/get_mut_ptr` (`query.rs:~2711, 3099`) —
  проверять вхождение `ComponentId` в декларированные `writes`; нарушение → `None` +
  `anomaly!` (ErrorHandler — World в scope есть). Без этого декларация write декоративна,
  а скриптинг/agent-IPC поверх — дыра.
- **S8 — Changed/Added-термы** динамического билдера (нужны инспектору редактора для
  change-polling без полных сканов; скриптингу — для реактивных запросов). Синтаксис
  дескриптора: `"Changed:Position"` / `"Added:Position"` — симметрично `With:`/`Without:`.
- **ErrorHandler-хвост (script-часть):** 4 `warn_once!`-сайта незарегистрированных компонентов
  в `iterators.rs` → `anomaly!` (World теперь в scope через DynQuery-путь). Остальной
  ErrorHandler-хвост (template/isolated) — НЕ в этой кампании.
- **Гейты фазы:** scripting E2E волны 7 (8 тестов) зелёные без ослабления; Miri по
  script-тестам; `query_cache` (`context.rs:110`) пересобирается на DynQuery-стейты или
  удаляется, если DynQuery-построение дешёвое (замерить, решить письменно).

### Фаза B — Lua-системы = первоклассные системы планировщика

- **Декларативная регистрация из скрипта:** script-API
  `system{ name = "...", query = {"Write:Position", "Read:Velocity"}, fn = function(it) ... end }`
  (вместо одного монолитного `run()`); `run()` без деклараций остаётся как совместимый
  fallback = одна эксклюзивная система.
- **Динамический access-set:** registrar транслирует `QueryDesc` → `ComponentId`
  reads/writes и регистрирует систему в планировщике с этим access. **Дизайн-пункт:**
  планировщику нужна поверхность регистрации систем с runtime-декларациями
  (сегодня access выводится из типов SystemParam при компиляции schedule) — спроектировать
  `add_dynamic_system(name, access, runner)` АККУРАТНО (не вторая параллельная система
  регистрации, а нижний слой той же; типизированный путь становится его частным случаем —
  фундамент, на который потом встаёт agent-IPC).
- **VM-токен:** каждая Lua-система дополнительно декларирует write на маркер-ресурс
  `ScriptVm` → планировщик честно сериализует Lua↔Lua (VM одна) и честно параллелит
  Lua↔Rust по реальным декларациям. Никакого Mutex — эксклюзив выражен в access-модели.
- **Детерминизм скриптовых команд:** deferred-спавны скриптов идут через per-system
  `Commands`-буферы (D8b) вместо прямого `world.spawn` в `apply_spawn_queue`
  (`script_engine.rs:446-487`) → детерминированные id для спавнов из Lua автоматически.
- **Hot-reload:** перерегистрация систем скрипта при reload (декларации могли измениться) —
  через существующий watcher-путь; смена access = re-compile schedule (планировщик это уже
  умеет по dirty-флагу — проверить, иначе добавить).
- **Гейты фазы:** hot-reload E2E волны 7 зелёные; новый тест — Lua-система с `Write:Position`
  НЕ параллелится с Rust-системой, пишущей Position, и параллелится с независимой;
  детерминизм-гейт волны 1 расширяется script-спавн-сценарием.

#### ⚙ ДИЗАЙН ФАЗЫ B (утверждён 2026-07-06 после переоценки §0.2b + чекпоинт с юзером)

**Стержневая находка (из кода):** ВСЕ системы планировщика обязаны быть `Send` (`SystemFn =
Box<dyn FnMut(&mut World) + Send>`, `ParSystem: Send + Sync`; параллельные диспатчатся на
rayon-воркеры через `SendPtr`, даже single-task stateful — `executor.rs:584`). Ресурсы —
`Send+Sync`. **NonSend-концепции нет.** mlua `Lua` — `!Send`; `ScriptContext.world_ptr:
NonNull<World>` — `!Send`. Итог: план неявно предполагал, что токен `ScriptVm` сам делает Lua
обычной Parallel-системой — НЕВЕРНО: токен управляет только конфликтами/порядком, не снимает
требование `Send` на бокс системы. Развилка: (A) mlua `send`-фича → тянет `Rc→Arc` +
Send-легализацию `world_ptr` = отклонённая §5 `Arc<Mutex<ScriptContext>>`-территория; (C) новый
вид систем **NonSend (main-thread)**. **Юзер выбрал (C)** — единственный путь без воскрешения
§5-проблем; эксклюзив VM выражен исполнением на main-потоке (никакого Mutex/Send-фейка).

**Следствие: `Scheduler` становится `!Send`** (хранит `!Send` NonSend-боксы в `self.systems` —
единый дом регистрации, не второй реестр). Проверено: `run()` НЕ оборачивается в `pool.install`;
движок/редактор Scheduler между потоками не двигают (драйвер play-режима живёт у хоста на своём
потоке — `apex-editor/play.rs:25`). Единственный потребитель `Scheduler: Send` — тест
`tests.rs:3103` (детерминированный 4-поточный пул) — адаптируется тривиально: создать+гонять
scheduler ВНУТРИ `install`-замыкания (Scheduler живёт целиком на потоке пула, не пересекает
границу). Future crash-isolation всё равно требует thread-affinity VM → `!Send` это корректно
навязывает. Записать addendum-нотой при ротации.

**Шаги (каждый — атомарный коммит со своими тестами + полные гейты шапки):**

**Прогресс: ФАЗА B ✅ ПОЛНОСТЬЮ (B1–B4).** B1 ✅ (`b39e0b1`) — фундамент NonSend, `Scheduler: !Send`.
B2 ✅ (`6b545d9`) — параллельный executor (Lua‖Rust через `rayon::in_place_scope`; rendezvous + Miri TB).
B3 ✅ (B3a `12a92ab` core-примитив declared-cell + B3b/c `6bd642c` scripting-интеграция `system{}`).
B4 ✅ (B4a `cd5f532` spawn/despawn через per-system Commands + детерминизм-гейт; B4b `c6cdd49`
`Scheduler::remove_system` + идемпотентный hot-reload re-register). **Гейт фазы B ПРОЙДЕН:** apex-ecs
workspace зелёный (scheduler 108 + scripting 10 unit/4 access/9 engine_e2e/8 script_systems_e2e),
clippy `--all-targets` net-neutral (4 pre-existing serialization/bench), Miri TB чист (declared-cell
write, remove_system, nonsend concurrency); **движок `check --workspace` ✅ + goldens `visual_tests`
649/0/9 БАЙТ-ИДЕНТИЧНЫ** ✅ (скриптинг в render-путь не входит). НЕ запушено.

**Открытый хвост фазы B (мелочь, §0.2a-честно):** resource-write/event ops из скрипт-СИСТЕМЫ (не из
монолита) требуют `&mut World` (не entity-local Commands-op) → сейчас громкий drop+warn; применять их
из скрипт-системы = отдельный заход (deferred-typed-value binding через `commands.add(Send-closure)`),
либо использовать Rust-систему/монолит. Spawn/despawn покрыты (per-system Commands). Фаза C (Lua↔Lua
VM-пул) — ⛔ ROI-gated, вне кампании.

**⚠ B3 — уточнение дизайна (переоценка §0.2b, soundness):** план предполагал раннер использует
«query()/commit() = путь фазы A», но commit фазы A = `&mut *world_ptr_mut()` → `query_builder_mut()`
(нужен `&mut World`), звучно ЛИШЬ когда ptr от genuine `&mut World` (монолит). В B3 раннер получает
`&World` (`ctx.world_ref()`), а стадия B2 держит `&World` (`run_stage_parallel(world: &World)`; воркеры
пишут через UnsafeCell из `*const World`). Формировать `&mut World` из `&World`-провенанса = **UB под
Tree Borrows**. **Фикс (B3a, foundation-first):** `QueryBuilderMut` держит `UnsafeWorldCell` (не
`&mut World`); новый `#[doc(hidden)] unsafe QueryBuilderMut::from_declared_cell(&World)` +
`World::query_builder_mut_declared(&self)` — write-запрос из `&World` под контрактом декларированного
доступа (эксклюзив доказывает планировщик, не `&mut World`; запись только в UnsafeCell-колонки, `&mut
World` НЕ формируется). commit пересажен на `ctx.world_ref()`+declared-cell; `ScriptContext::world_ptr_mut`
удалён. Работает и для монолита (эксклюзив тривиален), и для B3-раннера (звучно под конкуренцией).
**Бонус: этот core-путь Miri-абелен** (чистый core) — покрыт `dyn_write_from_declared_cell_writes_and_gates`
(Miri TB чист), чего mlua-путь фазы A не мог. Раннер НЕ зовёт `world_mut()`; структурные ops (spawn/
despawn/resource/event) из скрипт-СИСТЕМЫ → B4 (per-system Commands); в B3 громкий drop+warn (§0.2a).

- **B1 — фундамент NonSend (корректность-first, БЕЗ параллелизма):** `trait NonSendSystem {
  fn run(&mut self, ctx: SystemContext) }` (без Send/Sync); `SystemKind::NonSend { system:
  Box<dyn NonSendSystem>, access }`; `kind.access()` → `Some` (участвует в конфликт-детекции —
  `compile.rs:433/447`), `is_parallel()` → `false` (стадия с NonSend → `all_parallel=false` →
  `!all_parallel`-ветка = исполнение на main). Регистрация: публичный `add_dynamic_system(name,
  access, runner: FnMut(SystemContext)+Send)` (промоут тест-онли `add_par_access` → `Parallel`)
  + `add_dynamic_nonsend_system(name, access, runner: FnMut(SystemContext))` → `NonSend`; оба —
  тонкие обёртки над общим SystemDescriptor-backbone (типизированный `add_systems` = тот же
  backbone → «нижний слой той же регистрации»). Executor: match-арм `NonSend => system.run(ctx)`
  в `!all_parallel` + fallback-ветках (тот же `SystemContext::with_commands(..).with_last_run`).
  `Scheduler: !Send` + фикс теста `install`. Тест: NonSend-Rust-замыкание бежит на main; две
  системы с write одного ресурса сериализуются; NonSend↔Parallel-конфликт даёт правильный порядок.
- **B2 — параллельный executor (Lua‖Rust):** NonSend участвует в parallel-группировке; в
  `run_stage_parallel` NonSend-системы бегут на ГЛАВНОМ потоке ВНУТРИ `rayon::scope` (после
  спавна Send-тасков → concurrent с воркерами). Sound: compile развёл конфликтующие системы по
  стадиям → в одной стадии NonSend и Send-таски трогают непересекающиеся данные (тот же аргумент
  дизъюнктности, что воркер↔воркер). Per-system Commands-слот + `last_run`-окно как у прочих
  single-task. Тест: Lua-система `Write:Position` НЕ параллелится с Rust `Write:Position`,
  параллелится с независимой; детерминизм-гейт; goldens байт-идентичны.
- **B3 ✅ — ScriptVm-токен + registrar `system{}` + Lua-runner:** `ScriptVm` — ZST-маркер-ресурс;
  Lua-система декларирует write на него (→ Lua↔Lua сериализация) + component reads/writes из
  `QueryDesc` (через `ctx.binding`→`ComponentId`). Script-API `system{ name, query={...}, fn }`;
  runner (NonSend) держит `Rc<Lua>`+`Rc<RefCell<ScriptContext>>`+fn-handle+**доверенный write-set**
  (из декларации, НЕ `_meta` → S7-гейт становится СОДЕРЖАТЕЛЬНЫМ, закрывает трейд-офф фазы A);
  на `run(ctx)` ставит `world_ptr`, гонит Lua (query()/commit() = путь фазы A), чистит. `run()`
  без деклараций остаётся fallback = одна эксклюзивная (NonSend) система. Enforcement
  «только декларированные компоненты»: ScriptContext получает декларированные read/write ids →
  query()/commit() резолвят только их (undeclared → `anomaly!`).
- **B4 ✅ — детерминизм спавнов + hot-reload:** script-спавны через per-system Commands-слот (D8b,
  уже даёт детерминированные id для single-task на main); hot-reload = re-register скрипт-систем
  при reload (декларации могли смениться) → `invalidate_plan` → re-compile (планировщик умеет по
  dirty). Тест: детерминизм-гейт + script-спавн; hot-reload E2E.

### Фаза C — Lua↔Lua параллелизм (share-nothing VM-пул) — ⛔ ROI-gated, отдельное решение

НЕ делать в этой кампании. Условие открытия: профиль реального потребителя (движок/редактор),
показывающий Lua-время как значимую долю кадра. Дизайн-скетч для будущего: пул независимых
VM (по одной на воркер либо на группу систем), общее состояние — ТОЛЬКО через ECS; никаких
разделяемых VM/Mutex. До тех пор одна VM + VM-токен — честная и достаточная модель.

### Гейт волны 3

Полные гейты шапки; TECH_DEBT: §10.8, S7, S8, script-часть ErrorHandler-хвоста закрываются.
Руководство §17 (Lua) переписывается под `system{}`-API. Движок: `check --all-targets` +
goldens (скриптинг в render-пути не участвует — goldens не должны шевелиться вовсе).

---

## 5. ⚠ Дизайн-нота: развилка «per-system ScriptEngine через Arc<Mutex>» — ОТКЛОНЕНА (при ротации → ADR)

**Предложение (2026-07-06, внешнее):** `ScriptEngine` как ресурс; каждая Lua-система — Rust-система
с типизированными `Query`-параметрами + `lua: Res<ScriptEngine>`; `ScriptContext`
`Rc<RefCell<>>` → `Arc<Mutex<>>`; mlua-фича `send`; ожидание — планировщик параллелит
Lua-системы; оценка ~100-200 строк.

**Принято из предложения (цель):** Lua-системы как первоклассные системы планировщика с
декларациями доступа — это §10.8 + фаза B волны 3.

**Отклонено (реализация), причины:**
1. **Ложный параллелизм.** Одна `lua_State` принципиально однопоточна; `Mutex` вокруг VM
   означает, что каждая Lua-система держит лок всё время исполнения → Lua↔Lua параллелизм
   нулевой, contention 100%, плюс налог лока. Единственный реальный выигрыш (interleaving
   Lua↔Rust) достижим без Mutex — VM-токеном в access-модели (фаза B).
2. **Возврат класса soundness-дыр F3/ADR-002/S1/S2.** `Res<ScriptEngine>` декларирует «read
   ScriptEngine», фактически система пишет компоненты через VM → недекларированный live-write
   мимо conflict-детекции — ровно то, что закрывали волны 6/6б. Декларации обязаны быть
   ДИНАМИЧЕСКИМИ (из запросов скрипта) и видимыми планировщику.
3. **`Arc<Mutex<ScriptContext>>` легализует `world_ptr: NonNull<World>` (`context.rs:72`)
   как Send+Sync** — сырой указатель на World между потоками; soundness-регресс по построению.
4. **Статические `Query<&mut T>`-сигнатуры не работают для скриптов:** запросы известны только
   в рантайме (строки) и меняются при hot-reload; рукописная Rust-обёртка на каждую
   Lua-систему убивает динамику и не масштабируется.
5. **Deadlock вместо громкой паники:** lua-коллбеки берут ctx через `app_data`; `RefCell`
   реентрантный borrow — громкая паника, нереентрантный `Mutex` — тихий deadlock.
6. **Оценка «100-200 строк» нереалистична:** это архитектурная миграция (реестр скрипт-систем,
   динамические декларации, владение VM), не замена `Rc`→`Arc`.

**Итоговое направление:** фазы A/B волны 3 (DynQuery + динамические декларации + VM-токен);
настоящий Lua↔Lua параллелизм — только share-nothing VM-пул (фаза C, ROI-gated).

---

## 6. Порядок, зависимости, ротация

- **Порядок волн: 0 → 1 → 2 → 3.** Зависимости: 2.3 зависит от 2.2; фаза B волны 3 зависит от
  фазы A (DynQuery-доступ) и волны 2 (события для скрипт-систем через честный путь); D8b-эскроу
  (1.1) переиспользуется фазой B (детерминизм скриптовых спавнов). Внутри волн пункты атомарны.
- **TECH_DEBT-дисциплина:** пункт взят в работу → запись в TECH_DEBT получает указатель сюда;
  пункт закрыт → запись ✅ тем же коммитом (один факт — одно место).
- **Закрываемые записи по волнам:** волна 0 — A5(запись), ParallelPolicy, B5, B6, §1.4,
  string-table; волна 1 — D8b-overflow, D6-полное; волна 2 — S3, S4, F4b; волна 3 — §10.8,
  S7/S8, ErrorHandler(script-часть).
- **Ротация при закрытии кампании (тем же коммитом):** план → `plans/archive/` с выжимкой;
  дизайн-нота §5 → `decisions/ADR-NNN-script-systems-architecture.md`; addendum'ы ADR-001/003;
  руководство пользователя — секции детерминизма (§6.6a) и Lua (§17); статусы → TECH_DEBT.
- **Кросс-репо:** каждая волна заканчивается сборкой/гейтами движка; пуш ядра ДО движка.
