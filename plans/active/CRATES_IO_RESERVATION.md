# Кампания: резерв имён и публикация на crates.io (ApexForge_ECS)

> **Статус: 🔜 план (2026-07-05).** Источник истины кампании; статусы пунктов — ЗДЕСЬ.
> **Момент старта — за пользователем.** Решение (2026-07-05): резервируем/публикуем ПОСЛЕ полировки
> всех волн API_GOLDEN_PATH (в частности волны 5 — руководство), «профессионально, под руководством
> ассистента». До первого релиза ломать имена бесплатно; после публикации любой ренейм = major-bump —
> поэтому naming/prelude/deprecated-дисциплина (волны 3/4/P) закрыта ДО этой кампании (сделано).
>
> **Разделение труда (по договорённости):** всё, что готовится **внутри крейтов** (метаданные,
> LICENSE, README, dry-run) — делает ассистент (Часть A). Всё, что требует **аккаунта/секретов/
> необратимой публикации** — делает пользователь по пошаговой инструкции (Часть C), решения продукта —
> Часть B. Связано: `decisions/ADR-004-api-golden-path-canon.md` (Р-2 брендинг + свободные имена;
> план в `plans/archive/API_GOLDEN_PATH.md`), `docs/CONVENTIONS.md`.

---

## 0. TL;DR

Ядро функционально и по API готово к публикации, но **метаданными — нет**: `cargo publish` СЕЙЧАС
падает по двум жёстким причинам (ни у одного крейта нет `description`; path-зависимости без `version`).
Плюс нет LICENSE-файла, README, продуктовых метаданных (keywords/categories/repository). Кампания
доводит метаданные до publish-ready, резервирует бренд-имена (`apexforge-ecs`, `apexforge`, `apex-ecs`)
и публикует 8 крейтов ядра в топологическом порядке. **crates.io-имена проверены свободными 2026-07-05**
(Р-2): `apexforge-ecs`, `apexforge`, `apex-ecs`, `apex-core`, `apex-scheduler`, `apex-serialization`,
`apex-macros`, `apex-isolated`, `apex-scripting`, `apex-hot-reload`, `apex-graph` — все 404/FREE.

---

## 1. Текущее состояние (аудит 2026-07-05)

**Жёсткие блокеры (publish падает):**
1. **Ни у одного крейта нет `description`** — crates.io требует непустой `description`. (10 из 10
   Cargo.toml без него.)
2. **Path-зависимости без `version`** — при публикации path вырезается, нужен `version`. Пример:
   `apex-core = { path = "../apex-core" }` → должно стать
   `apex-core = { path = "../apex-core", version = "0.1.0" }`. Иначе publish зависимого крейта падает.

**Мягкие (не блокируют publish, но обязательны для профессионального релиза):**
3. **Нет `LICENSE`-файла** (в `[workspace.package]` заявлен `license = "MIT"`, но текст-файла нет).
4. **Нет README** и поля `readme` — это лендинг крейта на crates.io/docs.rs.
5. `authors = ["You"]` — плейсхолдер (поле устаревшее, но чинить).
6. Нет `repository`, `keywords` (≤5), `categories` (из фикс-списка crates.io), `homepage`,
   `documentation` (по умолчанию docs.rs — ок), `rust-version` (MSRV).
7. `apex-examples` / `apex-bench` **не должны публиковаться** → `publish = false`.
8. Умбрелла-крейты `apexforge-ecs` / `apexforge` / `apex-ecs` **не существуют** — нужна стратегия
   (фасад-реэкспорт vs squat-плейсхолдер) — см. Часть B.

**Известные факты:**
- Версия воркспейса — `0.1.0` (единая, `[workspace.package]`). Гайд местами говорил «0.3.0» — это
  была ошибка дока (правится волной 5).
- Rust toolchain разработки — 1.95.0. MSRV формально не установлен (см. B-6).
- Git-remote: `https://github.com/StanislavMal/apex-ecs` (для поля `repository`).

---

## 2. Порядок публикации (топологический, по факту графа зависимостей)

`cargo publish` — по одному крейту; зависимость должна УЖЕ быть на crates.io. Граф (внутренние deps):

```
apex-macros  (proc-macro, без внутр. deps)   ─┐
apex-graph   (без внутр. deps)               ─┤
apex-core        → apex-graph, apex-macros    │
apex-serialization → apex-core                │
apex-scheduler   → apex-core, apex-graph      │
apex-scripting   → apex-core, apex-macros     │
apex-isolated    → apex-core, apex-scheduler  │
apex-hot-reload  → apex-core, apex-serialization
apex-examples / apex-bench → publish = false (НЕ публикуются)
```

**Линейный порядок публикации (8 крейтов):**
1. `apex-macros`
2. `apex-graph`
3. `apex-core`
4. `apex-serialization`
5. `apex-scheduler`
6. `apex-scripting`
7. `apex-isolated`
8. `apex-hot-reload`
9. *(опц.)* умбрелла `apexforge-ecs` / `apex-ecs` — последней (зависит от всех).

После каждого publish — пауза ~30–60с (индексация crates.io), иначе следующий крейт не найдёт свежую
зависимость.

---

## 3. Часть A — подготовка ВНУТРИ крейтов (задачи ассистента)

> Выполняется когда пользователь даёт старт кампании (после полировки волн). Каждый шаг — свой коммит,
> гейты как обычно (workspace build + tests + clippy net-neutral + goldens byte-identical).

- **A-1. Метаданные `[workspace.package]`** (наследуются крейтами через `.workspace = true`): добавить
  `repository = "https://github.com/StanislavMal/apex-ecs"`, `rust-version = "<MSRV>"` (см. B-6),
  заменить `authors` (см. B-3), при желании `homepage`/`documentation`. `license = "MIT"` — оставить.
- **A-2. `description` на каждый публикуемый крейт** (уникальный, ≤ ~120 симв., EN — это код-метадата).
  Черновики (на утверждение B-4):
  - `apex-macros` — "Derive macros for the Apex ECS (Component, Bundle)."
  - `apex-graph` — "Directed-graph primitives used by the Apex ECS scheduler."
  - `apex-core` — "Archetypal ECS core: entities, components, queries, relations, events, snapshots."
  - `apex-serialization` — "World snapshot/restore and prefab serialization for the Apex ECS."
  - `apex-scheduler` — "Cost-model system scheduler for the Apex ECS."
  - `apex-scripting` — "Lua scripting integration for the Apex ECS."
  - `apex-isolated` — "Isolated-world bridge (cross-thread messaging) for the Apex ECS."
  - `apex-hot-reload` — "Hot-reload support for the Apex ECS."
- **A-3. `version` на все внутренние path-deps** — `{ path = "…", version = "0.1.0" }` во всех Cargo.toml
  (apex-core/scheduler/serialization/isolated/hot-reload/scripting). Хранить path И version: локальная
  сборка идёт по path, публикация — по version.
- **A-4. `keywords` (≤5) + `categories`** (из фикс-списка crates.io: напр. `game-development`,
  `data-structures`, `concurrency`, `simulation`) на каждый крейт — черновик на утверждение B-5.
- **A-5. `publish = false`** в `apex-examples` и `apex-bench`.
- **A-6. LICENSE.** Положить `LICENSE` (MIT, с правообладателем/годом из B-3) в корень; при отдельной
  публикации каждый крейт включает файл (либо `license-file`, либо копия — уточнить при A).
- **A-7. README.** Корневой `README.md` (лендинг репо) + `readme`-поле. По желанию — короткий per-crate
  README; минимально — общий с указанием `readme = "../../README.md"` (проверить, что cargo включит).
- **A-8. Умбрелла-крейт(ы)** — по решению B-1 (создать фасад или пропустить до squat).
- **A-9. `cargo publish --dry-run -p <crate>`** по каждому крейту (в порядке §2) — валидирует упаковку,
  метадату и что в архив не попадает лишнее (проверить `cargo package --list`). Ошибки чиню на месте.
  Это финальный гейт готовности Части A. `--dry-run` НЕ загружает ничего и не требует токена.

**Результат Части A:** `cargo publish --dry-run` зелёный для всех 8 крейтов; репозиторий publish-ready.
Реальная загрузка — Часть C (пользователь).

---

## 4. Часть B — решения, которые нужны от пользователя (продукт)

- **B-1. Стратегия бренд-имён `apexforge-ecs` / `apexforge` / `apex-ecs`.** Варианты:
  (а) **умбрелла-фасад** — создать крейт `apexforge-ecs` (или `apex-ecs`), который реэкспортирует
  golden-path (`pub use apex_core::prelude::*` и т.п.) = «батарейки в комплекте», один `use`; бренд
  `ApexForge_ECS` получает реальный крейт-вход; (б) **squat-плейсхолдер** — опубликовать пустой `0.0.1`
  «name reserved», заполнить позже; (в) **не резервировать** бренд-имена сейчас. Рекомендация: (а) для
  `apex-ecs` как умбрелла + (б) squat для `apexforge-ecs`/`apexforge` (бренд, могут понадобиться позже).
- **B-2. Момент резерва.** Публиковать реальные `0.1.0` сразу после Части A, ИЛИ сначала squat-резерв
  всех имён (`0.0.0`), а реальный релиз — позже. Squat защищает имена немедленно; реальный релиз
  привязывает `0.1.0`-контракт (breaking changes после = major).
- **B-3. Идентичность автора и правообладатель LICENSE.** Имя + email для `authors`/copyright
  (сейчас плейсхолдер `["You"]`; git-автор — `stanislav_m` / `allrightsm@gmail.com`).
- **B-4. Тексты `description`** — утвердить/поправить черновики A-2 (продуктовый голос).
- **B-5. `keywords`/`categories`** — утвердить набор A-4 (влияет на находимость).
- **B-6. MSRV (`rust-version`).** Зафиксировать минимальную Rust-версию (напр. `1.80`?) — потребует
  прогон на этой версии (ассистент проверит), либо не заявлять MSRV в первом релизе.
- **B-7. Публичность репозитория.** `repository`-ссылка ведёт на GitHub — репо должен быть публичным к
  моменту релиза (сейчас проверить). Приватный repository-URL в метадате = битая ссылка на crates.io.

---

## 5. Часть C — пошаговая последовательность действий ПОЛЬЗОВАТЕЛЯ

> Эти шаги ассистент выполнить не может (аккаунт, секретный токен, необратимая публикация). Выполняются
> ПОСЛЕ зелёной Части A. Публикация крейта **необратима** (можно только `yank`, но имя+версия заняты
> навсегда) — поэтому дежурит ассистент, а «кнопку» жмёт пользователь.

**C-1. Аккаунт crates.io.** Зайти на https://crates.io → «Log in with GitHub» (под аккаунтом
`StanislavMal`). В https://crates.io/settings/profile подтвердить email (без верифицированного email
publish запрещён).

**C-2. API-токен.** https://crates.io/settings/tokens → «New Token». Для первой публикации дать scope
`publish-new` + `publish-update` (можно ограничить конкретными crate-паттернами `apex-*`, `apexforge*`).
**Токен — секрет, показывается один раз.** Скопировать.

**C-3. Логин локально.** В терминале (в любой папке):
```
cargo login <вставить-токен>
```
Токен сохранится в `~/.cargo/credentials.toml` (в репозиторий НЕ коммитить — его там и нет).

**C-4. Проверка перед загрузкой (ассистент уже прогнал `--dry-run`, но перед реальным — ещё раз):**
```
cargo publish -p apex-macros --dry-run
```
Должно завершиться `Packaged … (dry run)` без ошибок.

**C-5. Публикация по одному, в порядке §2** (пауза после каждого ~30–60с):
```
cargo publish -p apex-macros
# подождать индексации, затем:
cargo publish -p apex-graph
cargo publish -p apex-core
cargo publish -p apex-serialization
cargo publish -p apex-scheduler
cargo publish -p apex-scripting
cargo publish -p apex-isolated
cargo publish -p apex-hot-reload
# (опц., по B-1) cargo publish -p apexforge-ecs   /   apex-ecs
```
Если крейт не находит свежую зависимость — подождать ещё и повторить (crates.io индексирует не мгновенно).

**C-6. (Опц.) Squat-резерв бренд-имён** (если B-2 = сначала резерв): для `apexforge-ecs`/`apexforge`
опубликовать минимальный `0.0.1`-плейсхолдер (ассистент подготовит крейт-заглушку в Части A-8), тем же
`cargo publish`.

**C-7. Постпубликация.** Проверить страницы на https://crates.io/crates/apex-core и docs.rs (docs.rs
собирает доки автоматически). При необходимости добавить co-owner:
`cargo owner --add <github-user> -p apex-core`. Ошибочную версию — `cargo yank --version 0.1.0 -p <crate>`
(имя остаётся занятым; повторно ту же версию не залить).

**Что ассистент делает параллельно/до C:** держит Часть A зелёной, готовит точные команды, дежурит на
ошибках упаковки/зависимостей, но НЕ вводит токен и НЕ запускает реальный `cargo publish`.

---

## 6. Гейты кампании

- Часть A: `cargo publish --dry-run -p <crate>` зелёный для всех 8 (+ умбрелла); `cargo package --list`
  не тянет лишнего; workspace build + tests + clippy net-neutral + goldens byte-identical (метадата не
  меняет поведение — goldens не должны дрогнуть).
- Часть C: каждая версия видна на crates.io и собралась на docs.rs.

## 7. Ротация при закрытии (CLAUDE.md)

План → `plans/archive/`; факт публикации + версии → CHANGELOG ядра (релизная секция); поле `repository`/
README — сам deliverable; связать с `API_GOLDEN_PATH` (Р-2). Публикация движка (apex-render и пр.) — вне
этой кампании (отдельный разговор; здесь только крейты ядра apex-ecs + бренд-имена).
