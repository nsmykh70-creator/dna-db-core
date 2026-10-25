# DNA-DB Core — Результаты тестов

> **English:** [TEST_RESULTS.md](TEST_RESULTS.md)
> Здесь приведены реальные, воспроизводимые результаты полного прогона проверок.

* **Дата прогона:** 2026-10-25 (21:27–21:56 +03:00)
* **Коммит:** рабочее дерево `main` (профиль release)
* **Инструментарий:** rustc 1.98.1, cargo 1.98.1, Python 3.12.9
* **Платформа:** Windows 10/11 x86_64 (MINGW64), `panic = "abort"`, LTO thin
* **Сырой лог Rust-прогона:** [cargo_test_output.txt](cargo_test_output.txt)
* **CI:** [.github/workflows/ci.yml](../.github/workflows/ci.yml) повторно
  запускает unit/интеграционные тесты и clippy на каждый push в `main`

---

## Сводка

| # | Проверка | Команда | Результат |
|---|---|---|---|
| 1 | Сборка release | `cargo build --release` | **PASS** — 1m 14s, 0 предупреждений |
| 2 | Unit-тесты (lib) | `cargo test --release` | **PASS** — 139 passed, 0 failed (0.63s) |
| 3 | Интеграционные тесты | `cargo test --release` | **PASS** — 13 passed, 0 failed (12.06s) |
| 4 | Линт | `cargo clippy --release --all-targets` | **PASS** — 0 ошибок, 67 предупреждений (стиль/сложность) |
| 5 | Синтаксис python-скриптов | `python -m py_compile scripts/*.py` | **PASS** — все 5 скриптов компилируются |
| 6 | Живой smoke-тест протокола | `python scripts/smoke_test.py` | **PASS** — 15/15 проверок |
| 7 | Долговечность kill -9 | `python scripts/kill9_test.py` | **PASS** — 3 701 подтверждённая запись, 0 потеряно, 0 испорчено |
| 8 | Chaos / самовосстановление | `python scripts/chaos_test.py` | **PASS** — 3/3 сценария, 300 000 проверок ключей |
| 9 | Генератор HTML-отчёта | `python scripts/gen_report.py` | **PASS** — KPI читает 152/152 из реального лога |

**Итого: 152 Rust-теста (139 lib + 13 integration), все зелёные, 0 падений.**

---

## 1–3. Сборка и тесты Rust

```
$ cargo build --release
    Finished `release` profile [optimized] target(s) in 1m 14s

$ cargo test --release
running 139 tests
test result: ok. 139 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.63s

running 13 tests
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.06s
```

Полный вывод по каждому тесту: [cargo_test_output.txt](cargo_test_output.txt).

Что покрыто: recovery WAL и обрезка torn-write, dual-strand repair,
кодек Reed–Solomon по всем паттернам потерь, компакция, checkpoint,
парсер/планировщик/исполнитель SQL (агрегаты, GROUP BY/HAVING, ORDER BY,
IN-подзапросы, secondary-индексы и планы `IndexScan`, нарушения
UNIQUE-индексов), парсер и исполнитель DnaQL, транзакции (атомарный
коммит, rollback, read-your-writes), нечёткий поиск против эталонного DP
на 8 000 фаззинг-кейсах и HTTP API веб-панели (требуется авторизация).

## 4. Clippy

```
$ cargo clippy --release --all-targets
… 0 ошибок, 67 предупреждений
```

Предупреждения — только стиль/сложность; крупнейшие группы:
`type_complexity` (15), документационные списки (5), `manual_chunks_exact`
/ `if_same_then_else` / `needless_range_loop` (по 4), `manual_is_multiple_of` (3).

Одна **ошибка** была найдена и исправлена в этом прогоне:
`clippy::approx_constant` в тесте `rowcodec_roundtrip_all_types`
(`Value::Float(3.14159)` выглядел как приближённое π). Значение в тесте
заменено на `123.456`; тест по-прежнему проверяет round-trip всех пяти
типов значений.

## 6. Живой smoke-тест протокола

```
$ python scripts/smoke_test.py
  [PASS] auth_ok / auth_bad / preauth_guard / ping
  [PASS] put_get / get_miss / delete / big_value (1 MiB)
  [PASS] sql_create / sql_insert / sql_select
  [PASS] dnaql_analyze / checkpoint
  [PASS] bad_opcode / restart_recovery (жёсткий kill + WAL replay)

15 checks, 0 failed
```

Машиночитаемые результаты: [smoke_test_results.json](smoke_test_results.json).

## 7. Долговечность kill -9

```
$ python scripts/kill9_test.py
[TEST] Total acknowledged writes before crash: 3701
[RESULT] Verification complete:
  - Total checked: 3701
  - Missing: 0
  - Corrupt: 0
>>> SUCCESS: 100% DURABILITY VERIFIED UNDER SIGKILL (kill -9) <<<
```

## 8. Chaos-тест (порча секторов → самовосстановление)

```
$ PYTHONUTF8=1 python scripts/chaos_test.py
SCENARIO 1/3 offline: PASS  — 100000 ключей, 60 секторов, 0 missing, 0 corrupted
SCENARIO 2/3 live:    PASS  — 100000 ключей, lesions found=45, repaired=45
SCENARIO 3/3 overdmg: PASS  — 4 шарда в страйпе → честный UNREPAIRABLE, wrong=0
CHAOS PASS: 3 scenario(s), 300000 key-verifications, seed=20261021
```

Машиночитаемые результаты: [chaos_results.json](chaos_results.json),
полная стенограмма: [chaos_full_run.log](chaos_full_run.log),
нарративный отчёт: [CHAOS_REPORT.md](CHAOS_REPORT.md).

> **Замечание для Windows:** `chaos_test.py` печатает символ `×`; на
> консоли cp1251/cp437 запускайте скрипт с `PYTHONUTF8=1`, иначе Python
> выбросит `UnicodeEncodeError` до первого сценария.

## 9. HTML-отчёт

```
$ python scripts/gen_report.py --out DNA-DB_Report.html
```

Генератор теперь берёт счётчики тестов из
[cargo_test_output.txt](cargo_test_output.txt) (unit- и интеграционный
наборы парсятся из вывода `cargo test`), а не из захардкоженных чисел,
поэтому KPI-плитки всегда соответствуют реальному прогону:
**152 / 152 тестов PASS, 15/15 smoke-проверок, 3/3 chaos-сценариев**.

---

## Воспроизвести всё локально

```bash
cargo build --release
cargo test --release                                  # 152 теста
cargo clippy --release --all-targets                  # 0 ошибок
python3 -m py_compile scripts/*.py
python3 scripts/smoke_test.py                         # 15/15
python3 scripts/kill9_test.py                         # 3701 записей, 0 потеряно
PYTHONUTF8=1 python3 scripts/chaos_test.py            # 3/3 сценария (Windows: PYTHONUTF8=1)
python3 scripts/gen_report.py                         # HTML-отчёт
```
