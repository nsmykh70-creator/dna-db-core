# Formal Specification: DnaQL (DNA Query Language) v2.1
**Document Version:** 2.1.0-PROD
**Status:** Approved Standard — реализованное подмножество
**Maintainer:** DNA-DB Architecture Board

---

## 1. Scope & Design Philosophy

DnaQL — предметно-ориентированный язык для геномных и био-реляционных нагрузок поверх 2-битной упаковки нуклеотидов.

Ключевые принципы:
1. **Zero-Impedance Bio-Relational Model:** таблицы = *Chromosomes*, строки = *Loci*, FK = комплементарные *Base-Pairing* указатели.
2. **Dual Syntax:** Standard DnaQL (формальный) и Quick DnaQL (`?`, `~N`, `->`, `|>`, `=>`).
3. **Register-Level Alignment:** bit-parallel Myers/Wu-Manber на 64-битных словах (см. §4.1).

**Отличие от v2.0-спеки:** в этом документе описано только то, что реально
реализовано и покрыто тестами. Заявления v2.0 про AVX-512, «256 bp/cycle» и
распределённый 2PC удалены как неподтверждённые (см. ROADMAP).

---

## 2. Formal Grammar (реализованное подмножество)

```ebnf
Query           ::= QuickSearch | StandardSearch | PipelineQuery | TxBlock | BioAnalysis ;

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
                  | "EMIT" ProjectionList ;

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
CmpExpr         ::= Operand [("=" | "!=" | "<>" | "<" | "<=" | ">" | ">=") Operand] | Operand "IS" "NULL" ;
Operand         ::= Literal | Identifier | "(" OrExpr ")" ;
Literal         ::= IntegerLiteral | FloatLiteral | StringLiteral | "NULL" ;
ProjectionList  ::= "*" | Identifier ("," Identifier)* ;
```

Лексер: строки `'...'` или `"..."`; числа int/float; идентификаторы `[A-Za-z_][A-Za-z0-9_]*`;
операторы `? ~ -> |> => += -= = != <> < <= > >= : ; , . ( ) { } *`.
Строка, состоящая только из A/C/G/T (в любом регистре), типизируется как `Value::Dna`.

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
