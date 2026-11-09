# Formal Specification: DnaQL (DNA Query Language) v2.2
**Document Version:** 2.2.0-PROD
**Status:** Approved Standard — реализованное подмножество
**Maintainer:** DNA-DB Architecture Board

---

## 1. Scope & Design Philosophy

DnaQL — предметно-ориентированный язык поверх 2-битной упаковки нуклеотидов: ДНК-представление данных, нечёткий поиск последовательностей и био-реляционные операции.

Ключевые принципы:
1. **Zero-Impedance Bio-Relational Model:** таблицы = *Chromosomes*, строки = *Loci*, FK = комплементарные *Base-Pairing* указатели.
2. **Dual Syntax:** Standard DnaQL (формальный) и Quick DnaQL (`?`, `~N`, `->`, `|>`, `=>`).
3. **Register-Level Alignment:** bit-parallel Myers/Wu-Manber на 64-битных словах (см. §4.1).

**Отличие от v2.0-спеки:** в этом документе описано только то, что реально
реализовано и покрыто тестами. Заявления v2.0 про AVX-512, «256 bp/cycle» и
распределённый 2PC удалены как неподтверждённые (см. ROADMAP).

**v2.2 (2026-11): SQL-конформность пайплайна.** Добавлены стадии `SKIP`,
`[LEFT] JOIN`, `GROUP` (агрегаты + HAVING), выражения в `EMIT` и `SORT`,
`NOT`/`LIKE`/`CAST` в выражениях, `SHOW CHROMOSOMES` и DDL-passthrough
(`CREATE/DROP INDEX`, `ALTER CHROMOSOME`) — трансляция SQL→DnaQL покрывает
весь SQL-словарь движка, кроме явно перечисленных исключений (§2.1).

---

## 2. Formal Grammar (реализованное подмножество)

```ebnf
Query           ::= QuickSearch | StandardSearch | PipelineQuery | TxBlock | BioAnalysis
                  | ShowChromosomes | DdlPassthrough ;

QuickSearch     ::= Identifier "?" StringLiteral
                    [("~" | "*") IntegerLiteral]
                    ["->" Identifier ["(" Identifier ")"]]
                    ["=>" "emit" ProjectionList] [";"] ;

StandardSearch  ::= "FIND" ["PATTERN"] StringLiteral
                    "IN" ["CHROMOSOME"] Identifier
                    ["WHERE" ("HAMMING_DISTANCE" | "LEVENSHTEIN_DISTANCE") "<=" IntegerLiteral]
                    ["LINK" Identifier "VIA" "COMPLEMENT" "(" Identifier ")"]
                    ["EMIT" ProjectionList] [";"] ;

PipelineQuery   ::= "FROM" Identifier ( "|>" PipelineStage )* [";"] ;
PipelineStage   ::= "LINK" Identifier [ "VIA" "COMPLEMENT" "(" Identifier ")" ]
                  | "WHERE" Expr
                  | "EMIT" EmitList
                  | "SORT" ( Identifier | Expr ) ["ASC" | "DESC"]
                  | "LIMIT" IntegerLiteral
                  | "SKIP" IntegerLiteral
                  | "JOIN" Identifier "ON" Operand "=" Operand ( "AND" Expr )*
                  | "LEFT" ["OUTER"] "JOIN" Identifier "ON" Operand "=" Operand ( "AND" Expr )*
                  | "GROUP" ["BY"] GroupItem ( "," GroupItem )* ["HAVING" Expr] ;

GroupItem       ::= Identifier | AggCall [ "AS" Identifier ] ;
AggCall         ::= ("COUNT" "(" [ "*" | Expr ] ")" ) | ("SUM"|"AVG"|"MIN"|"MAX" "(" Expr ")") ;

EmitList        ::= "*" | EmitItem ( "," EmitItem )* ;
EmitItem        ::= Identifier [ "AS" Identifier ] | Expr [ "AS" Identifier ] ;

ShowChromosomes ::= "SHOW" "CHROMOSOMES" [ "LIKE" StringLiteral ] [";"] ;
DdlPassthrough  ::= "CREATE" ["UNIQUE"] "INDEX" ["IF" "NOT" "EXISTS"] Identifier
                    "ON" Identifier "(" Identifier ")" [";"]
                  | "DROP" "INDEX" ["IF" "EXISTS"] Identifier [ "ON" Identifier ] [";"]
                  | "ALTER" ("CHROMOSOME"|"TABLE") Identifier "ADD" ["COLUMN"] Identifier
                    RawTypeFlagsTail [";"]
                  | "ALTER" ("CHROMOSOME"|"TABLE") Identifier "DROP" ["COLUMN"] Identifier [";"] ;

TxBlock         ::= "TX" "{" TxStmt ( ";" TxStmt )* "}"
                  | "SYNTHESIZE" "INTO" IdentifierList "BEGIN" TxStmtList ("LIGATE" | "HYDROLYZE") ;

TxStmt          ::= Identifier ":" IntegerLiteral "." Identifier ("=" | "+=" | "-=") Literal
                  | Identifier ":" IntegerLiteral "+=" "{" KVList "}"
                  | Identifier "+=" "{" KVList "}"          (* авто-locus из *_id поля *)
                  | "DROP" "LOCUS" Identifier ":" IntegerLiteral ["CASCADE"] ;

BioAnalysis     ::= "ANALYZE" ("SEQUENCE" StringLiteral | "LOCUS" Identifier ":" IntegerLiteral)
                    ( "|>" BioFunction )* ;
BioFunction     ::= "GC_CONTENT()" | "MELTING_TEMP()" | "REV_COMP()"
                  | "TRANSCRIBE()" | "TRANSLATE()" ;

Expr            ::= OrExpr ; OrExpr ::= AndExpr ("OR" AndExpr)* ;
AndExpr         ::= CmpExpr ("AND" CmpExpr)* ;
CmpExpr         ::= ["NOT"] CmpExpr
                  | Operand [("=" | "!=" | "<>" | "<" | "<=" | ">" | ">=") Operand]
                  | Operand ["NOT"] "LIKE" StringLiteral
                  | Operand ["NOT"] "IN" "(" ValueList ")"
                  | Operand "IS" ["NOT"] "NULL" ;
Operand         ::= Literal | Identifier | AggRef | CastCall | "(" OrExpr ")" ;
AggRef          ::= ("COUNT" "(" "*" ")" ) | ("SUM"|"AVG"|"MIN"|"MAX" "(" Expr ")") ;
CastCall        ::= "CAST" "(" Expr "AS" ("SIGNED"|"UNSIGNED"|"CHAR") ")" ;
Operand         ::= Operand (("+"|"-"|"*"|"/") Operand)* ;   (* приоритет как в SQL *)
Literal         ::= IntegerLiteral | FloatLiteral | StringLiteral | "NULL" ;
```

Лексер: строки `'...'` или `"..."`; числа int/float; идентификаторы `[A-Za-z_][A-Za-z0-9_]*`;
операторы `? ~ -> |> => += -= = != <> < <= > >= : ; , . ( ) { } *`.
Строка, состоящая только из A/C/G/T (в любом регистре), типизируется как `Value::Dna`.
Квалифицированная ссылка `chr.field` (после JOIN/LINK) — `Identifier "." Identifier`.

### 2.1. SQL-conformance exceptions (полный список)

Заявления SQL, не выражаемые в DnaQL v2.2 (транслятор SQL→DnaQL отвечает
явной ошибкой с подсказкой, а не молчаливым упрощением):

| SQL-конструкция | Причина / обход |
|---|---|
| подзапросы (`IN (SELECT …)`, scalar, `EXISTS`) | материализовать внутренний SELECT отдельно |
| оконные функции (`OVER …`) | только SQL |
| `RIGHT`/`FULL` JOIN | переписать как `LEFT JOIN` со сменой сторон (SQL-движок исполняет их именно так) |
| `DISTINCT`-агрегаты | только SQL |
| UPSERT-хвосты (`ON CONFLICT`, `ON DUPLICATE KEY UPDATE`, `INSERT IGNORE`) | только SQL (TX-грамматика не имеет upsert) |
| `WITH` (CTE), `UNION` | материализовать каждый SELECT отдельно |
| `EXPLAIN`, `SHOW STORAGE`, `BEGIN/COMMIT/ROLLBACK` | SQL-only; план DnaQL отдаётся в execution note каждого результата |

Всё остальное — проекции (включая вычислимые и алиасы), WHERE/NOT/LIKE/IN,
INNER/LEFT JOIN, GROUP BY + агрегаты + HAVING, ORDER BY (в т.ч. по
выражению), OFFSET/LIMIT, DDL (таблицы, индексы, колонки) и SHOW —
транслируется 1:1 и проверяется семантическим parity-тестом.

---

## 3. Data Type System

| Type | Representation | Footprint | Domain |
|:---|:---|:---|:---|
| `NUCLEOTIDE` | 2-bit (`A=00`, `C=01`, `G=10`, `T=11`) | 0.25 байта | одно основание |
| `SEQUENCE` | Packed 2-bit + явная длина | (L + 3) / 4 байта | контриги, гены, риды |
| `LOCUS` | Целочисленный ключ строки | ключ LSM | координата/ID |
| `CHROMOSOME` | Пространство ключей `chr_name:*` | prefix | LSM-партиция |
| `COMPLEMENT_LINK` | Прямой key-seek по FK | O(1) | «нулевой» foreign key |
| `Value` | Null / Int(i64) / Float(f64) / Text / Dna | бинарный RowCodec | поля строк |

*IUPAC 'N' не кодируется в 2 битах: на кодировании — ошибка, намеренно строгое поведение.*

---

## 4. Execution Semantics

### 4.1. Pattern Matching Engine (Bitap / Myers)
`FIND PATTERN P WHERE HAMMING_DISTANCE <= K` и `chr ? P ~K` исполняются
bit-parallel алгоритмом Wu-Manber на `u64` состояниях:

1. `Peq[c]` — маска позиций символа c в паттерне (1 = совпадение).
2. Состояния `R[d]`, бит i = 1 ⇔ prefix[0..=i] выравнивается на подстроку,
   заканчивающуюся в текущей позиции, с ≤ d правками. Инициализация
   `R[d] = (1<<d)-1` (делеции в начале), маскирование по ширине паттерна.
3. Тестируется только верхний уровень; **истинная дистанция** = минимальный
   уровень d с установленным битом (m-1).
4. Ограничение: длина паттерна 1..=64 (ширина бит-вектора). Для m > 64 —
   явная ошибка (chunked search — ROADMAP).
5. Результат содержит кратчайшее окно-выравнивание; гарантируется
   `edit_distance(pattern, matched_sequence) == mismatches`.

Корректность доказана property-тестами: 8000 случайных кейсов против
наивного semi-global DP (Levenshtein) и brute-force окон (Hamming).

### 4.2. Genetic Linkage Navigation (`LINK ... VIA COMPLEMENT`)
```dnaql
FROM chr_a |> LINK chr_b VIA COMPLEMENT(b_id) |> EMIT *;
```
Исполнение: значение поля `b_id` строки источника формирует ключ
`chr_b:<value>` и читается **прямым seek** (bloom + sparse index в сегментах,
skiplist в memtable). Hash join не строится. Коллизии имён полей:
сначала голое имя, затем `chr_b.<поле>`, затем `link_<поле>`.

### 4.2.1. Стадия JOIN (v2.2)
`FROM a |> [LEFT] JOIN b ON l = r [AND extra]*` — вложенный цикл с одним
сканом правой хромосомы; пара `l`/`r` вычисляется по левому/правому ряду
соответственно, NULL-значения не совпадают (SQL-семантика). Поля правой
стороны попадают в ряд по правилу имён §4.2: сначала голое имя, затем
`b.<поле>` (при коллизии), затем `link_<поле>`. Конъюнкты `AND extra`
вычисляются по слитому ряду ВО ВРЕМЯ сопоставления — при LEFT JOIN
отфильтрованные пары не «воскресают» как NULL-ряды. Не нашедшие пары ряды:
INNER — отбрасываются, LEFT — сохраняются без полей правой стороны
(квалифицированные ссылки читаются как NULL).

### 4.2.2. Стадия GROUP (v2.2)
`|> GROUP [BY] col, …, AGG(expr) [AS alias], … [HAVING cond]` — один ряд на
группу; групповые колонки сохраняют значение, агрегаты вычисляются с
пропуском NULL (SQL-семантика) и кладутся под каноническое имя
(`COUNT(*)`, `SUM(balance + 1)`) И под алиас — HAVING/ORDER BY резолвят
любую из форм, как это делает SQL-исполнитель. SUM/AVG приводят числовые
строки (MySQL-правило), MIN/MAX работают на любом сравнимом типе, пустая
группа агрегата — NULL. `|> SKIP n` пропускает первые n рядов (SQL OFFSET).

### 4.3. Транзакции (`TX { ... }` / `SYNTHESIZE ... LIGATE`)
Все инструкции буферизуются в write-set сессии и коммитятся **одной**
batch-записью WAL: один CRC, один fsync, один атомарный интервал
durability. Крэш между инструкциями оставляет лог без недоконченной
транзакции. Изоляция: Read Committed (MVCC — ROADMAP).

### 4.4. Био-функции (реальные вычисления)
- `GC_CONTENT()` — доля G+C, формат `44.44%`.
- `MELTING_TEMP()` — Wallace rule (<14 bp) или GC-формула.
- `REV_COMP()` — побитовый reverse complement (без ASCII round-trip).
- `TRANSCRIBE()` — T→U.
- `TRANSLATE()` — стандартная кодонная таблица до первого стоп-кодона.
