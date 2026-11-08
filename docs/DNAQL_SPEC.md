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
4. **Полная парность с SQL (v2.2):** каждая SQL-команда движка имеет нативную DnaQL-форму
   (JOIN, GROUP BY + агрегаты, OFFSET, вычисляемые проекции, UNION, SHOW / ALTER / INDEX DDL,
   UPDATE/DELETE по предикату), и `sql_to_dnaql` транслирует SQL ↔ DnaQL без потери семантики.

**Отличие от v2.1-спеки:** добавлены конвейерные стадии `JOIN` / `GROUP` / `OFFSET`,
вычисляемый `EMIT … AS`, компаунд `UNION [ALL]`, `EXPLAIN`, DDL поверх хромосом
(`ALTER CHROMOSOME`, `CREATE/DROP INDEX`, `SHOW CHROMOSOMES`) и оператор `?=`; в
выражения `WHERE` добавлен префиксный `NOT` (раньше заявлялся, но не разбирался).

---

## 2. Formal Grammar (реализованное подмножество)

```ebnf
Query           ::= QuickSearch | StandardSearch | PipelineQuery | TxBlock | BioAnalysis
                  | HealthCheck | ChromosomeDDL | IndexDDL | ShowChromosomes | Explain
                  | RowSetStmt ;

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
                  | "SORT" Identifier ["ASC" | "DESC"]
                  | "LIMIT" IntegerLiteral
                  | "OFFSET" IntegerLiteral                        (* v2.2 *)
                  | "GROUP" ["BY" IdentifierList] "AGG" AggItem ("," AggItem)*   (* v2.2 *)
                  | JoinStage ;                                    (* v2.2 *)

JoinStage       ::= ["INNER"] "JOIN" Identifier "ON" JoinCol "=" JoinCol
                  | ( "LEFT" | "RIGHT" | "FULL" ) ["OUTER"] "JOIN" Identifier
                    "ON" JoinCol "=" JoinCol ;
JoinCol         ::= Identifier [ "." Identifier ] ;   (* квалификатор срезается *)

EmitItem        ::= "*" | Identifier | Expr "AS" Identifier ;      (* v2.2 *)
AggItem         ::= Identifier "=" ("COUNT" "(" [ "*" ] ")"
                  | "SUM" "(" Identifier ")" | "AVG" "(" Identifier ")"
                  | "MIN" "(" Identifier ")" | "MAX" "(" Identifier ")") ;

Union           ::= PipelineQuery "UNION" ["ALL" | "DISTINCT"] PipelineQuery
                    ( "|>" TailStage )* ;                          (* v2.2 *)
TailStage       ::= "SORT" Identifier ["ASC"|"DESC"] | "LIMIT" IntegerLiteral
                  | "OFFSET" IntegerLiteral ;   (* хвост применяется ко всему объединению *)

TxBlock         ::= "TX" "{" TxStmt ( ";" TxStmt )* "}"
                  | "SYNTHESIZE" "INTO" IdentifierList "BEGIN" TxStmtList ("LIGATE" | "HYDROLYZE") ;

TxStmt          ::= Identifier ":" IntegerLiteral "." Identifier ("=" | "+=" | "-=") Literal
                  | Identifier ":" IntegerLiteral ("+=" | "?=") "{" KVList "}"
                  | Identifier ("+=" | "?=") "{" KVList "}"     (* авто-locus из *_id поля *)
                  | "UPDATE" Identifier "SET" Identifier "=" Literal ("," ...)* ["WHERE" Expr]
                  | "DELETE" "FROM" Identifier ["WHERE" Expr]
                  | "DROP" "LOCUS" Identifier ":" IntegerLiteral ["CASCADE"] ;

RowSetStmt      ::= "UPDATE" Identifier "SET" Identifier "=" Literal ("," ...)* ["WHERE" Expr] ";"
                  | "DELETE" "FROM" Identifier ["WHERE" Expr] ";" ;   (* авто-обёртка в TX *)

ChromosomeDDL   ::= "CREATE" "CHROMOSOME" Identifier ["IF NOT EXISTS"] "(" DefList ")" ";"
                  | "DROP" "CHROMOSOME" Identifier ["IF EXISTS"] ";"
                  | "ALTER" "CHROMOSOME" Identifier AlterAction ";" ;
AlterAction     ::= "ADD" "COLUMN" Identifier Type
                  | "DROP" "COLUMN" Identifier                    (* v4.2 *)
                  | "RENAME" "COLUMN" Identifier "TO" Identifier  (* v4.2 *)
                  | "MODIFY" "COLUMN" Identifier Type             (* v4.2 *)
                  | "CHANGE" "COLUMN" Identifier Identifier Type ;(* v4.2 *)

IndexDDL        ::= "CREATE" ["UNIQUE"] "INDEX" ["IF NOT EXISTS"] Identifier
                    "ON" Identifier "(" Identifier ")" ";"
                  | "DROP" "INDEX" ["IF EXISTS"] Identifier ["ON" Identifier] ";" ;

ShowChromosomes ::= "SHOW" ("CHROMOSOMES" | "TABLES") ";" ;

Explain         ::= "EXPLAIN" ( PipelineQuery | StandardSearch ) ";" ;   (* read-only *)

BioAnalysis     ::= "ANALYZE" ("SEQUENCE" StringLiteral | "LOCUS" Identifier ":" IntegerLiteral)
                    ( "|>" BioFunction )* ;
BioFunction     ::= "GC_CONTENT()" | "MELTING_TEMP()" | "REV_COMP()"
                  | "TRANSCRIBE()" | "TRANSLATE()" ;

Expr            ::= OrExpr ; OrExpr ::= AndExpr ("OR" AndExpr)* ;
AndExpr         ::= NotExpr ("AND" NotExpr)* ;
NotExpr         ::= "NOT" NotExpr | CmpExpr ;                     (* NOT — v2.2 *)
CmpExpr         ::= Operand [("=" | "!=" | "<>" | "<" | "<=" | ">" | ">=") Operand]
                  | Operand "IS" ["NOT"] "NULL"
                  | Identifier "IN" "(" LiteralList ")" ;
Operand         ::= Literal | Identifier | Operand ("+" | "-" | "*" | "/") Operand
                  | "(" OrExpr ")" ;
Literal         ::= IntegerLiteral | FloatLiteral | StringLiteral | "NULL" ;
ProjectionList  ::= "*" | Identifier ("," Identifier)* ;
```

Лексер: строки `'...'` или `"..."`; числа int/float; идентификаторы `[A-Za-z_][A-Za-z0-9_]*`;
операторы `? ?= ~ -> |> => += -= = != <> < <= > >= : ; , . ( ) { } * + - /`.
Строка, состоящая только из A/C/G/T (в любом регистре), типизируется как `Value::Dna`.
После запроса допускается только `;`/EOF — лишние токены дают ошибку разбора
(«unexpected trailing tokens»), тишина невозможна.

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

### 4.3. SQL-парные конвейерные стадии (v2.2)

**JOIN.** `|> [INNER|LEFT|RIGHT|FULL [OUTER]] JOIN chr ON l = r` — hash
equi-join с семантикой, побайтно повторяющей SQL-движок:
* строки — единое пространство имён; конфликтующие поля правой стороны
  получают префикс `joined_` (например `joined_user_id`);
* NULL-ключи не совпадают никогда (трёхзначная логика);
* LEFT продлевает непарные левые строки NULL-полями правых колонок;
  RIGHT — то же со сменой сторон; FULL добавляет непарные правые строки,
  продлённые NULL-полями левых колонок.

**GROUP.** `|> GROUP [BY cols] AGG name = FN(col), ...` — агрегация,
зеркалящая SQL: `COUNT(*)` считает все строки группы, `COUNT(col)`/`SUM`/`AVG`/
`MIN`/`MAX` пропускают NULL; `SUM` по целым остаётся `Int`, при наличии Float
даёт `Float`; `AVG` всегда `Float`; `SUM` по группе из одних NULL = NULL;
агрегация без GROUP BY по пустому входу даёт одну строку (COUNT = 0).
Группа без AGG проецирует только ключи группировки (семантика DISTINCT).
`HAVING` записывается обычным `|> WHERE` **после** стадии GROUP — агрегаты
после группировки являются обычными полями.

**OFFSET / LIMIT.** Применяются в порядке стадий; SQL-порядок `OFFSET → LIMIT`
даёт `|> OFFSET n |> LIMIT m`.

**EMIT.** `|> EMIT * | col | expr AS alias` — вычисляемые проекции
(`balance * 2 AS double`) и алиасы, как в SQL `SELECT expr AS name`.

**UNION.** `FROM a |> … UNION [ALL] FROM b |> … |> SORT/LIMIT/OFFSET` —
имена колонок берутся от левой стороны, `DISTINCT` (по умолчанию) дедуплицирует
полные строки, хвостовые SORT/LIMIT/OFFSET применяются к объединению
(та же семантика, что у SQL `UNION … ORDER BY … LIMIT`).

### 4.4. Транзакции (`TX { ... }` / `SYNTHESIZE ... LIGATE`)
Все инструкции буферизуются в write-set сессии и коммитятся **одной**
batch-записью WAL: один CRC, один fsync, один атомарный интервал
durability. Крэш между инструкциями оставляет лог без недоконченной
транзакции. Изоляция: Serializable-валидация первых касаний.

Внутри блока и на верхнем уровне доступны строчные операции (v2.2):
* `UPDATE chr SET f = v, ... [WHERE expr]` — обновление по предикату
  (без WHERE — все строки), литеральные присваивания; верхнеуровневая форма
  авто-оборачивается в атомарный `TX`;
* `DELETE FROM chr [WHERE expr]` — удаление по предикату с внешними ключами:
  `ON DELETE CASCADE` удаляет ссылающиеся дочерние строки (один уровень, как в SQL),
  `RESTRICT` даёт честную ошибку при наличии ссылающихся строк;
* `chr:locus ?= { k: v, ... }` — вставка **только если** locus отсутствует
  (DnaQL-форма SQL `INSERT ... ON CONFLICT DO NOTHING`); существующая строка
  не трогается, отчёт помечает результат полем `inserted`.

### 4.5. Био-функции (реальные вычисления)
- `GC_CONTENT()` — доля G+C, формат `44.44%`.
- `MELTING_TEMP()` — Wallace rule (<14 bp) или GC-формула.
- `REV_COMP()` — побитовый reverse complement (без ASCII round-trip).
- `TRANSCRIBE()` — T→U.
- `TRANSLATE()` — стандартная кодонная таблица до первого стоп-кодона.

### 4.6. DDL и интроспекция (v2.2)
- `CREATE CHROMOSOME` / `DROP CHROMOSOME` — как в v2.1, через реестр схем SQL-движка.
- `ALTER CHROMOSOME t ADD COLUMN c TYPE` — мгновенная metadata-only DDL (v3.9).
- `ALTER CHROMOSOME t DROP|RENAME|MODIFY|CHANGE COLUMN ...` (v4.2) —
  полный цикл колонки: DROP перезаписывает строки одним атомарным батчем
  (колонка не «воскресает»), RENAME/MODIFY/CHANGE конвертируют значения
  и перестраивают индексы; рендерятся через SQL-движок, семантика
  идентична `ALTER TABLE` (см. SQL_MANUAL_RU §2.3/§3.5).
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON chr (col)` /
  `DROP INDEX [IF EXISTS] name [ON chr]` — реестр индексов общий с SQL.
- `SHOW CHROMOSOMES` (алиас `SHOW TABLES`) — строки вида `chromosome`.
- `EXPLAIN FROM … | FIND …` — выполняет read-only запрос и возвращает план
  (поле `engine_note`), строки отбрасываются; `EXPLAIN` записи — ошибка без исполнения.
