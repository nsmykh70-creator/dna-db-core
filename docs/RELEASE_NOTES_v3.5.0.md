# DNA-DB Core v3.5.0 — Release Notes (audit remediation)

Date: 2026-10-31
Base: v3.4.0 · Scope: security & correctness remediation of the full v3.4.0 audit

## What changed in one paragraph

v3.5.0 closes every critical and high finding of the v3.4.0 audit: the
parser-borne stack-overflow kill-switch, two ACL bypasses and the
`admin/password` bootstrap default (P0) are gone; mirror-failure WAL
rollback, incremental replay flushes, real Serializable isolation,
fsync-free reads, a slowloris deadline, doctor-vs-server locking, subquery
rendering, hash-join semantics and integer-overflow handling (P1) are fixed;
the CRC documentation (P2) now matches the on-disk format. No on-disk format
changed — v3.4.0 data directories open as-is.

## Highlights

| Area | Before | After |
| --- | --- | --- |
| Deep query | 100k nested parens → stack overflow → whole server died (`panic=abort`) | parse error `nested too deeply (limit 128)` + `catch_unwind` per request |
| ACL | `ANALYZE LOCUS` + bare `DROP INDEX` bypassed table restrictions | both permission-checked |
| Bootstrap | `admin/password`, `must_change: false` | random 24-symbol password printed once, `must_change: true` |
| Mirror failure | WAL frame survived an unacknowledged write | WAL rolled back (`rollback_to`) |
| WAL replay | whole log buffered in RAM before a single capacity check | incremental flush when memtable fills |
| Serializable | degraded to last-writer-wins vs ReadCommitted rivals | commit lock covers every commit |
| Reads vs fsync | all reads blocked during strict WAL fsync | readers snapshot layers, never wait on fsync |
| Slowloris | per-read timeout only | absolute 30 s pre-auth deadline |
| `doctor` | could rewrite WAL/segments under a live server | refuses while the server holds `.dna-db.lock` |
| CI | both workflows triggered on `branches: ain]` (never ran) | `branches: [main]` |
| Subqueries | `expr_to_sql` dropped parens → wrong results | precedence-aware rendering |
| Hash join | `NULL = NULL` matched; `Int ≠ Float` | SQL three-valued logic; numeric Int/Float matching |
| Arithmetic | silent i64 wrap in TX `+=` and WHERE | errors / NULL (checked ops) |
| Docs | PROTOCOL.md claimed CRC32C (SSE4.2) | corrected to CRC-32 (IEEE, `crc32fast`) |

## Verification

- `cargo test --release`: **197 passed, 0 failed** (182 unit + 2 encryption
  + 13 recovery, incl. 11 new regression tests for the audit fixes).
- `scripts/smoke_test.py` against the release binary: **15/15** (auth,
  pre-auth guard, ping, CRUD, 1 MiB frames, SQL, DnaQL, checkpoint, bad
  opcode, hard-kill recovery).
- New security smoke (release binary): **12/12** — random bootstrap password
  printed once and accepted, static default gone, deep query rejected with
  the server alive afterwards, slowloris dropped, second server instance
  refused, `doctor` refuses a live DB and runs after stop, generated
  password survives restart.
- Durability: `scripts/kill9_test.py` (strict fsync, 8 writer threads) —
  **123 408 acknowledged writes, 0 missing, 0 corrupt** after SIGKILL + WAL
  replay; `scripts/chaos_test.py` — **3/3 scenarios, 300 000 key
  verifications** (RS(4+2) offline repair, live on-read repair, honest
  UNREPAIRABLE verdict).
- Three-way comparison (Debian 13, 2 vCPU, 2026-10-31): DNA-DB vs
  MySQL 8.4.6 vs PostgreSQL 17.11 at default durability — DNA-DB is
  **8.3× / 4.0× / 2.4× faster than MySQL** (single-thread load, point
  reads, mixed) and **1.9× / 2.5× / 1.1× faster than PostgreSQL**, up to
  **298 936 batch rows/s**; SQL conformance 25/32 supported with **0 wrong
  results** (MySQL 31/32, PostgreSQL 32/32). Details:
  [TEST_RESULTS.md](TEST_RESULTS.md), [DB_COMPARISON.md](DB_COMPARISON.md).

## Breaking / behavioural changes

1. **Bootstrap credentials** — no default password. Read the generated one
   from stderr on first start or pass `--admin-pass` / `DNA_DB_ADMIN_PASS`.
2. **`panic = "unwind"`** — an engine panic no longer aborts the process.
3. **Bare `DROP INDEX name`** is rejected for table-restricted users (use
   `DROP INDEX name ON table`); administrators are unaffected.
4. **`doctor`** refuses to run against a locked (live) data directory.
5. The repository no longer ships a prebuilt binary; build with
   `cargo build --release`.

## Quick start

```bash
cargo build --release
./target/release/dna-db-core serve --data-dir ./dna_data --port 9999 --admin-port 8080
# → "[SECURITY] bootstrap admin (shown ONCE ...): admin / <random>"
# → log in at https://127.0.0.1:8080/ and set your own password
```
