# Complete practical guide to the DnaQL language v2.2
**Official DNA-DB Core documentation**

Every example in this guide was **verified** against a DNA-DB 4.0
server (see the smoke tests and `cargo test --lib dna_ql`).

> **Russian:** [DNAQL_MANUAL_RU.md](DNAQL_MANUAL_RU.md)

---

## Chapter 1. Why DnaQL when SQL exists?

Standard SQL balances accounts beautifully, but it is blind to the laws
of nature:
* **Search with mutations.** Finding a sequence with ≤1 substitution
  (SNP) in SQL is a `LIKE` scan or a UDF over every row. In DnaQL it is
  a native bit-parallel Myers: `chr_variants ? 'GATCGATC' ~1` — 100,000
  bp in ~1 ms.
* **Memory.** `VARCHAR(4)` for `'ATGC'` costs 4+ bytes with overhead.
  DnaQL stores 4 nucleotides in **1 byte** (2 bits per base).
* **JOIN.** Relational linking in DnaQL is a direct key-seek
  (**O(1)**) through the complement index, with no hash table built.

At the same time DnaQL **does not replace SQL, it complements it**:
DNA-DB runs both the SQL surface
(SELECT/JOIN/INSERT/UPDATE/DELETE/CREATE) and DnaQL on top of the same
storage. Since **v2.2** the two surfaces are at parity: every SQL
command has a native DnaQL form, and the built-in transpiler
(`sql_to_dnaql`) converts SQL → DnaQL without losing semantics — see
Chapter 5.

---

## Chapter 2. Two syntax styles

### 1. Quick (Quick DnaQL) — for terminals and scripts
* `?` — fuzzy search; `~N` — up to N edits (Levenshtein);
  `-> chr(col)` — link to a related chromosome; `=> emit` — projection.

```dnaql
chr_variants ? 'ATGCGATCGATC' ~1 -> chr_patients(patient_id) => emit locus, mismatches, name;
```

### 2. Standard (Standard DnaQL) — the formal form
```dnaql
FIND PATTERN 'ATGCGATCGATC'
IN CHROMOSOME chr_variants
WHERE HAMMING_DISTANCE <= 1
LINK chr_patients VIA COMPLEMENT(patient_id)
EMIT locus, mismatches, name;
```

`~N` and `LEVENSHTEIN_DISTANCE` compute true Levenshtein
(insertions+deletions+substitutions); `HAMMING_DISTANCE` is
substitutions only. The result always carries the **true** match
distance (`mismatches`), not the threshold.

---

## Chapter 3. Typical tasks

### Task 1: Searching for an oncogene marker with a mutation (SNP)
```dnaql
chr_tp53 ? 'GACTGCCCT' ~1 => emit *;
```
Mechanics: for every row of the chromosome the `sequence` field
(Value::Dna) is taken, bit-parallel Myers runs, and the report contains
locus, offset_bp, mismatches, matched_sequence.

### Task 2: Relational JOIN across three chromosomes
```dnaql
FROM chr_orders
|> LINK chr_users VIA COMPLEMENT(user_id)
|> LINK chr_variants VIA COMPLEMENT(user_id)
|> WHERE total_usd > 300.00
|> EMIT order_id, username, variant_name;
```
Each `LINK` is a direct key-seek `chr_<target>:<fk value>`; the `WHERE`
expression supports comparisons, AND/OR/NOT, `IN (list)`,
`IS [NOT] NULL` and arithmetic.

### Task 3: Atomic transaction
```dnaql
TX {
  chr_accounts:14.balance -= 350.00;
  chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 };
}
```
* `chr:locus.field = v` — assignment; `+=` / `-=` — arithmetic on
  Int/Float (operands must be numeric; you cannot subtract from NULL —
  create the row first).
* `chr += { ... }` without an explicit locus — auto key from the
  integer `*_id` field.
* `DROP LOCUS chr:locus [CASCADE]` — delete a row.
* The whole block commits as **one** batched WAL write (one fsync): a
  crash in the middle of the block leaves no partial effect.

The expanded equivalent:
```dnaql
SYNTHESIZE INTO chr_accounts, chr_orders BEGIN
  chr_accounts:14.balance -= 350.00;
  chr_orders:9021 += { user_id: 14, total_usd: 350.00 };
LIGATE
```

### Task 4: Bio-analytical pipeline
```dnaql
ANALYZE SEQUENCE 'CGCGCGATCGATCGCGC'
|> GC_CONTENT()
|> MELTING_TEMP()
|> REV_COMP()
|> TRANSCRIBE()
|> TRANSLATE();
```
Functions: `GC_CONTENT()` (GC fraction), `MELTING_TEMP()` (Wallace
formula), `REV_COMP()` (bitwise), `TRANSCRIBE()` (T→U), `TRANSLATE()`
(codons → amino acids up to a stop codon). `ANALYZE LOCUS
chr_variants:1 |> ...` is also supported — analyze a sequence taken
from a row.

### Task 5: SQL equivalents in the pipeline (v2.2)
```dnaql
FROM chr_orders |> JOIN chr_users ON user_id = user_id |> WHERE status = 'paid';
FROM chr_orders |> GROUP BY user_id AGG cnt = COUNT(*), total = SUM(total_usd)
             |> WHERE cnt >= 1 |> SORT total DESC;
FROM chr_users |> LEFT JOIN chr_orders ON user_id = user_id;
FROM chr_users |> SORT balance DESC |> OFFSET 5 |> LIMIT 10;
FROM chr_users |> EMIT balance * 2 AS double_balance;
FROM chr_users |> EMIT user_id UNION FROM chr_blacklist |> EMIT user_id |> LIMIT 3;
```
(`WHERE` after `GROUP` is HAVING; `INNER JOIN` may be written as plain
`JOIN`; LEFT/RIGHT/FULL take an optional `OUTER`.)
* `JOIN` is a hash equi-join with SQL semantics: conflicting fields of
  the right side are named `joined_<field>`; LEFT NULL-extends unmatched
  left rows, RIGHT/FULL do the same for the other/both sides; NULL keys
  never match.
* `GROUP` mirrors SQL aggregates: `COUNT(*)` counts every row, the
  others skip NULLs; SUM over integers stays Int, with a Float it is
  Float; AVG is always Float.
* `UNION [ALL]` — column names come from the left side, DISTINCT
  deduplicates full rows, and the trailing `SORT/LIMIT/OFFSET` applies
  to the compound result.

### Task 6: Row-set statements and UPSERT (v2.2)
```dnaql
UPDATE chr_accounts SET status = 'frozen' WHERE balance < 0;
DELETE FROM chr_orders WHERE status = 'abandoned';
TX { chr_accounts:14 ?= { account_id: 14, balance: 0.0 } };
```
* Predicate `UPDATE`/`DELETE`: a top-level statement auto-wraps into one
  atomic TX (one fsync); `DELETE` honours foreign keys —
  `ON DELETE CASCADE` cascades (one level, like SQL), `RESTRICT` fails
  loudly when referencing rows exist.
* `?=` is the DnaQL spelling of `INSERT ... ON CONFLICT DO NOTHING`: an
  existing locus is left untouched; the report marks the insert with an
  `inserted` field.

### Task 7: DDL and introspection (v2.2)
```dnaql
SHOW CHROMOSOMES;
ALTER CHROMOSOME chr_users ADD COLUMN note TEXT;
CREATE INDEX idx_users_note ON chr_users (note);
DROP INDEX idx_users_note ON chr_users;
EXPLAIN FROM chr_users |> WHERE balance > 90.0;
```
(`SHOW CHROMOSOMES` is an alias of `SHOW TABLES`; `EXPLAIN` returns the
plan instead of rows.) DDL goes through the schema/index registry
shared with SQL, so a chromosome created in DnaQL is visible to SQL
clients and vice versa.

### Task 8: Locus storage map: STORAGE_MAP() (v4.1)

A/C/G/T chains are the internal representation — humans do not need to
read them. `STORAGE_MAP()` returns the storage map of one locus:
chromosome, locus, exact key, column type, the human-readable value and
its physical representation (RowCodec tag, size). It works on ANY row —
a sequence field is not required; this is the DnaQL counterpart of the
SQL statement `SHOW STORAGE ... WHERE <pk> = <literal>`.

```dnaql
ANALYZE LOCUS chr_orders:1042 |> STORAGE_MAP();
-- function    chromosome  locus  storage_key       column   sql_type  value    stored_as
-- STORAGE_MAP chr_orders  1042   chr_orders:1042   order_id INT       1042     RowCodec tag 1: i64 little-endian (8 B) = 1042
-- STORAGE_MAP chr_orders  1042   chr_orders:1042   status   TEXT      PENDING  RowCodec tag 3: UTF-8 text, u32 length + 7 B
```

**Admin panel (v4.1):** the 🔬 button on every row of the table browser
opens the SQL `SHOW STORAGE` view of that row; its «🧬 STORAGE_MAP в
DnaQL-консоли» button prefills exactly this `ANALYZE LOCUS <table>:<pk>
|> STORAGE_MAP();` command in the DnaQL console and runs it — the same
physical map in DnaQL terms, one click from either console.

---

## Chapter 4. Rosetta Stone cheat sheet: SQL vs DnaQL

| Operation | ANSI SQL | Standard DnaQL | Quick DnaQL |
|:---|:---|:---|:---|
| Search with mutation | *not native* | `FIND PATTERN 'ACGT' IN chr WHERE HAMMING_DISTANCE <= 1` | `chr ? 'ACGT' ~1` |
| Relational JOIN | `SELECT ... JOIN b ON ...` | `FROM a JOIN b ON x = y` (v2.2) or `FROM a LINK b VIA COMPLEMENT(a_id)` (key-seek) | `FROM a |> JOIN b ON x = y` / `FROM a |> LINK b` |
| Aggregates / GROUP BY | `COUNT/SUM/AVG/MIN/MAX + GROUP BY` | `FROM a |> GROUP BY g AGG n = COUNT(*)` (v2.2) | — |
| HAVING | `HAVING COUNT(*) > 2` | `\|> GROUP ... \|> WHERE n > 2` (v2.2) | — |
| OFFSET | `LIMIT 10 OFFSET 5` | `\|> OFFSET 5 \|> LIMIT 10` (v2.2) | — |
| Computed projection | `SELECT x * 2 AS d` | `\|> EMIT x * 2 AS d` (v2.2) | — |
| UNION | `SELECT ... UNION [ALL] SELECT ...` | `FROM a \|> ... UNION [ALL] FROM b \|> ...` (v2.2) | — |
| UPSERT (DO NOTHING) | `INSERT ... ON CONFLICT DO NOTHING` | `TX { chr:k ?= { ... } }` (v2.2) | — |
| UPDATE by predicate | `UPDATE t SET ... WHERE ...` | `UPDATE t SET ... WHERE ...` (v2.2) | — |
| DELETE by predicate | `DELETE FROM t WHERE ...` | `DELETE FROM t WHERE ...` (v2.2) | `TX { DROP LOCUS t:7 }` (by locus) |
| Introspection | `SHOW TABLES` | `SHOW CHROMOSOMES` (v2.2) | — |
| Online DDL | `ALTER TABLE t ADD COLUMN c T` | `ALTER CHROMOSOME t ADD COLUMN c T` (v2.2) | — |
| Indexes | `CREATE INDEX i ON t (c)` | `CREATE INDEX i ON t (c)` (v2.2) | — |
| Query plan | `EXPLAIN <query>` | `EXPLAIN FROM a \|> ...` (v2.2) | — |
| Transaction | `BEGIN; UPDATE; INSERT; COMMIT;` | `SYNTHESIZE ... BEGIN ... LIGATE` | `TX { ... }` |
| Reverse complement | *external library* | `ANALYZE SEQUENCE s |> REV_COMP()` | `ANALYZE SEQUENCE s |> REV_COMP()` |
| Field filter | `WHERE x > 1` | `FROM a |> WHERE x > 1` | `FROM a |> WHERE x > 1` |

---

## Chapter 5. The SQL → DnaQL transpiler (v2.2)

The built-in transpiler (`SqlEngine::sql_to_dnaql`) converts parsed SQL
into equivalent DnaQL text. Coverage equals the full SQL engine surface:

| SQL | DnaQL |
|:---|:---|
| `SELECT cols FROM t WHERE ... ORDER BY ... LIMIT/OFFSET` | `FROM t \|> WHERE ... \|> SORT ... \|> OFFSET n \|> LIMIT n \|> EMIT ...` |
| `INNER/LEFT/RIGHT/FULL [OUTER] JOIN ... ON ...` | `\|> [LEFT\|RIGHT\|FULL] JOIN t2 ON x = y` |
| `GROUP BY` + aggregates + `HAVING` | `\|> GROUP BY g AGG name = FN(col)` + `\|> WHERE` (HAVING) |
| `UNION [ALL] ... ORDER BY/LIMIT` | `... UNION [ALL] ... \|> SORT/LIMIT/OFFSET` |
| `INSERT ... VALUES ...` | `TX { chr:pk += { ... }; ... }` |
| `INSERT ... ON CONFLICT DO NOTHING` | `TX { chr:pk ?= { ... } }` |
| `INSERT ... ON CONFLICT DO UPDATE SET <all non-PK>` | `TX { chr:pk += { ... } }` (exact upsert) |
| `UPDATE ... WHERE pk = N` | `TX { chr:pk.field = v }` (compact locus form) |
| `UPDATE ... WHERE <predicate>` | `UPDATE chr SET ... WHERE <predicate>` |
| `DELETE FROM t WHERE pk = N` / by predicate | `TX { DROP LOCUS t:pk }` / `DELETE FROM t WHERE ...` |
| `CREATE/DROP TABLE`, `ALTER TABLE ADD COLUMN` | `CREATE/DROP CHROMOSOME`, `ALTER CHROMOSOME ADD COLUMN` |
| `CREATE [UNIQUE] INDEX / DROP INDEX` | the same form in DnaQL |
| `SHOW TABLES` | `SHOW CHROMOSOMES` |
| `EXPLAIN SELECT ...` | `EXPLAIN FROM ...` |

SQL-only constructs fail with an explicit error naming the construct
(never a silent approximation): session `BEGIN/COMMIT/ROLLBACK` (DnaQL
wraps writes in `TX {}` blocks), CTEs (`WITH`), scalar/`EXISTS`
subqueries and window functions, and `ON CONFLICT DO UPDATE` with a
partial SET list (a partial keep-old-values update has no DnaQL
spelling — run it as SQL).

## Chapter 6. Errors and edge cases

| Situation | Behaviour |
|---|---|
| Pattern empty or longer than 64 bp | error `pattern length must be 1..=64` |
| Character outside ACGT in pattern/sequence | error `Invalid nucleotide` |
| Unterminated quote | `unterminated string literal` |
| Expression after WHERE not recognized | error naming the expected token |
| Trailing tokens after a query | `unexpected trailing tokens` (never silence) |
| `-= 5` on a NULL field | `+= requires numeric operands` — create the row first |
| FK link to a non-existent row | `link_missing` field in the result (not an error) |
| Unknown bio function | `unknown bio function '<name>'` |
| `DELETE FROM t` with RESTRICT references | `RESTRICT violation: ... is referenced by ...` |
| `UPDATE t SET <unknown field>` | `unknown column ... in UPDATE` |
| `EXPLAIN` of a write (`TX { ... }`) | rejected without executing — EXPLAIN is read-only |

Next: [DNAQL_SPEC.md](DNAQL_SPEC.md) — formal grammar and semantics.
