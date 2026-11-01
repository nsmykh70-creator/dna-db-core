# DNA-DB Core v3.0.0 — BENCHMARKS (реальные замеры)

Машина: контейнер CI-песочницы (Debian, x86_64), файловая система overlayfs.
Сборка: `cargo build --release` (opt-level=3, lto=thin, codegen-units=1).
Rust: 1.99.1. Дата: 2026-10-19.

> ⚠️ **Важная оговорка о fsync:** в песочнице `sync_data()` почти бесплатен
> (overlayfs/tmpfs). На реальном NVMe добавьте ~50–300 мкс на каждый fsync
> в режиме strict; групповой commit (ROADMAP P1) сгладит это для
> многопоточных нагрузок. Числа ниже — честные замеры THIS environment.

```
ops: 50 000            (bench subcommand, default 50k)
[PUT strict-fsync]  50 000 ops in 0.048s = 1 033 138 ops/s, avg 0.97 us/op
[GET random]        25 000 ops in 0.020s = 1 275 432 ops/s, avg 0.78 us/op
[SCAN prefix]       50 000 rows in 0.014s = 14 164 us total
[BATCH x10 atomic]  5 000 batches (50 000 rows) in 0.029s = 173 777 batches/s
[BITAP Levenshtein] 100 000 bp, k<=1: 296 matches in 1.062 ms = 94.1 Mbp/s
[BITAP Hamming]     100 000 bp, k<=1: 127 matches in 0.989 ms = 101.1 Mbp/s
```

## v3.5.0 — сравнительный прогон с MySQL 8.4.6 и PostgreSQL 17.11 (2026-10-31, Linux)

Одинаковая нагрузка и RNG на всех трёх системах, у каждой — durability по умолчанию
(DNA-DB `--sync strict`, MySQL `innodb_flush_log_at_trx_commit=1`, PG `synchronous_commit=on`).
Сырые данные: [bench_compare_results_v350_linux.json](bench_compare_results_v350_linux.json),
графики: [charts/v350/](charts/v350/), отчёт: [DB_COMPARISON_RU.md](DB_COMPARISON_RU.md).


| Фаза (одинаковая нагрузка и RNG) | DNA-DB v3.5.0 | MySQL 8.4.6 | PostgreSQL 17.11 |
|---|---:|---:|---:|
| Load 10 000 KV rows, single connection (ack per row) | **36 260 rows/s** | 4 353 rows/s | 19 182 rows/s |
| Multithreaded load, 8 threads × 1 000 single-row INSERTs | 17 636 rows/s | 8 316 rows/s | **22 157 rows/s** |
| Batched load, 8 000 rows in 40 × 200-row INSERTs | **298 936 rows/s** | 162 166 rows/s | 215 926 rows/s |
| Point read ×4 000, single connection | **43 779 ops/s** | 11 049 ops/s | 17 565 ops/s |
| Mixed 80/20, 8 threads × 500 ops | **20 411 ops/s** | 8 403 ops/s | 17 985 ops/s |
| SQL `INSERT`, 3 000 rows | **24 986 ops/s** | 4 003 ops/s | 16 811 ops/s |
| SQL `SELECT … WHERE pk`, ×1 500 | 22 470 ops/s | 8 916 ops/s | **23 638 ops/s** |
| Verified reads returning a wrong value | **0** | 0 | 0 |

Итог: DNA-DB быстрее MySQL во всех 7 фазах (1,8–8,3×) и PostgreSQL в 5 из 7
фаз; PostgreSQL выигрывает в пике параллельных одиночных INSERT (22,2k против
17,6k rows/s) и чуть впереди в SQL SELECT по PK (23,6k против 22,5k ops/s).
Неверных значений при проверке чтений — 0 у всех трёх систем.

---

## Выводы

- **Throughput точечных операций** упирается в блокировки WAL (одна RwLock на append) — масштабирование записи по ядрам ожидается от группового commit.
- **Bitap**: ~100 Mbp/s на одном ядре — скалярный bit-parallel Myers. Векторизация (AVX2) — ROADMAP P3.
- **Префиксный скан** 50k строк ≈ 14 мс — оптимизируется range-итераторами с слиянием слоёв (ROADMAP P1), сейчас — merged map.

## Долговечность (главный результат)

```
[TEST] Sending SIGKILL (kill -9) to PID ... during 8-thread concurrent write load
[TEST] Total acknowledged writes before crash: 99 090
[TEST] Restarting DNA-DB server to trigger WAL replay and torn-write auto-recovery...
[RESULT] Total checked: 99 091 | Missing: 0 | Corrupt: 0
>>> SUCCESS: 100% DURABILITY VERIFIED UNDER SIGKILL (kill -9) <<<
```

Повторить: `cargo build --release && python3 scripts/kill9_test.py`.

---

# v3.0 «Repairosome» — оверхед кодирования и скорость репарации

Дата: 2026-10-22. Та же машина/сборка, что и выше. Геометрия по умолчанию:
RS(4+2), блоки 4 KiB, CRC-32 (IEEE, `crc32fast`) на блок.

## Запись и чтение (bench, 50 000 ops)

```
v3.0 (RS 4+2)                          v2.1.0 (тот же прогон)
[PUT strict-fsync] 738 882 ops/s       [PUT strict-fsync] 1 033 138 ops/s
[GET random]      1 059 534 ops/s      [GET random]      1 275 432 ops/s
[SCAN prefix]      15 690 us           [SCAN prefix]      14 164 us
[BATCH x10]       152 798 b/s          [BATCH x10]       173 777 b/s
```

- **Оверхед записи ≈ −28 %**: на flush сегмент кодируется страйпами
  RS(4+2) (encode_single GF(2^8)) + подсчёт CRC-32 на каждый блок.
  Ожидаемо по дорожной карте (§6: «запись — кодирование страйпов на flush,
  замерим в Ф6»). Чтение — CRC-32 на блок при проходе: ≈ −17 %.
- Место: +50 % (два parity-блока на четыре дата-блока), как и заявлено.
- Стоимость — плата за математическое восстановление до 2 битых блоков
  из 6 на каждом страйпе каждого сегмента.

## Репарация (chaos-тест, seed 20261021)

| Операция | Скорость |
|---|---|
| Полный офлайн-скан сегментов (CRC-32 каждого блока) | ~3.2 MB/s×100 сегментов: 80 бляшек найдены за 0.1 с |
| Реконструкция одного страйпа (из ≤ m битых шардов) | < 1 мс (RS(4+2) над 6×4 KiB) |
| On-read repair (прозрачный для клиента) | +1 перезапись блока + синхронизация: чтение после порчи остаётся ~10⁵ ops/s |
| Doctor `--fix` 80 битых секторов | 0.1 с суммарно, включая полный скан |

## Выводы

- Деградация записи −28 % и места +50 % — цена за самовосстановление
  4 KiB-секторов; конфигурируется (k=8/m=4 → +50 % места, но большие страйпы
  и меньше кодирования на байт для холодных данных).
- Полный прогон chaos-критерия (100 000 ключей, 80 битых секторов,
  doctor, рестарт, верификация всех ключей) — ~6 секунд в песочнице.

## Потребление ресурсов: RAM и диск — DNA-DB 4.0.0 vs MySQL 8.0.46 vs PostgreSQL 16.15 (2026-11-01, Linux)

Методика: настройки по умолчанию у всех; RAM — сумма PSS всех процессов СУБД
(в покое после создания схемы и в пике под нагрузкой, опрос каждые 50 мс);
диск — прирост каталога данных после корректной остановки (включая WAL/binlog).
Одинаковый детерминированный датасет seed 42, одиночные вставки с ack на каждую.
Оба масштаба — один прогон GitHub Actions (ubuntu-24.04); между масштабами базы
удалялись, binlog усекался (RESET MASTER), PostgreSQL перезапускался.
Для DNA-DB дополнительно выполнена офлайн-компакция (`dna-db-core compact`:
checkpoint + слияние всех сегментов, удаление затёртых версий и надгробий).
Скрипт: [measure_resources.py](../scripts/measure_resources.py), сырые данные:
[resources_compare_linux.json](resources_compare_linux.json) (100k),
[resources_compare_linux_1m.json](resources_compare_linux_1m.json) (1M).

### 100 000 строк (значение 160 Б, 4 000 чтений)

| Метрика | DNA-DB 4.0.0 | DNA-DB после компакции | MySQL 8.0.46 | PostgreSQL 16.15 |
|---|---|---|---|---|
| RAM в покое (сумма PSS), MiB | 22.4 | — | 376.7 | **22.0** |
| RAM в пике (нагрузка + чтения), MiB | 69.6 | — | 410.1 | **55.5** |
| Прирост каталога данных, MiB | **42.2** | **29.8** | 59.1 | 56.1 |

### 1 000 000 строк (значение 160 Б, 20 000 чтений)

| Метрика | DNA-DB 4.0.0 | DNA-DB после компакции | MySQL 8.0.46 | PostgreSQL 16.15 |
|---|---|---|---|---|
| RAM в покое (сумма PSS), MiB | **22.5** | — | 388.7 | 22.9 |
| RAM в пике (нагрузка + чтения), MiB | **127.6** | — | 503.5 | 166.5 |
| Прирост каталога данных, MiB | **535.5** | **297.9** | 635.4 | 556.4 |

- На 1 000 000 строк DNA-DB потребляет в пике **3,9× меньше RAM, чем MySQL**
  (127,6 против 503,5 MiB) и на 23 % меньше, чем PostgreSQL; в покое — 22,5 MiB
  против 388,7 MiB у MySQL (17,3×).
- Офлайн-компакция ужимает каталог DNA-DB с 535,5 до **297,9 MiB** (−44 %):
  оверхед к полезной нагрузке (164 MiB) падает с ×3,3 до **×1,8**; у MySQL ×3,9,
  у PostgreSQL ×3,4. После компакции DNA-DB занимает на **53 % меньше диска,
  чем MySQL**, и на 46 % меньше, чем PostgreSQL; на 100 000 строк — 29,8 MiB
  (−50 % к MySQL, −47 % к PostgreSQL).
