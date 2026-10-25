# DNA-DB Core — Test Results

> **Русская версия:** [TEST_RESULTS_RU.md](TEST_RESULTS_RU.md)
> This file lists the real, reproducible results of the full verification run.
> Three-way benchmark vs MySQL/PostgreSQL: [DB_COMPARISON.md](DB_COMPARISON.md).

* **Date of run:** 2026-10-25 (21:27–21:56 +03:00)
* **Commit under test:** working tree of `main` (release profile)
* **Toolchain:** rustc 1.98.1, cargo 1.98.1, Python 3.12.9
* **Platform:** Windows 10/11 x86_64 (MINGW64), `panic = "abort"`, LTO thin
* **Raw log of the Rust run:** [cargo_test_output.txt](cargo_test_output.txt)
* **CI:** [.github/workflows/ci.yml](../.github/workflows/ci.yml) re-runs the
  unit/integration tests and clippy on every push to `main`

---

## Summary

| # | Check | Command | Result |
|---|---|---|---|
| 1 | Release build | `cargo build --release` | **PASS** — finished in 1m 14s, 0 warnings |
| 2 | Unit tests (lib) | `cargo test --release` | **PASS** — 139 passed, 0 failed, 0 ignored (0.63s) |
| 3 | Integration tests | `cargo test --release` | **PASS** — 13 passed, 0 failed (12.06s) |
| 4 | Lint | `cargo clippy --release --all-targets` | **PASS** — 0 errors, 67 warnings (style/complexity only, see below) |
| 5 | Python scripts syntax | `python -m py_compile scripts/*.py` | **PASS** — all 5 scripts compile |
| 6 | Live protocol smoke | `python scripts/smoke_test.py` | **PASS** — 15/15 checks |
| 7 | kill -9 durability | `python scripts/kill9_test.py` | **PASS** — 3 701 acknowledged writes, 0 missing, 0 corrupt |
| 8 | Chaos / self-healing | `python scripts/chaos_test.py` | **PASS** — 3/3 scenarios, 300 000 key verifications |
| 9 | HTML report generator | `python scripts/gen_report.py` | **PASS** — KPI reads 152/152 tests from the real log |

**Total: 152 Rust tests (139 lib + 13 integration), all green, 0 failures.**

---

## 1–3. Rust build and tests

```
$ cargo build --release
   Compiling dna-db-core v3.0.0
    Finished `release` profile [optimized] target(s) in 1m 14s

$ cargo test --release
running 139 tests
test result: ok. 139 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.63s

running 13 tests
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.06s
```

Full per-test output: [cargo_test_output.txt](cargo_test_output.txt).

Notable groups covered by the suite: WAL recovery and torn-write
truncation, dual-strand repair, Reed–Solomon codec against every loss
pattern, compaction, checkpointing, SQL parser/planner/executor
(aggregates, GROUP BY/HAVING, ORDER BY, IN-subqueries, secondary
indexes and `IndexScan` plans, UNIQUE index violations), DnaQL parser
and executor, transactions (atomic commit, rollback, read-your-writes),
fuzzy search vs a reference DP over 8 000 fuzz cases, and the HTTP admin
panel API (auth required).

## 4. Clippy

```
$ cargo clippy --release --all-targets
… 0 errors, 67 warnings
```

Warnings are style/complexity only — the largest groups:

| Count | Lint |
|---|---|
| 15 | `type_complexity` — suggest factoring types into `type` aliases |
| 5 | `doc_lazy_continuation` — doc list item without indentation |
| 4 | `manual_chunks_exact` / `if_same_then_else` / `needless_range_loop` |
| 3 | `manual_is_multiple_of` |
| rest | single-occurrence style lints |

One **error** was found and fixed during this run: `clippy::approx_constant`
in the `rowcodec_roundtrip_all_types` test (`Value::Float(3.14159)` looked
like an approximate π). The test value was changed to `123.456`; the test
still round-trips all five value types.

## 6. Live protocol smoke test

```
$ python scripts/smoke_test.py
  [PASS] auth_ok
  [PASS] auth_bad — wrong password -> b'NO'
  [PASS] preauth_guard — b'ERR: not authenticated'
  [PASS] ping — status=ok
  [PASS] put_get — ack=b'OK' len=140
  [PASS] get_miss — empty frame on miss
  [PASS] delete
  [PASS] big_value — 1 MiB roundtrip, 1048576 bytes
  [PASS] sql_create
  [PASS] sql_insert
  [PASS] sql_select — 1 row(s)
  [PASS] dnaql_analyze
  [PASS] checkpoint
  [PASS] bad_opcode — b'ERR: unknown opcode 0x7F'
  [PASS] restart_recovery — after hard kill + WAL replay: b'DO-NOT-LOSE'

15 checks, 0 failed
```

Machine-readable results: [smoke_test_results.json](smoke_test_results.json).

## 7. kill -9 durability

```
$ python scripts/kill9_test.py
[TEST] Starting DNA-DB kill -9 crash durability test...
[TEST] Sending SIGKILL (kill -9) to PID 19076...
[TEST] Total acknowledged writes before crash: 3701
[TEST] Restarting DNA-DB server to trigger WAL replay and torn-write auto-recovery...
[RESULT] Verification complete:
  - Total checked: 3701
  - Missing: 0
  - Corrupt: 0
>>> SUCCESS: 100% DURABILITY VERIFIED UNDER SIGKILL (kill -9) <<<
```

## 8. Chaos test (sector corruption → self-healing)

```
$ PYTHONUTF8=1 python scripts/chaos_test.py
=== SCENARIO 1/3: offline (restart + doctor --fix) ===
[1.1] loaded 100000 keys in 94.2s, checkpointed, SIGKILLed
[1.4] OFFLINE SCENARIO: PASS — 0 missing, 0 corrupted
=== SCENARIO 2/3: live (damage while serving, on-read repair) ===
[2.2] read all 100000 keys in 21.8s (on-read repair): missing=0, corrupted=0
[2.3] lesions: found=45, repaired=45, bytes_repaired=184320
[2.4] LIVE SCENARIO: PASS
=== SCENARIO 3/3: over-damage (> m shards in one stripe) ===
[3.2] doctor verdict: UNREPAIRABLE (unrecoverable stripes: 1, damaged blocks: 4)
[3.4] reads: wrong=0, unreadable=483
[3.5] OVER-DAMAGE SCENARIO: PASS — honest failure reporting

{"scenario": "offline",  "keys": 100000, "sectors_damaged": 60, "missing": 0, "corrupted": 0, "verdict": "PASS"}
{"scenario": "live",     "keys": 100000, "sectors_damaged": 60, "missing": 0, "corrupted": 0, "verdict": "PASS", "lesions_repaired": 45}
{"scenario": "overdmg",  "keys": 100000, "damaged_shards_in_one_stripe": 4, "doctor_verdict": "UNREPAIRABLE", "wrong_values": 0, "verdict": "PASS"}

CHAOS PASS: 3 scenario(s), 300000 key-verifications, seed=20261021
```

Machine-readable results: [chaos_results.json](chaos_results.json),
full transcript: [chaos_full_run.log](chaos_full_run.log),
narrative report: [CHAOS_REPORT.md](CHAOS_REPORT.md).

> **Windows note:** `chaos_test.py` prints `×`; run it with
> `PYTHONUTF8=1` on a cp1251/cp437 console, otherwise Python raises
> `UnicodeEncodeError` before the first scenario.

## 9. HTML report

```
$ python scripts/gen_report.py --out DNA-DB_Report.html
report: …/DNA-DB_Report.html
```

The generator now derives the test counters from
[cargo_test_output.txt](cargo_test_output.txt) (unit and integration
suites parsed from the `cargo test` output) instead of hardcoded
numbers, so the KPI tiles always match the real run: **152 / 152 tests
PASS, 15/15 smoke checks, 3/3 chaos scenarios**.

---

## Reproduce everything locally

```bash
cargo build --release
cargo test --release                                  # 152 tests
cargo clippy --release --all-targets                  # 0 errors
python3 -m py_compile scripts/*.py
python3 scripts/smoke_test.py                         # 15/15
python3 scripts/kill9_test.py                         # 3701 writes, 0 lost
PYTHONUTF8=1 python3 scripts/chaos_test.py            # 3/3 scenarios (Windows: PYTHONUTF8=1)
python3 scripts/gen_report.py                         # HTML report
```
