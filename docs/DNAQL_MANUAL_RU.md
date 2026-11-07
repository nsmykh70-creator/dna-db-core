# Полное практическое руководство по языку DnaQL v2.2

> **English:** [DNAQL_MANUAL.md](DNAQL_MANUAL.md)
**Официальная документация DNA-DB Core**

Все примеры в этом руководстве **проверены** на сервере DNA-DB 4.0
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

При этом DnaQL **не заменяет SQL, а дополняет его**: в DNA-DB работает и
SQL-подмножество (SELECT/JOIN/INSERT/UPDATE/DELETE/CREATE), и DnaQL — поверх
одного и того же хранилища. Начиная с **v2.2** поверхностей равны: каждая
SQL-команда имеет нативную DnaQL-форму, а встроенный транслятор
(`sql_to_dnaql`) переводит SQL → DnaQL без потери семантики — см. Главу 5.

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
|> LINK chr_variants VIA COMPLEMENT(user_id)
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

### Задача 5: SQL-эквиваленты в конвейере (v2.2)
```dnaql
FROM chr_orders |> JOIN chr_users ON user_id = user_id |> WHERE status = 'paid';
FROM chr_orders |> GROUP BY user_id AGG cnt = COUNT(*), total = SUM(total_usd)
             |> WHERE cnt >= 1 |> SORT total DESC;
FROM chr_users |> LEFT JOIN chr_orders ON user_id = user_id;
FROM chr_users |> SORT balance DESC |> OFFSET 5 |> LIMIT 10;
FROM chr_users |> EMIT balance * 2 AS double_balance;
FROM chr_users |> EMIT user_id UNION FROM chr_blacklist |> EMIT user_id |> LIMIT 3;
```
(`WHERE` после `GROUP` — это HAVING; вместо `INNER JOIN` можно писать просто
`JOIN`; LEFT/RIGHT/FULL принимают необязательный `OUTER`.)
* `JOIN` — hash equi-join с SQL-семантикой: конфликтующие поля правой стороны
  именуются `joined_<поле>`; LEFT продлевает непарные строки NULL-полями,
  RIGHT/FULL — то же для другой/обеих сторон; NULL-ключи не совпадают.
* `GROUP` зеркалит SQL-агрегаты: COUNT(*) считает все строки, остальные
  функции пропускают NULL; SUM по целым остаётся Int, с Float — Float;
  AVG всегда Float.
* `UNION [ALL]` — имена колонок от левой стороны, DISTINCT дедуплицирует,
  хвостовые `SORT/LIMIT/OFFSET` применяются к объединению.

### Задача 6: Строчные операции и UPSERT (v2.2)
```dnaql
UPDATE chr_accounts SET status = 'frozen' WHERE balance < 0;
DELETE FROM chr_orders WHERE status = 'abandoned';
TX { chr_accounts:14 ?= { account_id: 14, balance: 0.0 } };
```
* `UPDATE`/`DELETE` по предикату: верхний уровень авто-оборачивается в один
  атомарный TX (один fsync); `DELETE` учитывает внешние ключи —
  `ON DELETE CASCADE` каскадит (один уровень, как SQL), `RESTRICT` даёт ошибку.
* `?=` — DnaQL-форма `INSERT ... ON CONFLICT DO NOTHING`: существующий locus
  не перезаписывается; отчёт помечает вставку полем `inserted`.

### Задача 7: DDL и интроспекция (v2.2)
```dnaql
SHOW CHROMOSOMES;
ALTER CHROMOSOME chr_users ADD COLUMN note TEXT;
CREATE INDEX idx_users_note ON chr_users (note);
DROP INDEX idx_users_note ON chr_users;
EXPLAIN FROM chr_users |> WHERE balance > 90.0;
```
(`SHOW CHROMOSOMES` — алиас `SHOW TABLES`; `EXPLAIN` возвращает план вместо строк.)
DDL идёт через общий с SQL реестр схем/индексов, поэтому хромосома,
созданная в DnaQL, видна SQL-клиентам и наоборот.

### Задача 8: Карта хранения локуса: STORAGE_MAP() (v4.1)

Цепочки A/C/G/T — внутреннее представление; человеку их читать не нужно.
`STORAGE_MAP()` возвращает карту хранения одного локуса: хромосому, локус,
точный ключ, тип колонки, значение в человекочитаемом виде и физическое
представление (тег RowCodec, размер). Работает с любой строкой,
последовательность не требуется — это DnaQL-эквивалент SQL-команды
`SHOW STORAGE ... WHERE <pk> = <literal>`.

```dnaql
ANALYZE LOCUS chr_orders:1042 |> STORAGE_MAP();
-- function    chromosome  locus  storage_key       column   sql_type  value    stored_as
-- STORAGE_MAP chr_orders  1042   chr_orders:1042   order_id INT       1042     RowCodec tag 1: i64 little-endian (8 B) = 1042
-- STORAGE_MAP chr_orders  1042   chr_orders:1042   status   TEXT      PENDING  RowCodec tag 3: UTF-8 text, u32 length + 7 B
```

**Веб-панель (v4.1):** кнопка 🔬 у каждой строки браузера таблиц открывает
SQL-вид `SHOW STORAGE` этой строки; её кнопка «🧬 STORAGE_MAP в
DnaQL-консоли» подставляет ровно эту команду `ANALYZE LOCUS <таблица>:<pk>
|> STORAGE_MAP();` в DnaQL-консоль и выполняет её — та же физическая карта
в терминах DnaQL, в один клик из любой консоли.

---

## Глава 4. Шпаргалка Rosetta Stone: SQL vs DnaQL

| Операция | ANSI SQL | Standard DnaQL | Quick DnaQL |
|:---|:---|:---|:---|
| Поиск с мутацией | *нет нативно* | `FIND PATTERN 'ACGT' IN chr WHERE HAMMING_DISTANCE <= 1` | `chr ? 'ACGT' ~1` |
| Реляционный JOIN | `SELECT ... JOIN b ON ...` | `FROM a JOIN b ON x = y` (v2.2) или `FROM a LINK b VIA COMPLEMENT(a_id)` (key-seek) | `FROM a |> JOIN b ON x = y` / `FROM a |> LINK b` |
| Агрегаты/GROUP BY | `COUNT/SUM/AVG/MIN/MAX + GROUP BY` | `FROM a |> GROUP BY g AGG n = COUNT(*)` (v2.2) | — |
| HAVING | `HAVING COUNT(*) > 2` | `|> GROUP ... |> WHERE n > 2` (v2.2) | — |
| OFFSET | `LIMIT 10 OFFSET 5` | `|> OFFSET 5 |> LIMIT 10` (v2.2) | — |
| Вычисляемая проекция | `SELECT x * 2 AS d` | `|> EMIT x * 2 AS d` (v2.2) | — |
| UNION | `SELECT ... UNION [ALL] SELECT ...` | `FROM a |> ... UNION [ALL] FROM b |> ...` (v2.2) | — |
| UPSERT (DO NOTHING) | `INSERT ... ON CONFLICT DO NOTHING` | `TX { chr:k ?= { ... } }` (v2.2) | — |
| UPDATE по предикату | `UPDATE t SET ... WHERE ...` | `UPDATE t SET ... WHERE ...` (v2.2) | — |
| DELETE по предикату | `DELETE FROM t WHERE ...` | `DELETE FROM t WHERE ...` (v2.2) | `TX { DROP LOCUS t:7 }` (по locus) |
| Интроспекция | `SHOW TABLES` | `SHOW CHROMOSOMES` (v2.2) | — |
| Online DDL | `ALTER TABLE t ADD COLUMN c T` | `ALTER CHROMOSOME t ADD COLUMN c T` (v2.2) | — |
| Индексы | `CREATE INDEX i ON t (c)` | `CREATE INDEX i ON t (c)` (v2.2) | — |
| План | `EXPLAIN <query>` | `EXPLAIN FROM a |> ...` (v2.2) | — |
| Транзакция | `BEGIN; UPDATE; INSERT; COMMIT;` | `SYNTHESIZE ... BEGIN ... LIGATE` | `TX { ... }` |
| Обратный комплемент | *внешняя библиотека* | `ANALYZE SEQUENCE s |> REV_COMP()` | `ANALYZE SEQUENCE s |> REV_COMP()` |
| Фильтр по полю | `WHERE x > 1` | `FROM a |> WHERE x > 1` | `FROM a |> WHERE x > 1` |

---

## Глава 5. Транслятор SQL → DnaQL (v2.2)

Встроенный транслятор (`SqlEngine::sql_to_dnaql`) переводит разобранный SQL
в эквивалентный текст DnaQL. Покрытие — полная поверхность SQL-движка:

| SQL | DnaQL |
|:---|:---|
| `SELECT cols FROM t WHERE ... ORDER BY ... LIMIT/OFFSET` | `FROM t \|> WHERE ... \|> SORT ... \|> OFFSET n \|> LIMIT n \|> EMIT ...` |
| `INNER/LEFT/RIGHT/FULL [OUTER] JOIN ... ON ...` | `\|> [LEFT\|RIGHT\|FULL] JOIN t2 ON x = y` |
| `GROUP BY` + агрегаты + `HAVING` | `\|> GROUP BY g AGG имя = FN(col)` + `\|> WHERE` (HAVING) |
| `UNION [ALL] ... ORDER BY/LIMIT` | `... UNION [ALL] ... \|> SORT/LIMIT/OFFSET` |
| `INSERT ... VALUES ...` | `TX { chr:pk += { ... }; ... }` |
| `INSERT ... ON CONFLICT DO NOTHING` | `TX { chr:pk ?= { ... } }` |
| `INSERT ... ON CONFLICT DO UPDATE SET <все non-PK>` | `TX { chr:pk += { ... } }` (точный upsert) |
| `UPDATE ... WHERE pk = N` | `TX { chr:pk.field = v }` (компактная locus-форма) |
| `UPDATE ... WHERE <предикат>` | `UPDATE chr SET ... WHERE <предикат>` |
| `DELETE FROM t WHERE pk = N` / по предикату | `TX { DROP LOCUS t:pk }` / `DELETE FROM t WHERE ...` |
| `CREATE/DROP TABLE`, `ALTER TABLE ADD COLUMN` | `CREATE/DROP CHROMOSOME`, `ALTER CHROMOSOME ADD COLUMN` |
| `CREATE [UNIQUE] INDEX / DROP INDEX` | та же форма в DnaQL |
| `SHOW TABLES` | `SHOW CHROMOSOMES` |
| `EXPLAIN SELECT ...` | `EXPLAIN FROM ...` |

SQL-only остаются (честная ошибка с именем конструкции, без тихой подмены):
сессионные `BEGIN/COMMIT/ROLLBACK` (DnaQL оборачивает записи в `TX {}`),
CTE `WITH`, скалярные/`EXISTS`-подзапросы и оконные функции, а также
`ON CONFLICT DO UPDATE` с частичным списком SET (частичное «оставить старые
значения» не выражается — используйте SQL).

## Глава 6. Ошибки и пограничные случаи

| Ситуация | Поведение |
|---|---|
| Паттерн пуст или длиннее 64 bp | ошибка `pattern length must be 1..=64` |
| Символ вне ACGT в паттерне/последовательности | ошибка `Invalid nucleotide` |
| Незакрытая кавычка | `unterminated string literal` |
| Выражение после WHERE не распознано | ошибка с указанием ожидаемого токена |
| Лишние токены после запроса | `unexpected trailing tokens` (тишины нет) |
| `-= 5` от NULL-поля | `+= requires numeric operands` — сначала создайте строку |
| FK-линк на несуществующую строку | поле `link_missing` в результате (не ошибка) |
| Неизвестная био-функция | `unknown bio function '<имя>'` |
| `DELETE FROM t` при RESTRICT-ссылках | `RESTRICT violation: ... is referenced by ...` |
| `UPDATE t SET неизвестное_поле` | `unknown column ... in UPDATE` |
| `EXPLAIN` записи (`TX { ... }`) | ошибка без исполнения — EXPLAIN только для чтений |

Далее: [DNAQL_SPEC.md](DNAQL_SPEC.md) — формальная грамматика и семантика.
