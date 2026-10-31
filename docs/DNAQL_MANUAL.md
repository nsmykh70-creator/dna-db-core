# Complete practical guide to the DnaQL language v2.1
**Official DNA-DB Core documentation**

Every example in this guide was **verified** against a DNA-DB 2.1.0
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
DNA-DB 2.1 runs both the SQL subset
(SELECT/JOIN/INSERT/UPDATE/DELETE/CREATE) and DnaQL on top of the same
storage.

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
|> LINK chr_genomic_variants VIA COMPLEMENT(user_id)
|> WHERE total_usd > 300.00
|> EMIT order_id, username, variant_name;
```
Each `LINK` is a direct key-seek `chr_<target>:<fk value>`; the `WHERE`
expression supports comparisons, AND/OR/NOT, IS NULL.

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

---

## Chapter 4. Rosetta Stone cheat sheet: SQL vs DnaQL

| Operation | ANSI SQL | Standard DnaQL | Quick DnaQL |
|:---|:---|:---|:---|
| Search with mutation | *not native* | `FIND PATTERN 'ACGT' IN chr WHERE HAMMING_DISTANCE <= 1` | `chr ? 'ACGT' ~1` |
| Relational JOIN | `SELECT ... JOIN b ON ...` | `FROM a LINK b VIA COMPLEMENT(a_id)` | `FROM a |> LINK b => emit *` |
| Transaction | `BEGIN; UPDATE; INSERT; COMMIT;` | `SYNTHESIZE ... BEGIN ... LIGATE` | `TX { ... }` |
| Reverse complement | *external library* | `ANALYZE SEQUENCE s |> REV_COMP()` | `ANALYZE SEQUENCE s |> REV_COMP()` |
| Field filter | `WHERE x > 1` | `FROM a |> WHERE x > 1` | `FROM a |> WHERE x > 1` |

---

## Chapter 5. Errors and edge cases

| Situation | Behaviour |
|---|---|
| Pattern empty or longer than 64 bp | error `pattern length must be 1..=64` |
| Character outside ACGT in pattern/sequence | error `Invalid nucleotide` |
| Unterminated quote | `unterminated string literal` |
| Expression after WHERE not recognized | error naming the expected token |
| `-= 5` on a NULL field | `+= requires numeric operands` — create the row first |
| FK link to a non-existent row | `link_missing` field in the result (not an error) |
| Unknown bio function | `unknown bio function '<name>'` |

Next: [DNAQL_SPEC.md](DNAQL_SPEC.md) — formal grammar and semantics.
