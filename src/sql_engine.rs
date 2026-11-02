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
use crate::expr::{ArithOp, CastTy, CmpOp, Expr};
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
    /// v3.6: statement-scoped CTE materialisation store. Populated while a
    /// `WITH ... SELECT` runs, always emptied before the statement returns.
    cte_store: std::sync::Mutex<std::collections::HashMap<String, Vec<BTreeMap<String, Value>>>>,
    /// CMS edition (branch cms-edition): per-table AUTO_INCREMENT sequences.
    /// Shared Arc so the compat gate (`cms::compat::CmsSqlGate`) and the
    /// engine observe one counter per table. Seeded lazily from MAX(pk).
    #[cfg(feature = "cms")]
    pub cms_sequences: std::sync::Arc<crate::cms::sequence::AutoIncrementRegistry>,
}

/// v3.6 session transaction buffer: DML inside `BEGIN ... COMMIT` is
/// collected here and applied as ONE atomic `write_batch` on COMMIT
/// (a single fsync durability interval, mirroring tx.rs semantics).
#[derive(Default)]
pub struct SqlTxBuffer {
    pub ops: Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)>,
    pub active: bool,
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
                    // CMS edition: MySQL string-literal escapes. `\%` and
                    // `\_` stay two characters so LIKE patterns keep their
                    // escaped wildcards ('\_transient\_%'); the rest map per
                    // the MySQL manual, unknown escapes drop the backslash.
                    #[cfg(feature = "cms")]
                    {
                        if b[i] == '\\' {
                            if i + 1 >= b.len() {
                                return Err("SQL: dangling backslash in string literal".into());
                            }
                            let e = b[i + 1];
                            match e {
                                '0' => s.push('\0'),
                                'n' => s.push('\n'),
                                't' => s.push('\t'),
                                'r' => s.push('\r'),
                                'b' => s.push('\u{0008}'),
                                'Z' => s.push('\u{001a}'),
                                '\'' | '"' | '\\' => s.push(e),
                                '%' | '_' => {
                                    s.push('\\');
                                    s.push(e);
                                }
                                other => s.push(other),
                            }
                            i += 2;
                            continue;
                        }
                    }
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
pub enum Stmt {
    Select {
        projection: Vec<SelectItem>, // empty = *
        from: String,
        /// Alias of the FROM table (AS t) — for `t.*` expansion.
        from_alias: Option<String>,
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
        /// v3.6: `ON CONFLICT (col) DO NOTHING | DO UPDATE SET ...` (UPSERT).
        on_conflict: Option<OnConflict>,
    },
    Update {
        table: String,
        /// v4.2-cms: SET rhs is a full expression evaluated against the OLD
        /// row (MySQL semantics) — `SET stock = stock - 1` (WooCommerce).
        assignments: Vec<(String, Expr)>,
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
    /// v3.6: `SELECT ... UNION [ALL] SELECT ...` — compound set operation.
    /// ORDER BY / LIMIT / OFFSET captured from the right-hand select apply
    /// to the compound result (standard SQL semantics).
    Union {
        left: Box<Stmt>,
        right: Box<Stmt>,
        all: bool,
        order_by: Vec<(String, bool)>,
        limit: Option<usize>,
        offset: usize,
    },
    /// v3.6: `SHOW TABLES` (alias `SHOW CHROMOSOMES`) — table introspection.
    ShowTables,
    /// v4.1: `SHOW STORAGE FOR <table> [WHERE <pk> = <literal>]` — the
    /// SQL→DnaQL→DNA storage map. Without a predicate: one row per stored
    /// locus (chromosome, locus, storage key, payload size, DNA columns).
    /// With `WHERE <pk-col> = <literal>`: per-column detail for that locus —
    /// every value decoded back to the human-readable form next to its
    /// physical RowCodec representation (nucleotide chains included).
    ShowStorage {
        table: String,
        pk: Option<(String, Value)>,
    },
    /// v3.6: `BEGIN | COMMIT | ROLLBACK` — session transaction control.
    /// DML inside an active transaction is buffered and applied as ONE
    /// atomic WAL batch on COMMIT (single fsync durability interval).
    Tx { action: TxAction },
    /// v3.6: `WITH name AS (SELECT ...) [, name2 AS (...)] SELECT ...` —
    /// common table expressions. Each CTE is materialised once into a
    /// statement-scoped temp store and referenced like a table.
    Cte {
        ctes: Vec<(String, Stmt)>,
        body: Box<Stmt>,
    },
    /// v3.9 Online DDL: instant metadata-only `ALTER TABLE t ADD COLUMN c T`
    /// (no row rewrite; existing rows read the new column as NULL, or the
    /// DEFAULT literal captured for MySQL-dialect tables).
    AddColumn {
        table: String,
        column: String,
        col_type: ColumnType,
        col_default: Option<String>,
    },
    /// CMS edition: `ALTER TABLE t DROP COLUMN c` (dbDelta revert path).
    DropColumn {
        table: String,
        column: String,
    },
}

/// v3.6 UPSERT tail of an INSERT statement.
#[derive(Debug, Clone)]
pub(crate) enum OnConflict {
    /// `ON CONFLICT (pk) DO NOTHING` — skip conflicting rows.
    DoNothing,
    /// `ON CONFLICT (pk) DO UPDATE SET col = <literal> | EXCLUDED.col`.
    DoUpdate {
        assignments: Vec<(String, UpsertValue)>,
    },
}

/// Right side of a `DO UPDATE SET` assignment: a literal or the conflicting
/// row's new value (`EXCLUDED.col`).
#[derive(Debug, Clone)]
pub(crate) enum UpsertValue {
    Literal(Value),
    Excluded(String),
}

/// v3.6 session transaction control statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxAction {
    Begin,
    Commit,
    Rollback,
}

/// One item of a SELECT projection: a plain column or an aggregate call,
/// each optionally renamed with `AS alias`.
#[derive(Debug, Clone)]
pub(crate) enum SelectItem {
    Star,
    /// CMS edition: `t.*` — expanded at execution (see exec_select).
    AliasStar(String),
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
    /// CMS edition: an arithmetic ORDER BY key (`ORDER BY meta_value+0`,
    /// the WordPress numeric-sort idiom). The expression is materialised
    /// per row before sorting but emits NO output column — MySQL returns
    /// only the listed projection for such queries.
    OrderKeyOnly {
        expr: Expr,
        /// Canonical rendering used as the internal row key.
        rendered: String,
    },
    /// v3.6: scalar subquery in the projection, e.g.
    /// `(SELECT MAX(total) FROM chr_orders WHERE user_id = id) AS max_order`.
    /// Correlated references are resolved against the current outer row.
    ScalarSub {
        stmt: Box<Stmt>,
        /// Rendered SQL (plan display + re-execution for uncorrelated subs).
        canonical: String,
        /// Output column name (alias, else the canonical rendering).
        rendered: String,
    },
    /// v3.6: window function `ROW_NUMBER() / RANK() / DENSE_RANK() OVER
    /// (PARTITION BY ... ORDER BY ...)` computed over the filtered row set.
    Window {
        func: WindowFunc,
        partition: Vec<String>,
        order: Vec<(String, bool)>,
        /// Canonical rendering used as the internal row key.
        canonical: String,
        /// Output column name: alias if given, else the canonical rendering.
        rendered: String,
    },
}

/// v3.6 ranking window functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
}

impl WindowFunc {
    fn name(&self) -> &'static str {
        match self {
            WindowFunc::RowNumber => "ROW_NUMBER",
            WindowFunc::Rank => "RANK",
            WindowFunc::DenseRank => "DENSE_RANK",
        }
    }
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "ROW_NUMBER" => Some(WindowFunc::RowNumber),
            "RANK" => Some(WindowFunc::Rank),
            "DENSE_RANK" => Some(WindowFunc::DenseRank),
            _ => None,
        }
    }
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
    /// Table alias (AS t) — kept for `t.*` projection expansion.
    pub(crate) alias: Option<String>,
    pub(crate) left_col: String, // qualified or bare (resolved later)
    pub(crate) right_col: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinType {
    Inner,
    Left,
    /// v3.6: `RIGHT [OUTER] JOIN` — executed as a left join with the sides
    /// swapped (identical row semantics, single-namespace row model).
    Right,
    /// v3.6: `FULL [OUTER] JOIN` — matched rows plus unmatched rows from
    /// BOTH sides (right rows padded with NULLs).
    Full,
}

impl PartialEq for Stmt {
    /// Structural equality via canonical debug rendering. Only the compiler
    /// (through Expr's derived PartialEq) needs this; subquery-heavy
    /// statements are rarely compared and Debug is a faithful total map.
    fn eq(&self, other: &Self) -> bool {
        format!("{:?}", self) == format!("{:?}", other)
    }
}

struct SParser {
    toks: Vec<STok>,
    pos: usize,
    /// Recursion depth of the expression/select parser. The grammar recurses
    /// once per nested parenthesis (and once per nested subquery), so a
    /// hostile frame full of '(' would overflow the stack and, with the old
    /// panic=abort profile, kill the whole server. Both recursion cycles go
    /// through `parse_or` (expressions) or `parse_select` (subqueries), so
    /// guarding those two bounds the entire grammar.
    depth: usize,
}

/// Maximum nesting accepted by the SQL parser (parenthesised expressions,
/// nested subqueries, or any mix of both). 128 was the original value; the
/// CMS edition added parser branches whose debug-build stack frames made a
/// 128-deep recursion overflow the 2 MiB test-thread stack BEFORE the guard
/// fired, so the limit now sits at a safer 100 (real CMS queries nest < 10).
const MAX_PARSE_DEPTH: usize = 100;

impl SParser {
    /// Enter one recursion level; errors out instead of smashing the stack.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(format!(
                "SQL: expression nested too deeply (limit {})",
                MAX_PARSE_DEPTH
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

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
    /// Consume an optional table alias ('FROM chr_orders o' / 'AS t').
    /// The row model stays single-namespace (qualified refs alias.col are
    /// stripped at parse time); the alias is still RETURNED so `t.*`
    /// projections can resolve it to the underlying table.
    fn maybe_alias(&mut self) -> Option<String> {
        // WordPress always writes explicit aliases: `FROM wp_terms AS t`.
        if self.kw("AS") {
            if let STok::Ident(name) = self.peek().clone() {
                self.next();
                return Some(name);
            }
        }
        if let STok::Ident(s) = self.peek().clone() {
            let lower = s.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "join"
                    | "inner"
                    | "left"
                    | "right"
                    | "full"
                    | "outer"
                    | "cross"
                    | "where"
                    | "limit"
                    | "offset"
                    | "on"
                    | "group"
                    | "order"
                    | "union"
                    | "all"
                    | "having"
            ) {
                self.next();
                return Some(s.clone());
            }
        }
        None
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
        } else if self.kw("WITH") {
            self.parse_cte()
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
        } else if self.kw("BEGIN") || self.kw("START") {
            // START TRANSACTION is the fully standard spelling of BEGIN.
            if self.kw("TRANSACTION") {
                self.eat_semi();
            }
            Ok(Stmt::Tx { action: TxAction::Begin })
        } else if self.kw("COMMIT") || self.kw("END") {
            self.eat_semi();
            Ok(Stmt::Tx { action: TxAction::Commit })
        } else if self.kw("ROLLBACK") {
            self.eat_semi();
            Ok(Stmt::Tx { action: TxAction::Rollback })
        } else if self.kw("SHOW") {
            if self.kw("TABLES") || self.kw("CHROMOSOMES") {
                self.eat_semi();
                Ok(Stmt::ShowTables)
            } else if self.kw("STORAGE") {
                // v4.1: SHOW STORAGE FOR <table> [WHERE <pk> = <literal>]
                self.expect_kw("FOR")?;
                let table = self.ident()?;
                let mut pk = None;
                if self.kw("WHERE") {
                    let col = self.ident()?;
                    self.expect(STok::Eq)?;
                    let lit = self.value()?;
                    pk = Some((col, lit));
                }
                self.eat_semi();
                Ok(Stmt::ShowStorage { table, pk })
            } else {
                Err("SQL: unsupported SHOW target (supported: SHOW TABLES / SHOW CHROMOSOMES / SHOW STORAGE FOR <table>)".into())
            }
        } else if self.kw("ALTER") {
            self.expect_kw("TABLE")?;
            let table = self.ident()?;
            if self.kw("DROP") {
                // MySQL revert path: ALTER TABLE t DROP COLUMN c.
                self.kw("COLUMN");
                let column = self.ident()?;
                self.eat_semi();
                return Ok(Stmt::DropColumn { table, column });
            }
            self.expect_kw("ADD")?;
            self.kw("COLUMN");
            let column = self.ident()?;
            let col_type = self.parse_column_type()?;
            // CMS edition: trailing column flags — the DEFAULT literal
            // (or the NOT NULL zero-value) becomes the new column's
            // effective default, exactly like CREATE TABLE.
            #[cfg(feature = "cms")]
            {
                let mut col_default: Option<String> = None;
                let mut not_null = false;
                loop {
                    if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                        continue;
                    }
                    if self.kw("NOT") {
                        if !self.kw("NULL") {
                            return Err("SQL: expected NULL after NOT in column definition".into());
                        }
                        not_null = true;
                        continue;
                    }
                    if self.kw("NULL") {
                        continue;
                    }
                    if self.kw("DEFAULT") {
                        col_default = Some(match self.peek().clone() {
                            STok::Str(s) => {
                                self.next();
                                s
                            }
                            STok::Int(v) => {
                                self.next();
                                v.to_string()
                            }
                            STok::Float(v) => {
                                self.next();
                                v.to_string()
                            }
                            STok::Ident(s) => {
                                self.next();
                                s
                            }
                            t => return Err(format!("SQL: bad DEFAULT value {:?}", t)),
                        });
                        continue;
                    }
                    if self.kw("COMMENT") {
                        self.consume_default_value()?;
                        continue;
                    }
                    break;
                }
                if col_default.is_none() && not_null {
                    col_default = Some(match &col_type {
                        ColumnType::Timestamp => "0000-00-00 00:00:00".to_string(),
                        ColumnType::Text | ColumnType::Dna2Bit => String::new(),
                        ColumnType::Int64 | ColumnType::Float64
                        | ColumnType::ForeignLink { .. } => "0".to_string(),
                    });
                }
                self.eat_semi();
                return Ok(Stmt::AddColumn { table, column, col_type, col_default });
            }
            #[cfg(not(feature = "cms"))]
            {
                let _ = self.kw("NOT") && self.kw("NULL");
                self.eat_semi();
                return Ok(Stmt::AddColumn { table, column, col_type, col_default: None });
            }
        } else if self.kw("EXPLAIN") {
            // kw() already consumed the EXPLAIN token.
            let inner = self.parse_stmt()?;
            Ok(Stmt::Explain(Box::new(inner)))
        } else {
            Err(format!(
                "SQL: unsupported statement starting at {:?}",
                self.peek()
            ))
        }
    }

    /// Consume a statement-terminating `;` if present.
    fn eat_semi(&mut self) {
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
    }

    /// `WITH name AS (SELECT ...) [, name2 AS (...)] body` (v3.6).
    fn parse_cte(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        let mut ctes = Vec::new();
        loop {
            let name = self.ident()?;
            self.expect_kw("AS")?;
            self.expect(STok::LParen)?;
            if !self.kw("SELECT") {
                return Err("SQL: CTE body must be a SELECT (WITH name AS (SELECT ...))".into());
            }
            let def = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            ctes.push((name, def));
            if matches!(self.peek(), STok::Comma) {
                self.next();
                continue;
            }
            break;
        }
        let body = self.parse_stmt()?;
        self.leave();
        Ok(Stmt::Cte {
            ctes,
            body: Box::new(body),
        })
    }

    /// Body of a SELECT after the SELECT keyword has been consumed.
    /// v3.6: recognises a trailing `UNION [ALL] SELECT ...` and builds a
    /// compound statement (chained left-associatively).
    fn parse_select_tail(&mut self) -> Result<Stmt, String> {
        let stmt = self.parse_select()?;
        if !self.kw("UNION") {
            return Ok(stmt);
        }
        let all = self.kw("ALL");
        // Peek WITHOUT consuming: parse_stmt handles SELECT/WITH itself
        // (consuming the keyword here swallowed the projection token).
        let next_is_select = matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT") || s.eq_ignore_ascii_case("WITH"));
        if !next_is_select {
            return Err("SQL: expected SELECT after UNION".into());
        }
        let right = self.parse_stmt()?;
        // ORDER BY / LIMIT / OFFSET parsed into the right-hand select belong
        // to the compound result (standard SQL set-operation semantics).
        let (order_by, limit, offset) = match &right {
            Stmt::Select {
                order_by,
                limit,
                offset,
                ..
            } => (order_by.clone(), *limit, *offset),
            _ => (Vec::new(), None, 0),
        };
        Ok(Stmt::Union {
            left: Box::new(stmt),
            right: Box::new(right),
            all,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_select(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        let result = self.parse_select_inner();
        self.leave();
        result
    }

    fn parse_select_inner(&mut self) -> Result<Stmt, String> {
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
        // v4.0: FROM-less SELECT (`SELECT (SELECT MAX(x) FROM t) AS mx`) —
        // the canonical way to evaluate scalar expressions / subqueries.
        // Everything else still requires FROM.
        let from = if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("FROM")) {
            self.expect_kw("FROM")?;
            self.ident()?
        } else {
            String::new()
        };
        let from_alias = self.maybe_alias();
        let mut joins = Vec::new();
        loop {
            let join_type = if self.kw("INNER") {
                self.expect_kw("JOIN")?;
                JoinType::Inner
            } else if self.kw("LEFT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Left
            } else if self.kw("RIGHT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Right
            } else if self.kw("FULL") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Full
            } else if self.kw("JOIN") {
                JoinType::Inner
            } else {
                break;
            };
            let table = self.ident()?;
            let jalias = self.maybe_alias();
            self.expect_kw("ON")?;
            // WordPress always sends parenthesized join conditions:
            // ON (a.id = b.id) — accept the optional parens.
            let paren = matches!(self.peek(), STok::LParen);
            if paren {
                self.next();
            }
            // CMS edition (branch `cms-edition`): ON 1=1 marks a pure cross
            // join. The compat gate rewrites MySQL comma-FROM lists
            // (`FROM a, b WHERE …`) into `a INNER JOIN b ON 1=1`, keeping the
            // filter predicates in WHERE exactly like MySQL does.
            if matches!(self.peek(), STok::Int(1)) {
                let save = self.pos;
                self.next();
                if matches!(self.next(), STok::Eq) && matches!(self.next(), STok::Int(1)) {
                    if paren {
                        self.expect(STok::RParen)?;
                    }
                    joins.push(JoinSpec {
                        join_type,
                        table,
                        alias: jalias,
                        left_col: "1".to_string(),
                        right_col: "1".to_string(),
                    });
                    continue;
                }
                self.pos = save;
            }
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
            if paren {
                self.expect(STok::RParen)?;
            }
            joins.push(JoinSpec {
                join_type,
                table,
                alias: jalias,
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
                let mut col = self.ident()?;
                // qualified reference a.col — the qualifier is stripped.
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    col = self.ident()?;
                }
                cols.push(col);
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
                let start = self.pos;
                let mut key = self.ident()?;
                // qualified reference `t.col` — the qualifier is stripped
                // (single-namespace row model), as in projections.
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    key = self.ident()?;
                }
                if matches!(self.peek(), STok::LParen) {
                    self.next();
                    let mut distinct = false;
                    if matches!(self.peek(), STok::Ident(d) if d.eq_ignore_ascii_case("DISTINCT")) {
                        self.next();
                        distinct = true;
                    }
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        Some(self.ident()?)
                    };
                    self.expect(STok::RParen)?;
                    key = render_agg_distinct(&key, arg.as_deref(), distinct);
                }
                // CMS edition: an arithmetic order key — `ORDER BY
                // meta_value+0 DESC` is the canonical WordPress/WooCommerce
                // numeric-sort idiom for varchar meta. Rewind and parse the
                // whole key as an expression; exec_select materialises it
                // per row before sorting and it emits no output column.
                // (Plain builds keep the original strict grammar: the key
                // ends here and the operator surfaces as trailing tokens.)
                else if cfg!(feature = "cms")
                    && matches!(
                        self.peek(),
                        STok::Plus | STok::Minus | STok::Star | STok::Slash
                    )
                {
                    self.pos = start;
                    let expr = self.parse_expr()?;
                    let rendered = expr.describe();
                    projection.push(SelectItem::OrderKeyOnly {
                        expr,
                        rendered: rendered.clone(),
                    });
                    key = rendered;
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
        let mut limit: Option<usize> = None;
        let mut offset: usize = 0;
        if self.kw("LIMIT") {
            match self.next() {
                STok::Int(i) => {
                    // MySQL pagination form `LIMIT offset, count` — WordPress
                    // (WP_Query) sends it everywhere; == LIMIT count OFFSET
                    // offset. Without the cms feature the comma stays a
                    // parse error (original strict grammar).
                    #[cfg(feature = "cms")]
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                        offset = i as usize;
                        match self.next() {
                            STok::Int(c) => limit = Some(c as usize),
                            t => return Err(format!("SQL: LIMIT expects int, got {:?}", t)),
                        }
                    } else {
                        limit = Some(i as usize);
                    }
                    #[cfg(not(feature = "cms"))]
                    {
                        limit = Some(i as usize);
                    }
                }
                t => return Err(format!("SQL: LIMIT expects int, got {:?}", t)),
            }
        }
        if self.kw("OFFSET") {
            match self.next() {
                STok::Int(i) => offset = i as usize,
                t => return Err(format!("SQL: OFFSET expects int, got {:?}", t)),
            }
        }
        // MySQL row-locking tail: `FOR UPDATE` / `FOR SHARE` [OF …] [NOWAIT
        // | SKIP LOCKED]. The engine has no row-level locks yet (lock wait /
        // deadlock detection is CMS_EDITION_RU.md §4 item 2); inside the
        // gate's transaction buffer the statement is still correct, just
        // not serialized — accept and ignore (WooCommerce stock paths).
        #[cfg(feature = "cms")]
        if self.kw("FOR") {
            if !(self.kw("UPDATE") || self.kw("SHARE")) {
                return Err("SQL: expected UPDATE or SHARE after FOR".into());
            }
            if self.kw("OF") {
                loop {
                    let _ = self.ident()?;
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
            }
            if self.kw("NOWAIT") {
            } else if self.kw("SKIP") {
                self.kw("LOCKED");
            }
        }
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Select {
            projection,
            from,
            from_alias,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    /// One projection item: `*`, a plain column, an aggregate call, a window
    /// function, a scalar subquery or an arithmetic expression
    /// (`total_usd * 2 AS doub`).
    fn parse_select_item(&mut self) -> Result<SelectItem, String> {
        if matches!(self.peek(), STok::Star) {
            self.next();
            return Ok(SelectItem::Star);
        }
        // CMS edition: `t.*` — every column of one FROM/JOIN table
        // (expanded at execution against the alias → table map).
        if let STok::Ident(qual) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::Dot) && matches!(self.peek_at(2), STok::Star) {
                self.next();
                self.next();
                self.next();
                return Ok(SelectItem::AliasStar(qual));
            }
        }
        // v3.6: scalar subquery `(SELECT ...) [AS alias]`.
        if matches!(self.peek(), STok::LParen)
            && matches!(self.peek_at(1), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT"))
        {
            self.next(); // (
            if !self.kw("SELECT") {
                return Err("SQL: expected SELECT in scalar subquery".into());
            }
            let sub = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            let alias = self.maybe_as_alias()?;
            let canonical = render_select_sql(&sub);
            let rendered = alias.unwrap_or_else(|| format!("({})", canonical));
            return Ok(SelectItem::ScalarSub {
                stmt: Box::new(sub),
                canonical,
                rendered,
            });
        }
        // v3.6: window function ROW_NUMBER()/RANK()/DENSE_RANK() OVER (...).
        if let STok::Ident(s) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::LParen)
                && WindowFunc::from_name(&s).is_some()
            {
                let func = WindowFunc::from_name(&s).unwrap();
                self.next();
                self.next(); // (
                self.expect(STok::RParen)?;
                self.expect_kw("OVER")?;
                self.expect(STok::LParen)?;
                let mut partition = Vec::new();
                if self.kw("PARTITION") {
                    self.expect_kw("BY")?;
                    loop {
                        partition.push(self.ident()?);
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                let mut order = Vec::new();
                if self.kw("ORDER") {
                    self.expect_kw("BY")?;
                    loop {
                        let col = self.ident()?;
                        let asc = if self.kw("DESC") {
                            false
                        } else {
                            self.kw("ASC");
                            true
                        };
                        order.push((col, asc));
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(STok::RParen)?;
                let alias = self.maybe_as_alias()?;
                let canonical = render_window(&func, &partition, &order);
                let rendered = alias.unwrap_or_else(|| canonical.clone());
                return Ok(SelectItem::Window {
                    func,
                    partition,
                    order,
                    canonical,
                    rendered,
                });
            }
        }
        // aggregate call: COUNT(*) / SUM(col) — recognised before the general
        // expression path so `COUNT(*)` never parses as multiplication.
        if let STok::Ident(s) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::LParen) && AggFunc::from_name(&s).is_ok() {
                self.next();
                self.next(); // (
                let mut distinct = false;
                if matches!(self.peek(), STok::Ident(d) if d.eq_ignore_ascii_case("DISTINCT")) {
                    self.next();
                    distinct = true;
                }
                let arg = if matches!(self.peek(), STok::Star) {
                    self.next();
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(STok::RParen)?;
                if distinct && arg.is_none() {
                    return Err("SQL: DISTINCT aggregate requires a column argument".into());
                }
                let func = AggFunc::from_name(&s)?;
                let canonical = render_agg_distinct(&s, arg.as_deref(), distinct);
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
                // Qualified reference t.col — the qualifier is stripped
                // (single-namespace row model), as in ORDER BY / GROUP BY.
                let mut source = source;
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    source = self.ident()?;
                }
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
                    | "right"
                    | "full"
                    | "outer"
                    | "on"
                    | "and"
                    | "or"
                    | "asc"
                    | "desc"
                    | "union"
                    | "all"
                    | "over"
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
            let on_conflict = self.parse_on_conflict_tail()?;
            self.eat_semi();
            return Ok(Stmt::Insert {
                table,
                rows,
                on_conflict,
            });
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
        let on_conflict = self.parse_on_conflict_tail()?;
        self.eat_semi();
        Ok(Stmt::Insert {
            table,
            rows,
            on_conflict,
        })
    }

    /// v3.6 UPSERT tail: `ON CONFLICT (col) DO NOTHING | DO UPDATE SET ...`.
    fn parse_on_conflict_tail(&mut self) -> Result<Option<OnConflict>, String> {
        if !self.kw("ON") {
            return Ok(None);
        }
        // CMS edition: MySQL `ON DUPLICATE KEY UPDATE col = VALUES(col), …`
        // — the same DO UPDATE machinery as ON CONFLICT, but firing on ANY
        // unique-key conflict (PK or secondary UNIQUE), which exec_insert_tx
        // resolves through the unique-index entry.
        #[cfg(feature = "cms")]
        if self.kw("DUPLICATE") {
            self.expect_kw("KEY")?;
            self.expect_kw("UPDATE")?;
            let mut assignments = Vec::new();
            loop {
                let col = self.ident()?;
                match self.next() {
                    STok::Eq => {}
                    t => {
                        return Err(format!(
                            "SQL: expected '=' in ON DUPLICATE KEY UPDATE, got {:?}",
                            t
                        ))
                    }
                }
                // VALUES(col) — the conflicting row's proposed value.
                if let STok::Ident(s) = self.peek().clone() {
                    if s.eq_ignore_ascii_case("VALUES")
                        && matches!(self.peek_at(1), STok::LParen)
                    {
                        self.next();
                        self.next();
                        let source = self.ident()?;
                        self.expect(STok::RParen)?;
                        assignments.push((col, UpsertValue::Excluded(source)));
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                            continue;
                        }
                        break;
                    }
                }
                assignments.push((col, UpsertValue::Literal(self.value()?)));
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            return Ok(Some(OnConflict::DoUpdate { assignments }));
        }
        self.expect_kw("CONFLICT")?;
        self.expect(STok::LParen)?;
        let _target = self.ident()?; // conflict target column (PK expected)
        self.expect(STok::RParen)?;
        self.expect_kw("DO")?;
        if self.kw("NOTHING") {
            return Ok(Some(OnConflict::DoNothing));
        }
        if !self.kw("UPDATE") {
            return Err("SQL: expected DO NOTHING or DO UPDATE after ON CONFLICT".into());
        }
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            match self.next() {
                STok::Eq => {}
                t => return Err(format!("SQL: expected '=' in DO UPDATE SET, got {:?}", t)),
            }
            // Value: EXCLUDED.col (the row that caused the conflict) or a literal.
            if let STok::Ident(s) = self.peek().clone() {
                if s.eq_ignore_ascii_case("EXCLUDED") {
                    self.next();
                    self.expect(STok::Dot)?;
                    let source = self.ident()?;
                    assignments.push((col, UpsertValue::Excluded(source)));
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                        continue;
                    }
                    break;
                }
            }
            assignments.push((col, UpsertValue::Literal(self.value()?)));
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        Ok(Some(OnConflict::DoUpdate { assignments }))
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
            // v4.2-cms: rhs may be a column/arithmetic expression — it is
            // evaluated per-row against the pre-update values.
            let v = self.parse_operand()?;
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
            // CMS edition: MySQL constraint lines inside CREATE TABLE —
            // `KEY name (col(len))`, `INDEX name (col)`, `UNIQUE KEY name (col)`,
            // `FULLTEXT KEY name (col)`. Single-column keys become engine
            // secondary indexes; composite keys index their first column
            // non-uniquely (engine single-PK limitation, CMS_EDITION_RU.md §4).
            #[cfg(feature = "cms")]
            if self.peek_is_cms_constraint() {
                self.parse_cms_table_constraint(&mut t)?;
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                    continue;
                } else {
                    break;
                }
            }
            if self.kw("PRIMARY") {
                self.expect_kw("KEY")?;
                self.expect(STok::LParen)?;
                let pk = self.ident()?;
                // CMS edition: composite PRIMARY KEY (a, b) — the engine keys
                // on the first column; extra key parts are consumed.
                #[cfg(feature = "cms")]
                while matches!(self.peek(), STok::Comma) {
                    self.next();
                    let _ = self.ident()?;
                    self.consume_type_len()?;
                }
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
                let ty = self.parse_column_type()?;
                // CMS edition: MySQL-dialect column flags in any order
                // (UNSIGNED NOT NULL DEFAULT '…' AUTO_INCREMENT …).
                // AUTO_INCREMENT records the sequence column (exec_insert_tx
                // assigns values); lengths/defaults are consumed and dropped.
                #[cfg(feature = "cms")]
                {
                    let mut not_null = false;
                    let mut default: Option<String> = None;
                    loop {
                        if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                            continue;
                        }
                        if self.kw("NOT") {
                            if !self.kw("NULL") {
                                return Err("SQL: expected NULL after NOT in column definition".into());
                            }
                            not_null = true;
                            continue;
                        }
                        if self.kw("NULL") {
                            continue;
                        }
                        if self.kw("AUTO_INCREMENT") {
                            t.auto_increment_column = Some(col.clone());
                            continue;
                        }
                        if self.kw("DEFAULT") {
                            default = Some(match self.peek().clone() {
                                STok::Str(s) => {
                                    self.next();
                                    s
                                }
                                STok::Int(v) => {
                                    self.next();
                                    v.to_string()
                                }
                                STok::Float(v) => {
                                    self.next();
                                    v.to_string()
                                }
                                STok::Ident(s) => {
                                    self.next();
                                    s
                                }
                                t => return Err(format!("SQL: bad DEFAULT value {:?}", t)),
                            });
                            continue;
                        }
                        if self.kw("CHARACTER") {
                            self.kw("SET");
                            let _ = self.ident();
                            continue;
                        }
                        if self.kw("COLLATE") {
                            let _ = self.ident();
                            continue;
                        }
                        if self.kw("COMMENT") {
                            self.consume_default_value()?;
                            continue;
                        }
                        break;
                    }
                    // Effective column default (MySQL dialect): the explicit
                    // DEFAULT literal wins; a NOT NULL column without one
                    // gets MySQL's implicit zero-value ('' / 0 / zero-date,
                    // non-strict mode — WordPress hosting default). Nullable
                    // columns stay sparse (NULL) on INSERT. exec_insert_tx
                    // fills omitted columns from this map.
                    let effective = match default {
                        Some(d) => Some(d),
                        None if not_null => Some(match &ty {
                            ColumnType::Timestamp => "0000-00-00 00:00:00".to_string(),
                            ColumnType::Text | ColumnType::Dna2Bit => String::new(),
                            ColumnType::Int64 | ColumnType::Float64
                            | ColumnType::ForeignLink { .. } => "0".to_string(),
                        }),
                        None => None,
                    };
                    if let Some(d) = effective {
                        t.column_defaults.insert(col.clone(), d);
                    }
                }
                #[cfg(not(feature = "cms"))]
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
        // CMS edition: MySQL table options after the column list —
        // `ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4
        // COLLATE=utf8mb4_unicode_520_ci` — consumed and ignored.
        #[cfg(feature = "cms")]
        {
            loop {
                // `=` is STok::Eq — kw() only matches Ident tokens, so Eq is
                // consumed explicitly everywhere below.
                if self.kw("ENGINE") {
                    self.expect(STok::Eq)?;
                    let _ = self.ident()?;
                } else if self.kw("AUTO_INCREMENT") {
                    self.expect(STok::Eq)?;
                    let _ = self.next();
                } else if self.kw("DEFAULT") {
                    if !(self.kw("CHARSET") || self.kw("COLLATE") || self.kw("CHARACTER")) {
                        return Err("SQL: unsupported DEFAULT table option".into());
                    }
                    self.kw("SET");
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else if self.kw("CHARSET") || self.kw("COLLATE") {
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else if self.kw("CHARACTER") {
                    self.kw("SET");
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        t.foreign_keys = fks;
        Ok(Stmt::CreateTable {
            table: t,
            if_not_exists,
        })
    }

    /// Column type of a CREATE TABLE column. Without the cms feature this is
    /// the original base-type grammar; with cms it also accepts the
    /// MySQL-dialect names WordPress DDL uses (bigint/tinyint/datetime/
    /// longtext/decimal/...), consuming optional `(n)` lengths.
    fn parse_column_type(&mut self) -> Result<ColumnType, String> {
        if self.kw("INT") || self.kw("INTEGER") || self.kw("INT64") {
            #[cfg(feature = "cms")]
            self.consume_type_len()?;
            Ok(ColumnType::Int64)
        } else if self.kw("FLOAT") || self.kw("FLOAT64") || self.kw("DOUBLE") {
            Ok(ColumnType::Float64)
        } else if self.kw("TEXT") || self.kw("VARCHAR") || self.kw("STRING") {
            #[cfg(feature = "cms")]
            self.consume_type_len()?; // VARCHAR(191), TEXT(65535)
            Ok(ColumnType::Text)
        } else if self.kw("DNA") || self.kw("DNA2BIT") {
            Ok(ColumnType::Dna2Bit)
        } else if self.kw("TIMESTAMP") {
            Ok(ColumnType::Timestamp)
        } else {
            // CMS edition: MySQL-dialect names map onto base engine types.
            #[cfg(feature = "cms")]
            {
                if self.kw("BIGINT")
                    || self.kw("TINYINT")
                    || self.kw("SMALLINT")
                    || self.kw("MEDIUMINT")
                {
                    self.consume_type_len()?;
                    return Ok(ColumnType::Int64);
                }
                if self.kw("DATETIME") || self.kw("DATE") || self.kw("TIME") {
                    return Ok(ColumnType::Timestamp);
                }
                if self.kw("LONGTEXT")
                    || self.kw("MEDIUMTEXT")
                    || self.kw("TINYTEXT")
                    || self.kw("CHAR")
                    || self.kw("BLOB")
                {
                    self.consume_type_len()?;
                    return Ok(ColumnType::Text);
                }
                if self.kw("DECIMAL") || self.kw("NUMERIC") || self.kw("REAL") {
                    self.consume_type_len()?; // DECIMAL(10,2)
                    return Ok(ColumnType::Float64);
                }
            }
            Err("SQL: unknown column type".into())
        }
    }

    /// Consumes an optional `(n)` / `(n,m)` type length (cms only).
    #[cfg(feature = "cms")]
    fn consume_type_len(&mut self) -> Result<(), String> {
        if matches!(self.peek(), STok::LParen) {
            self.next();
            match self.next() {
                STok::Int(_) | STok::Float(_) => {}
                t => return Err(format!("SQL: expected length after '(', got {:?}", t)),
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
                match self.next() {
                    STok::Int(_) | STok::Float(_) => {}
                    t => return Err(format!("SQL: expected precision after ',', got {:?}", t)),
                }
            }
            self.expect(STok::RParen)?;
        }
        Ok(())
    }

    /// Consumes a DEFAULT/COMMENT value literal: string, number, or bare
    /// identifier (NULL / CURRENT_TIMESTAMP) (cms only).
    #[cfg(feature = "cms")]
    fn consume_default_value(&mut self) -> Result<(), String> {
        match self.peek() {
            STok::Str(_) | STok::Int(_) | STok::Float(_) | STok::Ident(_) => {
                let _ = self.next();
                Ok(())
            }
            t => Err(format!("SQL: bad DEFAULT value {:?}", t)),
        }
    }

    /// True when the next token starts a MySQL constraint line inside
    /// CREATE TABLE (cms only): KEY / INDEX / UNIQUE / FULLTEXT / SPATIAL.
    #[cfg(feature = "cms")]
    fn peek_is_cms_constraint(&self) -> bool {
        matches!(self.peek(), STok::Ident(w) if matches!(
            w.to_ascii_uppercase().as_str(),
            "KEY" | "INDEX" | "UNIQUE" | "FULLTEXT" | "SPATIAL"
        ))
    }

    /// Parses one MySQL constraint line into the table's secondary indexes
    /// (cms only). Composite keys keep only the first column (non-unique) —
    /// a unique composite would reject valid rows under the single-column
    /// index model.
    #[cfg(feature = "cms")]
    fn parse_cms_table_constraint(&mut self, t: &mut ChromosomeTable) -> Result<(), String> {
        let unique = self.kw("UNIQUE");
        let fulltext = self.kw("FULLTEXT") || self.kw("SPATIAL");
        // Optional second keyword: UNIQUE KEY / UNIQUE INDEX / FULLTEXT KEY /
        // plain KEY / INDEX.
        let _ = self.kw("KEY") || self.kw("INDEX");
        // Optional index name (absent when the column list starts directly).
        let mut name = String::new();
        if !matches!(self.peek(), STok::LParen) {
            name = self.ident()?;
        }
        self.expect(STok::LParen)?;
        let mut first_col = String::new();
        let mut composite = false;
        loop {
            let c = self.ident()?;
            self.consume_type_len()?; // (191) prefix length — dropped
            if first_col.is_empty() {
                first_col = c;
            } else {
                composite = true;
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(STok::RParen)?;
        if first_col.is_empty() {
            return Err("SQL: empty key column list".into());
        }
        if name.is_empty() {
            name = format!("{}_{}_idx", t.name, first_col);
        }
        let enforced_unique = unique && !composite && !fulltext;
        t.secondary_indexes.push(SecondaryIndexMeta {
            name,
            column: first_col,
            unique: enforced_unique,
        });
        Ok(())
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
        let mut left = self.parse_cmp()?;
        while self.kw("AND") {
            let right = self.parse_cmp()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn parse_cmp(&mut self) -> Result<Expr, String> {
        // v3.6: `NOT <predicate>` including `[NOT] EXISTS (SELECT ...)`.
        if self.kw("NOT") {
            let inner = self.parse_cmp()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("EXISTS")) {
            self.expect_kw("EXISTS")?;
            self.expect(STok::LParen)?;
            if !self.kw("SELECT") {
                return Err("SQL: EXISTS requires a SELECT subquery".into());
            }
            let sub = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            return Ok(Expr::Exists {
                stmt: Box::new(sub),
                negated: false,
            });
        }
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
        // `col [NOT] IN (v1, v2, ...)` / `col [NOT] IN (SELECT ...)`
        // CMS edition: NOT IN is emitted by WordPress core constantly
        // (`comment_type NOT IN ('note')`, post_status NOT IN, …).
        let mut negated_in = false;
        if self.kw("NOT") {
            if self.kw("IN") {
                negated_in = true;
            } else {
                self.expect_kw("LIKE")?;
                let pattern = self.like_pattern()?;
                return Ok(Expr::Like {
                    operand: Box::new(left),
                    pattern,
                    negated: true,
                });
            }
        }
        if negated_in || self.kw("IN") {
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
                let in_sub = Expr::InSubquery { column, query };
                // NOT IN (SELECT …): negate the materialised IN node.
                return if negated_in {
                    Ok(Expr::Not(Box::new(in_sub)))
                } else {
                    Ok(in_sub)
                };
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
            return Ok(Expr::InList { column, values, negated: negated_in });
        }
        if self.kw("IS") {
            if self.kw("NOT") {
                self.expect_kw("NULL")?;
                return Ok(Expr::IsNotNull(Box::new(left)));
            }
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(left)));
        }
        // MySQL `expr LIKE 'pattern'` (the `NOT` variant is handled in the
        // IN/NOT-IN block above). WordPress core uses LIKE for transient
        // cleanup, slug lookups and meta searches.
        if self.kw("LIKE") {
            let pattern = self.like_pattern()?;
            return Ok(Expr::Like {
                operand: Box::new(left),
                pattern,
                negated: false,
            });
        }
        Ok(left)
    }

    /// Pattern literal of a LIKE predicate (quoted string, optionally
    /// parenthesized — WordPress sends both `LIKE 'x'` and `LIKE ('x')`).
    fn like_pattern(&mut self) -> Result<String, String> {
        let paren = matches!(self.peek(), STok::LParen);
        if paren {
            self.next();
        }
        let pattern = match self.next() {
            STok::Str(s) => s,
            t => return Err(format!("SQL: LIKE expects a quoted pattern, got {:?}", t)),
        };
        if paren {
            self.expect(STok::RParen)?;
        }
        Ok(pattern)
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
                // MySQL `CAST(expr AS type)` (WordPress meta queries:
                // `CAST(meta_value AS SIGNED) > 100`).
                if s.eq_ignore_ascii_case("CAST") && matches!(self.peek_at(1), STok::LParen) {
                    self.next();
                    self.next(); // (
                    let inner = self.parse_or()?;
                    self.expect_kw("AS")?;
                    let ty = match self.next() {
                        STok::Ident(t) if t.eq_ignore_ascii_case("SIGNED") => CastTy::Signed,
                        STok::Ident(t) if t.eq_ignore_ascii_case("UNSIGNED") => CastTy::Unsigned,
                        STok::Ident(t)
                            if t.eq_ignore_ascii_case("CHAR")
                                || t.eq_ignore_ascii_case("NCHAR")
                                || t.eq_ignore_ascii_case("BINARY") =>
                        {
                            CastTy::Char
                        }
                        t => {
                            return Err(format!(
                                "SQL: CAST expects SIGNED/UNSIGNED/CHAR, got {:?}",
                                t
                            ))
                        }
                    };
                    // Optional CHAR(n) width / SIGNED INTEGER keyword.
                    if matches!(self.peek(), STok::LParen) {
                        self.next();
                        let _ = self.next(); // width literal
                        self.expect(STok::RParen)?;
                    } else if matches!(self.peek(), STok::Ident(w) if w.eq_ignore_ascii_case("INTEGER"))
                    {
                        self.next();
                    }
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Cast {
                        inner: Box::new(inner),
                        ty,
                    });
                }
                // aggregate reference (HAVING COUNT(*) > 2): resolves to the
                // rendered aggregate column of the grouped result row.
                if matches!(self.peek_at(1), STok::LParen) && AggFunc::from_name(&s).is_ok() {
                    self.next();
                    self.next(); // (
                    let mut distinct = false;
                    if matches!(self.peek(), STok::Ident(d) if d.eq_ignore_ascii_case("DISTINCT")) {
                        self.next();
                        distinct = true;
                    }
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        Some(self.ident()?)
                    };
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Column(render_agg_distinct(
                        &s,
                        arg.as_deref(),
                        distinct,
                    )));
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
                // v3.6: scalar subquery `(SELECT ...)` in operand position
                // (WHERE comparisons, arithmetic, IS NULL checks).
                if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT")) {
                    self.next(); // SELECT (parse_select_tail expects it gone)
                    let sub = self.parse_select_tail()?;
                    self.expect(STok::RParen)?;
                    return Ok(Expr::ScalarSubquery {
                        stmt: Box::new(sub),
                    });
                }
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
    let mut parser = SParser {
        toks,
        pos: 0,
        depth: 0,
    };
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
        Stmt::DropColumn { table, .. } => {
            out.push(table.clone());
        }
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
                match item {
                    SelectItem::Computed { expr, .. } => collect_expr_tables(expr, out)?,
                    // Scalar subqueries contribute their own tables.
                    SelectItem::ScalarSub { stmt, .. } => {
                        collect_stmt_tables(stmt, out)?;
                    }
                    SelectItem::Window { .. } => {}
                    _ => {}
                }
            }
            if let Some(expr) = where_clause {
                collect_expr_tables(expr, out)?;
            }
            if let Some(expr) = having {
                collect_expr_tables(expr, out)?;
            }
        }
        Stmt::Union { left, right, .. } => {
            collect_stmt_tables(left, out)?;
            collect_stmt_tables(right, out)?;
        }
        Stmt::Cte { ctes, body } => {
            for (_, def) in ctes {
                collect_stmt_tables(def, out)?;
            }
            collect_stmt_tables(body, out)?;
        }
        Stmt::ShowTables | Stmt::Tx { .. } => {}
        Stmt::ShowStorage { table, .. } => out.push(table.clone()),
        Stmt::AddColumn { table, .. } => out.push(table.clone()),
        Stmt::Insert { table, rows, on_conflict } => {
            out.push(table.clone());
            // Column values are literals; no table refs beyond the target.
            if let Some(OnConflict::DoUpdate { assignments }) = on_conflict {
                for (col, val) in assignments {
                    let _ = (col, val);
                }
            }
        }
        Stmt::DropTable { table, .. } => out.push(table.clone()),
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
            // The bare form (`DROP INDEX name`) does not name its table, and
            // this function must not touch storage to resolve the name — so a
            // table-restricted user cannot be permission-checked here.
            // Reject it for the ACL path instead of silently authorising an
            // index drop on ANY table (the ACL bypass this used to be).
            // Administrators never reach this code (they skip the ACL), and
            // everybody else can use the documented ON form.
            if table.is_none() {
                return Err(
                    "SQL: DROP INDEX without ON <table> cannot be permission-checked; \
                     use DROP INDEX <name> ON <table>"
                        .to_string(),
                );
            }
            out.push(table.clone().unwrap());
        }
        Stmt::Explain(inner) => collect_stmt_tables(inner, out)?,
    }
    Ok(())
}

/// Every column a statement references (WHERE, HAVING, projections,
/// computed items). Used to distinguish inner columns from correlated
/// outer references of a subquery.
fn collect_stmt_columns(stmt: &Stmt, out: &mut Vec<String>) -> Result<(), String> {
    match stmt {
        Stmt::Select {
            projection,
            where_clause,
            having,
            ..
        } => {
            for item in projection {
                match item {
                    SelectItem::Column { source, .. } => {
                        if !out.contains(source) {
                            out.push(source.clone());
                        }
                    }
                    SelectItem::Computed { expr, .. } => {
                        expr.collect_columns(out);
                    }
                    SelectItem::Agg { arg, .. } => {
                        if let Some(c) = arg {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                    }
                    SelectItem::ScalarSub { stmt, .. } => collect_stmt_columns(stmt, out)?,
                    SelectItem::Window { partition, order, .. } => {
                        for c in partition {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                        for (c, _) in order {
                            if !out.contains(c) {
                                out.push(c.clone());
                            }
                        }
                    }
                    // CMS edition: ORDER BY sort keys are internal — they
                    // create no outer references of their own.
                    SelectItem::OrderKeyOnly { .. }
                    | SelectItem::AliasStar(_)
                    | SelectItem::Star => {}
                }
            }
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
            if let Some(e) = having {
                e.collect_columns(out);
            }
        }
        Stmt::Union { left, right, order_by, .. } => {
            collect_stmt_columns(left, out)?;
            collect_stmt_columns(right, out)?;
            for (c, _) in order_by {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
        }
        Stmt::Cte { ctes, body } => {
            for (_, def) in ctes {
                collect_stmt_columns(def, out)?;
            }
            collect_stmt_columns(body, out)?;
        }
        Stmt::Update { assignments, where_clause, .. } => {
            for (c, _) in assignments {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
        }
        Stmt::Delete { where_clause, .. } => {
            if let Some(e) = where_clause {
                e.collect_columns(out);
            }
        }
        Stmt::Insert { rows, .. } => {
            for row in rows {
                for (c, _) in row {
                    if !out.contains(c) {
                        out.push(c.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_expr_tables(expr: &Expr, out: &mut Vec<String>) -> Result<(), String> {
    match expr {
        Expr::Like { operand, .. } => collect_expr_tables(operand, out)?,
        Expr::InSubquery { query, .. } => {
            let nested = referenced_tables(query)?;
            out.extend(nested);
        }
        Expr::ScalarSubquery { stmt } | Expr::Exists { stmt, .. } => {
            collect_stmt_tables(stmt, out)?;
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
        Expr::Cast { inner, .. } => collect_expr_tables(inner, out)?,
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
            cte_store: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "cms")]
            cms_sequences: std::sync::Arc::new(crate::cms::sequence::AutoIncrementRegistry::new()),
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
        let mut p = SParser { toks, pos: 0, depth: 0 };
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

    /// v4.1: human-readable storage map for one or more parsed SQL
    /// statements: which chromosome (table) and locus (PK) every row lands
    /// at, the exact storage key, and how each value is physically
    /// represented. Read-only — nothing is executed. Used by
    /// `dna-db-core sql2dnaql` (the "storage map" section) and documented
    /// in SQL_MANUAL.
    pub fn explain_storage(&self, sql: &str) -> Result<String, String> {
        let toks = stokenize(sql)?;
        let mut p = SParser { toks, pos: 0, depth: 0 };
        let mut out = Vec::new();
        while p.peek() != &STok::Eof {
            let stmt = p.parse_stmt()?;
            out.push(self.storage_map_stmt(&stmt)?);
        }
        if out.is_empty() {
            return Err("SQL: nothing to map (empty statement)".into());
        }
        Ok(out.join("\n\n"))
    }

    fn storage_map_stmt(&self, stmt: &Stmt) -> Result<String, String> {
        match stmt {
            Stmt::Insert { table, rows, .. } => {
                let schema = self.get_schema(table).ok_or_else(|| {
                    format!(
                        "SQL: relation '{}' does not exist (CREATE TABLE first)",
                        table
                    )
                })?;
                let mut lines = vec![format!(
                    "INSERT into chromosome '{}' — every row becomes one locus \
                     under the key '<table>:<pk>' (DnaQL: <chromosome>:<locus>):",
                    table
                )];
                for row in rows {
                    let pk = row
                        .iter()
                        .find(|(c, _)| *c == schema.primary_locus_column)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| {
                            format!(
                                "SQL: INSERT is missing the primary key column '{}'",
                                schema.primary_locus_column
                            )
                        })?;
                    let key = Self::row_key(table, &pk);
                    lines.push(format!(
                        "  locus {} -> key \"{}\" ({} B payload):",
                        pk,
                        key,
                        RowCodec::encode(row).len()
                    ));
                    for (col, value) in row {
                        lines.push(format!(
                            "    {} [{}] = {} | stored: {}",
                            col,
                            schema.column_type_name(col),
                            value,
                            value.stored_as()
                        ));
                    }
                }
                Ok(lines.join("\n"))
            }
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
                let schema = self.get_schema(table).ok_or_else(|| {
                    format!(
                        "SQL: relation '{}' does not exist (CREATE TABLE first)",
                        table
                    )
                })?;
                let pkcol = &schema.primary_locus_column;
                let locus = where_clause.as_ref().and_then(|e| match e {
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        left,
                        right,
                    } => match (&**left, &**right) {
                        (Expr::Column(c), Expr::Literal(v)) if c == pkcol => Some(v.clone()),
                        (Expr::Literal(v), Expr::Column(c)) if c == pkcol => Some(v.clone()),
                        _ => None,
                    },
                    _ => None,
                });
                let verb = if matches!(stmt, Stmt::Update { .. }) {
                    "UPDATE"
                } else {
                    "DELETE"
                };
                match locus {
                    Some(v) => {
                        let key = Self::row_key(table, &v);
                        Ok(format!(
                            "{} addresses exactly one locus: key \"{}\" \
                             (chromosome '{}', locus {})",
                            verb, key, table, v
                        ))
                    }
                    None => Ok(format!(
                        "{} scans chromosome '{}' (key prefix \"{}:\") and \
                         touches every locus matching the predicate; add \
                         WHERE {} = <literal> to address exactly one locus",
                        verb, table, table, pkcol
                    )),
                }
            }
            Stmt::Select { from, .. } if !from.is_empty() => Ok(format!(
                "SELECT reads loci of chromosome '{}' — point gets on \
                     \"{}:<pk>\" or a prefix scan \"{}:\"",
                from, from, from
            )),
            Stmt::CreateTable { table, .. } => Ok(format!(
                "table '{}' -> chromosome '{}'; the schema itself is a \
                     meta-row under the key \"{}:schema\"",
                table.name, table.name, table.name
            )),
            Stmt::DropTable { table, .. } => Ok(format!(
                "DROP removes chromosome '{}' with all its loci \
                     (keys \"{}:<pk>\") and the meta-row \"{}:schema\"",
                table, table, table
            )),
            other => Ok(format!(
                "({} — does not address row storage directly)",
                Self::describe_stmt(other)
            )),
        }
    }

    pub fn execute(&self, sql: &str) -> Result<QueryResult, String> {
        let mut tx = None;
        self.execute_in_tx(sql, &mut tx)
    }

    /// Execute a statement with an optional session transaction (v3.6).
    /// `tx` carries the `BEGIN`/`COMMIT`/`ROLLBACK` state across calls:
    /// while active, DML operations are buffered and applied as one atomic
    /// WAL batch on COMMIT; ROLLBACK discards them.
    pub fn execute_in_tx(
        &self,
        sql: &str,
        tx: &mut Option<SqlTxBuffer>,
    ) -> Result<QueryResult, String> {
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
        let trimmed_upper = sql.trim_start().to_ascii_uppercase();
        // SELECT VERSION() — server identity string (v3.6; used by pgwire
        // clients, DBeaver, psql and the LiveConsole preview).
        if trimmed_upper == "SELECT VERSION()"
            || trimmed_upper.starts_with("SELECT VERSION()")
                && (trimmed_upper.len() == "SELECT VERSION()".len()
                    || trimmed_upper["SELECT VERSION()".len()..].starts_with(';'))
        {
            let mut m = std::collections::BTreeMap::new();
            m.insert(
                "version".to_string(),
                Value::Text(Self::server_version()),
            );
            return Ok(QueryResult {
                columns: vec!["version".to_string()],
                rows: vec![m],
                execution_plan: "ScalarEvaluate(VERSION())".to_string(),
                execution_time_us: start.elapsed().as_micros() as u64,
                message: None,
            });
        }
        let toks = stokenize(sql)?;
        let mut p = SParser { toks, pos: 0, depth: 0 };
        let stmt = p.parse_stmt()?;
        if p.peek() != &STok::Eof {
            return Err(format!(
                "SQL: unexpected trailing tokens starting at {:?} (unsupported syntax?)",
                p.peek()
            ));
        }
        let (columns, rows, plan, message) = self.exec_stmt(stmt, tx)?;
        Ok(QueryResult {
            columns,
            rows,
            execution_plan: plan,
            execution_time_us: start.elapsed().as_micros() as u64,
            message,
        })
    }

    /// Server identity string reported by VERSION() and pgwire.
    pub fn server_version() -> String {
        format!(
            "DNA-DB v{} (Linux x86_64, LSM RS(4+2), pgwire 5432)",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn exec_stmt(
        &self,
        stmt: Stmt,
        tx: &mut Option<SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        match stmt {
            Stmt::Select {
                projection,
                from,
                from_alias,
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
                from_alias.as_deref(),
                &joins,
                where_clause,
                &group_by,
                having,
                &order_by,
                limit,
                offset,
            ),
            Stmt::Union {
                left,
                right,
                all,
                order_by,
                limit,
                offset,
            } => self.exec_union(*left, *right, all, order_by, limit, offset),
            Stmt::Cte { ctes, body } => self.exec_cte(ctes, *body, tx),
            Stmt::ShowTables => self.exec_show_tables(),
            Stmt::ShowStorage { table, pk } => self.exec_show_storage(&table, pk.as_ref()),
            Stmt::Tx { action } => self.exec_tx(action, tx),
            Stmt::Insert { table, rows, on_conflict } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_insert_tx(&table, rows, on_conflict, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_insert_tx(&table, rows, on_conflict, None),
                }
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_update_tx(&table, &assignments, where_clause, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_update_tx(&table, &assignments, where_clause, None),
                }
            }
            Stmt::Delete { table, where_clause } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_delete_tx(&table, where_clause, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_delete_tx(&table, where_clause, None),
                }
            }
            Stmt::CreateTable {
                table,
                if_not_exists,
            } => self.exec_create_table(table, if_not_exists),
            Stmt::DropTable { table, if_exists } => self.exec_drop_table(&table, if_exists),
            Stmt::CreateIndex {
                name,
                table,
                column,
                unique,
                if_not_exists,
            } => self.exec_create_index(&name, &table, &column, unique, if_not_exists),
            Stmt::DropIndex {
                name,
                if_exists,
                table,
            } => self.exec_drop_index(&name, if_exists, table.as_deref()),
            Stmt::AddColumn {
                table,
                column,
                col_type,
                col_default,
            } => self.exec_add_column(&table, &column, col_type, col_default.as_deref()),
            Stmt::DropColumn { table, column } => self.exec_drop_column(&table, &column),
            Stmt::Explain(inner) => self.exec_explain(*inner),
        }
    }

    // ---------- SHOW / transactions / CTE ----------

    fn exec_show_tables(
        &self,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schemas = self.schemas.read();
        let mut rows = Vec::new();
        for (name, schema) in schemas.iter() {
            let mut r = BTreeMap::new();
            r.insert("table_name".to_string(), Value::Text(name.clone()));
            r.insert(
                "engine".to_string(),
                Value::Text("LSM+ReedSolomon RS(4+2)".to_string()),
            );
            r.insert(
                "columns".to_string(),
                Value::Int(schema.columns.len() as i64),
            );
            rows.push(r);
        }
        // CTEs are statement-scoped, never listed here.
        let n = rows.len();
        Ok((
            vec!["table_name".into(), "engine".into(), "columns".into()],
            rows,
            format!("ShowTables({} table(s))", n),
            Some(format!("{} table(s)", n)),
        ))
    }

    /// v4.1: SHOW STORAGE — the SQL→DnaQL→DNA storage map. Read-only:
    /// explains where rows physically live, never mutates anything.
    /// Without a PK predicate: one summary row per stored locus. With
    /// `WHERE <pk-col> = <literal>`: per-column detail for that locus,
    /// values decoded to the human-readable form next to the physical
    /// RowCodec representation (nucleotide chains included).
    fn exec_show_storage(
        &self,
        table: &str,
        pk: Option<&(String, Value)>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self.get_schema(table).ok_or_else(|| {
            format!(
                "SQL: relation '{}' does not exist (CREATE TABLE first)",
                table
            )
        })?;
        match pk {
            Some((col, lit)) => {
                if *col != schema.primary_locus_column {
                    return Err(format!(
                        "SQL: SHOW STORAGE filters by the primary key '{}' — '{}' is not the PK of '{}'",
                        schema.primary_locus_column, col, table
                    ));
                }
                let key = Self::row_key(table, lit);
                let payload = self
                    .engine
                    .get(key.as_bytes())
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("SQL: locus {} not found", key))?;
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                let mut rows = Vec::new();
                for (fname, value) in fields {
                    let mut r = BTreeMap::new();
                    r.insert("chromosome".to_string(), Value::Text(table.to_string()));
                    r.insert("locus".to_string(), Value::Text(lit.to_string()));
                    r.insert("storage_key".to_string(), Value::Text(key.clone()));
                    r.insert("column".to_string(), Value::Text(fname.clone()));
                    r.insert(
                        "sql_type".to_string(),
                        Value::Text(schema.column_type_name(&fname)),
                    );
                    r.insert("value".to_string(), Value::Text(value.to_string()));
                    r.insert("stored_as".to_string(), Value::Text(value.stored_as()));
                    rows.push(r);
                }
                let n = rows.len();
                Ok((
                    vec![
                        "chromosome".into(),
                        "locus".into(),
                        "storage_key".into(),
                        "column".into(),
                        "sql_type".into(),
                        "value".into(),
                        "stored_as".into(),
                    ],
                    rows,
                    format!("ShowStorage(point {})", key),
                    Some(format!(
                        "locus {}:{}: {} column(s); values are shown decoded, \
                         stored_as describes the physical RowCodec bytes",
                        table, lit, n
                    )),
                ))
            }
            None => {
                let prefix = format!("{}:", table);
                let raw = self
                    .engine
                    .scan_prefix(prefix.as_bytes())
                    .map_err(|e| e.to_string())?;
                let mut rows = Vec::new();
                for (key, payload) in raw {
                    if String::from_utf8_lossy(&key).ends_with(":schema") {
                        continue;
                    }
                    let fields = RowCodec::decode(&payload).map_err(|e| {
                        format!("row {} unreadable: {}", String::from_utf8_lossy(&key), e)
                    })?;
                    let dna_columns = fields
                        .iter()
                        .filter(|(_, v)| matches!(v, Value::Dna(_)))
                        .count();
                    let key_s = String::from_utf8_lossy(&key).to_string();
                    let locus = key_s
                        .strip_prefix(&prefix)
                        .unwrap_or("?")
                        .to_string();
                    let mut r = BTreeMap::new();
                    r.insert("chromosome".to_string(), Value::Text(table.to_string()));
                    r.insert("locus".to_string(), Value::Text(locus));
                    r.insert("storage_key".to_string(), Value::Text(key_s));
                    r.insert("fields".to_string(), Value::Int(fields.len() as i64));
                    r.insert(
                        "payload_bytes".to_string(),
                        Value::Int(payload.len() as i64),
                    );
                    r.insert("dna_columns".to_string(), Value::Int(dna_columns as i64));
                    rows.push(r);
                }
                let n = rows.len();
                Ok((
                    vec![
                        "chromosome".into(),
                        "locus".into(),
                        "storage_key".into(),
                        "fields".into(),
                        "payload_bytes".into(),
                        "dna_columns".into(),
                    ],
                    rows,
                    format!("ShowStorage(scan {})", table),
                    Some(format!(
                        "chromosome '{}': {} stored locus/loci; add \
                         WHERE <pk> = <literal> for the per-column detail",
                        table, n
                    )),
                ))
            }
        }
    }

    fn exec_tx(
        &self,
        action: TxAction,
        tx: &mut Option<SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        match action {
            TxAction::Begin => {
                if tx.as_ref().map(|t| t.active).unwrap_or(false) {
                    return Err("SQL: transaction already active (COMMIT or ROLLBACK first)".into());
                }
                *tx = Some(SqlTxBuffer {
                    ops: Vec::new(),
                    active: true,
                });
                Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("BEGIN".into()))])],
                    "TxBegin".into(),
                    Some("transaction started; DML is buffered until COMMIT".into()),
                ))
            }
            TxAction::Commit => {
                let buf = tx.take();
                match buf {
                    Some(buf) if buf.active => {
                        let n = buf.ops.len();
                        if n > 0 {
                            self.engine
                                .write_batch(&buf.ops)
                                .map_err(|e| format!("COMMIT failed: {}", e))?;
                        }
                        Ok((
                            vec!["status".into()],
                            vec![mk_row(&[("status", Value::Text("COMMIT".into()))])],
                            format!("TxCommit(ops={})", n),
                            Some(format!(
                                "{} buffered operation(s) applied in ONE atomic WAL frame",
                                n
                            )),
                        ))
                    }
                    _ => Err("SQL: COMMIT without an active transaction".into()),
                }
            }
            TxAction::Rollback => {
                let buf = tx.take();
                let n = buf.map(|b| b.ops.len()).unwrap_or(0);
                Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("ROLLBACK".into()))])],
                    format!("TxRollback(discarded={})", n),
                    Some(format!("{} buffered operation(s) discarded", n)),
                ))
            }
        }
    }

    fn exec_cte(
        &self,
        ctes: Vec<(String, Stmt)>,
        body: Stmt,
        tx: &mut Option<SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut plan = String::from("CTE(");
        {
            let mut store = self.cte_store.lock().unwrap();
            if !store.is_empty() {
                return Err("SQL: nested WITH statements are not supported".into());
            }
        }
        // Execute each CTE body WITHOUT holding the store lock: statement
        // execution reads tables through read_table_rows(), which locks the
        // same store (std Mutex is not reentrant — holding it here used to
        // deadlock on the very first CTE that touched any table).
        for (name, def) in &ctes {
            if self.get_schema(name).is_some() {
                return Err(format!(
                    "SQL: CTE name '{}' conflicts with an existing table",
                    name
                ));
            }
            let (_, rows, _cplan, _) = self.exec_stmt(def.clone(), &mut None)?;
            let mut store = self.cte_store.lock().unwrap();
            if store.contains_key(name) {
                return Err(format!("SQL: duplicate CTE name '{}'", name));
            }
            plan.push_str(&format!("{}[{} rows] ", name, rows.len()));
            store.insert(name.clone(), rows);
        }
        plan.push_str(")");
        let result = self.exec_stmt(body, tx);
        // Statement-scoped: whatever happens, the CTE data does not outlive it.
        self.cte_store.lock().unwrap().clear();
        let (columns, rows, body_plan, message) = result?;
        plan.push_str(&body_plan);
        Ok((columns, rows, plan, message))
    }

    /// v3.6 UNION / UNION ALL executor. Both sides must be SELECT-shaped
    /// statements with the same number of output columns. `UNION` dedups
    /// full rows, `UNION ALL` keeps everything. Top-level ORDER BY / LIMIT /
    /// OFFSET apply to the compound result.
    fn exec_union(
        &self,
        left: Stmt,
        right: Stmt,
        all: bool,
        order_by: Vec<(String, bool)>,
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
        let (lcols, mut lrows, lplan, _) = self.exec_select_stmt(&left)?;
        let (rcols, rrows, rplan, _) = self.exec_select_stmt(&right)?;
        if lcols.len() != rcols.len() {
            return Err(format!(
                "SQL: UNION requires the same number of columns ({} vs {})",
                lcols.len(),
                rcols.len()
            ));
        }
        // Standard SQL: output column names come from the FIRST select.
        // Right rows carry their own names — rename positionally so the
        // combined result set is homogeneous (UNION of `SELECT username`
        // and `SELECT status` must not produce mixed column keys).
        let renamed = rrows
            .into_iter()
            .map(|row| {
                let mut out = std::collections::BTreeMap::new();
                for (i, v) in row.into_values().enumerate() {
                    if let Some(name) = lcols.get(i) {
                        out.insert(name.clone(), v);
                    }
                }
                out
            })
            .collect::<Vec<_>>();
        let _ = rcols;
        let mut plan = format!(
            "Union({}) [ {} | {} ]",
            if all { "ALL" } else { "DISTINCT" },
            lplan,
            rplan
        );
        lrows.extend(renamed);
        if !all {
            // Dedup by the canonical encoding of every field.
            let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            lrows.retain(|row| {
                let mut key = Vec::new();
                for (k, v) in row.iter() {
                    key.extend_from_slice(k.as_bytes());
                    key.push(0);
                    key.extend_from_slice(&encode_index_value(v));
                    key.push(0);
                }
                seen.insert(key)
            });
        }
        // Top-level ORDER BY / LIMIT / OFFSET.
        if !order_by.is_empty() {
            plan.push_str(&format!(
                " -> Sort({})",
                order_by
                    .iter()
                    .map(|(k, asc)| format!("{} {}", k, if *asc { "ASC" } else { "DESC" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            lrows.sort_by(|a, b| {
                for (key, asc) in &order_by {
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
        if offset > 0 {
            lrows.drain(0..offset.min(lrows.len()));
        }
        if let Some(n) = limit {
            lrows.truncate(n);
        }
        let columns: Vec<String> = lrows
            .first()
            .map(|r| r.keys().cloned().collect())
            .unwrap_or_default();
        let n = lrows.len();
        Ok((
            columns,
            lrows,
            plan,
            Some(format!("{} row(s) after UNION {}", n, if all { "ALL" } else { "DISTINCT" })),
        ))
    }

    /// Run a SELECT-shaped statement (Select or nested Union/CTE) and return
    /// its result tuple. Used by UNION and EXPLAIN.
    fn exec_select_stmt(
        &self,
        stmt: &Stmt,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        match stmt {
            Stmt::Select {
                projection,
                from,
                from_alias,
                joins,
                where_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => self.exec_select(
                projection,
                from,
                from_alias.as_deref(),
                joins,
                where_clause.clone(),
                group_by,
                having.clone(),
                order_by,
                *limit,
                *offset,
            ),
            Stmt::Union {
                left,
                right,
                all,
                order_by,
                limit,
                offset,
            } => self.exec_union(
                (**left).clone(),
                (**right).clone(),
                *all,
                order_by.clone(),
                *limit,
                *offset,
            ),
            Stmt::Cte { ctes, body } => {
                let mut none = None;
                self.exec_cte(ctes.clone(), (**body).clone(), &mut none)
            }
            other => Err(format!(
                "SQL: expected a SELECT statement, got {}",
                Self::describe_stmt(other)
            )),
        }
    }

    /// Columns referenced by a (sub)statement that are NOT resolvable from
    /// its own tables — i.e. correlated references to the outer query.
    /// A column counts as "own" when it exists in the schema of one of the
    /// statement's tables; unknown tables (CTEs) treat all their WHERE
    /// columns as own columns to avoid false correlation.
    fn stmt_outer_refs(&self, stmt: &Stmt) -> Result<Vec<String>, String> {
        let mut tables = Vec::new();
        collect_stmt_tables(stmt, &mut tables)?;
        let mut own_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
        for t in &tables {
            if let Some(schema) = self.get_schema(t) {
                for c in schema.columns.keys() {
                    own_cols.insert(c.clone());
                }
                own_cols.insert(schema.primary_locus_column.clone());
            }
        }
        let mut refs = Vec::new();
        collect_stmt_columns(stmt, &mut refs)?;
        refs.retain(|c| !own_cols.contains(c));
        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    /// Resolve subquery nodes of `e` for ONE outer row: replace
    /// ScalarSubquery / Exists / correlated InSubquery with literal values.
    /// Non-subquery subtrees are evaluated directly (they stay intact).
    fn resolve_expr_for_row(&self, e: &Expr, row: &BTreeMap<String, Value>) -> Result<Expr, String> {
        Ok(match e {
            Expr::Like { operand, pattern, negated } => Expr::Like {
                operand: Box::new(self.resolve_expr_for_row(operand, row)?),
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::ScalarSubquery { stmt } => {
                match self.eval_scalar_sub_for_row(stmt, row)? {
                    Some(v) => Expr::Literal(v),
                    // Empty result → SQL NULL (three-valued logic).
                    None => Expr::Literal(Value::Null),
                }
            }
            Expr::Exists { stmt, negated } => {
                let mut sub = (**stmt).clone();
                self.bind_outer_refs_select(&mut sub, row)?;
                if let Stmt::Select { limit, .. } = &mut sub {
                    *limit = Some(1); // EXISTS only needs one row
                }
                let sql = render_stmt_sql(&sub);
                let res = self.execute(&sql)?;
                let exists = !res.rows.is_empty();
                Expr::Literal(Value::Int((exists != *negated) as i64))
            }
            Expr::InSubquery { column, query } => {
                // Re-parse the rendered subquery; correlated references are
                // bound to this row, then the result materialises to an IN list.
                let toks = stokenize(query)?;
                let mut p = SParser { toks, pos: 0, depth: 0 };
                let sub = p.parse_stmt()?;
                let mut sub = sub;
                self.bind_outer_refs_select(&mut sub, row)?;
                let sql = render_stmt_sql(&sub);
                let res = self.execute(&sql)?;
                let col = res
                    .columns
                    .first()
                    .cloned()
                    .ok_or_else(|| "SQL: IN subquery returned no columns".to_string())?;
                let values: Vec<Value> = res
                    .rows
                    .iter()
                    .filter_map(|r| r.get(&col).cloned())
                    .collect();
                Expr::InList {
                    column: column.clone(),
                    values,
                    negated: false,
                }
            }
            Expr::Not(inner) => Expr::Not(Box::new(self.resolve_expr_for_row(inner, row)?)),
            Expr::IsNull(inner) => {
                Expr::IsNull(Box::new(self.resolve_expr_for_row(inner, row)?))
            }
            Expr::IsNotNull(inner) => {
                Expr::IsNotNull(Box::new(self.resolve_expr_for_row(inner, row)?))
            }
            Expr::And(a, b) => Expr::And(
                Box::new(self.resolve_expr_for_row(a, row)?),
                Box::new(self.resolve_expr_for_row(b, row)?),
            ),
            Expr::Or(a, b) => Expr::Or(
                Box::new(self.resolve_expr_for_row(a, row)?),
                Box::new(self.resolve_expr_for_row(b, row)?),
            ),
            Expr::Cmp { op, left, right } => Expr::Cmp {
                op: *op,
                left: Box::new(self.resolve_expr_for_row(left, row)?),
                right: Box::new(self.resolve_expr_for_row(right, row)?),
            },
            Expr::Arith { op, left, right } => Expr::Arith {
                op: *op,
                left: Box::new(self.resolve_expr_for_row(left, row)?),
                right: Box::new(self.resolve_expr_for_row(right, row)?),
            },
            Expr::Cast { inner, ty } => Expr::Cast {
                inner: Box::new(self.resolve_expr_for_row(inner, row)?),
                ty: *ty,
            },
            Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => e.clone(),
        })
    }

    /// Rewrite a statement's WHERE (and projection scalar subs) binding every
    /// correlated column reference to the current outer row's value.
    fn bind_outer_refs_select(
        &self,
        stmt: &mut Stmt,
        row: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        let refs = self.stmt_outer_refs(stmt)?;
        if refs.is_empty() {
            return Ok(());
        }
        let Stmt::Select { where_clause, .. } = stmt else {
            return Err("SQL: correlated subquery must be a SELECT".into());
        };
        fn rewrite(e: &Expr, row: &BTreeMap<String, Value>, refs: &[String]) -> Expr {
            match e {
                Expr::Column(c) if refs.contains(c) => {
                    Expr::Literal(row.get(c).cloned().unwrap_or(Value::Null))
                }
                Expr::Not(inner) => Expr::Not(Box::new(rewrite(inner, row, refs))),
                Expr::IsNull(inner) => Expr::IsNull(Box::new(rewrite(inner, row, refs))),
                Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(rewrite(inner, row, refs))),
                Expr::And(a, b) => {
                    Expr::And(Box::new(rewrite(a, row, refs)), Box::new(rewrite(b, row, refs)))
                }
                Expr::Or(a, b) => {
                    Expr::Or(Box::new(rewrite(a, row, refs)), Box::new(rewrite(b, row, refs)))
                }
                Expr::Cmp { op, left, right } => Expr::Cmp {
                    op: *op,
                    left: Box::new(rewrite(left, row, refs)),
                    right: Box::new(rewrite(right, row, refs)),
                },
                Expr::Arith { op, left, right } => Expr::Arith {
                    op: *op,
                    left: Box::new(rewrite(left, row, refs)),
                    right: Box::new(rewrite(right, row, refs)),
                },
                Expr::Cast { inner, ty } => Expr::Cast {
                    inner: Box::new(rewrite(inner, row, refs)),
                    ty: *ty,
                },
                other => other.clone(),
            }
        }
        if let Some(w) = where_clause {
            *w = rewrite(w, row, &refs);
        }
        Ok(())
    }

    /// Execute a scalar subquery for one outer row and return its first
    /// column value (None when the subquery produced no rows → SQL NULL).
    fn eval_scalar_sub_for_row(
        &self,
        stmt: &Stmt,
        row: &BTreeMap<String, Value>,
    ) -> Result<Option<Value>, String> {
        let mut sub = stmt.clone();
        self.bind_outer_refs_select(&mut sub, row)?;
        if let Stmt::Select { limit, .. } = &mut sub {
            *limit = Some(1);
        }
        let sql = render_stmt_sql(&sub);
        let res = self.execute(&sql)?;
        Ok(res
            .rows
            .first()
            .and_then(|r| r.values().next().cloned()))
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
                from_alias,
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
                    from_alias.as_deref(),
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
            // v3.6: compound/CTE selects also run read-only so their plan is
            // the real one.
            Stmt::Union { .. } | Stmt::Cte { .. } => {
                let (_, rows, plan, _) = self.exec_select_stmt(&inner)?;
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
            Stmt::DropColumn { table, column } => {
                format!("AlterTable({} DROP COLUMN {})", table, column)
            }
            Stmt::Insert { table, rows, on_conflict: _ } => {
                format!("Insert({}, {} row(s))", table, rows.len())
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => {
                let set = assignments
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k, v.describe()))
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
            Stmt::Union { left, right, all, .. } => format!(
                "Union({}) {} UNION{} {}",
                if *all { "ALL" } else { "DISTINCT" },
                Self::describe_stmt(left),
                if *all { " ALL" } else { "" },
                Self::describe_stmt(right)
            ),
            Stmt::ShowTables => "ShowTables()".into(),
            Stmt::ShowStorage { table, pk } => match pk {
                Some(_) => format!("ShowStorage(point {})", table),
                None => format!("ShowStorage(scan {})", table),
            },
            Stmt::AddColumn { table, column, .. } => {
                format!("AlterTable({} ADD COLUMN {})", table, column)
            }
            Stmt::Tx { action } => format!("Tx({:?})", action),
            Stmt::Cte { ctes, body } => format!(
                "CTE({}) {}",
                ctes.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", "),
                Self::describe_stmt(body)
            ),
            Stmt::Explain(_) => "Explain(...)".into(),
        }
    }

    // ---------- storage helpers ----------

    fn row_key(table: &str, pk: &Value) -> String {
        format!("{}:{}", table, pk)
    }

    fn read_table_rows(&self, table: &str) -> Result<Vec<BTreeMap<String, Value>>, String> {
        // v3.6: statement-scoped CTE tables take precedence over disk.
        if let Some(rows) = self.cte_store.lock().unwrap().get(table) {
            return Ok(rows.clone());
        }
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
            // CMS edition: MySQL happily keeps several indexes over one
            // column (dbDelta churn creates them); allow duplicates there.
            #[cfg(not(feature = "cms"))]
            return Err(format!(
                "SQL: column '{}.{}' already has an index (rename it there or drop it first)",
                table, column
            ));
            #[cfg(feature = "cms")]
            let _ = (table, column);
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

    /// v3.9 Online DDL: `ALTER TABLE t ADD COLUMN c T` — metadata-only and
    /// instant: the schema row is rewritten (one WAL batch), no existing
    /// row is touched, reads of the new column on old rows yield NULL.
    fn exec_add_column(
        &self,
        table: &str,
        column: &str,
        col_type: ColumnType,
        col_default: Option<&str>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut schema = self
            .get_schema(table)
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        if schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: column '{}' already exists in table '{}'",
                column, table
            ));
        }
        schema.add_column(column.to_string(), col_type);
        if let Some(def) = col_default {
            schema
                .column_defaults
                .insert(column.to_string(), def.to_string());
        }
        let start = std::time::Instant::now();
        self.persist_schema(&schema)?;
        let us = start.elapsed().as_micros() as u64;
        Ok((
            vec!["status".into(), "table".into(), "column".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("table", Value::Text(table.to_string())),
                ("column", Value::Text(column.to_string())),
            ])],
            format!(
                "AlterTable({}, instant metadata-only, {} µs, no row rewrite)",
                table, us
            ),
            Some(format!(
                "column '{}' added to '{}' without blocking reads or writes",
                column, table
            )),
        ))
    }

    /// CMS edition / generic: `ALTER TABLE t DROP COLUMN c` — metadata-only
    /// drop (schema row rewritten; stored payloads keep the old field but it
    /// is no longer visible in queries).
    fn exec_drop_column(
        &self,
        table: &str,
        column: &str,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut schema = self
            .get_schema(table)
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        if !schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: can't DROP COLUMN '{}'; check that it exists in table '{}'",
                column, table
            ));
        }
        schema.columns.remove(column);
        schema.column_defaults.remove(column);
        // CMS edition: keep the declaration-order list in sync.
        schema.column_order.retain(|c| c != column);
        schema.secondary_indexes.retain(|idx| idx.column != column);
        if schema.auto_increment_column.as_deref() == Some(column) {
            schema.auto_increment_column = None;
        }
        self.persist_schema(&schema)?;
        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!("AlterTable({} DROP COLUMN {})", table, column),
            Some(format!("column '{}' dropped from '{}'", column, table)),
        ))
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
        // CMS edition: drop the AUTO_INCREMENT counter with the table —
        // a recreated table must start from 1 again (MySQL parity).
        #[cfg(feature = "cms")]
        self.cms_sequences.reset(table);
        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!("DropTable({})", table),
            None,
        ))
    }

    fn exec_insert_tx(
        &self,
        table: &str,
        rows: Vec<Vec<(String, Value)>>,
        on_conflict: Option<OnConflict>,
        mut sink: Option<&mut SqlTxBuffer>,
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
        let mut skipped_conflicts = 0usize;
        let mut upserted = 0usize;
        let mut seen_keys = std::collections::HashSet::new();
        // UNIQUE indexes: (column, encoded value) -> owning row key, so two
        // rows of one multi-row INSERT cannot slip the same value past us.
        let mut stmt_unique: std::collections::HashMap<(String, Vec<u8>), String> =
            std::collections::HashMap::new();
        for row in rows {
            let map: BTreeMap<String, Value> = row.into_iter().collect();
            // CMS edition: AUTO_INCREMENT assignment. A missing / NULL / 0
            // value for the sequence column gets the next generated id;
            // an explicit id > 0 moves the counter forward (MySQL keeps
            // monotonicity after explicit inserts). Seeded lazily from
            // MAX(pk) so a restarted server resumes the sequence.
            #[cfg(feature = "cms")]
            let map = {
                let mut map = map;
                if let Some(auto_col) = schema.auto_increment_column.clone() {
                    let needs_id = match map.get(&auto_col) {
                        None | Some(Value::Null) => true,
                        Some(Value::Int(0)) => true,
                        _ => false,
                    };
                    if needs_id {
                        let prefix = format!("{}:", table);
                        let seed = || -> Result<u64, String> {
                            let raw = self
                                .engine
                                .scan_prefix(prefix.as_bytes())
                                .map_err(|e| e.to_string())?;
                            let mut max = 0u64;
                            for (k, _) in raw {
                                let key = String::from_utf8_lossy(&k).to_string();
                                if let Some(sfx) = key.strip_prefix(prefix.as_str()) {
                                    if let Ok(v) = sfx.parse::<u64>() {
                                        max = max.max(v);
                                    }
                                }
                            }
                            Ok(max)
                        };
                        let next_id = self.cms_sequences.next(table, seed)?;
                        map.insert(auto_col, Value::Int(next_id as i64));
                    } else if let Some(Value::Int(v)) = map.get(&auto_col) {
                        if *v > 0 {
                            self.cms_sequences.bump_to(table, *v as u64);
                        }
                    }
                }
                map
            };
            // CMS edition: DEFAULT fill — omitted columns get their effective
            // default (explicit DEFAULT or NOT NULL zero-value), matching
            // MySQL non-strict INSERT behavior (CMS_EDITION_RU.md §4.11).
            #[cfg(feature = "cms")]
            let map = {
                let mut map = map;
                for (col, def) in &schema.column_defaults {
                    if map.contains_key(col) {
                        continue;
                    }
                    let v = if def.eq_ignore_ascii_case("NULL") {
                        Value::Null
                    } else if let Ok(i) = def.parse::<i64>() {
                        Value::Int(i)
                    } else if let Ok(f) = def.parse::<f64>() {
                        Value::Float(f)
                    } else {
                        Value::Text(def.clone())
                    };
                    map.insert(col.clone(), v);
                }
                map
            };
            let pk = Self::pk_of(&schema, &map)?;
            let key = Self::row_key(table, &pk);
            if !seen_keys.insert(key.clone()) {
                // v3.6: same statement hit the same PK — ON CONFLICT applies.
                match on_conflict {
                    Some(OnConflict::DoNothing) => {
                        skipped_conflicts += 1;
                        continue;
                    }
                    _ => return Err(format!("duplicate primary key in INSERT: {}", key)),
                }
            }
            if self
                .engine
                .get(key.as_bytes())
                .map_err(|error| error.to_string())?
                .is_some()
            {
                // v3.6: existing row + ON CONFLICT → UPSERT / skip.
                match &on_conflict {
                    Some(OnConflict::DoNothing) => {
                        skipped_conflicts += 1;
                        continue;
                    }
                    Some(OnConflict::DoUpdate { assignments }) => {
                        let raw = self
                            .engine
                            .get(key.as_bytes())
                            .map_err(|error| error.to_string())?
                            .expect("row existence checked above");
                        let mut existing: BTreeMap<String, Value> =
                            RowCodec::decode(&raw)
                                .map_err(|e| format!("row {} unreadable: {}", key, e))?
                                .into_iter()
                                .collect();
                        for (col, val) in assignments {
                            let v = match val {
                                UpsertValue::Literal(v) => v.clone(),
                                UpsertValue::Excluded(source) => map
                                    .get(source)
                                    .cloned()
                                    .ok_or_else(|| {
                                        format!(
                                            "SQL: EXCLUDED.{} not present in the conflicting row",
                                            source
                                        )
                                    })?,
                            };
                            existing.insert(col.clone(), v);
                        }
                        // PK must not change via DO UPDATE SET.
                        let new_pk = Self::pk_of(&schema, &existing)?;
                        if Self::row_key(table, &new_pk) != key {
                            return Err(
                                "SQL: ON CONFLICT DO UPDATE SET cannot change the primary key".into()
                            );
                        }
                        let payload = RowCodec::encode(
                            &existing
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<Vec<_>>(),
                        );
                        batch.extend(Self::index_delta_batch(
                            &schema,
                            None,
                            Some((table, &existing)),
                        ));
                        batch.push((
                            crate::wal::WalOp::Put,
                            key.clone().into_bytes(),
                            Some(payload),
                        ));
                        upserted += 1;
                        continue;
                    }
                    None => return Err(format!("duplicate primary key in INSERT: {}", key)),
                }
            }
            // UNIQUE index enforcement — also covers duplicates between rows
            // of this same statement (via stmt_unique) and against rows that
            // already exist in the engine (via index scan).
            let mut handled_by_unique_upsert = false;
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
                    // CMS edition: `ON DUPLICATE KEY UPDATE` fires on ANY
                    // unique-key conflict (not only the PK). Resolve the
                    // conflicting row through the index entry (the entry
                    // value holds the row key) and apply DO UPDATE.
                    #[cfg(feature = "cms")]
                    if let Some(OnConflict::DoUpdate { assignments }) = &on_conflict {
                        let row_key_bytes = entries[0].1.clone();
                        let raw = self
                            .engine
                            .get(&row_key_bytes)
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| {
                                "UNIQUE index entry points to a missing row".to_string()
                            })?;
                        let mut existing: BTreeMap<String, Value> = RowCodec::decode(&raw)
                            .map_err(|e| format!("row unreadable via unique index: {}", e))?
                            .into_iter()
                            .collect();
                        for (col, val) in assignments {
                            let nv = match val {
                                UpsertValue::Literal(l) => l.clone(),
                                UpsertValue::Excluded(source) => map
                                    .get(source)
                                    .cloned()
                                    .ok_or_else(|| {
                                        format!(
                                            "SQL: VALUES({}) not present in the inserted row",
                                            source
                                        )
                                    })?,
                            };
                            existing.insert(col.clone(), nv);
                        }
                        let new_pk = Self::pk_of(&schema, &existing)?;
                        let new_key = Self::row_key(table, &new_pk);
                        if new_key.as_bytes() != row_key_bytes.as_slice() {
                            return Err(
                                "SQL: ON DUPLICATE KEY UPDATE cannot change the primary key".into(),
                            );
                        }
                        let payload = RowCodec::encode(
                            &existing
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<Vec<_>>(),
                        );
                        batch.extend(Self::index_delta_batch(
                            &schema,
                            None,
                            Some((table, &existing)),
                        ));
                        batch.push((
                            crate::wal::WalOp::Put,
                            row_key_bytes.clone(),
                            Some(payload),
                        ));
                        upserted += 1;
                        handled_by_unique_upsert = true;
                        break;
                    }
                    return Err(format!(
                        "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                        table, idx.column, v, idx.name
                    ));
                }
                stmt_unique.insert((idx.column.clone(), enc), key.clone());
            }
            if handled_by_unique_upsert {
                continue;
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

        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("insert failed: {}", e))?;
        }

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

        let msg = match (upserted, skipped_conflicts) {
            (0, 0) => format!("{} row(s) inserted", n),
            (u, s) => format!(
                "{} row(s) inserted, {} upserted, {} skipped (ON CONFLICT)",
                n, u, s
            ),
        };
        Ok((
            vec!["inserted_key".into(), "pk".into()],
            out_rows,
            format!(
                "BatchInsert({}) rows={} via single WAL frame{}",
                table,
                n + upserted,
                if on_conflict.is_some() { " + UPSERT" } else { "" }
            ),
            Some(msg),
        ))
    }

    fn exec_update_tx(
        &self,
        table: &str,
        assignments: &[(String, Expr)],
        where_clause: Option<Expr>,
        mut sink: Option<&mut SqlTxBuffer>,
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
            for (col, e) in assignments {
                // MySQL semantics: every SET rhs sees the row's OLD values
                // (`stock_quantity = stock_quantity - 1`). Eval failure
                // (unknown column/subquery) yields NULL — three-valued logic.
                new_row.insert(col.clone(), e.eval(&row).unwrap_or(Value::Null));
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
        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("update failed: {}", e))?;
        }

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

        let mut updated = changes.len();
        // CMS edition (MySQL parity): UPDATE's affected count is the number
        // of rows whose values actually CHANGED — wpdb branches on it.
        #[cfg(feature = "cms")]
        {
            updated = changes
                .iter()
                .filter(|(_, old, _, new)| !row_maps_equal(old, new))
                .count();
        }
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

    fn exec_delete_tx(
        &self,
        table: &str,
        where_clause: Option<Expr>,
        mut sink: Option<&mut SqlTxBuffer>,
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
        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("delete failed: {}", e))?;
        }

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
        from_alias: Option<&str>,
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
        // CMS edition: expand `t.*` projections against the alias → table
        // map (MySQL semantics: the table's declared column order).
        let alias_tables: Vec<(String, String)> = {
            let mut m = Vec::new();
            if !from.is_empty() {
                let qual = from_alias
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| from.to_string());
                m.push((qual, from.to_string()));
            }
            for j in joins {
                let qual = j.alias.clone().unwrap_or_else(|| j.table.clone());
                m.push((qual, j.table.clone()));
            }
            m
        };
        let mut expanded: Vec<SelectItem> = Vec::with_capacity(projection.len());
        for item in projection {
            match item {
                SelectItem::AliasStar(qual) => {
                    let table = alias_tables
                        .iter()
                        .find(|(a, _)| a.eq_ignore_ascii_case(qual))
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| {
                            format!("SQL: unknown table or alias '{}' in projection", qual)
                        })?;
                    let schema = self.get_schema(&table).ok_or_else(|| {
                        format!("SQL: relation '{}' does not exist", table)
                    })?;
                    // MySQL semantics: declaration order (CREATE TABLE list,
                    // then ALTER TABLE ADD), not the alphabetical BTreeMap
                    // order — WordPress builds assoc rows from these labels.
                    for col in schema.declared_columns() {
                        expanded.push(SelectItem::Column {
                            source: col.clone(),
                            alias: None,
                        });
                    }
                }
                other => expanded.push(other.clone()),
            }
        }
        let projection: &[SelectItem] = &expanded;
        // v4.0: unknown FROM/JOIN tables are a hard error (SQL semantics:
        // PostgreSQL answers 42P01 undefined_table). CTE tables count as
        // existing while their WITH statement runs. FROM-less SELECTs
        // evaluate a single empty row.
        if !from.is_empty()
            && self.get_schema(from).is_none()
            && !self.cte_contains(from)
        {
            return Err(format!("SQL: relation '{}' does not exist (CREATE TABLE first)", from));
        }
        for j in joins {
            if self.get_schema(&j.table).is_none() && !self.cte_contains(&j.table) {
                return Err(format!(
                    "SQL: relation '{}' does not exist (CREATE TABLE first)",
                    j.table
                ));
            }
        }
        // Materialise IN-subqueries before any planning.
        let where_clause = self.materialize_subqueries(where_clause)?;
        // FROM — candidates via point lookup / secondary index / seq scan.
        // A FROM-less SELECT sources exactly one (empty) row.
        let (mut current, mut plan) = if from.is_empty() {
            (vec![BTreeMap::new()], "ScalarEvaluate()".to_string())
        } else {
            let (rows, op) = self.fetch_candidates(from, where_clause.as_ref())?;
            (rows, op)
        };

        // JOINs (hash join on equi-condition; O(n+m) build+probe)
        for j in joins {
            let right = self.read_table_rows(&j.table)?;
            // ON 1=1 (cross join): merge every left row with every right row;
            // the surviving filter lives in WHERE, exactly like MySQL.
            let is_cross = j.left_col == "1" && j.right_col == "1";
            let next = if is_cross {
                cross_join(&current, &right)
            } else {
                match j.join_type {
                    JoinType::Inner => hash_join(&current, &right, &j.left_col, &j.right_col),
                    JoinType::Left => hash_join_left(&current, &right, &j.left_col, &j.right_col),
                    // RIGHT JOIN == LEFT JOIN with the sides swapped (the row
                    // model is single-namespace, so column naming is unchanged).
                    JoinType::Right => {
                        hash_join_left(&right, &current, &j.right_col, &j.left_col)
                    }
                    // FULL OUTER JOIN: left join + every right row that never
                    // matched, padded with NULLs on the left columns.
                    JoinType::Full => {
                        hash_join_full(&current, &right, &j.left_col, &j.right_col)
                    }
                }
            };
            plan.push_str(&format!(
                " -> {}HashJoin({}.{}, {}.{})",
                match j.join_type {
                    JoinType::Left => "Left",
                    JoinType::Right => "Right",
                    JoinType::Full => "Full",
                    JoinType::Inner => "Inner",
                },
                from,
                j.left_col,
                j.table,
                j.right_col
            ));
            current = next;
        }

        // WHERE (with per-row resolution of correlated subqueries, v3.6)
        if let Some(e) = &where_clause {
            plan.push_str(&format!(" -> Filter({})", e.describe()));
            if expr_has_subquery(e) {
                let mut kept = Vec::with_capacity(current.len());
                for row in current.drain(..) {
                    let resolved = self.resolve_expr_for_row(e, &row)?;
                    if resolved.eval_bool(&row) {
                        kept.push(row);
                    }
                }
                current = kept;
            } else {
                current.retain(|r| e.eval_bool(r));
            }
        }

        // Window functions (v3.6): computed over the filtered row set, before
        // projection and ORDER BY. Not combinable with GROUP BY/aggregates.
        let window_items: Vec<&SelectItem> = projection
            .iter()
            .filter(|p| matches!(p, SelectItem::Window { .. }))
            .collect();
        if !window_items.is_empty() {
            if !collect_agg_items(projection, having.as_ref()).is_empty()
                || !group_by.is_empty()
                || having.is_some()
            {
                return Err(
                    "SQL: window functions cannot be combined with GROUP BY/aggregates".into(),
                );
            }
            for item in &window_items {
                if let SelectItem::Window {
                    func,
                    partition,
                    order,
                    canonical,
                    ..
                } = item
                {
                    compute_window(current.as_mut_slice(), *func, partition, order, canonical);
                    plan.push_str(&format!(" -> Window({})", canonical));
                }
            }
        }

        // Scalar subqueries in the projection (v3.6): uncorrelated ones run
        // once; correlated ones are re-resolved per output row.
        let scalar_subs: Vec<(&String, &Stmt)> = projection
            .iter()
            .filter_map(|p| match p {
                SelectItem::ScalarSub { stmt, canonical, .. } => Some((canonical, stmt.as_ref())),
                _ => None,
            })
            .collect();
        for (canonical, sub) in &scalar_subs {
            let outer_refs = self.stmt_outer_refs(sub)?;
            if outer_refs.is_empty() {
                let rendered_sql = render_stmt_sql(sub);
                let sub_res = self.execute(&rendered_sql)?;
                let value = sub_res
                    .rows
                    .first()
                    .and_then(|r| r.values().next().cloned())
                    .unwrap_or(Value::Null);
                for row in current.iter_mut() {
                    row.insert(canonical.as_str().to_string(), value.clone());
                }
                plan.push_str(&format!(" -> ScalarSub(once: {})", canonical));
            }
        }
        if scalar_subs.iter().any(|(_, s)| {
            self.stmt_outer_refs(s).map(|r| !r.is_empty()).unwrap_or(false)
        }) {
            plan.push_str(" -> ScalarSub(per-row)");
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

        // CMS edition: arithmetic ORDER BY keys (`OrderKeyOnly`) are
        // materialised into the rows under their rendered name so the sort
        // below sees real values. They emit no output column (MySQL
        // returns only the listed projection).
        for item in projection {
            if let SelectItem::OrderKeyOnly { expr, rendered } = item {
                for row in current.iter_mut() {
                    if !row.contains_key(rendered) {
                        let v = expr.eval(row).unwrap_or(Value::Null);
                        row.insert(rendered.clone(), v);
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
                // v3.6: window + scalar-sub items expose their precomputed
                // canonical key; correlated scalar subs evaluate per row.
                let correlated: Vec<(String, String, &Stmt)> = projection
                    .iter()
                    .filter_map(|p| match p {
                        SelectItem::ScalarSub { stmt, canonical, rendered } => {
                            Some((canonical.clone(), rendered.clone(), stmt.as_ref()))
                        }
                        _ => None,
                    })
                    .filter(|(_, _, s)| {
                        self.stmt_outer_refs(s).map(|r| !r.is_empty()).unwrap_or(false)
                    })
                    .collect();
                for item in projection {
                    match item {
                        SelectItem::Column { source, alias } => {
                            // v4.2-cms: resolve the projection column against
                            // the JOIN universe (FROM + every JOIN table).
                            // MySQL collations are case-insensitive —
                            // `SELECT TOTAL_SALES` resolves to total_sales;
                            // a truly unknown column is MySQL errno 1054,
                            // not a silent NULL cell. When no schema is
                            // reachable at all (system/dynamic sources) the
                            // legacy pass-through applies.
                            let mut universe: Vec<String> = vec![from.to_string()];
                            universe.extend(joins.iter().map(|j| j.table.clone()));
                            let mut resolved: Option<String> = None;
                            let mut any_schema = false;
                            for t in &universe {
                                if let Some(schema) = self.get_schema(t) {
                                    any_schema = true;
                                    if schema.columns.contains_key(source.as_str()) {
                                        resolved = Some(source.clone());
                                        break;
                                    }
                                    if let Some(actual) = schema
                                        .columns
                                        .keys()
                                        .find(|c| c.eq_ignore_ascii_case(source))
                                    {
                                        resolved = Some(actual.clone());
                                        break;
                                    }
                                }
                            }
                            let resolved = match resolved {
                                Some(r) => r,
                                None if any_schema => {
                                    return Err(format!(
                                        "SQL: unknown column '{}' in SELECT projection (tables: {})",
                                        source,
                                        universe.join(", ")
                                    ))
                                }
                                None => source.clone(),
                            };
                            let out = alias.clone().unwrap_or_else(|| source.clone());
                            specs.push(if out == *source {
                                source.clone()
                            } else {
                                format!("{} AS {}", source, out)
                            });
                            columns.push((resolved, out));
                        }
                        SelectItem::ScalarSub { canonical, rendered, .. } => {
                            specs.push(format!("{} AS {}", canonical, rendered));
                            columns.push((canonical.clone(), rendered.clone()));
                        }
                        SelectItem::Window { canonical, rendered, .. } => {
                            specs.push(format!("{} AS {}", canonical, rendered));
                            columns.push((canonical.clone(), rendered.clone()));
                        }
                        SelectItem::Computed { expr, rendered } => {
                            specs.push(format!("{} AS {}", expr.describe(), rendered));
                            computed.push((rendered.clone(), expr.clone()));
                        }
                        SelectItem::Agg { rendered, .. } => specs.push(rendered.clone()),
                        // CMS edition: sort keys emit no output column.
                        SelectItem::OrderKeyOnly { .. }
                        | SelectItem::AliasStar(_)
                        | SelectItem::Star => {}
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
                let mut evaluated: Vec<BTreeMap<String, Value>> = Vec::with_capacity(current.len());
                for r in current.into_iter() {
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
                    // Correlated scalar subqueries run against THIS row.
                    for (canonical, out_name, sub) in &correlated {
                        if let Some(v) = self.eval_scalar_sub_for_row(sub, &r)? {
                            out.insert(out_name.clone(), v);
                        } else {
                            let _ = canonical;
                        }
                    }
                    evaluated.push(out);
                }
                current = evaluated;
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
                        SelectItem::Window { rendered, .. } | SelectItem::ScalarSub { rendered, .. } => {
                            if !keep.contains(rendered) {
                                keep.push(rendered.clone());
                            }
                        }
                        // CMS edition: sort keys emit no output column.
                        SelectItem::OrderKeyOnly { .. }
                        | SelectItem::AliasStar(_)
                        | SelectItem::Star => {}
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

        // Column list for tabular consumers. Derived from the projection so
        // zero-row results still carry a RowDescription (PDO / psql need
        // it); `SELECT *` falls back to the FROM schema, then the first row.
        let columns: Vec<String> = {
            let mut cols: Vec<String> = Vec::new();
            for item in projection {
                match item {
                    SelectItem::Column { source, alias } => {
                        // No dedup: MySQL reports duplicated output columns
                        // (t.*, tt.* share term_id) one per projection item.
                        cols.push(alias.clone().unwrap_or_else(|| source.clone()));
                    }
                    SelectItem::Agg { rendered, .. }
                    | SelectItem::Window { rendered, .. }
                    | SelectItem::ScalarSub { rendered, .. }
                    | SelectItem::Computed { rendered, .. } => {
                        if !cols.contains(rendered) {
                            cols.push(rendered.clone());
                        }
                    }
                    // CMS edition: sort keys emit no output column.
                    SelectItem::OrderKeyOnly { .. }
                    | SelectItem::Star
                    | SelectItem::AliasStar(_) => {}
                }
            }
            if cols.is_empty() {
                // `SELECT *` (and zero rows): the FROM table's schema order.
                if let Some(schema) = self.get_schema(from) {
                    // CMS edition: declaration order (MySQL `SELECT *`
                    // reports columns as declared, not alphabetically).
                    cols.extend(schema.declared_columns());
                }
                if cols.is_empty() {
                    cols = current
                        .first()
                        .map(|r| r.keys().cloned().collect())
                        .unwrap_or_default();
                }
            }
            cols
        };
        Ok((columns, current, plan, None))
    }

    /// v3.6: is `name` a live CTE of the statement currently running?
    fn cte_contains(&self, name: &str) -> bool {
        self.cte_store.lock().unwrap().contains_key(name)
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

    /// Replace IN-subquery nodes with materialised IN-lists; resolve
    /// uncorrelated scalar subqueries / EXISTS to literals (v3.6). Correlated
    /// nodes stay in place and are resolved per row by the executor.
    fn materialize_subqueries(&self, expr: Option<Expr>) -> Result<Option<Expr>, String> {
        fn walk(sql: &SqlEngine, e: &Expr) -> Result<Expr, String> {
            match e {
                Expr::Like { operand, pattern, negated } => Ok(Expr::Like {
                    operand: Box::new(walk(sql, operand)?),
                    pattern: pattern.clone(),
                    negated: *negated,
                }),
                Expr::InSubquery { column, query } => {
                    // Correlated? Render and re-parse the subquery, then ask
                    // for outer references. Correlated nodes resolve per row.
                    let toks = stokenize(query)?;
                    let mut p = SParser { toks, pos: 0, depth: 0 };
                    let sub = p.parse_stmt()?;
                    if !sql.stmt_outer_refs(&sub)?.is_empty() {
                        return Ok(e.clone());
                    }
                    let sub_res = sql.execute(query)?;
                    let col = sub_res
                        .columns
                        .first()
                        .cloned()
                        .ok_or_else(|| "SQL: IN subquery returned no columns".to_string())?;
                    let values: Vec<Value> = sub_res
                        .rows
                        .iter()
                        .filter_map(|r| r.get(&col).cloned())
                        .collect();
                    Ok(Expr::InList {
                        column: column.clone(),
                        values,
                        negated: match e {
                            Expr::InList { negated, .. } => *negated,
                            _ => false,
                        },
                    })
                }
                Expr::ScalarSubquery { stmt } => {
                    if !sql.stmt_outer_refs(stmt)?.is_empty() {
                        return Ok(e.clone()); // per-row resolution
                    }
                    let rendered = render_stmt_sql(stmt);
                    let sub_res = sql.execute(&rendered)?;
                    let value = sub_res
                        .rows
                        .first()
                        .and_then(|r| r.values().next().cloned())
                        .unwrap_or(Value::Null);
                    Ok(Expr::Literal(value))
                }
                Expr::Exists { stmt, negated } => {
                    if !sql.stmt_outer_refs(stmt)?.is_empty() {
                        return Ok(e.clone()); // per-row resolution
                    }
                    let mut sub = (**stmt).clone();
                    if let Stmt::Select { limit, .. } = &mut sub {
                        *limit = Some(1);
                    }
                    let res = sql.execute(&render_stmt_sql(&sub))?;
                    let exists = !res.rows.is_empty();
                    Ok(Expr::Literal(Value::Int((exists != *negated) as i64)))
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
                Expr::Cast { inner, ty } => Ok(Expr::Cast {
                    inner: Box::new(walk(sql, inner)?),
                    ty: *ty,
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
            // DISTINCT aggregates (COUNT(DISTINCT col)): encoded values seen
            // per group; duplicates do not count twice.
            seen: Vec<std::collections::HashSet<Vec<u8>>>,
        }
        let n_agg = agg_items.len();
        // "COUNT(DISTINCT customer_id)" — canonical carries the DISTINCT form.
        let distinct: Vec<bool> = agg_items
            .iter()
            .map(|(canonical, _, _, _)| canonical.contains("(DISTINCT"))
            .collect();
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
                seen: vec![std::collections::HashSet::new(); n_agg],
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
                        if distinct[i] && !state.seen[i].insert(encode_index_value(v)) {
                            // Duplicate value in the group — MySQL DISTINCT
                            // semantics: counted/summed once.
                            continue;
                        }
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
                    seen: vec![std::collections::HashSet::new(); n_agg],
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
            Expr::InList { column, values, negated } => {
                if !negated
                    && schema.secondary_index_on(column).is_some()
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
        // COUNT(DISTINCT col): HAVING/ORDER BY references render the
        // canonical form; the aggregation itself runs on the bare column.
        Some(arg.strip_prefix("DISTINCT ").unwrap_or(arg).to_string())
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
fn row_maps_equal(
    a: &BTreeMap<String, Value>,
    b: &BTreeMap<String, Value>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, v)| {
        b.get(k)
            .map(|w| v.compare(w) == std::cmp::Ordering::Equal)
            .unwrap_or(false)
    })
}

fn typed_string_literal(s: &str) -> Value {
    let pure_acgt = !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'));
    if !pure_acgt {
        return Value::Text(s.to_string());
    }
    // CMS edition (branch cms-edition): only literals that are ALREADY
    // uppercase ACGT keep DNA typing. WordPress content is full of
    // lowercase pure-ACGT words ('cat', 'tag', 'gattaca') that the
    // uppercase-normalising sniff would corrupt into pseudo-DNA.
    #[cfg(feature = "cms")]
    if s.chars().all(|c| c.is_ascii_uppercase()) {
        return Value::Dna(s.to_string());
    }
    #[cfg(feature = "cms")]
    return Value::Text(s.to_string());
    #[cfg(not(feature = "cms"))]
    return Value::Dna(s.to_uppercase());
}

/// Canonical output name of an aggregate call: `COUNT(*)`, `SUM(balance)`.
fn render_agg(func: &str, arg: Option<&str>) -> String {
    format!("{}({})", func.to_ascii_uppercase(), arg.unwrap_or("*"))
}

/// Like render_agg, but emits the MySQL DISTINCT form `COUNT(DISTINCT col)`.
fn render_agg_distinct(func: &str, arg: Option<&str>, distinct: bool) -> String {
    if distinct {
        format!(
            "{}(DISTINCT {})",
            func.to_ascii_uppercase(),
            arg.unwrap_or("*")
        )
    } else {
        render_agg(func, arg)
    }
}

/// SQL rendering of a parsed expression (string literals re-quoted) — used
/// to serialise IN-subqueries for re-execution.
fn expr_to_sql(e: &Expr) -> String {
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
    // Arithmetic precedence: Add/Sub bind looser than Mul/Div. A child
    // arithmetic node whose precedence is <= the parent's must be
    // parenthesised, otherwise `a * (b + c)` rendered as `a * b + c`
    // re-parses (and re-executes) to a DIFFERENT result — the silently
    // wrong subquery results this renderer used to produce.
    fn arith_prec(op: &ArithOp) -> u8 {
        match op {
            ArithOp::Add | ArithOp::Sub => 1,
            ArithOp::Mul | ArithOp::Div => 2,
        }
    }
    fn render_arith(op: &ArithOp, left: &Expr, right: &Expr) -> String {
        let prec = arith_prec(op);
        let side = |e: &Expr| match e {
            Expr::Arith { op: child, .. } if arith_prec(child) <= prec => {
                format!("({})", expr_to_sql(e))
            }
            other => expr_to_sql(other),
        };
        format!("{} {} {}", side(left), op.as_str(), side(right))
    }
    match e {
        Expr::Like { operand, pattern, negated } => format!(
            "{} {}LIKE '{}'",
            expr_to_sql(operand),
            if *negated { "NOT " } else { "" },
            pattern
        ),
        Expr::Literal(Value::Text(s)) | Expr::Literal(Value::Dna(s)) => quote(s),
        Expr::Literal(v) => v.to_string(),
        Expr::Column(c) => c.clone(),
        Expr::Not(inner) => format!("NOT {}", expr_to_sql(inner)),
        Expr::And(a, b) => format!("({} AND {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::Or(a, b) => format!("({} OR {})", expr_to_sql(a), expr_to_sql(b)),
        Expr::IsNull(inner) => format!("{} IS NULL", expr_to_sql(inner)),
        Expr::IsNotNull(inner) => format!("{} IS NOT NULL", expr_to_sql(inner)),
        Expr::Arith { op, left, right } => render_arith(op, left, right),
        Expr::Cmp { op, left, right } => {
            format!(
                "{} {} {}",
                expr_to_sql(left),
                op.as_str(),
                expr_to_sql(right)
            )
        }
        Expr::Cast { inner, ty } => {
            format!("CAST({} AS {})", expr_to_sql(inner), ty.as_str())
        }
        Expr::InList { column, values, negated } => format!(
            "{}{} IN ({})",
            if *negated { "NOT " } else { "" },
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
        Expr::ScalarSubquery { stmt } => format!("({})", render_select_sql(stmt)),
        Expr::Exists { stmt, negated } => format!(
            "{}EXISTS ({})",
            if *negated { "NOT " } else { "" },
            render_select_sql(stmt)
        ),
    }
}

/// Render a parsed SELECT back to SQL text (for IN-subquery materialisation).
fn render_select_sql(stmt: &Stmt) -> String {
    let Stmt::Select {
        projection,
        from,
        from_alias: _,
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
            .filter_map(|item| match item {
                SelectItem::Star => Some("*".to_string()),
                SelectItem::AliasStar(qual) => Some(format!("{}.*", qual)),
                SelectItem::Column { source, alias } => match alias {
                    Some(a) if a != source => Some(format!("{} AS {}", source, a)),
                    _ => Some(source.clone()),
                },
                SelectItem::Computed { expr, rendered } => {
                    let base = expr_to_sql(expr);
                    if *rendered == expr.describe() {
                        Some(base)
                    } else {
                        Some(format!("{} AS {}", base, rendered))
                    }
                }
                SelectItem::Agg { canonical, .. } => Some(canonical.clone()),
                SelectItem::ScalarSub { canonical, .. } => Some(canonical.clone()),
                SelectItem::Window { canonical, .. } => Some(canonical.clone()),
                // CMS edition: sort keys are not part of the SQL text.
                SelectItem::OrderKeyOnly { .. } => None,
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
                JoinType::Right => "RIGHT",
                JoinType::Full => "FULL",
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

/// Join-key encoding with SQL comparison semantics:
///  * NULL never matches anything, not even another NULL (three-valued
///    logic) — previously NULL keys were hashed together and joined;
///  * Int and Float compare numerically, so Int(5) and Float(5.0) must
///    land in the same bucket (the raw index encoding is per-type, which
///    silently dropped cross-type matches).
fn join_key(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Float(f)
            if f.is_finite() && f.fract() == 0.0 && *f >= i64::MIN as f64 && *f < i64::MAX as f64 =>
        {
            Some(crate::relational::encode_index_value(&Value::Int(*f as i64)))
        }
        other => Some(crate::relational::encode_index_value(other)),
    }
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
            if let Some(key) = join_key(v) {
                index.entry(key).or_default().push(r);
            }
        }
    }
    let mut out = Vec::new();
    for l in left {
        if let Some(v) = l.get(left_col) {
            if let Some(key) = join_key(v) {
                if let Some(matches) = index.get(&key) {
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
    }
    out
}

/// CMS edition (branch `cms-edition`): pure cross join — the executor form
/// of `INNER JOIN … ON 1=1`, which the compat gate produces when rewriting
/// MySQL comma-FROM lists (`FROM a, b WHERE …`). Same merge rules as the
/// equi-joins: right-side colliding columns get the `joined_` prefix.
fn cross_join(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
) -> Vec<BTreeMap<String, Value>> {
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in left {
        for r in right {
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
        .filter_map(join_key)
        .collect();
    let right_cols: Vec<String> = right
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for l in left {
        let key = l.get(left_col).and_then(join_key);
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

/// v3.6 FULL OUTER JOIN: every matched pair, plus unmatched LEFT rows (right
/// columns NULL) and unmatched RIGHT rows (left columns NULL). NULL keys
/// never match (three-valued logic, same as hash_join/hash_join_left).
fn hash_join_full(
    left_rows: &[BTreeMap<String, Value>],
    right_rows: &[BTreeMap<String, Value>],
    left_col: &str,
    right_col: &str,
) -> Vec<BTreeMap<String, Value>> {
    use std::collections::HashMap;
    // key -> indices into right_rows
    let mut index: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, r) in right_rows.iter().enumerate() {
        if let Some(k) = join_key(r.get(right_col).unwrap_or(&Value::Null)) {
            index.entry(k).or_default().push(i);
        }
    }
    let right_cols: Vec<String> = right_rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    let mut matched_right: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for l in left_rows {
        let key = join_key(l.get(left_col).unwrap_or(&Value::Null));
        let matches: &[usize] = key
            .as_ref()
            .and_then(|k| index.get(k))
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if matches.is_empty() {
            let mut row = l.clone();
            for c in &right_cols {
                if !row.contains_key(c) {
                    row.insert(c.clone(), Value::Null);
                }
            }
            out.push(row);
            continue;
        }
        for &ri in matches {
            matched_right.insert(ri);
            let r = &right_rows[ri];
            let mut row = l.clone();
            for (k, v) in r.iter() {
                row.insert(k.clone(), v.clone());
            }
            out.push(row);
        }
    }
    // Right rows that never matched: keep the right columns, NULL-pad the
    // left side (any column the right row does not carry becomes NULL —
    // the single-namespace model unifies both sides).
    for (i, r) in right_rows.iter().enumerate() {
        if matched_right.contains(&i) {
            continue;
        }
        let mut row = r.clone();
        for (k, v) in row.iter_mut() {
            if !r.contains_key(k) {
                *v = Value::Null;
            }
        }
        out.push(row);
    }
    out
}

/// Does this expression contain SQL subquery nodes (InSubquery that may be
/// correlated, ScalarSubquery, Exists)? Used to switch the WHERE filter to
/// the per-row resolution path.
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery { .. } | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expr_has_subquery(inner)
        }
        Expr::And(a, b) | Expr::Or(a, b) => expr_has_subquery(a) || expr_has_subquery(b),
        Expr::Cmp { left, right, .. } | Expr::Arith { left, right, .. } => {
            expr_has_subquery(left) || expr_has_subquery(right)
        }
        Expr::Like { operand, .. } => expr_has_subquery(operand),
        Expr::Cast { inner, .. } => expr_has_subquery(inner),
        Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => false,
    }
}

/// Canonical rendering of a window function expression.
fn render_window(func: &WindowFunc, partition: &[String], order: &[(String, bool)]) -> String {
    let mut s = format!("{}() OVER (", func.name());
    if !partition.is_empty() {
        s.push_str(&format!("PARTITION BY {} ", partition.join(", ")));
    }
    if !order.is_empty() {
        s.push_str(&format!(
            "ORDER BY {}",
            order
                .iter()
                .map(|(c, asc)| format!("{} {}", c, if *asc { "ASC" } else { "DESC" }))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push(')');
    s
}

/// Render any SELECT-shaped statement back to SQL text (subquery execution).
pub(crate) fn render_stmt_sql(stmt: &Stmt) -> String {
    render_select_sql(stmt)
}

/// Compute a ranking window function over `rows` in place, inserting the
/// result under `canonical`. Rows are grouped by the partition key (encoded
/// values), sorted by the window ORDER BY inside each partition, then
/// numbered: ROW_NUMBER = position, RANK = position of first peer,
/// DENSE_RANK = consecutive peer groups.
fn compute_window(
    rows: &mut [BTreeMap<String, Value>],
    func: WindowFunc,
    partition: &[String],
    order: &[(String, bool)],
    canonical: &str,
) {
    // Index rows by partition key.
    let mut partitions: std::collections::BTreeMap<Vec<u8>, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let mut key = Vec::new();
        for col in partition {
            let v = row.get(col).cloned().unwrap_or(Value::Null);
            key.extend_from_slice(&encode_index_value(&v));
            key.push(0);
        }
        partitions.entry(key).or_default().push(i);
    }
    for (_, mut idxs) in partitions {
        if !order.is_empty() {
            idxs.sort_by(|&a, &b| {
                for (col, asc) in order {
                    let va = rows[a].get(col).cloned().unwrap_or(Value::Null);
                    let vb = rows[b].get(col).cloned().unwrap_or(Value::Null);
                    let ord = va.compare(&vb);
                    if ord != std::cmp::Ordering::Equal {
                        return if *asc { ord } else { ord.reverse() };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let mut rank = 1usize;
        let mut dense = 1usize;
        for (pos, &i) in idxs.iter().enumerate() {
            let value = match func {
                WindowFunc::RowNumber => (pos + 1) as i64,
                WindowFunc::Rank => rank as i64,
                WindowFunc::DenseRank => dense as i64,
            };
            rows[i].insert(canonical.to_string(), Value::Int(value));
            let is_last = pos + 1 == idxs.len();
            if !is_last {
                // Peers share the same ORDER BY values; a new peer group
                // advances RANK to the current position and DENSE_RANK by 1.
                let a = &rows[i];
                let b = &rows[idxs[pos + 1]];
                let peer = order.iter().all(|(col, _)| {
                    let va = a.get(col).cloned().unwrap_or(Value::Null);
                    let vb = b.get(col).cloned().unwrap_or(Value::Null);
                    va.compare(&vb) == std::cmp::Ordering::Equal
                });
                if !peer {
                    rank = pos + 2;
                    dense += 1;
                }
            }
        }
    }
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

    // ---------- v4.1: SHOW STORAGE ----------

    #[test]
    fn show_storage_maps_row_to_locus_and_decodes_values() {
        let (_e, sql) = setup();
        sql.execute(
            "CREATE TABLE chr_samples (sample_id INT, label TEXT, seq DNA, \
             PRIMARY KEY (sample_id))",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_samples (sample_id = 7, label = 'demo', seq = 'CGCGCGATCGATCGCGC')",
        )
        .unwrap();

        // Point mode: one row per column, values decoded, chains included.
        let res = sql
            .execute("SHOW STORAGE FOR chr_samples WHERE sample_id = 7")
            .unwrap();
        assert_eq!(
            res.columns,
            vec![
                "chromosome".to_string(),
                "locus".to_string(),
                "storage_key".to_string(),
                "column".to_string(),
                "sql_type".to_string(),
                "value".to_string(),
                "stored_as".to_string(),
            ]
        );
        let by_col = |name: &str| {
            res.rows
                .iter()
                .find(|r| r.get("column") == Some(&Value::Text(name.into())))
                .unwrap()
                .clone()
        };
        assert_eq!(by_col("sample_id").get("storage_key"),
                   Some(&Value::Text("chr_samples:7".into())));
        assert_eq!(by_col("sample_id").get("sql_type"),
                   Some(&Value::Text("INT".into())));
        assert_eq!(by_col("sample_id").get("value"),
                   Some(&Value::Text("7".into())));
        assert_eq!(by_col("label").get("value"),
                   Some(&Value::Text("demo".into())));
        assert_eq!(by_col("seq").get("sql_type"),
                   Some(&Value::Text("DNA".into())));
        // The chain is shown decoded AND its physical form is described.
        assert_eq!(by_col("seq").get("value"),
                   Some(&Value::Text("CGCGCGATCGATCGCGC".into())));
        assert!(matches!(
            by_col("seq").get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 4") && s.contains("17 nt")
        ));

        // Scan mode: one summary row per stored locus.
        let res2 = sql.execute("SHOW STORAGE FOR chr_samples").unwrap();
        assert_eq!(res2.rows.len(), 1);
        assert_eq!(res2.rows[0].get("storage_key"),
                   Some(&Value::Text("chr_samples:7".into())));
        assert_eq!(res2.rows[0].get("dna_columns"),
                   Some(&Value::Int(1)));
        assert!(matches!(
            res2.rows[0].get("payload_bytes"),
            Some(Value::Int(n)) if *n > 0
        ));

        // Filtering by a non-PK column is rejected, unknown locus errors.
        assert!(sql
            .execute("SHOW STORAGE FOR chr_samples WHERE label = 'demo'")
            .is_err());
        assert!(sql
            .execute("SHOW STORAGE FOR chr_samples WHERE sample_id = 999")
            .is_err());
        assert!(sql.execute("SHOW STORAGE FOR no_such_table").is_err());
    }

    #[test]
    fn explain_storage_maps_insert_before_execution() {
        let (_e, sql) = setup();
        let map = sql
            .explain_storage(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (5, 'ada', 100.0)",
            )
            .unwrap();
        assert!(map.contains("chromosome 'chr_users'"), "{}", map);
        assert!(map.contains("locus 5"), "{}", map);
        assert!(map.contains("chr_users:5"), "{}", map);
        assert!(map.contains("balance [FLOAT] = 100"), "{}", map);
        assert!(map.contains("RowCodec tag 2"), "{}", map);
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

/// Regression tests for the v3.5.0 audit fixes (P0-1, P0-2, P1).
#[cfg(test)]
mod security_regression_tests {
    use super::*;


    #[test]
    fn parser_rejects_pathologically_nested_expressions() {
        // 100k nested parentheses used to recurse once per '(' and smash the
        // stack (with panic=abort that killed the WHOLE server). Now it is a
        // plain parse error.
        let deep = format!(
            "SELECT * FROM chr_t WHERE {} 1 = 1 {}",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        let err = referenced_tables(&deep)
            .err()
            .expect("pathologically deep query must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parser_rejects_deeply_nested_subqueries() {
        let mut query = String::new();
        for _ in 0..2_000 {
            query.push_str("SELECT a FROM chr_t WHERE a IN (");
        }
        query.push_str("1 = 1");
        for _ in 0..2_000 {
            query.push(')');
        }
        let err = referenced_tables(&query)
            .err()
            .expect("deep subquery nesting must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bare_drop_index_is_rejected_for_permission_checking() {
        // The bare form does not name its table, so it cannot be
        // permission-checked without storage access; the ACL resolver must
        // reject it instead of silently authorising a drop on ANY table.
        let err = referenced_tables("DROP INDEX idx_x")
            .err()
            .expect("bare DROP INDEX must not pass the ACL resolver");
        assert!(err.contains("ON <table>"), "unexpected error: {err}");
        // The documented ON form keeps working.
        let tables = referenced_tables("DROP INDEX idx_x ON chr_t").unwrap();
        assert_eq!(tables, vec!["chr_t".to_string()]);
    }

    #[test]
    fn expr_to_sql_keeps_operator_precedence() {
        // a * (b + c) must not render as `a * b + c` (silently wrong
        // materialised subquery results).
        let e = Expr::Arith {
            op: ArithOp::Mul,
            left: Box::new(Expr::Column("a".into())),
            right: Box::new(Expr::Arith {
                op: ArithOp::Add,
                left: Box::new(Expr::Column("b".into())),
                right: Box::new(Expr::Column("c".into())),
            }),
        };
        assert_eq!(expr_to_sql(&e), "a * (b + c)");

        // Left-associative same-precedence chain stays parenthesised.
        let e2 = Expr::Arith {
            op: ArithOp::Sub,
            left: Box::new(Expr::Arith {
                op: ArithOp::Sub,
                left: Box::new(Expr::Column("a".into())),
                right: Box::new(Expr::Column("b".into())),
            }),
            right: Box::new(Expr::Column("c".into())),
        };
        assert_eq!(expr_to_sql(&e2), "(a - b) - c");

        // Higher-precedence child needs no parens.
        let e3 = Expr::Arith {
            op: ArithOp::Add,
            left: Box::new(Expr::Column("a".into())),
            right: Box::new(Expr::Arith {
                op: ArithOp::Mul,
                left: Box::new(Expr::Column("b".into())),
                right: Box::new(Expr::Column("c".into())),
            }),
        };
        assert_eq!(expr_to_sql(&e3), "a + b * c");
    }

    #[test]
    fn hash_join_null_never_matches_and_int_matches_float() {
        fn row(col: &str, v: Value) -> BTreeMap<String, Value> {
            let mut m = BTreeMap::new();
            m.insert(col.to_string(), v);
            m
        }
        let left = vec![
            row("id", Value::Null),
            row("id", Value::Int(5)),
            row("id", Value::Int(7)),
        ];
        let right = vec![
            row("id", Value::Null),
            row("id", Value::Float(5.0)),
            row("id", Value::Int(5)),
        ];
        // Int(5) joins Float(5.0) and Int(5) => 2 result rows; NULL never
        // joins anything (not even NULL = NULL), Int(7) has no rival.
        let joined = hash_join(&left, &right, "id", "id");
        assert_eq!(joined.len(), 2);

        // LEFT join = 2 matched pairs + the NULL-keyed row + the Int(7) row
        // (both kept with NULLs on the right side).
        let left_joined = hash_join_left(&left, &right, "id", "id");
        assert_eq!(left_joined.len(), 4);
    }
}
