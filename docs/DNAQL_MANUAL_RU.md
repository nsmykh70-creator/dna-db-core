# Полное практическое руководство по языку DnaQL v2.1
**Официальная документация DNA-DB Core**

Все примеры в этом руководстве **проверены** на сервере DNA-DB 2.1.0
(см. smoke-тесты и `cargo test --lib dna_ql`).

---

## Глава 1. Зачем DnaQL, если есть SQL?

Стандартный SQL прекрасно складывает балансы, но слеп к законам природы:
* **Поиск с мутациями.** Найти последовательность с ≤1 заменой (SNP) в SQL — это
  `LIKE`-скан или UDF по всем строкам. В DnaQL это нативный bit-parallel Myers:
  `chr_variants ? 'GATCGATC' ~1` — 100 000 bp за ~1 мс.
* **Память.** `VARCHAR(4)` для `'ATGC'` — 4+ байта с оверхедом. DnaQL хранит
  4 нуклеотида в **1 байте** (2 бита на основание).
* **JOIN.** Реляционное сцепление в DnaQL — прямой key-seek
  (**O(1)**) по комплементарному указателю, без построения hash table.

При этом DnaQL **не заменяет SQL, а дополняет его**: в DNA-DB 2.1 работает и
SQL-подмножество (SELECT/JOIN/INSERT/UPDATE/DELETE/CREATE), и DnaQL — поверх
одного и того же хранилища.

---

## Глава 2. Два стиля синтаксиса

### 1. Быстрый (Quick DnaQL) — для терминала и скриптов
* `?` — нечёткий поиск; `~N` — до N правок (Levenshtein);
  `-> chr(col)` — линк на связанную хромосому; `=> emit` — проекция.

```dnaql
chr_variants ? 'ATGCGATCGATC' ~1 -> chr_patients(patient_id) => emit locus, mismatches, name;
```

### 2. Стандартный (Standard DnaQL) — формальная запись
```dnaql
FIND PATTERN 'ATGCGATCGATC'
IN CHROMOSOME chr_variants
WHERE HAMMING_DISTANCE <= 1
LINK chr_patients VIA COMPLEMENT(patient_id)
EMIT locus, mismatches, name;
```

`~N` и `LEVENSHTEIN_DISTANCE` считают настоящий Levenshtein (вставки+удаления+замены);
`HAMMING_DISTANCE` — только замены. Результат всегда содержит **истинную**
дистанцию совпадения (`mismatches`), а не порог.

---

## Глава 3. Типовые задачи

### Задача 1: Поиск онкомаркера с мутацией (SNP)
```dnaql
chr_tp53 ? 'GACTGCCCT' ~1 => emit *;
```
Механика: по каждой строке хромосомы берётся поле `sequence` (Value::Dna),
запускается bit-parallel Myers, отчёт — locus, offset_bp, mismatches,
matched_sequence.

### Задача 2: Реляционный JOIN трёх хромосом
```dnaql
FROM chr_orders
|> LINK chr_users VIA COMPLEMENT(user_id)
|> LINK chr_genomic_variants VIA COMPLEMENT(user_id)
|> WHERE total_usd > 300.00
|> EMIT order_id, username, variant_name;
```
Каждый `LINK` — прямой key-seek `chr_<цель>:<значение FK>`; выражение `WHERE`
поддерживает сравнения, AND/OR/NOT, IS NULL.

### Задача 3: Атомарная транзакция
```dnaql
TX {
  chr_accounts:14.balance -= 350.00;
  chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 };
}
```
* `chr:locus.field = v` — присваивание; `+=` / `-=` — арифметика над Int/Float
  (операнды должны быть числовыми; из NULL вычитать нельзя — сначала создайте строку).
* `chr += { ... }` без явного locus — авто-ключ из целочисленного поля `*_id`.
* `DROP LOCUS chr:locus [CASCADE]` — удаление строки.
* Весь блок коммитится **одной** batch-записью WAL (один fsync): крэш посреди
  блока не оставляет частичный эффект.

Эквивалент в развёрнутой форме:
```dnaql
SYNTHESIZE INTO chr_accounts, chr_orders BEGIN
  chr_accounts:14.balance -= 350.00;
  chr_orders:9021 += { user_id: 14, total_usd: 350.00 };
LIGATE
```

### Задача 4: Био-аналитический конвейер
```dnaql
ANALYZE SEQUENCE 'CGCGCGATCGATCGCGC'
|> GC_CONTENT()
|> MELTING_TEMP()
|> REV_COMP()
|> TRANSCRIBE()
|> TRANSLATE();
```
Функции: `GC_CONTENT()` (доля GC), `MELTING_TEMP()` (Wallace/формула),
`REV_COMP()` (побитовый), `TRANSCRIBE()` (T→U), `TRANSLATE()`
(кодоны → аминокислоты до стоп-кодона). Также поддерживается
`ANALYZE LOCUS chr_variants:1 |> ...` — анализ последовательности из строки.

---

## Глава 4. Шпаргалка Rosetta Stone: SQL vs DnaQL

| Операция | ANSI SQL | Standard DnaQL | Quick DnaQL |
|:---|:---|:---|:---|
| Поиск с мутацией | *нет нативно* | `FIND PATTERN 'ACGT' IN chr WHERE HAMMING_DISTANCE <= 1` | `chr ? 'ACGT' ~1` |
| Реляционный JOIN | `SELECT ... JOIN b ON ...` | `FROM a LINK b VIA COMPLEMENT(a_id)` | `FROM a |> LINK b => emit *` |
| Транзакция | `BEGIN; UPDATE; INSERT; COMMIT;` | `SYNTHESIZE ... BEGIN ... LIGATE` | `TX { ... }` |
| Обратный комплемент | *внешняя библиотека* | `ANALYZE SEQUENCE s |> REV_COMP()` | `ANALYZE SEQUENCE s |> REV_COMP()` |
| Фильтр по полю | `WHERE x > 1` | `FROM a |> WHERE x > 1` | `FROM a |> WHERE x > 1` |

---

## Глава 5. Ошибки и пограничные случаи

| Ситуация | Поведение |
|---|---|
| Паттерн пуст или длиннее 64 bp | ошибка `pattern length must be 1..=64` |
| Символ вне ACGT в паттерне/последовательности | ошибка `Invalid nucleotide` |
| Незакрытая кавычка | `unterminated string literal` |
| Выражение после WHERE не распознано | ошибка с указанием ожидаемого токена |
| `-= 5` от NULL-поля | `+= requires numeric operands` — сначала создайте строку |
| FK-линк на несуществующую строку | поле `link_missing` в результате (не ошибка) |
| Неизвестная био-функция | `unknown bio function '<имя>'` |

Далее: [DNAQL_SPEC.md](DNAQL_SPEC.md) — формальная грамматика и семантика.
