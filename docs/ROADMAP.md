# DNA-DB Core — ROADMAP

> **Russian:** [ROADMAP_RU.md](ROADMAP_RU.md)

## Delivered in v3.1 (panel + full SQL)

Date: 2026-10-25

| Former limitation | Status in v3.1 | Where to look |
|---|---|---|
| No secondary/range indexes — WHERE on a non-PK column = full scan | **Closed**: `CREATE/DROP INDEX`, stored in the schema, honoured by DML, `IndexScan` plans for equality/`IN`/range | `sql_engine.rs` (index_*), index unit tests, [SQL_MANUAL.md §3.4](SQL_MANUAL.md) |
| No aggregates/ORDER BY/GROUP BY | **Closed**: COUNT/SUM/AVG/MIN/MAX, GROUP BY + HAVING, ORDER BY ASC/DESC, LIMIT/OFFSET | aggregate unit tests |
| No subqueries | **Partially closed**: `WHERE c IN (SELECT …)` is materialized (in SELECT/UPDATE/DELETE); no correlated subqueries | IN-subquery unit tests |
| No web admin panel | **Closed**: built-in `serve --admin-port` panel (overview/health, table browser, indexes, SQL/DnaQL consoles, HTTP API) | `admin.rs`, `admin.html`, the `panel_requires_auth_and_serves_api` test |
| No replication/cluster (mirror = two local disks) | Remains P2 (see "Replication" below) | — |
| Protocol without TLS | Remains P2 — recommendation: SSH/stunnel/VPN tunnel | README, "Limitations" |
| JOIN plans hash-only on FK | Remains P2 (index-based join plans) | — |

v3.1 tests: `cargo test --release` → **152 passed, 0 failed, 0 warnings**.

---

# DNA-DB Core — ROADMAP for the v2.1.0 release

Date: 2026-10-19
Baseline version: v2.0.0 ("DnaQL production" — an audit showed that a large part of the functionality was stubs)

---

## Part I. Delivered in v2.1.0 (demo code replaced with working code)

### Critical fixes (P0)

| # | Problem in v2.0 | Solution in v2.1 | Test |
|---|---|---|---|
| 1 | `WalReader::recover` existed but was never called — data was not recovered | Recovery wired into `Engine::open`: WAL replay over segments | `crash_recovery_preserves_all_data`, kill9_test |
| 2 | `SegmentWriter` was never called — the flush path was missing, the "LSM" was memtable-only | Full path: rotation → SSTable → reads from segments | `flush_creates_readable_segments` |
| 3 | `SegmentReader` did not exist | Implemented: bloom pre-pass + sparse index binary search + CRC32 | 4 unit tests |
| 4 | Bitap/Myers skipped valid matches (>50% of cases under fuzzing), `mismatches` always = max_distance | Correct initialization of `R[d]=(1<<d)-1`, true distance, recovery of the shortest alignment window | 8000-case fuzz vs a reference DP |
| 5 | The SQL engine ignored the query text and returned hardcoded rows | Tokenizer + parser + executor: SELECT/JOIN/WHERE/LIMIT/INSERT/UPDATE/DELETE/CREATE/DROP | 9 unit tests |
| 6 | The DnaQL parser returned fake ASTs, timings were falsified (`.max(18)`) | A real parser for an EBNF subset, real time, real data | 8 unit tests + smoke |
| 7 | "2PC Polymerase": prepare() only changed status, commit was not atomic | commit = one batched WAL write = one fsync = one atomic interval; honest naming (single-node atomic commit) | `write_batch_is_atomic_on_recovery` |
| 8 | The password was stored in plaintext (the field was called `hash`) | salted SHA-256 + constant-time comparison (subtle) | `lockout_after_max_attempts` |
| 9 | No server at all (only SecurityManager), nothing to attach `kill9_test.py` to | TCP server: thread-per-connection, frames ≤16MB, compatible with the kill9_test wire format | 3 unit tests + live smoke |
| 10 | Memtable rotation was skipped when the immutable was busy → unbounded growth (anti-OOM claim violated) | `ensure_capacity`: synchronous flush under flush_lock with double-check | `flush_creates_readable_segments` |

### New modules and features

| Module | What it does |
|---|---|
| `bloom.rs` | Deterministic serializable bloom filter (replaces fastbloom, auditable, k=4, FNV1a+double hashing) |
| `complement_index.rs` | A real O(1) FK index (Base-Pairing Complementarity): maintained on INSERT/DELETE, hits/misses statistics |
| `bio_plugins.rs` | Real Smith-Waterman (DP + traceback), CRISPR PAM scanner (NGG/NRG), ORF-finder, sliding-window GC |
| `stats.rs` | Real counters: ops, bytes, average put/get latencies, flush/compaction, uptime |
| `expr.rs` | Shared expression AST (AND/OR/NOT, comparisons, IS NULL) for WHERE in DnaQL and SQL |
| `RowCodec` (relational.rs) | Binary typed row codec (tag-length-value): Null/Int/Float/Text/Dna |
| `checkpoint()` | Flush everything + truncate WAL (durability point) |
| `compact()` | Full segment compaction with absorbed tombstones removed |
| `scan_prefix()` | Sorted merged scan by prefix across all layers |
| `write_batch()` | Atomic multi-write (one WAL frame) |
| CLI `serve/bench/shell` | Server, honest benchmarks, interactive REPL client |
| Drop for Engine | Flushes the WAL buffer on clean shutdown (Batched/Async) |

### Removed stubs

| v2.0 module | Reason for removal | Replacement |
|---|---|---|
| `iouring_wal.rs` | `append_zero_copy` only incremented a counter, there was no I/O | Honest SyncMode modes; group commit is on the roadmap |
| `wasm_runtime.rs` | Returned the string "executed, 0 memory leaks" without executing anything | `bio_plugins.rs` (real functions) |
| `helix_ql.rs` (v1) | Dead predecessor of DnaQL | — |

---

## Part II. Roadmap ahead (v2.2 → v3.0)

### P1 — next releases

1. **MVCC / snapshot isolation** — TxIsolationLevel::SnapshotIsolation/Serializable were declarative (effectively Read Committed). Plan: timestamp-ordered keys + version GC.
2. **Range queries and iterators** — `Engine::range(start..end)` with a merged sorted iteration across layers (today: scan_prefix).
3. **Group commit** — with more than one parallel client, combine fsync of several acknowledgements into one interval (effect: xN throughput on real NVMe).
4. **Block-organized SSTables** — block compression (zstd/lz4) + page-level CRC; currently records are written sequentially without blocks.
5. **Long Bitap patterns (>64 bp)** — chunked/seed-and-extend search; today an honest failure when m>64.
6. **WAL segmentation** — rotate active.wal by size instead of truncating only on checkpoint.
7. **Schema DDL in DnaQL** — `CREATE CHROMOSOME ... (locus INT, sequence DNA)` for symmetry with SQL.
8. **Token authentication** — token-based sessions over user:pass (the hook already exists in the wire protocol).

### P2 — infrastructure

9. **Backups and snapshots** — a consistent snapshot (checkpoint + segment copy) + restore from an archive.
10. **Replication** — today DualStrandMirror is a mirrored WAL of a single node; next step is streaming replication to a second node with replay.
11. **Distributed 2PC** — a real two-phase commit for multi-node transactions (coordinator + participants, prepared log).
12. **io_uring WAL** — a real implementation (SQPOLL, registered buffers) for Linux ≥5.10 behind a feature flag.
13. **Client SDKs** — Python client (the wire client already exists in the smoke tests), Rust client crate.
14. **WASM plugins** — a wasmtime sandbox for user bio functions (the BioPluginFn trait already exists).

### P3 — performance

15. **Bitap vectorization** — AVX2/NEON to scan several patterns in parallel; auto-detection via `is_x86_feature_detected!`.
16. **Tiered storage automation** — background thread: demote cold segments by age/size (the manual API is ready).
17. **Benchmarks on real hardware** — current numbers come from an overlayfs sandbox (fsync ≈ free); needs a run on NVMe with dataset.tar and publication in BENCHMARKS.md.
18. **Fuzzing infrastructure** — cargo-fuzz targets for WAL recovery and the SQL/DnaQL parsers.

### Technical debt (accepted compromises of v2.1)

- Table schemas are stored as serde_json (`__schema:*`) — replace with a binary format if desired.
- Text PKs that coincide with numbers (`"14"` vs `14`) produce different keys — collisions are excluded by the Value::Display format, but the semantics should be documented.
- `hash_join_left` recomputes the matched set — micro-optimization on P2.
- INSERT does not check column types against the schema (Value is self-contained) — add strict typing.
