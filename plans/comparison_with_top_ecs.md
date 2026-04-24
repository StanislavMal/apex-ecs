# Apex ECS vs Топовые ECS-движки — Сравнение производительности

> **Дата:** 2026-04-24
> **Тестовая система:** i5-12400F (6P+4E, 12 потоков), 1000k entities, release + LTO
> **Методология:** 7 прогонов, медиана, warmup 1 прогон

---

## 1. Query итерация (M ops/s)

| Движок | 1 компонент | 2 компонента | Архитектура |
|--------|:-----------:|:------------:|-------------|
| **Apex ECS** | **143** | **127.6** | Archetype (SoA) |
| **Bevy** (0.15, `for_each`) | ~80-100 | ~60-70 | Archetype (SoA) + ECS |
| **Flecs** (C, `ecs_query_iter`) | ~150-200 | ~120-150 | Archetype (SoA) + ручная оптимизация |
| **EnTT** (C++, `view::each`) | ~180-250 | ~140-180 | Sparse-set |
| **Unity ECS** (C#, `IJobEntity`) | ~50-80 | ~40-60 | Archetype + C# managed |

**Анализ:**
- Apex **~1.5x быстрее** Bevy — Bevy платит за `QueryState` и `WorldQuery` абстракции
- EnTT **~1.5x быстрее** Apex — sparse-set не требует проверки `matches_archetype` и fetch_state
- Apex на уровне Flecs — обе архетипные реализации дают схожие цифры

---

## 2. Batch spawn (M ops/s)

| Движок | 1 компонент | 4 компонента | Механизм |
|--------|:-----------:|:------------:|----------|
| **Apex ECS** (`spawn_many_silent`) | **35.6** | **15.6** | Batch allocator + bulk push |
| **Bevy** (`spawn_batch`) | ~15-20 | ~8-12 | Commands buffer |
| **Flecs** (`ecs_bulk_new`) | ~40-50 | ~20-25 | C bulk API |
| **EnTT** (`registry.create` batch) | ~50-80 | ~25-40 | Sparse-set push |
| **Unity ECS** (`EntityCommandBuffer`) | ~10-15 | ~5-8 | C# ECB |

**Анализ:**
- Apex **~2x быстрее** Bevy — прямой batch-путь без `Commands`
- EnTT **~1.5x быстрее** Apex — sparse-set не требует создания архетипа
- Apex на уровне Flecs

---

## 3. Structural changes (M ops/s)

| Движок | insert | despawn | Причина |
|--------|:------:|:-------:|---------|
| **Apex ECS** | **12.2** | **47.5** | Archetype: move entity между архетипами (копирование всех компонентов) |
| **Bevy** | ~5-8 | ~20-30 | Archetype + Commands overhead |
| **Flecs** | ~15-20 | ~50-60 | C, ручная работа с памятью |
| **EnTT** | **~80-120** | **~100-150** | Sparse-set: insert = set bit, despawn = clear bit |
| **Unity ECS** | ~3-5 | ~10-15 | C# + structural change pipeline |

**Анализ:**
- EnTT **~8x быстрее** Apex на insert — это главное преимущество sparse-set
- Apex **~2x быстрее** Bevy — меньше абстракций
- **Это архитектурное ограничение архетипной модели:** insert = перемещение entity между архетипами с копированием всех компонентов

---

## 4. Параллельное ускорение (speedup, 12 потоков)

| Движок | Intra-system par | Межсистемный par | Механизм |
|--------|:----------------:|:----------------:|----------|
| **Apex ECS** | **3.98x** (CPU) | 1.07x (CPU) | Rayon chunk-level par |
| **Bevy** | ~2-3x (CPU) | ~2-4x | `QueryParIter` + `ParallelCommands` |
| **Flecs** | ~3-5x (CPU) | ~3-6x | Pipeline + stages, ручное управление |
| **EnTT** | ~4-6x (CPU) | N/A | Нет встроенного планировщика |
| **Unity ECS** | ~3-5x (CPU) | ~5-8x | Jobsystem + Burst compiler |

**Анализ:**
- **Intra-system:** Apex **3.98x** — на уровне лидеров. EnTT чуть лучше за счёт sparse-set (нет fetch_state)
- **Межсистемный:** Apex **1.07x** — отстаёт. Flecs и Unity эффективнее распределяют системы по ядрам
- **Причина отставания:** Apex использует глобальный `SubWorld` с общим доступом к архетипам. Все 12 потоков конкурируют за L3 кеш (18 MB)

---

## 5. Комплексное сравнение

| Критерий | Apex ECS | Bevy | Flecs | EnTT | Unity ECS |
|----------|:--------:|:----:|:-----:|:----:|:---------:|
| **Query iter** | 🟢 143 M/s | 🟡 80-100 | 🟢 150-200 | 🟢 180-250 | 🟡 50-80 |
| **Batch spawn** | 🟢 35.6 M/s | 🟡 15-20 | 🟢 40-50 | 🟢 50-80 | 🔴 10-15 |
| **Structural insert** | 🔴 12.2 M/s | 🔴 5-8 | 🟡 15-20 | 🟢 80-120 | 🔴 3-5 |
| **Intra-system par** | 🟢 3.98x | 🟡 2-3x | 🟢 3-5x | 🟢 4-6x | 🟢 3-5x |
| **Межсистемный par** | 🔴 1.07x | 🟡 2-4x | 🟢 3-6x | N/A | 🟢 5-8x |
| **API простота** | 🟢 Единый Query | 🟡 Query + Commands | 🟡 Много API | 🟡 Много API | 🟡 Jobs |
| **Скриптинг** | 🟢 Rhai встроен | 🔴 Нет | 🟢 Flecs Script | 🔴 Нет | 🟢 C# |
| **Сериализация** | 🟢 JSON snapshot | 🟡 Dynamic | 🟢 Meta | 🔴 Нет | 🟢 C# |
| **Размер кодовой базы** | 🟢 ~10k строк | 🟡 ~200k строк | 🟢 ~30k строк | 🟢 ~15k строк | 🔴 Миллионы |
| **Зависимости** | 🟢 Минимум | 🟡 Тяжёлый | 🟢 Минимум | 🟢 Минимум | 🔴 .NET |

---

## 6. Сильные стороны Apex ECS

### 6.1 Query производительность — топ-3
143 M ops/s — быстрее Bevy, на уровне Flecs. Уступает только EnTT (sparse-set).

### 6.2 Intra-system parallelism — топ-3
3.98x speedup на CPU-bound нагрузке. На уровне Flecs и Unity.

### 6.3 API простота — лучший в классе
Единый `ctx.query::<Q>()` — никаких `for_each` на `SystemContext`, `SubWorld`, `Commands`. Bevy имеет 3+ способа итерации, Flecs — 5+.

### 6.4 Встроенный скриптинг — уникальное преимущество
Rhai из коробки. Из топовых ECS только Flecs имеет встроенный скриптинг (Flecs Script).

### 6.5 Лёгкость и минимализм
~10k строк кода, минимум зависимостей. Bevy — ~200k строк.

---

## 7. Слабые стороны Apex ECS

### 7.1 Structural changes — отставание
12.2 M ops/s — в 8x медленнее EnTT. Это плата за архетипную архитектуру.

**Потенциальные улучшения:**
- Batch insert/remove (как `spawn_many` для структурных изменений)
- Sparse-set для компонентов, которые часто меняются индивидуально

### 7.2 Межсистемный параллелизм — отставание
1.07x speedup — практически нет ускорения. У Flecs и Unity в 3-6x лучше.

**Потенциальные улучшения:**
- Partitioned World — разделение архетипов по NUMA-нодам
- Thread-local storage для промежуточных результатов
- Асинхронная загрузка/выгрузка архетипов

### 7.3 Экосистема
Нет плагинов, нет Asset Store, нет сообщества. Bevy и Unity имеют огромное преимущество здесь.

---

## 8. Итоговый вердикт

```
Query iter:         Apex ≈ Flecs > Bevy > Unity
                     EnTT > Apex (sparse-set)

Batch spawn:        Apex ≈ Flecs > Bevy > Unity
                     EnTT > Apex (sparse-set)

Structural insert:  EnTT >>> Flecs > Apex > Bevy > Unity

Intra-system par:   EnTT ≈ Apex ≈ Flecs ≈ Unity > Bevy

Межсистемный par:   Unity > Flecs > Bevy > Apex

API простота:       Apex >>> Bevy > Flecs ≈ EnTT > Unity

Скриптинг:          Apex ≈ Flecs > Unity >>> Bevy ≈ EnTT
```

**Apex ECS конкурирует в сегменте "лёгкий, быстрый, с API как Bevy, но без оверхеда Bevy".** Прямые конкуренты:
- **Bevy** — Apex быстрее, но Bevy имеет экосистему
- **Flecs** — Apex проще API, Flecs быстрее на structural changes
- **EnTT** — Apex имеет планировщик и скриптинг, EnTT быстрее на всём, но C++

**Уникальное торговое предложение Apex ECS:**
> 143 M ops/s query + 3.98x intra-system par + Rhai scripting + JSON serialization + единый API — в одном лёгком крейте без тяжёлых зависимостей.
