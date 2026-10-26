# DNA-DB Core v3.3 «Paneldb Security» (production hardening)

> **English:** [README.md](README.md) · здесь — [русская версия](README_RU.md)

Высокопроизводительный встраиваемый и клиент-серверный движок баз данных на Rust с архитектурой LSM-Tree, 2-битной компрессией нуклеотидов, двухнитевым зеркалированием дисков, языком **DnaQL**, **полноценным SQL-подмножеством** (агрегаты, GROUP BY/HAVING, ORDER BY, IN/подзапросы, **secondary- и диапазонные индексы**), встроенной **веб-панелью управления** (phpMyAdmin-подобной) и самовосстановлением при повреждении секторов.

## 0.2. Production-безопасность (v3.3)

| Контроль | Реализация |
|---|---|
| Транспорт | **rustls TLS 1.2/1.3 на обоих слушателях** (бинарный TCP-протокол и HTTPS-панель), HSTS для HTTPS, plaintext на TLS-порт не принимается |
| Сертификаты | пара PEM через `--tls-cert/--tls-key`; при первом старте генерируется self-signed сертификат для localhost в `DATA/tls/` (только для разработки) |
| Mutual TLS | `--tls-client-ca CA.pem` требует клиентский сертификат, подписанный этим CA, на TCP и HTTPS; в shell — `--tls-client-cert/--tls-client-key` |
| Пароли | **Argon2id** PHC-хеши в `DATA/users.json`, соль на пользователя, выравнивание времени для несуществующих пользователей |
| Пользователи | bootstrap `admin` / `password`, затем создание/отключение/удаление пользователей и сброс/смена пароля в панели; роли `admin`, `operator`, `viewer`; последнего активного администратора удалить нельзя |
| Сессии | 256-битные CSPRNG bearer-токены, TTL 12 ч, отзыв при смене пароля/отключении |
| Brute force | персистентный **lockout по IP и по логину** (5 ошибок / 60 с), глобальный throttle, последовательный Argon2 (ограниченная память) |
| Шифрование at rest | XChaCha20-Poly1305 AEAD для значения до записи в WAL/SST/зеркало; ключ из `DNA_DB_ENCRYPTION_KEY`, `--encryption-key-file` или `DATA/.dna-db.key`; старые plaintext-строки читаются |
| Ротация ключа | offline `rotate-key` копирует все живые строки в новый каталог с новым ключом, сохраняет users/TLS и оставляет rollback-копию |
| Supply chain | `cargo audit` чист (0 уязвимостей, 0 warning); vendored byte-compatible Reed–Solomon убирает `lru`/`instant` advisories |
| Защита панели | CSP с SHA-256-хешем inline-скрипта SPA (без `unsafe-inline` для скриптов), `X-Frame-Options: DENY`, `nosniff`, `no-referrer`, ограничивающий `Permissions-Policy` |
| Pre-auth DoS | лимит pre-auth кадра 64 KiB (вместо 128 × 16 MiB), 10 с таймаут до аутентификации, 64 одновременных соединения панели, 128 TCP-соединений |
| Bind | по умолчанию loopback (`127.0.0.1`); публикация — только явный `--bind` |

```bash
# production-подобный запуск: TLS и шифрование включены по умолчанию.
# При первом старте создаётся admin/password — сразу смените его в панели.
./target/release/dna-db-core serve --data-dir ./dna_data --port 9999 --admin-port 8080
# https://127.0.0.1:8080/  (self-signed: передайте DATA/tls/cert.pem клиентам)

# свой CA / публичный сертификат:
./target/release/dna-db-core serve --data-dir ./dna_data \
  --tls-cert /etc/letsencrypt/live/db.example/fullchain.pem \
  --tls-key  /etc/letsencrypt/live/db.example/privkey.pem \
  --admin-pass 'change-me-now'

# mutual TLS (TCP + панель):
./target/release/dna-db-core serve --data-dir ./dna_data \
  --tls-cert server.pem --tls-key server.key --tls-client-ca clients-ca.pem

# shell по TLS к локальному self-signed серверу:
./target/release/dna-db-core shell --tls-ca ./dna_data/tls/cert.pem

# mTLS shell:
./target/release/dna-db-core shell --tls-ca ca.pem \
  --tls-client-cert client.pem --tls-client-key client.key

# offline-ротация ключа (сервер остановлен; новый ключ — вне DATA):
./target/release/dna-db-core rotate-key \
  --data-dir ./dna_data --new-key-file /secure/dna-new.key

# только для локальной диагностики (не на недоверенной сети):
#   --no-tls   --no-encryption
```

> **Храните ключ шифрования отдельно от каталога данных.** Без ключа значения
> не восстановить. Ключи записей и их количество остаются видимыми в WAL/SST
> by design; AEAD защищает значения приложения, а не ключевое пространство LSM.
> Per-table ACL и внешнего IdP нет; роли глобальные. Модель угроз, процедура
> ротации и остаточные ограничения — в [SECURITY.md](SECURITY.md) и
> [SECURITY_RU.md](SECURITY_RU.md).

## 0.1. Новое в v3.1 — SQL и веб-панель

| Возможность | Детали | Проверка |
|---|---|---|
| Агрегаты | `COUNT(*)/COUNT(col)`, `SUM`, `AVG`, `MIN`, `MAX` | unit-тесты агрегатов |
| `GROUP BY` / `HAVING` | группировка + фильтр по агрегатам (агрегаты в HAVING не протекают в вывод) | unit-тесты |
| `ORDER BY` (ASC/DESC) | сортировка по колонкам/выражениям, в т.ч. по агрегатам | unit-тесты |
| `IN`-списки и подзапросы | `WHERE c IN (1,2,3)` и `WHERE c IN (SELECT …)` в SELECT/UPDATE/DELETE | unit-тесты |
| `LIMIT` / `OFFSET` | пагинация | веб-панель использует |
| **Secondary-индексы** | `CREATE INDEX idx ON t (col)` / `DROP INDEX` — хранятся в схеме, поддерживаются при DML | unit-тесты индексов |
| **IndexScan-планы** | равенство/`IN`/диапазон по индексированной колонке → `IndexScan(...)` вместо `SeqScan` | unit-тесты планов |
| **Веб-панель** | `serve --admin-port 8080`: обзор/здоровье, таблицы + браузер с пагинацией, индексы, SQL- и DnaQL-консоли, управление пользователями | HTTPS/API-интеграция + `scripts/security_probe.py` |
| **Редактирование строк** | клик по строке → модальное окно (phpMyAdmin-стиль): поля по схеме, NULL-галочки, живое SQL-превью, UPDATE/DELETE с подтверждением | ручной e2e-сценарий в браузере |
| **Токенизатор SQL** | отрицательные числа (`-25.5`) и экранирование кавычек по стандарту (`'O''Brien'`) | `tokenizer_negative_numbers_and_escaped_quotes`, `update_with_negative_and_quoted_values` |

```bash
# панель управления (учётки те же, что у TCP-сервера):
./target/release/dna-db-core serve --data-dir ./dna_data --admin-pass secret --admin-port 8080
# затем откройте https://127.0.0.1:8080/ (TLS включён по умолчанию)
```

Скриншоты панели: [docs/screenshots](docs/screenshots).

---

# DNA-DB Core v3.0 «Repairosome» (Self-Healing Release)

Высокопроизводительный встраиваемый и клиент-серверный движок баз данных на Rust с архитектурой LSM-Tree, 2-битной компрессией нуклеотидных последовательностей, двухнитевым зеркалированием дисков (Dual-Strand Hot-Swap), предметно-ориентированным языком **DnaQL**, SQL-подмножеством, проверенной защитой от `kill -9` — и **самовосстановлением при повреждении секторов SSD** по принципу репарации ДНК.

> **Принцип релиза v3.0:** каждое заявление подтверждено кодом и тестами.
> Критерий готовности выполнен: **CHAOS PASS — 100 000/100 000 ключей
> восстановлено** после случайной порчи 4 KiB-секторов
> ([docs/CHAOS_REPORT.md](docs/CHAOS_REPORT.md)).

## 0. Новое в v3.0 — Repairosome (самовосстановление)

Аналогия ДНК → инженерия:

| Механизм в ДНК | Реализация в DNA-DB |
|---|---|
| Избыточность генома | Reed–Solomon GF(2^8) внутри каждого сегмента: k=4 данных + m=2 parity, блоки 4 KiB (формат `DNASST02`) |
| Датчики повреждений (checkpoint-киназы) | CRC32C на каждый 4 KiB-блок (SSE4.2), двойная копия таблицы CRC, резервированные суперблок/футер |
| Эксцизионная репарация (BER/NER) | on-read repair: CRC-мисс → реконструкция страйпа из выживших шардов → перезапись → журнал |
| Ферменты-сканеры | фоновый scrubber (по умолчанию включён: 24 ч, 32 MiB/s, настраивается) |
| Гомологичная рекомбинация | каскад: parity → копия сегмента на Стрэнде B; авто-resync после ремонта |
| Журнал мутаций | `repair_journal.jsonl` + метрики lesions_found/repaired/unrecoverable |
| Иммунный надзор | `dna-db-core doctor --data-dir D [--fix] [--migrate] [--json]` — офлайн-скан, ремонт, миграция v2.1→v3.0 |
| Граница клетки | карантин: непоправимый страйп честно возвращает ошибку с локализацией segment:stripe:shard, БД продолжает работать |

Доказательства (все — исполняемые):

- **Chaos-тест**: `python3 scripts/chaos_test.py` — 100 000 ключей → порча 80 секторов (bit-flip/zero-fill) → `doctor --fix` → 100 % восстановление; live-режим (порча на работающем сервере); over-damage (честный отказ за границей гарантии). Результаты: [docs/CHAOS_REPORT.md](docs/CHAOS_REPORT.md).
- **Тесты**: 174 зелёных (`cargo test --release`) — 159 lib + 15 integration, включая RS-кодек, репарацию, карантин, миграцию, ротацию WAL, parity закрытых WAL и encryption integration.
- **Формат**: DNASST02 задокументирован в [docs/PROTOCOL.md](docs/PROTOCOL.md); DNASST01 читается, миграция — вручную `doctor --migrate` (приоритет — надёжность).

Быстрый старт самовосстановления:

```bash
cargo build --release
# эксплуатация (scrubber включён по умолчанию):
./target/release/dna-db-core serve --data-dir ./dna_data --admin-pass secret
# диагностика и ремонт (офлайн):
./target/release/dna-db-core doctor --data-dir ./dna_data            # скан
./target/release/dna-db-core doctor --data-dir ./dna_data --fix      # ремонт
./target/release/dna-db-core doctor --data-dir ./dna_data --migrate  # v2.1 → v3.0
# в сессии: SHOW HEALTH;  /  VERIFY CHROMOSOME chr_users;  /  REPAIR CHROMOSOME chr_users;
```

---

# DNA-DB Core v2.1.0 (базовая функциональность)


Высокопроизводительный встраиваемый и клиент-серверный движок баз данных на Rust с архитектурой LSM-Tree, 2-битной компрессией нуклеотидных последовательностей (4 бита/основание → 0.25 байта), двухнитевым зеркалированием дисков (Dual-Strand Hot-Swap), предметно-ориентированным языком **DnaQL**, SQL-подмножеством и проверенной защитой от `kill -9`.

> **Принцип релиза v2.1.0:** каждое заявление в этом README подтверждено кодом и тестами. Всё, что не реализовано, честно помечено в [ROADMAP_RU.md](docs/ROADMAP_RU.md).

## 1. Что реально работает (и чем доказано)

| Возможность | Реализация | Проверка |
|---|---|---|
| Хранилище WAL + MemTable + SSTable | `wal.rs`, `memtable.rs`, `segment.rs` | 21 unit-тест + 9 интеграционных |
| Восстановление после kill -9 | WAL replay в `Engine::open`, авто-обрезка torn tail | `scripts/kill9_test.py`: **99 091 записей, 0 потеряно** |
| Flush на диск + SegmentReader | bloom-фильтр + sparse index + CRC32 на запись | unit-тесты, reopening тесты |
| Компакция сегментов | merge-all, свежие версии побеждают | `compaction_merges_and_preserves_data` |
| Атомарные транзакции | batch-запись WAL (один fsync = один атомарный интервал) | `write_batch_is_atomic_on_recovery` |
| DnaQL (Standard + Quick) | токенайзер → парсер → исполнитель над реальным хранилищем | 8 unit-тестов + живой smoke |
| SQL-подмножество | SELECT/JOIN(WHERE/IN-подзапросы)/GROUP BY/HAVING/ORDER BY/LIMIT/OFFSET, агрегаты, INSERT, UPDATE, DELETE (CASCADE), CREATE/DROP TABLE, **CREATE/DROP INDEX + IndexScan** | 40+ unit-тестов |
| Веб-панель администрирования | HTTP/1.1 + SPA (без внешних зависимостей): обзор, таблицы, консоли SQL/DnaQL | `admin::tests::panel_requires_auth_and_serves_api` |
| Bitap/Myers нечёткий поиск | корректный Wu-Manber, **истинная** дистанция, Levenshtein + Hamming | фаззинг 8000 кейсов vs эталонный DP |
| Bio-плагины | настоящий Smith-Waterman, PAM-сканер, ORF-finder, GC-окна | unit-тесты |
| TCP-сервер | thread-per-connection, TLS, фреймы 16 MiB (64 KiB до auth), Argon2id, персистентный IP/username-lockout | unit-тесты + `scripts/security_probe.py` |
| CLI | `serve` / `bench` / `shell` | ручная проверка |
| Метрики | реальные счётчики (ops, байты, средние латентности) | unit-тест |

## 2. Быстрый старт

### Сборка
```bash
cargo build --release
# бинарник: target/release/dna-db-core
```

### Запуск сервера
```bash
./target/release/dna-db-core serve \
  --data-dir ./dna_data \
  --port 9999 \
  --admin-pass password \
  --admin-port 8080 \
  --sync strict \
  --demo-data          # опционально: демо-таблицы chr_users/chr_orders/chr_patients/chr_variants
```
TLS и шифрование payload включены по умолчанию. На новом каталоге данных
bootstrap-учётка — **`admin` / `password`**; сразу смените её в панели.
Для явного пароля первого старта используйте `--admin-pass` или `DNA_DB_ADMIN_PASS`.
После запуска откройте **https://127.0.0.1:8080/** (сгенерированный сертификат
self-signed; передайте `DATA/tls/cert.pem` CLI/HTTP-клиентам или укажите реальный).
`--admin-port 0` отключает панель; панель слушает тот же интерфейс, что и `--bind` (по умолчанию loopback).

### Shell-клиент
```bash
./target/release/dna-db-core shell --port 9999 --tls-ca ./dna_data/tls/cert.pem
dna-db> SELECT order_id, username, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE total_usd > 400;
dna-db> ANALYZE SEQUENCE 'ATGGCCTAA' |> GC_CONTENT() |> TRANSLATE();
dna-db> .status
```

### Как библиотека (Cargo)
```rust
use dna_db_core::{Engine, EngineConfig, SyncMode};

let engine = Engine::open(EngineConfig {
    dir_path: "./dna_data".into(),
    memtable_capacity: 8 * 1024 * 1024,
    sync_mode: SyncMode::Strict,
    mirror_disk_path: None,
})?;

engine.put(b"chr1:100", b"ATGCCCGGTAAA")?;
let value = engine.get(b"chr1:100")?; // Some(b"ATGCCCGGTAAA")

engine.checkpoint()?;        // flush всех memtable + усечение WAL
engine.compact()?;           // слияние сегментов
```

## 3. DnaQL — язык запросов

Два равноправных синтаксиса (см. [DNAQL_SPEC.md](docs/DNAQL_SPEC.md) и [DNAQL_MANUAL_RU.md](docs/DNAQL_MANUAL_RU.md)):

```dnaql
-- Быстрый: нечёткий поиск с линковкой на пациента
chr_variants ? 'GATCGATC' ~1 -> chr_patients(patient_id) => emit locus, mismatches, name;

-- Стандартный: то же самое формально
FIND PATTERN 'GATCGATC'
IN CHROMOSOME chr_variants
WHERE HAMMING_DISTANCE <= 1
LINK chr_patients VIA COMPLEMENT(patient_id)
EMIT locus, mismatches, name;

-- Реляционный пайплайн
FROM chr_orders |> WHERE total_usd > 300 |> LINK chr_users VIA COMPLEMENT(user_id) |> EMIT order_id, username;

-- Атомарная транзакция (один fsync = один интервал durability)
TX {
  chr_accounts:14.balance -= 350.00;
  chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 };
}

-- Био-анализ
ANALYZE SEQUENCE 'CGCGCGATCGATCGCGC' |> GC_CONTENT() |> MELTING_TEMP() |> REV_COMP() |> TRANSLATE();
```

`LINK VIA COMPLEMENT` — это O(1) прямой key-seek по первичному ключу целевой хромосомы (никаких hash join).

## 4. SQL-подмножество (v3.1 — полностью рабочий)

```sql
CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id));
CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT,
  PRIMARY KEY (order_id),
  FOREIGN KEY (user_id) REFERENCES chr_users(user_id) ON DELETE CASCADE);

INSERT INTO chr_users (user_id, username, balance) VALUES (14, 'prof_crick', 890.00);
INSERT INTO chr_users (user_id = 18, username = 'rosalind_f', balance = 12.5);

-- проекция, фильтры, IN-списки и подзапросы, сортировка, пагинация
SELECT order_id, total_usd FROM chr_orders
  WHERE status IN ('paid', 'shipped')
    AND user_id IN (SELECT user_id FROM chr_users WHERE balance > 100)
  ORDER BY total_usd DESC LIMIT 10 OFFSET 20;

-- агрегаты + группировка + фильтр по агрегатам
SELECT status, COUNT(*) AS n, SUM(total_usd) AS revenue
  FROM chr_orders GROUP BY status HAVING COUNT(*) > 1 ORDER BY revenue DESC;

-- secondary-индексы: равенство, IN и диапазоны идут через IndexScan, а не SeqScan
CREATE INDEX idx_orders_status ON chr_orders (status);
CREATE INDEX idx_orders_total  ON chr_orders (total_usd);
EXPLAIN SELECT order_id FROM chr_orders WHERE total_usd > 100 AND total_usd < 600;
--  -> IndexScan(chr_orders.total_usd >100 AND <600) -> Filter(...) -> Sort(...)
DROP INDEX idx_orders_status ON chr_orders;

UPDATE chr_users SET balance = 950.00 WHERE user_id = 14;
DELETE FROM chr_users WHERE user_id = 14;   -- каскад по chr_orders
DROP TABLE chr_orders;
```
INSERT поддерживает две формы: позиционную `(cols) VALUES (vals)` и типизированную `(col = val, ...)`.
Доступ по первичному ключу — point-lookup (bloom + sparse index), по secondary-индексу — `IndexScan`;
всё остальное — честный `SeqScan` с фильтром. JOIN выполняется hash-join'ом по FK-комплементарному указателю.
В планах видно, что реально сделано: `SeqScan(t) -> InnerHashJoin(a.x, b.y) -> Filter(...) -> Project([...])`.

Полный SQL-мануал: [docs/SQL_MANUAL_RU.md](docs/SQL_MANUAL_RU.md).

## 5. Долговечность (durability)

- `--sync strict` — fsync каждой записи **до** подтверждения клиенту. Проверено `kill -9`: 99 091 подтверждённых записей — 0 потерь.
- `--sync batched` — буферизация, fsync каждые 64 KiB (при crash теряется незарезервированный хвост; при штатном выключении буфер сбрасывается в `Drop`).
- `--sync async` — только page cache (для эфемерных нагрузок).
- Torn writes: CRC32 на каждый кадр WAL и каждую запись сегмента; повреждённый хвост автоматически обрезается при открытии.

## 6. Архитектура

```
                  DnaQL / SQL / KV
                        |
                 ┌──────▼──────┐
                 │    Engine   │  stats, checkpoint, compact, scan
                 └──┬───────┬──┘
        WAL (CRC32) │       │ ComplementIndex (O(1) FK)
                    │       │
   ┌────────────────▼──┐  ┌─▼──────────────┐
   │ MemTable (skiplist)│  │ DualStrandMirror│  Strand Beta WAL
   └─────────┬─────────┘  └────────────────┘
             │ capacity
   ┌─────────▼──────────────────────┐
   │ SSTable segments (+bloom sidecar, sparse index, CRC) │
   │ cold/ tier (TieredStorageManager)                    │
   └──────────────────────────────────────────────────────┘
```

Ключи: `<chromosome>:<locus>` (строки таблиц — RowCodec-бинарные payload'ы), `__schema:<table>` — JSON-схемы.
Полное описание форматов: [PROTOCOL.md](docs/PROTOCOL.md).

## 7. Тесты и бенчмарки

```bash
cargo test --release                # 174 тестов (159 lib + 15 integration)
python3 scripts/kill9_test.py       # kill -9 durability (нужен собранный release-бинарник)
python3 scripts/chaos_test.py       # порча секторов -> doctor --fix -> 100% восстановление
./target/release/dna-db-core bench  # реальные замеры (см. docs/BENCHMARKS.md)
```

Результаты полного прогона проверок: [docs/TEST_RESULTS_RU.md](docs/TEST_RESULTS_RU.md).

Сравнительный бенчмарк **DNA-DB vs MySQL vs PostgreSQL** (графики, методика, сырые данные):
[docs/DB_COMPARISON_RU.md](docs/DB_COMPARISON_RU.md).

Таблица соответствия SQL-фич (JOIN / агрегаты / индексы, 32 проверки на всех трёх системах):
[docs/SQL_FEATURES_RU.md](docs/SQL_FEATURES_RU.md).

Скриншоты веб-панели и отчёты о тестах: [docs/screenshots](docs/screenshots), [docs/](docs/).

## 8. Лицензия и статус

Проект использует **сплит-лицензию** — разные участки под разными лицензиями:

| Участок | Лицензия | Файлы |
|---|---|---|
| Веб-панель администрирования | **Apache-2.0** | `src/admin.rs`, `src/admin.html` |
| Ядро движка, SQL/DnaQL, TCP-сервер, тесты, скрипты, документация | **AGPL-3.0-only** | все остальные файлы |

Каждый исходный файл несёт строку SPDX, однозначно определяющую его
лицензию (`// SPDX-License-Identifier: …`). Полные тексты:
[LICENSE-AGPL](LICENSE-AGPL), [LICENSE-APACHE](LICENSE-APACHE), сводка —
[LICENSE](LICENSE).

**Почему так:** ядро движка — AGPL-3.0-only, поэтому любые модификации
и сетевое использование остаются открытыми; интерактивная веб-панель —
пермиссивная Apache-2.0 (как у phpMyAdmin), её можно свободно встраивать
и переиспользовать. Apache-2.0 односторонне совместима с AGPL-3.0:
собранный бинарник (ядро + панель) распространяется на условиях
AGPL-3.0 с сохранением уведомлений Apache в исходниках панели.

Релиз v2.1.0 — рабочий движок нового типа (геномно-реляционная модель), пригодный для встраивания и клиент-серверной эксплуатации. Ограничения текущей версии (MVCC, distributed 2PC, io_uring, WASM-плагины, длинные паттерны bitap) зафиксированы в [docs/ROADMAP.md](docs/ROADMAP.md).
