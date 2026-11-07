// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL (DNA Query Language) v2.2 — REAL tokenizer, parser and executor.
//!
//! Implemented grammar (subset of DNAQL_SPEC.md, fully functional):
//!
//!   Query      ::= PatternSearch | PipelineQuery | TxBlock | BioAnalysis
//!               |  HealthCheck | CreateChromosome | DropChromosome
//!               |  AlterChromosome | CreateIndex | DropIndex
//!               |  ShowChromosomes | Explain
//!   QuickSearch::= Ident '?' StringLit ['~' Int | '*' Int]
//!                  ['->' Ident ['(' Ident ')']] ['=>' 'emit' Projection] [';']
//!   StdSearch  ::= 'FIND' ['PATTERN'] StringLit 'IN' ['CHROMOSOME'] Ident
//!                  ['WHERE' ('HAMMING_DISTANCE'|'LEVENSHTEIN_DISTANCE') '<=' Int]
//!                  ['LINK' Ident 'VIA' 'COMPLEMENT' '(' Ident ')']
//!                  ['EMIT' Projection] [';']
//!   Pipeline   ::= 'FROM' Ident ( '|>' Stage )* ['UNION' ['ALL'] Pipeline Tail]*
//!   Stage      ::= 'LINK' Ident ['VIA' 'COMPLEMENT' '(' Ident ')']
//!               |  'WHERE' Expr
//!               |  'EMIT' EmitList
//!               |  'SORT' Ident ['ASC'|'DESC']
//!               |  'LIMIT' Int | 'OFFSET' Int
//!               |  'GROUP' ['BY' IdentList] 'AGG' Name '=' Fn '(' [Ident] ')' (',' ...)*
//!               |  Join
//!   Join       ::= ['INNER'] JOIN Ident 'ON' Ident '=' Ident
//!               |  (LEFT|RIGHT|FULL) ['OUTER'] JOIN Ident 'ON' Ident '=' Ident
//!   TxBlock    ::= 'TX' '{' Stmt (';' Stmt)* '}'
//!               |  'SYNTHESIZE' 'INTO' IdentList 'BEGIN' StmtList ('LIGATE'|'HYDROLYZE')
//!   Stmt       ::= Ident ':' Int '.' Ident ('='|'+='|'-=') Literal
//!               |  Ident ':' Int ('+='|'?=') '{' KVList '}'
//!               |  Ident '?=' '{' KVList '}'        (insert-if-absent)
//!               |  'DROP' 'LOCUS' Ident ':' Int ['CASCADE']
//!   DDL        ::= 'CREATE' ['UNIQUE'] 'INDEX' Ident 'ON' Ident '(' Ident ')'
//!               |  'DROP' 'INDEX' Ident ['ON' Ident]
//!               |  'ALTER' 'CHROMOSOME' Ident 'ADD' 'COLUMN' Ident Type
//!               |  'SHOW' ('CHROMOSOMES'|'TABLES')
//!   Explain    ::= 'EXPLAIN' (Pipeline|PatternSearch)
//!
//! The v2.2 extension brings DnaQL to full surface parity with the SQL
//! engine (JOIN, GROUP BY + aggregates, OFFSET, computed EMIT, UNION,
//! SHOW / ALTER / CREATE INDEX / DROP INDEX), so every SQL statement has a
//! native DnaQL spelling and `sql_to_dnaql` can translate losslessly.
//! Join and aggregate semantics mirror sql_engine exactly (hash join with
//! `joined_` conflict renaming, NULL-extension for outer joins, aggregates
//! skipping NULLs).
//!
//! Execution reads/writes the REAL Engine storage:
//!   rows: '<table>:<pk>' -> RowCodec payloads (field 'sequence' = Value::Dna)

use crate::dna::PackedDna;
use crate::engine::Engine;
use crate::expr::{self, ArithOp, CmpOp, Expr};
use crate::relational::{RowCodec, Value};
use crate::simd_bitap::BitapMatcher;
use crate::sql_engine::SqlEngine;
use crate::tx::TransactionCoordinator;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum DistanceMode {
    Hamming,
    Levenshtein,
}

#[derive(Debug, Clone)]
pub struct LinkSpec {
    pub chromosome: String,
    pub column: String,
}

#[derive(Debug, Clone)]
pub enum DnaQlAst {
    PatternSearch {
        chromosome: String,
        pattern: String,
        mode: DistanceMode,
        max_distance: usize,
        link: Option<LinkSpec>,
        projection: Vec<String>,
    },
    PipelineQuery {
        source: String,
        stages: Vec<PipelineStage>,
    },
    Tx {
        statements: Vec<TxStmt>,
    },
    BioAnalysis {
        sequence: Option<String>,
        locus: Option<(String, i64)>,
        functions: Vec<String>,
    },
    /// VERIFY / REPAIR CHROMOSOME — segment integrity check (v3.0)
    HealthCheck {
        chromosome: String,
        fix: bool,
    },
    /// CREATE CHROMOSOME name (col TYPE, ..., PRIMARY KEY (...),
    /// FOREIGN KEY (...) REFERENCES ...) — schema DDL. The parenthesis body
    /// is kept as raw definition clauses and handed verbatim to the SQL
    /// engine, so DnaQL DDL and SQL CREATE TABLE accept exactly the same
    /// grammar (types, PRIMARY KEY, FOREIGN KEY, NOT NULL, lengths).
    CreateChromosome {
        table: String,
        definitions: Vec<String>,
        if_not_exists: bool,
    },
    /// DROP CHROMOSOME name [IF EXISTS]
    DropChromosome {
        table: String,
        if_exists: bool,
    },
    /// `ALTER CHROMOSOME name ADD COLUMN col TYPE` (v2.2) — rendered through
    /// the SQL engine's metadata-only online DDL.
    AlterChromosome {
        table: String,
        column: String,
        /// Raw type spelling (INT, FLOAT, TEXT, DNA, TIMESTAMP, ...).
        col_type: String,
    },
    /// `CREATE [UNIQUE] INDEX name ON chromosome (col)` (v2.2) — rendered
    /// through the SQL engine so both dialects share one index registry.
    CreateIndex {
        name: String,
        table: String,
        column: String,
        unique: bool,
        if_not_exists: bool,
    },
    /// `DROP INDEX name [ON chromosome]` (v2.2).
    DropIndex {
        name: String,
        table: Option<String>,
        if_exists: bool,
    },
    /// `SHOW CHROMOSOMES` (alias `SHOW TABLES`) — table introspection.
    ShowChromosomes,
    /// `EXPLAIN <read-only query>` (v2.2): executes the query (reads are
    /// side-effect free) and reports the plan instead of the rows. Write
    /// queries are rejected without executing.
    Explain(Box<DnaQlAst>),
    /// `Pipeline UNION [ALL] Pipeline [SORT/LIMIT/OFFSET tail]` (v2.2).
    /// Mirrors SQL: column names come from the left side, DISTINCT (default)
    /// dedups full rows, tail stages apply to the compound result.
    Union {
        left: Box<DnaQlAst>,
        right: Box<DnaQlAst>,
        all: bool,
        tail: Vec<PipelineStage>,
    },
}

#[derive(Debug, Clone)]
pub enum PipelineStage {
    Link(Option<LinkSpec>),
    Filter(Expr),
    Emit(Vec<EmitItem>),
    /// `|> SORT column [ASC|DESC]` — order the rows (was SQL-only before).
    Sort {
        column: String,
        asc: bool,
    },
    /// `|> LIMIT n` — keep the first n rows after the previous stage.
    Limit(usize),
    /// `|> OFFSET n` (v2.2) — skip the first n rows (SQL OFFSET semantics;
    /// applies before LIMIT).
    Offset(usize),
    /// `|> JOIN / LEFT / RIGHT / FULL [OUTER] JOIN t ON l = r` (v2.2) —
    /// hash equi-join mirroring the SQL engine (single-namespace rows,
    /// `joined_` conflict renaming, NULL-extension for outer sides).
    Join(JoinSpec),
    /// `|> GROUP [BY cols] AGG name = FN(col), ...` (v2.2) — aggregation
    /// mirroring the SQL engine (NULLs skipped, Int sum stays Int, AVG is
    /// Float, empty input + no GROUP BY yields one row). Without AGG the
    /// stage projects only the group columns (DISTINCT semantics).
    Group {
        by: Vec<String>,
        aggs: Vec<(String, AggFunc, Option<String>)>,
    },
}

/// SQL surface join kinds (v2.2). DnaQL spells INNER as bare `JOIN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// One equi-join stage: `JOIN <chromosome> ON <left_col> = <right_col>`.
/// `left_col` addresses the accumulated stream, `right_col` the joined
/// chromosome (bare names — DnaQL rows are single-namespace).
#[derive(Debug, Clone)]
pub struct JoinSpec {
    pub kind: JoinKind,
    pub chromosome: String,
    pub left_col: String,
    pub right_col: String,
}

/// Aggregate functions available in `|> GROUP ... AGG name = FN(col)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn as_str(&self) -> &'static str {
        match self {
            AggFunc::Count => "COUNT",
            AggFunc::Sum => "SUM",
            AggFunc::Avg => "AVG",
            AggFunc::Min => "MIN",
            AggFunc::Max => "MAX",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggFunc::Count),
            "SUM" => Some(AggFunc::Sum),
            "AVG" => Some(AggFunc::Avg),
            "MIN" => Some(AggFunc::Min),
            "MAX" => Some(AggFunc::Max),
            _ => None,
        }
    }
}

/// One EMIT item (v2.2): a bare column, `*`, or a computed expression with
/// an optional `AS alias` (`|> EMIT total_usd * 2 AS double`).
#[derive(Debug, Clone)]
pub enum EmitItem {
    Star,
    Col(String),
    Computed { expr: Expr, alias: String },
}

#[derive(Debug, Clone)]
pub enum TxStmt {
    /// chr:locus.field = literal | += literal | -= literal
    Mutate {
        table: String,
        locus: i64,
        field: String,
        op: AssignOp,
        value: Value,
    },
    /// chr:locus += { k: v, ... }
    Append {
        table: String,
        locus: i64,
        fields: Vec<(String, Value)>,
    },
    /// chr:locus ?= { k: v, ... } (v2.2) — insert ONLY when the locus is
    /// absent; an existing locus is left untouched. This is the DnaQL
    /// spelling of SQL `INSERT ... ON CONFLICT DO NOTHING`.
    InsertIfAbsent {
        table: String,
        locus: i64,
        fields: Vec<(String, Value)>,
    },
    /// DROP LOCUS chr:locus [CASCADE]
    DropLocus {
        table: String,
        locus: i64,
        cascade: bool,
    },
    /// `UPDATE chr SET f = v, ... [WHERE expr]` (v2.2) — row-set update
    /// mirroring SQL UPDATE (no WHERE = every row). Literal assignments only
    /// (same as the SQL engine's UPDATE grammar).
    UpdateRows {
        table: String,
        assignments: Vec<(String, Value)>,
        where_clause: Option<Expr>,
    },
    /// `DELETE FROM chr [WHERE expr]` (v2.2) — row-set delete mirroring SQL
    /// DELETE, including one-level FK CASCADE and honest RESTRICT errors.
    DeleteRows {
        table: String,
        where_clause: Option<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
}

#[derive(Debug, Clone)]
pub struct DnaQlRow {
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct DnaQlResult {
    pub ast_summary: String,
    pub rows: Vec<DnaQlRow>,
    pub execution_time_us: u64,
    pub engine_note: String,
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Question,  // ?
    QuestionEq, // ?= (insert-if-absent)
    Tilde,     // ~
    Arrow,     // ->
    Pipe,      // |>
    FatArrow,  // =>
    PlusEq,    // +=
    MinusEq,   // -=
    Eq,        // =
    Ne,        // != or <>
    Le,        // <=
    Ge,        // >=
    Lt,        // <
    Gt,        // >
    Colon,     // :
    Semicolon, // ;
    Comma,     // ,
    Dot,       // .
    LParen,
    RParen,
    LBrace,
    RBrace,
    Star,
    Plus,  // +
    Minus, // -
    Slash, // /
    Eof,
}

fn tokenize(input: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let b: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '-' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Arrow);
                i += 2;
            }
            '|' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Pipe);
                i += 2;
            }
            '=' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::FatArrow);
                i += 2;
            }
            '+' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::PlusEq);
                i += 2;
            }
            '-' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::MinusEq);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Le);
                i += 2;
            }
            '>' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Ge);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '!' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '?' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::QuestionEq);
                i += 2;
            }
            '?' => {
                toks.push(Tok::Question);
                i += 1;
            }
            '~' => {
                toks.push(Tok::Tilde);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '=' => {
                toks.push(Tok::Eq);
                i += 1;
            }
            '<' => {
                toks.push(Tok::Lt);
                i += 1;
            }
            '>' => {
                toks.push(Tok::Gt);
                i += 1;
            }
            ':' => {
                toks.push(Tok::Colon);
                i += 1;
            }
            ';' => {
                toks.push(Tok::Semicolon);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                toks.push(Tok::Dot);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            '{' => {
                toks.push(Tok::LBrace);
                i += 1;
            }
            '}' => {
                toks.push(Tok::RBrace);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == quote {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                if !closed {
                    return Err("DnaQL: unterminated string literal".into());
                }
                toks.push(Tok::Str(s));
            }
            '0'..='9' => {
                let mut num = String::new();
                let mut is_float = false;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        if is_float {
                            return Err("DnaQL: malformed number".into());
                        }
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(Tok::Float(num.parse().map_err(|_| "DnaQL: bad float")?));
                } else {
                    toks.push(Tok::Int(num.parse().map_err(|_| "DnaQL: bad int")?));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                    s.push(b[i]);
                    i += 1;
                }
                toks.push(Tok::Ident(s));
            }
            other => return Err(format!("DnaQL: unexpected character '{}'", other)),
        }
    }
    toks.push(Tok::Eof);
    Ok(toks)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// Recursion depth of the expression parser. The parser is a
    /// recursive-descent tree walk, so a frame full of nested parentheses
    /// would otherwise recurse once per '(' and smash the stack (SIGSEGV or,
    /// with the old panic=abort profile, a whole-process abort). All
    /// recursion cycles go through `parse_or`, so guarding it bounds the
    /// whole grammar.
    depth: usize,
    /// v2.2: while parsing the RIGHT side of a UNION, SORT/LIMIT/OFFSET
    /// stages belong to the compound result (SQL semantics) — the pipeline
    /// stage loop stops and leaves them for the UNION tail.
    hold_tail: bool,
}

/// Maximum expression nesting accepted by the DnaQL parser.
const MAX_PARSE_DEPTH: usize = 128;

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Self {
            toks,
            pos: 0,
            depth: 0,
            hold_tail: false,
        }
    }

    /// Enter one expression-recursion level. Errors out (instead of
    /// overflowing the stack) when the query nests too deeply.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(format!(
                "DnaQL: expression nested too deeply (limit {})",
                MAX_PARSE_DEPTH
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        self.pos += 1;
        t
    }

    fn kw(&mut self, word: &str) -> bool {
        if let Tok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(word) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn expect_kw(&mut self, word: &str) -> Result<(), String> {
        if self.kw(word) {
            Ok(())
        } else {
            Err(format!("DnaQL: expected '{}', got {:?}", word, self.peek()))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("DnaQL: expected identifier, got {:?}", t)),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        match self.next() {
            Tok::Int(i) => Ok(i),
            t => Err(format!("DnaQL: expected integer, got {:?}", t)),
        }
    }

    fn str_lit(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Str(s) => Ok(s),
            t => Err(format!("DnaQL: expected string literal, got {:?}", t)),
        }
    }

    fn expect(&mut self, t: Tok) -> Result<(), String> {
        if self.peek() == &t {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("DnaQL: expected {:?}, got {:?}", t, self.peek()))
        }
    }

    fn parse_query(&mut self) -> Result<DnaQlAst, String> {
        let query = match self.peek().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("FIND") => self.parse_standard_search(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("FROM") => self.parse_pipeline(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TX") => self.parse_tx(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SYNTHESIZE") => self.parse_synthesize(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ANALYZE") => self.parse_bio(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("VERIFY") => self.parse_health_check(false),
            Tok::Ident(s) if s.eq_ignore_ascii_case("REPAIR") => self.parse_health_check(true),
            Tok::Ident(s) if s.eq_ignore_ascii_case("CREATE") => self.parse_create(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("DROP") => self.parse_drop(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ALTER") => self.parse_alter_chromosome(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SHOW") => self.parse_show(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("EXPLAIN") => self.parse_explain(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("UPDATE") => {
                // Top-level row-set update: auto-wrapped in one atomic TX
                // (single-statement autocommit, same as SQL). The UPDATE
                // keyword is consumed here; parse_update_rows starts at the
                // table name (the same helper serves TX blocks).
                self.next();
                let stmt = self.parse_update_rows()?;
                if matches!(self.peek(), Tok::Semicolon) {
                    self.next();
                }
                Ok(DnaQlAst::Tx {
                    statements: vec![stmt],
                })
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("DELETE") => {
                self.next();
                let stmt = self.parse_delete_rows()?;
                if matches!(self.peek(), Tok::Semicolon) {
                    self.next();
                }
                Ok(DnaQlAst::Tx {
                    statements: vec![stmt],
                })
            }
            Tok::Ident(_) => self.parse_quick_search(),
            t => Err(format!("DnaQL: unexpected token {:?}", t)),
        }?;
        // v2.2: `pipeline UNION [ALL] pipeline [tail stages]` — compound set
        // operation mirroring SQL. The tail accepts SORT/LIMIT/OFFSET only
        // (same restriction as the SQL engine's UNION).
        if matches!(query, DnaQlAst::PipelineQuery { .. }) && self.kw("UNION") {
            let all = if self.kw("ALL") {
                true
            } else {
                self.kw("DISTINCT");
                false
            };
            self.hold_tail = true;
            let right = self.parse_pipeline()?;
            self.hold_tail = false;
            let mut tail = Vec::new();
            while matches!(self.peek(), Tok::Pipe) {
                tail.push(self.parse_tail_stage()?);
            }
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::Union {
                left: Box::new(query),
                right: Box::new(right),
                all,
                tail,
            });
        }
        // Strict tail check (same discipline as the SQL parser): anything
        // that is not EOF after the query is a parse error, never silence.
        match self.peek() {
            Tok::Eof => {}
            Tok::Semicolon => {
                self.next();
            }
            other => {
                return Err(format!(
                    "DnaQL: unexpected trailing tokens starting at {:?}",
                    other
                ))
            }
        }
        Ok(query)
    }

    /// Tail stages after UNION: SORT / LIMIT / OFFSET (applied to the
    /// compound result — same as the SQL engine's top-level ORDER BY/LIMIT).
    fn parse_tail_stage(&mut self) -> Result<PipelineStage, String> {
        self.next(); // |>
        if self.kw("SORT") {
            let column = self.ident()?;
            let asc = if self.kw("DESC") {
                false
            } else {
                self.kw("ASC");
                true
            };
            Ok(PipelineStage::Sort { column, asc })
        } else if self.kw("LIMIT") {
            let n = self.int()?;
            if n < 0 {
                return Err("DnaQL: LIMIT expects a non-negative integer".into());
            }
            Ok(PipelineStage::Limit(n as usize))
        } else if self.kw("OFFSET") {
            let n = self.int()?;
            if n < 0 {
                return Err("DnaQL: OFFSET expects a non-negative integer".into());
            }
            Ok(PipelineStage::Offset(n as usize))
        } else {
            Err(format!(
                "DnaQL: UNION tail accepts SORT/LIMIT/OFFSET, got {:?}",
                self.peek()
            ))
        }
    }

    /// VERIFY CHROMOSOME <name>;  /  REPAIR CHROMOSOME <name>;
    /// Block-level integrity scan of every segment holding keys of the
    /// chromosome. REPAIR rewrites damaged shards from parity on the fly.
    fn parse_health_check(&mut self, fix: bool) -> Result<DnaQlAst, String> {
        self.next(); // VERIFY / REPAIR
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after {}, got {:?}",
                if fix { "REPAIR" } else { "VERIFY" },
                kw
            ));
        }
        let chromosome = self.ident()?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::HealthCheck { chromosome, fix })
    }

    /// CREATE CHROMOSOME body after the `CREATE CHROMOSOME` keyword pair:
    /// name, [IF NOT EXISTS], (col TYPE, ..., PRIMARY KEY (...), FOREIGN KEY
    /// ... REFERENCES ...). The parenthesis body is collected as raw
    /// definition clauses at top comma level, with nested parens kept
    /// intact. Types and constraints are validated when the rendered CREATE
    /// TABLE runs through the SQL engine. Without an explicit PRIMARY KEY
    /// the first column is the locus.
    fn parse_chromosome_body(&mut self) -> Result<DnaQlAst, String> {
        let table = self.ident()?;
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        self.expect(Tok::LParen)?;
        let mut definitions: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut depth = 0usize;
        loop {
            match self.peek().clone() {
                Tok::RParen if depth == 0 => {
                    self.next();
                    break;
                }
                Tok::RParen => {
                    self.next();
                    depth -= 1;
                    push_clause_token(&mut buf, ")");
                }
                Tok::LParen => {
                    self.next();
                    depth += 1;
                    push_clause_token(&mut buf, "(");
                }
                Tok::Comma if depth == 0 => {
                    self.next();
                    let clause = buf.trim().to_string();
                    if !clause.is_empty() {
                        definitions.push(clause);
                    }
                    buf.clear();
                }
                Tok::Ident(s) => {
                    self.next();
                    push_clause_token(&mut buf, &s);
                }
                Tok::Int(i) => {
                    self.next();
                    push_clause_token(&mut buf, &i.to_string());
                }
                Tok::Float(f) => {
                    self.next();
                    push_clause_token(&mut buf, &float_token(&f));
                }
                Tok::Str(s) => {
                    self.next();
                    push_clause_token(&mut buf, &format!("'{}'", s));
                }
                Tok::Star => {
                    self.next();
                    push_clause_token(&mut buf, "*");
                }
                Tok::Dot => {
                    self.next();
                    push_clause_token(&mut buf, ".");
                }
                Tok::Semicolon if depth == 0 => {
                    self.next();
                    break;
                }
                t => {
                    return Err(format!(
                        "DnaQL: unexpected token {:?} in CREATE CHROMOSOME column list",
                        t
                    ))
                }
            }
        }
        let tail = buf.trim().to_string();
        if !tail.is_empty() {
            definitions.push(tail);
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        if definitions.is_empty() {
            return Err("DnaQL: CREATE CHROMOSOME needs at least one column".into());
        }
        Ok(DnaQlAst::CreateChromosome {
            table,
            definitions,
            if_not_exists,
        })
    }

    /// DROP CHROMOSOME body after the `DROP CHROMOSOME` keyword pair:
    /// name [IF EXISTS];
    fn parse_chromosome_drop_tail(&mut self) -> Result<DnaQlAst, String> {
        let table = self.ident()?;
        let if_exists = if self.kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::DropChromosome { table, if_exists })
    }

    /// CREATE dispatch: CHROMOSOME (existing DDL) or [UNIQUE] INDEX (v2.2).
    fn parse_create(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // CREATE
        if self.kw("UNIQUE") {
            self.expect_kw("INDEX")?;
            return self.parse_index_tail(true);
        }
        if self.kw("INDEX") {
            return self.parse_index_tail(false);
        }
        self.expect_kw("CHROMOSOME")?;
        self.parse_chromosome_body()
    }

    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON chromosome (col);`
    /// (IF NOT EXISTS in the SQL position — before the name.)
    fn parse_index_tail(&mut self, unique: bool) -> Result<DnaQlAst, String> {
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        self.expect_kw("ON")?;
        let table = self.ident()?;
        self.expect(Tok::LParen)?;
        let column = self.ident()?;
        self.expect(Tok::RParen)?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        })
    }

    /// DROP dispatch: CHROMOSOME (existing DDL) or INDEX (v2.2).
    /// `DROP INDEX [IF EXISTS] name [ON chromosome];`
    fn parse_drop(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // DROP
        if self.kw("INDEX") {
            let if_exists = if self.kw("IF") {
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            let table = if self.kw("ON") {
                Some(self.ident()?)
            } else {
                None
            };
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::DropIndex {
                name,
                table,
                if_exists,
            });
        }
        self.expect_kw("CHROMOSOME")?;
        self.parse_chromosome_drop_tail()
    }

    /// `ALTER CHROMOSOME t ADD COLUMN c TYPE;` (v2.2) — metadata-only online
    /// DDL rendered through the SQL engine.
    fn parse_alter_chromosome(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // ALTER
        self.expect_kw("CHROMOSOME")?;
        let table = self.ident()?;
        self.expect_kw("ADD")?;
        self.expect_kw("COLUMN")?;
        let column = self.ident()?;
        let col_type = self.ident()?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::AlterChromosome {
            table,
            column,
            col_type,
        })
    }

    /// `SHOW CHROMOSOMES;` (alias `SHOW TABLES;`) — v2.2.
    fn parse_show(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // SHOW
        if self.kw("CHROMOSOMES") || self.kw("TABLES") {
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::ShowChromosomes);
        }
        Err(format!(
            "DnaQL: expected CHROMOSOMES or TABLES after SHOW, got {:?}",
            self.peek()
        ))
    }

    /// `EXPLAIN <read-only query>;` (v2.2) — report the execution plan
    /// instead of the rows. Only reads (pipeline/pattern search) are
    /// accepted; writes would execute, so they are rejected up front.
    fn parse_explain(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // EXPLAIN
        let inner = match self.peek().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("FROM") => self.parse_pipeline()?,
            Tok::Ident(s) if s.eq_ignore_ascii_case("FIND") => self.parse_standard_search()?,
            t => {
                return Err(format!(
                    "DnaQL: EXPLAIN accepts FROM-pipelines and FIND-searches, got {:?}",
                    t
                ))
            }
        };
        Ok(DnaQlAst::Explain(Box::new(inner)))
    }

    // Quick: chr ? 'PAT' ~K -> chr2(col) => emit a,b ;
    fn parse_quick_search(&mut self) -> Result<DnaQlAst, String> {
        let chromosome = self.ident()?;
        self.expect(Tok::Question)?;
        let pattern = self.str_lit()?;

        let mode = DistanceMode::Levenshtein;
        let mut max_distance = 1usize;
        if matches!(self.peek(), Tok::Tilde) || matches!(self.peek(), Tok::Star) {
            self.next();
            max_distance = self.int()? as usize;
        }

        let mut link = None;
        if matches!(self.peek(), Tok::Arrow) {
            self.next();
            let chr2 = self.ident()?;
            let mut col = "user_id".to_string();
            if matches!(self.peek(), Tok::LParen) {
                self.next();
                col = self.ident()?;
                self.expect(Tok::RParen)?;
            }
            link = Some(LinkSpec {
                chromosome: chr2,
                column: col,
            });
        }

        let mut projection = Vec::new();
        if matches!(self.peek(), Tok::FatArrow) {
            self.next();
            self.expect_kw("emit")?;
            projection = self.parse_projection()?;
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }

        Ok(DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            projection,
        })
    }

    // Standard: FIND [PATTERN] 'PAT' IN [CHROMOSOME] chr
    //   [WHERE HAMMING_DISTANCE <= K] [LINK chr2 VIA COMPLEMENT(col)] [EMIT ...] ;
    fn parse_standard_search(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("FIND")?;
        self.kw("PATTERN");
        let pattern = self.str_lit()?;
        self.expect_kw("IN")?;
        self.kw("CHROMOSOME");
        let chromosome = self.ident()?;

        let mut mode = DistanceMode::Levenshtein;
        let mut max_distance = 1usize;
        if self.kw("WHERE") {
            if self.kw("HAMMING_DISTANCE") {
                mode = DistanceMode::Hamming;
            } else if self.kw("LEVENSHTEIN_DISTANCE") {
                mode = DistanceMode::Levenshtein;
            } else {
                return Err("DnaQL: expected HAMMING_DISTANCE or LEVENSHTEIN_DISTANCE".into());
            }
            self.expect(Tok::Le)?;
            max_distance = self.int()? as usize;
        }

        let mut link = None;
        if self.kw("LINK") {
            let chr2 = self.ident()?;
            self.expect_kw("VIA")?;
            self.expect_kw("COMPLEMENT")?;
            self.expect(Tok::LParen)?;
            let col = self.ident()?;
            self.expect(Tok::RParen)?;
            link = Some(LinkSpec {
                chromosome: chr2,
                column: col,
            });
        }

        let mut projection = Vec::new();
        if self.kw("EMIT") {
            projection = self.parse_projection()?;
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }

        Ok(DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            projection,
        })
    }

    fn parse_projection(&mut self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(out); // empty = all fields
        }
        loop {
            out.push(self.ident()?);
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        Ok(out)
    }

    // FROM chr |> LINK chr2 VIA COMPLEMENT(col) |> WHERE expr |> EMIT a,b
    //    |> JOIN t2 ON a = b |> GROUP BY g AGG n = COUNT(*) |> SORT x |> OFFSET n |> LIMIT n
    fn parse_pipeline(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("FROM")?;
        let source = self.ident()?;
        let mut stages = Vec::new();
        while matches!(self.peek(), Tok::Pipe) {
            self.next();
            if self.kw("LINK") {
                let chr2 = self.ident()?;
                let mut spec = LinkSpec {
                    chromosome: chr2,
                    column: "user_id".to_string(),
                };
                if self.kw("VIA") {
                    self.expect_kw("COMPLEMENT")?;
                    self.expect(Tok::LParen)?;
                    spec.column = self.ident()?;
                    self.expect(Tok::RParen)?;
                }
                stages.push(PipelineStage::Link(Some(spec)));
            } else if self.kw("WHERE") {
                let e = self.parse_expr()?;
                stages.push(PipelineStage::Filter(e));
            } else if self.kw("EMIT") {
                let p = self.parse_emit_items()?;
                stages.push(PipelineStage::Emit(p));
            } else if self.kw("SORT") {
                if self.hold_tail {
                    // rewind keyword + pipe: SORT belongs to the UNION tail
                    self.pos -= 2;
                    break;
                }
                let column = self.ident()?;
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                stages.push(PipelineStage::Sort { column, asc });
            } else if self.kw("LIMIT") {
                if self.hold_tail {
                    self.pos -= 2; // keyword + pipe
                    break;
                }
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: LIMIT expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Limit(n as usize));
            } else if self.kw("OFFSET") {
                if self.hold_tail {
                    self.pos -= 2; // keyword + pipe
                    break;
                }
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: OFFSET expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Offset(n as usize));
            } else if self.kw("GROUP") {
                stages.push(self.parse_group_stage()?);
            } else if self.kw("JOIN") {
                stages.push(self.parse_join_tail(JoinKind::Inner)?);
            } else if self.kw("INNER") {
                self.expect_kw("JOIN")?;
                stages.push(self.parse_join_tail(JoinKind::Inner)?);
            } else if self.kw("LEFT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                stages.push(self.parse_join_tail(JoinKind::Left)?);
            } else if self.kw("RIGHT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                stages.push(self.parse_join_tail(JoinKind::Right)?);
            } else if self.kw("FULL") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                stages.push(self.parse_join_tail(JoinKind::Full)?);
            } else {
                return Err(format!("DnaQL: unknown pipeline stage {:?}", self.peek()));
            }
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::PipelineQuery { source, stages })
    }

    /// JOIN tail after the JOIN keyword: `chr ON left_col = right_col`.
    /// Qualified names (`t.col`) are accepted and stripped — DnaQL rows are
    /// single-namespace, mirroring the SQL engine's row model.
    fn parse_join_tail(&mut self, kind: JoinKind) -> Result<PipelineStage, String> {
        let chromosome = self.ident()?;
        self.expect_kw("ON")?;
        let left_col = self.parse_join_column()?;
        self.expect(Tok::Eq)?;
        let right_col = self.parse_join_column()?;
        Ok(PipelineStage::Join(JoinSpec {
            kind,
            chromosome,
            left_col,
            right_col,
        }))
    }

    /// One side of an ON clause: bare identifier, optionally qualified
    /// (`t.col` / `alias.col`) — the qualifier is stripped.
    fn parse_join_column(&mut self) -> Result<String, String> {
        let mut col = self.ident()?;
        if matches!(self.peek(), Tok::Dot) {
            self.next();
            col = self.ident()?;
        }
        Ok(col)
    }

    /// `|> GROUP [BY col, ...] AGG name = FN(col), ...`
    /// AGG is required only when BY is omitted (otherwise the stage is a
    /// DISTINCT-style projection of the group columns).
    fn parse_group_stage(&mut self) -> Result<PipelineStage, String> {
        let mut by = Vec::new();
        if self.kw("BY") {
            loop {
                by.push(self.ident()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
        }
        let mut aggs = Vec::new();
        if self.kw("AGG") {
            loop {
                let name = self.ident()?;
                self.expect(Tok::Eq)?;
                let func_name = self.ident()?;
                let func = AggFunc::from_name(&func_name)
                    .ok_or_else(|| format!("DnaQL: unknown aggregate function '{}'", func_name))?;
                self.expect(Tok::LParen)?;
                let arg = if matches!(self.peek(), Tok::Star) {
                    self.next();
                    if func != AggFunc::Count {
                        return Err("DnaQL: only COUNT accepts (*)".into());
                    }
                    None
                } else if matches!(self.peek(), Tok::RParen) {
                    if func == AggFunc::Count {
                        // COUNT() == COUNT(*) — accepted for convenience.
                        None
                    } else {
                        return Err(format!(
                            "DnaQL: {} requires a column argument",
                            func.as_str()
                        ));
                    }
                } else {
                    Some(self.ident()?)
                };
                self.expect(Tok::RParen)?;
                aggs.push((name, func, arg));
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
        }
        if by.is_empty() && aggs.is_empty() {
            return Err("DnaQL: GROUP needs BY columns and/or AGG items".into());
        }
        Ok(PipelineStage::Group { by, aggs })
    }

    /// EMIT list (v2.2): `*`, bare columns, and computed expressions with
    /// optional aliases — `EMIT user_id, total_usd * 2 AS double`.
    fn parse_emit_items(&mut self) -> Result<Vec<EmitItem>, String> {
        let mut out = Vec::new();
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(vec![EmitItem::Star]);
        }
        loop {
            // Bare column when the item is a single identifier (no AS tail,
            // no arithmetic that would make it a computed expression).
            let start = self.pos;
            if let Tok::Ident(name) = self.peek().clone() {
                self.next();
                let qualified = matches!(self.peek(), Tok::Dot);
                let aliased = self.kw("AS");
                // An arithmetic operator right after the identifier means
                // this item is a computed expression (`balance * 2 AS x`).
                let arithmetic = matches!(
                    self.peek(),
                    Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash
                );
                if !qualified && !aliased && !arithmetic {
                    out.push(EmitItem::Col(name));
                    if matches!(self.peek(), Tok::Comma) {
                        self.next();
                        continue;
                    }
                    break;
                }
                if arithmetic || (qualified && aliased) {
                    // Rewind and parse the full expression.
                    self.pos = start;
                    let expr = self.parse_expr()?;
                    self.expect_kw("AS")?;
                    let alias = self.ident()?;
                    out.push(EmitItem::Computed { expr, alias });
                } else if aliased {
                    if qualified {
                        return Err("DnaQL: alias a bare column, not a qualified name".into());
                    }
                    let alias = self.ident()?;
                    out.push(EmitItem::Computed {
                        expr: Expr::Column(name),
                        alias,
                    });
                } else {
                    // Qualified column reference: strip the qualifier.
                    self.expect(Tok::Dot)?;
                    let bare = self.ident()?;
                    out.push(EmitItem::Col(bare));
                }
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                    continue;
                }
                break;
            }
            self.pos = start;
            // Computed expression: `expr AS alias`.
            let expr = self.parse_expr()?;
            self.expect_kw("AS")?;
            let alias = self.ident()?;
            out.push(EmitItem::Computed { expr, alias });
            if matches!(self.peek(), Tok::Comma) {
                self.next();
                continue;
            }
            break;
        }
        Ok(out)
    }

    // TX { stmt; stmt; ... }
    fn parse_tx(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("TX")?;
        self.expect(Tok::LBrace)?;
        let mut statements = Vec::new();
        loop {
            if matches!(self.peek(), Tok::RBrace) {
                self.next();
                break;
            }
            statements.push(self.parse_tx_stmt()?);
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            } else if !matches!(self.peek(), Tok::RBrace) {
                return Err("DnaQL: expected ';' or '}' in TX block".into());
            }
        }
        Ok(DnaQlAst::Tx { statements })
    }

    // SYNTHESIZE INTO a, b BEGIN stmt; stmt LIGATE
    fn parse_synthesize(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("SYNTHESIZE")?;
        self.expect_kw("INTO")?;
        let mut _targets = Vec::new();
        loop {
            _targets.push(self.ident()?);
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect_kw("BEGIN")?;
        let mut statements = Vec::new();
        loop {
            if self.kw("LIGATE") || self.kw("HYDROLYZE") {
                break;
            }
            statements.push(self.parse_tx_stmt()?);
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
        }
        Ok(DnaQlAst::Tx { statements })
    }

    fn parse_tx_stmt(&mut self) -> Result<TxStmt, String> {
        if self.kw("DROP") {
            self.expect_kw("LOCUS")?;
            let table = self.ident()?;
            self.expect(Tok::Colon)?;
            let locus = self.int()?;
            let cascade = self.kw("CASCADE");
            return Ok(TxStmt::DropLocus {
                table,
                locus,
                cascade,
            });
        }
        // v2.2: row-set statements inside TX blocks.
        if self.kw("UPDATE") {
            return self.parse_update_rows();
        }
        if self.kw("DELETE") {
            return self.parse_delete_rows();
        }
        let table = self.ident()?;

        // Explicit locus: `chr:locus (+= { ... } | ?= { ... } | .field = v)`.
        let explicit_locus = if matches!(self.peek(), Tok::Colon) {
            self.next();
            Some(self.int()?)
        } else {
            None
        };

        // v2.2: `?=` — insert-if-absent (SQL ON CONFLICT DO NOTHING spelling).
        if matches!(self.peek(), Tok::QuestionEq) {
            self.next();
            self.expect(Tok::LBrace)?;
            let fields = self.parse_kv_fields()?;
            let locus = match explicit_locus {
                Some(l) => l,
                None => auto_locus(&fields)?,
            };
            return Ok(TxStmt::InsertIfAbsent {
                table,
                locus,
                fields,
            });
        }

        if matches!(self.peek(), Tok::PlusEq) {
            // chr += { k: v, ... } (auto-locus from the PK-like field) or
            // chr:locus += { k: v, ... }.
            self.next();
            self.expect(Tok::LBrace)?;
            let fields = self.parse_kv_fields()?;
            let locus = match explicit_locus {
                Some(l) => l,
                None => auto_locus(&fields)?,
            };
            return Ok(TxStmt::Append {
                table,
                locus,
                fields,
            });
        }

        let locus = explicit_locus.ok_or_else(|| {
            format!(
                "DnaQL: expected ':', '+=' or '?=' after chromosome '{}', got {:?}",
                table,
                self.peek()
            )
        })?;

        // field path: .field
        self.expect(Tok::Dot)?;
        let field = self.ident()?;
        let op = match self.next() {
            Tok::Eq => AssignOp::Set,
            Tok::PlusEq => AssignOp::Add,
            Tok::MinusEq => AssignOp::Sub,
            t => return Err(format!("DnaQL: expected '=', '+=' or '-=', got {:?}", t)),
        };
        let value = self.parse_value()?;
        Ok(TxStmt::Mutate {
            table,
            locus,
            field,
            op,
            value,
        })
    }

    /// `UPDATE chr SET f = v, ... [WHERE expr]` (v2.2). Shared by the
    /// top-level dispatch (auto-wrapped in a one-statement TX) and TX blocks.
    fn parse_update_rows(&mut self) -> Result<TxStmt, String> {
        let table = self.ident()?;
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            let field = self.ident()?;
            self.expect(Tok::Eq)?;
            let value = self.parse_value()?;
            assignments.push((field, value));
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(TxStmt::UpdateRows {
            table,
            assignments,
            where_clause,
        })
    }

    /// `DELETE FROM chr [WHERE expr]` (v2.2).
    fn parse_delete_rows(&mut self) -> Result<TxStmt, String> {
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(TxStmt::DeleteRows {
            table,
            where_clause,
        })
    }

    /// KV list after `+=` / `?=`: `{ k: v, k2: v2 }`.
    fn parse_kv_fields(&mut self) -> Result<Vec<(String, Value)>, String> {
        let mut fields = Vec::new();
        loop {
            if matches!(self.peek(), Tok::RBrace) {
                self.next();
                break;
            }
            let k = self.ident()?;
            self.expect(Tok::Colon)?;
            let v = self.parse_value()?;
            fields.push((k, v));
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            }
        }
        Ok(fields)
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.next() {
            Tok::Minus => match self.next() {
                Tok::Int(i) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| "DnaQL: integer literal out of range".to_string()),
                Tok::Float(f) => Ok(Value::Float(-f)),
                t => Err(format!("DnaQL: expected number after '-', got {:?}", t)),
            },
            Tok::Int(i) => Ok(Value::Int(i)),
            Tok::Float(f) => Ok(Value::Float(f)),
            Tok::Str(s) => {
                // DNA sequences: all-ACGT strings become Value::Dna
                if !s.is_empty()
                    && s.chars()
                        .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'))
                {
                    Ok(Value::Dna(s.to_uppercase()))
                } else {
                    Ok(Value::Text(s))
                }
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            t => Err(format!("DnaQL: expected literal, got {:?}", t)),
        }
    }

    // ANALYZE SEQUENCE 'ACGT' |> GC_CONTENT() |> ...
    fn parse_bio(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("ANALYZE")?;
        let (sequence, locus) = if self.kw("SEQUENCE") {
            (Some(self.str_lit()?), None)
        } else if self.kw("LOCUS") {
            let table = self.ident()?;
            self.expect(Tok::Colon)?;
            let id = self.int()?;
            (None, Some((table, id)))
        } else {
            return Err("DnaQL: expected SEQUENCE or LOCUS after ANALYZE".into());
        };
        let mut functions = Vec::new();
        while matches!(self.peek(), Tok::Pipe) {
            self.next();
            let fname = self.ident()?;
            self.expect(Tok::LParen)?;
            self.expect(Tok::RParen)?;
            functions.push(fname.to_uppercase());
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::BioAnalysis {
            sequence,
            locus,
            functions,
        })
    }

    // ---- expressions (WHERE) ----
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let result = self.parse_or_inner();
        self.leave();
        result
    }

    fn parse_or_inner(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.kw("OR") {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_not()?;
        while self.kw("AND") {
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `NOT <cmp>` — v2.2: the shared Expr::Not AST (same as SQL); the
    /// manual promised NOT support, the parser now delivers it.
    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.kw("NOT") {
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let left = self.parse_operand()?;
        let op = match self.peek() {
            Tok::Eq => Some(CmpOp::Eq),
            Tok::Ne => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let right = self.parse_operand()?;
            return Ok(Expr::Cmp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        if self.kw("IS") {
            if self.kw("NOT") {
                self.expect_kw("NULL")?;
                return Ok(Expr::IsNotNull(Box::new(left)));
            }
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(left)));
        }
        // `col IN (v1, v2, ...)` — same semantics as SQL; subqueries do not
        // exist in DnaQL (the SQL engine materialises those itself).
        if self.kw("IN") {
            let column = match left {
                Expr::Column(c) => c,
                other => {
                    return Err(format!(
                        "DnaQL: IN requires a column on the left, got '{}'",
                        other.describe()
                    ))
                }
            };
            self.expect(Tok::LParen)?;
            let mut values = Vec::new();
            loop {
                values.push(self.parse_value()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            return Ok(Expr::InList { column, values });
        }
        Ok(left)
    }

    /// additive  ::= multiplicative (('+' | '-') multiplicative)*
    fn parse_operand(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => ArithOp::Add,
                Tok::Minus => ArithOp::Sub,
                _ => break,
            };
            self.next();
            let right = self.parse_multiplicative()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// multiplicative ::= unary (('*' | '/') unary)*
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => ArithOp::Mul,
                Tok::Slash => ArithOp::Div,
                _ => break,
            };
            self.next();
            let right = self.parse_unary()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// unary ::= '-' primary | primary
    fn parse_unary(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Tok::Minus) {
            self.next();
            let inner = self.parse_unary()?;
            return Ok(Expr::Arith {
                op: ArithOp::Sub,
                left: Box::new(Expr::Literal(Value::Int(0))),
                right: Box::new(inner),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.next();
                if !s.is_empty()
                    && s.chars()
                        .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'))
                {
                    Ok(Expr::Literal(Value::Dna(s.to_uppercase())))
                } else {
                    Ok(Expr::Literal(Value::Text(s)))
                }
            }
            Tok::Ident(s) => {
                self.next();
                if s.eq_ignore_ascii_case("NULL") {
                    Ok(Expr::Literal(Value::Null))
                } else {
                    Ok(Expr::Column(s))
                }
            }
            Tok::LParen => {
                self.next();
                let e = self.parse_or()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            t => Err(format!("DnaQL: expected operand, got {:?}", t)),
        }
    }
}

/// Auto-locus from the PK-like integer field (`id` / `*_id`) of a KV list.
fn auto_locus(fields: &[(String, Value)]) -> Result<i64, String> {
    fields
        .iter()
        .find(|(k, v)| (k == "id" || k.ends_with("_id")) && matches!(v, Value::Int(_)))
        .and_then(|(_, v)| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        })
        .ok_or_else(|| "DnaQL: statement without explicit locus needs an integer '<...>_id' field".to_string())
}

/// Append one raw token to a CREATE CHROMOSOME definition clause buffer,
/// separating tokens with a single space (whitespace is irrelevant to the
/// SQL engine that re-parses these clauses).
fn push_clause_token(buf: &mut String, tok: &str) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(tok);
}

/// Float rendering that always keeps a decimal point, so re-parsing the
/// token yields a Float (and not an Int) again.
pub(crate) fn float_token(f: &f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

// ---------------------------------------------------------------------------
// Engine (parser + executor)
// ---------------------------------------------------------------------------

/// Parse a DnaQL query and return every chromosome/table it references. Used
/// by the per-user ACL before execution; this does not touch storage.
pub fn referenced_tables(query: &str) -> Result<Vec<String>, String> {
    let toks = tokenize(query)?;
    let mut parser = Parser::new(toks);
    let ast = parser.parse_query()?;
    let mut tables = Vec::new();
    collect_ast_tables(&ast, &mut tables);
    tables.sort();
    tables.dedup();
    Ok(tables)
}

fn collect_ast_tables(ast: &DnaQlAst, out: &mut Vec<String>) {
    match ast {
        DnaQlAst::PatternSearch {
            chromosome, link, ..
        } => {
            out.push(chromosome.clone());
            if let Some(link) = link {
                out.push(link.chromosome.clone());
            }
        }
        DnaQlAst::PipelineQuery { source, stages } => {
            out.push(source.clone());
            for stage in stages {
                match stage {
                    PipelineStage::Link(Some(link)) => out.push(link.chromosome.clone()),
                    PipelineStage::Join(spec) => out.push(spec.chromosome.clone()),
                    _ => {}
                }
            }
        }
        DnaQlAst::Tx { statements } => {
            for statement in statements {
                match statement {
                    TxStmt::Mutate { table, .. }
                    | TxStmt::Append { table, .. }
                    | TxStmt::InsertIfAbsent { table, .. }
                    | TxStmt::DropLocus { table, .. }
                    | TxStmt::UpdateRows { table, .. }
                    | TxStmt::DeleteRows { table, .. } => out.push(table.clone()),
                }
            }
        }
        DnaQlAst::BioAnalysis { locus, .. } => {
            // ANALYZE LOCUS <table>:<id> reads one row of <table>, so the
            // per-user table ACL must see it. Returning an empty list here
            // let a table-restricted user read any locus of any table.
            if let Some((table, _)) = locus {
                out.push(table.clone());
            }
        }
        DnaQlAst::HealthCheck { chromosome, .. } => out.push(chromosome.clone()),
        DnaQlAst::CreateChromosome { table, .. }
        | DnaQlAst::DropChromosome { table, .. }
        | DnaQlAst::AlterChromosome { table, .. }
        | DnaQlAst::CreateIndex { table, .. } => out.push(table.clone()),
        DnaQlAst::DropIndex { table, .. } => {
            if let Some(t) = table {
                out.push(t.clone());
            }
        }
        // SHOW CHROMOSOMES lists every table; the ACL allows it through and
        // the executor filters by the caller's grants.
        DnaQlAst::ShowChromosomes => {}
        DnaQlAst::Explain(inner) => collect_ast_tables(inner, out),
        DnaQlAst::Union { left, right, .. } => {
            collect_ast_tables(left, out);
            collect_ast_tables(right, out);
        }
    }
}

pub struct DnaQlEngine {
    pub engine: Arc<Engine>,
    pub tx: TransactionCoordinator,
    /// Shared SQL engine — schema DDL (CREATE/DROP CHROMOSOME) goes through
    /// it so the SQL side's in-memory schema cache stays consistent.
    pub sql: Arc<SqlEngine>,
}

impl DnaQlEngine {
    pub fn new(engine: Arc<Engine>, sql: Arc<SqlEngine>) -> Self {
        let tx = TransactionCoordinator::new(engine.clone());
        Self { engine, tx, sql }
    }

    pub fn parse(&self, query: &str) -> Result<DnaQlAst, String> {
        let toks = tokenize(query)?;
        let mut p = Parser::new(toks);
        p.parse_query()
    }

    pub fn execute(&self, query: &str) -> Result<DnaQlResult, String> {
        let start = std::time::Instant::now();
        let ast = self.parse(query)?;
        let result = match &ast {
            DnaQlAst::PatternSearch {
                chromosome,
                pattern,
                mode,
                max_distance,
                link,
                projection,
            } => self.exec_pattern_search(
                chromosome,
                pattern,
                mode.clone(),
                *max_distance,
                link.clone(),
                projection.clone(),
            )?,
            DnaQlAst::PipelineQuery { source, stages } => self.exec_pipeline(source, stages)?,
            DnaQlAst::Tx { statements } => self.exec_tx(statements)?,
            DnaQlAst::BioAnalysis {
                sequence,
                locus,
                functions,
            } => self.exec_bio(sequence.clone(), locus.clone(), functions)?,
            DnaQlAst::HealthCheck { chromosome, fix } => {
                self.exec_health_check(chromosome, *fix)?
            }
            DnaQlAst::CreateChromosome {
                table,
                definitions,
                if_not_exists,
            } => self.exec_create_chromosome(table, definitions, *if_not_exists)?,
            DnaQlAst::DropChromosome { table, if_exists } => {
                self.exec_drop_chromosome(table, *if_exists)?
            }
            DnaQlAst::AlterChromosome {
                table,
                column,
                col_type,
            } => self.exec_alter_chromosome(table, column, col_type)?,
            DnaQlAst::CreateIndex {
                name,
                table,
                column,
                unique,
                if_not_exists,
            } => self.exec_create_index(name, table, column, *unique, *if_not_exists)?,
            DnaQlAst::DropIndex {
                name,
                table,
                if_exists,
            } => self.exec_drop_index(name, table.as_deref(), *if_exists)?,
            DnaQlAst::ShowChromosomes => self.exec_show_chromosomes()?,
            DnaQlAst::Explain(inner) => {
                // Reads are side-effect free, so EXPLAIN executes the query
                // and reports the plan instead of the rows.
                let (_, note) = self.exec_readonly(inner)?;
                return Ok(DnaQlResult {
                    ast_summary: format!("Explain({:?})", ast_summary(inner)),
                    rows: Vec::new(),
                    engine_note: format!("plan: {}", note),
                    execution_time_us: start.elapsed().as_micros() as u64,
                });
            }
            DnaQlAst::Union {
                left,
                right,
                all,
                tail,
            } => self.exec_union(left, right, *all, tail)?,
        };

        Ok(DnaQlResult {
            ast_summary: format!("{:?}", ast_summary(&ast)),
            rows: result.0,
            engine_note: result.1,
            execution_time_us: start.elapsed().as_micros() as u64,
        })
    }

    fn exec_pattern_search(
        &self,
        chromosome: &str,
        pattern: &str,
        mode: DistanceMode,
        max_distance: usize,
        link: Option<LinkSpec>,
        projection: Vec<String>,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        if pattern.is_empty() || pattern.len() > 64 {
            return Err(format!(
                "DnaQL: pattern length must be 1..=64, got {}",
                pattern.len()
            ));
        }
        PackedDna::from_ascii(pattern).map_err(|e| format!("DnaQL: invalid pattern: {}", e))?;

        let prefix = format!("{}:", chromosome);
        let raw_rows = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;

        let mut out = Vec::new();
        let mut scanned = 0usize;
        for (key, payload) in raw_rows {
            scanned += 1;
            let fields = RowCodec::decode(&payload).map_err(|e| {
                format!(
                    "DnaQL: row {} unreadable: {}",
                    String::from_utf8_lossy(&key),
                    e
                )
            })?;
            let row_map: BTreeMap<String, Value> = fields.into_iter().collect();

            let seq_str = match row_map.get("sequence") {
                Some(Value::Dna(s)) | Some(Value::Text(s)) => s.clone(),
                _ => continue, // rows without sequence can't be pattern-searched
            };
            let packed = PackedDna::from_ascii(&seq_str)
                .map_err(|e| format!("DnaQL: stored sequence invalid: {}", e))?;

            let matches = match mode {
                DistanceMode::Levenshtein => BitapMatcher::search(&packed, pattern, max_distance),
                DistanceMode::Hamming => {
                    BitapMatcher::search_hamming(&packed, pattern, max_distance)
                }
            };

            for m in matches {
                let mut result_row = BTreeMap::new();
                result_row.insert(
                    "locus".to_string(),
                    Value::Text(String::from_utf8_lossy(&key).to_string()),
                );
                result_row.insert("offset_bp".to_string(), Value::Int(m.offset_bp as i64));
                result_row.insert("mismatches".to_string(), Value::Int(m.mismatches as i64));
                result_row.insert(
                    "matched_seq".to_string(),
                    Value::Dna(m.matched_sequence.clone()),
                );
                // merge source row fields (locus_id, variant_name, ...)
                for (k, v) in &row_map {
                    result_row.entry(k.clone()).or_insert_with(|| v.clone());
                }

                // Complementary linkage: O(1) direct key seek
                if let Some(spec) = &link {
                    let fk_val = row_map.get(&spec.column).cloned().unwrap_or(Value::Null);
                    let target_key = format!("{}:{}", spec.chromosome, fk_val);
                    if let Some(target_bytes) = self
                        .engine
                        .get(target_key.as_bytes())
                        .map_err(|e| format!("DnaQL storage error: {}", e))?
                    {
                        let target_fields = RowCodec::decode(&target_bytes)
                            .map_err(|e| format!("DnaQL: link target unreadable: {}", e))?;
                        for (k, v) in target_fields {
                            // prefer the plain field name, then namespace, then link_
                            let name = if !result_row.contains_key(&k) {
                                k.clone()
                            } else {
                                let ns = format!("{}.{}", spec.chromosome, k);
                                if result_row.contains_key(&ns) {
                                    format!("link_{}", k)
                                } else {
                                    ns
                                }
                            };
                            result_row.insert(name, v);
                        }
                    } else {
                        result_row.insert("link_missing".to_string(), Value::Text(target_key));
                    }
                }

                let row = if projection.is_empty() {
                    DnaQlRow { fields: result_row }
                } else {
                    let mut filtered = BTreeMap::new();
                    for p in &projection {
                        if let Some(v) = result_row.get(p) {
                            filtered.insert(p.clone(), v.clone());
                        }
                    }
                    DnaQlRow { fields: filtered }
                };
                out.push(row);
            }
        }

        let note = format!(
            "real scan: {} loci, bitap {} k={}, {} matches",
            scanned,
            match mode {
                DistanceMode::Hamming => "Hamming",
                DistanceMode::Levenshtein => "Levenshtein",
            },
            max_distance,
            out.len()
        );
        Ok((out, note))
    }

    fn exec_create_chromosome(
        &self,
        table: &str,
        definitions: &[String],
        if_not_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        if definitions.is_empty() {
            return Err("DnaQL: CREATE CHROMOSOME needs at least one column".into());
        }
        let mut clauses: Vec<String> = definitions.to_vec();
        // No explicit PRIMARY KEY clause: the first column becomes the locus,
        // mirroring SQL CREATE TABLE's fallback (first column as PK).
        let has_pk = definitions.iter().any(|d| {
            d.split_whitespace()
                .next()
                .map(|w| w.eq_ignore_ascii_case("PRIMARY"))
                .unwrap_or(false)
        });
        if !has_pk {
            let first = definitions[0]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if first.is_empty() {
                return Err("DnaQL: CREATE CHROMOSOME first clause names no column".into());
            }
            clauses.push(format!("PRIMARY KEY ({})", first));
        }
        let nie = if if_not_exists { "IF NOT EXISTS " } else { "" };
        let sql = format!("CREATE TABLE {}{} ({})", nie, table, clauses.join(", "));
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("chromosome '{}' created", table)),
        ))
    }

    fn exec_drop_chromosome(
        &self,
        table: &str,
        if_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let sql = if if_exists {
            format!("DROP TABLE IF EXISTS {}", table)
        } else {
            format!("DROP TABLE {}", table)
        };
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("chromosome '{}' dropped", table)),
        ))
    }

    /// `ALTER CHROMOSOME t ADD COLUMN c TYPE` — rendered through the SQL
    /// engine's metadata-only online DDL (one schema-row rewrite, instant).
    fn exec_alter_chromosome(
        &self,
        table: &str,
        column: &str,
        col_type: &str,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type);
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("column '{}' added to chromosome '{}'", column, table)),
        ))
    }

    /// `CREATE [UNIQUE] INDEX name ON chromosome (col)` — rendered through
    /// the SQL engine so both dialects share one index registry.
    fn exec_create_index(
        &self,
        name: &str,
        table: &str,
        column: &str,
        unique: bool,
        if_not_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let uniq = if unique { "UNIQUE " } else { "" };
        let nie = if if_not_exists { "IF NOT EXISTS " } else { "" };
        let sql = format!(
            "CREATE {}INDEX {}{} ON {} ({})",
            uniq, nie, name, table, column
        );
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("index '{}' created on '{}'", name, table)),
        ))
    }

    /// `DROP INDEX name [ON chromosome]` — rendered through the SQL engine.
    fn exec_drop_index(
        &self,
        name: &str,
        table: Option<&str>,
        if_exists: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let on = table
            .map(|t| format!(" ON {}", t))
            .unwrap_or_default();
        // SQL parser's DROP INDEX grammar takes IF EXISTS before the name.
        let sql = if if_exists {
            format!("DROP INDEX IF EXISTS {}{}", name, on)
        } else {
            format!("DROP INDEX {}{}", name, on)
        };
        let res = self
            .sql
            .execute(&sql)
            .map_err(|e| format!("DnaQL DDL: {}", e))?;
        Ok((
            vec![],
            res.message
                .unwrap_or_else(|| format!("index '{}' dropped", name)),
        ))
    }

    /// `SHOW CHROMOSOMES` — one row per table registered in the schema
    /// registry (the same registry the SQL engine's SHOW TABLES reads).
    fn exec_show_chromosomes(&self) -> Result<(Vec<DnaQlRow>, String), String> {
        let mut tables = self.sql.list_tables();
        tables.sort();
        let rows = tables
            .into_iter()
            .map(|t| {
                let mut f = BTreeMap::new();
                f.insert(
                    "chromosome".to_string(),
                    Value::Text(t.clone()),
                );
                DnaQlRow { fields: f }
            })
            .collect();
        Ok((rows, "real table introspection via the shared schema registry".into()))
    }

    fn exec_pipeline(
        &self,
        source: &str,
        stages: &[PipelineStage],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let (mut rows, mut plan) = self.scan_rows(source, "SeqScan")?;
        for stage in stages {
            self.apply_stage(stage, &mut rows, &mut plan)?;
        }
        let note = format!("real pipeline: {}", plan.join(" -> "));
        Ok((rows, note))
    }

    /// Scan one chromosome's rows and insert the synthetic `locus` field,
    /// mirroring the SQL engine's `read_table_rows` (schema meta-rows and
    /// index keys never leak into results).
    fn scan_rows(&self, source: &str, plan_label: &str) -> Result<(Vec<DnaQlRow>, Vec<String>), String> {
        let prefix = format!("{}:", source);
        let raw_rows = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;
        let mut rows: Vec<DnaQlRow> = Vec::new();
        for (key, payload) in raw_rows {
            if String::from_utf8_lossy(&key).ends_with(":schema") {
                continue; // legacy meta-row visibility rule (same as SQL)
            }
            let fields =
                RowCodec::decode(&payload).map_err(|e| format!("DnaQL: row unreadable: {}", e))?;
            let mut row_map: BTreeMap<String, Value> = fields.into_iter().collect();
            row_map.insert(
                "locus".to_string(),
                Value::Text(String::from_utf8_lossy(&key).to_string()),
            );
            rows.push(DnaQlRow { fields: row_map });
        }
        Ok((rows, vec![format!("{}({})", plan_label, source)]))
    }

    /// Apply one pipeline stage to the row stream (used by exec_pipeline and
    /// the UNION tail).
    fn apply_stage(
        &self,
        stage: &PipelineStage,
        rows: &mut Vec<DnaQlRow>,
        plan: &mut Vec<String>,
    ) -> Result<(), String> {
        match stage {
            PipelineStage::Link(spec) => {
                let spec = spec.clone().ok_or("DnaQL: LINK requires a target")?;
                plan.push(format!("KeySeek({}.{})", spec.chromosome, spec.column));
                for row in rows.iter_mut() {
                    let fk_val = row.fields.get(&spec.column).cloned().unwrap_or(Value::Null);
                    let target_key = format!("{}:{}", spec.chromosome, fk_val);
                    if let Some(target_bytes) = self
                        .engine
                        .get(target_key.as_bytes())
                        .map_err(|e| format!("DnaQL storage error: {}", e))?
                    {
                        let target_fields = RowCodec::decode(&target_bytes)
                            .map_err(|e| format!("DnaQL: link target unreadable: {}", e))?;
                        for (k, v) in target_fields {
                            let mut name = format!("{}.{}", spec.chromosome, k);
                            if row.fields.contains_key(&name) {
                                name = format!("link_{}", k);
                            }
                            row.fields.insert(name, v);
                        }
                    }
                }
            }
            PipelineStage::Filter(e) => {
                plan.push(format!("Filter({})", e.describe()));
                rows.retain(|r| e.eval_bool(&r.fields));
            }
            PipelineStage::Emit(projection) => {
                plan.push(format!("Project({:?})", projection));
                for row in rows.iter_mut() {
                    let mut filtered = BTreeMap::new();
                    for item in projection {
                        match item {
                            EmitItem::Star => {
                                for (k, v) in row.fields.clone() {
                                    filtered.insert(k, v);
                                }
                            }
                            EmitItem::Col(name) => {
                                if let Some(v) = row.fields.get(name.as_str()) {
                                    filtered.insert(name.clone(), v.clone());
                                }
                            }
                            EmitItem::Computed { expr, alias } => {
                                let v = expr.eval(&row.fields).unwrap_or(Value::Null);
                                filtered.insert(alias.clone(), v);
                            }
                        }
                    }
                    row.fields = filtered;
                }
            }
            PipelineStage::Sort { column, asc } => {
                plan.push(format!(
                    "Sort({} {})",
                    column,
                    if *asc { "ASC" } else { "DESC" }
                ));
                rows.sort_by(|a, b| {
                    let va = a.fields.get(column).cloned().unwrap_or(Value::Null);
                    let vb = b.fields.get(column).cloned().unwrap_or(Value::Null);
                    let ord = va.compare(&vb);
                    if *asc {
                        ord
                    } else {
                        ord.reverse()
                    }
                });
            }
            PipelineStage::Limit(n) => {
                plan.push(format!("Limit({})", n));
                rows.truncate(*n);
            }
            PipelineStage::Offset(n) => {
                plan.push(format!("Offset({})", n));
                rows.drain(0..(*n).min(rows.len()));
            }
            PipelineStage::Join(spec) => {
                let (right, _) = self.scan_rows(&spec.chromosome, "SeqScan")?;
                let joined = self.hash_join_stage(rows, right, spec, plan)?;
                *rows = joined;
            }
            PipelineStage::Group { by, aggs } => {
                self.apply_group(by, aggs, rows, plan)?;
            }
        }
        Ok(())
    }

    /// Hash equi-join mirroring the SQL engine (`sql_engine::hash_join`
    /// family): single-namespace rows, `joined_` prefix on conflicting field
    /// names, NULL keys never match, outer sides NULL-extended from the
    /// other side's column set. RIGHT is LEFT with the sides swapped.
    fn hash_join_stage(
        &self,
        left: &[DnaQlRow],
        right: Vec<DnaQlRow>,
        spec: &JoinSpec,
        plan: &mut Vec<String>,
    ) -> Result<Vec<DnaQlRow>, String> {
        use std::collections::HashMap;
        let label = match spec.kind {
            JoinKind::Inner => "HashJoin",
            JoinKind::Left => "LeftHashJoin",
            JoinKind::Right => "RightHashJoin",
            JoinKind::Full => "FullHashJoin",
        };
        plan.push(format!(
            "{}({}.{}, {}.{})",
            label, spec.chromosome, spec.left_col, spec.chromosome, spec.right_col
        ));

        // RIGHT == LEFT with sides swapped (identical row semantics).
        let (probe_rows, build_rows) = match spec.kind {
            JoinKind::Right => (right, left.to_vec()),
            _ => (left.to_vec(), right),
        };
        let (probe_col, build_col) = match spec.kind {
            JoinKind::Right => (&spec.right_col, &spec.left_col),
            _ => (&spec.left_col, &spec.right_col),
        };
        let kind = match spec.kind {
            JoinKind::Right => JoinKind::Left,
            other => other,
        };

        let join_key = |v: Option<&Value>| -> Option<Vec<u8>> {
            let v = v?;
            if v.is_null() {
                None
            } else {
                Some(crate::relational::encode_index_value(v))
            }
        };

        // Build phase: key -> indices into build_rows.
        let mut index: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (i, r) in build_rows.iter().enumerate() {
            if let Some(key) = join_key(r.fields.get(build_col)) {
                index.entry(key).or_default().push(i);
            }
        }
        let build_cols: Vec<String> = build_rows
            .first()
            .map(|r| r.fields.keys().cloned().collect())
            .unwrap_or_default();
        let probe_cols: Vec<String> = probe_rows
            .first()
            .map(|r| r.fields.keys().cloned().collect())
            .unwrap_or_default();

        let mut out: Vec<DnaQlRow> = Vec::new();
        let mut matched_build: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for l in &probe_rows {
            let key = join_key(l.fields.get(probe_col));
            let matches: &[usize] = match &key {
                Some(k) => index.get(k).map(|v| v.as_slice()).unwrap_or(&[]),
                None => &[],
            };
            if matches.is_empty() {
                if kind == JoinKind::Left || kind == JoinKind::Full {
                    let mut row = l.fields.clone();
                    for c in &build_cols {
                        row.entry(c.clone()).or_insert(Value::Null);
                    }
                    out.push(DnaQlRow { fields: row });
                }
                continue;
            }
            for &bi in matches {
                matched_build.insert(bi);
                let r = &build_rows[bi];
                let mut merged = l.fields.clone();
                for (k, val) in r.fields.iter() {
                    let name = if merged.contains_key(k) {
                        format!("joined_{}", k)
                    } else {
                        k.clone()
                    };
                    merged.insert(name, val.clone());
                }
                out.push(DnaQlRow { fields: merged });
            }
        }

        if kind == JoinKind::Full {
            for (i, r) in build_rows.iter().enumerate() {
                if matched_build.contains(&i) {
                    continue;
                }
                let mut row = r.fields.clone();
                for c in &probe_cols {
                    row.entry(c.clone()).or_insert(Value::Null);
                }
                out.push(DnaQlRow { fields: row });
            }
        }
        Ok(out)
    }

    /// `|> GROUP [BY cols] AGG name = FN(col)` — aggregation mirroring the
    /// SQL engine's `aggregate_rows` (NULLs skipped; Int sum stays Int when
    /// no Float was seen; AVG always Float; SUM of an all-NULL group is
    /// NULL; empty input with no GROUP BY yields one row, COUNT = 0).
    fn apply_group(
        &self,
        by: &[String],
        aggs: &[(String, AggFunc, Option<String>)],
        rows: &mut Vec<DnaQlRow>,
        plan: &mut Vec<String>,
    ) -> Result<(), String> {
        plan.push(format!(
            "Aggregate(group_by={:?}, aggs={:?})",
            by,
            aggs.iter().map(|(n, f, a)| format!("{}={}{:?}", n, f.as_str(), a)).collect::<Vec<_>>()
        ));
        struct GroupState {
            sample: BTreeMap<String, Value>,
            counts: Vec<u64>,
            non_null: Vec<u64>,
            sums: Vec<f64>,
            saw_float: Vec<bool>,
            mins: Vec<Option<Value>>,
            maxs: Vec<Option<Value>>,
        }
        let n_agg = aggs.len();
        let mut groups: BTreeMap<Vec<u8>, GroupState> = BTreeMap::new();
        for row in rows.iter() {
            let mut key: Vec<u8> = Vec::new();
            for col in by {
                let v = row.fields.get(col).cloned().unwrap_or(Value::Null);
                key.extend_from_slice(&crate::relational::encode_index_value(&v));
                key.push(0);
            }
            let state = groups.entry(key).or_insert_with(|| GroupState {
                sample: {
                    let mut m = BTreeMap::new();
                    for col in by {
                        m.insert(col.clone(), row.fields.get(col).cloned().unwrap_or(Value::Null));
                    }
                    m
                },
                counts: vec![0; n_agg],
                non_null: vec![0; n_agg],
                sums: vec![0.0; n_agg],
                saw_float: vec![false; n_agg],
                mins: vec![None; n_agg],
                maxs: vec![None; n_agg],
            });
            for (i, (_, func, arg)) in aggs.iter().enumerate() {
                state.counts[i] += 1;
                let value = match arg {
                    // COUNT(*): every group member counts, no column read.
                    None => {
                        state.non_null[i] += 1;
                        None
                    }
                    Some(col) => row.fields.get(col),
                };
                if let Some(v) = value {
                    if !v.is_null() {
                        state.non_null[i] += 1;
                        match (func, v) {
                            (AggFunc::Sum | AggFunc::Avg, Value::Int(n)) => {
                                state.sums[i] += *n as f64;
                            }
                            (AggFunc::Sum | AggFunc::Avg, Value::Float(f)) => {
                                state.sums[i] += *f;
                                state.saw_float[i] = true;
                            }
                            (AggFunc::Min, val) => {
                                let cur = state.mins[i].take();
                                state.mins[i] = Some(match cur {
                                    Some(c) if c.compare(val) == std::cmp::Ordering::Less => c,
                                    _ => val.clone(),
                                });
                            }
                            (AggFunc::Max, val) => {
                                let cur = state.maxs[i].take();
                                state.maxs[i] = Some(match cur {
                                    Some(c) if c.compare(val) == std::cmp::Ordering::Greater => c,
                                    _ => val.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // No GROUP BY over an empty set still yields one row (COUNT = 0).
        if groups.is_empty() && by.is_empty() {
            groups.insert(
                Vec::new(),
                GroupState {
                    sample: BTreeMap::new(),
                    counts: vec![0; n_agg],
                    non_null: vec![0; n_agg],
                    sums: vec![0.0; n_agg],
                    saw_float: vec![false; n_agg],
                    mins: vec![None; n_agg],
                    maxs: vec![None; n_agg],
                },
            );
        }

        let mut out = Vec::with_capacity(groups.len());
        for (_, state) in groups {
            let mut fields = state.sample;
            for (i, (name, func, _)) in aggs.iter().enumerate() {
                let v = match func {
                    AggFunc::Count => Value::Int(state.non_null[i] as i64),
                    AggFunc::Sum => {
                        if state.non_null[i] == 0 {
                            Value::Null
                        } else if state.saw_float[i] {
                            Value::Float(state.sums[i])
                        } else {
                            Value::Int(state.sums[i] as i64)
                        }
                    }
                    AggFunc::Avg => {
                        if state.non_null[i] == 0 {
                            Value::Null
                        } else {
                            Value::Float(state.sums[i] / state.non_null[i] as f64)
                        }
                    }
                    AggFunc::Min => state.mins[i].clone().unwrap_or(Value::Null),
                    AggFunc::Max => state.maxs[i].clone().unwrap_or(Value::Null),
                };
                fields.insert(name.clone(), v);
            }
            out.push(DnaQlRow { fields });
        }
        *rows = out;
        Ok(())
    }

    /// Execute a read-only query and return only its plan note (EXPLAIN).
    fn exec_readonly(&self, ast: &DnaQlAst) -> Result<(Vec<DnaQlRow>, String), String> {
        match ast {
            DnaQlAst::PipelineQuery { source, stages } => {
                let (mut rows, mut plan) = self.scan_rows(source, "SeqScan")?;
                for stage in stages {
                    self.apply_stage(stage, &mut rows, &mut plan)?;
                }
                Ok((Vec::new(), format!("real pipeline: {}", plan.join(" -> "))))
            }
            DnaQlAst::PatternSearch { .. } => {
                // Pattern search runs its own executor; reuse it read-only.
                let res = self.exec_pattern_search_ast(ast)?;
                Ok((Vec::new(), res.1))
            }
            other => Err(format!(
                "DnaQL: EXPLAIN supports read-only queries, got {:?}",
                ast_summary(other)
            )),
        }
    }

    /// Re-execute a PatternSearch AST node through the public executor.
    fn exec_pattern_search_ast(&self, ast: &DnaQlAst) -> Result<(Vec<DnaQlRow>, String), String> {
        if let DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            projection,
        } = ast
        {
            self.exec_pattern_search(
                chromosome,
                pattern,
                mode.clone(),
                *max_distance,
                link.clone(),
                projection.clone(),
            )
        } else {
            Err("DnaQL: internal error: not a pattern search".into())
        }
    }

    /// `Pipeline UNION [ALL] Pipeline [SORT/LIMIT/OFFSET tail]` — mirrors
    /// the SQL engine's exec_union: output field names come from the left
    /// side, DISTINCT (default) dedups full rows, tail stages apply to the
    /// compound result (OFFSET before LIMIT).
    fn exec_union(
        &self,
        left: &DnaQlAst,
        right: &DnaQlAst,
        all: bool,
        tail: &[PipelineStage],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let mut lrows = self.exec_readonly_rows(left)?;
        let rrows = self.exec_readonly_rows(right)?;
        lrows.extend(rrows);
        if !all {
            let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            lrows.retain(|row| {
                let mut key = Vec::new();
                for (k, v) in row.fields.iter() {
                    key.extend_from_slice(k.as_bytes());
                    key.push(0);
                    key.extend_from_slice(&crate::relational::encode_index_value(v));
                    key.push(0);
                }
                seen.insert(key)
            });
        }
        // Tail stages: SORT / LIMIT / OFFSET applied to the compound result.
        let mut plan = format!(
            "Union({})",
            if all { "ALL" } else { "DISTINCT" }
        );
        let mut rows = lrows;
        for stage in tail {
            let mut p = Vec::new();
            self.apply_stage(stage, &mut rows, &mut p)?;
            plan.push_str(&format!(" -> {}", p.join(" -> ")));
        }
        let n = rows.len();
        Ok((
            rows,
            format!(
                "real pipeline: {} ({} row(s) after UNION {})",
                plan,
                n,
                if all { "ALL" } else { "DISTINCT" }
            ),
        ))
    }

    /// Full row materialisation for UNION sides (rows + empty note).
    fn exec_readonly_rows(&self, ast: &DnaQlAst) -> Result<Vec<DnaQlRow>, String> {
        match ast {
            DnaQlAst::PipelineQuery { source, stages } => {
                let (mut rows, mut plan) = self.scan_rows(source, "SeqScan")?;
                for stage in stages {
                    self.apply_stage(stage, &mut rows, &mut plan)?;
                }
                Ok(rows)
            }
            DnaQlAst::PatternSearch { .. } => Ok(self.exec_pattern_search_ast(ast)?.0),
            other => Err(format!(
                "DnaQL: UNION sides must be FROM-pipelines or FIND-searches, got {:?}",
                ast_summary(other)
            )),
        }
    }

    fn exec_tx(&self, statements: &[TxStmt]) -> Result<(Vec<DnaQlRow>, String), String> {
        // 1. Buffer mutations into a transaction session. Reads go THROUGH
        // the session (read-your-writes: a statement sees earlier statements'
        // buffered effects) and record Serializable validation baselines.
        let mut session = self.tx.begin(crate::tx::TxIsolationLevel::Serializable);
        let mut skipped = 0usize;
        let mut skipped_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut updated_keys: Vec<String> = Vec::new();
        let mut deleted_keys: Vec<String> = Vec::new();
        let mut cascaded = 0usize;

        /// All candidate keys of one chromosome: committed storage rows plus
        /// rows buffered earlier in this same transaction (read-your-writes).
        fn candidate_keys(
            engine: &Engine,
            session: &crate::tx::TransactionSession,
            table: &str,
        ) -> Result<Vec<String>, String> {
            let prefix = format!("{}:", table);
            let mut keys: Vec<String> = engine
                .scan_prefix(prefix.as_bytes())
                .map_err(|e| format!("DnaQL storage error: {}", e))?
                .into_iter()
                .map(|(k, _)| String::from_utf8_lossy(&k).to_string())
                .collect();
            for k in session.pending_keys_with_prefix(prefix.as_bytes()) {
                let ks = String::from_utf8_lossy(&k).to_string();
                if !keys.contains(&ks) {
                    keys.push(ks);
                }
            }
            keys.sort();
            keys.dedup();
            Ok(keys)
        }

        for stmt in statements {
            match stmt {
                TxStmt::Mutate {
                    table,
                    locus,
                    field,
                    op,
                    value,
                } => {
                    let key = format!("{}:{}", table, locus);
                    let existing = session.read_row(&key)?;
                    let mut row = existing.unwrap_or_default();
                    let old = row.get(field).cloned().unwrap_or(Value::Null);
                    let new_val = apply_assign(&old, *op, value)?;
                    row.insert(field.clone(), new_val);
                    session.put_row(key.into_bytes(), row);
                }
                TxStmt::Append {
                    table,
                    locus,
                    fields,
                } => {
                    let key = format!("{}:{}", table, locus);
                    let existing = session.read_row(&key)?;
                    let mut row = existing.unwrap_or_default();
                    for (k, v) in fields {
                        row.insert(k.clone(), v.clone());
                    }
                    session.put_row(key.into_bytes(), row);
                }
                TxStmt::InsertIfAbsent {
                    table,
                    locus,
                    fields,
                } => {
                    let key = format!("{}:{}", table, locus);
                    if session.read_row(&key)?.is_some() {
                        // Locus already present: ON CONFLICT DO NOTHING —
                        // the existing row stays untouched.
                        skipped += 1;
                        skipped_keys.insert(key);
                        continue;
                    }
                    let mut row = BTreeMap::new();
                    for (k, v) in fields {
                        row.insert(k.clone(), v.clone());
                    }
                    session.put_row(key.into_bytes(), row);
                }
                TxStmt::DropLocus {
                    table,
                    locus,
                    cascade: _,
                } => {
                    let key = format!("{}:{}", table, locus);
                    session.delete(key.into_bytes());
                }
                TxStmt::UpdateRows {
                    table,
                    assignments,
                    where_clause,
                } => {
                    // Validate assignments against the schema when present.
                    if let Some(schema) = self.sql.get_schema(table) {
                        for (col, _) in assignments {
                            if !schema.columns.contains_key(col) {
                                return Err(format!(
                                    "DnaQL: unknown column '{}' in UPDATE of '{}'",
                                    col, table
                                ));
                            }
                        }
                    }
                    let keys = candidate_keys(&self.engine, &session, table)?;
                    for key in keys {
                        let fields = match session.read_row(&key)? {
                            Some(f) => f,
                            None => continue,
                        };
                        if let Some(e) = where_clause {
                            if !e.eval_bool(&fields) {
                                continue;
                            }
                        }
                        let mut row = fields;
                        for (col, v) in assignments {
                            row.insert(col.clone(), v.clone());
                        }
                        session.put_row(key.clone().into_bytes(), row);
                        updated_keys.push(key);
                    }
                }
                TxStmt::DeleteRows {
                    table,
                    where_clause,
                } => {
                    let schema = self
                        .sql
                        .get_schema(table)
                        .ok_or_else(|| format!("DnaQL: table '{}' not registered", table))?;
                    let keys = candidate_keys(&self.engine, &session, table)?;
                    let mut selected: Vec<(String, Value)> = Vec::new();
                    for key in keys {
                        let fields = match session.read_row(&key)? {
                            Some(f) => f,
                            None => continue,
                        };
                        if let Some(e) = where_clause {
                            if !e.eval_bool(&fields) {
                                continue;
                            }
                        }
                        let pk = fields
                            .get(&schema.primary_locus_column)
                            .cloned()
                            .ok_or_else(|| {
                                format!(
                                    "DnaQL: row {} is missing the primary key column '{}'",
                                    key, schema.primary_locus_column
                                )
                            })?;
                        selected.push((key, pk));
                    }
                    // FK handling, mirroring the SQL engine: RESTRICT refuses
                    // when a referencing row is not itself being deleted;
                    // CASCADE removes the child row (one level, like SQL).
                    let selected_keys: std::collections::HashSet<String> =
                        selected.iter().map(|(k, _)| k.clone()).collect();
                    let referencing: Vec<(String, crate::relational::ForeignKeyConstraint)> = {
                        let mut refs = Vec::new();
                        for t in self.sql.list_tables() {
                            if let Some(ts) = self.sql.get_schema(&t) {
                                for fk in &ts.foreign_keys {
                                    if fk.target_chromosome == *table {
                                        refs.push((t.clone(), fk.clone()));
                                    }
                                }
                            }
                        }
                        refs
                    };
                    let mut deletions: Vec<String> = selected
                        .iter()
                        .map(|(k, _)| k.clone())
                        .collect();
                    for (_, parent_pk) in &selected {
                        for (child_table, fk) in &referencing {
                            let child_keys =
                                candidate_keys(&self.engine, &session, child_table)?;
                            for child_key in child_keys {
                                if selected_keys.contains(&child_key)
                                    || deletions.contains(&child_key)
                                {
                                    continue;
                                }
                                let child_fields = match session.read_row(&child_key)? {
                                    Some(f) => f,
                                    None => continue,
                                };
                                let refers = child_fields
                                    .get(&fk.column)
                                    .map(|v| v.compare(parent_pk) == std::cmp::Ordering::Equal)
                                    .unwrap_or(false);
                                if !refers {
                                    continue;
                                }
                                if fk.on_delete_cascade {
                                    deletions.push(child_key);
                                } else {
                                    return Err(format!(
                                        "DnaQL: RESTRICT violation: {} row {} is referenced by {}",
                                        table, parent_pk, child_table
                                    ));
                                }
                            }
                        }
                    }
                    cascaded += deletions.len().saturating_sub(selected.len());
                    for key in &deletions {
                        session.delete(key.clone().into_bytes());
                        deleted_keys.push(key.clone());
                    }
                }
            }
        }

        // 2. Atomic commit: baseline validation (Serializable) + one WAL batch
        // frame, one fsync. Conflicts surface as retryable errors here.
        let lsn = self.tx.commit(&mut session)?;

        // 3. Report resulting rows
        let mut out = Vec::new();
        for stmt in statements {
            match stmt {
                TxStmt::Mutate { table, locus, .. } | TxStmt::Append { table, locus, .. } => {
                    let key = format!("{}:{}", table, locus);
                    let mut fields = self.read_row(&key)?.unwrap_or_default();
                    fields.insert("locus".to_string(), Value::Text(key.clone()));
                    out.push(DnaQlRow { fields });
                }
                TxStmt::InsertIfAbsent { table, locus, .. } => {
                    let key = format!("{}:{}", table, locus);
                    let mut fields = self.read_row(&key)?.unwrap_or_default();
                    let inserted = !skipped_keys.contains(&key);
                    fields.insert("locus".to_string(), Value::Text(key.clone()));
                    fields.insert(
                        "inserted".to_string(),
                        Value::Int(inserted as i64),
                    );
                    out.push(DnaQlRow { fields });
                }
                TxStmt::DropLocus { table, locus, .. } => {
                    let mut f = BTreeMap::new();
                    f.insert(
                        "dropped".to_string(),
                        Value::Text(format!("{}:{}", table, locus)),
                    );
                    out.push(DnaQlRow { fields: f });
                }
                TxStmt::UpdateRows { .. } => {
                    for key in &updated_keys {
                        let mut fields = self.read_row(key)?.unwrap_or_default();
                        fields.insert("locus".to_string(), Value::Text(key.clone()));
                        out.push(DnaQlRow { fields });
                    }
                }
                TxStmt::DeleteRows { .. } => {
                    for key in &deleted_keys {
                        let mut f = BTreeMap::new();
                        f.insert("dropped".to_string(), Value::Text(key.clone()));
                        out.push(DnaQlRow { fields: f });
                    }
                }
            }
        }

        let skipped_note = if skipped > 0 {
            format!(", {} conflict(s) skipped (DO NOTHING)", skipped)
        } else {
            String::new()
        };
        let cascade_note = if cascaded > 0 {
            format!(", {} row(s) cascaded", cascaded)
        } else {
            String::new()
        };
        Ok((
            out,
            format!(
                "real TX: {} statements committed atomically at LSN {}{}{}",
                statements.len(),
                lsn,
                skipped_note,
                cascade_note
            ),
        ))
    }

    fn read_row(&self, key: &str) -> Result<Option<BTreeMap<String, Value>>, String> {
        match self
            .engine
            .get(key.as_bytes())
            .map_err(|e| format!("storage: {}", e))?
        {
            Some(bytes) => {
                let fields = RowCodec::decode(&bytes)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    /// VERIFY / REPAIR CHROMOSOME (P5.3): block-level integrity scan of
    /// every segment that holds keys of the chromosome.
    fn exec_health_check(
        &self,
        chromosome: &str,
        fix: bool,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let prefix = format!("{}:", chromosome);
        let segments = self
            .engine
            .segments_with_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;
        if segments.is_empty() {
            return Ok((
                Vec::new(),
                format!("chromosome '{}' has no stored segments", chromosome),
            ));
        }
        let (blocks, lesions, repaired, unrecoverable) = self
            .engine
            .verify_segments(&segments, fix)
            .map_err(|e| format!("DnaQL verify error: {}", e))?;

        let mut rows = Vec::new();
        for seg in &segments {
            let mut row = std::collections::BTreeMap::new();
            row.insert(
                "segment".to_string(),
                Value::Text(
                    seg.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_string(),
                ),
            );
            row.insert("format".to_string(), Value::Text("DNASST02".into()));
            rows.push(DnaQlRow { fields: row });
        }
        let note = format!(
            "{} chromosome '{}': {} segment(s), {} blocks scanned, {} lesion(s), {} repaired, {} unrecoverable stripe(s){}",
            if fix { "REPAIR" } else { "VERIFY" },
            chromosome,
            segments.len(),
            blocks,
            lesions,
            repaired,
            unrecoverable,
            if !fix && lesions > 0 {
                " (run REPAIR CHROMOSOME to fix)"
            } else {
                ""
            }
        );
        // Attach summary as the final row for tabular consumers.
        let mut summary = std::collections::BTreeMap::new();
        summary.insert("segment".to_string(), Value::Text("SUMMARY".into()));
        summary.insert("blocks_scanned".to_string(), Value::Int(blocks as i64));
        summary.insert("lesions_found".to_string(), Value::Int(lesions as i64));
        summary.insert("lesions_repaired".to_string(), Value::Int(repaired as i64));
        summary.insert(
            "unrecoverable_stripes".to_string(),
            Value::Int(unrecoverable as i64),
        );
        rows.push(DnaQlRow { fields: summary });
        Ok((rows, note))
    }

    fn exec_bio(
        &self,
        sequence: Option<String>,
        locus: Option<(String, i64)>,
        functions: &[String],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        // v4.1: STORAGE_MAP() — the per-column storage map of one locus.
        // Works on ANY row (no sequence field required) and decodes every
        // value back to the human-readable form — the DnaQL counterpart of
        // the SQL statement SHOW STORAGE ... WHERE <pk> = <literal>.
        if functions.iter().any(|f| f == "STORAGE_MAP") {
            if functions.len() != 1 {
                return Err(
                    "DnaQL: STORAGE_MAP() must be the only pipeline function".into(),
                );
            }
            let Some((table, id)) = locus else {
                return Err(
                    "DnaQL: STORAGE_MAP() needs ANALYZE LOCUS <table>:<locus>".into(),
                );
            };
            return self.exec_storage_map(&table, id);
        }
        let seq_str = match (sequence, locus) {
            (Some(s), _) => s,
            (None, Some((table, id))) => {
                let key = format!("{}:{}", table, id);
                let row = self
                    .read_row(&key)?
                    .ok_or_else(|| format!("DnaQL: locus {} not found", key))?;
                match row.get("sequence") {
                    Some(Value::Dna(s)) | Some(Value::Text(s)) => s.clone(),
                    _ => return Err(format!("DnaQL: locus {} has no sequence field", key)),
                }
            }
            (None, None) => return Err("DnaQL: ANALYZE requires SEQUENCE or LOCUS".into()),
        };
        let packed = PackedDna::from_ascii(&seq_str)
            .map_err(|e| format!("DnaQL: invalid sequence: {}", e))?;

        let mut out = Vec::new();
        for fname in functions {
            let mut fields = BTreeMap::new();
            fields.insert("function".to_string(), Value::Text(fname.clone()));
            match fname.as_str() {
                "GC_CONTENT" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Text(format!("{:.2}%", packed.gc_content() * 100.0)),
                    );
                }
                "MELTING_TEMP" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Text(format!("{:.1} C", packed.melting_temp())),
                    );
                }
                "REV_COMP" => {
                    fields.insert(
                        "result".to_string(),
                        Value::Dna(packed.reverse_complement().to_ascii()),
                    );
                }
                "TRANSCRIBE" => {
                    fields.insert("result".to_string(), Value::Text(packed.transcribe()));
                }
                "TRANSLATE" => {
                    fields.insert("result".to_string(), Value::Text(packed.translate()));
                }
                other => return Err(format!("DnaQL: unknown bio function '{}'", other)),
            }
            out.push(DnaQlRow { fields });
        }
        Ok((out, format!("real bio-analysis of {} bp", packed.len)))
    }

    /// v4.1: ANALYZE LOCUS <table>:<id> |> STORAGE_MAP() — per-column map of
    /// one stored row: chromosome, locus, exact storage key, SQL type, the
    /// decoded human-readable value and the physical RowCodec representation.
    /// Same report as SQL `SHOW STORAGE ... WHERE <pk> = <literal>`.
    fn exec_storage_map(
        &self,
        table: &str,
        id: i64,
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let key = format!("{}:{}", table, id);
        let row = self
            .read_row(&key)?
            .ok_or_else(|| format!("DnaQL: locus {} not found", key))?;
        let schema = self.sql.get_schema(table);
        let mut out = Vec::new();
        let mut payload_bytes = 0usize;
        for (fname, value) in row {
            // approximate the per-field wire size the way RowCodec does:
            // 4 B key length + key + 1 B tag + payload
            payload_bytes += 4 + fname.len() + 1
                + match &value {
                    Value::Null => 0,
                    Value::Int(_) | Value::Float(_) => 8,
                    Value::Text(s) | Value::Dna(s) => 4 + s.len(),
                };
            let mut fields = BTreeMap::new();
            fields.insert(
                "function".to_string(),
                Value::Text("STORAGE_MAP".to_string()),
            );
            fields.insert(
                "chromosome".to_string(),
                Value::Text(table.to_string()),
            );
            fields.insert("locus".to_string(), Value::Int(id));
            fields.insert("storage_key".to_string(), Value::Text(key.clone()));
            fields.insert("column".to_string(), Value::Text(fname.clone()));
            fields.insert(
                "sql_type".to_string(),
                Value::Text(match &schema {
                    Some(s) => s.column_type_name(&fname),
                    None => value.type_name().to_string(),
                }),
            );
            fields.insert("value".to_string(), Value::Text(value.to_string()));
            fields.insert(
                "stored_as".to_string(),
                Value::Text(value.stored_as()),
            );
            out.push(DnaQlRow { fields });
        }
        let note = format!(
            "storage map of locus {}:{} ({} column(s), ~{} B payload); values are \
             decoded for the operator, stored_as describes the physical bytes",
            table, id, out.len(), payload_bytes
        );
        Ok((out, note))
    }
}

fn apply_assign(old: &Value, op: AssignOp, v: &Value) -> Result<Value, String> {
    match op {
        AssignOp::Set => Ok(v.clone()),
        AssignOp::Add => match (old, v) {
            // Checked arithmetic: TX mutations are durable, so a silently
            // wrapped i64 (e.g. i64::MAX + 1 becoming negative) would
            // corrupt persisted balances/counts. Overflow aborts the
            // mutation instead.
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(*b)
                .map(Value::Int)
                .ok_or_else(|| "DnaQL: integer overflow in +=".to_string()),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            _ => Err("DnaQL: += requires numeric operands".into()),
        },
        AssignOp::Sub => match (old, v) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_sub(*b)
                .map(Value::Int)
                .ok_or_else(|| "DnaQL: integer overflow in -=".to_string()),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 - b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a - *b as f64)),
            _ => Err("DnaQL: -= requires numeric operands".into()),
        },
    }
}

fn ast_summary(ast: &DnaQlAst) -> String {
    match ast {
        DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            ..
        } => {
            format!(
                "PatternSearch {{ chr: '{}', pattern: '{}', mode: {:?}, k: {}, link: {} }}",
                chromosome,
                pattern,
                mode,
                max_distance,
                link.as_ref()
                    .map(|l| format!("{}({})", l.chromosome, l.column))
                    .unwrap_or_else(|| "none".into())
            )
        }
        DnaQlAst::PipelineQuery { source, stages } => {
            format!(
                "PipelineQuery {{ source: '{}', stages: {} }}",
                source,
                stages.len()
            )
        }
        DnaQlAst::Tx { statements } => {
            format!("Tx {{ statements: {} }}", statements.len())
        }
        DnaQlAst::BioAnalysis { functions, .. } => {
            format!("BioAnalysis {{ functions: {:?} }}", functions)
        }
        DnaQlAst::HealthCheck { chromosome, fix } => {
            format!("HealthCheck {{ chr: '{}', fix: {} }}", chromosome, fix)
        }
        DnaQlAst::CreateChromosome {
            table,
            definitions,
            if_not_exists,
        } => {
            format!(
                "CreateChromosome {{ table: '{}', definitions: {}, if_not_exists: {} }}",
                table,
                definitions.len(),
                if_not_exists
            )
        }
        DnaQlAst::DropChromosome { table, if_exists } => {
            format!(
                "DropChromosome {{ table: '{}', if_exists: {} }}",
                table, if_exists
            )
        }
        DnaQlAst::AlterChromosome {
            table,
            column,
            col_type,
        } => format!(
            "AlterChromosome {{ table: '{}', column: '{}', type: '{}' }}",
            table, column, col_type
        ),
        DnaQlAst::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        } => format!(
            "CreateIndex {{ name: '{}', table: '{}', column: '{}', unique: {}, if_not_exists: {} }}",
            name, table, column, unique, if_not_exists
        ),
        DnaQlAst::DropIndex {
            name,
            table,
            if_exists,
        } => format!(
            "DropIndex {{ name: '{}', table: {:?}, if_exists: {} }}",
            name, table, if_exists
        ),
        DnaQlAst::ShowChromosomes => "ShowChromosomes".to_string(),
        DnaQlAst::Explain(inner) => format!("Explain {{ {} }}", ast_summary(inner)),
        DnaQlAst::Union { left, right, all, tail } => format!(
            "Union {{ all: {}, tail: {}, left: ({}), right: ({}) }}",
            all,
            tail.len(),
            ast_summary(left),
            ast_summary(right)
        ),
    }
}

// re-export for SQL engine reuse
pub use expr::Expr as DnaQlExpr;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_ql_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup() -> (Arc<Engine>, DnaQlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("ql"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: crate::wal::SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();

        // chr_variants: loci with sequences
        let variants = vec![
            (1, "BRCA1_exon2", "ATGCGATCGATCGACCATGCGATCGATCATGCGATCAATC"),
            (
                2,
                "TP53_promoter",
                "GGCCTTAAGGCCTTAACCGGATGCATCGAATTCGAGCTAG",
            ),
            (
                3,
                "CYP2D6_poly",
                "TTAACCGGATCGGATCCGTAAGCTTCGGATCCAAGCTTACG",
            ),
        ];
        for (id, name, seq) in variants {
            let mut row = BTreeMap::new();
            row.insert("variant_id".to_string(), Value::Int(id));
            row.insert("variant_name".to_string(), Value::Text(name.to_string()));
            row.insert("sequence".to_string(), Value::Dna(seq.to_string()));
            row.insert("patient_id".to_string(), Value::Int(id * 10));
            let key = format!("chr_variants:{}", id);
            let payload = RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            engine.put(key.as_bytes(), &payload).unwrap();
        }

        // chr_patients: link targets
        for i in 10..40i64 {
            let mut row = BTreeMap::new();
            row.insert("patient_id".to_string(), Value::Int(i));
            row.insert("name".to_string(), Value::Text(format!("patient_{}", i)));
            let key = format!("chr_patients:{}", i);
            let payload = RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            engine.put(key.as_bytes(), &payload).unwrap();
        }

        let sql = Arc::new(SqlEngine::new(engine.clone()));
        let ql = DnaQlEngine::new(engine.clone(), sql);
        (engine, ql)
    }

    #[test]
    fn quick_search_finds_exact_and_fuzzy() {
        let (_e, ql) = setup();
        // exact match: GATCGATC appears in variant 1 sequence
        let res = ql.execute("chr_variants ? 'GATCGATC' ~0").unwrap();
        assert!(
            res.rows.len() >= 1,
            "expected matches, got {:?}",
            res.ast_summary
        );
        assert!(res.engine_note.contains("real scan"));

        // with 1 mismatch, should find MORE or equal
        let res2 = ql.execute("chr_variants ? 'GATCGATC' ~1").unwrap();
        assert!(res2.rows.len() >= res.rows.len());

        // Hamming mode
        let res3 = ql.execute("chr_variants ? 'GATCGATC' ~1").unwrap();
        assert!(!res3.rows.is_empty());
    }

    // ---------- v4.1: STORAGE_MAP ----------

    #[test]
    fn storage_map_decodes_locus_without_sequence() {
        let (_e, ql) = setup();
        // chr_patients rows have NO sequence field — STORAGE_MAP must still
        // produce the full per-column map (unlike the sequence bio functions).
        let res = ql
            .execute("ANALYZE LOCUS chr_patients:12 |> STORAGE_MAP();")
            .unwrap();
        assert_eq!(res.rows.len(), 2, "{:?}", res.engine_note);
        let by_col = |name: &str| {
            res.rows
                .iter()
                .find(|r| r.fields.get("column") == Some(&Value::Text(name.into())))
                .unwrap()
        };
        assert_eq!(
            by_col("patient_id").fields.get("value"),
            Some(&Value::Text("12".into()))
        );
        assert_eq!(
            by_col("patient_id").fields.get("sql_type"),
            Some(&Value::Text("INT".into()))
        );
        assert_eq!(
            by_col("patient_id").fields.get("storage_key"),
            Some(&Value::Text("chr_patients:12".into()))
        );
        assert!(matches!(
            by_col("patient_id").fields.get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 1")
        ));
        assert_eq!(
            by_col("name").fields.get("value"),
            Some(&Value::Text("patient_12".into()))
        );
        assert!(res.engine_note.contains("storage map of locus chr_patients:12"));
    }

    #[test]
    fn storage_map_shows_dna_chain_and_rejects_misuse() {
        let (_e, ql) = setup();
        let res = ql
            .execute("ANALYZE LOCUS chr_variants:1 |> STORAGE_MAP();")
            .unwrap();
        let seq_row = res
            .rows
            .iter()
            .find(|r| r.fields.get("column") == Some(&Value::Text("sequence".into())))
            .unwrap();
        assert_eq!(
            seq_row.fields.get("value"),
            Some(&Value::Text("ATGCGATCGATCGACCATGCGATCGATCATGCGATCAATC".into()))
        );
        assert!(matches!(
            seq_row.fields.get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 4")
        ));

        // Needs a LOCUS, not a literal SEQUENCE.
        assert!(ql
            .execute("ANALYZE SEQUENCE 'ACGT' |> STORAGE_MAP();")
            .is_err());
        // Cannot be combined with the sequence functions.
        assert!(ql
            .execute("ANALYZE LOCUS chr_variants:1 |> STORAGE_MAP() |> GC_CONTENT();")
            .is_err());
        // Unknown locus is a clean error.
        assert!(ql.execute("ANALYZE LOCUS chr_variants:999 |> STORAGE_MAP();").is_err());
    }

    #[test]
    fn standard_search_with_link() {
        let (_e, ql) = setup();
        let q = "FIND PATTERN 'ATGCGATCGATC' \
                 IN CHROMOSOME chr_variants \
                 WHERE HAMMING_DISTANCE <= 2 \
                 LINK chr_patients VIA COMPLEMENT(patient_id) \
                 EMIT locus, offset_bp, mismatches, name";
        let res = ql.execute(q).unwrap();
        assert!(!res.rows.is_empty());
        // linked patient name present
        let has_link = res.rows.iter().any(|r| r.fields.contains_key("name"));
        assert!(
            has_link,
            "link fields missing: {:?}",
            res.rows[0].fields.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn pipeline_where_and_emit() {
        let (_e, ql) = setup();
        let q = "FROM chr_variants |> WHERE variant_id >= 2 |> EMIT variant_id, variant_name";
        let res = ql.execute(q).unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(res.engine_note.contains("Filter"));
        // projection applied
        for r in &res.rows {
            assert!(r.fields.contains_key("variant_id"));
            assert!(!r.fields.contains_key("sequence"));
        }
    }

    #[test]
    fn tx_block_mutates_atomically() {
        let (e, ql) = setup();
        // seed a balance row
        let mut row = BTreeMap::new();
        row.insert("balance".to_string(), Value::Float(1000.0));
        let payload = RowCodec::encode(&[("balance".to_string(), Value::Float(1000.0))]);
        e.put(b"chr_accounts:14", &payload).unwrap();

        let q = "TX { \
                   chr_accounts:14.balance -= 350.00; \
                   chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 }; \
                 }";
        let res = ql.execute(q).unwrap();
        assert!(
            res.engine_note.contains("atomically"),
            "{}",
            res.engine_note
        );

        // balance updated
        let raw = e.get(b"chr_accounts:14").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        let bal = fields.iter().find(|(k, _)| k == "balance").unwrap();
        assert_eq!(bal.1, Value::Float(650.0));

        // order created
        let raw2 = e.get(b"chr_orders:9021").unwrap().unwrap();
        let fields2 = RowCodec::decode(&raw2).unwrap();
        assert!(fields2
            .iter()
            .any(|(k, v)| k == "order_id" && *v == Value::Int(9021)));
    }

    #[test]
    fn bio_analysis_real_values() {
        let (_e, ql) = setup();
        let q = "ANALYZE SEQUENCE 'ATGGCCTAA' |> GC_CONTENT() |> MELTING_TEMP() |> TRANSLATE()";
        let res = ql.execute(q).unwrap();
        assert_eq!(res.rows.len(), 3);
        let gc = &res.rows[0];
        assert_eq!(
            gc.fields.get("result"),
            Some(&Value::Text("44.44%".to_string()))
        );
        let tr = &res.rows[2];
        assert_eq!(
            tr.fields.get("result"),
            Some(&Value::Text("MA".to_string()))
        );
    }

    #[test]
    fn parse_errors_are_informative() {
        let (_e, ql) = setup();
        assert!(ql.execute("BOGUS STATEMENT").is_err());
        assert!(ql.execute("chr ? 'ATGCGN' ~1").is_err()); // N not allowed
        assert!(ql.execute("chr ? 'ATGC").is_err()); // unterminated
    }

    #[test]
    fn drop_locus_removes_row() {
        let (e, ql) = setup();
        let q = "TX { DROP LOCUS chr_variants:3 }";
        ql.execute(q).unwrap();
        assert!(e.get(b"chr_variants:3").unwrap().is_none());
        // survives restart
        drop(ql);
        let e2 = Engine::open(EngineConfig {
            dir_path: e.data_dir().to_path_buf(),
            ..Default::default()
        })
        .unwrap();
        assert!(e2.get(b"chr_variants:3").unwrap().is_none());
        assert!(e2.get(b"chr_variants:1").unwrap().is_some());
    }

    // ------------------------------------------------------------------
    // v2.2 — SQL parity features
    // ------------------------------------------------------------------

    /// A small relational fixture registered through SQL DDL so schemas and
    /// FKs exist: chr_users(1..3) ← chr_orders(10..13, one orphan-free set
    /// plus one user with no orders).
    fn setup_relational() -> (Arc<Engine>, DnaQlEngine) {
        let (engine, _ql) = setup();
        let sql = Arc::new(SqlEngine::new(engine.clone()));
        sql.execute(
            "CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, \
             PRIMARY KEY (user_id))",
        )
        .unwrap();
        sql.execute(
            "CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, \
             PRIMARY KEY (order_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id) \
             ON DELETE CASCADE)",
        )
        .unwrap();
        for (id, name, bal) in [
            (1, "ada", 100.0),
            (2, "grace", 250.5),
            (3, "alan", 75.0),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id, username, balance) VALUES ({}, '{}', {:?})",
                id, name, bal
            ))
            .unwrap();
        }
        for (oid, uid, total, status) in [
            (10, 1, 50.0, "paid"),
            (11, 1, 70.0, "open"),
            (12, 2, 200.0, "paid"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES \
                 ({}, {}, {:?}, '{}')",
                oid, uid, total, status
            ))
            .unwrap();
        }
        let ql = DnaQlEngine::new(engine.clone(), sql);
        (engine, ql)
    }

    #[test]
    fn not_and_in_and_is_not_null_filter_rows() {
        let (_e, ql) = setup_relational();
        let r = ql
            .execute("FROM chr_users |> WHERE NOT (balance > 80.0) |> EMIT user_id;")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "{:?}", r.rows);
        assert_eq!(
            r.rows[0].fields.get("user_id"),
            Some(&Value::Int(3)),
            "NOT keeps only user 3 (75.0)"
        );

        let r = ql
            .execute("FROM chr_users |> WHERE user_id IN (1, 3) |> EMIT user_id;")
            .unwrap();
        assert_eq!(r.rows.len(), 2);

        let r = ql
            .execute("FROM chr_users |> WHERE balance IS NOT NULL |> EMIT user_id;")
            .unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn join_stages_mirror_sql_semantics() {
        let (engine, ql) = setup_relational();
        let sql = ql.sql.clone();

        // INNER JOIN: matched rows only; conflicting user_id surfaces as
        // joined_user_id (single-namespace naming identical to SQL).
        let got = ql
            .execute(
                "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
                 |> WHERE status = 'paid' |> EMIT username, total_usd;",
            )
            .unwrap();
        let want = sql
            .execute(
                "SELECT username, total_usd FROM chr_users u \
                 INNER JOIN chr_orders o ON u.user_id = o.user_id WHERE status = 'paid'",
            )
            .unwrap();
        assert_eq!(got.rows.len(), 2);
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            for col in ["username", "total_usd"] {
                assert_eq!(w.get(col), g.fields.get(col), "INNER JOIN {}", col);
            }
        }

        // LEFT JOIN NULL-extension: user 3 has no orders.
        let got = ql
            .execute(
                "FROM chr_users |> LEFT JOIN chr_orders ON user_id = user_id \
                 |> SORT user_id ASC |> EMIT username, order_id;",
            )
            .unwrap();
        assert_eq!(got.rows.len(), 4, "3 users, 1 with two orders");
        let alan = got
            .rows
            .iter()
            .find(|r| r.fields.get("username") == Some(&Value::Text("alan".into())))
            .unwrap();
        assert_eq!(alan.fields.get("order_id"), Some(&Value::Null));

        // RIGHT JOIN == LEFT with sides swapped: orphan-free orders cover
        // users 1 and 2; user 3 drops out.
        let got = ql
            .execute(
                "FROM chr_users |> RIGHT JOIN chr_orders ON user_id = user_id |> EMIT user_id;",
            )
            .unwrap();
        assert_eq!(got.rows.len(), 3, "3 orders");

        // FULL OUTER JOIN: every user + every order (no orphans here, so
        // exactly the 4-row left-join set).
        let got = ql
            .execute(
                "FROM chr_users |> FULL JOIN chr_orders ON user_id = user_id |> EMIT username;",
            )
            .unwrap();
        assert_eq!(got.rows.len(), 4);
        let _ = engine;
    }

    #[test]
    fn group_stage_aggregates_and_having() {
        let (_e, ql) = setup_relational();
        // GROUP BY + named aggregates + HAVING (WHERE after GROUP).
        let r = ql
            .execute(
                "FROM chr_orders |> GROUP BY user_id AGG cnt = COUNT(*), total = SUM(total_usd) \
                 |> WHERE cnt >= 1 |> SORT cnt DESC |> EMIT user_id, cnt, total;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2, "two groups: user 1 (2 orders), user 2 (1)");
        let user1 = r
            .rows
            .iter()
            .find(|r| r.fields.get("user_id") == Some(&Value::Int(1)))
            .unwrap();
        assert_eq!(user1.fields.get("cnt"), Some(&Value::Int(2)));
        assert_eq!(user1.fields.get("total"), Some(&Value::Float(120.0)));

        // Whole-table aggregate (no GROUP BY): one row, COUNT = 3.
        let r = ql
            .execute("FROM chr_orders |> GROUP AGG n = COUNT(*) |> EMIT n;")
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("n"), Some(&Value::Int(3)));

        // Aggregates skip NULLs; SUM of an all-NULL group is NULL.
        let sql = ql.sql.clone();
        sql.execute(
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (13, 3, 0.0, 'nullish')",
        )
        .unwrap();
        sql.execute("UPDATE chr_orders SET total_usd = NULL WHERE order_id = 13")
            .unwrap();
        let r = ql
            .execute(
                "FROM chr_orders |> GROUP BY user_id AGG s = SUM(total_usd), c = COUNT(total_usd) \
                 |> WHERE user_id = 3 |> EMIT s, c;",
            )
            .unwrap();
        assert_eq!(r.rows[0].fields.get("s"), Some(&Value::Null));
        assert_eq!(r.rows[0].fields.get("c"), Some(&Value::Int(0)));

        // Pure GROUP BY (no AGG) = DISTINCT-style projection.
        let r = ql
            .execute("FROM chr_orders |> GROUP BY status |> EMIT status;")
            .unwrap();
        assert_eq!(r.rows.len(), 3, "paid, open, nullish");
    }

    #[test]
    fn offset_computed_emit_and_union() {
        let (_e, ql) = setup_relational();
        // OFFSET before LIMIT.
        let r = ql
            .execute(
                "FROM chr_users |> SORT user_id ASC |> OFFSET 1 |> LIMIT 2 |> EMIT user_id;",
            )
            .unwrap();
        let ids: Vec<i64> = r
            .rows
            .iter()
            .filter_map(|r| match r.fields.get("user_id") {
                Some(Value::Int(i)) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![2, 3]);

        // Computed EMIT with an alias.
        let r = ql
            .execute("FROM chr_users |> EMIT user_id, balance * 2 AS double_balance;")
            .unwrap();
        let ada = r
            .rows
            .iter()
            .find(|r| r.fields.get("user_id") == Some(&Value::Int(1)))
            .unwrap();
        assert_eq!(
            ada.fields.get("double_balance"),
            Some(&Value::Float(200.0))
        );

        // UNION dedups; UNION ALL keeps both; tail stages hit the compound.
        let r = ql
            .execute("FROM chr_users |> EMIT user_id UNION FROM chr_orders |> EMIT user_id;")
            .unwrap();
        assert_eq!(r.rows.len(), 3, "dedup: users 1,2,3");
        let r = ql
            .execute(
                "FROM chr_users |> EMIT user_id UNION ALL FROM chr_orders |> EMIT user_id;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 6, "3 users + 3 orders");

        // FIND search can be a UNION side too.
        let r = ql
            .execute("FROM chr_users |> EMIT user_id UNION ALL FROM chr_variants |> EMIT variant_id;")
            .unwrap();
        assert_eq!(r.rows.len(), 6);
    }

    #[test]
    fn show_alter_and_index_ddl() {
        let (_e, ql) = setup_relational();
        // SHOW CHROMOSOMES (and the TABLES alias).
        let r = ql.execute("SHOW CHROMOSOMES;").unwrap();
        let names: Vec<String> = r
            .rows
            .iter()
            .filter_map(|r| match r.fields.get("chromosome") {
                Some(Value::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"chr_users".to_string()));
        assert!(names.contains(&"chr_orders".to_string()));
        ql.execute("SHOW TABLES;").unwrap();

        // ALTER CHROMOSOME ADD COLUMN — metadata-only online DDL.
        ql.execute("ALTER CHROMOSOME chr_users ADD COLUMN note TEXT;")
            .unwrap();
        assert!(ql
            .sql
            .get_schema("chr_users")
            .unwrap()
            .columns
            .contains_key("note"));

        // CREATE / DROP INDEX render through the shared SQL registry.
        ql.execute("CREATE INDEX idx_users_note ON chr_users (note);")
            .unwrap();
        assert!(ql
            .sql
            .get_schema("chr_users")
            .unwrap()
            .secondary_index_on("note")
            .is_some());
        ql.execute("DROP INDEX idx_users_note ON chr_users;").unwrap();
        assert!(ql
            .sql
            .get_schema("chr_users")
            .unwrap()
            .secondary_index_on("note")
            .is_none());
    }

    #[test]
    fn explain_returns_plan_without_rows() {
        let (_e, ql) = setup_relational();
        let r = ql
            .execute("EXPLAIN FROM chr_users |> WHERE balance > 90.0 |> EMIT user_id;")
            .unwrap();
        assert!(r.rows.is_empty());
        assert!(r.engine_note.contains("Filter"), "{}", r.engine_note);
        assert!(r.engine_note.contains("SeqScan"), "{}", r.engine_note);
        // EXPLAIN of a write is rejected without executing.
        assert!(ql.execute("EXPLAIN TX { chr_users:1.balance = 0.0 }").is_err());
    }

    #[test]
    fn rowset_update_delete_and_insert_if_absent() {
        let (engine, ql) = setup_relational();
        let sql = ql.sql.clone();

        // Row-set UPDATE with a broad WHERE.
        let r = ql
            .execute("UPDATE chr_users SET balance = 0.0 WHERE balance < 90.0;")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "only user 3 (75.0)");
        let raw = engine.get(b"chr_users:3").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(0.0)));

        // UPDATE without WHERE touches every row.
        ql.execute("UPDATE chr_users SET status_extra = 'x';").is_err(); // unknown column
        let r = ql.execute("UPDATE chr_users SET balance = 1.5;").unwrap();
        assert_eq!(r.rows.len(), 3);

        // ?= insert-if-absent (ON CONFLICT DO NOTHING spelling).
        let r = ql
            .execute(
                "TX { chr_users:9 ?= { user_id: 9, username: 'new', balance: 10.0 } };",
            )
            .unwrap();
        assert_eq!(r.rows[0].fields.get("inserted"), Some(&Value::Int(1)));
        let r = ql
            .execute(
                "TX { chr_users:9 ?= { user_id: 9, username: 'AGAIN', balance: 99.0 } };",
            )
            .unwrap();
        assert_eq!(r.rows[0].fields.get("inserted"), Some(&Value::Int(0)));
        let raw = engine.get(b"chr_users:9").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(
            fields.iter().any(|(k, v)| k == "username" && *v == Value::Text("new".into())),
            "?= must not overwrite an existing locus"
        );

        // Row-set DELETE honors FK CASCADE (user 1's orders go too) and
        // RESTRICT errors.
        let r = ql.execute("DELETE FROM chr_users WHERE user_id = 1;").unwrap();
        assert_eq!(r.rows.len(), 3, "user 1 + 2 cascaded orders");
        assert!(engine.get(b"chr_users:1").unwrap().is_none());
        assert!(engine.get(b"chr_orders:10").unwrap().is_none());
        assert!(engine.get(b"chr_orders:11").unwrap().is_none());

        // RESTRICT: chr_users is referenced by chr_orders ON DELETE CASCADE,
        // so create a RESTRICT reference to exercise the error path.
        sql.execute(
            "CREATE TABLE chr_notes (note_id INT, user_id INT, PRIMARY KEY (note_id), \
             FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_notes (note_id, user_id) VALUES (1, 2)")
            .unwrap();
        let err = ql.execute("DELETE FROM chr_users WHERE user_id = 2;").unwrap_err();
        assert!(err.contains("RESTRICT"), "{}", err);

        // DELETE without WHERE removes everything.
        let r = ql.execute("DELETE FROM chr_orders;").unwrap();
        assert_eq!(r.rows.len(), 1, "only order 12 is left at this point");
        let _ = sql;
    }
}

#[cfg(test)]
mod acl_parse_tests {
    use super::referenced_tables;

    #[test]
    fn collects_pipeline_and_link_tables() {
        let tables = referenced_tables(
            "FROM chr_users |> LINK chr_orders VIA COMPLEMENT(user_id)",
        )
        .unwrap();
        assert_eq!(tables, vec!["chr_orders".to_string(), "chr_users".to_string()]);
    }

    #[test]
    fn collects_transaction_tables() {
        let tables = referenced_tables("TX { chr_variants += { id: 1 } }").unwrap();
        assert_eq!(tables, vec!["chr_variants".to_string()]);
    }
}

/// Regression tests for the v3.5.0 audit fixes (P0-1, P0-2, TX += overflow).
#[cfg(test)]
mod security_regression_tests {
    use super::*;

    #[test]
    fn parser_rejects_pathologically_nested_expressions() {
        let deep = format!("({}a = 1{})", "(".repeat(100_000), ")".repeat(100_000));
        let toks = tokenize(&deep).unwrap();
        let mut p = Parser::new(toks);
        let err = p
            .parse_expr()
            .err()
            .expect("pathologically deep expression must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn referenced_tables_includes_bioanalysis_locus_table() {
        // ANALYZE LOCUS reads a row of the named table, so the per-user ACL
        // must see it (previously an empty list => ACL bypass).
        let tables = referenced_tables("ANALYZE LOCUS chr_virus:42 |> GC_CONTENT()").unwrap();
        assert_eq!(tables, vec!["chr_virus".to_string()]);
        // A literal-sequence analysis touches no table at all.
        let tables = referenced_tables("ANALYZE SEQUENCE 'ACGT' |> GC_CONTENT()").unwrap();
        assert!(tables.is_empty());
    }

    #[test]
    fn tx_assign_overflow_is_rejected_not_wrapped() {
        // A durable mutation must never silently wrap i64: MAX + 1 would
        // turn a persisted counter/balance negative.
        assert!(apply_assign(&Value::Int(i64::MAX), AssignOp::Add, &Value::Int(1)).is_err());
        assert!(apply_assign(&Value::Int(i64::MIN), AssignOp::Sub, &Value::Int(1)).is_err());
        assert_eq!(
            apply_assign(&Value::Int(2), AssignOp::Add, &Value::Int(3)).unwrap(),
            Value::Int(5)
        );
        assert_eq!(
            apply_assign(&Value::Int(5), AssignOp::Sub, &Value::Int(2)).unwrap(),
            Value::Int(3)
        );
    }
}
