// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL (DNA Query Language) v2.1 — REAL tokenizer, parser and executor.
//!
//! Implemented grammar (subset of DNAQL_SPEC.md, fully functional):
//!
//!   Query      ::= PatternSearch | PipelineQuery | TxBlock | BioAnalysis
//!   QuickSearch::= Ident '?' StringLit ['~' Int | '*' Int]
//!                  ['->' Ident ['(' Ident ')']] ['=>' 'emit' Projection] [';']
//!   StdSearch  ::= 'FIND' ['PATTERN'] StringLit 'IN' ['CHROMOSOME'] Ident
//!                  ['WHERE' ('HAMMING_DISTANCE'|'LEVENSHTEIN_DISTANCE') '<=' Int]
//!                  ['LINK' Ident 'VIA' 'COMPLEMENT' '(' Ident ')']
//!                  ['EMIT' Projection] [';']
//!   Pipeline   ::= 'FROM' Ident ( '|>' Stage )*
//!   Stage      ::= 'LINK' Ident ['VIA' 'COMPLEMENT' '(' Ident ')']
//!               |  'WHERE' Expr
//!               |  'EMIT' Projection
//!   TxBlock    ::= 'TX' '{' Stmt (';' Stmt)* '}'
//!               |  'SYNTHESIZE' 'INTO' IdentList 'BEGIN' StmtList ('LIGATE'|'HYDROLYZE')
//!   Stmt       ::= Ident ':' Int '.' Ident ('='|'+='|'-=') Literal
//!               |  Ident ':' Int '+=' '{' KVList '}'
//!               |  'DROP' 'LOCUS' Ident ':' Int ['CASCADE']
//!   Bio        ::= 'ANALYZE' ('SEQUENCE' StringLit | 'LOCUS' Ident ':' Int)
//!                  ( '|>' BioFn )*
//!   BioFn      ::= 'GC_CONTENT()' | 'MELTING_TEMP()' | 'REV_COMP()'
//!               |  'TRANSCRIBE()' | 'TRANSLATE()'
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
}

#[derive(Debug, Clone)]
pub enum PipelineStage {
    Link(Option<LinkSpec>),
    Filter(Expr),
    Emit(Vec<String>),
    /// `|> SORT column [ASC|DESC]` — order the rows (was SQL-only before).
    Sort {
        column: String,
        asc: bool,
    },
    /// `|> LIMIT n` — keep the first n rows after the previous stage.
    Limit(usize),
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
    /// DROP LOCUS chr:locus [CASCADE]
    DropLocus {
        table: String,
        locus: i64,
        cascade: bool,
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
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Self { toks, pos: 0 }
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
        match self.peek().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("FIND") => self.parse_standard_search(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("FROM") => self.parse_pipeline(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TX") => self.parse_tx(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SYNTHESIZE") => self.parse_synthesize(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ANALYZE") => self.parse_bio(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("VERIFY") => self.parse_health_check(false),
            Tok::Ident(s) if s.eq_ignore_ascii_case("REPAIR") => self.parse_health_check(true),
            Tok::Ident(s) if s.eq_ignore_ascii_case("CREATE") => self.parse_create_chromosome(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("DROP") => self.parse_drop_chromosome(),
            Tok::Ident(_) => self.parse_quick_search(),
            t => Err(format!("DnaQL: unexpected token {:?}", t)),
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

    /// CREATE CHROMOSOME <name> [IF NOT EXISTS] (col TYPE, ...);
    /// The parenthesis body is collected as raw definition clauses at top
    /// comma level: "id INT", "PRIMARY KEY (id)", "FOREIGN KEY ...
    /// REFERENCES ..." with nested parens kept intact. Types and constraints
    /// are validated when the rendered CREATE TABLE runs through the SQL
    /// engine. Without an explicit PRIMARY KEY the first column is the locus.
    fn parse_create_chromosome(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // CREATE
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after CREATE, got {:?}",
                kw
            ));
        }
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

    /// DROP CHROMOSOME <name> [IF EXISTS];
    fn parse_drop_chromosome(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // DROP
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after DROP, got {:?}",
                kw
            ));
        }
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
                let p = self.parse_projection()?;
                stages.push(PipelineStage::Emit(p));
            } else if self.kw("SORT") {
                let column = self.ident()?;
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                stages.push(PipelineStage::Sort { column, asc });
            } else if self.kw("LIMIT") {
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: LIMIT expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Limit(n as usize));
            } else {
                return Err(format!("DnaQL: unknown pipeline stage {:?}", self.peek()));
            }
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::PipelineQuery { source, stages })
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
        let table = self.ident()?;

        if matches!(self.peek(), Tok::PlusEq) {
            // chr += { k: v, ... } — auto-locus from the PK-like field (*_id / id)
            self.next();
            self.expect(Tok::LBrace)?;
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
            let locus = fields
                .iter()
                .find(|(k, v)| (k == "id" || k.ends_with("_id")) && matches!(v, Value::Int(_)))
                .and_then(|(_, v)| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .ok_or("DnaQL: append without explicit locus needs an integer '<...>_id' field")?;
            return Ok(TxStmt::Append {
                table,
                locus,
                fields,
            });
        }

        self.expect(Tok::Colon)?;
        let locus = self.int()?;

        if matches!(self.peek(), Tok::PlusEq) {
            // chr:locus += { k: v, ... }
            self.next();
            self.expect(Tok::LBrace)?;
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
            return Ok(TxStmt::Append {
                table,
                locus,
                fields,
            });
        }

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
        let mut left = self.parse_and()?;
        while self.kw("OR") {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_cmp()?;
        while self.kw("AND") {
            let right = self.parse_cmp()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
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
                if let PipelineStage::Link(Some(link)) = stage {
                    out.push(link.chromosome.clone());
                }
            }
        }
        DnaQlAst::Tx { statements } => {
            for statement in statements {
                match statement {
                    TxStmt::Mutate { table, .. }
                    | TxStmt::Append { table, .. }
                    | TxStmt::DropLocus { table, .. } => out.push(table.clone()),
                }
            }
        }
        DnaQlAst::BioAnalysis { .. } => {}
        DnaQlAst::HealthCheck { chromosome, .. } => out.push(chromosome.clone()),
        DnaQlAst::CreateChromosome { table, .. }
        | DnaQlAst::DropChromosome { table, .. } => out.push(table.clone()),
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

    fn exec_pipeline(
        &self,
        source: &str,
        stages: &[PipelineStage],
    ) -> Result<(Vec<DnaQlRow>, String), String> {
        let prefix = format!("{}:", source);
        let raw_rows = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| format!("DnaQL storage error: {}", e))?;

        let mut rows: Vec<DnaQlRow> = Vec::new();
        for (key, payload) in raw_rows {
            let fields =
                RowCodec::decode(&payload).map_err(|e| format!("DnaQL: row unreadable: {}", e))?;
            let mut row_map: BTreeMap<String, Value> = fields.into_iter().collect();
            row_map.insert(
                "locus".to_string(),
                Value::Text(String::from_utf8_lossy(&key).to_string()),
            );
            rows.push(DnaQlRow { fields: row_map });
        }

        let mut plan = vec![format!("SeqScan({})", source)];
        for stage in stages {
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
                        for p in projection {
                            if let Some(v) = row.fields.get(p.as_str()) {
                                filtered.insert(p.clone(), v.clone());
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
            }
        }

        let note = format!("real pipeline: {}", plan.join(" -> "));
        Ok((rows, note))
    }

    fn exec_tx(&self, statements: &[TxStmt]) -> Result<(Vec<DnaQlRow>, String), String> {
        // 1. Buffer mutations into a transaction session. Reads go THROUGH
        // the session (read-your-writes: a statement sees earlier statements'
        // buffered effects) and record Serializable validation baselines.
        let mut session = self.tx.begin(crate::tx::TxIsolationLevel::Serializable);

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
                TxStmt::DropLocus {
                    table,
                    locus,
                    cascade: _,
                } => {
                    let key = format!("{}:{}", table, locus);
                    session.delete(key.into_bytes());
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
                TxStmt::DropLocus { table, locus, .. } => {
                    let mut f = BTreeMap::new();
                    f.insert(
                        "dropped".to_string(),
                        Value::Text(format!("{}:{}", table, locus)),
                    );
                    out.push(DnaQlRow { fields: f });
                }
            }
        }

        Ok((
            out,
            format!(
                "real TX: {} statements committed atomically at LSN {}",
                statements.len(),
                lsn
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
}

fn apply_assign(old: &Value, op: AssignOp, v: &Value) -> Result<Value, String> {
    match op {
        AssignOp::Set => Ok(v.clone()),
        AssignOp::Add => match (old, v) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(*a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + *b as f64)),
            _ => Err("DnaQL: += requires numeric operands".into()),
        },
        AssignOp::Sub => match (old, v) {
            (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a - b)),
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
