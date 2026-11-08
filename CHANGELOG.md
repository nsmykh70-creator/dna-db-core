# Changelog

## v4.2.0 (2026-11) — ограничения сняты: полный ALTER TABLE, prepared statements, окна с рамками, ROLLUP/CUBE, WITH RECURSIVE, IndexJoin

Цель релиза — устранение документированных «честных ограничений»
(RELEASE_NOTES_v4.0.0 «Честные ограничения» и SQL_MANUAL §3.5).

- **Полный `ALTER TABLE`** (раньше — только `ADD COLUMN`):
  - `DROP COLUMN` — один атомарный WAL-батч переписывает схему, все строки
    (без удалённого поля — колонка не может «воскреснуть») и сметает
    индексы по колонке; PK-колонка и FK-колонки защищены честными ошибками;
  - `RENAME COLUMN a TO b` — атомарная перезапись схемы, строк и индексов;
    PK-переименование поддержано (ключи строк выводятся из значений);
    FK в других таблицах, ссылающиеся на колонку, обновляются;
  - `MODIFY COLUMN c T` / `CHANGE COLUMN a b T` — смена типа с конверсией
    значений: NULL → NULL; →INT: FLOAT округляется (MySQL-семантика,
    половина от нуля), TEXT парсится; →FLOAT: INT расширяется, TEXT
    парсится; →TEXT: любой Display-рендер; →DNA: TEXT обязан быть чистой
    ACGT (то же правило, что у INSERT-литералов); →TIMESTAMP: без изменений.
    Всё готовится в памяти: один неконвертируемый символ отменяет весь
    стейтмент до записи (никаких частичных конверсий). Смена типа
    PK-колонки не поддерживается (ключи строк выводятся из значения PK).
- **Prepared statements в pgwire** (PostgreSQL wire v3): расширенный
  протокол `Parse/Bind/Describe/Execute/Close/Flush/Sync` вместо ошибки
  0A000. Поддержаны именованные и безымянные стейтменты и порталы,
  плейсхолдеры `$n` и `?` (смешивать нельзя), текстовый формат параметров,
  NULL через -1, двоичный формат — явная понятная ошибка; после
  ErrorResponse остальные сообщения батча пропускаются до Sync (спека).
  Проверено живым клиентом psycopg2 (libpq): параметризованные SELECT и
  INSERT, новый E2E `scripts/pgwire_extended_e2e.py`. Попутный фикс
  протокола: CommandComplete теперь NUL-терминированный (требование libpq),
  INSERT-теги считают строки по `inserted_key`.
- **Оконные функции v4.2**: агрегаты как оконные функции
  (`SUM/COUNT/AVG/MIN/MAX OVER (...)`), рамки `ROWS BETWEEN ... AND ...`
  (полная грамматика: UNBOUNDED/n PRECEDING/FOLLOWING, CURRENT ROW) и
  `RANGE` для UNBOUNDED/CURRENT ROW границ; стандартные дефолты (вся
  партиция без ORDER BY; RANGE UNBOUNDED PRECEDING..CURRENT ROW с пирами).
  Окна вычисляются **после** агрегации (стандартный порядок): работают
  поверх GROUP BY, ссылки в PARTITION BY/ORDER BY могут быть агрегатами
  (`RANK() OVER (ORDER BY SUM(total_usd) DESC)`), вложенные агрегаты
  `SUM(SUM(x)) OVER ()` вычисляются как групповой агрегат (без GROUP BY —
  честная ошибка, как в PostgreSQL).
- **Группировка по наборам**: `GROUP BY a, b WITH ROLLUP` (MySQL),
  `GROUP BY ROLLUP (a, b)` и `GROUP BY CUBE (a, b)` (стандарт) —
  супер-агрегатные строки с NULL-маркерами, гранд-итог; CUBE ограничен
  12 колонками (4096 наборов).
- **`WITH RECURSIVE`**: CTE вида `base UNION [ALL] recursive` вычисляется
  итеративно (рекурсивная сторона видит только дельту прошлого раунда);
  `UNION` дедуплицирует строки (циклы в данных завершаются сами),
  `UNION ALL` — нет; жёсткие лимиты: 10 000 раундов / 1 000 000 строк.
- **Индексные JOIN-планы**: INNER/LEFT соединения по равенству переключаются
  на `IndexJoin` (проба индекса или PK на каждую строку внешней стороны),
  когда правая сторона индексирована и заметно больше левой (эвристика
  стоимости); RIGHT/FULL остаются на симметричных hash-планах. План всегда
  виден в `execution_plan` (`IndexJoin(...)` / `HashJoin(...)`).
- **Побочные улучшения чтения**: `SELECT *` проецирует ровно колонки схемы
  (устаревшие поля старых payload'ов невидимы), чтение строк фильтруется по
  живой схеме (read_table_rows / read_row_point), UPDATE физически чистит
  устаревшие поля при перезаписи строки.

Осталось честно (вне рамок релиза): Raft — одна группа без снапшотов
(чтения на лидере), S3-клиент без multipart (сегменты ≤ 5 ГБ), `LATERAL`
и value-offset `RANGE`-рамки, смена типа PK-колонки. См.
[RELEASE_NOTES_v4.2.0.md](docs/RELEASE_NOTES_v4.2.0.md).

## v4.1.0 (2026-11) — transparent storage & panel ops

- **Transparent storage for SQL operators** — the operator writes plain SQL
  and never has to read A/C/G/T chains:
  - `SHOW STORAGE FOR <table> [WHERE <pk> = <literal>]` (SQL): the
    SQL→DnaQL→DNA storage map. Without a predicate — one summary row per
    stored locus (chromosome, locus, storage key, payload size, DNA columns);
    with a PK filter — per-column detail with every value decoded to the
    human-readable form next to its physical RowCodec representation
    (nucleotide chains included). PK-only filter, read-only.
  - `ANALYZE LOCUS <table>:<pk> |> STORAGE_MAP();` (DnaQL): the same map,
    works on any row (no sequence field required).
  - `dna-db-core sql2dnaql --sql "INSERT ..."` now also prints a
    "storage map" section: which chromosome/locus/key the row will land at
    and how each value is physically stored — before anything is written.
- **Offline compaction CLI:** `dna-db-core compact --data-dir DIR [--json]` —
  checkpoint + full segment merge (drops shadowed versions and consumed
  tombstones); refuses a live data dir via the advisory lock (same as
  doctor). The resource benchmark now reports `disk_after_compact_mib`
  for DNA-DB alongside the raw footprint.
- **Admin panel catches up with v4.1** (embedded SPA, `serve --admin-port`):
  - `POST /api/explain` — SQL → DnaQL translation plus the physical storage
    map for a statement **without executing it**. The SQL console shows the
    block automatically after any mutating statement and on demand via the
    new "SQL → DnaQL" button — the operator sees the exact chromosome,
    locus, storage key and RowCodec representation before writing.
  - "Физическое хранение" (🔬) button on every row of the table browser —
    runs `SHOW STORAGE FOR <table> WHERE <pk> = …` and renders the
    per-column map in a modal.
  - `POST /api/compact` (admin only) — online full compaction safe under
    concurrent readers; the new "Обслуживание" dashboard panel reports the
    data-dir size before/after, freed bytes and removed-file count.
  - **Compaction history** — every completed run (panel `panel:<user>`, CLI,
    manual, auto) is appended to `<data_dir>/compaction_history.jsonl`
    (capped at 500 records) and served to the panel via the new
    `GET /api/compactions` (admin only): timestamp, trigger, segment count,
    bytes before → after, saved bytes, duration. The "Обслуживание" panel
    renders the journal as a table, newest first.
  - **Prometheus export of compaction results** — `/metrics` now carries
    `dnadb_compactions_total`, `dnadb_compaction_bytes_before` /
    `dnadb_compaction_bytes_after` / `dnadb_compaction_bytes_saved`,
    `dnadb_compaction_last_duration_seconds`,
    `dnadb_compaction_last_success_timestamp_seconds` and
    `dnadb_data_dir_bytes` (whole-directory footprint).
  - **STORAGE_MAP hand-off** — the 🔬 storage-map modal of the table browser
    gained a "STORAGE_MAP в DnaQL-консоли" button: it prefills
    `ANALYZE LOCUS <table>:<pk> |> STORAGE_MAP();` in the DnaQL console and
    runs it — the same physical map in DnaQL terms (STORAGE_MAP mode of the
    console). STORAGE_MAP examples in the DnaQL cheatsheet.
  - `GET /api/health` now reports `data_dir_bytes`; the dashboard shows a
    "Данные на диске" card (GiB-aware formatter, path in the tooltip) and a
    compaction counter card.
  - Tests: `admin::tests::panel_explain_compact_and_disk_size` (real
    HTTP round-trips: ACL 403s for viewers, JOIN rejection, TX transpile,
    `/api/compactions`, `/metrics` compaction gauges).

## Unreleased — DnaQL v2.2 «SQL parity» + SQL engine fixes

**DnaQL v2.2** (`src/dna_ql.rs`, `src/sql_to_dnaql.rs`) closes the last
dialect gaps: every SQL command now has a native DnaQL spelling and the
`sql_to_dnaql` transpiler converts SQL → DnaQL without losing semantics.

- New pipeline stages mirroring the SQL engine: `JOIN` / `LEFT` / `RIGHT` /
  `FULL [OUTER] JOIN` (hash join, `joined_` conflict naming, NULL-extension),
  `GROUP [BY …] AGG name = COUNT/SUM/AVG/MIN/MAX(col)` (NULL-skipping, Int-sum
  preservation, HAVING = `WHERE` after `GROUP`), `OFFSET`; computed
  `EMIT expr AS alias`; `NOT` in `WHERE` expressions (the manual promised it,
  the parser now delivers).
- `UNION [ALL]` compound pipelines with a SORT/LIMIT/OFFSET tail applied to
  the compound result (SQL semantics; column names from the left side,
  DISTINCT dedup).
- DDL & introspection: `SHOW CHROMOSOMES` (alias `SHOW TABLES`),
  `ALTER CHROMOSOME … ADD COLUMN`, `CREATE [UNIQUE] INDEX` / `DROP INDEX`,
  `EXPLAIN FROM … | FIND …` (plan instead of rows; writes rejected).
- Row-set statements: top-level / in-TX `UPDATE chr SET … [WHERE …]` and
  `DELETE FROM chr [WHERE …]` (atomic one-WAL-batch TX, FK CASCADE /
  honest RESTRICT), and the `?=` insert-if-absent operator — the DnaQL
  spelling of `INSERT … ON CONFLICT DO NOTHING`.
- Transpiler now translates the full SQL surface (previously JOIN, GROUP BY,
  OFFSET, computed projections, UNION, SHOW/ALTER/INDEX/EXPLAIN were
  rejected); fixed the UPSERT translation bug where the `ON CONFLICT` tail
  was silently ignored — `DO NOTHING` → `?=`, full non-PK `DO UPDATE SET` →
  exact `+=` upsert, partial `DO UPDATE SET` → explicit error.

**SQL engine fixes** (`src/sql_engine.rs`):
- `UNION` renamed right-hand rows positionally over BTreeMap iteration
  order, drifting values between slots whenever a right row carried a
  different key set than the first row (e.g. after a partial INSERT);
  the rename now goes by field name (regression test:
  `union_rename_is_by_field_name_not_row_order`).
- `FULL OUTER JOIN` silently overwrote conflicting columns instead of
  using the `joined_` prefix like INNER/LEFT JOIN, and its unmatched-right
  branch NULL-padding was dead code — both fixed
  (`full_join_conflict_columns_use_joined_prefix`).
- Conformance matrix in `scripts/sql_feature_test.py` updated: DNA-DB is
  32/32 (the v3.0 gaps are closed); only MySQL's FULL OUTER JOIN remains
  in `GAPS`.

**Test robustness:** the Raft failover test's convergence window widened
from 4 s to 10 s (ticker threads can be starved on loaded 2-vCPU runners;
correctness assertions unchanged).

Verification: `cargo test --release` **230 PASS** (199 lib + 31
integration, incl. 8 new DnaQL v2.2 feature tests and the SQL↔DnaQL
parity test); canonical SQL conformance run: **32/32 ok, 0 mismatch,
0 error**; protocol smoke suite 15/15.

## v4.0.0-GA (2026-10-31) — Enterprise production release

Full implementation of the roadmap on top of the audited v3.5.0 baseline
(details in [docs/RELEASE_NOTES_v4.0.0.md](docs/RELEASE_NOTES_v4.0.0.md)):

- **Unified Query Engine:** `RIGHT`/`FULL [OUTER] JOIN`, `UNION [ALL]`,
  scalar & correlated subqueries, `EXISTS`, CTE (`WITH`), UPSERT
  (`INSERT ... ON CONFLICT`), window functions (`ROW_NUMBER`/`RANK`/
  `DENSE_RANK`), sessions (`BEGIN`/`COMMIT`/`ROLLBACK`), `EXPLAIN`,
  `SHOW TABLES`, `SELECT VERSION()` — SQL conformance **32/32 ok,
  0 unsupported, 0 mismatch**.
- **pgwire bridge** (`src/pgwire.rs`): PostgreSQL Wire Protocol v3 on port
  5432 — the same SqlEngine and ACL served over both protocols; E2E client
  test passes 8/8 steps.
- **Raft consensus** (`src/raft.rs` + `cluster` subcommand): quorum
  elections, persistent term/log, log replication with conflict rollback,
  failover < 2 s SLA, zero loss of committed writes.
- **Cloud tiering & online DDL** (`src/tiered_storage.rs`): S3-compatible
  client (AWS SigV4, rustls), segment demotion/promotion, metadata-only
  `ALTER TABLE ... ADD COLUMN`, PITR snapshots (`pitr --target/--restore`).
- **Enterprise GA:** Prometheus `/metrics` endpoint (0.0.4 text format),
  updated Kubernetes manifests (`DnaDbCluster` CRD, StatefulSet,
  ServiceMonitor).
- Verification: `cargo test --release` **215 PASS**; chaos suite 3/3
  (300 000 key verifications, 0 losses); benchmark v4.0.0 — batch-load
  308 423 rows/s, SQL SELECT pk 27 074 ops/s
  ([docs/bench_compare_results_v400_linux.json](docs/bench_compare_results_v400_linux.json)).

## v3.5.0 (2026-10-31) — Audit remediation release

All findings from the v3.4.0 security & quality audit (3 × P0, 12 × P1, plus
the actionable P2 items) are fixed in this release. 197 automated tests pass
(182 unit + 2 encryption + 13 recovery, incl. 11 new regression tests), the
production smoke suite (15 checks) and the new security smoke suite (12
checks) pass against the release binary. Durability was re-proven under
SIGKILL (123 408 acknowledged writes, 0 lost, 0 corrupt) and chaos (3/3
scenarios, 300 000 key verifications); the three-way benchmark against
MySQL 8.4.6 and PostgreSQL 17.11 was re-run on Linux (see
docs/TEST_RESULTS.md and docs/DB_COMPARISON.md).

### P0 — critical security

- **Stack overflow in the recursive-descent parsers killed the whole server.**
  Both the SQL and DnaQL parsers now bound expression/subquery nesting
  (limit 128) and return a plain parse error
  (`SQL: expression nested too deeply (limit 128)` /
  `DnaQL: ...`) instead of recursing once per `(`.  On top of that the release
  profile switched from `panic = "abort"` to `panic = "unwind"`, and every
  query handler (TCP protocol + admin panel console) runs inside a
  `catch_unwind` fault guard: a panic now costs one request
  (`ERR: internal error: request aborted by the fault guard`), not the
  process and all of its connections.
  (sql_engine.rs, dna_ql.rs, server.rs, admin.rs, Cargo.toml)
- **ACL bypass: `ANALYZE LOCUS` read rows of any table.**
  `DnaQlAst::BioAnalysis` returned an empty table list from the ACL
  resolver, so a table-restricted user could read any locus of any table.
  The locus table is now part of the permission check.
  (dna_ql.rs `collect_ast_tables`)
- **ACL bypass: bare `DROP INDEX name` skipped the permission check.**
  The bare form does not name its table and the ACL resolver cannot touch
  storage to resolve it, so it silently authorised an index drop on ANY
  table. It is now rejected for table-restricted users with a hint to use
  the documented `DROP INDEX <name> ON <table>` form; administrators are
  unaffected.  (sql_engine.rs `collect_stmt_tables`)
- **Bootstrap credential `admin/password` with `must_change: false`.**
  A fresh data directory now generates a cryptographically random 24-symbol
  bootstrap password (≈140 bits, unambiguous alphabet), prints it exactly
  once to stderr and seeds the account with `must_change = true`. The static
  default is gone: `--admin-pass password` is still accepted but also forces
  `must_change`.  The stale prebuilt binary was removed from the repository.
  (main.rs, auth.rs `open_with_policy`, server.rs `SecurityManager::open_with_policy`)

### P1 — high

- **Mirror write failure left the persisted WAL frame in place.** A write
  that failed to the caller (mirror/Strand B error) resurrected itself from
  the WAL on the next restart. `put`/`delete`/`write_batch` now capture the
  pre-append WAL mark and roll the frame back (`WalWriter::logical_len` +
  `WalWriter::rollback_to`, including the rotated-WAL edge case) before
  returning the error; the memtable is only touched after the mirror
  succeeds.  (engine.rs, wal.rs)
- **WAL replay ignored memtable capacity → OOM on restart.** Replaying a
  huge WAL buffered the whole log in memory before the single end-of-loop
  capacity check. The replay loop now flushes incrementally whenever the
  active memtable fills up.  (engine.rs `Engine::open`)
- **Serializable isolation silently degraded to last-writer-wins.** The
  commit lock covered only Serializable commits, so a ReadCommitted rival
  could land between a Serializable transaction's baseline validation and
  its batch. Every commit now holds the commit lock across validate→write.
  (tx.rs `TransactionCoordinator::commit`)
- **Global lock during fsync blocked all reads.** Readers held
  `mutation_gate.read()` for the whole lookup, so the writer's WAL fsync
  stalled every `get`/`scan`. Readers now take a short gate to snapshot the
  active/immutable memtable Arcs and the segment list, then read without the
  gate; tombstone semantics and newest-layer-wins merging keep the results
  linearizable.  (engine.rs `read_snapshot`)
- **Slowloris: only a per-read timeout existed.** A client drip-feeding one
  byte every 9 s could hold an unauthenticated connection forever. An
  absolute 30 s pre-auth deadline now drops the connection regardless of
  traffic.  (server.rs `PRE_AUTH_TOTAL_TIMEOUT`)
- **`doctor` could mutate a live database.** The server now holds an
  exclusive `flock` on `<data-dir>/.dna-db.lock` for its whole lifetime
  (fs2; the OS releases it on process death, so no stale locks), `doctor`
  refuses to run while the lock is held, and two concurrent doctor runs are
  mutually excluded. A second server instance on the same data dir is also
  refused.  (main.rs `acquire_dir_lock`)
- **`expr_to_sql` dropped parentheses.** `a * (b + c)` was rendered as
  `a * b + c`, so materialised `IN (SELECT ...)` subqueries computed
  silently wrong results. Rendering is now precedence-aware.
  (sql_engine.rs `expr_to_sql`)
- **Hash join semantics.** `NULL = NULL` rows were joined (three-valued
  logic violation) and `Int(5)` did not match `Float(5.0)` (per-type hash
  encoding). Join keys now skip NULLs and canonicalise integral floats.
  (sql_engine.rs `hash_join`/`hash_join_left`)
- **Silent integer overflow in TX mutations.** DnaQL `+=`/`-=` wrapped i64
  silently; overflow now aborts the mutation with an error. WHERE-clause
  arithmetic switched from `wrapping_*` to checked ops (overflow yields
  NULL).  (dna_ql.rs `apply_assign`, expr.rs `arith`)
- **Both CI workflows had broken triggers** (`branches: ain]` instead of
  `branches: [main]`) — CI never ran. Fixed in
  `.github/workflows/ci.yml` and `bench-linux.yml`.

### P2 — documentation / quality

- `docs/PROTOCOL.md` and `README.md` claimed per-block **CRC32C (SSE4.2)**;
  the implementation is CRC-32 (IEEE) via `crc32fast`. Docs corrected — the
  on-disk format is unchanged.
- 11 new regression tests lock every fix above in place
  (`security_regression_tests`, `isolation_regression_tests`,
  `rollback_regression_tests`).

### Upgrade notes

- The release profile is now `panic = "unwind"`; the fault guard turns any
  residual panic into a per-request error. If you embed the engine, panics
  no longer abort your process.
- `doctor` exits with an error while a server (or another doctor) holds the
  data directory lock — stop the server first.
- Client scripts that relied on logging in with `admin/password` on a fresh
  data directory must read the generated password from stderr once, or pass
  `--admin-pass`/`DNA_DB_ADMIN_PASS` explicitly.
