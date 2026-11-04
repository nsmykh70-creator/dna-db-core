// SPDX-License-Identifier: AGPL-3.0-only
//! Statement AST (`Stmt`) and its supporting enums — projection items,
//! joins, conflict clauses, window/aggregate function descriptors.

use crate::expr::Expr;
use crate::relational::{ChromosomeTable, ColumnType, Value};

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
        /// CMS edition: `INSERT IGNORE` — rows that hit ANY uniqueness
        /// constraint (primary key or unique index) are skipped instead of
        /// failing the statement (MediaWiki `INSERT IGNORE INTO`, Joomla
        /// session writes).
        ignore: bool,
    },
    /// CMS edition: `INSERT INTO t (cols) SELECT …` — the SELECT is executed
    /// and its rows are inserted positionally (MySQL semantics). Used by
    /// WooCommerce/ActionScheduler claim/migration paths.
    InsertSelect {
        table: String,
        cols: Vec<String>,
        select: Box<Stmt>,
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
    /// CMS edition: bare column ref — the EXISTING (conflicting) row's value,
    /// `ON DUPLICATE KEY UPDATE c = c`.
    Column(String),
    /// CMS edition: existing value plus a literal delta,
    /// `ON DUPLICATE KEY UPDATE ss_users = ss_users+1` (MediaWiki
    /// SiteStatsUpdate, WooCommerce counters).
    ColumnAdd { column: String, delta: Value },
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
    /// CMS edition: `SUM(expr)` / `AVG(expr)` … with an EXPRESSION argument
    /// (Joomla's dashboard: `SUM(CASE WHEN … END) AS due_count`). The
    /// expression is materialised per row under `arg_name` before
    /// aggregation; only the aggregate output column is emitted.
    AggOverExpr {
        expr: Expr,
        /// Row key the expression is materialised under (= its rendering).
        arg_name: String,
        func: AggFunc,
        rendered: String,
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
    pub(super) fn name(&self) -> &'static str {
        match self {
            WindowFunc::RowNumber => "ROW_NUMBER",
            WindowFunc::Rank => "RANK",
            WindowFunc::DenseRank => "DENSE_RANK",
        }
    }
    pub(super) fn from_name(name: &str) -> Option<Self> {
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
    pub(super) fn from_name(name: &str) -> Result<Self, String> {
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

