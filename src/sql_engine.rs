// SPDX-License-Identifier: AGPL-3.0-only
//! Relational SQL Engine — REAL tokenizer + parser + executor.
//!
//! Implemented subset (honest scope):
//!   SELECT cols|* FROM t [INNER|LEFT JOIN t2 ON a = b]* [WHERE expr] [LIMIT n]
//!   INSERT INTO t (cols) VALUES (...),(...)
//!   UPDATE t SET c = v, ... [WHERE expr]
//!   DELETE FROM t [WHERE expr]
//!   CREATE TABLE t (col TYPE [NOT NULL], ..., [FOREIGN KEY (col) REFERENCES t2(col) [ON DELETE CASCADE]])
//!   DROP TABLE t
//!
//! Storage: rows live in the Engine under keys '<table>:<pk>'; schemas under
//! '<table>:schema'. Joins: hash join on equi-conditions, nested-loop fallback.
//! FK validation on INSERT; ON DELETE CASCADE on DELETE. ComplementIndex
//! maintained for every FK column (O(1) link navigation for DnaQL).
//!
//! Execution plans are HONEST: they describe what actually ran.

use crate::complement_index::ComplementIndex;
use crate::engine::Engine;
use crate::expr::{ArithOp, CmpOp, Expr};
use crate::relational::{
    encode_index_value, encode_ordered_value, ChromosomeTable, ColumnType, ForeignKeyConstraint,
    RowCodec, SecondaryIndexMeta, Value,
};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct QueryRow {
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub execution_plan: String,
    pub execution_time_us: u64,
    pub message: Option<String>,
}

pub struct SqlEngine {
    pub engine: Arc<Engine>,
    schemas: RwLock<BTreeMap<String, ChromosomeTable>>,
    pub complement_index: RwLock<ComplementIndex>,
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum STok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Comma,
    Semi,
    LParen,
    RParen,
    Star,
    Plus,
    Minus,
    Slash,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Dot,
    Eof,
}

fn stokenize(input: &str) -> Result<Vec<STok>, String> {
    let b: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            ',' => {
                toks.push(STok::Comma);
                i += 1;
            }
            ';' => {
                toks.push(STok::Semi);
                i += 1;
            }
            '(' => {
                toks.push(STok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(STok::RParen);
                i += 1;
            }
            '*' => {
                toks.push(STok::Star);
                i += 1;
            }
            '+' => {
                toks.push(STok::Plus);
                i += 1;
            }
            '/' => {
                toks.push(STok::Slash);
                i += 1;
            }
            '.' => {
                toks.push(STok::Dot);
                i += 1;
            }
            '=' => {
                toks.push(STok::Eq);
                i += 1;
            }
            '!' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Ne);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Le);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(STok::Ne);
                i += 2;
            }
            '<' => {
                toks.push(STok::Lt);
                i += 1;
            }
            '>' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Ge);
                i += 2;
            }
            '>' => {
                toks.push(STok::Gt);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == quote {
                        // standard SQL escape: doubled quote inside a string is a literal quote
                        if i + 1 < b.len() && b[i + 1] == quote {
                            s.push(quote);
                            i += 2;
                            continue;
                        }
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                if !closed {
                    return Err("SQL: unterminated string".into());
                }
                toks.push(STok::Str(s));
            }
            // `-5` is a negative literal only at an operand start position;
            // after an operand (`a-5`) the same character is the subtraction
            // operator, tokenized below.
            '-' if i + 1 < b.len()
                && b[i + 1].is_ascii_digit()
                && (toks.is_empty()
                    || matches!(
                        toks.last(),
                        Some(
                            STok::Comma
                                | STok::Semi
                                | STok::LParen
                                | STok::Eq
                                | STok::Ne
                                | STok::Lt
                                | STok::Le
                                | STok::Gt
                                | STok::Ge
                                | STok::Plus
                                | STok::Minus
                                | STok::Star
                                | STok::Slash
                        )
                    )) =>
            {
                let mut num = String::new();
                let mut is_float = false;
                num.push('-');
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(STok::Float(num.parse().map_err(|_| "SQL: bad float")?));
                } else {
                    toks.push(STok::Int(num.parse().map_err(|_| "SQL: bad int")?));
                }
            }
            '-' => {
                toks.push(STok::Minus);
                i += 1;
            }
            '0'..='9' => {
                let mut num = String::new();
                let mut is_float = false;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(STok::Float(num.parse().map_err(|_| "SQL: bad float")?));
                } else {
                    toks.push(STok::Int(num.parse().map_err(|_| "SQL: bad int")?));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                    s.push(b[i]);
                    i += 1;
                }
                toks.push(STok::Ident(s));
            }
            other => return Err(format!("SQL: unexpected character '{}'", other)),
        }
    }
    toks.push(STok::Eof);
    Ok(toks)
}

// ---------------------------------------------------------------------------
// Parser — builds an AST per statement kind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum Stmt {
    Select {
        projection: Vec<SelectItem>, // empty = *
        from: String,
        joins: Vec<JoinSpec>,
        where_clause: Option<Expr>,
        group_by: Vec<String>,
        having: Option<Expr>,
        order_by: Vec<(String, bool)>, // (column / aggregate name, asc)
        limit: Option<usize>,
        offset: usize,
    },
    Insert {
        table: String,
        rows: Vec<Vec<(String, Value)>>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Value)>,
        where_clause: Option<Expr>,
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
    CreateTable {
        table: ChromosomeTable,
        if_not_exists: bool,
    },
    DropTable {
        table: String,
        if_exists: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
        /// Optional `ON <table>` qualifier (documented syntax).
        table: Option<String>,
    },
    /// `EXPLAIN <statement>` — DML/DDL are described without executing;
    /// a SELECT is executed read-only so the plan reflects reality
    /// (only its plan is returned, rows are discarded).
    Explain(Box<Stmt>),
}

/// One item of a SELECT projection: a plain column or an aggregate call,
/// each optionally renamed with `AS alias`.
#[derive(Debug, Clone)]
pub(crate) enum SelectItem {
    Star,
    Column {
        source: String,
        alias: Option<String>,
    },
    Agg {
        func: AggFunc,
        arg: Option<String>, // None = COUNT(*)
        /// Output column name: alias if given, else e.g. "COUNT(*)".
        rendered: String,
        /// Aggregate key inside HAVING/ORDER BY (canonical rendering).
        canonical: String,
    },
    /// Computed projection: an arithmetic expression, e.g. `total_usd * 2`.
    Computed {
        expr: Expr,
        /// Output column name: alias if given, else the rendered expression.
        rendered: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    fn from_name(name: &str) -> Result<Self, String> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Ok(AggFunc::Count),
            "SUM" => Ok(AggFunc::Sum),
            "AVG" => Ok(AggFunc::Avg),
            "MIN" => Ok(AggFunc::Min),
            "MAX" => Ok(AggFunc::Max),
            other => Err(format!("SQL: unknown aggregate function '{}'", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JoinSpec {
    pub(crate) join_type: JoinType,
    pub(crate) table: String,
    pub(crate) left_col: String, // qualified or bare (resolved later)
    pub(crate) right_col: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinType {
    Inner,
    Left,
}

struct SParser {
    toks: Vec<STok>,
    pos: usize,
}

impl SParser {
    fn peek(&self) -> &STok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }
    fn peek_at(&self, n: usize) -> &STok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)]
    }
    fn next(&mut self) -> STok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        self.pos += 1;
        t
    }
    fn kw(&mut self, w: &str) -> bool {
        if let STok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(w) {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn expect_kw(&mut self, w: &str) -> Result<(), String> {
        if self.kw(w) {
            Ok(())
        } else {
            Err(format!("SQL: expected {}", w))
        }
    }
    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            STok::Ident(s) => Ok(s),
            t => Err(format!("SQL: expected identifier, got {:?}", t)),
        }
    }
    fn expect(&mut self, t: STok) -> Result<(), String> {
        if self.peek() == &t {
            self.next();
            Ok(())
        } else {
            Err(format!("SQL: expected {:?}, got {:?}", t, self.peek()))
        }
    }
    /// Consume an optional table alias ('FROM chr_orders o').
    /// Aliases are parsed and dropped: the row model is single-namespace
    /// (qualified refs alias.col are stripped at parse time).
    fn maybe_alias(&mut self) {
        if let STok::Ident(s) = self.peek() {
            let lower = s.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "join"
                    | "inner"
                    | "left"
                    | "right"
                    | "where"
                    | "limit"
                    | "offset"
                    | "on"
                    | "group"
                    | "order"
                    | "union"
                    | "having"
            ) {
                self.next();
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.next() {
            STok::Minus => match self.next() {
                STok::Int(i) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| "SQL: integer literal out of range".to_string()),
                STok::Float(f) => Ok(Value::Float(-f)),
                t => Err(format!("SQL: expected number after '-', got {:?}", t)),
            },
            STok::Int(i) => Ok(Value::Int(i)),
            STok::Float(f) => Ok(Value::Float(f)),
            STok::Str(s) => Ok(typed_string_literal(&s)),
            STok::Ident(s) if s.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            t => Err(format!("SQL: expected literal, got {:?}", t)),
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        if self.kw("SELECT") {
            self.parse_select_tail()
        } else if self.kw("INSERT") {
            self.parse_insert()
        } else if self.kw("UPDATE") {
            self.parse_update()
        } else if self.kw("DELETE") {
            self.parse_delete()
        } else if self.kw("CREATE") {
            self.parse_create()
        } else if self.kw("DROP") {
            self.parse_drop()
        } else if self.kw("EXPLAIN") {
            self.next();
            let inner = self.parse_stmt()?;
            Ok(Stmt::Explain(Box::new(inner)))
        } else {
            Err(format!(
                "SQL: unsupported statement starting at {:?}",
                self.peek()
            ))
        }
    }

    /// Body of a SELECT after the SELECT keyword has been consumed.
    fn parse_select_tail(&mut self) -> Result<Stmt, String> {
        let stmt = self.parse_select()?;
        // Validate trailing tokens for top-level statements only (sub-selects
        // are followed by ')' which the caller consumes).
        Ok(stmt)
    }

    fn parse_select(&mut self) -> Result<Stmt, String> {
        let mut projection = Vec::new();
        if matches!(self.peek(), STok::Star) {
            self.next();
            projection.push(SelectItem::Star);
        } else {
            loop {
                projection.push(self.parse_select_item()?);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
        }
        self.expect_kw("FROM")?;
        let from = self.ident()?;
        self.maybe_alias();
        let mut joins = Vec::new();
        loop {
            let join_type = if self.kw("INNER") {
                self.expect_kw("JOIN")?;
                JoinType::Inner
            } else if self.kw("LEFT") {
                self.expect_kw("JOIN")?;
                JoinType::Left
            } else if self.kw("JOIN") {
                JoinType::Inner
            } else {
                break;
            };
            let table = self.ident()?;
            self.maybe_alias();
            self.expect_kw("ON")?;
            let left_col = self.ident()?;
            // optional qualified name a.col
            let mut lc = left_col;
            if matches!(self.peek(), STok::Dot) {
                self.next();
                lc = self.ident()?;
            }
            // = sign
            match self.next() {
                STok::Eq => {}
                t => return Err(format!("SQL: expected = in JOIN ON, got {:?}", t)),
            }
            let mut right_col = self.ident()?;
            if matches!(self.peek(), STok::Dot) {
                self.next();
                right_col = self.ident()?;
            }
            joins.push(JoinSpec {
                join_type,
                table,
                left_col: lc,
                right_col,
            });
        }
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let group_by = if self.kw("GROUP") {
            self.expect_kw("BY")?;
            let mut cols = Vec::new();
            loop {
                cols.push(self.ident()?);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            cols
        } else {
            Vec::new()
        };
        let having = if self.kw("HAVING") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let order_by = if self.kw("ORDER") {
            self.expect_kw("BY")?;
            let mut keys = Vec::new();
            loop {
                // order key: a column, or an aggregate call (COUNT(*) ...)
                let mut key = self.ident()?;
                // qualified reference `t.col` — the qualifier is stripped
                // (single-namespace row model), as in projections.
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    key = self.ident()?;
                }
                if matches!(self.peek(), STok::LParen) {
                    self.next();
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        Some(self.ident()?)
                    };
                    self.expect(STok::RParen)?;
                    key = render_agg(&key, arg.as_deref());
                }
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                keys.push((key, asc));
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
        };
        let limit = if self.kw("LIMIT") {
            match self.next() {
                STok::Int(i) => Some(i as usize),
                t => return Err(format!("SQL: LIMIT expects int, got {:?}", t)),
            }
        } else {
            None
        };
        let offset = if self.kw("OFFSET") {
            match self.next() {
                STok::Int(i) => i as usize,
                t => return Err(format!("SQL: OFFSET expects int, got {:?}", t)),
            }
        } else {
            0
        };
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Select {
            projection,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// One projection item: `*`, a plain column, an aggregate call or an
    /// arithmetic expression (`total_usd * 2 AS doub`).
    fn parse_select_item(&mut self) -> Result<SelectItem, String> {
        if matches!(self.peek(), STok::Star) {
            self.next();
            return Ok(SelectItem::Star);
        }
        // aggregate call: COUNT(*) / SUM(col) — recognised before the general
        // expression path so `COUNT(*)` never parses as multiplication.
        if let STok::Ident(s) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::LParen) && AggFunc::from_name(&s).is_ok() {
                self.next();
                self.next(); // (
                let arg = if matches!(self.peek(), STok::Star) {
                    self.next();
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(STok::RParen)?;
                let func = AggFunc::from_name(&s)?;
                let canonical = render_agg(&s, arg.as_deref());
                let alias = self.maybe_as_alias()?;
                let rendered = alias.clone().unwrap_or_else(|| canonical.clone());
                return Ok(SelectItem::Agg {
                    func,
                    arg,
                    rendered,
                    canonical,
                });
            }
        }
        let expr = self.parse_operand()?;
        match expr {
            Expr::Column(source) => {
                let alias = self.maybe_as_alias()?;
                Ok(SelectItem::Column { source, alias })
            }
            other => {
                let alias = self.maybe_as_alias()?;
                let rendered = alias.unwrap_or_else(|| other.describe());
                Ok(SelectItem::Computed {
                    expr: other,
                    rendered,
                })
            }
        }
    }

    /// Optional `AS alias` / bare alias after an item.
    fn maybe_as_alias(&mut self) -> Result<Option<String>, String> {
        if self.kw("AS") {
            return self.ident().map(Some);
        }
        // bare alias (SELECT COUNT(*) c) — an identifier that starts a new
        // clause only if it is a keyword.
        if let STok::Ident(s) = self.peek() {
            let lower = s.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "from"
                    | "where"
                    | "group"
                    | "having"
                    | "order"
                    | "limit"
                    | "offset"
                    | "join"
                    | "inner"
                    | "left"
                    | "on"
                    | "and"
                    | "or"
                    | "asc"
                    | "desc"
                    | "union"
            ) {
                return Ok(Some(self.ident()?));
            }
        }
        Ok(None)
    }

    fn parse_insert(&mut self) -> Result<Stmt, String> {
        self.expect_kw("INTO")?;
        let table = self.ident()?;
        self.expect(STok::LParen)?;

        // Form 1 (extended, typed): INSERT INTO t (col = v1, col2 = v2)
        // Form 2 (standard, positional): INSERT INTO t (c1, c2) VALUES (v1, v2), ...
        let first = self.ident()?;
        if matches!(self.peek(), STok::Eq) {
            self.next();
            let v = self.value()?;
            let mut rows = vec![vec![(first, v)]];
            loop {
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
                let col = self.ident()?;
                match self.next() {
                    STok::Eq => {}
                    t => {
                        return Err(format!(
                            "SQL: expected '=' after column '{}', got {:?}",
                            col, t
                        ))
                    }
                }
                let v = self.value()?;
                rows[0].push((col, v));
            }
            self.expect(STok::RParen)?;
            if matches!(self.peek(), STok::Semi) {
                self.next();
            }
            return Ok(Stmt::Insert { table, rows });
        }

        // Standard positional form
        let mut cols = vec![first];
        loop {
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
            cols.push(self.ident()?);
        }
        self.expect(STok::RParen)?;
        self.expect_kw("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(STok::LParen)?;
            let mut values = Vec::new();
            loop {
                values.push(self.value()?);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.expect(STok::RParen)?;
            if values.len() != cols.len() {
                return Err(format!(
                    "SQL: VALUES tuple has {} values but column list has {}",
                    values.len(),
                    cols.len()
                ));
            }
            rows.push(cols.iter().cloned().zip(values).collect::<Vec<_>>());
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Insert { table, rows })
    }

    fn parse_update(&mut self) -> Result<Stmt, String> {
        let table = self.ident()?;
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            match self.next() {
                STok::Eq => {}
                t => return Err(format!("SQL: expected = in SET, got {:?}", t)),
            }
            let v = self.value()?;
            assignments.push((col, v));
            if matches!(self.peek(), STok::Comma) {
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
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Update {
            table,
            assignments,
            where_clause,
        })
    }

    fn parse_delete(&mut self) -> Result<Stmt, String> {
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Delete {
            table,
            where_clause,
        })
    }

    fn parse_create(&mut self) -> Result<Stmt, String> {
        if self.kw("INDEX") {
            return self.parse_create_index(false);
        }
        if self.kw("UNIQUE") {
            self.expect_kw("INDEX")?;
            return self.parse_create_index(true);
        }
        self.expect_kw("TABLE")?;
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let table_name = self.ident()?;
        self.expect(STok::LParen)?;
        let mut t = ChromosomeTable::new(table_name.clone(), "id");
        let mut fks: Vec<ForeignKeyConstraint> = Vec::new();
        loop {
            if self.kw("PRIMARY") {
                self.expect_kw("KEY")?;
                self.expect(STok::LParen)?;
                let pk = self.ident()?;
                t.primary_locus_column = pk;
                self.expect(STok::RParen)?;
            } else if self.kw("FOREIGN") {
                self.expect_kw("KEY")?;
                self.expect(STok::LParen)?;
                let col = self.ident()?;
                self.expect(STok::RParen)?;
                self.expect_kw("REFERENCES")?;
                let target = self.ident()?;
                self.expect(STok::LParen)?;
                let target_col = self.ident()?;
                self.expect(STok::RParen)?;
                let on_delete_cascade = if self.kw("ON") {
                    self.expect_kw("DELETE")?;
                    if self.kw("CASCADE") {
                        true
                    } else {
                        self.kw("RESTRICT");
                        false
                    }
                } else {
                    false
                };
                fks.push(ForeignKeyConstraint {
                    column: col,
                    target_chromosome: target,
                    target_locus_column: target_col,
                    on_delete_cascade,
                });
            } else {
                let col = self.ident()?;
                let ty = if self.kw("INT") || self.kw("INTEGER") || self.kw("INT64") {
                    ColumnType::Int64
                } else if self.kw("FLOAT") || self.kw("FLOAT64") || self.kw("DOUBLE") {
                    ColumnType::Float64
                } else if self.kw("TEXT") || self.kw("VARCHAR") || self.kw("STRING") {
                    ColumnType::Text
                } else if self.kw("DNA") || self.kw("DNA2BIT") {
                    ColumnType::Dna2Bit
                } else if self.kw("TIMESTAMP") {
                    ColumnType::Timestamp
                } else {
                    return Err(format!("SQL: unknown column type for '{}'", col));
                };
                let _ = self.kw("NOT") && self.kw("NULL"); // accepted, not enforced in v2.1
                t.add_column(col, ty);
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(STok::RParen)?;
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        t.foreign_keys = fks;
        Ok(Stmt::CreateTable {
            table: t,
            if_not_exists,
        })
    }

    /// CREATE [UNIQUE handled above] INDEX [IF NOT EXISTS] name ON table (col)
    fn parse_create_index(&mut self, unique: bool) -> Result<Stmt, String> {
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
        self.expect(STok::LParen)?;
        let column = self.ident()?;
        self.expect(STok::RParen)?;
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        })
    }

    fn parse_drop(&mut self) -> Result<Stmt, String> {
        if self.kw("INDEX") {
            let if_exists = if self.kw("IF") {
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            // Documented form: DROP INDEX <name> ON <table>. The bare form
            // without ON stays accepted (used by existing scripts/tests).
            let table = if self.kw("ON") {
                Some(self.ident()?)
            } else {
                None
            };
            if matches!(self.peek(), STok::Semi) {
                self.next();
            }
            return Ok(Stmt::DropIndex {
                name,
                if_exists,
                table,
            });
        }
        self.expect_kw("TABLE")?;
        let if_exists = if self.kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let table = self.ident()?;
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::DropTable { table, if_exists })
    }

    // expression parsing (same shape as DnaQL)
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
            STok::Eq => Some(CmpOp::Eq),
            STok::Ne => Some(CmpOp::Ne),
            STok::Lt => Some(CmpOp::Lt),
            STok::Le => Some(CmpOp::Le),
            STok::Gt => Some(CmpOp::Gt),
            STok::Ge => Some(CmpOp::Ge),
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
        // `col IN (v1, v2, ...)` / `col IN (SELECT ...)`
        if self.kw("IN") {
            let column = match left {
                Expr::Column(c) => c,
                other => {
                    return Err(format!(
                        "SQL: IN requires a column on the left, got '{}'",
                        other.describe()
                    ))
                }
            };
            self.expect(STok::LParen)?;
            if self.kw("SELECT") {
                // Materialised sub-select: render it back to SQL text and let
                // the executor run it (single-column projection required).
                let sub = self.parse_select_tail()?;
                if matches!(self.peek(), STok::RParen) {
                    self.next();
                } else {
                    return Err("SQL: expected ')' after IN subquery".into());
                }
                let query = render_select_sql(&sub);
                return Ok(Expr::InSubquery { column, query });
            }
            let mut values = Vec::new();
            loop {
                values.push(self.value()?);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.expect(STok::RParen)?;
            return Ok(Expr::InList { column, values });
        }
        if self.kw("IS") {
            if self.kw("NOT") {
                self.expect_kw("NULL")?;
                return Ok(Expr::IsNotNull(Box::new(left)));
            }
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(left)));
        }
        Ok(left)
    }
    /// Additive level of an arithmetic expression: `a + b - c`.
    fn parse_operand(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                STok::Plus => ArithOp::Add,
                STok::Minus => ArithOp::Sub,
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
    /// Multiplicative level: `a * b / c`.
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_primary()?;
        loop {
            let op = match self.peek() {
                STok::Star => ArithOp::Mul,
                STok::Slash => ArithOp::Div,
                _ => break,
            };
            self.next();
            let right = self.parse_primary()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }
    /// Primary: literal, column (qualified `t.col` is stripped), aggregate
    /// call, parenthesised expression or unary minus.
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            STok::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            STok::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            // Same typing rule as INSERT values: pure-ACGT literals are DNA,
            // so WHERE comparisons match the stored (typed) representation.
            STok::Str(s) => {
                self.next();
                Ok(Expr::Literal(typed_string_literal(&s)))
            }
            STok::Ident(s) => {
                // aggregate reference (HAVING COUNT(*) > 2): resolves to the
                // rendered aggregate column of the grouped result row.
                if matches!(self.peek_at(1), STok::LParen) && AggFunc::from_name(&s).is_ok() {
                    self.next();
                    self.next(); // (
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        Some(self.ident()?)
                    };
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Column(render_agg(&s, arg.as_deref())));
                }
                self.next();
                if s.eq_ignore_ascii_case("NULL") {
                    Ok(Expr::Literal(Value::Null))
                } else if matches!(self.peek(), STok::Dot) {
                    // qualified: alias.col -> col
                    self.next();
                    let col = self.ident()?;
                    Ok(Expr::Column(col))
                } else {
                    Ok(Expr::Column(s))
                }
            }
            STok::LParen => {
                self.next();
                let e = self.parse_or()?;
                match self.next() {
                    STok::RParen => Ok(e),
                    t => Err(format!("SQL: expected ')', got {:?}", t)),
                }
            }
            STok::Minus => {
                self.next();
                match self.parse_primary()? {
                    Expr::Literal(Value::Int(i)) => Ok(Expr::Literal(Value::Int(-i))),
                    Expr::Literal(Value::Float(f)) => Ok(Expr::Literal(Value::Float(-f))),
                    other => Ok(Expr::Arith {
                        op: ArithOp::Sub,
                        left: Box::new(Expr::Literal(Value::Int(0))),
                        right: Box::new(other),
                    }),
                }
            }
            t => Err(format!("SQL: expected operand, got {:?}", t)),
        }
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Parse a SQL statement and return every table it references, including
/// tables in JOINs, foreign-key definitions and `IN (SELECT ...)` subqueries.
/// This is used by the per-user ACL before execution; it never touches storage.
pub(crate) fn referenced_tables(sql: &str) -> Result<Vec<String>, String> {
    let toks = stokenize(sql)?;
    let mut parser = SParser { toks, pos: 0 };
    let stmt = parser.parse_stmt()?;
    if parser.peek() != &STok::Eof {
        return Err(format!(
            "SQL: unexpected trailing tokens starting at {:?} (unsupported syntax?)",
            parser.peek()
        ));
    }
    let mut tables = Vec::new();
    collect_stmt_tables(&stmt, &mut tables)?;
    tables.sort();
    tables.dedup();
    Ok(tables)
}

fn collect_stmt_tables(stmt: &Stmt, out: &mut Vec<String>) -> Result<(), String> {
    match stmt {
        Stmt::Select {
            projection,
            from,
            joins,
            where_clause,
            having,
            ..
        } => {
            out.push(from.clone());
            for join in joins {
                out.push(join.table.clone());
            }
            for item in projection {
                if let SelectItem::Computed { expr, .. } = item {
                    collect_expr_tables(expr, out)?;
                }
            }
            if let Some(expr) = where_clause {
                collect_expr_tables(expr, out)?;
            }
            if let Some(expr) = having {
                collect_expr_tables(expr, out)?;
            }
        }
        Stmt::Insert { table, .. } | Stmt::DropTable { table, .. } => out.push(table.clone()),
        Stmt::Update {
            table,
            where_clause,
            ..
        }
        | Stmt::Delete {
            table,
            where_clause,
            ..
        } => {
            out.push(table.clone());
            if let Some(expr) = where_clause {
                collect_expr_tables(expr, out)?;
            }
        }
        Stmt::CreateTable { table, .. } => {
            out.push(table.name.clone());
            for fk in &table.foreign_keys {
                out.push(fk.target_chromosome.clone());
            }
        }
        Stmt::CreateIndex { table, .. } => out.push(table.clone()),
        Stmt::DropIndex { table, .. } => {
            if let Some(table) = table {
                out.push(table.clone());
            }
        }
        Stmt::Explain(inner) => collect_stmt_tables(inner, out)?,
    }
    Ok(())
}

fn collect_expr_tables(expr: &Expr, out: &mut Vec<String>) -> Result<(), String> {
    match expr {
        Expr::InSubquery { query, .. } => {
            let nested = referenced_tables(query)?;
            out.extend(nested);
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            collect_expr_tables(inner, out)?;
        }
        Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Cmp { left, right, .. }
        | Expr::Arith { left, right, .. } => {
            collect_expr_tables(left, out)?;
            collect_expr_tables(right, out)?;
        }
        Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => {}
    }
    Ok(())
}

impl SqlEngine {

    pub fn new(engine: Arc<Engine>) -> Self {
        let s = Self {
            engine,
            schemas: RwLock::new(BTreeMap::new()),
            complement_index: RwLock::new(ComplementIndex::new()),
        };
        if let Err(error) = s.load_schemas_from_disk() {
            eprintln!("[SQL] Could not rebuild persisted schema/index state: {error}");
        }
        s
    }

    fn load_schemas_from_disk(&self) -> Result<(), String> {
        let rows = self
            .engine
            .scan_prefix(b"__schema:")
            .map_err(|e| e.to_string())?;
        let mut loaded = Vec::new();
        for (_, payload) in rows {
            if let Ok(t) = serde_json::from_slice::<ChromosomeTable>(&payload) {
                loaded.push(t);
            }
        }
        {
            let mut schemas = self.schemas.write();
            let mut index = self.complement_index.write();
            for table in &loaded {
                for fk in &table.foreign_keys {
                    index.register_column(&table.name, &fk.column);
                }
                schemas.insert(table.name.clone(), table.clone());
            }
        }
        for table in &loaded {
            let rows = self.read_table_rows(&table.name)?;
            let mut index = self.complement_index.write();
            for row in rows {
                let pk = Self::pk_of(table, &row)?;
                let key = Self::row_key(&table.name, &pk);
                for fk in &table.foreign_keys {
                    if let Some(value) = row.get(&fk.column) {
                        if !value.is_null() {
                            index.upsert(&table.name, &fk.column, &key, value);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.schemas.read().keys().cloned().collect()
    }

    pub fn get_schema(&self, table: &str) -> Option<ChromosomeTable> {
        self.schemas.read().get(table).cloned()
    }

    /// Convert one or more SQL statements (separated by `;`) into the
    /// equivalent DnaQL query text. Statements without a DnaQL equivalent
    /// (JOIN, aggregates, CREATE INDEX, EXPLAIN, ...) return an explicit
    /// error naming the construct — nothing is silently approximated.
    ///
    /// ```text
    /// let dnaql = engine.sql_to_dnaql(
    ///     "SELECT user_id, username FROM chr_users WHERE balance > 100 LIMIT 10"
    /// ).unwrap();
    /// assert_eq!(
    ///     dnaql,
    ///     "FROM chr_users |> WHERE balance > 100 |> LIMIT 10 |> EMIT user_id, username;"
    /// );
    /// ```
    pub fn sql_to_dnaql(&self, sql: &str) -> Result<String, String> {
        let toks = stokenize(sql)?;
        let mut p = SParser { toks, pos: 0 };
        let mut out = Vec::new();
        while p.peek() != &STok::Eof {
            let stmt = p.parse_stmt()?;
            out.push(crate::sql_to_dnaql::transpile(&stmt, &|t| {
                self.get_schema(t)
            })?);
        }
        if out.is_empty() {
            return Err("SQL: nothing to convert (empty statement)".into());
        }
        Ok(out.join("\n"))
    }

    pub fn execute(&self, sql: &str) -> Result<QueryResult, String> {
        let start = std::time::Instant::now();
        // SHOW HEALTH — engine health snapshot (P5.2), handled before the
        // regular SQL grammar so it works everywhere SQL does.
        if sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SHOW HEALTH")
        {
            let health = self.engine.health();
            let rows = vec![{
                let mut m = std::collections::BTreeMap::new();
                m.insert("health".to_string(), Value::Text(health.to_string()));
                m
            }];
            return Ok(QueryResult {
                columns: vec!["health".to_string()],
                rows,
                execution_plan: "SHOW HEALTH: engine.health() snapshot".to_string(),
                execution_time_us: start.elapsed().as_micros() as u64,
                message: Some(format!(
                    "v3.0 Repairosome — lesions: {} found / {} repaired / {} unrecoverable",
                    health["lesions"]["found"],
                    health["lesions"]["repaired"],
                    health["lesions"]["unrecoverable"]
                )),
            });
        }
        let toks = stokenize(sql)?;
        let mut p = SParser { toks, pos: 0 };
        let stmt = p.parse_stmt()?;
        if p.peek() != &STok::Eof {
            return Err(format!(
                "SQL: unexpected trailing tokens starting at {:?} (unsupported syntax?)",
                p.peek()
            ));
        }
        let (columns, rows, plan, message) = match stmt {
            Stmt::Select {
                projection,
                from,
                joins,
                where_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => self.exec_select(
                &projection,
                &from,
                &joins,
                where_clause,
                &group_by,
                having,
                &order_by,
                limit,
                offset,
            )?,
            Stmt::Insert { table, rows } => self.exec_insert(&table, rows)?,
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => self.exec_update(&table, &assignments, where_clause)?,
            Stmt::Delete {
                table,
                where_clause,
            } => self.exec_delete(&table, where_clause)?,
            Stmt::CreateTable {
                table,
                if_not_exists,
            } => self.exec_create_table(table, if_not_exists)?,
            Stmt::DropTable { table, if_exists } => self.exec_drop_table(&table, if_exists)?,
            Stmt::CreateIndex {
                name,
                table,
                column,
                unique,
                if_not_exists,
            } => self.exec_create_index(&name, &table, &column, unique, if_not_exists)?,
            Stmt::DropIndex {
                name,
                if_exists,
                table,
            } => self.exec_drop_index(&name, if_exists, table.as_deref())?,
            Stmt::Explain(inner) => self.exec_explain(*inner)?,
        };
        Ok(QueryResult {
            columns,
            rows,
            execution_plan: plan,
            execution_time_us: start.elapsed().as_micros() as u64,
            message,
        })
    }

    // ---------- EXPLAIN ----------

    /// EXPLAIN: DML/DDL are described without running (no side effects); a
    /// SELECT runs read-only so the reported access paths are the real ones,
    /// but only the plan is returned — result rows are discarded.
    fn exec_explain(
        &self,
        inner: Stmt,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        match inner {
            Stmt::Select {
                projection,
                from,
                joins,
                where_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                let (_, rows, plan, _) = self.exec_select(
                    &projection,
                    &from,
                    &joins,
                    where_clause,
                    &group_by,
                    having,
                    &order_by,
                    limit,
                    offset,
                )?;
                let n = rows.len();
                Ok((
                    vec!["plan".into()],
                    vec![mk_row(&[("plan", Value::Text(plan.clone()))])],
                    format!("Explain(Select rows={})", n),
                    Some(format!(
                        "{} row(s); the SELECT ran read-only so the plan is real, rows discarded",
                        n
                    )),
                ))
            }
            other => {
                let desc = Self::describe_stmt(&other);
                Ok((
                    vec!["plan".into()],
                    vec![mk_row(&[("plan", Value::Text(desc.clone()))])],
                    format!("Explain({})", desc),
                    Some("statement NOT executed".into()),
                ))
            }
        }
    }

    /// Static plan description for a non-SELECT statement (EXPLAIN without
    /// execution — nothing is mutated).
    fn describe_stmt(stmt: &Stmt) -> String {
        match stmt {
            Stmt::Select { from, .. } => format!("Select(FROM {})", from),
            Stmt::Insert { table, rows } => {
                format!("Insert({}, {} row(s))", table, rows.len())
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => {
                let set = assignments
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                let w = where_clause
                    .as_ref()
                    .map(|e| format!(" WHERE {}", e.describe()))
                    .unwrap_or_default();
                format!("Update({} SET {}{})", table, set, w)
            }
            Stmt::Delete {
                table,
                where_clause,
            } => format!(
                "Delete({}{})",
                table,
                where_clause
                    .as_ref()
                    .map(|e| format!(" WHERE {}", e.describe()))
                    .unwrap_or_default()
            ),
            Stmt::CreateTable {
                table,
                if_not_exists,
            } => format!(
                "CreateTable({}{})",
                table.name,
                if *if_not_exists {
                    " [if-not-exists]"
                } else {
                    ""
                }
            ),
            Stmt::DropTable { table, if_exists } => format!(
                "DropTable({}{})",
                table,
                if *if_exists { " [if-exists]" } else { "" }
            ),
            Stmt::CreateIndex {
                name,
                table,
                column,
                unique,
                ..
            } => format!(
                "CreateIndex({} ON {}({}){})",
                name,
                table,
                column,
                if *unique { " UNIQUE" } else { "" }
            ),
            Stmt::DropIndex { name, table, .. } => format!(
                "DropIndex({}{})",
                name,
                table
                    .as_ref()
                    .map(|t| format!(" ON {}", t))
                    .unwrap_or_default()
            ),
            Stmt::Explain(_) => "Explain(...)".into(),
        }
    }

    // ---------- storage helpers ----------

    fn row_key(table: &str, pk: &Value) -> String {
        format!("{}:{}", table, pk)
    }

    fn read_table_rows(&self, table: &str) -> Result<Vec<BTreeMap<String, Value>>, String> {
        let prefix = format!("{}:", table);
        let raw = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (key, payload) in raw {
            // skip the schema meta-row
            if key.ends_with(b":schema") || String::from_utf8_lossy(&key).ends_with(":schema") {
                continue;
            }
            let fields = RowCodec::decode(&payload)
                .map_err(|e| format!("row {} unreadable: {}", String::from_utf8_lossy(&key), e))?;
            out.push(fields.into_iter().collect());
        }
        Ok(out)
    }

    fn pk_of(schema: &ChromosomeTable, row: &BTreeMap<String, Value>) -> Result<Value, String> {
        row.get(&schema.primary_locus_column)
            .cloned()
            .ok_or_else(|| format!("row missing PK column '{}'", schema.primary_locus_column))
    }

    /// Fetch a single row by full storage key '<table>:<pk>'.
    fn read_row_by_key(&self, row_key: &str) -> Result<Option<BTreeMap<String, Value>>, String> {
        if row_key.ends_with(":schema") || row_key.starts_with("__idx:") {
            return Ok(None);
        }
        match self
            .engine
            .get(row_key.as_bytes())
            .map_err(|e| e.to_string())?
        {
            Some(payload) => {
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", row_key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    // -------- primary-key point lookup (no full scan) --------
    // Rows are keyed '<table>:<pk>', so a WHERE that pins the primary key can
    // resolve via the engine's keyed get (memtable skiplist + segment sparse
    // index + bloom filter) instead of decoding the whole chromosome.
    //
    // Safety rule: we only take this path when EVERY logical branch of the
    // predicate pins the pk to a literal — i.e. OR of AND-conjunctions, each
    // containing '<pk> = <literal>'. The full WHERE is still evaluated on the
    // fetched row(s) afterwards, so results are identical to SeqScan; only the
    // candidate set shrinks.
    fn pk_eq_literals(expr: &Expr, pk_col: &str) -> Option<Vec<Value>> {
        match expr {
            Expr::Or(a, b) => {
                let mut out = Self::pk_eq_literals(a, pk_col)?;
                out.extend(Self::pk_eq_literals(b, pk_col)?);
                Some(out)
            }
            _ => {
                fn find(e: &Expr, pk_col: &str) -> Option<Value> {
                    match e {
                        Expr::And(a, b) => find(a, pk_col).or_else(|| find(b, pk_col)),
                        Expr::Cmp {
                            op: CmpOp::Eq,
                            left,
                            right,
                        } => {
                            let lit = match (left.as_ref(), right.as_ref()) {
                                (Expr::Column(c), Expr::Literal(v)) if c == pk_col => Some(v),
                                (Expr::Literal(v), Expr::Column(c)) if c == pk_col => Some(v),
                                _ => None,
                            }?;
                            (!lit.is_null()).then(|| lit.clone())
                        }
                        _ => None,
                    }
                }
                find(expr, pk_col).map(|lit| vec![lit])
            }
        }
    }

    /// Fetch a single row by storage key '<table>:<pk>' (keyed get, no scan).
    fn read_row_point(
        &self,
        table: &str,
        pk: &Value,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let key = Self::row_key(table, pk);
        // Same visibility rule as read_table_rows: '<table>:schema' is meta.
        if key.ends_with(":schema") {
            return Ok(None);
        }
        match self.engine.get(key.as_bytes()).map_err(|e| e.to_string())? {
            Some(payload) => {
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    // ---------- statements ----------

    fn exec_create_table(
        &self,
        table: ChromosomeTable,
        if_not_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        {
            let schemas = self.schemas.read();
            if schemas.contains_key(&table.name) {
                if if_not_exists {
                    return Ok((
                        vec!["status".into()],
                        vec![mk_row(&[("status", Value::Text("EXISTS_OK".into()))])],
                        format!("CreateTable({}) [if-not-exists]", table.name),
                        Some(format!("table '{}' already exists", table.name)),
                    ));
                }
                return Err(format!("table '{}' already exists", table.name));
            }
        }
        let payload = serde_json::to_vec(&table).map_err(|e| e.to_string())?;
        self.engine
            .put(format!("__schema:{}", table.name).as_bytes(), &payload)
            .map_err(|e| e.to_string())?;

        for fk in &table.foreign_keys {
            self.complement_index
                .write()
                .register_column(&table.name, &fk.column);
        }
        self.schemas
            .write()
            .insert(table.name.clone(), table.clone());

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("CREATED".into()))])],
            format!(
                "CreateTable({}) cols={} fks={}",
                table.name,
                table.columns.len(),
                table.foreign_keys.len()
            ),
            None,
        ))
    }

    fn exec_create_index(
        &self,
        name: &str,
        table: &str,
        column: &str,
        unique: bool,
        if_not_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut table_schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' does not exist", table))?;
        if !table_schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: cannot create index '{}': column '{}' does not exist on '{}'",
                name, column, table
            ));
        }
        if table_schema
            .secondary_indexes
            .iter()
            .any(|i| i.name == name)
        {
            if if_not_exists {
                return Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("EXISTS_OK".into()))])],
                    format!("CreateIndex({}) [if-not-exists]", name),
                    Some(format!("index '{}' already exists", name)),
                ));
            }
            return Err(format!("index '{}' already exists", name));
        }
        if table_schema.secondary_index_on(column).is_some() {
            return Err(format!(
                "SQL: column '{}.{}' already has an index (rename it there or drop it first)",
                table, column
            ));
        }

        // Backfill: one WAL batch of index entries over the existing rows.
        // A UNIQUE index must verify existing data first — duplicates abort
        // before anything is written.
        let rows = self.read_table_rows(table)?;
        let base = index_prefix(table, column);
        let mut batch = Vec::with_capacity(rows.len());
        let mut indexed = 0usize;
        if unique {
            let mut seen: std::collections::HashMap<Vec<u8>, Value> =
                std::collections::HashMap::new();
            for row in &rows {
                let Some(v) = row.get(column) else { continue };
                if v.is_null() {
                    continue; // SQL: NULLs never conflict in a UNIQUE index
                }
                let enc = encode_ordered_value(v);
                if seen.contains_key(&enc) {
                    return Err(format!(
                        "SQL: cannot create UNIQUE index '{}': duplicate value {} on column '{}.{}'",
                        name, v, table, column
                    ));
                }
                seen.insert(enc, v.clone());
            }
        }
        for row in &rows {
            let Some(v) = row.get(column) else { continue };
            if v.is_null() {
                continue; // NULLs are never index hits (SQL semantics)
            }
            let pk = Self::pk_of(&table_schema, row)?;
            let row_key = Self::row_key(table, &pk);
            let mut key = base.clone();
            key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
            key.extend_from_slice(row_key.as_bytes());
            batch.push((crate::wal::WalOp::Put, key, Some(row_key.into_bytes())));
            indexed += 1;
        }
        if !batch.is_empty() {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("index backfill failed: {}", e))?;
        }

        table_schema.secondary_indexes.push(SecondaryIndexMeta {
            name: name.to_string(),
            column: column.to_string(),
            unique,
        });
        self.persist_schema(&table_schema)?;

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("CREATED".into()))])],
            format!(
                "CreateIndex({}{} ON {}.{} rows={})",
                if unique { "UNIQUE " } else { "" },
                name,
                table,
                column,
                indexed
            ),
            Some(format!(
                "{}index '{}' created on {}.{} ({} entries)",
                if unique { "UNIQUE " } else { "" },
                name,
                table,
                column,
                indexed
            )),
        ))
    }

    fn exec_drop_index(
        &self,
        name: &str,
        if_exists: bool,
        table: Option<&str>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        // Locate the index: within the `ON <table>` table when qualified,
        // otherwise across all schemas (legacy bare `DROP INDEX name`).
        let target = {
            let schemas = self.schemas.read();
            match table {
                Some(want) => schemas.get(want).and_then(|t| {
                    t.secondary_indexes
                        .iter()
                        .find(|i| i.name == name)
                        .map(|i| (t.name.clone(), i.column.clone()))
                }),
                None => schemas.values().find_map(|t| {
                    t.secondary_indexes
                        .iter()
                        .find(|i| i.name == name)
                        .map(|i| (t.name.clone(), i.column.clone()))
                }),
            }
        };
        let Some((table, column)) = target else {
            if if_exists {
                return Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("DROP_OK".into()))])],
                    format!("DropIndex({}) [if-exists]", name),
                    None,
                ));
            }
            return Err(match table {
                Some(want) => format!("index '{}' does not exist on table '{}'", name, want),
                None => format!("index '{}' does not exist", name),
            });
        };

        // Delete every entry of this column's index in one batch.
        let base = index_prefix(&table, &column);
        let entries = self.engine.scan_prefix(&base).map_err(|e| e.to_string())?;
        let batch: Vec<_> = entries
            .into_iter()
            .map(|(k, _)| (crate::wal::WalOp::Delete, k, None))
            .collect();
        let removed = batch.len();
        if !batch.is_empty() {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("index drop failed: {}", e))?;
        }

        let mut schema = self
            .get_schema(&table)
            .ok_or_else(|| format!("table '{}' vanished", table))?;
        schema.secondary_indexes.retain(|i| i.name != name);
        self.persist_schema(&schema)?;

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!(
                "DropIndex({} on {}.{}) entries={}",
                name, table, column, removed
            ),
            Some(format!(
                "index '{}' dropped ({} entries removed)",
                name, removed
            )),
        ))
    }

    /// Persist a table schema row (CREATE INDEX / DROP INDEX mutate it).
    fn persist_schema(&self, table: &ChromosomeTable) -> Result<(), String> {
        let payload = serde_json::to_vec(table).map_err(|e| e.to_string())?;
        self.engine
            .put(format!("__schema:{}", table.name).as_bytes(), &payload)
            .map_err(|e| e.to_string())?;
        self.schemas
            .write()
            .insert(table.name.clone(), table.clone());
        Ok(())
    }

    /// Enforce every UNIQUE secondary index of `table` for the row `new`
    /// (full row map + its row key). `old_key`, when present, is the key of
    /// the row being replaced: its own current index entries don't count as
    /// conflicts. A non-NULL `new` value must appear in the index at most
    /// once besides the row itself.
    fn check_unique_violations(
        &self,
        table: &str,
        schema: &ChromosomeTable,
        row_key: &str,
        new: &BTreeMap<String, Value>,
        old_key: Option<&str>,
    ) -> Result<(), String> {
        for idx in schema.secondary_indexes.iter().filter(|i| i.unique) {
            let Some(v) = new.get(&idx.column) else {
                continue;
            };
            if v.is_null() {
                continue; // SQL: multiple NULLs never conflict
            }
            let mut prefix = index_prefix(table, &idx.column);
            prefix.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
            let entries = self
                .engine
                .scan_prefix(&prefix)
                .map_err(|e| e.to_string())?;
            for (_, payload) in entries {
                let Ok(holder) = String::from_utf8(payload) else {
                    continue;
                };
                if holder == row_key {
                    continue; // this row's own (unchanged) entry
                }
                if Some(holder.as_str()) == old_key {
                    continue; // entry being replaced by this very update
                }
                return Err(format!(
                    "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                    table, idx.column, v, idx.name
                ));
            }
        }
        Ok(())
    }

    /// Secondary-index write batch for a row change: entries whose key/value
    /// differs between `old` and `new` (empty when no indexed column moved).
    fn index_delta_batch(
        schema: &ChromosomeTable,
        old: Option<(&str, &BTreeMap<String, Value>)>,
        new: Option<(&str, &BTreeMap<String, Value>)>,
    ) -> Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)> {
        let mut batch = Vec::new();
        if schema.secondary_indexes.is_empty() {
            return batch;
        }
        for idx in &schema.secondary_indexes {
            let col = &idx.column;
            let old_entry = old.and_then(|(table, row)| {
                row.get(col).filter(|v| !v.is_null()).and_then(|v| {
                    let row_key = Self::row_key(table, &Self::pk_of(schema, row).ok()?);
                    let mut key = index_prefix(table, col);
                    key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    key.extend_from_slice(row_key.as_bytes());
                    Some((key, row_key))
                })
            });
            let new_entry = new.and_then(|(table, row)| {
                row.get(col).filter(|v| !v.is_null()).and_then(|v| {
                    let row_key = Self::row_key(table, &Self::pk_of(schema, row).ok()?);
                    let mut key = index_prefix(table, col);
                    key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    key.extend_from_slice(row_key.as_bytes());
                    Some((key, row_key))
                })
            });
            match (old_entry, new_entry) {
                (Some((old_key, _)), Some((new_key, _))) if old_key == new_key => {}
                (old_e, new_e) => {
                    if let Some((old_key, _)) = old_e {
                        batch.push((crate::wal::WalOp::Delete, old_key, None));
                    }
                    if let Some((new_key, new_row_key)) = new_e {
                        batch.push((
                            crate::wal::WalOp::Put,
                            new_key,
                            Some(new_row_key.into_bytes()),
                        ));
                    }
                }
            }
        }
        batch
    }

    fn exec_drop_table(
        &self,
        table: &str,
        if_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        {
            let schemas = self.schemas.read();
            if !schemas.contains_key(table) {
                if if_exists {
                    return Ok((
                        vec!["status".into()],
                        vec![mk_row(&[("status", Value::Text("DROP_OK".into()))])],
                        format!("DropTable({}) [if-exists]", table),
                        None,
                    ));
                }
                return Err(format!("table '{}' does not exist", table));
            }
        }
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' does not exist", table))?;
        let rows = self.read_table_rows(table)?;
        if !schema.secondary_indexes.is_empty() || !rows.is_empty() {
            let mut batch: Vec<_> = rows
                .iter()
                .filter_map(|row| {
                    Self::pk_of(&schema, row)
                        .ok()
                        .map(|pk| Self::row_key(table, &pk))
                })
                .map(|key| (crate::wal::WalOp::Delete, key.into_bytes(), None))
                .collect();
            for row in &rows {
                batch.extend(Self::index_delta_batch(&schema, Some((table, row)), None));
            }
            self.engine.write_batch(&batch).map_err(|e| e.to_string())?;
        }
        // Sweep any leftover index entries (e.g. rows added out-of-band).
        let base = index_table_prefix(table);
        let leftovers = self.engine.scan_prefix(&base).map_err(|e| e.to_string())?;
        if !leftovers.is_empty() {
            let sweep: Vec<_> = leftovers
                .into_iter()
                .map(|(k, _)| (crate::wal::WalOp::Delete, k, None))
                .collect();
            self.engine.write_batch(&sweep).map_err(|e| e.to_string())?;
        }
        self.engine
            .delete(format!("__schema:{}", table).as_bytes())
            .map_err(|e| e.to_string())?;
        self.schemas.write().remove(table);
        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!("DropTable({})", table),
            None,
        ))
    }

    fn exec_insert(
        &self,
        table: &str,
        rows: Vec<Vec<(String, Value)>>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered (CREATE TABLE first)", table))?;

        // FK validation
        for row in &rows {
            for fk in &schema.foreign_keys {
                if let Some((_, v)) = row.iter().find(|(c, _)| c == &fk.column) {
                    if !v.is_null() {
                        let target_key = Self::row_key(&fk.target_chromosome, v);
                        let exists = self
                            .engine
                            .get(target_key.as_bytes())
                            .map_err(|e| e.to_string())?
                            .is_some();
                        if !exists {
                            return Err(format!(
                                "FK violation: {}={} not found in '{}'",
                                fk.column, v, fk.target_chromosome
                            ));
                        }
                    }
                }
            }
        }

        let mut batch = Vec::new();
        let mut inserted_keys = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();
        // UNIQUE indexes: (column, encoded value) -> owning row key, so two
        // rows of one multi-row INSERT cannot slip the same value past us.
        let mut stmt_unique: std::collections::HashMap<(String, Vec<u8>), String> =
            std::collections::HashMap::new();
        for row in rows {
            let map: BTreeMap<String, Value> = row.into_iter().collect();
            let pk = Self::pk_of(&schema, &map)?;
            let key = Self::row_key(table, &pk);
            if !seen_keys.insert(key.clone()) {
                return Err(format!("duplicate primary key in INSERT: {}", key));
            }
            if self
                .engine
                .get(key.as_bytes())
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(format!("duplicate primary key in INSERT: {}", key));
            }
            // UNIQUE index enforcement — also covers duplicates between rows
            // of this same statement (via stmt_unique) and against rows that
            // already exist in the engine (via index scan).
            for idx in schema.secondary_indexes.iter().filter(|i| i.unique) {
                let Some(v) = map.get(&idx.column) else {
                    continue;
                };
                if v.is_null() {
                    continue;
                }
                let enc = encode_ordered_value(v);
                if let Some(earlier) = stmt_unique.get(&(idx.column.clone(), enc.clone())) {
                    if earlier != &key {
                        return Err(format!(
                            "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                            table, idx.column, v, idx.name
                        ));
                    }
                }
                let mut prefix = index_prefix(table, &idx.column);
                prefix.extend_from_slice(&escape_index_part(&enc));
                let entries = self
                    .engine
                    .scan_prefix(&prefix)
                    .map_err(|e| e.to_string())?;
                if !entries.is_empty() {
                    return Err(format!(
                        "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                        table, idx.column, v, idx.name
                    ));
                }
                stmt_unique.insert((idx.column.clone(), enc), key.clone());
            }
            let payload = RowCodec::encode(
                &map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            // Secondary-index entries ride the same WAL batch (atomic with
            // the row: a crash can never leave a dangling index entry).
            batch.extend(Self::index_delta_batch(&schema, None, Some((table, &map))));
            batch.push((
                crate::wal::WalOp::Put,
                key.clone().into_bytes(),
                Some(payload),
            ));
            inserted_keys.push((key, map, pk));
        }

        self.engine
            .write_batch(&batch)
            .map_err(|e| format!("insert failed: {}", e))?;

        // maintain complement index
        {
            let mut idx = self.complement_index.write();
            for (key, map, _) in &inserted_keys {
                for fk in &schema.foreign_keys {
                    if let Some(v) = map.get(&fk.column) {
                        idx.upsert(table, &fk.column, &key.clone(), v);
                    }
                }
            }
        }

        let n = inserted_keys.len();
        let mut out_rows = Vec::new();
        for (key, _, pk) in &inserted_keys {
            let mut r = BTreeMap::new();
            r.insert("inserted_key".to_string(), Value::Text(key.clone()));
            r.insert("pk".to_string(), pk.clone());
            out_rows.push(r);
        }

        Ok((
            vec!["inserted_key".into(), "pk".into()],
            out_rows,
            format!("BatchInsert({}) rows={} via single WAL frame", table, n),
            Some(format!("{} row(s) inserted", n)),
        ))
    }

    fn exec_update(
        &self,
        table: &str,
        assignments: &[(String, Value)],
        where_clause: Option<Expr>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered", table))?;
        // Materialise IN-subqueries, then point-get when WHERE pins the PK.
        let where_clause = self.materialize_subqueries(where_clause)?;
        let pk_lits = where_clause
            .as_ref()
            .and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point(table, lit)? {
                        out.push(row);
                    }
                }
                out
            }
            None => self.read_table_rows(table)?,
        };

        for (column, _) in assignments {
            if !schema.columns.contains_key(column) {
                return Err(format!("unknown column '{}' in UPDATE", column));
            }
        }

        let mut changes = Vec::new();
        let mut affected_old_keys = std::collections::HashSet::new();
        for row in rows {
            let matches = where_clause
                .as_ref()
                .map(|e| e.eval_bool(&row))
                .unwrap_or(true);
            if !matches {
                continue;
            }
            let old_pk = Self::pk_of(&schema, &row)?;
            let old_key = Self::row_key(table, &old_pk);
            let mut new_row = row.clone();
            for (col, val) in assignments {
                new_row.insert(col.clone(), val.clone());
            }
            let pk = Self::pk_of(&schema, &new_row)?;
            let key = Self::row_key(table, &pk);
            affected_old_keys.insert(old_key.clone());
            changes.push((old_key, row, key, new_row));
        }

        let mut new_keys = std::collections::HashSet::new();
        for (old_key, _, new_key, new_row) in &changes {
            if !new_keys.insert(new_key.clone()) {
                return Err(format!(
                    "UPDATE would create duplicate primary key: {}",
                    new_key
                ));
            }
            if !affected_old_keys.contains(new_key)
                && self
                    .engine
                    .get(new_key.as_bytes())
                    .map_err(|error| error.to_string())?
                    .is_some()
            {
                return Err(format!(
                    "UPDATE conflicts with existing primary key: {}",
                    new_key
                ));
            }

            // UNIQUE secondary-index constraints (target row excluded).
            self.check_unique_violations(table, &schema, new_key, new_row, Some(old_key.as_str()))?;

            for fk in &schema.foreign_keys {
                if let Some(value) = new_row.get(&fk.column) {
                    if !value.is_null() {
                        let parent_key = Self::row_key(&fk.target_chromosome, value);
                        if self
                            .engine
                            .get(parent_key.as_bytes())
                            .map_err(|error| error.to_string())?
                            .is_none()
                        {
                            return Err(format!(
                                "FK violation: {}={} not found in '{}'",
                                fk.column, value, fk.target_chromosome
                            ));
                        }
                    }
                }
            }
        }

        let mut batch = Vec::new();
        for (old_key, _, new_key, _) in &changes {
            if old_key != new_key {
                batch.push((
                    crate::wal::WalOp::Delete,
                    old_key.clone().into_bytes(),
                    None,
                ));
            }
        }
        for (old_key, old_row, new_key, new_row) in &changes {
            // Index delta: old value entry removed / new one added in the
            // same atomic batch as the row rewrite.
            let old_pk = Self::pk_of(&schema, old_row)?;
            let old_table_key = Self::row_key(&schema.name, &old_pk);
            let new_pk = Self::pk_of(&schema, new_row)?;
            let new_table_key = Self::row_key(&schema.name, &new_pk);
            let _ = (old_key, new_key);
            batch.extend(Self::index_delta_batch(
                &schema,
                Some((schema.name.as_str(), old_row)),
                Some((schema.name.as_str(), new_row)),
            ));
            // Row-key move (pk change) also moves index suffixes: the delta
            // above already computed fresh keys from the new row/pk.
            let _ = (&old_table_key, &new_table_key);
            let payload = RowCodec::encode(
                &new_row
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            batch.push((
                crate::wal::WalOp::Put,
                new_key.clone().into_bytes(),
                Some(payload),
            ));
        }
        self.engine
            .write_batch(&batch)
            .map_err(|e| format!("update failed: {}", e))?;

        {
            let mut index = self.complement_index.write();
            for (old_key, old_row, _, _) in &changes {
                for fk in &schema.foreign_keys {
                    if let Some(value) = old_row.get(&fk.column) {
                        index.remove(&schema.name, &fk.column, old_key, value);
                    }
                }
            }
            for (_, _, new_key, new_row) in &changes {
                for fk in &schema.foreign_keys {
                    if let Some(value) = new_row.get(&fk.column) {
                        if !value.is_null() {
                            index.upsert(&schema.name, &fk.column, new_key, value);
                        }
                    }
                }
            }
        }

        let updated = changes.len();
        let scan_op = match &pk_lits {
            Some(lits) if lits.len() == 1 => format!("PointGet({} = {})", table, lits[0]),
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!("SeqScan({})", table),
        };
        Ok((
            vec!["affected_rows".into()],
            vec![mk_row(&[("affected_rows", Value::Int(updated as i64))])],
            format!(
                "{} -> Update({:?})",
                scan_op,
                assignments.iter().map(|(c, _)| c).collect::<Vec<_>>()
            ),
            Some(format!("{} row(s) updated", updated)),
        ))
    }

    fn exec_delete(
        &self,
        table: &str,
        where_clause: Option<Expr>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered", table))?;
        // Materialise IN-subqueries, then point-get when WHERE pins the PK.
        let where_clause = self.materialize_subqueries(where_clause)?;
        let pk_lits = where_clause
            .as_ref()
            .and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point(table, lit)? {
                        out.push(row);
                    }
                }
                out
            }
            None => self.read_table_rows(table)?,
        };

        // Which tables reference this one (for CASCADE)?
        let referencing: Vec<(String, ForeignKeyConstraint)> = {
            let schemas = self.schemas.read();
            schemas
                .iter()
                .flat_map(|(tname, t)| {
                    t.foreign_keys
                        .iter()
                        .filter(|fk| fk.target_chromosome == table)
                        .map(move |fk| (tname.clone(), fk.clone()))
                })
                .collect()
        };

        let mut selected = Vec::new();
        for row in rows {
            let matches = where_clause
                .as_ref()
                .map(|e| e.eval_bool(&row))
                .unwrap_or(true);
            if !matches {
                continue;
            }
            let pk = Self::pk_of(&schema, &row)?;
            selected.push((Self::row_key(table, &pk), pk, row));
        }

        let selected_keys: std::collections::HashSet<String> =
            selected.iter().map(|(key, _, _)| key.clone()).collect();
        let mut deletions: BTreeMap<String, (String, BTreeMap<String, Value>)> = BTreeMap::new();
        for (key, _, row) in &selected {
            deletions.insert(key.clone(), (table.to_string(), row.clone()));
        }
        for (_, parent_pk, _) in &selected {
            for (child_table, fk) in &referencing {
                let child_schema = self
                    .get_schema(child_table)
                    .ok_or_else(|| format!("referencing table '{}' has no schema", child_table))?;
                let child_rows = self.read_table_rows(child_table)?;
                for child in child_rows {
                    if child
                        .get(&fk.column)
                        .map(|value| value.compare(parent_pk) == std::cmp::Ordering::Equal)
                        .unwrap_or(false)
                    {
                        let child_pk = Self::pk_of(&child_schema, &child)?;
                        let child_key = Self::row_key(child_table, &child_pk);
                        if !fk.on_delete_cascade && !selected_keys.contains(&child_key) {
                            return Err(format!(
                                "RESTRICT violation: {} row {} is referenced by {}",
                                table, parent_pk, child_table
                            ));
                        }
                        if fk.on_delete_cascade {
                            deletions.insert(child_key, (child_table.clone(), child));
                        }
                    }
                }
            }
        }

        let deleted = selected.len();
        let cascaded = deletions.len().saturating_sub(deleted);
        let scan_op = match &pk_lits {
            Some(lits) if lits.len() == 1 => format!("PointGet({} = {})", table, lits[0]),
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!("SeqScan({})", table),
        };
        let mut batch: Vec<_> = deletions
            .keys()
            .map(|key| (crate::wal::WalOp::Delete, key.as_bytes().to_vec(), None))
            .collect();
        // Remove secondary-index entries for every deleted row (parents and
        // cascaded children alike).
        for (key, (del_table, row)) in &deletions {
            if let Some(del_schema) = self.get_schema(del_table) {
                batch.extend(Self::index_delta_batch(
                    &del_schema,
                    Some((del_table.as_str(), row)),
                    None,
                ));
                let _ = key;
            }
        }
        self.engine
            .write_batch(&batch)
            .map_err(|e| format!("delete failed: {}", e))?;

        {
            let mut index = self.complement_index.write();
            for (key, (deleted_table, row)) in &deletions {
                if let Some(deleted_schema) = self.get_schema(deleted_table) {
                    for fk in &deleted_schema.foreign_keys {
                        if let Some(value) = row.get(&fk.column) {
                            index.remove(deleted_table, &fk.column, key, value);
                        }
                    }
                }
            }
        }

        Ok((
            vec!["status".into(), "deleted".into(), "cascaded".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("deleted", Value::Int(deleted as i64)),
                ("cascaded", Value::Int(cascaded as i64)),
            ])],
            format!(
                "{} -> Delete{} ",
                scan_op,
                if referencing.is_empty() {
                    String::new()
                } else {
                    " + FK cascade scan".to_string()
                }
            ),
            Some(format!("{} deleted, {} cascaded", deleted, cascaded)),
        ))
    }

    fn exec_select(
        &self,
        projection: &[SelectItem],
        from: &str,
        joins: &[JoinSpec],
        where_clause: Option<Expr>,
        group_by: &[String],
        having: Option<Expr>,
        order_by: &[(String, bool)],
        limit: Option<usize>,
        offset: usize,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        // Materialise IN-subqueries before any planning.
        let where_clause = self.materialize_subqueries(where_clause)?;
        // FROM — candidates via point lookup / secondary index / seq scan.
        let (mut current, scan_op) = self.fetch_candidates(from, where_clause.as_ref())?;
        let mut plan = scan_op;

        // JOINs (hash join on equi-condition; O(n+m) build+probe)
        for j in joins {
            let right = self.read_table_rows(&j.table)?;
            let next = match j.join_type {
                JoinType::Inner => hash_join(&current, &right, &j.left_col, &j.right_col),
                JoinType::Left => hash_join_left(&current, &right, &j.left_col, &j.right_col),
            };
            plan.push_str(&format!(
                " -> {}HashJoin({}.{}, {}.{})",
                if j.join_type == JoinType::Left {
                    "Left"
                } else {
                    "Inner"
                },
                from,
                j.left_col,
                j.table,
                j.right_col
            ));
            current = next;
        }

        // WHERE
        if let Some(e) = &where_clause {
            plan.push_str(&format!(" -> Filter({})", e.describe()));
            current.retain(|r| e.eval_bool(r));
        }

        // Aggregation (GROUP BY / HAVING / aggregate projections).
        let agg_items = collect_agg_items(projection, having.as_ref());
        let has_agg = !group_by.is_empty() || !agg_items.is_empty() || having.is_some();
        if has_agg {
            current = self.aggregate_rows(
                current,
                group_by,
                &agg_items,
                having.as_ref(),
                from,
                &mut plan,
            )?;
        }

        // A computed projection item may be referenced from ORDER BY by its
        // output alias — materialise such items into the rows before sorting.
        if !projection.is_empty() && !projection.iter().any(|p| matches!(p, SelectItem::Star)) {
            for item in projection {
                if let SelectItem::Computed { expr, rendered } = item {
                    if order_by.iter().any(|(k, _)| k == rendered) {
                        for row in current.iter_mut() {
                            if !row.contains_key(rendered) {
                                let v = expr.eval(row).unwrap_or(Value::Null);
                                row.insert(rendered.clone(), v);
                            }
                        }
                    }
                }
            }
        }

        // ORDER BY (post-aggregation, pre-projection: sorting may reference
        // columns the projection would otherwise drop).
        if !order_by.is_empty() {
            plan.push_str(&format!(
                " -> Sort({})",
                order_by
                    .iter()
                    .map(|(k, asc)| format!("{} {}", k, if *asc { "ASC" } else { "DESC" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            current.sort_by(|a, b| {
                for (key, asc) in order_by {
                    let va = a.get(key).cloned().unwrap_or(Value::Null);
                    let vb = b.get(key).cloned().unwrap_or(Value::Null);
                    let ord = va.compare(&vb);
                    if ord != std::cmp::Ordering::Equal {
                        return if *asc { ord } else { ord.reverse() };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        // Projection (plain queries only; grouped rows already carry exactly
        // the group columns + aggregate results).
        if !has_agg {
            let star =
                projection.is_empty() || projection.iter().any(|p| matches!(p, SelectItem::Star));
            if !star {
                let mut columns: Vec<(String, String)> = Vec::new();
                let mut computed: Vec<(String, Expr)> = Vec::new();
                let mut specs: Vec<String> = Vec::new();
                for item in projection {
                    match item {
                        SelectItem::Column { source, alias } => {
                            let out = alias.clone().unwrap_or_else(|| source.clone());
                            specs.push(if out == *source {
                                source.clone()
                            } else {
                                format!("{} AS {}", source, out)
                            });
                            columns.push((source.clone(), out));
                        }
                        SelectItem::Computed { expr, rendered } => {
                            specs.push(format!("{} AS {}", expr.describe(), rendered));
                            computed.push((rendered.clone(), expr.clone()));
                        }
                        SelectItem::Agg { rendered, .. } => specs.push(rendered.clone()),
                        SelectItem::Star => {}
                    }
                }
                // A computed expression may only reference real columns of
                // the source table (plain columns keep the legacy behaviour
                // of being skipped when absent).
                if let Some(schema) = self.get_schema(&from) {
                    let mut refs = Vec::new();
                    for (_, expr) in &computed {
                        expr.collect_columns(&mut refs);
                    }
                    for c in refs {
                        if !schema.columns.contains_key(&c) {
                            return Err(format!("SQL: unknown column '{}' in projection", c));
                        }
                    }
                }
                plan.push_str(&format!(" -> Project({:?})", specs));
                current = current
                    .into_iter()
                    .map(|r| {
                        let mut out = BTreeMap::new();
                        for (src, out_name) in &columns {
                            if let Some(v) = r.get(src) {
                                out.insert(out_name.clone(), v.clone());
                            }
                        }
                        for (out_name, expr) in &computed {
                            let v = expr.eval(&r).unwrap_or(Value::Null);
                            out.insert(out_name.clone(), v);
                        }
                        out
                    })
                    .collect();
            }
        }

        // Output columns of an aggregated query: group columns + projected
        // aggregates (aliases). HAVING-only aggregates are internal and get
        // trimmed here, after ORDER BY has had its chance to reference them.
        if has_agg {
            let star =
                projection.is_empty() || projection.iter().any(|p| matches!(p, SelectItem::Star));
            if !star {
                let mut keep: Vec<String> = group_by.to_vec();
                for item in projection {
                    match item {
                        SelectItem::Column { source, alias } => {
                            let out = alias.clone().unwrap_or_else(|| source.clone());
                            if !keep.contains(&out) {
                                keep.push(out);
                            }
                        }
                        SelectItem::Agg { rendered, .. } => {
                            if !keep.contains(rendered) {
                                keep.push(rendered.clone());
                            }
                        }
                        SelectItem::Computed { rendered, .. } => {
                            if !keep.contains(rendered) {
                                keep.push(rendered.clone());
                            }
                        }
                        SelectItem::Star => {}
                    }
                }
                for row in current.iter_mut() {
                    row.retain(|k, _| keep.contains(k));
                }
                // Computed items evaluate against the aggregated rows (group
                // columns and aggregate results are all present at this point).
                for item in projection {
                    if let SelectItem::Computed { expr, rendered } = item {
                        for row in current.iter_mut() {
                            let v = expr.eval(row).unwrap_or(Value::Null);
                            row.insert(rendered.clone(), v);
                        }
                    }
                }
            }
        }

        // OFFSET + LIMIT (last two operators, SQL semantics).
        if offset > 0 {
            plan.push_str(&format!(" -> Offset({})", offset));
            current.drain(0..offset.min(current.len()));
        }
        if let Some(n) = limit {
            plan.push_str(&format!(" -> Limit({})", n));
            current.truncate(n);
        }

        // Column list for tabular consumers.
        let columns: Vec<String> = current
            .first()
            .map(|r| r.keys().cloned().collect())
            .unwrap_or_default();
        Ok((columns, current, plan, None))
    }

    /// Candidate rows for a FROM table: PK point lookup > secondary index
    /// (eq / IN-list / range) > SeqScan. The full WHERE is always re-applied
    /// afterwards, so a conservative candidate set only costs performance.
    fn fetch_candidates(
        &self,
        from: &str,
        where_clause: Option<&Expr>,
    ) -> Result<(Vec<BTreeMap<String, Value>>, String), String> {
        let schema = self.get_schema(from);
        if let Some(schema) = &schema {
            if let Some(lits) =
                where_clause.and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column))
            {
                let mut out = Vec::with_capacity(lits.len());
                for lit in &lits {
                    if let Some(row) = self.read_row_point(from, lit)? {
                        out.push(row);
                    }
                }
                let plan = if lits.len() == 1 {
                    format!("PointGet({} = {})", from, lits[0])
                } else {
                    format!(
                        "PointGet({} IN ({}))",
                        from,
                        lits.iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return Ok((out, plan));
            }
            if let Some(expr) = where_clause {
                if let Some((col, access)) = index_access_for(schema, expr) {
                    let keys = self.index_candidate_keys(from, &col, &access)?;
                    let mut out = Vec::with_capacity(keys.len());
                    let mut seen = std::collections::HashSet::new();
                    for row_key in keys {
                        if !seen.insert(row_key.clone()) {
                            continue;
                        }
                        if let Some(row) = self.read_row_by_key(&row_key)? {
                            out.push(row);
                        }
                    }
                    return Ok((out, format!("IndexScan({}.{} {})", from, col, access)));
                }
            }
        }
        Ok((self.read_table_rows(from)?, format!("SeqScan({})", from)))
    }

    /// Resolve secondary-index entries to row keys for an access descriptor.
    fn index_candidate_keys(
        &self,
        table: &str,
        column: &str,
        access: &IndexAccess,
    ) -> Result<Vec<String>, String> {
        let base = index_prefix(table, column);
        let to_payloads = |entries: Vec<(Vec<u8>, Vec<u8>)>| -> Vec<String> {
            entries
                .into_iter()
                .filter_map(|(_, payload)| String::from_utf8(payload).ok())
                .collect()
        };
        match access {
            IndexAccess::Eq(values) => {
                let mut out = Vec::new();
                for v in values {
                    let mut prefix = base.clone();
                    prefix.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    let entries = self
                        .engine
                        .scan_prefix(&prefix)
                        .map_err(|e| e.to_string())?;
                    out.extend(to_payloads(entries));
                }
                Ok(out)
            }
            IndexAccess::Range { lo, hi } => {
                let mut start = base.clone();
                match lo {
                    Some((v, inclusive)) => {
                        let mut esc = escape_index_part(&encode_ordered_value(v));
                        if !inclusive {
                            // terminator byte bumped: strictly past every
                            // entry of v, still below any greater value.
                            *esc.last_mut().unwrap() = 0x01;
                        }
                        start.extend_from_slice(&esc);
                    }
                    None => {}
                }
                let mut end = base.clone();
                match hi {
                    Some((v, inclusive)) => {
                        let mut esc = escape_index_part(&encode_ordered_value(v));
                        if *inclusive {
                            // past every entry of v (incl. prefix extensions).
                            *esc.last_mut().unwrap() = 0x01;
                        }
                        // exclusive: esc(v) itself is the range end — every
                        // key of v extends it and therefore lands >= end.
                        end.extend_from_slice(&esc);
                    }
                    None => {
                        // exclusive end of the whole index: increment base.
                        end = next_prefix(&base);
                    }
                }
                let entries = self
                    .engine
                    .scan_range(&start, &end)
                    .map_err(|e| e.to_string())?;
                Ok(to_payloads(entries))
            }
        }
    }

    /// Replace IN-subquery nodes with materialised IN-lists.
    fn materialize_subqueries(&self, expr: Option<Expr>) -> Result<Option<Expr>, String> {
        fn walk(sql: &SqlEngine, e: &Expr) -> Result<Expr, String> {
            match e {
                Expr::InSubquery { column, query } => {
                    let sub = sql.execute(query)?;
                    let col = sub
                        .columns
                        .first()
                        .cloned()
                        .ok_or_else(|| "SQL: IN subquery returned no columns".to_string())?;
                    let values: Vec<Value> = sub
                        .rows
                        .iter()
                        .filter_map(|r| r.get(&col).cloned())
                        .collect();
                    Ok(Expr::InList {
                        column: column.clone(),
                        values,
                    })
                }
                Expr::InList { .. } | Expr::Literal(_) | Expr::Column(_) => Ok(e.clone()),
                Expr::Not(inner) => Ok(Expr::Not(Box::new(walk(sql, inner)?))),
                Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(walk(sql, inner)?))),
                Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(walk(sql, inner)?))),
                Expr::Arith { op, left, right } => Ok(Expr::Arith {
                    op: *op,
                    left: Box::new(walk(sql, left)?),
                    right: Box::new(walk(sql, right)?),
                }),
                Expr::And(a, b) => Ok(Expr::And(Box::new(walk(sql, a)?), Box::new(walk(sql, b)?))),
                Expr::Or(a, b) => Ok(Expr::Or(Box::new(walk(sql, a)?), Box::new(walk(sql, b)?))),
                Expr::Cmp { op, left, right } => Ok(Expr::Cmp {
                    op: *op,
                    left: Box::new(walk(sql, left)?),
                    right: Box::new(walk(sql, right)?),
                }),
            }
        }
        match expr {
            None => Ok(None),
            Some(e) => Ok(Some(walk(self, &e)?)),
        }
    }

    /// GROUP BY + aggregates + HAVING over an already-filtered row set.
    /// Aggregates are computed under their canonical name (`COUNT(*)`),
    /// HAVING evaluates against those canonical keys, then each surviving
    /// row is renamed to the projection's output alias before returning.
    fn aggregate_rows(
        &self,
        rows: Vec<BTreeMap<String, Value>>,
        group_by: &[String],
        agg_items: &[(String, String, AggFunc, Option<String>)],
        having: Option<&Expr>,
        table: &str,
        plan: &mut String,
    ) -> Result<Vec<BTreeMap<String, Value>>, String> {
        if let Some(schema) = self.get_schema(table) {
            for col in group_by {
                if !schema.columns.contains_key(col) {
                    return Err(format!("SQL: unknown column '{}' in GROUP BY", col));
                }
            }
        }
        plan.push_str(&format!(
            " -> Aggregate(group_by={:?}, aggs={:?})",
            group_by,
            agg_items
                .iter()
                .map(|(n, _, _, _)| n.clone())
                .collect::<Vec<_>>()
        ));

        // Group accumulator state.
        struct GroupState {
            sample: BTreeMap<String, Value>,
            // per aggregate: count(*), count(non-null), numeric sum, min, max
            counts: Vec<u64>,
            non_null: Vec<u64>,
            sums: Vec<f64>,
            saw_float: Vec<bool>,
            mins: Vec<Option<Value>>,
            maxs: Vec<Option<Value>>,
        }
        let n_agg = agg_items.len();
        let mut groups: std::collections::BTreeMap<Vec<u8>, GroupState> =
            std::collections::BTreeMap::new();

        for row in &rows {
            let mut key: Vec<u8> = Vec::new();
            for col in group_by {
                let v = row.get(col).cloned().unwrap_or(Value::Null);
                key.extend_from_slice(&encode_index_value(&v));
                key.push(0);
            }
            let state = groups.entry(key).or_insert_with(|| GroupState {
                sample: {
                    let mut m = BTreeMap::new();
                    for col in group_by {
                        m.insert(col.clone(), row.get(col).cloned().unwrap_or(Value::Null));
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
            for (i, (_, _, func, arg)) in agg_items.iter().enumerate() {
                state.counts[i] += 1;
                let value = match arg {
                    // COUNT(*): every group member counts, no column read.
                    None => {
                        state.non_null[i] += 1;
                        None
                    }
                    Some(col) => row.get(col),
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
        if groups.is_empty() && group_by.is_empty() {
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
            let mut row = state.sample;
            for (i, (canonical, _output, func, _)) in agg_items.iter().enumerate() {
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
                row.insert(canonical.clone(), v);
            }
            if let Some(h) = having {
                if !h.eval_bool(&row) {
                    continue;
                }
            }
            // Expose output aliases next to canonical keys (both present
            // during ORDER BY; non-projected canonical keys are trimmed in
            // exec_select after sorting).
            for (canonical, output, _, _) in agg_items {
                if canonical != output {
                    if let Some(v) = row.get(canonical).cloned() {
                        row.insert(output.clone(), v);
                    }
                }
            }
            out.push(row);
        }
        if having.is_some() {
            plan.push_str(" -> Having");
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Secondary-index planning helpers
// ---------------------------------------------------------------------------

/// Access descriptor for one indexed column, extracted from WHERE.
enum IndexAccess {
    /// `col = v` or `col IN (...)`: exact value set.
    Eq(Vec<Value>),
    /// Range with inclusive/exclusive bounds: `col > a AND col <= b`.
    Range {
        lo: Option<(Value, bool)>,
        hi: Option<(Value, bool)>,
    },
}

impl std::fmt::Display for IndexAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexAccess::Eq(values) => write!(
                f,
                "IN ({})",
                values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            IndexAccess::Range { lo, hi } => {
                let mut parts = Vec::new();
                if let Some((v, incl)) = lo {
                    parts.push(format!("{}{}", if *incl { ">=" } else { ">" }, v));
                }
                if let Some((v, incl)) = hi {
                    parts.push(format!("{}{}", if *incl { "<=" } else { "<" }, v));
                }
                if parts.is_empty() {
                    write!(f, "full index scan")
                } else {
                    write!(f, "{}", parts.join(" AND "))
                }
            }
        }
    }
}

/// Key prefix of one column's index entries.
fn index_prefix(table: &str, column: &str) -> Vec<u8> {
    format!("__idx:{}:{}:", table, column).into_bytes()
}

/// Key prefix of every index entry of a whole table.
fn index_table_prefix(table: &str) -> Vec<u8> {
    format!("__idx:{}:", table).into_bytes()
}

/// Order-preserving, self-delimiting encoding of an index value part:
/// internal 0x00 bytes are escaped as 0x00 0xFF, the part ends with 0x00 0x00.
fn escape_index_part(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// Smallest byte string strictly greater than every string starting with
/// `prefix` (used as an exclusive range end).
fn next_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last == 0xFF {
            out.pop();
        } else {
            *last += 1;
            return out;
        }
    }
    vec![0xFF] // degenerate: all-0xFF prefix
}

/// Extract an index-backed access from the WHERE conjunction tree.
/// Returns the first indexed column usable as a candidate filter; the full
/// WHERE is re-applied later, so this only needs to be conservative.
fn index_access_for(
    schema: &crate::relational::ChromosomeTable,
    expr: &Expr,
) -> Option<(String, IndexAccess)> {
    // Flatten the AND tree into conjuncts.
    let mut conjuncts = Vec::new();
    fn flatten<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(a, b) => {
                flatten(a, out);
                flatten(b, out);
            }
            other => out.push(other),
        }
    }
    flatten(expr, &mut conjuncts);

    // Pass 1: equality / IN-list on an indexed column.
    for c in &conjuncts {
        match c {
            Expr::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            } => {
                if let (Some(col), Some(v)) = (as_column(left), as_literal(right)) {
                    if schema.secondary_index_on(&col).is_some() && !v.is_null() {
                        return Some((col, IndexAccess::Eq(vec![v])));
                    }
                }
                if let (Some(v), Some(col)) = (as_literal(left), as_column(right)) {
                    if schema.secondary_index_on(&col).is_some() && !v.is_null() {
                        return Some((col, IndexAccess::Eq(vec![v])));
                    }
                }
            }
            Expr::InList { column, values } => {
                if schema.secondary_index_on(column).is_some()
                    && !values.is_empty()
                    && values.iter().all(|v| !v.is_null())
                {
                    return Some((column.clone(), IndexAccess::Eq(values.clone())));
                }
            }
            _ => {}
        }
    }

    // Pass 2: range bounds on an indexed column (tightest wins per side).
    let mut best: Option<(String, Option<(Value, bool)>, Option<(Value, bool)>)> = None;
    for c in &conjuncts {
        let Expr::Cmp { op, left, right } = c else {
            continue;
        };
        let (col, value, col_on_left) = match (as_column(left), as_literal(right)) {
            (Some(col), Some(v)) => (col, v, true),
            _ => match (as_literal(left), as_column(right)) {
                (Some(v), Some(col)) => (col, v, false),
                _ => continue,
            },
        };
        if schema.secondary_index_on(&col).is_none() || value.is_null() {
            continue;
        }
        // Normalise so the column is on the left (flip operator).
        let (op, col_on_left) = if col_on_left {
            (*op, true)
        } else {
            (
                match op {
                    CmpOp::Lt => CmpOp::Gt,
                    CmpOp::Le => CmpOp::Ge,
                    CmpOp::Gt => CmpOp::Lt,
                    CmpOp::Ge => CmpOp::Le,
                    other => *other,
                },
                true,
            )
        };
        let _ = col_on_left;
        let bound = match op {
            CmpOp::Gt => Some((value.clone(), false)),
            CmpOp::Ge => Some((value.clone(), true)),
            CmpOp::Lt => Some((value.clone(), false)),
            CmpOp::Le => Some((value.clone(), true)),
            _ => None,
        };
        let Some(bound) = bound else { continue };
        let entry = best.get_or_insert_with(|| (col.clone(), None, None));
        if entry.0 != col {
            continue; // one indexed column per scan; others stay as filters
        }
        let is_lower = matches!(op, CmpOp::Gt | CmpOp::Ge);
        if is_lower {
            let replace = match &entry.1 {
                None => true,
                Some((cur, cur_incl)) => {
                    bound.0.compare(cur) == std::cmp::Ordering::Greater
                        || (bound.0.compare(cur) == std::cmp::Ordering::Equal
                            && !bound.1
                            && *cur_incl)
                }
            };
            if replace {
                entry.1 = Some(bound);
            }
        } else {
            let replace = match &entry.2 {
                None => true,
                Some((cur, cur_incl)) => {
                    bound.0.compare(cur) == std::cmp::Ordering::Less
                        || (bound.0.compare(cur) == std::cmp::Ordering::Equal
                            && !bound.1
                            && *cur_incl)
                }
            };
            if replace {
                entry.2 = Some(bound);
            }
        }
    }
    best.map(|(col, lo, hi)| (col, IndexAccess::Range { lo, hi }))
}

fn as_column(e: &Expr) -> Option<String> {
    match e {
        Expr::Column(c) => Some(c.clone()),
        _ => None,
    }
}

fn as_literal(e: &Expr) -> Option<Value> {
    match e {
        Expr::Literal(v) => Some(v.clone()),
        _ => None,
    }
}

/// Aggregate items referenced by the projection or HAVING, as
/// (canonical name, output name, function, argument column) — deduplicated
/// by canonical name.
fn collect_agg_items(
    projection: &[SelectItem],
    having: Option<&Expr>,
) -> Vec<(String, String, AggFunc, Option<String>)> {
    let mut items: Vec<(String, String, AggFunc, Option<String>)> = Vec::new();
    let mut push = |name: String, out: String, func: AggFunc, arg: Option<String>| {
        if !items.iter().any(|(n, _, _, _)| *n == name) {
            items.push((name, out, func, arg));
        }
    };
    for item in projection {
        if let SelectItem::Agg {
            func,
            arg,
            rendered,
            canonical,
        } = item
        {
            push(canonical.clone(), rendered.clone(), *func, arg.clone());
        }
    }
    // Aggregates appearing only inside HAVING still need computing (they
    // stay internal: output name = canonical, stripped after filtering if
    // not projected).
    if let Some(h) = having {
        let mut cols = Vec::new();
        h.collect_columns(&mut cols);
        for c in cols {
            if let Some((func, arg)) = parse_rendered_agg(&c) {
                push(c.clone(), c, func, arg);
            }
        }
    }
    items
}

/// Inverse of `render_agg`: "COUNT(*)" -> (Count, None).
fn parse_rendered_agg(name: &str) -> Option<(AggFunc, Option<String>)> {
    let (func, rest) = name.split_once('(')?;
    let arg = rest.strip_suffix(')')?;
    let func = AggFunc::from_name(func).ok()?;
    let arg = if arg == "*" || arg.is_empty() {
        None
    } else {
        Some(arg.to_string())
    };
    Some((func, arg))
}

fn mk_row(fields: &[(&str, Value)]) -> BTreeMap<String, Value> {
    fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Literal typing rule shared by INSERT values and WHERE operands:
/// non-empty pure-ACGT strings become `Value::Dna` (uppercased), everything
/// else stays `Value::Text`.
fn typed_string_literal(s: &str) -> Value {
    if !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'))
    {
        Value::Dna(s.to_uppercase())
    } else {
        Value::Text(s.to_string())
    }
}

/// Canonical output name of an aggregate call: `COUNT(*)`, `SUM(balance)`.
fn render_agg(func: &str, arg: Option<&str>) -> String {
    format!("{}({})", func.to_ascii_uppercase(), arg.unwrap_or("*"))
}

/// SQL rendering of a parsed expression (string literals re-quoted) — used
/// to serialise IN-subqueries for re-execution.
fn expr_to_sql(e: &Expr) -> String {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
    match e {
        Expr::Literal(Value::Text(s)) | Expr::Literal(Value::Dna(s)) => quote(s),
        Expr::Literal(v) => v.to_string(),
        Expr::Column(c) => c.clone(),
        Expr::Not(inner) => format!("NOT {}", expr_to_sql(inner)),
        Expr::And(a, b) => format!("({} AND {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::Or(a, b) => format!("({} OR {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::IsNull(inner) => format!("{} IS NULL", expr_to_sql(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_sql(inner)),
        Expr::Arith { op, left, right } => {
            format!(
                "{} {} {}",
                expr_to_sql(left),
                op.as_str(),
                expr_to_sql(right)
            )
        }
        Expr::Cmp { op, left, right } => {
            format!(
                "{} {} {}",
                expr_to_sql(left),
                op.as_str(),
                expr_to_sql(right)
            )
        }
        Expr::InList { column, values } => format!(
            "{} IN ({})",
            column,
            values
                .iter()
                .map(|v| match v {
                    Value::Text(s) | Value::Dna(s) => quote(s),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::InSubquery { column, query } => format!("{} IN ({})", column, query),
    }
}

/// Render a parsed SELECT back to SQL text (for IN-subquery materialisation).
fn render_select_sql(stmt: &Stmt) -> String {
    let Stmt::Select {
        projection,
        from,
        joins,
        where_clause,
        group_by,
        having,
        order_by,
        limit,
        offset,
    } = stmt
    else {
        return "<non-select>".into();
    };
    let proj = if projection.is_empty() || projection.iter().any(|p| matches!(p, SelectItem::Star))
    {
        "*".to_string()
    } else {
        projection
            .iter()
            .map(|item| match item {
                SelectItem::Star => "*".to_string(),
                SelectItem::Column { source, alias } => match alias {
                    Some(a) if a != source => format!("{} AS {}", source, a),
                    _ => source.clone(),
                },
                SelectItem::Computed { expr, rendered } => {
                    let base = expr_to_sql(expr);
                    if *rendered == expr.describe() {
                        base
                    } else {
                        format!("{} AS {}", base, rendered)
                    }
                }
                SelectItem::Agg { canonical, .. } => canonical.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!("SELECT {} FROM {}", proj, from);
    for j in joins {
        out.push_str(&format!(
            " {} JOIN {} ON {} = {}",
            match j.join_type {
                JoinType::Inner => "INNER",
                JoinType::Left => "LEFT",
            },
            j.table,
            j.left_col,
            j.right_col
        ));
    }
    if let Some(w) = where_clause {
        out.push_str(&format!(" WHERE {}", expr_to_sql(w)));
    }
    if !group_by.is_empty() {
        out.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
    }
    if let Some(h) = having {
        out.push_str(&format!(" HAVING {}", expr_to_sql(h)));
    }
    for (key, asc) in order_by {
        out.push_str(&format!(
            " ORDER BY {} {}",
            key,
            if *asc { "ASC" } else { "DESC" }
        ));
    }
    if let Some(n) = limit {
        out.push_str(&format!(" LIMIT {}", n));
    }
    if *offset > 0 {
        out.push_str(&format!(" OFFSET {}", offset));
    }
    out
}

/// Inner hash join on equi-condition left_col = right_col.
/// Column collisions: right-side columns are prefixed '<table>.'.
fn hash_join(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    use std::collections::HashMap;
    let mut index: HashMap<Vec<u8>, Vec<&BTreeMap<String, Value>>> = HashMap::new();
    for r in right {
        if let Some(v) = r.get(right_col) {
            index
                .entry(crate::relational::encode_index_value(v))
                .or_default()
                .push(r);
        }
    }
    let mut out = Vec::new();
    for l in left {
        if let Some(v) = l.get(left_col) {
            if let Some(matches) = index.get(&crate::relational::encode_index_value(v)) {
                for r in matches {
                    let mut merged = l.clone();
                    for (k, val) in r.iter() {
                        let name = if merged.contains_key(k) {
                            format!("joined_{}", k)
                        } else {
                            k.clone()
                        };
                        merged.insert(name, val.clone());
                    }
                    out.push(merged);
                }
            }
        }
    }
    out
}

/// LEFT hash join: unmatched left rows are kept with NULLs for right columns.
fn hash_join_left(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    let mut out = hash_join(left, right, left_col, right_col);
    // find which left rows got no match: recompute matched set
    use std::collections::HashSet;
    let matched_keys: HashSet<Vec<u8>> = out
        .iter()
        .filter_map(|r| r.get(left_col))
        .map(crate::relational::encode_index_value)
        .collect();
    let right_cols: Vec<String> = right
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for l in left {
        let key = l.get(left_col).map(crate::relational::encode_index_value);
        let matched = key
            .as_ref()
            .map(|k| matched_keys.contains(k))
            .unwrap_or(false);
        if !matched {
            let mut row = l.clone();
            for c in &right_cols {
                if !row.contains_key(c) {
                    row.insert(c.clone(), Value::Null);
                }
            }
            out.push(row);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::wal::SyncMode;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_sql_{}_{}",
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

    fn setup() -> (Arc<Engine>, SqlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("sql"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine.clone());
        sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))").unwrap();
        sql.execute("CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, PRIMARY KEY (order_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id) ON DELETE CASCADE)").unwrap();
        (engine, sql)
    }

    #[test]
    fn create_insert_select() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 1500.50)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 890.00)",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1042, user_id = 12, total_usd = 350.00, status = 'PENDING')").unwrap();

        let res = sql
            .execute("SELECT user_id, username FROM chr_users WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("dr_watson".into()))
        );
        assert!(res.execution_plan.contains("Filter"));
    }

    #[test]
    fn select_where_pk_uses_point_get() {
        let (_e, sql) = setup();
        for i in 0..200 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.5)",
                i, i, i
            ))
            .unwrap();
        }

        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("u137".into()))
        );
        assert!(
            res.execution_plan.contains("PointGet"),
            "plan: {}",
            res.execution_plan
        );
        assert!(
            !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // nonexistent pk: point get misses without scanning
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 99999")
            .unwrap();
        assert!(res.rows.is_empty());
        assert!(res.execution_plan.contains("PointGet"));

        // OR of two pk equalities -> two point gets
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 3 OR user_id = 7")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(
            res.execution_plan.contains("PointGet") && res.execution_plan.contains("IN (3, 7)"),
            "plan: {}",
            res.execution_plan
        );

        // extra predicate beyond the pk still filters correctly
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137 AND balance > 200")
            .unwrap();
        assert!(res.rows.is_empty());
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137 AND balance > 100")
            .unwrap();
        assert_eq!(res.rows.len(), 1);

        // non-pk predicate -> honest SeqScan fallback
        let res = sql
            .execute("SELECT username FROM chr_users WHERE username = 'u50'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // NULL literal must not take the point path (pk = NULL is never true)
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = NULL")
            .unwrap();
        assert!(res.rows.is_empty());
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn point_get_with_text_pk_and_joins() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_genes (gene_id TEXT, name TEXT, PRIMARY KEY (gene_id))")
            .unwrap();
        sql.execute("INSERT INTO chr_genes (gene_id = 'BRCA1', name = 'repair')")
            .unwrap();
        sql.execute("INSERT INTO chr_genes (gene_id = 'TP53', name = 'apoptosis')")
            .unwrap();

        let res = sql
            .execute("SELECT name FROM chr_genes WHERE gene_id = 'TP53'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("name"),
            Some(&Value::Text("apoptosis".into()))
        );
        assert!(res.execution_plan.contains("PointGet"));

        // pk point get as the left side of a JOIN stays correct
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 5.0, status = 'OK')",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 2, user_id = 12, total_usd = 7.0, status = 'OK')",
        )
        .unwrap();
        let res = sql
            .execute(
                "SELECT order_id, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE order_id = 2",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1, "rows: {:?}", res.rows);
        assert_eq!(res.rows[0].get("total_usd"), Some(&Value::Float(7.0)));
        assert!(
            res.execution_plan.contains("PointGet") && res.execution_plan.contains("HashJoin"),
            "plan: {}",
            res.execution_plan
        );
    }

    #[test]
    fn update_and_delete_where_pk_use_point_get() {
        let (e, sql) = setup();
        for i in 0..50 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.0)",
                i, i, i
            ))
            .unwrap();
        }

        let res = sql
            .execute("UPDATE chr_users SET balance = 999.0 WHERE user_id = 25")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));
        assert!(
            res.execution_plan.contains("PointGet") && !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );
        let raw = e.get(b"chr_users:25").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(999.0)));

        // extra predicate: point get fetches, WHERE still filters
        let res = sql
            .execute("UPDATE chr_users SET balance = 1.0 WHERE user_id = 25 AND balance > 2000")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(0)));

        let res = sql
            .execute("DELETE FROM chr_users WHERE user_id = 25")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert!(
            res.execution_plan.contains("PointGet"),
            "plan: {}",
            res.execution_plan
        );
        assert!(e.get(b"chr_users:25").unwrap().is_none());

        // non-pk WHERE keeps SeqScan semantics
        let res = sql
            .execute("DELETE FROM chr_users WHERE username = 'u30'")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn tokenizer_negative_numbers_and_escaped_quotes() {
        // negative int / float literals
        let toks = stokenize("UPDATE t SET a = -1, b = -2.5").unwrap();
        assert!(toks.iter().any(|t| matches!(t, STok::Int(i) if *i == -1)));
        assert!(toks
            .iter()
            .any(|t| matches!(t, STok::Float(f) if (*f - -2.5).abs() < f64::EPSILON)));

        // doubled single quote inside a string is a literal quote
        let toks = stokenize("UPDATE t SET name = 'O''Brien'").unwrap();
        assert!(toks
            .iter()
            .any(|t| matches!(t, STok::Str(s) if s == "O'Brien")));
    }

    #[test]
    fn update_with_negative_and_quoted_values() {
        let (e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 7, username = 'plain', balance = 100.0)")
            .unwrap();

        let res = sql
            .execute(
                "UPDATE chr_users SET balance = -25.5, username = 'O''Brien' WHERE user_id = 7",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));

        let raw = e.get(b"chr_users:7").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(-25.5)));
        assert!(fields
            .iter()
            .any(|(k, v)| k == "username" && *v == Value::Text("O'Brien".into())));
    }

    #[test]
    fn unique_index_rejects_duplicate_insert() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alice', balance = 10.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.0)")
            .unwrap();

        sql.execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap();

        // duplicate value rejected
        let err = sql
            .execute("INSERT INTO chr_users (user_id = 3, username = 'alice', balance = 30.0)")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");
        // nothing was written
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE user_id = 3")
            .unwrap();
        assert!(res.rows.is_empty());

        // distinct value accepted
        sql.execute("INSERT INTO chr_users (user_id = 4, username = 'carol', balance = 40.0)")
            .unwrap();

        // multi-row INSERT with an in-statement duplicate rejected
        let err = sql
            .execute(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (5, 'dave', 1.0), (6, 'dave', 2.0)",
            )
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");

        // same statement with distinct values accepted atomically
        sql.execute(
            "INSERT INTO chr_users (user_id, username, balance) VALUES (7, 'eve', 1.0), (8, 'frank', 2.0)",
        )
        .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'eve'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn unique_index_rejects_duplicate_update_and_allows_nulls() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alice', balance = 10.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.0)")
            .unwrap();
        sql.execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap();

        // updating one row onto another's unique value is rejected
        let err = sql
            .execute("UPDATE chr_users SET username = 'alice' WHERE user_id = 2")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");

        // moving to a free value works
        sql.execute("UPDATE chr_users SET username = 'robert' WHERE user_id = 2")
            .unwrap();

        // NULL is not a value: multiple NULL usernames never conflict
        sql.execute("CREATE TABLE chr_notes (id INT, owner TEXT, note TEXT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 1, owner = NULL, note = 'a')")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 2, owner = NULL, note = 'b')")
            .unwrap();
        sql.execute("CREATE UNIQUE INDEX uq_notes_owner ON chr_notes (owner)")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 3, owner = NULL, note = 'c')")
            .unwrap();
        // but a real duplicate still fails
        sql.execute("INSERT INTO chr_notes (id = 4, owner = 'zed', note = 'd')")
            .unwrap();
        let err = sql
            .execute("INSERT INTO chr_notes (id = 5, owner = 'zed', note = 'e')")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");
    }

    #[test]
    fn unique_index_creation_fails_on_existing_duplicates() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'dup', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'dup', balance = 2.0)")
            .unwrap();

        let err = sql
            .execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap_err();
        assert!(err.contains("cannot create UNIQUE index"), "err: {err}");
        // index must not have been registered
        let schema = sql.get_schema("chr_users").unwrap();
        assert!(schema.secondary_index_on("username").is_none());

        // plain CREATE INDEX on the same data still succeeds
        sql.execute("CREATE INDEX idx_users_username ON chr_users (username)")
            .unwrap();
        let schema = sql.get_schema("chr_users").unwrap();
        let idx = schema.secondary_index_on("username").unwrap();
        assert!(!idx.unique);
    }

    #[test]
    fn hash_join_select() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 1500.50)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 890.00)",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1042, user_id = 12, total_usd = 350.00, status = 'OK')").unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1089, user_id = 14, total_usd = 740.00, status = 'OK')").unwrap();

        let res = sql.execute(
            "SELECT order_id, username, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE total_usd > 300"
        ).unwrap();
        assert_eq!(res.rows.len(), 2, "rows: {:?}", res.rows);
        assert!(res.execution_plan.contains("HashJoin"));
        assert!(res.execution_plan.contains("Filter"));
        assert!(res.execution_plan.contains("Project"));
    }

    #[test]
    fn fk_violation_rejected() {
        let (_e, sql) = setup();
        let err = sql.execute("INSERT INTO chr_orders (order_id = 1, user_id = 999, total_usd = 10.0, status = 'X')").unwrap_err();
        assert!(err.contains("FK violation"), "{}", err);
    }

    #[test]
    fn update_and_delete_with_cascade() {
        let (e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 100.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 50.0, status = 'X')",
        )
        .unwrap();

        let res = sql
            .execute("UPDATE chr_users SET balance = 200.00 WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));

        let raw = e.get(b"chr_users:12").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(200.0)));

        // DELETE with cascade removes user + orders
        let res = sql
            .execute("DELETE FROM chr_users WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert_eq!(res.rows[0].get("cascaded"), Some(&Value::Int(1)));
        assert!(e.get(b"chr_users:12").unwrap().is_none());
        assert!(e.get(b"chr_orders:1").unwrap().is_none());
    }

    #[test]
    fn update_primary_key_moves_row_without_leaving_stale_key() {
        let (engine, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("UPDATE chr_users SET user_id = 13 WHERE user_id = 12")
            .unwrap();
        assert!(engine.get(b"chr_users:12").unwrap().is_none());
        assert!(engine.get(b"chr_users:13").unwrap().is_some());
    }

    #[test]
    fn delete_restrict_blocks_referenced_parent() {
        let (engine, sql) = setup();
        sql.execute(
            "CREATE TABLE chr_notes (note_id INT, user_id INT, PRIMARY KEY (note_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (note_id = 1, user_id = 12)")
            .unwrap();

        let error = sql
            .execute("DELETE FROM chr_users WHERE user_id = 12")
            .unwrap_err();
        assert!(error.contains("RESTRICT"), "{error}");
        assert!(engine.get(b"chr_users:12").unwrap().is_some());
        assert!(engine.get(b"chr_notes:1").unwrap().is_some());
    }

    #[test]
    fn complement_index_updates_removes_and_rebuilds() {
        let (engine, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 14, username = 'b', balance = 2.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 5, user_id = 12, total_usd = 9.0, status = 'S')",
        )
        .unwrap();
        sql.execute("UPDATE chr_orders SET user_id = 14 WHERE order_id = 5")
            .unwrap();

        {
            let mut index = sql.complement_index.write();
            assert!(index
                .lookup_all("chr_orders", "user_id", &Value::Int(12))
                .is_empty());
            assert_eq!(
                index.lookup_all("chr_orders", "user_id", &Value::Int(14)),
                vec!["chr_orders:5".to_string()]
            );
        }
        sql.execute("DELETE FROM chr_orders WHERE order_id = 5")
            .unwrap();
        {
            let mut index = sql.complement_index.write();
            assert!(index
                .lookup_all("chr_orders", "user_id", &Value::Int(14))
                .is_empty());
        }

        sql.execute(
            "INSERT INTO chr_orders (order_id = 6, user_id = 12, total_usd = 3.0, status = 'S')",
        )
        .unwrap();
        let reloaded = SqlEngine::new(engine);
        let mut index = reloaded.complement_index.write();
        assert_eq!(
            index.lookup_all("chr_orders", "user_id", &Value::Int(12)),
            vec!["chr_orders:6".to_string()]
        );
    }

    #[test]
    fn persistence_across_restart() {
        let dir = tempdir("persist");
        {
            let engine = Engine::open(EngineConfig {
                dir_path: dir.clone(),
                memtable_capacity: 1024 * 1024,
                sync_mode: SyncMode::Strict,
                mirror_disk_path: None,
                ..Default::default()
            })
            .unwrap();
            let sql = SqlEngine::new(engine.clone());
            sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))").unwrap();
            sql.execute(
                "INSERT INTO chr_users (user_id = 7, username = 'rosalind_f', balance = 999.99)",
            )
            .unwrap();
        }
        // Reopen: schema + rows must reload
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine);
        assert_eq!(sql.list_tables(), vec!["chr_users".to_string()]);
        let res = sql
            .execute("SELECT username, balance FROM chr_users WHERE user_id = 7")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("rosalind_f".into()))
        );
    }

    #[test]
    fn left_join_keeps_unmatched() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 100.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 200.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 10.0, status = 'A')",
        )
        .unwrap();

        let res = sql.execute(
            "SELECT username, order_id FROM chr_users LEFT JOIN chr_orders ON user_id = user_id"
        ).unwrap();
        assert_eq!(res.rows.len(), 2); // both users present
        let crick = res
            .rows
            .iter()
            .find(|r| r.get("username") == Some(&Value::Text("prof_crick".into())))
            .unwrap();
        assert_eq!(crick.get("order_id"), Some(&Value::Null));
    }

    #[test]
    fn limit_and_multi_insert() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 13, username = 'b', balance = 2.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 14, username = 'c', balance = 3.0)")
            .unwrap();
        let res = sql
            .execute("SELECT username FROM chr_users LIMIT 2")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(res.execution_plan.contains("Limit(2)"));
    }

    #[test]
    fn dna_values_stored_typed() {
        let (e, sql) = setup();
        sql.execute("CREATE TABLE chr_variants (variant_id INT, variant_name TEXT, sequence DNA, PRIMARY KEY (variant_id))").unwrap();
        sql.execute("INSERT INTO chr_variants (variant_id = 1, variant_name = 'BRCA1', sequence = 'ACGTACGT')").unwrap();
        let raw = e.get(b"chr_variants:1").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        let seq = fields.iter().find(|(k, _)| k == "sequence").unwrap();
        assert_eq!(seq.1, Value::Dna("ACGTACGT".into()));
    }

    #[test]
    fn complement_index_maintained() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 5, user_id = 12, total_usd = 9.0, status = 'S')",
        )
        .unwrap();
        // O(1) navigation via index
        let key = sql
            .complement_index
            .write()
            .lookup("chr_orders", "user_id", &Value::Int(12))
            .map(|s| s.to_string());
        assert_eq!(key, Some("chr_orders:5".to_string()));
    }

    // ---- v3.1: secondary indexes, ORDER BY, IN, aggregates ----

    #[test]
    fn create_index_equijoin_lookup_uses_index_scan() {
        let (_e, sql) = setup();
        for i in 0..100 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.5)",
                i, i, i
            ))
            .unwrap();
        }
        sql.execute("CREATE INDEX idx_users_name ON chr_users (username)")
            .unwrap();

        // Equality on an indexed column -> IndexScan, not SeqScan.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'u42'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("user_id"), Some(&Value::Int(42)));
        assert!(
            res.execution_plan.contains("IndexScan"),
            "plan: {}",
            res.execution_plan
        );
        assert!(
            !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // The index catalog persists inside the schema row.
        let schema = sql.get_schema("chr_users").unwrap();
        assert!(schema.secondary_index_on("username").is_some());
    }

    #[test]
    fn index_range_scan_bounded() {
        let (_e, sql) = setup();
        for i in 0..50 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                i, i, i
            ))
            .unwrap();
        }
        sql.execute("CREATE INDEX idx_bal ON chr_users (balance)")
            .unwrap();

        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE balance >= 10 AND balance < 20")
            .unwrap();
        assert_eq!(res.rows.len(), 10, "rows: {:?}", res.rows);
        for r in &res.rows {
            let uid = match r.get("user_id") {
                Some(Value::Int(i)) => *i,
                _ => -1,
            };
            assert!((10..20).contains(&uid), "uid {uid} out of range");
        }
        assert!(
            res.execution_plan.contains("IndexScan") && res.execution_plan.contains(">="),
            "plan: {}",
            res.execution_plan
        );

        // Reverse-direction bounds and exclusive edges.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE 40 >= balance AND balance > 30")
            .unwrap();
        assert_eq!(res.rows.len(), 10);

        // Open-ended range.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE balance > 45")
            .unwrap();
        assert_eq!(res.rows.len(), 4);
    }

    #[test]
    fn index_maintained_on_update_and_delete() {
        let (engine, sql) = setup();
        sql.execute("CREATE INDEX idx_name ON chr_users (username)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alpha', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'beta', balance = 2.0)")
            .unwrap();

        // UPDATE moves the index entry with the row.
        sql.execute("UPDATE chr_users SET username = 'gamma' WHERE user_id = 1")
            .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'alpha'")
            .unwrap();
        assert!(res.rows.is_empty(), "stale alpha entry: {:?}", res.rows);
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'gamma'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);

        // DELETE removes entries.
        sql.execute("DELETE FROM chr_users WHERE user_id = 2")
            .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'beta'")
            .unwrap();
        assert!(res.rows.is_empty());
        // The physical index key for 'gamma' must exist (payload = row key).
        let entries = engine.scan_prefix(b"__idx:chr_users:username:").unwrap();
        assert_eq!(entries.len(), 1, "entries: {:?}", entries.len());
        assert_eq!(
            String::from_utf8(entries[0].1.clone()).unwrap(),
            "chr_users:1"
        );

        // DROP INDEX removes the catalog entry and all entries.
        sql.execute("DROP INDEX idx_name").unwrap();
        assert!(sql
            .get_schema("chr_users")
            .unwrap()
            .secondary_indexes
            .is_empty());
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'gamma'")
            .unwrap();
        assert_eq!(res.rows.len(), 1); // falls back to SeqScan, same results
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn index_survives_restart_and_cascade_delete_cleans_entries() {
        let dir = tempdir("idx_persist");
        {
            let engine = Engine::open(EngineConfig {
                dir_path: dir.clone(),
                memtable_capacity: 1024 * 1024,
                sync_mode: SyncMode::Strict,
                mirror_disk_path: None,
                ..Default::default()
            })
            .unwrap();
            let sql = SqlEngine::new(engine.clone());
            sql.execute("CREATE TABLE chr_t (id INT, tag TEXT, PRIMARY KEY (id))")
                .unwrap();
            sql.execute("CREATE INDEX idx_tag ON chr_t (tag)").unwrap();
            sql.execute("INSERT INTO chr_t (id = 1, tag = 'x')")
                .unwrap();
        }
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine);
        // Schema (and index catalog) reloaded from disk.
        assert!(sql
            .get_schema("chr_t")
            .unwrap()
            .secondary_index_on("tag")
            .is_some());
        let res = sql.execute("SELECT id FROM chr_t WHERE tag = 'x'").unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("IndexScan"),
            "plan: {}",
            res.execution_plan
        );
    }

    #[test]
    fn order_by_asc_desc_and_limit() {
        let (_e, sql) = setup();
        for (id, bal) in [(3, 30.0), (1, 10.0), (4, 40.0), (2, 20.0)] {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                id, id, bal
            ))
            .unwrap();
        }
        let res = sql
            .execute("SELECT username FROM chr_users ORDER BY balance DESC")
            .unwrap();
        let names: Vec<_> = res
            .rows
            .iter()
            .map(|r| r.get("username").cloned().unwrap_or(Value::Null))
            .collect();
        assert_eq!(
            names,
            vec![
                Value::Text("u4".into()),
                Value::Text("u3".into()),
                Value::Text("u2".into()),
                Value::Text("u1".into())
            ]
        );
        assert!(res.execution_plan.contains("Sort"));

        let res = sql
            .execute("SELECT username FROM chr_users ORDER BY balance ASC LIMIT 2")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].get("username"), Some(&Value::Text("u1".into())));
        assert_eq!(res.rows[1].get("username"), Some(&Value::Text("u2".into())));
    }

    #[test]
    fn in_list_and_in_subquery() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_tags (tag_id INT, label TEXT, PRIMARY KEY (tag_id))")
            .unwrap();
        for i in 0..10 {
            sql.execute(&format!(
                "INSERT INTO chr_tags (tag_id = {}, label = 'L{}')",
                i, i
            ))
            .unwrap();
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                i, i, i
            ))
            .unwrap();
        }

        // IN with a literal list.
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id IN (2, 4, 6)")
            .unwrap();
        assert_eq!(res.rows.len(), 3);

        // IN with a subquery: users whose id appears in chr_tags... users 0..10,
        // tags 0..10 -> all; restrict the subquery to prove it runs.
        let res = sql
            .execute(
                "SELECT username FROM chr_users WHERE user_id IN (SELECT tag_id FROM chr_tags WHERE tag_id >= 7)",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 3, "rows: {:?}", res.rows);

        // The plan shows the materialised IN-list (subquery executed once).
        let res = sql
            .execute(
                "SELECT username FROM chr_users WHERE user_id IN (SELECT tag_id FROM chr_tags WHERE tag_id = 3)",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("IN (3)"),
            "plan: {}",
            res.execution_plan
        );

        // NULL semantics: NULL column never matches an IN list.
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id IN (1, NULL)")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn aggregates_count_sum_avg_min_max() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'u1', balance = 1.0)")
            .unwrap();
        for (id, bal, status) in [
            (1, 10.5, "OK"),
            (2, 20.5, "OK"),
            (3, 30.5, "FAIL"),
            (4, 40.5, "FAIL"),
            (5, 50.5, "FAIL"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = 1, total_usd = {}, status = '{}')",
                id, bal, status
            ))
            .unwrap();
        }

        // Global aggregates (no GROUP BY): one row even over the full set.
        let res = sql
            .execute(
                "SELECT COUNT(*) AS c, SUM(total_usd) AS s, AVG(total_usd) AS a, MIN(total_usd) AS lo, MAX(total_usd) AS hi FROM chr_orders",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1, "rows: {:?}", res.rows);
        let r = &res.rows[0];
        assert_eq!(r.get("c"), Some(&Value::Int(5)));
        assert_eq!(r.get("s"), Some(&Value::Float(152.5)));
        assert_eq!(r.get("a"), Some(&Value::Float(30.5)));
        assert_eq!(r.get("lo"), Some(&Value::Float(10.5)));
        assert_eq!(r.get("hi"), Some(&Value::Float(50.5)));
        assert!(
            res.execution_plan.contains("Aggregate"),
            "plan: {}",
            res.execution_plan
        );

        // COUNT over an empty filtered set is 0, not an error.
        let res = sql
            .execute("SELECT COUNT(*) FROM chr_orders WHERE order_id = 999")
            .unwrap();
        assert_eq!(res.rows[0].get("COUNT(*)"), Some(&Value::Int(0)));
    }

    #[test]
    fn group_by_with_having_and_order_of_aggregates() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'u1', balance = 1.0)")
            .unwrap();
        for (id, bal, status) in [
            (1, 10.5, "OK"),
            (2, 20.5, "OK"),
            (3, 30.5, "FAIL"),
            (4, 40.5, "FAIL"),
            (5, 50.5, "FAIL"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = 1, total_usd = {}, status = '{}')",
                id, bal, status
            ))
            .unwrap();
        }

        let res = sql
            .execute(
                "SELECT status, COUNT(*) AS n, SUM(total_usd) AS s FROM chr_orders GROUP BY status ORDER BY s DESC",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 2, "rows: {:?}", res.rows);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        assert_eq!(res.rows[0].get("n"), Some(&Value::Int(3)));
        assert_eq!(res.rows[0].get("s"), Some(&Value::Float(121.5)));
        assert_eq!(res.rows[1].get("status"), Some(&Value::Text("OK".into())));
        assert_eq!(res.rows[1].get("n"), Some(&Value::Int(2)));

        // HAVING filters groups by aggregate value.
        let res = sql
            .execute("SELECT status, COUNT(*) FROM chr_orders GROUP BY status HAVING COUNT(*) > 2")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        assert_eq!(res.rows[0].get("COUNT(*)"), Some(&Value::Int(3)));

        // Aggregate over a WHERE-filtered subset.
        let res = sql
            .execute(
                "SELECT status, SUM(total_usd) FROM chr_orders WHERE total_usd >= 20 GROUP BY status",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        // HAVING on SUM
        let res = sql
            .execute("SELECT status FROM chr_orders GROUP BY status HAVING SUM(total_usd) > 100")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        // SUM of whole-int columns stays Int (typed arithmetic, no float drift).
        let res = sql
            .execute("SELECT SUM(order_id) AS total_ids FROM chr_orders")
            .unwrap();
        assert_eq!(res.rows[0].get("total_ids"), Some(&Value::Int(15)));
    }

    #[test]
    fn null_values_stay_out_of_the_index() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_n (id INT, tag TEXT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("CREATE INDEX idx_n_tag ON chr_n (tag)")
            .unwrap();
        sql.execute("INSERT INTO chr_n (id = 1, tag = NULL)")
            .unwrap();
        sql.execute("INSERT INTO chr_n (id = 2, tag = 't')")
            .unwrap();

        let res = sql.execute("SELECT id FROM chr_n WHERE tag = 't'").unwrap();
        assert_eq!(res.rows.len(), 1);
        // NULL tag row: IS NULL still found (index only accelerates, never changes results).
        let res = sql
            .execute("SELECT id FROM chr_n WHERE tag IS NULL")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }
}
#[cfg(test)]
mod acl_parse_tests {
    use super::referenced_tables;

    #[test]
    fn collects_select_join_and_subquery_tables() {
        let tables = referenced_tables(
            "SELECT u.username FROM chr_users u JOIN chr_orders o ON u.user_id = o.user_id \
             WHERE u.user_id IN (SELECT user_id FROM chr_bans)",
        )
        .unwrap();
        assert_eq!(
            tables,
            vec!["chr_bans".to_string(), "chr_orders".to_string(), "chr_users".to_string()]
        );
    }

    #[test]
    fn collects_create_table_foreign_keys() {
        let tables = referenced_tables(
            "CREATE TABLE chr_orders (id INT, user_id INT, PRIMARY KEY (id), \
             FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        assert!(tables.contains(&"chr_orders".to_string()));
        assert!(tables.contains(&"chr_users".to_string()));
    }
}
