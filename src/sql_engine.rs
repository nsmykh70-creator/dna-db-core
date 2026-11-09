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
    /// v3.6: statement-scoped CTE materialisation store. Populated while a
    /// `WITH ... SELECT` runs, always emptied before the statement returns.
    cte_store: std::sync::Mutex<std::collections::HashMap<String, Vec<BTreeMap<String, Value>>>>,
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
        joins: Vec<JoinSpec>,
        where_clause: Option<Expr>,
        group_by: Vec<String>,
        /// v4.2 grouping sets (ROLLUP / CUBE / WITH ROLLUP): empty for a
        /// plain GROUP BY; otherwise one set per super-aggregate level.
        /// `group_by` keeps the declared column list for validation and
        /// NULL-marker insertion; the sets drive the actual aggregation.
        grouping: Vec<Vec<String>>,
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
    /// v4.2: `WITH RECURSIVE` — a CTE whose definition is `base UNION [ALL]
    /// recursive` is evaluated iteratively (the recursive side sees only
    /// the previous round's rows; iteration stops on an empty round).
    Cte {
        ctes: Vec<(String, Stmt)>,
        /// v4.2: WITH RECURSIVE — iterative materialisation per CTE whose
        /// definition is a UNION of a base and a self-referencing side.
        recursive: bool,
        body: Box<Stmt>,
    },
    /// v4.2 full online DDL: `ALTER TABLE t <action>` — ADD / DROP / RENAME
    /// COLUMN and MODIFY / CHANGE COLUMN (type conversion) are all supported.
    /// ADD stays metadata-only and instant; every other action rewrites the
    /// schema and all row payloads in ONE atomic WAL batch.
    AlterTable {
        table: String,
        action: AlterAction,
    },
}

/// v4.2: the action part of an `ALTER TABLE` statement.
#[derive(Debug, Clone)]
pub(crate) enum AlterAction {
    /// `ADD [COLUMN] c T` — metadata-only and instant (v3.9 behaviour kept).
    AddColumn {
        column: String,
        col_type: ColumnType,
    },
    /// `DROP [COLUMN] c` — metadata-only: the column disappears from the
    /// schema (and from every projection), stale bytes inside old row
    /// payloads are ignored by readers and physically removed on the next
    /// rewrite of each row. Secondary indexes on the column are swept.
    DropColumn {
        column: String,
    },
    /// `RENAME COLUMN a TO b` — one atomic batch rewrites the schema plus
    /// every row payload that carries the old key and the secondary index
    /// built over it.
    RenameColumn {
        old: String,
        new: String,
    },
    /// `MODIFY [COLUMN] c T` — every stored value is converted to the new
    /// type. All rows are converted in memory first; a single inconvertible
    /// value aborts the whole statement before anything is written.
    ModifyColumn {
        column: String,
        col_type: ColumnType,
    },
    /// `CHANGE [COLUMN] a b T` — rename + type conversion in one pass.
    ChangeColumn {
        old: String,
        new: String,
        col_type: ColumnType,
    },
    /// `RENAME TO t2` (v4.2.1) — one atomic batch moves the schema row,
    /// every row key and every secondary-index prefix (entries are rewritten
    /// to point at the new name); FKs in other tables are re-pointed.
    RenameTable {
        to: String,
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
    /// (PARTITION BY ... ORDER BY ...)`; v4.2 adds aggregate window
    /// functions (SUM/COUNT/AVG/MIN/MAX over a frame) and ROWS/RANGE
    /// frames. Computed AFTER aggregation (standard SQL order): over the
    /// grouped rows when GROUP BY/aggregates are present, else over the
    /// filtered row set.
    Window {
        func: WindowFunc,
        partition: Vec<String>,
        order: Vec<(String, bool)>,
        /// v4.2: optional explicit `ROWS BETWEEN ... AND ...` / `RANGE ...`
        /// frame; None = the standard default (whole partition without
        /// ORDER BY, RANGE UNBOUNDED PRECEDING..CURRENT ROW with it).
        frame: Option<WindowFrame>,
        /// Canonical rendering used as the internal row key.
        canonical: String,
        /// Output column name: alias if given, else the canonical rendering.
        rendered: String,
    },
}

/// v4.2: one bound of a window frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameBound {
    UnboundedPreceding,
    /// `ROWS n PRECEDING` (n >= 0; 0 == CURRENT ROW).
    Preceding(i64),
    CurrentRow,
    /// `ROWS n FOLLOWING` (n >= 0; 0 == CURRENT ROW).
    Following(i64),
    UnboundedFollowing,
}

/// v4.2: explicit window frame (`ROWS`/`RANGE` between two bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowFrame {
    pub start: FrameBound,
    pub end: FrameBound,
}

/// v3.6 ranking window functions; v4.2 adds aggregates used as window
/// functions (`SUM(x) OVER (...)` etc. — the frame determines the peers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    /// An aggregate evaluated per row over the window frame. `arg == None`
    /// is only valid for COUNT(*).
    Agg {
        func: AggFunc,
        arg: Option<String>,
    },
}

impl WindowFunc {
    /// Function name for plan/canonical rendering (aggregate style for the
    /// Agg variant, e.g. `SUM(total)`).
    fn render_name(&self) -> String {
        match self {
            WindowFunc::RowNumber => "ROW_NUMBER".to_string(),
            WindowFunc::Rank => "RANK".to_string(),
            WindowFunc::DenseRank => "DENSE_RANK".to_string(),
            WindowFunc::Agg { func, arg } => format!(
                "{}({})",
                match func {
                    AggFunc::Count => "COUNT",
                    AggFunc::Sum => "SUM",
                    AggFunc::Avg => "AVG",
                    AggFunc::Min => "MIN",
                    AggFunc::Max => "MAX",
                },
                arg.as_deref().unwrap_or("*")
            ),
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
    /// True for ranking functions (their value never depends on the frame).
    fn is_ranking(&self) -> bool {
        !matches!(self, WindowFunc::Agg { .. })
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
/// nested subqueries, or any mix of both).
const MAX_PARSE_DEPTH: usize = 128;

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
            // v4.2: full ALTER TABLE surface — ADD / DROP / RENAME /
            // MODIFY / CHANGE COLUMN (MySQL-compatible spellings; the
            // COLUMN keyword is optional exactly like in MySQL).
            let action = if self.kw("ADD") {
                let _ = self.kw("COLUMN");
                let column = self.ident()?;
                let col_type = self.parse_col_type(&column)?;
                AlterAction::AddColumn { column, col_type }
            } else if self.kw("DROP") {
                let _ = self.kw("COLUMN");
                AlterAction::DropColumn {
                    column: self.ident()?,
                }
            } else if self.kw("RENAME") {
                if self.kw("COLUMN") {
                    let old = self.ident()?;
                    self.expect_kw("TO")?;
                    AlterAction::RenameColumn { old, new: self.ident()? }
                } else {
                    // v4.2.1: RENAME TO t2 — table rename.
                    self.expect_kw("TO")?;
                    AlterAction::RenameTable { to: self.ident()? }
                }
            } else if self.kw("MODIFY") {
                let _ = self.kw("COLUMN");
                let column = self.ident()?;
                let col_type = self.parse_col_type(&column)?;
                AlterAction::ModifyColumn { column, col_type }
            } else if self.kw("CHANGE") {
                let _ = self.kw("COLUMN");
                let old = self.ident()?;
                let new = self.ident()?;
                let col_type = self.parse_col_type(&new)?;
                AlterAction::ChangeColumn { old, new, col_type }
            } else {
                return Err(
                    "SQL: unsupported ALTER TABLE action (supported: ADD / DROP / RENAME / MODIFY / CHANGE COLUMN)"
                        .into(),
                );
            };
            self.eat_semi();
            Ok(Stmt::AlterTable { table, action })
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

    /// Parse the column-type tail of a DDL item (v4.2 helper shared by
    /// every `ALTER TABLE` action): INT | INTEGER | INT64 | FLOAT |
    /// FLOAT64 | DOUBLE | TEXT | VARCHAR | STRING | DNA | DNA2BIT |
    /// TIMESTAMP. A trailing `NOT NULL` is accepted but not enforced,
    /// mirroring CREATE TABLE.
    fn parse_col_type(&mut self, column: &str) -> Result<ColumnType, String> {
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
            return Err(format!("SQL: unknown column type for '{}'", column));
        };
        let _ = self.kw("NOT") && self.kw("NULL"); // accepted, not enforced
        Ok(ty)
    }

    /// `WITH name AS (SELECT ...) [, name2 AS (...)] body` (v3.6).
    fn parse_cte(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        // v4.2: `WITH RECURSIVE name AS (base UNION [ALL] recursive) body`.
        let recursive = self.kw("RECURSIVE");
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
            recursive,
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
        self.maybe_alias();
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
        let (group_by, grouping) = if self.kw("GROUP") {
            self.expect_kw("BY")?;
            // v4.2: set-based grouping — `GROUP BY ROLLUP (a, b)` /
            // `GROUP BY CUBE (a, b)` (standard SQL spellings).
            let set_head = matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("ROLLUP") || s.eq_ignore_ascii_case("CUBE"));
            if set_head && matches!(self.peek_at(1), STok::LParen) {
                let cube = matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("CUBE"));
                self.next();
                self.expect(STok::LParen)?;
                let mut cols = Vec::new();
                loop {
                    cols.push(self.ident()?);
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
                self.expect(STok::RParen)?;
                let sets = if cube {
                    cube_sets(&cols)?
                } else {
                    rollup_sets(&cols)
                };
                (cols, sets)
            } else {
                let mut cols = Vec::new();
                loop {
                    cols.push(self.ident()?);
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
                // v4.2: MySQL spelling `GROUP BY a, b WITH ROLLUP`.
                let grouping = if self.kw("WITH") {
                    self.expect_kw("ROLLUP")?;
                    rollup_sets(&cols)
                } else {
                    Vec::new()
                };
                (cols, grouping)
            }
        } else {
            (Vec::new(), Vec::new())
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
            grouping,
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
        // v3.6: window functions `ROW_NUMBER()/RANK()/DENSE_RANK() OVER (...)`;
        // v4.2 also accepts aggregates as window functions (`SUM(x) OVER
        // (...)`) — an aggregate call followed by OVER is a window, anything
        // else stays a plain aggregate (the parser backtracks to try it).
        if let STok::Ident(s) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::LParen) {
                if WindowFunc::from_name(&s).is_some() {
                    let func = WindowFunc::from_name(&s).unwrap();
                    self.next();
                    self.next(); // (
                    self.expect(STok::RParen)?;
                    self.expect_kw("OVER")?;
                    let (partition, order, frame) = self.parse_over_spec()?;
                    let alias = self.maybe_as_alias()?;
                    let canonical = render_window(&func, &partition, &order, frame);
                    let rendered = alias.unwrap_or_else(|| canonical.clone());
                    return Ok(SelectItem::Window {
                        func,
                        partition,
                        order,
                        frame,
                        canonical,
                        rendered,
                    });
                }
                if AggFunc::from_name(&s).is_ok() {
                    let save = self.pos;
                    let func = AggFunc::from_name(&s).unwrap();
                    self.next();
                    self.next(); // (
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        let first = self.ident()?;
                        // v4.2: nested aggregate `SUM(SUM(x)) OVER (...)` —
                        // the inner aggregate names the canonical key that
                        // the grouped rows carry when windows run over a
                        // GROUP BY result (standard SQL spelling).
                        if matches!(self.peek(), STok::LParen) {
                            if AggFunc::from_name(&first).is_ok() {
                                self.next(); // (
                                let inner = if matches!(self.peek(), STok::Star) {
                                    self.next();
                                    None
                                } else {
                                    Some(self.ident()?)
                                };
                                self.expect(STok::RParen)?;
                                Some(render_agg(&first, inner.as_deref()))
                            } else {
                                return Err(format!(
                                    "SQL: unsupported window argument '{}(...)' (column or aggregate expected)",
                                    first
                                ));
                            }
                        } else {
                            Some(first)
                        }
                    };
                    self.expect(STok::RParen)?;
                    if self.kw("OVER") {
                        let (partition, order, frame) = self.parse_over_spec()?;
                        let wfunc = WindowFunc::Agg { func, arg };
                        let alias = self.maybe_as_alias()?;
                        let canonical = render_window(&wfunc, &partition, &order, frame);
                        let rendered = alias.unwrap_or_else(|| canonical.clone());
                        return Ok(SelectItem::Window {
                            func: wfunc,
                            partition,
                            order,
                            frame,
                            canonical,
                            rendered,
                        });
                    }
                    // Not a window: rewind and let the aggregate branch parse it.
                    self.pos = save;
                }
            }
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

    /// v4.2: the body of an `OVER (...)` clause: PARTITION BY refs, ORDER
    /// BY refs and an optional ROWS/RANGE frame.
    fn parse_over_spec(
        &mut self,
    ) -> Result<(Vec<String>, Vec<(String, bool)>, Option<WindowFrame>), String> {
        self.expect(STok::LParen)?;
        let mut partition = Vec::new();
        if self.kw("PARTITION") {
            self.expect_kw("BY")?;
            loop {
                partition.push(self.parse_window_ref()?);
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
                let col = self.parse_window_ref()?;
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
        let frame = if self.kw("ROWS") {
            Some(self.parse_frame(false)?)
        } else if self.kw("RANGE") {
            Some(self.parse_frame(true)?)
        } else {
            None
        };
        self.expect(STok::RParen)?;
        Ok((partition, order, frame))
    }

    /// One reference inside PARTITION BY / ORDER BY of a window: a plain
    /// column, or an aggregate call rendered canonically — `OVER (ORDER BY
    /// SUM(amount))` references the aggregate key that the grouped row
    /// carries when windows run over a GROUP BY result.
    fn parse_window_ref(&mut self) -> Result<String, String> {
        let name = self.ident()?;
        if matches!(self.peek(), STok::LParen) {
            if AggFunc::from_name(&name).is_ok() {
                self.next(); // (
                let arg = if matches!(self.peek(), STok::Star) {
                    self.next();
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(STok::RParen)?;
                return Ok(render_agg(&name, arg.as_deref()));
            }
            return Err(format!(
                "SQL: unsupported OVER reference '{}(...)' (column or aggregate expected)",
                name
            ));
        }
        Ok(name)
    }

    /// v4.2: a ROWS/RANGE frame after the unit keyword was consumed.
    /// `RANGE` accepts only UNBOUNDED PRECEDING / CURRENT ROW /
    /// UNBOUNDED FOLLOWING bounds (value-offset RANGE frames are a
    /// documented gap); `ROWS` accepts the full grammar.
    fn parse_frame(&mut self, is_range: bool) -> Result<WindowFrame, String> {
        let (start, end) = if self.kw("BETWEEN") {
            let s = self.parse_frame_bound()?;
            self.expect_kw("AND")?;
            let e = self.parse_frame_bound()?;
            (s, e)
        } else {
            // Shorthand `ROWS <bound>` == BETWEEN UNBOUNDED PRECEDING AND <bound>.
            (FrameBound::UnboundedPreceding, self.parse_frame_bound()?)
        };
        if is_range {
            let ok = matches!(
                start,
                FrameBound::UnboundedPreceding | FrameBound::CurrentRow
            ) && matches!(
                end,
                FrameBound::UnboundedFollowing | FrameBound::CurrentRow
            );
            if !ok {
                return Err(
                    "SQL: RANGE frames support only UNBOUNDED PRECEDING, CURRENT ROW and UNBOUNDED FOLLOWING bounds (use ROWS for value offsets)"
                        .into(),
                );
            }
        }
        // Frame sanity: start must not lie after end.
        let bad = matches!(
            (start, end),
            (FrameBound::CurrentRow, FrameBound::Preceding(_))
                | (FrameBound::Following(_), FrameBound::Preceding(_))
                | (FrameBound::Following(_), FrameBound::CurrentRow)
                | (FrameBound::UnboundedFollowing, _)
        ) || match (start, end) {
            (FrameBound::Preceding(a), FrameBound::Preceding(b)) => a < b,
            (FrameBound::Following(a), FrameBound::Following(b)) => a > b,
            _ => false,
        };
        if bad {
            return Err("SQL: window frame start must not come after its end".into());
        }
        Ok(WindowFrame { start, end })
    }

    /// One frame bound: UNBOUNDED PRECEDING/FOLLOWING, n PRECEDING/FOLLOWING
    /// (n >= 0) or CURRENT ROW.
    fn parse_frame_bound(&mut self) -> Result<FrameBound, String> {
        if self.kw("UNBOUNDED") {
            if self.kw("PRECEDING") {
                return Ok(FrameBound::UnboundedPreceding);
            }
            if self.kw("FOLLOWING") {
                return Ok(FrameBound::UnboundedFollowing);
            }
            return Err("SQL: expected PRECEDING or FOLLOWING after UNBOUNDED".into());
        }
        if self.kw("CURRENT") {
            self.expect_kw("ROW")?;
            return Ok(FrameBound::CurrentRow);
        }
        if let STok::Int(n) = self.peek().clone() {
            self.next();
            if n < 0 {
                return Err("SQL: frame offset must be non-negative".into());
            }
            if self.kw("PRECEDING") {
                return Ok(FrameBound::Preceding(n));
            }
            if self.kw("FOLLOWING") {
                return Ok(FrameBound::Following(n));
            }
            return Err("SQL: expected PRECEDING or FOLLOWING after frame offset".into());
        }
        Err("SQL: expected frame bound (UNBOUNDED PRECEDING/FOLLOWING, n PRECEDING/FOLLOWING, CURRENT ROW)".into())
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
        Stmt::Cte { ctes, body, .. } => {
            for (_, def) in ctes {
                collect_stmt_tables(def, out)?;
            }
            collect_stmt_tables(body, out)?;
        }
        Stmt::ShowTables | Stmt::Tx { .. } => {}
        Stmt::ShowStorage { table, .. } => out.push(table.clone()),
        Stmt::AlterTable { table, .. } => out.push(table.clone()),
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
                    SelectItem::Star => {}
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
        Stmt::Cte { ctes, body, .. } => {
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
                joins,
                where_clause,
                group_by,
                grouping,
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
                &grouping,
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
            Stmt::Cte { ctes, recursive, body } => {
                self.exec_cte(ctes, recursive, *body, tx)
            }
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
            Stmt::AlterTable { table, action } => self.exec_alter_table(&table, action),
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
        recursive: bool,
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
            let rows = if recursive {
                self.exec_recursive_cte(name, def, &mut plan)?
            } else {
                let (_, rows, _cplan, _) = self.exec_stmt(def.clone(), &mut None)?;
                rows
            };
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

    /// v4.2: one iterative `WITH RECURSIVE` CTE. The definition must be
    /// `base UNION [ALL] recursive` where the recursive side references the
    /// CTE name. The base term runs once; each round publishes ONLY the
    /// previous round's rows under the CTE name (standard delta semantics),
    /// runs the recursive side, and appends the fresh rows — `UNION`
    /// dedupes full rows across all rounds, `UNION ALL` keeps everything.
    /// Iteration stops on an empty round; hard caps guard runaway
    /// recursion (clear error instead of unbounded memory).
    fn exec_recursive_cte(
        &self,
        name: &str,
        def: &Stmt,
        plan: &mut String,
    ) -> Result<Vec<BTreeMap<String, Value>>, String> {
        const MAX_RECURSIVE_ITERATIONS: usize = 10_000;
        const MAX_RECURSIVE_ROWS: usize = 1_000_000;
        let Stmt::Union {
            left,
            right,
            all,
            ..
        } = def
        else {
            return Err(format!(
                "SQL: recursive CTE '{}' must be `WITH RECURSIVE name AS (base UNION [ALL] recursive)`",
                name
            ));
        };
        let fingerprint = |row: &BTreeMap<String, Value>| -> Vec<u8> {
            crate::relational::RowCodec::encode(
                &row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(),
            )
        };
        // Base term: evaluated once, must not reference the CTE itself (the
        // store is empty at this point, so a self-reference is a clean
        // "relation does not exist" error from the engine).
        let (_, base_rows, _, _) = self.exec_stmt((**left).clone(), &mut None)?;
        let mut acc: Vec<BTreeMap<String, Value>> = Vec::with_capacity(base_rows.len());
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for row in base_rows {
            if !*all {
                let fp = fingerprint(&row);
                if !seen.insert(fp) {
                    continue;
                }
            }
            acc.push(row);
        }
        let mut delta = acc.clone();
        let mut iterations = 0usize;
        loop {
            iterations += 1;
            if iterations > MAX_RECURSIVE_ITERATIONS {
                self.cte_store.lock().unwrap().remove(name);
                return Err(format!(
                    "SQL: recursive CTE '{}' exceeded {} iterations (cyclic recursion?)",
                    name, MAX_RECURSIVE_ITERATIONS
                ));
            }
            // Publish ONLY the previous round's rows for this round.
            {
                let mut store = self.cte_store.lock().unwrap();
                store.insert(name.to_string(), std::mem::take(&mut delta));
            }
            let (_, new_rows, _, _) = self.exec_stmt((**right).clone(), &mut None)?;
            delta = Vec::new();
            for row in new_rows {
                if !*all {
                    let fp = fingerprint(&row);
                    if !seen.contains(&fp) {
                        seen.insert(fp);
                        delta.push(row);
                    }
                } else {
                    delta.push(row);
                }
            }
            if delta.is_empty() {
                break;
            }
            if acc.len() + delta.len() > MAX_RECURSIVE_ROWS {
                self.cte_store.lock().unwrap().remove(name);
                return Err(format!(
                    "SQL: recursive CTE '{}' produced more than {} rows",
                    name, MAX_RECURSIVE_ROWS
                ));
            }
            acc.extend(delta.iter().cloned());
        }
        // Leave the store clean: the caller publishes the full result set.
        self.cte_store.lock().unwrap().remove(name);
        Ok(acc)
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
        // and `SELECT status` must not produce mixed column keys). The
        // mapping goes by NAME (field name -> its projection position ->
        // left name), not by iteration order: BTreeMap rows iterate in
        // alphabetical key order, which does not necessarily match the
        // projection order of either side.
        let rename: std::collections::HashMap<&str, &String> = rcols
            .iter()
            .zip(lcols.iter())
            .map(|(r, l)| (r.as_str(), l))
            .collect();
        let renamed = rrows
            .into_iter()
            .map(|row| {
                let mut out = std::collections::BTreeMap::new();
                for (k, v) in row {
                    let name = rename.get(k.as_str()).cloned().unwrap_or(&k);
                    out.insert(name.clone(), v);
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
                joins,
                where_clause,
                group_by,
                grouping,
                having,
                order_by,
                limit,
                offset,
            } => self.exec_select(
                projection,
                from,
                joins,
                where_clause.clone(),
                group_by,
                grouping,
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
            Stmt::Cte { ctes, recursive, body } => {
                let mut none = None;
                self.exec_cte(ctes.clone(), *recursive, (**body).clone(), &mut none)
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
                joins,
                where_clause,
                group_by,
                grouping,
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
                    &grouping,
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
            Stmt::AlterTable { table, action } => {
                let detail = match action {
                    AlterAction::AddColumn { column, .. } => {
                        format!("ADD COLUMN {})", column)
                    }
                    AlterAction::DropColumn { column } => format!("DROP COLUMN {})", column),
                    AlterAction::RenameColumn { old, new } => {
                        format!("RENAME COLUMN {} TO {})", old, new)
                    }
                    AlterAction::ModifyColumn { column, .. } => {
                        format!("MODIFY COLUMN {})", column)
                    }
                    AlterAction::ChangeColumn { old, new, .. } => {
                        format!("CHANGE COLUMN {} TO {})", old, new)
                    }
                    AlterAction::RenameTable { to } => {
                        format!("RENAME TO {})", to)
                    }
                };
                format!("AlterTable({} {}", table, detail)
            }
            Stmt::Tx { action } => format!("Tx({:?})", action),
            Stmt::Cte { ctes, recursive, body } => format!(
                "{}({}) {}",
                if *recursive { "RecursiveCTE" } else { "CTE" },
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
        // v4.2: fields of DROP COLUMN-ed columns that still linger inside
        // old row payloads are invisible to every read path (WHERE, joins,
        // ORDER BY): rows are projected onto the live schema right here.
        let live: Option<std::collections::HashSet<String>> =
            self.get_schema(table).map(|s| s.columns.keys().cloned().collect());
        let mut out = Vec::new();
        for (key, payload) in raw {
            // skip the schema meta-row
            if key.ends_with(b":schema") || String::from_utf8_lossy(&key).ends_with(":schema") {
                continue;
            }
            let fields = RowCodec::decode(&payload)
                .map_err(|e| format!("row {} unreadable: {}", String::from_utf8_lossy(&key), e))?;
            match &live {
                Some(cols) => out.push(
                    fields
                        .into_iter()
                        .filter(|(k, _)| cols.contains(k))
                        .collect(),
                ),
                None => out.push(fields.into_iter().collect()),
            }
        }
        Ok(out)
    }

    /// v4.2: index nested-loop join — for every left row, probe the right
    /// table's secondary index (or the primary key) with the join value and
    /// merge matches exactly like `hash_join` (conflicting right columns are
    /// renamed `joined_<col>`; NULL keys never match). LEFT joins keep
    /// unmatched left rows padded with NULL right columns, mirroring
    /// `hash_join_left`.
    fn index_join(
        &self,
        left_rows: &[BTreeMap<String, Value>],
        j: &JoinSpec,
        table: &str,
        right_col: &str,
    ) -> Result<Vec<BTreeMap<String, Value>>, String> {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        let is_pk = right_col == schema.primary_locus_column;
        if !is_pk && schema.secondary_index_on(right_col).is_none() {
            return Err(format!(
                "SQL: IndexJoin requires an index on '{}.{}'",
                table, right_col
            ));
        }
        let right_cols: Vec<String> = schema.columns.keys().cloned().collect();
        let mut out: Vec<BTreeMap<String, Value>> = Vec::new();
        for l in left_rows {
            let key = l.get(&j.left_col).cloned().unwrap_or(Value::Null);
            let mut matched: Vec<BTreeMap<String, Value>> = Vec::new();
            if !key.is_null() {
                if is_pk {
                    if let Some(row) = self.read_row_point(table, &key)? {
                        matched.push(row);
                    }
                } else {
                    let mut prefix = index_prefix(table, right_col);
                    prefix.extend_from_slice(&escape_index_part(&encode_ordered_value(&key)));
                    let entries = self
                        .engine
                        .scan_prefix(&prefix)
                        .map_err(|e| e.to_string())?;
                    for (_, payload) in entries {
                        if let Ok(holder) = String::from_utf8(payload) {
                            if let Some(row) = self.read_row_by_key(&holder)? {
                                matched.push(row);
                            }
                        }
                    }
                }
            }
            if matched.is_empty() {
                if j.join_type == JoinType::Left {
                    let mut row = l.clone();
                    for c in &right_cols {
                        if !row.contains_key(c) {
                            row.insert(c.clone(), Value::Null);
                        }
                    }
                    out.push(row);
                }
                continue;
            }
            for r in matched {
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
                // v4.2: hide fields of DROP COLUMN-ed columns (same
                // visibility rule as read_table_rows).
                let live: std::collections::HashSet<String> = self
                    .get_schema(table)
                    .map(|s| s.columns.keys().cloned().collect())
                    .unwrap_or_default();
                Ok(Some(
                    fields
                        .into_iter()
                        .filter(|(k, _)| live.is_empty() || live.contains(k))
                        .collect(),
                ))
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

    /// v4.2 dispatch of `ALTER TABLE <table> <action>`.
    fn exec_alter_table(
        &self,
        table: &str,
        action: AlterAction,
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
            AlterAction::AddColumn { column, col_type } => {
                self.exec_add_column(table, &column, col_type)
            }
            AlterAction::DropColumn { column } => self.exec_drop_column(table, &column),
            AlterAction::RenameColumn { old, new } => {
                self.exec_rename_column(table, &old, &new)
            }
            AlterAction::ModifyColumn { column, col_type } => {
                self.exec_alter_column_type(table, &column, None, col_type)
            }
            AlterAction::ChangeColumn { old, new, col_type } => {
                self.exec_alter_column_type(table, &old, Some(&new), col_type)
            }
            AlterAction::RenameTable { to } => self.exec_rename_table(table, &to),
        }
    }

    /// v4.2.1 Online DDL: `ALTER TABLE t RENAME TO t2`. One WAL batch moves
    /// the schema row, every `<t>:<pk>` row key and every `__idx:t:*`
    /// prefix — index entry keys AND payloads are rewritten to point at the
    /// new name (a payload still naming the old table would leave every
    /// moved entry dangling). Foreign keys in other tables that target `t`
    /// are re-pointed to `t2` and the complementarity (FK) index is
    /// re-keyed.
    fn exec_rename_table(
        &self,
        table: &str,
        to: &str,
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
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        if self.get_schema(to).is_some() {
            return Err(format!("SQL: relation '{}' already exists", to));
        }
        if self.cte_contains(to) {
            return Err(format!("SQL: relation name '{}' conflicts with an active CTE", to));
        }
        // Re-point FK references in other schemas.
        let mut referrers: Vec<ChromosomeTable> = Vec::new();
        {
            let schemas = self.schemas.read();
            for (name, s) in schemas.iter() {
                if name == table {
                    continue;
                }
                let mut changed = false;
                let mut s2 = s.clone();
                for fk in s2.foreign_keys.iter_mut() {
                    if fk.target_chromosome == *table {
                        fk.target_chromosome = to.to_string();
                        changed = true;
                    }
                }
                if changed {
                    referrers.push(s2);
                }
            }
        }
        // Move schema + rows + index entries in one batch.
        let mut batch = Vec::new();
        let payload = serde_json::to_vec(&schema).map_err(|e| e.to_string())?;
        batch.push((
            crate::wal::WalOp::Delete,
            format!("__schema:{}", table).into_bytes(),
            None,
        ));
        batch.push((
            crate::wal::WalOp::Put,
            format!("__schema:{}", to).into_bytes(),
            Some(payload),
        ));
        let old_row_prefix = format!("{}:", table).into_bytes();
        let new_row_prefix = format!("{}:", to).into_bytes();
        let old_idx_prefix = format!("__idx:{}:", table).into_bytes();
        let new_idx_prefix = format!("__idx:{}:", to).into_bytes();
        let mut moved_rows = 0usize;
        for (key, val) in self
            .engine
            .scan_prefix(&old_row_prefix)
            .map_err(|e| e.to_string())?
        {
            batch.push((crate::wal::WalOp::Delete, key.clone(), None));
            let mut new_key = new_row_prefix.clone();
            new_key.extend_from_slice(&key[old_row_prefix.len()..]);
            batch.push((crate::wal::WalOp::Put, new_key, Some(val)));
            moved_rows += 1;
        }
        let mut moved_idx = 0usize;
        for (key, val) in self
            .engine
            .scan_prefix(&old_idx_prefix)
            .map_err(|e| e.to_string())?
        {
            batch.push((crate::wal::WalOp::Delete, key.clone(), None));
            let old_row_key = String::from_utf8_lossy(&val).to_string();
            let new_row_key = format!(
                "{}{}",
                String::from_utf8_lossy(&new_row_prefix),
                &old_row_key[old_row_prefix.len().min(old_row_key.len())..]
            );
            let mut new_key = new_idx_prefix.clone();
            let tail_len = old_row_key.len();
            let keep_up_to = key.len().saturating_sub(tail_len);
            new_key.extend_from_slice(&key[old_idx_prefix.len()..keep_up_to]);
            new_key.extend_from_slice(new_row_key.as_bytes());
            batch.push((
                crate::wal::WalOp::Put,
                new_key,
                Some(new_row_key.into_bytes()),
            ));
            moved_idx += 1;
        }
        if !batch.is_empty() {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("ALTER TABLE RENAME TO failed: {}", e))?;
        }
        for s in &referrers {
            self.persist_schema(s)?;
        }
        {
            let mut schemas = self.schemas.write();
            schemas.remove(table);
            schemas.insert(to.to_string(), schema.clone());
        }
        {
            let mut index = self.complement_index.write();
            for fk in &schema.foreign_keys {
                index.register_column(to, &fk.column);
            }
            for row in self.read_table_rows(to)? {
                let Ok(pk) = Self::pk_of(&schema, &row) else {
                    continue;
                };
                let key = Self::row_key(to, &pk);
                for fk in &schema.foreign_keys {
                    if let Some(value) = row.get(&fk.column) {
                        if !value.is_null() {
                            index.remove(table, &fk.column, &key, value);
                            index.upsert(to, &fk.column, &key, value);
                        }
                    }
                }
            }
        }
        Ok((
            vec!["status".into(), "from".into(), "to".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("from", Value::Text(table.to_string())),
                ("to", Value::Text(to.to_string())),
            ])],
            format!(
                "AlterTable({} RENAME TO {}, {} rows, {} index entries moved)",
                table, to, moved_rows, moved_idx
            ),
            Some(format!(
                "table '{}' renamed to '{}' ({} rows, {} index entries, {} referrers updated)",
                table,
                to,
                moved_rows,
                moved_idx,
                referrers.len()
            )),
        ))
    }

    /// v3.9 Online DDL: `ALTER TABLE t ADD COLUMN c T` — metadata-only and
    /// instant: the schema row is rewritten (one WAL batch), no existing
    /// row is touched, reads of the new column on old rows yield NULL.
    fn exec_add_column(
        &self,
        table: &str,
        column: &str,
        col_type: ColumnType,
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

    /// v4.2: `ALTER TABLE t DROP COLUMN c` — one atomic WAL batch rewrites
    /// the schema and every row payload without the dropped field, so the
    /// column can never resurrect (re-adding it starts empty). Secondary
    /// indexes on the column are swept in the same batch.
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
                "SQL: unknown column '{}' in table '{}'",
                column, table
            ));
        }
        if column == schema.primary_locus_column {
            return Err(format!(
                "SQL: cannot DROP COLUMN '{}': it is the primary key of '{}'",
                column, table
            ));
        }
        if schema.foreign_keys.iter().any(|f| f.column == column) {
            return Err(format!(
                "SQL: cannot DROP COLUMN '{}': a foreign key is declared on it; drop the constraint first",
                column
            ));
        }
        if self.foreign_key_targets(table, column)? {
            return Err(format!(
                "SQL: cannot DROP COLUMN '{}': a foreign key in another table references {}.({})",
                column, table, column
            ));
        }
        let start = std::time::Instant::now();
        // One atomic batch: rewritten rows (without the dropped field) +
        // index sweep + new schema row. read_table_rows already projects
        // onto the live schema, so writing its rows back drops the stale
        // field physically.
        let rows = self.read_table_rows(table)?;
        let had_index = schema.secondary_index_on(column).is_some();
        schema.columns.remove(column);
        schema.secondary_indexes.retain(|i| i.column != column);
        let mut batch: Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)> =
            Vec::with_capacity(rows.len() + 8);
        if had_index {
            let prefix = index_prefix(table, column);
            let entries = self
                .engine
                .scan_prefix(&prefix)
                .map_err(|e| e.to_string())?;
            for (key, _) in entries {
                batch.push((crate::wal::WalOp::Delete, key, None));
            }
        }
        let rows_count = rows.len();
        for mut row in rows {
            // The rows were read while the column was still live, so the
            // dropped field is removed explicitly here (the atomic batch
            // writes the row payloads without it).
            row.remove(column);
            let pk = Self::pk_of(&schema, &row)?;
            batch.push((
                crate::wal::WalOp::Put,
                Self::row_key(table, &pk).into_bytes(),
                Some(crate::relational::RowCodec::encode(
                    &row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(),
                )),
            ));
        }
        let payload = serde_json::to_vec(&schema).map_err(|e| e.to_string())?;
        batch.push((
            crate::wal::WalOp::Put,
            format!("__schema:{}", table).into_bytes(),
            Some(payload),
        ));
        self.engine.write_batch(&batch).map_err(|e| e.to_string())?;
        self.schemas
            .write()
            .insert(table.to_string(), schema.clone());
        let us = start.elapsed().as_micros() as u64;
        Ok((
            vec!["status".into(), "table".into(), "column".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("table", Value::Text(table.to_string())),
                ("column", Value::Text(column.to_string())),
            ])],
            format!(
                "AlterTable({} DROP COLUMN {}, {} rows rewritten, {} µs, one atomic batch)",
                table,
                column,
                rows_count,
                us
            ),
            Some(format!(
                "column '{}' removed from '{}' atomically (schema + data + indexes)",
                column, table
            )),
        ))
    }

    /// v4.2: `ALTER TABLE t RENAME COLUMN a TO b` — schema metadata plus a
    /// physical rewrite of every row payload carrying the old key, all in
    /// ONE atomic WAL batch (single fsync durability interval). Secondary
    /// indexes on the column and foreign keys referencing it are updated.
    fn exec_rename_column(
        &self,
        table: &str,
        old: &str,
        new: &str,
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
        if old == new {
            return Ok((
                vec!["status".into()],
                vec![mk_row(&[("status", Value::Text("OK".into()))])],
                format!("AlterTable({} RENAME COLUMN {} TO {}) [no-op]", table, old, new),
                None,
            ));
        }
        if !schema.columns.contains_key(old) {
            return Err(format!(
                "SQL: unknown column '{}' in table '{}'",
                old, table
            ));
        }
        if schema.columns.contains_key(new) {
            return Err(format!(
                "SQL: column '{}' already exists in table '{}'",
                new, table
            ));
        }
        let start = std::time::Instant::now();
        // Move the column inside the schema map, keeping the declared type.
        let col_type = schema.columns.remove(old).expect("checked above");
        schema.columns.insert(new.to_string(), col_type);
        if schema.primary_locus_column == old {
            schema.primary_locus_column = new.to_string();
        }
        for idx in &mut schema.secondary_indexes {
            if idx.column == old {
                idx.column = new.to_string();
            }
        }
        for fk in &mut schema.foreign_keys {
            if fk.column == old {
                fk.column = new.to_string();
            }
        }
        // Foreign keys in OTHER tables that reference this column.
        let mut extra_schemas = Vec::new();
        {
            let schemas = self.schemas.read();
            for (other_name, other) in schemas.iter() {
                if other_name == table {
                    continue;
                }
                let mut updated = other.clone();
                let mut changed = false;
                for fk in &mut updated.foreign_keys {
                    if fk.target_chromosome == table && fk.target_locus_column == old {
                        fk.target_locus_column = new.to_string();
                        changed = true;
                    }
                }
                if changed {
                    extra_schemas.push(updated);
                }
            }
        }
        // Rewrite every row payload under the new key name.
        let rows = self.read_table_rows(table)?;
        let mut batch: Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)> =
            Vec::with_capacity(rows.len() + 8);
        let had_index = schema.secondary_index_on(new).is_some();
        if had_index {
            let prefix = index_prefix(table, old);
            let entries = self
                .engine
                .scan_prefix(&prefix)
                .map_err(|e| e.to_string())?;
            for (key, _) in entries {
                batch.push((crate::wal::WalOp::Delete, key, None));
            }
        }
        for row in &rows {
            let mut rewritten = row.clone();
            if let Some(v) = rewritten.remove(old) {
                rewritten.insert(new.to_string(), v);
            }
            let pk = Self::pk_of(&schema, &rewritten)?;
            batch.push((
                crate::wal::WalOp::Put,
                Self::row_key(table, &pk).into_bytes(),
                Some(crate::relational::RowCodec::encode(
                    &rewritten.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(),
                )),
            ));
            if had_index {
                batch.extend(Self::index_delta_batch(
                    &schema,
                    None,
                    Some((table, &rewritten)),
                ));
            }
        }
        for extra in &extra_schemas {
            let payload = serde_json::to_vec(extra).map_err(|e| e.to_string())?;
            batch.push((
                crate::wal::WalOp::Put,
                format!("__schema:{}", extra.name).into_bytes(),
                Some(payload),
            ));
        }
        let payload = serde_json::to_vec(&schema).map_err(|e| e.to_string())?;
        batch.push((
            crate::wal::WalOp::Put,
            format!("__schema:{}", table).into_bytes(),
            Some(payload),
        ));
        self.engine.write_batch(&batch).map_err(|e| e.to_string())?;
        self.schemas
            .write()
            .insert(table.to_string(), schema.clone());
        for extra in extra_schemas {
            self.schemas.write().insert(extra.name.clone(), extra);
        }
        let us = start.elapsed().as_micros() as u64;
        Ok((
            vec!["status".into(), "table".into(), "column".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("table", Value::Text(table.to_string())),
                ("column", Value::Text(new.to_string())),
            ])],
            format!(
                "AlterTable({} RENAME COLUMN {} TO {}, {} rows rewritten, {} µs, one atomic batch)",
                table,
                old,
                new,
                rows.len(),
                us
            ),
            Some(format!(
                "column '{}' renamed to '{}' in '{}' atomically (schema + data + indexes)",
                old, new, table
            )),
        ))
    }

    /// v4.2: `ALTER TABLE t MODIFY COLUMN c T` / `CHANGE COLUMN a b T` —
    /// converts every stored value to the new type and (for CHANGE) renames
    /// the column, all in ONE atomic WAL batch. The whole statement is
    /// prepared in memory first: a single inconvertible value aborts it
    /// before anything is written (no partial conversion, ever). The
    /// primary-key column may keep its type but cannot change it (row keys
    /// are derived from the stored PK value).
    fn exec_alter_column_type(
        &self,
        table: &str,
        column: &str,
        new_name: Option<&str>,
        new_type: ColumnType,
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
        let old_type = schema
            .columns
            .get(column)
            .cloned()
            .ok_or_else(|| format!("SQL: unknown column '{}' in table '{}'", column, table))?;
        let final_name = new_name.unwrap_or(column).to_string();
        if final_name != column && schema.columns.contains_key(&final_name) {
            return Err(format!(
                "SQL: column '{}' already exists in table '{}'",
                final_name, table
            ));
        }
        if matches!(new_type, ColumnType::ForeignLink { .. }) {
            return Err(
                "SQL: ALTER COLUMN to a foreign-key type is not supported; declare FKs in CREATE TABLE"
                    .into(),
            );
        }
        let type_changes = old_type != new_type;
        if type_changes && column == schema.primary_locus_column {
            return Err(format!(
                "SQL: changing the type of the primary-key column '{}' is not supported (drop and recreate the table instead)",
                column
            ));
        }
        let start = std::time::Instant::now();
        let rows = self.read_table_rows(table)?;
        // Convert everything in memory first — atomicity guarantee.
        let mut converted: Vec<BTreeMap<String, Value>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut out = row.clone();
            let pk_hint = row.get(&schema.primary_locus_column).map(|v| v.to_string()).unwrap_or_default();
            let v = out.remove(column).unwrap_or(Value::Null);
            let nv = if type_changes {
                convert_value(&v, &new_type).map_err(|e| {
                    format!("SQL: {} (row pk={})", e, pk_hint)
                })?
            } else {
                v
            };
            out.insert(final_name.clone(), nv);
            converted.push(out);
        }
        // Schema updates.
        if final_name != column {
            let col_ty = schema.columns.remove(column).expect("checked above");
            schema.columns.insert(final_name.clone(), col_ty);
            if schema.primary_locus_column == column {
                schema.primary_locus_column = final_name.clone();
            }
            for idx in &mut schema.secondary_indexes {
                if idx.column == column {
                    idx.column = final_name.clone();
                }
            }
            for fk in &mut schema.foreign_keys {
                if fk.column == column {
                    fk.column = final_name.clone();
                }
            }
        }
        if type_changes {
            schema.columns.insert(final_name.clone(), new_type.clone());
        }
        // Referencing FKs that name the old column of this table.
        let mut extra_schemas = Vec::new();
        if final_name != column {
            let schemas = self.schemas.read();
            for (other_name, other) in schemas.iter() {
                if other_name == table {
                    continue;
                }
                let mut updated = other.clone();
                let mut changed = false;
                for fk in &mut updated.foreign_keys {
                    if fk.target_chromosome == table && fk.target_locus_column == column {
                        fk.target_locus_column = final_name.clone();
                        changed = true;
                    }
                }
                if changed {
                    extra_schemas.push(updated);
                }
            }
        }
        // One atomic batch: rewritten rows, index rebuild, schema rows.
        let mut batch: Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)> =
            Vec::with_capacity(converted.len() * 2 + 8);
        let had_index = schema.secondary_index_on(&final_name).is_some();
        if had_index {
            let prefix = index_prefix(table, column);
            let entries = self
                .engine
                .scan_prefix(&prefix)
                .map_err(|e| e.to_string())?;
            for (key, _) in entries {
                batch.push((crate::wal::WalOp::Delete, key, None));
            }
        }
        for row in &converted {
            let pk = Self::pk_of(&schema, row)?;
            batch.push((
                crate::wal::WalOp::Put,
                Self::row_key(table, &pk).into_bytes(),
                Some(crate::relational::RowCodec::encode(
                    &row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(),
                )),
            ));
            if had_index {
                batch.extend(Self::index_delta_batch(&schema, None, Some((table, row))));
            }
        }
        for extra in &extra_schemas {
            let payload = serde_json::to_vec(extra).map_err(|e| e.to_string())?;
            batch.push((
                crate::wal::WalOp::Put,
                format!("__schema:{}", extra.name).into_bytes(),
                Some(payload),
            ));
        }
        let payload = serde_json::to_vec(&schema).map_err(|e| e.to_string())?;
        batch.push((
            crate::wal::WalOp::Put,
            format!("__schema:{}", table).into_bytes(),
            Some(payload),
        ));
        self.engine.write_batch(&batch).map_err(|e| e.to_string())?;
        self.schemas
            .write()
            .insert(table.to_string(), schema.clone());
        for extra in extra_schemas {
            self.schemas.write().insert(extra.name.clone(), extra);
        }
        let us = start.elapsed().as_micros() as u64;
        let verb = if final_name != column { "CHANGE" } else { "MODIFY" };
        Ok((
            vec!["status".into(), "table".into(), "column".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("table", Value::Text(table.to_string())),
                ("column", Value::Text(final_name.clone())),
            ])],
            format!(
                "AlterTable({} {} COLUMN {} {}, {} rows rewritten, {} µs, one atomic batch)",
                table,
                verb,
                final_name,
                Self::type_word(&new_type),
                converted.len(),
                us
            ),
            Some(format!(
                "column '{}' of '{}' now stores {} atomically",
                final_name, table, Self::type_word(&new_type)
            )),
        ))
    }

    /// Human-readable word for a column type (status rows of ALTER).
    fn type_word(t: &ColumnType) -> &'static str {
        match t {
            ColumnType::Int64 => "INT",
            ColumnType::Float64 => "FLOAT",
            ColumnType::Text => "TEXT",
            ColumnType::Dna2Bit => "DNA",
            ColumnType::Timestamp => "TIMESTAMP",
            ColumnType::ForeignLink { .. } => "FK",
        }
    }

    /// True when any OTHER table declares a foreign key targeting
    /// `table(column)` (used by DROP COLUMN to refuse unsafe drops).
    fn foreign_key_targets(&self, table: &str, column: &str) -> Result<bool, String> {
        let schemas = self.schemas.read();
        Ok(schemas.iter().any(|(name, t)| {
            name != table
                && t.foreign_keys
                    .iter()
                    .any(|f| f.target_chromosome == table && f.target_locus_column == column)
        }))
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
        assignments: &[(String, Value)],
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
            // v4.2: physically drop fields of DROP COLUMN-ed columns when a
            // row is rewritten (metadata-only DDL leaves stale bytes in old
            // payloads; the next rewrite of each row cleans them up).
            let payload = RowCodec::encode(
                &new_row
                    .iter()
                    .filter(|(k, _)| schema.columns.contains_key(k.as_str()))
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
        joins: &[JoinSpec],
        where_clause: Option<Expr>,
        group_by: &[String],
        grouping: &[Vec<String>],
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

        // JOINs (hash join on equi-condition; O(n+m) build+probe). v4.2: an
        // indexed right side switches INNER/LEFT joins to IndexJoin —
        // per-outer-row index probes (or PK point gets) instead of a hash
        // build over the whole right side — whenever the left side is small
        // enough for that to pay off (cost heuristic below). RIGHT/FULL
        // keep the symmetric hash plans.
        for j in joins {
            // v4.2.1 fix (silent empty-join bug): SQL allows BOTH ON
            // orderings (`ON left.col = right.col` and
            // `ON right.col = left.col`). The parser stores the sides
            // positionally, so a query written in the reversed order used to
            // bind the columns to the wrong tables and the join silently
            // produced nothing. Resolve the sides against the joined table's
            // schema: the column that exists there is the right side. When
            // both/neither exist the parsed order stands (identical names —
            // the common no-op case).
            let mut j2 = j.clone();
            let right_schema = self.get_schema(&j.table);
            if let Some(rs) = right_schema.as_ref() {
                let l_in_right = rs.columns.contains_key(&j.left_col);
                let r_in_right = rs.columns.contains_key(&j.right_col);
                if l_in_right && !r_in_right {
                    std::mem::swap(&mut j2.left_col, &mut j2.right_col);
                }
            }
            let j = &j2;
            let right = self.read_table_rows(&j.table)?;
            let indexed = right_schema
                .as_ref()
                .map(|s| {
                    j.right_col == s.primary_locus_column
                        || s.secondary_index_on(&j.right_col).is_some()
                })
                .unwrap_or(false);
            // Cost heuristic: with |left| ≪ |right|, n probes beat building
            // a hash table over every right row. INNER/LEFT only — RIGHT /
            // FULL need the symmetric side, which the index does not serve.
            let use_index_join = indexed
                && matches!(j.join_type, JoinType::Inner | JoinType::Left)
                && current.len() * 8 + 16 < right.len();
            let next = if use_index_join {
                self.index_join(&current, &j, &j.table, &j.right_col)?
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
                " -> {}Join({}.{}, {}.{})",
                if use_index_join { "Index" } else { "Hash" },
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

        // v4.2: window functions are computed AFTER aggregation (standard
        // SQL evaluation order). The projection items are collected here;
        // the actual computation happens once the aggregated row set (when
        // GROUP BY/aggregates are present) or the filtered row set exists.
        let window_items: Vec<&SelectItem> = projection
            .iter()
            .filter(|p| matches!(p, SelectItem::Window { .. }))
            .collect();

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
        let mut agg_items = collect_agg_items(projection, having.as_ref());
        // v4.2: nested aggregates inside aggregate windows over a GROUP BY
        // result (`SUM(SUM(x)) OVER ()`) — the inner SUM(x) must be computed
        // as a group aggregate first. Only meaningful (and legal) when
        // grouping actually happens; nested aggregates without GROUP BY are
        // a hard error, same as PostgreSQL.
        let nested_window_aggs: Vec<String> = projection
            .iter()
            .filter_map(|p| match p {
                SelectItem::Window {
                    func: WindowFunc::Agg { arg: Some(a), .. },
                    ..
                } if parse_rendered_agg(a).is_some() => Some(a.clone()),
                _ => None,
            })
            .collect();
        if !nested_window_aggs.is_empty() {
            if group_by.is_empty() {
                return Err(
                    "SQL: aggregate function calls cannot be nested (SUM(SUM(x)) OVER ... requires GROUP BY)"
                        .into(),
                );
            }
            for warg in nested_window_aggs {
                if let Some((func, arg)) = parse_rendered_agg(&warg) {
                    if !agg_items.iter().any(|(n, _, _, _)| *n == warg) {
                        agg_items.push((warg.clone(), warg.clone(), func, arg));
                    }
                }
            }
        }
        let has_agg = !group_by.is_empty() || !agg_items.is_empty() || having.is_some();
        if !grouping.is_empty() {
            // v4.2: ROLLUP / CUBE / WITH ROLLUP — aggregate once per
            // grouping set and merge; columns outside a set read NULL
            // (the standard super-aggregate marker). Every set runs the
            // same aggregate projections and HAVING as a plain GROUP BY.
            plan.push_str(&format!(
                " -> Aggregate(grouping_sets={}, cols={:?})",
                grouping.len(),
                group_by
            ));
            let base = current;
            let mut merged: Vec<BTreeMap<String, Value>> = Vec::new();
            for set in grouping {
                if set.is_empty() && agg_items.is_empty() {
                    continue; // nothing to report for a bare grand-total row
                }
                let part = self.aggregate_rows(
                    base.clone(),
                    set,
                    &agg_items,
                    having.as_ref(),
                    from,
                    &mut plan,
                )?;
                for mut row in part {
                    for col in group_by {
                        if !set.contains(col) {
                            row.insert(col.clone(), Value::Null);
                        }
                    }
                    merged.push(row);
                }
            }
            current = merged;
        } else if has_agg {
            current = self.aggregate_rows(
                current,
                group_by,
                &agg_items,
                having.as_ref(),
                from,
                &mut plan,
            )?;
        }

        // v4.2: window functions (v3.6 ranking; v4.2 aggregate windows and
        // ROWS/RANGE frames) run over the aggregated rows when GROUP BY /
        // aggregates are present (standard SQL evaluation order), else over
        // the filtered row set. Partition/ORDER BY references must be
        // columns (or aggregate keys) present in that row set.
        if !window_items.is_empty() {
            for item in &window_items {
                if let SelectItem::Window {
                    func,
                    partition,
                    order,
                    frame,
                    canonical,
                    ..
                } = item
                {
                    compute_window(
                        current.as_mut_slice(),
                        func,
                        partition,
                        order,
                        *frame,
                        canonical,
                    );
                    plan.push_str(&format!(" -> Window({})", canonical));
                }
            }
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
                            let out = alias.clone().unwrap_or_else(|| source.clone());
                            specs.push(if out == *source {
                                source.clone()
                            } else {
                                format!("{} AS {}", source, out)
                            });
                            columns.push((source.clone(), out));
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
                // v4.2.1: schema columns of the FROM table — used to
                // NULL-fill physically-missing fields (instant ADD COLUMN /
                // post-ALTER rows read as NULL, the documented v3.9
                // behaviour). Unknown columns keep the legacy skip.
                let schema_cols = self.get_schema(&from).map(|s| s.columns.clone());
                let mut evaluated: Vec<BTreeMap<String, Value>> = Vec::with_capacity(current.len());
                for r in current.into_iter() {
                    let mut out = BTreeMap::new();
                    for (src, out_name) in &columns {
                        let v = match r.get(src) {
                            Some(v) => v.clone(),
                            None => match schema_cols.as_ref() {
                                Some(cols) if cols.contains_key(src) => Value::Null,
                                _ => continue,
                            },
                        };
                        out.insert(out_name.clone(), v);
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
                // v4.2: grouped rows carry window / scalar-sub results under
                // their canonical keys (there is no non-grouped Project pass
                // to rename them) — rename canonical → output alias first.
                let renames: Vec<(String, String)> = projection
                    .iter()
                    .filter_map(|p| match p {
                        SelectItem::Window { canonical, rendered, .. }
                        | SelectItem::ScalarSub { canonical, rendered, .. }
                            if canonical != rendered =>
                        {
                            Some((canonical.clone(), rendered.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                if !renames.is_empty() {
                    for row in current.iter_mut() {
                        for (from_k, to_k) in &renames {
                            if let Some(v) = row.remove(from_k) {
                                row.insert(to_k.clone(), v);
                            }
                        }
                    }
                }
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

        // v4.2: `SELECT *` over a plain table projects exactly the schema
        // columns — this hides fields of DROP COLUMN-ed columns still
        // lingering inside old row payloads. Keys introduced by the
        // statement itself (window canonicals, computed aliases, scalar
        // subqueries) stay; JOIN outputs are left untouched.
        if (projection.is_empty() || projection.iter().any(|p| matches!(p, SelectItem::Star)))
            && joins.is_empty()
            && !from.is_empty()
        {
            if let Some(schema) = self.get_schema(from) {
                let mut keep: std::collections::HashSet<String> =
                    schema.columns.keys().cloned().collect();
                for item in projection {
                    match item {
                        SelectItem::Window { canonical, .. } => {
                            keep.insert(canonical.clone());
                        }
                        SelectItem::Computed { rendered, .. } => {
                            keep.insert(rendered.clone());
                        }
                        SelectItem::ScalarSub { canonical, .. } => {
                            keep.insert(canonical.clone());
                        }
                        _ => {}
                    }
                }
                for row in current.iter_mut() {
                    let stale: Vec<String> = row
                        .keys()
                        .filter(|k| !keep.contains(*k))
                        .cloned()
                        .collect();
                    for k in stale {
                        row.remove(&k);
                    }
                }
            }
        }

        // Column list for tabular consumers.
        let columns: Vec<String> = current
            .first()
            .map(|r| r.keys().cloned().collect())
            .unwrap_or_default();
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

/// v4.2: value conversion for `ALTER TABLE ... MODIFY/CHANGE COLUMN`.
/// Rules (documented in SQL_MANUAL §2.3):
///   * NULL converts to NULL under every target type;
///   * →INT: FLOAT rounds half away from zero (MySQL semantics), TEXT must
///     parse as i64 (otherwise the whole statement aborts), DNA never;
///   * →FLOAT: INT widens, TEXT must parse as f64, DNA never;
///   * →TEXT: every value renders through its Display form (DNA keeps the
///     nucleotide chain);
///   * →DNA: TEXT must be non-empty pure ACGT (same rule as INSERT
///     literals, `typed_string_literal`), DNA passes through;
///   * →TIMESTAMP: values pass through unchanged (the engine never
///     re-renders timestamps).
fn convert_value(v: &Value, to: &ColumnType) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match to {
        ColumnType::Int64 => match v {
            Value::Int(i) => Ok(Value::Int(*i)),
            Value::Float(f) => Ok(Value::Int(f.round() as i64)),
            Value::Text(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| {
                format!(
                    "cannot convert '{}' to INT: not an integer",
                    s
                )
            }),
            Value::Dna(s) => Err(format!("cannot convert DNA '{}' to INT", s)),
            Value::Null => unreachable!("handled above"),
        },
        ColumnType::Float64 => match v {
            Value::Int(i) => Ok(Value::Float(*i as f64)),
            Value::Float(f) => Ok(Value::Float(*f)),
            Value::Text(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| {
                format!("cannot convert '{}' to FLOAT: not a number", s)
            }),
            Value::Dna(s) => Err(format!("cannot convert DNA '{}' to FLOAT", s)),
            Value::Null => unreachable!("handled above"),
        },
        ColumnType::Text => Ok(Value::Text(match v {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Text(s) => s.clone(),
            Value::Dna(s) => s.clone(),
            Value::Null => unreachable!("handled above"),
        })),
        ColumnType::Dna2Bit => match v {
            Value::Dna(s) => Ok(Value::Dna(s.clone())),
            Value::Text(s) => match typed_string_literal(s) {
                Value::Dna(d) => Ok(Value::Dna(d)),
                _ => Err(format!(
                    "cannot convert '{}' to DNA: expected non-empty ACGT sequence",
                    s
                )),
            },
            other => Err(format!(
                "cannot convert '{}' to DNA: expected non-empty ACGT sequence",
                other
            )),
        },
        ColumnType::Timestamp => Ok(v.clone()),
        ColumnType::ForeignLink { .. } => {
            Err("cannot ALTER a column into a foreign-key type".into())
        }
    }
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
        joins,
        where_clause,
        group_by,
        grouping,
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
                SelectItem::ScalarSub { canonical, .. } => canonical.clone(),
                SelectItem::Window { canonical, .. } => canonical.clone(),
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
        // v4.2: re-render the grouping-set form faithfully (rollup chain →
        // WITH ROLLUP; full power set → CUBE; otherwise the ROLLUP spelling).
        if grouping.len() > 1 {
            let is_rollup = grouping.len() == group_by.len() + 1
                && grouping
                    .iter()
                    .enumerate()
                    .all(|(k, set)| *set == group_by[..group_by.len() - k]);
            let is_cube = grouping.len() == 1usize << group_by.len();
            if is_cube {
                out.push_str(&format!(" GROUP BY CUBE ({})", group_by.join(", ")));
            } else if is_rollup {
                out.push_str(&format!(" GROUP BY {} WITH ROLLUP", group_by.join(", ")));
            } else {
                out.push_str(&format!(" GROUP BY ROLLUP ({})", group_by.join(", ")));
            }
        } else {
            out.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
        }
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
                // Same conflict naming as hash_join/hash_join_left: the
                // right side's conflicting columns surface as `joined_<col>`
                // (previously FULL silently overwrote the left values,
                // diverging from the other join kinds).
                let name = if row.contains_key(k) {
                    format!("joined_{}", k)
                } else {
                    k.clone()
                };
                row.insert(name, v.clone());
            }
            out.push(row);
        }
    }
    // Right rows that never matched: keep the right columns, NULL-pad the
    // left side (the single-namespace model unifies both sides — the pad
    // columns come from the LEFT side's column set, mirroring how the left
    // join NULL-pads unmatched left rows from the right column set).
    let left_cols: Vec<String> = left_rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for (i, r) in right_rows.iter().enumerate() {
        if matched_right.contains(&i) {
            continue;
        }
        let mut row = r.clone();
        for c in &left_cols {
            row.entry(c.clone()).or_insert(Value::Null);
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
        Expr::Literal(_) | Expr::Column(_) | Expr::InList { .. } => false,
    }
}

/// Canonical rendering of a window function expression.
fn render_window(
    func: &WindowFunc,
    partition: &[String],
    order: &[(String, bool)],
    frame: Option<WindowFrame>,
) -> String {
    let mut s = format!("{} OVER (", func.render_name());
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
    if let Some(f) = frame {
        if !s.ends_with('(') {
            s.push(' ');
        }
        s.push_str(&render_frame(f));
    }
    s.push(')');
    s
}

/// Canonical rendering of a frame (`ROWS BETWEEN 1 PRECEDING AND CURRENT ROW`).
fn render_frame(f: WindowFrame) -> String {
    let bound = |b: FrameBound| match b {
        FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_string(),
        FrameBound::Preceding(n) => format!("{} PRECEDING", n),
        FrameBound::CurrentRow => "CURRENT ROW".to_string(),
        FrameBound::Following(n) => format!("{} FOLLOWING", n),
        FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_string(),
    };
    format!("ROWS BETWEEN {} AND {}", bound(f.start), bound(f.end))
}

/// v4.2: grouping sets of `ROLLUP (a, b, c)` — full list, then every
/// prefix, ending with the grand-total (empty) set.
fn rollup_sets(cols: &[String]) -> Vec<Vec<String>> {
    (0..=cols.len())
        .rev()
        .map(|k| cols[..k].to_vec())
        .collect()
}

/// v4.2: grouping sets of `CUBE (a, b, ...)` — every subset of the column
/// list (2^n sets), from the full set down to the grand-total. Capped at
/// 12 columns (4096 sets) to keep the explosion honest.
fn cube_sets(cols: &[String]) -> Result<Vec<Vec<String>>, String> {
    if cols.len() > 12 {
        return Err(format!(
            "SQL: CUBE over {} columns is not supported (limit 12)",
            cols.len()
        ));
    }
    let n = cols.len();
    let mut sets = Vec::with_capacity(1 << n);
    for mask in (0u32..(1 << n) as u32).rev() {
        sets.push(
            (0..n)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| cols[i].clone())
                .collect(),
        );
    }
    Ok(sets)
}

/// Render any SELECT-shaped statement back to SQL text (subquery execution).
pub(crate) fn render_stmt_sql(stmt: &Stmt) -> String {
    render_select_sql(stmt)
}

/// Compute a window function over `rows` in place, inserting the result
/// under `canonical`. Rows are grouped by the partition key (encoded
/// values), sorted by the window ORDER BY inside each partition, then
/// evaluated: ROW_NUMBER = position, RANK = position of first peer,
/// DENSE_RANK = consecutive peer groups. v4.2 adds aggregate window
/// functions: the value of `SUM/COUNT/AVG/MIN/MAX OVER (...)` is the
/// aggregate over the frame of the current row — explicit ROWS/RANGE
/// bounds when given, otherwise the standard default (whole partition
/// without ORDER BY; RANGE UNBOUNDED PRECEDING..CURRENT ROW with peers
/// included when ORDER BY is present).
fn compute_window(
    rows: &mut [BTreeMap<String, Value>],
    func: &WindowFunc,
    partition: &[String],
    order: &[(String, bool)],
    frame: Option<WindowFrame>,
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
        if func.is_ranking() {
            let mut rank = 1usize;
            let mut dense = 1usize;
            for (pos, &i) in idxs.iter().enumerate() {
                let value = match func {
                    WindowFunc::RowNumber => (pos + 1) as i64,
                    WindowFunc::Rank => rank as i64,
                    WindowFunc::DenseRank => dense as i64,
                    WindowFunc::Agg { .. } => unreachable!("guarded by is_ranking"),
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
            continue;
        }
        // v4.2: aggregate window function — evaluate the frame per row.
        let WindowFunc::Agg { func: agg, arg } = func else {
            unreachable!("guarded by is_ranking");
        };
        let n = idxs.len();
        // Last peer of every position (RANGE frames include all peers).
        let last_peer: Vec<usize> = {
            let mut out = Vec::with_capacity(n);
            let mut start = 0usize;
            while start < n {
                let mut end = start;
                while end + 1 < n {
                    let a = &rows[idxs[start]];
                    let b = &rows[idxs[end + 1]];
                    let peer = order.iter().all(|(col, _)| {
                        let va = a.get(col).cloned().unwrap_or(Value::Null);
                        let vb = b.get(col).cloned().unwrap_or(Value::Null);
                        va.compare(&vb) == std::cmp::Ordering::Equal
                    });
                    if peer {
                        end += 1;
                    } else {
                        break;
                    }
                }
                for _ in start..=end {
                    out.push(end);
                }
                start = end + 1;
            }
            out
        };
        for (pos, &i) in idxs.iter().enumerate() {
            // Frame range [start..=end] in partition order.
            let (start, end) = match frame {
                None => {
                    if order.is_empty() {
                        (0usize, n - 1)
                    } else {
                        // Default frame: RANGE UNBOUNDED PRECEDING..CURRENT ROW.
                        (0usize, last_peer[pos])
                    }
                }
                Some(f) => {
                    let end = match f.end {
                        FrameBound::UnboundedFollowing => n - 1,
                        FrameBound::CurrentRow => {
                            if f.start == FrameBound::CurrentRow {
                                // RANGE CURRENT ROW includes peers; ROWS
                                // CURRENT ROW is exactly this row.
                                last_peer[pos]
                            } else {
                                pos
                            }
                        }
                        FrameBound::Preceding(k) => pos.saturating_sub(k as usize),
                        FrameBound::Following(k) => (pos + k as usize).min(n - 1),
                        FrameBound::UnboundedPreceding => 0,
                    };
                    let start = match f.start {
                        FrameBound::UnboundedPreceding => 0usize,
                        FrameBound::Preceding(k) => pos.saturating_sub(k as usize),
                        FrameBound::CurrentRow => pos,
                        FrameBound::Following(k) => (pos + k as usize).min(n),
                        FrameBound::UnboundedFollowing => n,
                    };
                    (start, end)
                }
            };
            let value = if end < start {
                // Empty frame: aggregates over nothing are NULL (standard).
                Value::Null
            } else {
                let slice: Vec<&BTreeMap<String, Value>> =
                    idxs[start..=end].iter().map(|&j| &rows[j]).collect();
                eval_window_agg(agg, arg.as_deref(), &slice)
            };
            rows[i].insert(canonical.to_string(), value);
        }
    }
}

/// Evaluate one aggregate (COUNT/SUM/AVG/MIN/MAX) over a frame slice.
/// NULL values are skipped (COUNT(*) counts every row); SUM/COUNT of pure
/// integers stays Int, any Float involvement widens to Float, mirroring
/// the GROUP BY aggregate semantics.
fn eval_window_agg(
    agg: &AggFunc,
    arg: Option<&str>,
    slice: &[&BTreeMap<String, Value>],
) -> Value {
    match agg {
        AggFunc::Count => {
            let c = match arg {
                None => slice.len() as i64,
                Some(col) => slice
                    .iter()
                    .filter(|r| r.get(col).map(|v| !v.is_null()).unwrap_or(false))
                    .count() as i64,
            };
            Value::Int(c)
        }
        AggFunc::Sum => {
            let mut sum = 0.0f64;
            let mut saw_float = false;
            let mut saw_any = false;
            for r in slice {
                if let Some(v) = arg.and_then(|col| r.get(col)) {
                    match v {
                        Value::Int(i) => {
                            sum += *i as f64;
                            saw_any = true;
                        }
                        Value::Float(f) => {
                            sum += *f;
                            saw_float = true;
                            saw_any = true;
                        }
                        _ => {}
                    }
                }
            }
            if !saw_any {
                Value::Null
            } else if saw_float {
                Value::Float(sum)
            } else {
                Value::Int(sum as i64)
            }
        }
        AggFunc::Avg => {
            let mut sum = 0.0f64;
            let mut cnt = 0u64;
            for r in slice {
                if let Some(v) = arg.and_then(|col| r.get(col)) {
                    match v {
                        Value::Int(i) => {
                            sum += *i as f64;
                            cnt += 1;
                        }
                        Value::Float(f) => {
                            sum += *f;
                            cnt += 1;
                        }
                        _ => {}
                    }
                }
            }
            if cnt == 0 {
                Value::Null
            } else {
                Value::Float(sum / cnt as f64)
            }
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<Value> = None;
            for r in slice {
                if let Some(v) = arg.and_then(|col| r.get(col)) {
                    if v.is_null() {
                        continue;
                    }
                    best = Some(match best {
                        None => v.clone(),
                        Some(b) => {
                            let ord = v.compare(&b);
                            let take = if matches!(agg, AggFunc::Min) {
                                ord == std::cmp::Ordering::Less
                            } else {
                                ord == std::cmp::Ordering::Greater
                            };
                            if take {
                                v.clone()
                            } else {
                                b
                            }
                        }
                    });
                }
            }
            best.unwrap_or(Value::Null)
        }
    }
}

#[cfg(test)]
mod tests {

    // ---------------- v4.2.1 fixes ----------------

    #[test]
    fn join_on_accepts_either_side_ordering() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_b (bid INT, aid2 INT, PRIMARY KEY (bid))").unwrap();
        sql.execute("CREATE TABLE chr_a (aid INT, nm TEXT, PRIMARY KEY (aid))").unwrap();
        sql.execute("INSERT INTO chr_b (bid = 1, aid2 = 1)").unwrap();
        sql.execute("INSERT INTO chr_a (aid = 1, nm = 'ada')").unwrap();
        // Reversed ON ordering (right table's column FIRST) — used to bind
        // the sides positionally and silently return zero rows.
        let res = sql
            .execute("SELECT nm FROM chr_b b JOIN chr_a a ON a.aid = b.aid2")
            .unwrap();
        assert_eq!(res.rows, vec![mk_row(&[("nm", Value::Text("ada".into()))])]);
        // Standard ordering still works.
        let res = sql
            .execute("SELECT nm FROM chr_b b JOIN chr_a a ON b.aid2 = a.aid")
            .unwrap();
        assert_eq!(res.rows, vec![mk_row(&[("nm", Value::Text("ada".into()))])]);
        // LEFT JOIN with the reversed ordering keeps the unmatched left row.
        sql.execute("INSERT INTO chr_b (bid = 2, aid2 = 9)").unwrap();
        let res = sql
            .execute("SELECT bid, nm FROM chr_b b LEFT JOIN chr_a a ON a.aid = b.aid2")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        let unmatched = res
            .rows
            .iter()
            .find(|r| r.get("bid") == Some(&Value::Int(2)))
            .unwrap();
        assert!(matches!(unmatched.get("nm"), Some(Value::Null)));
    }

    #[test]
    fn alter_rename_table_moves_schema_rows_and_indexes() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'carl', balance = 5.0)")
            .unwrap();
        sql.execute("CREATE INDEX idx_u ON chr_users (username)").unwrap();
        sql.execute("ALTER TABLE chr_users RENAME TO chr_members").unwrap();
        // The old name is gone; the new one holds the rows.
        assert!(sql.execute("SELECT * FROM chr_users").is_err());
        let res = sql
            .execute("SELECT * FROM chr_members WHERE user_id = 1")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        // The index moved WITH the table — and its entries point at the new
        // name (the payload rewrite is what makes the lookup work).
        let res = sql
            .execute("SELECT user_id FROM chr_members WHERE username = 'carl'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(res.execution_plan.contains("IndexScan"), "plan: {}", res.execution_plan);
        // Rename onto an existing table is rejected.
        assert!(sql.execute("ALTER TABLE chr_orders RENAME TO chr_members").is_err());
        // SHOW TABLES lists the new name exactly once.
        let res = sql.execute("SHOW TABLES").unwrap();
        assert_eq!(
            res.rows
                .iter()
                .filter(|r| r.get("table_name")
                    == Some(&Value::Text("chr_members".into())))
                .count(),
            1
        );
    }

    #[test]
    fn instant_add_column_reads_null_on_old_rows() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 9.0)")
            .unwrap();
        // Instant metadata-only ADD COLUMN: old rows must read the new
        // column as NULL (documented v3.9 behaviour — previously the
        // projection simply skipped the missing field).
        sql.execute("ALTER TABLE chr_users ADD COLUMN note TEXT").unwrap();
        let res = sql
            .execute("SELECT user_id, note FROM chr_users WHERE user_id = 1")
            .unwrap();
        assert_eq!(res.rows[0].get("user_id"), Some(&Value::Int(1)));
        assert_eq!(res.rows[0].get("note"), Some(&Value::Null));
        // SELECT * carries the column too (NULL), and new rows write it.
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 1.0, note = 'hi')")
            .unwrap();
        let res = sql.execute("SELECT note FROM chr_users").unwrap();
        let notes: Vec<&Value> = res.rows.iter().map(|r| r.get("note").unwrap()).collect();
        assert_eq!(notes, vec![&Value::Null, &Value::Text("hi".into())]);
    }
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

    /// Regression: UNION renamed right rows positionally over BTreeMap
    /// iteration order (alphabetical), which drifted values between slots
    /// whenever a right row carried a different key set than the first row
    /// (e.g. after a partial INSERT). The rename now goes by field name.
    #[test]
    fn union_rename_is_by_field_name_not_row_order() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 100.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 10, user_id = 1, total_usd = 5.0, status = 'open')",
        )
        .unwrap();
        // Partial row: no total_usd → the right side's rows are heterogeneous.
        sql.execute("INSERT INTO chr_orders (order_id = 11, user_id = 1, status = 'paid')")
            .unwrap();

        let res = sql
            .execute(
                "SELECT username, balance, user_id FROM chr_users \
                 UNION SELECT status, total_usd, order_id FROM chr_orders",
            )
            .unwrap();
        // Every row of the right side must map consistently (alphabetical
        // engine convention): order_id→balance? no — rcols zip lcols gives
        // order_id→balance, status→user_id, total_usd→username. The
        // regression is that the partial row previously drifted its
        // user_id value into the user_id slot instead of following the
        // same per-name mapping as the full rows.
        let partial = res
            .rows
            .iter()
            .find(|r| r.get("balance") == Some(&Value::Int(11)))
            .expect("partial row must appear with order_id mapped to balance");
        assert_eq!(
            partial.get("user_id"),
            Some(&Value::Text("paid".into())),
            "status must map to user_id consistently: {:?}",
            partial
        );
    }

    /// Regression: FULL OUTER JOIN previously overwrote conflicting columns
    /// (no `joined_` prefix), diverging from INNER/LEFT JOIN naming.
    #[test]
    fn full_join_conflict_columns_use_joined_prefix() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 99, username = 'lonely', balance = 0.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 9.0, status = 'X')",
        )
        .unwrap();

        let res = sql
            .execute("SELECT * FROM chr_users FULL OUTER JOIN chr_orders ON user_id = user_id")
            .unwrap();
        assert_eq!(res.rows.len(), 2, "rows: {:?}", res.rows);
        for row in &res.rows {
            // Matched rows carry the right side's user_id as joined_user_id;
            // the left user_id stays untouched.
            if row.get("username") == Some(&Value::Text("a".into())) {
                assert_eq!(
                    row.get("joined_user_id"),
                    Some(&Value::Int(12)),
                    "matched row must expose the right side's key as joined_user_id: {:?}",
                    row
                );
            }
        }
        // The unmatched left row (user 99) is NULL-extended from the right
        // column set; the unmatched-right branch is unreachable here because
        // FKs forbid orphan orders — the DnaQL parity test covers it.
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

    // ---------------- v4.2: full ALTER TABLE ----------------

    fn get_val(sql: &SqlEngine, q: &str, col: &str) -> Value {
        let res = sql.execute(q).unwrap();
        assert_eq!(res.rows.len(), 1, "expected one row for: {}", q);
        res.rows[0].get(col).cloned().unwrap_or(Value::Null)
    }

    #[test]
    fn alter_table_drop_column_hides_and_sweeps_index() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 10.5)")
            .unwrap();
        sql.execute("CREATE INDEX idx_balance ON chr_users (balance)")
            .unwrap();
        sql.execute("ALTER TABLE chr_users DROP COLUMN balance")
            .unwrap();
        let res = sql.execute("SELECT * FROM chr_users").unwrap();
        assert!(!res.columns.contains(&"balance".to_string()));
        // WHERE over a dropped column matches nothing (unknown column reads
        // NULL and NULL = x is never true — three-valued logic).
        assert_eq!(
            sql.execute("SELECT user_id FROM chr_users WHERE balance = 10.5")
                .map(|r| r.rows.len())
                .unwrap_or(0),
            0
        );
        // The index over the dropped column was swept: the IndexScan helper
        // must not find entries (and re-adding the column yields NULLs).
        sql.execute("ALTER TABLE chr_users ADD COLUMN balance FLOAT")
            .unwrap();
        assert_eq!(
            get_val(&sql, "SELECT balance FROM chr_users WHERE user_id = 1", "balance"),
            Value::Null
        );
        // Stale payload bytes are physically cleaned on the next rewrite.
        sql.execute("UPDATE chr_users SET balance = 7.25 WHERE user_id = 1")
            .unwrap();
        assert_eq!(
            get_val(&sql, "SELECT balance FROM chr_users WHERE user_id = 1", "balance"),
            Value::Float(7.25)
        );
        // Guards: the PK column and FK-carrier columns are refused.
        assert!(sql.execute("ALTER TABLE chr_users DROP COLUMN user_id").is_err());
        assert!(sql.execute("ALTER TABLE chr_orders DROP COLUMN user_id").is_err());
    }

    #[test]
    fn alter_table_rename_column_rewrites_rows_and_indexes() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 10.5)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.5)")
            .unwrap();
        sql.execute("CREATE INDEX idx_name ON chr_users (username)")
            .unwrap();
        sql.execute("ALTER TABLE chr_users RENAME COLUMN username TO login")
            .unwrap();
        // Data survived the physical rewrite under the new key.
        assert_eq!(
            get_val(&sql, "SELECT login FROM chr_users WHERE user_id = 1", "login"),
            Value::Text("ada".into())
        );
        // The old name is gone from the schema: nothing is projected.
        let res = sql.execute("SELECT username FROM chr_users").unwrap();
        assert!(!res.columns.contains(&"username".to_string()));
        assert!(res.rows.iter().all(|r| !r.contains_key("username")));
        // The renamed index still serves equality lookups (IndexScan plan).
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE login = 'bob'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(res.execution_plan.contains("IndexScan"), "plan: {}", res.execution_plan);
        // Renaming onto an existing column is refused; PK rename is allowed
        // (row keys are value-derived, so nothing moves).
        assert!(sql
            .execute("ALTER TABLE chr_users RENAME COLUMN balance TO login")
            .is_err());
        sql.execute("ALTER TABLE chr_users RENAME COLUMN user_id TO uid")
            .unwrap();
        assert_eq!(
            get_val(&sql, "SELECT uid FROM chr_users WHERE login = 'ada'", "uid"),
            Value::Int(1)
        );
    }

    #[test]
    fn alter_table_modify_column_converts_values() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_t (id INT, note TEXT, score FLOAT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("INSERT INTO chr_t (id = 1, note = '42', score = 2.5)")
            .unwrap();
        sql.execute("INSERT INTO chr_t (id = 2, note = '7', score = 2.4)")
            .unwrap();
        // TEXT → INT parses every value; FLOAT → INT rounds half away from
        // zero (MySQL semantics: 2.5 → 3, 2.4 → 2).
        sql.execute("ALTER TABLE chr_t MODIFY COLUMN note INT").unwrap();
        assert_eq!(get_val(&sql, "SELECT note FROM chr_t WHERE id = 1", "note"), Value::Int(42));
        sql.execute("ALTER TABLE chr_t MODIFY COLUMN score INT").unwrap();
        assert_eq!(get_val(&sql, "SELECT score FROM chr_t WHERE id = 1", "score"), Value::Int(3));
        assert_eq!(get_val(&sql, "SELECT score FROM chr_t WHERE id = 2", "score"), Value::Int(2));
        // INT → TEXT renders; TEXT → DNA validates ACGT.
        sql.execute("ALTER TABLE chr_t MODIFY COLUMN note TEXT").unwrap();
        assert_eq!(
            get_val(&sql, "SELECT note FROM chr_t WHERE id = 1", "note"),
            Value::Text("42".into())
        );
        // Atomicity: one inconvertible value aborts the whole statement and
        // leaves the data untouched.
        sql.execute("INSERT INTO chr_t (id = 3, note = 'not-a-number')").unwrap();
        assert!(sql.execute("ALTER TABLE chr_t MODIFY COLUMN note INT").is_err());
        assert_eq!(
            get_val(&sql, "SELECT note FROM chr_t WHERE id = 3", "note"),
            Value::Text("not-a-number".into())
        );
        assert_eq!(
            get_val(&sql, "SELECT note FROM chr_t WHERE id = 1", "note"),
            Value::Text("42".into())
        );
        // CHANGE = rename + conversion in one pass.
        sql.execute("ALTER TABLE chr_t CHANGE COLUMN note memo TEXT").unwrap();
        assert_eq!(
            get_val(&sql, "SELECT memo FROM chr_t WHERE id = 3", "memo"),
            Value::Text("not-a-number".into())
        );
        // PK type change is refused.
        assert!(sql.execute("ALTER TABLE chr_t MODIFY COLUMN id TEXT").is_err());
    }

    // ---------------- v4.2: aggregate windows + frames ----------------

    #[test]
    fn window_aggregates_frames_and_running_totals() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'a', balance = 10.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'b', balance = 20.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 3, username = 'c', balance = 30.0)")
            .unwrap();
        // Default frame with ORDER BY = RANGE UNBOUNDED PRECEDING..CURRENT
        // ROW: running total.
        let res = sql
            .execute(
                "SELECT user_id, SUM(balance) OVER (ORDER BY user_id) AS run \
                 FROM chr_users ORDER BY user_id ASC",
            )
            .unwrap();
        let run: Vec<Value> = res
            .rows
            .iter()
            .map(|r| r.get("run").cloned().unwrap())
            .collect();
        assert_eq!(run, vec![Value::Float(10.0), Value::Float(30.0), Value::Float(60.0)]);
        // Whole-partition frame (no ORDER BY): grand total on every row.
        let res = sql
            .execute(
                "SELECT user_id, SUM(balance) OVER () AS total, \
                 COUNT(*) OVER () AS n FROM chr_users WHERE user_id <= 2 ORDER BY user_id ASC",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("total"), Some(&Value::Float(30.0)));
        assert_eq!(res.rows[1].get("n"), Some(&Value::Int(2)));
        // Explicit ROWS frame: moving sum of the previous and current row.
        let res = sql
            .execute(
                "SELECT user_id, \
                 SUM(balance) OVER (ORDER BY user_id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS mv \
                 FROM chr_users ORDER BY user_id ASC",
            )
            .unwrap();
        let mv: Vec<Value> = res
            .rows
            .iter()
            .map(|r| r.get("mv").cloned().unwrap())
            .collect();
        assert_eq!(mv, vec![Value::Float(10.0), Value::Float(30.0), Value::Float(50.0)]);
        // ROWS 1 FOLLOWING .. UNBOUNDED FOLLOWING skips the current row.
        let res = sql
            .execute(
                "SELECT user_id, \
                 SUM(balance) OVER (ORDER BY user_id ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) AS tail \
                 FROM chr_users ORDER BY user_id ASC",
            )
            .unwrap();
        let tail: Vec<Value> = res
            .rows
            .iter()
            .map(|r| r.get("tail").cloned().unwrap())
            .collect();
        assert_eq!(tail, vec![Value::Float(50.0), Value::Float(30.0), Value::Null]);
        // MIN/MAX/AVG windows and ranking functions still work together.
        let res = sql
            .execute(
                "SELECT MIN(balance) OVER (ORDER BY user_id) AS mn, \
                 ROW_NUMBER() OVER (ORDER BY balance DESC) AS rk \
                 FROM chr_users WHERE user_id <= 2 ORDER BY user_id ASC",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("mn"), Some(&Value::Float(10.0)));
        assert_eq!(res.rows[1].get("rk"), Some(&Value::Int(1)));
        // RANGE beyond the supported bounds is an honest error.
        assert!(sql
            .execute("SELECT SUM(balance) OVER (ORDER BY user_id RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM chr_users")
            .is_err());
    }

    #[test]
    fn window_functions_over_group_by_result() {
        let (_e, sql) = setup();
        for uid in 1..=3 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = 1.0)",
                uid, uid
            ))
            .unwrap();
        }
        for (oid, uid, total) in [(10, 1, 50.0), (11, 1, 70.0), (12, 2, 200.0), (13, 3, 999.0)] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = {}, total_usd = {:.1}, status = 'paid')",
                oid, uid, total
            ))
            .unwrap();
        }
        // Windows run AFTER aggregation: partition/order may reference group
        // columns and aggregate keys; aggregate arguments use the nested
        // spelling (SUM(SUM(x))) to name the grouped aggregate.
        let res = sql
            .execute(
                "SELECT user_id, SUM(total_usd) AS total, \
                 RANK() OVER (ORDER BY SUM(total_usd) DESC) AS rk \
                 FROM chr_orders GROUP BY user_id ORDER BY user_id ASC",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0].get("user_id"), Some(&Value::Int(1)));
        assert_eq!(res.rows[0].get("rk"), Some(&Value::Int(3)));
        assert_eq!(res.rows[2].get("user_id"), Some(&Value::Int(3)));
        assert_eq!(res.rows[2].get("rk"), Some(&Value::Int(1)));
        // Grand total over the grouped rows: SUM(SUM(total_usd)) OVER ()
        // = 50+70+200+999 = 1319 on every group row.
        let res = sql
            .execute(
                "SELECT user_id, SUM(SUM(total_usd)) OVER () AS grand \
                 FROM chr_orders GROUP BY user_id ORDER BY user_id ASC",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("grand"), Some(&Value::Float(1319.0)));
        assert_eq!(res.rows[2].get("grand"), Some(&Value::Float(1319.0)));
    }

    // ---------------- v4.2: ROLLUP / CUBE ----------------

    #[test]
    fn group_by_with_rollup_and_cube() {
        let (_e, sql) = setup();
        for uid in 1..=2 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = 1.0)",
                uid, uid
            ))
            .unwrap();
        }
        for (oid, uid, total, st) in [
            (10, 1, 50.0, "paid"),
            (11, 1, 70.0, "open"),
            (12, 2, 200.0, "paid"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = {}, total_usd = {:.1}, status = '{}')",
                oid, uid, total, st
            ))
            .unwrap();
        }
        // MySQL spelling: GROUP BY a, b WITH ROLLUP — per-user subtotal plus
        // the grand total, rolled-up columns carry NULL.
        let res = sql
            .execute(
                "SELECT user_id, status, SUM(total_usd) AS s \
                 FROM chr_orders GROUP BY user_id, status WITH ROLLUP",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 3 + 2 + 1); // 3 (u,s) + 2 (u,NULL) + 1 grand
        let grand = res
            .rows
            .iter()
            .find(|r| r.get("user_id") == Some(&Value::Null) && r.get("status") == Some(&Value::Null))
            .expect("grand total row");
        assert_eq!(grand.get("s"), Some(&Value::Float(320.0)));
        // Standard spelling GROUP BY ROLLUP(a, b) is the same sets.
        let res2 = sql
            .execute(
                "SELECT user_id, status, SUM(total_usd) AS s \
                 FROM chr_orders GROUP BY ROLLUP (user_id, status)",
            )
            .unwrap();
        assert_eq!(res2.rows.len(), res.rows.len());
        // CUBE(user_id, status) = 4 sets: (u,s), (u), (s), ().
        let res = sql
            .execute(
                "SELECT user_id, status, COUNT(*) AS c \
                 FROM chr_orders GROUP BY CUBE (user_id, status)",
            )
            .unwrap();
        // distinct (u,s) = 3, distinct users = 2, distinct statuses = 2,
        // grand total = 1.
        assert_eq!(res.rows.len(), 3 + 2 + 2 + 1);
    }

    // ---------------- v4.2: WITH RECURSIVE ----------------

    #[test]
    fn with_recursive_walks_hierarchy_and_dedupes_cycles() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_org (id INT, manager_id INT, PRIMARY KEY (id))")
            .unwrap();
        for (id, mgr) in [(1, 0), (2, 1), (3, 2), (4, 2), (5, 3)] {
            sql.execute(&format!(
                "INSERT INTO chr_org (id = {}, manager_id = {})",
                id, mgr
            ))
            .unwrap();
        }
        // Walk from 5 up to the root: 5 → 3 → 2 → 1.
        let res = sql
            .execute(
                "WITH RECURSIVE chain AS ( \
                     SELECT id, manager_id FROM chr_org WHERE id = 5 \
                     UNION ALL \
                     SELECT c.id, c.manager_id FROM chr_org c \
                       INNER JOIN chain ch ON c.id = ch.manager_id \
                 ) SELECT id FROM chain ORDER BY id ASC",
            )
            .unwrap();
        let ids: Vec<Value> = res.rows.iter().map(|r| r.get("id").cloned().unwrap()).collect();
        assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(5)]);
        // Cyclic data + UNION dedupe terminates naturally.
        sql.execute("CREATE TABLE chr_cyc (id INT, nxt INT, PRIMARY KEY (id))")
            .unwrap();
        for (id, nxt) in [(1, 2), (2, 3), (3, 1)] {
            sql.execute(&format!("INSERT INTO chr_cyc (id = {}, nxt = {})", id, nxt))
                .unwrap();
        }
        let res = sql
            .execute(
                "WITH RECURSIVE walk AS ( \
                     SELECT id, nxt FROM chr_cyc WHERE id = 1 \
                     UNION \
                     SELECT c.id, c.nxt FROM chr_cyc c INNER JOIN walk w ON c.id = w.nxt \
                 ) SELECT COUNT(*) AS n FROM walk",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("n"), Some(&Value::Int(3)));
        // UNION ALL over a cycle hits the iteration cap with a clear error
        // (not a hang, not unbounded memory).
        let err = sql
            .execute(
                "WITH RECURSIVE spin AS ( \
                     SELECT id, nxt FROM chr_cyc WHERE id = 1 \
                     UNION ALL \
                     SELECT c.id, c.nxt FROM chr_cyc c INNER JOIN spin s ON c.id = s.nxt \
                 ) SELECT COUNT(*) FROM spin",
            )
            .unwrap_err();
        assert!(err.contains("iterations"), "err: {}", err);
        // Non-UNION recursive bodies are rejected up front.
        assert!(sql
            .execute(
                "WITH RECURSIVE bad AS (SELECT id FROM chr_org WHERE id = 1) SELECT * FROM bad"
            )
            .is_err());
    }

    // ---------------- v4.2: IndexJoin plans ----------------

    #[test]
    fn index_join_plan_for_small_left_side() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'nobody', balance = 2.0)")
            .unwrap();
        // 200 indexed orders, all belonging to user 1.
        for i in 10..210 {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = 1, total_usd = 5.0, status = 'paid')",
                i
            ))
            .unwrap();
        }
        sql.execute("CREATE INDEX idx_orders_user ON chr_orders (user_id)")
            .unwrap();
        // Left side = 2 users, right side = 200 indexed orders → IndexJoin.
        let res = sql
            .execute(
                "SELECT username, order_id FROM chr_users \
                 INNER JOIN chr_orders ON user_id = user_id",
            )
            .unwrap();
        assert!(res.execution_plan.contains("IndexJoin"), "plan: {}", res.execution_plan);
        assert_eq!(res.rows.len(), 200);
        // LEFT join keeps the orderless user with NULL right columns.
        let res = sql
            .execute(
                "SELECT username, order_id FROM chr_users \
                 LEFT JOIN chr_orders ON user_id = user_id WHERE username = 'nobody'",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("order_id"), Some(&Value::Null));
        // RIGHT / FULL stay on hash plans.
        let res = sql
            .execute(
                "SELECT username FROM chr_users \
                 RIGHT JOIN chr_orders ON user_id = user_id WHERE order_id = 10",
            )
            .unwrap();
        assert!(res.execution_plan.contains("HashJoin"), "plan: {}", res.execution_plan);
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
