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
RS(4+2), блоки 4 KiB, CRC32C на блок (SSE4.2).

## Запись и чтение (bench, 50 000 ops)

```
v3.0 (RS 4+2)                          v2.1.0 (тот же прогон)
[PUT strict-fsync] 738 882 ops/s       [PUT strict-fsync] 1 033 138 ops/s
[GET random]      1 059 534 ops/s      [GET random]      1 275 432 ops/s
[SCAN prefix]      15 690 us           [SCAN prefix]      14 164 us
[BATCH x10]       152 798 b/s          [BATCH x10]       173 777 b/s
```

- **Оверхед записи ≈ −28 %**: на flush сегмент кодируется страйпами
  RS(4+2) (encode_single GF(2^8)) + подсчёт CRC32C на каждый блок.
  Ожидаемо по дорожной карте (§6: «запись — кодирование страйпов на flush,
  замерим в Ф6»). Чтение — CRC32C на блок при проходе: ≈ −17 %.
- Место: +50 % (два parity-блока на четыре дата-блока), как и заявлено.
- Стоимость — плата за математическое восстановление до 2 битых блоков
  из 6 на каждом страйпе каждого сегмента.

## Репарация (chaos-тест, seed 20261021)

| Операция | Скорость |
|---|---|
| Полный офлайн-скан сегментов (CRC32C каждого блока) | ~3.2 MB/s×100 сегментов: 80 бляшек найдены за 0.1 с |
| Реконструкция одного страйпа (из ≤ m битых шардов) | < 1 мс (RS(4+2) над 6×4 KiB) |
| On-read repair (прозрачный для клиента) | +1 перезапись блока + синхронизация: чтение после порчи остаётся ~10⁵ ops/s |
| Doctor `--fix` 80 битых секторов | 0.1 с суммарно, включая полный скан |

## Выводы

- Деградация записи −28 % и места +50 % — цена за самовосстановление
  4 KiB-секторов; конфигурируется (k=8/m=4 → +50 % места, но большие страйпы
  и меньше кодирования на байт для холодных данных).
- Полный прогон chaos-критерия (100 000 ключей, 80 битых секторов,
  doctor, рестарт, верификация всех ключей) — ~6 секунд в песочнице.
