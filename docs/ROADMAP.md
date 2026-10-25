# DNA-DB Core — ROADMAP

## Выполнено в v3.1 (панель + полноценный SQL)

Дата: 2026-10-25

| Было в списке ограничений | Статус v3.1 | Где смотреть |
|---|---|---|
| Нет secondary/диапазонных индексов — WHERE по не-PK-колонкам = полный скан | **Закрыто**: `CREATE/DROP INDEX`, хранение в схеме, поддержка в DML, планы `IndexScan` для равенства/`IN`/диапазона | `sql_engine.rs` (index_*), unit-тесты индексов, [SQL_MANUAL_RU.md §3.4](SQL_MANUAL_RU.md) |
| Нет агрегатов/ORDER BY/GROUP BY | **Закрыто**: COUNT/SUM/AVG/MIN/MAX, GROUP BY + HAVING, ORDER BY ASC/DESC, LIMIT/OFFSET | unit-тесты агрегатов |
| Нет подзапросов | **Частично закрыто**: `WHERE c IN (SELECT …)` материализуется (в SELECT/UPDATE/DELETE); коррелированных подзапросов нет | unit-тесты IN-subquery |
| Нет веб-панели администрирования | **Закрыто**: встроенная панель `serve --admin-port` (обзор/здоровье, браузер таблиц, индексы, SQL/DnaQL-консоли, HTTP API) | `admin.rs`, `admin.html`, тест `panel_requires_auth_and_serves_api` |
| Нет репликации/кластера (mirror = два локальных диска) | Осталось P2 (см. «Репликация» ниже) | — |
| Протокол без TLS | Осталось P2 — рекомендация: SSH/stunnel/VPN-туннель | README §Ограничения |
| JOIN-планы только hash по FK | Осталось P2 (индексные join-планы) | — |

Тесты v3.1: `cargo test --release` → **147 passed, 0 failed, 0 warnings**.

---

# DNA-DB Core — ROADMAP релиза v2.1.0

Дата: 2026-10-19
Базовая версия: v2.0.0 («DnaQL production» — аудит показал, что значительная часть функций была заглушками)

---

## Часть I. Выполнено в v2.1.0 (демо-код заменён на рабочий)

### Критические исправления (P0)

| # | Проблема в v2.0 | Решение в v2.1 | Тест |
|---|---|---|---|
| 1 | `WalReader::recover` существовал, но нигде не вызывался — данные не восстанавливались | Recovery подключён в `Engine::open`: WAL replay поверх сегментов | `crash_recovery_preserves_all_data`, kill9_test |
| 2 | `SegmentWriter` никогда не вызывался — flush-путь отсутствовал, «LSM» была только memtable | Полный путь: ротация → SSTable → чтение из сегментов | `flush_creates_readable_segments` |
| 3 | `SegmentReader` не существовал | Реализован: bloom pre-pass + sparse index binary search + CRC32 | 4 unit-теста |
| 4 | Bitap/Myers пропускал валидные совпадения (>50% случаев, фаззинг), `mismatches` всегда = max_distance | Корректная инициализация `R[d]=(1<<d)-1`, истинная дистанция, восстановление кратчайшего окна выравнивания | фаззинг 8000 кейсов vs эталонный DP |
| 5 | SQL-движок игнорировал текст запроса, возвращал захардкоженные строки | Токенайзер + парсер + исполнитель: SELECT/JOIN/WHERE/LIMIT/INSERT/UPDATE/DELETE/CREATE/DROP | 9 unit-тестов |
| 6 | DnaQL-парсер возвращал фиктивные AST, тайминги фальсифицировались (`.max(18)`) | Настоящий парсер EBNF-подмножества, реальное время, реальные данные | 8 unit-тестов + smoke |
| 7 | «2PC Polymerase»: prepare() менял только статус, commit не был атомарным | commit = одна batch-запись WAL = один fsync = один атомарный интервал; честное именование (single-node atomic commit) | `write_batch_is_atomic_on_recovery` |
| 8 | Пароль хранился открытым текстом (поле называлось `hash`) | salted SHA-256 + constant-time сравнение (subtle) | `lockout_after_max_attempts` |
| 9 | Сервер отсутствовал (только SecurityManager), `kill9_test.py` не к чему было подключать | TCP-сервер: thread-per-connection, фреймы ≤16MB, совместим с wire-форматом kill9_test | 3 unit-теста + live smoke |
| 10 | Ротация memtable пропускалась при занятом immutable → неограниченный рост (анти-OOM claim нарушен) | `ensure_capacity`: синхронный flush под flush_lock с double-check | `flush_creates_readable_segments` |

### Новые модули и функции

| Модуль | Что делает |
|---|---|
| `bloom.rs` | Детерминированный сериализуемый bloom-фильтр (замена fastbloom, auditable, k=4, FNV1a+double hashing) |
| `complement_index.rs` | Настоящий O(1) FK-индекс (Base-Pairing Complementarity): поддерживается на INSERT/DELETE, статистика hits/misses |
| `bio_plugins.rs` | Реальный Smith-Waterman (DP + traceback), CRISPR PAM-сканер (NGG/NRG), ORF-finder, sliding-window GC |
| `stats.rs` | Реальные счётчики: ops, байты, средние латентности put/get, flush/compaction, uptime |
| `expr.rs` | Общий AST выражений (AND/OR/NOT, сравнения, IS NULL) для WHERE в DnaQL и SQL |
| `RowCodec` (relational.rs) | Бинарный типизированный кодек строк (tag-length-value): Null/Int/Float/Text/Dna |
| `checkpoint()` | Flush всего + усечение WAL (точка durability) |
| `compact()` | Полная компакция сегментов с удалением поглощённых tombstone |
| `scan_prefix()` | Сортированный merged-скан по префиксу через все слои |
| `write_batch()` | Атомарная мульти-запись (один WAL-кадр) |
| CLI `serve/bench/shell` | Сервер, честные бенчмарки, интерактивный REPL-клиент |
| Drop для Engine | Сброс WAL-буфера при штатном выключении (Batched/Async) |

### Удалённые заглушки

| Модуль v2.0 | Причина удаления | Замена |
|---|---|---|
| `iouring_wal.rs` | `append_zero_copy` только инкрементировал счётчик, I/O не было | Честные SyncMode-режимы; group commit в ROADMAP |
| `wasm_runtime.rs` | Возвращал строку «executed, 0 memory leaks» без выполнения | `bio_plugins.rs` (реальные функции) |
| `helix_ql.rs` (v1) | Мёртвый предшественник DnaQL | — |

---

## Часть II. Дорожная карта далее (v2.2 → v3.0)

### P1 — ближайшие релизы

1. **MVCC / изоляция снимков** — TxIsolationLevel::SnapshotIsolation/Serializable сейчас декларативны (фактически Read Committed). План: timestamp-ordered ключи + GC версий.
2. **Range-запросы и итераторы** — `Engine::range(start..end)` со слитой сортированной итерацией по слоям (сейчас — scan_prefix).
3. **Групповой commit** — при >1 параллельных клиентах объединять fsync нескольких подтверждений в один интервал (эффект: xN throughput на реальном NVMe).
4. **Блочная компоновка SSTable** — сжатие блоков (zstd/lz4) + page-level CRC; сейчас записи идут подряд без блоков.
5. **Длинные паттерны Bitap (>64 bp)** — chunked/seed-and-extend поиск; сейчас честный отказ при m>64.
6. **WAL-сегментация** — ротация active.wal по размеру вместо усечения только на checkpoint.
7. **Schema DDL в DnaQL** — `CREATE CHROMOSOME ... (locus INT, sequence DNA)` для симметрии с SQL.
8. **Аутентификация токенами** — token-based сессии поверх user:pass (заготовка есть в wire-протоколе).

### P2 — инфраструктура

9. **Бэкапы и снапшоты** — консистентный снапшот (checkpoint + копия сегментов) + восстановление из архива.
10. **Репликация** — сегодня DualStrandMirror — это зеркальный WAL одного узла; следующий шаг — потоковая репликация на второй узел с replay.
11. **Distributed 2PC** — настоящий двухфазный коммит для мультиузловых транзакций (координатор + участники, prepared-лог).
12. **io_uring WAL** — реальная реализация (SQPOLL, registered buffers) под Linux ≥5.10 за feature-флагом.
13. **Клиентские SDK** — Python-клиент (готовый wire-клиент уже в smoke-тестах), Rust client crate.
14. **WASM-плагины** — песочница wasmtime для пользовательских bio-функций (трейт BioPluginFn уже есть).

### P3 — производительность

15. **Векторизация bitap** — AVX2/NEON для параллельного скана нескольких паттернов; авто-детект через `is_x86_feature_detected!`.
16. **Tiered storage автоматика** — фоновый поток: демоция холодных сегментов по возрасту/размеру (ручной API готов).
17. **Бенчмарки на реальном железе** — текущие числа получены на overlayfs-песочнице (fsync ≈ бесплатный); нужен прогон на NVMe с dataset.tar и публикация в BENCHMARKS.md.
18. **Фаззинг инфраструктура** — cargo-fuzz таргеты на WAL-recover и SQL/DnaQL парсеры.

### Технический долг (принятые компромиссы v2.1)

- Схемы таблиц хранятся как serde_json (`__schema:*`) — при желании заменить на бинарный формат.
- Текстовые PK, совпадающие с числовыми (`"14"` vs `14`), дают разные ключи — коллизии исключены форматом Value::Display, но документировать семантику.
- `hash_join_left` вычисляет matched-набор повторно — микрооптимизация на P2.
- INSERT не проверяет типы колонок против схемы (Value самодостаточен) — добавить строгую типизацию.
