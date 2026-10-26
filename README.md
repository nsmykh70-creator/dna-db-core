# DNA-DB Core v3.2 «Paneldb Security» (production hardening)

> This repository is bilingual: **English** (this file) · [Русский](README_RU.md)

A high-performance embeddable and client/server database engine written in Rust, featuring an LSM-Tree architecture, 2-bit nucleotide compression, dual-strand disk mirroring, the **DnaQL** query language, a **full SQL subset** (aggregates, GROUP BY/HAVING, ORDER BY, IN/subqueries, **secondary and range indexes**), a built-in **web admin panel** (phpMyAdmin-style) and self-healing on sector corruption.

## 0.2. Production security (v3.2)

| Control | Implementation |
|---|---|
| Transport | **rustls TLS 1.2/1.3 on both listeners** (binary TCP protocol and HTTPS panel), HSTS on HTTPS, no plaintext fallback on the TLS port |
| Certificates | `--tls-cert/--tls-key` PEM pair; a self-signed localhost pair is generated under `DATA/tls/` on first start (development only) |
| Passwords | **Argon2id** PHC hashes in `DATA/users.json`, per-user salt, timing-equalised unknown users |
| Users | bootstrap `admin` / `password`, then create/disable/delete users and reset/change passwords in the panel; roles `admin`, `operator`, `viewer`; the last active admin cannot be removed |
| Sessions | 256-bit CSPRNG bearer tokens, 12 h TTL, revoked on password change/disable |
| Brute force | persistent **per-IP and per-username lockout** (5 failures / 60 s), global failure throttle, serialised Argon2 (bounded memory) |
| Encryption at rest | XChaCha20-Poly1305 AEAD per value before WAL/SST/mirror writes; key from `DNA_DB_ENCRYPTION_KEY`, `--encryption-key-file` or `DATA/.dna-db.key`; legacy plaintext rows remain readable |
| Panel hardening | CSP with SHA-256 hash for the inline SPA script (no `unsafe-inline` for scripts), `X-Frame-Options: DENY`, `nosniff`, `no-referrer`, restrictive `Permissions-Policy` |
| Pre-auth DoS | 64 KiB pre-auth frame cap (instead of 128 × 16 MiB allocation), 10 s pre-auth read timeout, 64 concurrent panel connections, 128 TCP connections |
| Bind | loopback (`127.0.0.1`) by default; public exposure is an explicit `--bind` decision |

```bash
# production-ish start: TLS + encryption are on by default.
# The first start creates admin/password — change it in the panel immediately.
./target/release/dna-db-core serve --data-dir ./dna_data --port 9999 --admin-port 8080
# https://127.0.0.1:8080/  (self-signed cert: pass DATA/tls/cert.pem to clients)

# custom CA / public certificate:
./target/release/dna-db-core serve --data-dir ./dna_data \
  --tls-cert /etc/letsencrypt/live/db.example/fullchain.pem \
  --tls-key  /etc/letsencrypt/live/db.example/privkey.pem \
  --admin-pass 'change-me-now'

# shell over TLS against a self-signed local server:
./target/release/dna-db-core shell --tls-ca ./dna_data/tls/cert.pem

# development-only escapes (never on an untrusted network):
#   --no-tls   --no-encryption
```

> **Back up the encryption key separately from the data directory.** Without the
> key the values are unrecoverable. Record keys and row counts remain visible
> in WAL/SST indexes by design; the AEAD envelope protects application values,
> not the LSM key space. See [SECURITY.md](SECURITY.md) for the threat model,
> operational checklist and residual `cargo audit` warnings.

## 0.1. What's new in v3.1 — SQL and the web panel

| Feature | Details | Verification |
|---|---|---|
| Aggregates | `COUNT(*)/COUNT(col)`, `SUM`, `AVG`, `MIN`, `MAX` | aggregate unit tests |
| `GROUP BY` / `HAVING` | grouping + filter on aggregates (aggregates in HAVING do not leak into the output) | unit tests |
| `ORDER BY` (ASC/DESC) | sorting by columns/expressions, including aggregates | unit tests |
| `IN`-lists and subqueries | `WHERE c IN (1,2,3)` and `WHERE c IN (SELECT …)` in SELECT/UPDATE/DELETE | unit tests |
| `LIMIT` / `OFFSET` | pagination | used by the web panel |
| **Secondary indexes** | `CREATE INDEX idx ON t (col)` / `DROP INDEX` — stored in the schema, honoured by DML | index unit tests |
| **IndexScan plans** | equality/`IN`/range on an indexed column → `IndexScan(...)` instead of `SeqScan` | plan unit tests |
| **Web panel** | `serve --admin-port 8080`: overview/health, tables + paginated browser, indexes, SQL and DnaQL consoles, user administration | HTTPS/API integration + security probe |
| **Row editing** | click a row → modal dialog (phpMyAdmin-style): schema-driven fields, NULL checkboxes, live SQL preview, UPDATE/DELETE with confirmation | manual e2e scenario in the browser |
| **SQL tokenizer** | negative numbers (`-25.5`) and standard quote escaping (`'O''Brien'`) | `tokenizer_negative_numbers_and_escaped_quotes`, `update_with_negative_and_quoted_values` |

```bash
# admin panel (credentials are the same as for the TCP server):
./target/release/dna-db-core serve --data-dir ./dna_data --admin-pass secret --admin-port 8080
# then open https://127.0.0.1:8080/ (TLS is on by default)
```

Panel screenshots: [docs/screenshots](docs/screenshots).

---

# DNA-DB Core v3.0 «Repairosome» (Self-Healing Release)

A high-performance embeddable and client/server database engine written in Rust, featuring an LSM-Tree architecture, 2-bit nucleotide sequence compression, dual-strand disk mirroring (Dual-Strand Hot-Swap), the domain-specific **DnaQL** language, a SQL subset, proven `kill -9` safety — and **self-healing on SSD sector corruption** modelled on DNA repair.

> **Release principle of v3.0:** every claim is backed by code and tests.
> The readiness criterion is met: **CHAOS PASS — 100,000/100,000 keys
> recovered** after random corruption of 4 KiB sectors
> ([docs/CHAOS_REPORT.md](docs/CHAOS_REPORT.md)).

## 0. What's new in v3.0 — Repairosome (self-healing)

DNA → engineering analogy:

| Mechanism in DNA | Implementation in DNA-DB |
|---|---|
| Genome redundancy | Reed–Solomon GF(2^8) inside every segment: k=4 data + m=2 parity, 4 KiB blocks (format `DNASST02`) |
| Damage sensors (checkpoint kinases) | CRC32C per 4 KiB block (SSE4.2), duplicated CRC table, redundant superblock/footer |
| Excision repair (BER/NER) | on-read repair: CRC miss → stripe reconstruction from surviving shards → rewrite → journal |
| Scanner enzymes | background scrubber (on by default: 24 h, 32 MiB/s, configurable) |
| Homologous recombination | cascade: parity → segment copy on Strand B; auto-resync after repair |
| Mutation log | `repair_journal.jsonl` + lesions_found/repaired/unrecoverable metrics |
| Immune surveillance | `dna-db-core doctor --data-dir D [--fix] [--migrate] [--json]` — offline scan, repair, v2.1→v3.0 migration |
| Cell boundary | quarantine: an unrecoverable stripe honestly returns an error localised to segment:stripe:shard, the DB keeps running |

Proofs (all executable):

- **Chaos test**: `python3 scripts/chaos_test.py` — 100,000 keys → 80 sectors corrupted (bit-flip/zero-fill) → `doctor --fix` → 100 % recovery; live mode (corruption on a running server); over-damage (honest failure beyond the guarantee). Results: [docs/CHAOS_REPORT.md](docs/CHAOS_REPORT.md).
- **Tests**: 174 green (`cargo test --release`) — 159 library + 15 integration; includes RS codec against every loss pattern, interleaving, repair pipeline, quarantine, migration, WAL rotation, closed-WAL parity and encryption integration.
- **Format**: DNASST02 is documented in [docs/PROTOCOL.md](docs/PROTOCOL.md); DNASST01 is readable, migration is manual via `doctor --migrate` (reliability first).

Self-healing quick start:

```bash
cargo build --release
# operation (scrubber is on by default):
./target/release/dna-db-core serve --data-dir ./dna_data --admin-pass secret
# diagnostics and repair (offline):
./target/release/dna-db-core doctor --data-dir ./dna_data            # scan
./target/release/dna-db-core doctor --data-dir ./dna_data --fix      # repair
./target/release/dna-db-core doctor --data-dir ./dna_data --migrate  # v2.1 → v3.0
# in a session: SHOW HEALTH;  /  VERIFY CHROMOSOME chr_users;  /  REPAIR CHROMOSOME chr_users;
```

---

# DNA-DB Core v2.1.0 (baseline functionality)

A high-performance embeddable and client/server database engine written in Rust, featuring an LSM-Tree architecture, 2-bit nucleotide sequence compression (4 bits/base → 0.25 bytes), dual-strand disk mirroring (Dual-Strand Hot-Swap), the domain-specific **DnaQL** language, a SQL subset and proven `kill -9` safety.

> **Release principle of v2.1.0:** every claim in this README is backed by code and tests. Anything not implemented is honestly marked in [ROADMAP.md](docs/ROADMAP.md).

## 1. What actually works (and how it is proven)

| Capability | Implementation | Verification |
|---|---|---|
| WAL + MemTable + SSTable storage | `wal.rs`, `memtable.rs`, `segment.rs` | 21 unit tests + 9 integration tests |
| Recovery after kill -9 | WAL replay in `Engine::open`, automatic torn-tail truncation | `scripts/kill9_test.py`: **99,091 records, 0 lost** |
| Flush to disk + SegmentReader | bloom filter + sparse index + CRC32 per record | unit tests, reopening tests |
| Segment compaction | merge-all, newest versions win | `compaction_merges_and_preserves_data` |
| Atomic transactions | batched WAL write (one fsync = one atomic interval) | `write_batch_is_atomic_on_recovery` |
| DnaQL (Standard + Quick) | tokenizer → parser → executor over real storage | 8 unit tests + live smoke |
| SQL subset | SELECT/JOIN(WHERE/IN-subqueries)/GROUP BY/HAVING/ORDER BY/LIMIT/OFFSET, aggregates, INSERT, UPDATE, DELETE (CASCADE), CREATE/DROP TABLE, **CREATE/DROP INDEX + IndexScan** | 40+ unit tests |
| Web admin panel | HTTPS + SPA (no external dependencies): overview, tables, SQL/DnaQL consoles, user management | `admin::tests::panel_requires_auth_and_serves_api` + `scripts/security_probe.py` |
| Bitap/Myers fuzzy search | correct Wu-Manber, **true** distance, Levenshtein + Hamming | 8000-case fuzz vs a reference DP |
| Bio plugins | real Smith-Waterman, PAM scanner, ORF-finder, GC windows | unit tests |
| TCP server | thread-per-connection, TLS, 16 MiB frame cap (64 KiB pre-auth), Argon2id, persistent IP/username lockout | unit tests + `scripts/security_probe.py` |
| CLI | `serve` / `bench` / `shell` | manual verification |
| Metrics | real counters (ops, bytes, average latencies) | unit test |

## 2. Quick start

### Build
```bash
cargo build --release
# binary: target/release/dna-db-core
```

### Run the server
```bash
./target/release/dna-db-core serve \
  --data-dir ./dna_data \
  --port 9999 \
  --admin-pass password \
  --admin-port 8080 \
  --sync strict \
  --demo-data          # optional: demo tables chr_users/chr_orders/chr_patients/chr_variants
```
A password is required (`--admin-pass` or the `DNA_DB_ADMIN_PASS` variable) — there is no default password.
Once running, open **https://127.0.0.1:8080/** (the generated certificate is self-signed; pass `DATA/tls/cert.pem` to clients or use a real certificate).
`--admin-port 0` disables the panel; the panel listens on the same interface as `--bind` (loopback by default).

### Shell client
```bash
./target/release/dna-db-core shell --port 9999 --tls-ca ./dna_data/tls/cert.pem
dna-db> SELECT order_id, username, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE total_usd > 400;
dna-db> ANALYZE SEQUENCE 'ATGGCCTAA' |> GC_CONTENT() |> TRANSLATE();
dna-db> .status
```

### As a library (Cargo)
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

engine.checkpoint()?;        // flush all memtables + truncate WAL
engine.compact()?;           // merge segments
```

## 3. DnaQL — the query language

Two equivalent syntaxes (see [DNAQL_SPEC.md](docs/DNAQL_SPEC.md) and [DNAQL_MANUAL.md](docs/DNAQL_MANUAL.md)):

```dnaql
-- Quick: fuzzy search linked to the patient
chr_variants ? 'GATCGATC' ~1 -> chr_patients(patient_id) => emit locus, mismatches, name;

-- Standard: the same thing, formally
FIND PATTERN 'GATCGATC'
IN CHROMOSOME chr_variants
WHERE HAMMING_DISTANCE <= 1
LINK chr_patients VIA COMPLEMENT(patient_id)
EMIT locus, mismatches, name;

-- Relational pipeline
FROM chr_orders |> WHERE total_usd > 300 |> LINK chr_users VIA COMPLEMENT(user_id) |> EMIT order_id, username;

-- Atomic transaction (one fsync = one durability interval)
TX {
  chr_accounts:14.balance -= 350.00;
  chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 };
}

-- Bio analysis
ANALYZE SEQUENCE 'CGCGCGATCGATCGCGC' |> GC_CONTENT() |> MELTING_TEMP() |> REV_COMP() |> TRANSLATE();
```

`LINK VIA COMPLEMENT` is an O(1) direct key-seek on the primary key of the target chromosome (no hash join).

## 4. SQL subset (v3.1 — fully working)

```sql
CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id));
CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT,
  PRIMARY KEY (order_id),
  FOREIGN KEY (user_id) REFERENCES chr_users(user_id) ON DELETE CASCADE);

INSERT INTO chr_users (user_id, username, balance) VALUES (14, 'prof_crick', 890.00);
INSERT INTO chr_users (user_id = 18, username = 'rosalind_f', balance = 12.5);

-- projection, filters, IN-lists and subqueries, sorting, pagination
SELECT order_id, total_usd FROM chr_orders
  WHERE status IN ('paid', 'shipped')
    AND user_id IN (SELECT user_id FROM chr_users WHERE balance > 100)
  ORDER BY total_usd DESC LIMIT 10 OFFSET 20;

-- aggregates + grouping + filter on aggregates
SELECT status, COUNT(*) AS n, SUM(total_usd) AS revenue
  FROM chr_orders GROUP BY status HAVING COUNT(*) > 1 ORDER BY revenue DESC;

-- secondary indexes: equality, IN and ranges go through IndexScan, not SeqScan
CREATE INDEX idx_orders_status ON chr_orders (status);
CREATE INDEX idx_orders_total  ON chr_orders (total_usd);
EXPLAIN SELECT order_id FROM chr_orders WHERE total_usd > 100 AND total_usd < 600;
--  -> IndexScan(chr_orders.total_usd >100 AND <600) -> Filter(...) -> Sort(...)
DROP INDEX idx_orders_status ON chr_orders;

UPDATE chr_users SET balance = 950.00 WHERE user_id = 14;
DELETE FROM chr_users WHERE user_id = 14;   -- cascades to chr_orders
DROP TABLE chr_orders;
```
INSERT supports two forms: positional `(cols) VALUES (vals)` and typed `(col = val, ...)`.

Primary-key access is a point-lookup (bloom + sparse index), an secondary index gives `IndexScan`;
everything else is an honest `SeqScan` with a filter. JOIN runs as a hash join over the FK complement index.
The plan shows what actually happens: `SeqScan(t) -> InnerHashJoin(a.x, b.y) -> Filter(...) -> Project([...])`.

Full SQL manual: [docs/SQL_MANUAL.md](docs/SQL_MANUAL.md).

## 5. Durability

- `--sync strict` — fsync of every write **before** it is acknowledged to the client. Proven with `kill -9`: 99,091 acknowledged records — 0 lost.
- `--sync batched` — buffering, fsync every 64 KiB (a crash loses the unflushed tail; on a clean shutdown the buffer is flushed in `Drop`).
- `--sync async` — page cache only (for ephemeral workloads).
- Torn writes: CRC32 on every WAL frame and every segment record; a damaged tail is truncated automatically on open.

## 6. Architecture

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

Keys: `<chromosome>:<locus>` (table rows are binary RowCodec payloads), `__schema:<table>` — JSON schemas.
Full format description: [PROTOCOL.md](docs/PROTOCOL.md).

## 7. Tests and benchmarks

```bash
cargo test --release                # 174 tests (159 lib + 15 integration)
python3 scripts/kill9_test.py       # kill -9 durability (requires a built release binary)
python3 scripts/chaos_test.py       # sector corruption -> doctor --fix -> 100% recovery
./target/release/dna-db-core bench  # real measurements (see docs/BENCHMARKS.md)
```

Test results of the full verification run: [docs/TEST_RESULTS.md](docs/TEST_RESULTS.md).

Three-way benchmark **DNA-DB vs MySQL vs PostgreSQL** (charts, methodology, raw data):
[docs/DB_COMPARISON.md](docs/DB_COMPARISON.md).

SQL feature conformance table (JOIN / aggregates / indexes, 32 checks on all three systems):
[docs/SQL_FEATURES.md](docs/SQL_FEATURES.md).

Web panel screenshots and test reports: [docs/screenshots](docs/screenshots), [docs/](docs/).

## 8. License and status

The project uses **dual licensing** — different parts under different licenses:

| Component | License | Files |
|---|---|---|
| Web admin panel | **Apache-2.0** | `src/admin.rs`, `src/admin.html` |
| Engine core, SQL/DnaQL, TCP server, tests, scripts, documentation | **AGPL-3.0-only** | all other files |

Every source file carries an SPDX line that unambiguously states its
license (`// SPDX-License-Identifier: …`). Full texts:
[LICENSE-AGPL](LICENSE-AGPL), [LICENSE-APACHE](LICENSE-APACHE), summary —
[LICENSE](LICENSE).

**Why:** the engine core is AGPL-3.0-only, so any modification and network
use stays open; the interactive web panel is permissive Apache-2.0
(like phpMyAdmin) and can be embedded and reused freely. Apache-2.0 is
one-way compatible with AGPL-3.0: the combined binary (core + panel) is
distributed under AGPL-3.0, retaining the Apache notices in the panel sources.

The v2.1.0 release is a working engine of a new kind (genome-relational model), suitable for embedding and client/server operation. Current limitations (MVCC, distributed 2PC, io_uring, WASM plugins, long bitap patterns) are recorded in [docs/ROADMAP.md](docs/ROADMAP.md).
