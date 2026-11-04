// SPDX-License-Identifier: AGPL-3.0-only
//! Shared expression AST + evaluator for WHERE/FILTER clauses.
//! Used by both DnaQL pipelines and the SQL engine.
//! Grammar: or_expr -> and_expr (OR and_expr)*
//!          and_expr -> cmp_expr (AND cmp_expr)*
//!          cmp_expr -> operand ((=|!=|<|<=|>|>=) operand)?
//!          operand -> literal | column | ( or_expr ) | -operand
//! Literals: integers, floats, 'strings', NULL, TRUE/FALSE.

use crate::relational::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp {
        op: CmpOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull(Box<Expr>),
    /// `col IS NOT NULL` — the negation of IsNull (never NULL-valued itself).
    IsNotNull(Box<Expr>),
    /// Arithmetic in projections / operands: `total_usd * 2`.
    /// Int op Int stays Int for `+ - *`; division always yields Float and
    /// division by zero yields NULL (documented SQL surface behaviour).
    Arith {
        op: ArithOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `col [NOT] IN (v1, v2, ...)` — evaluated here; NULL semantics: no
    /// match when the column value is NULL, or (NOT IN) the list contains
    /// NULL without a match — MySQL three-valued logic.
    InList {
        column: String,
        values: Vec<Value>,
        negated: bool,
    },
    /// `col IN (SELECT ...)` — SQL-only node. Must be materialised into an
    /// InList by the executor before evaluation (eval returns NULL).
    InSubquery {
        column: String,
        /// Rendered sub-SELECT, re-executed by the SQL engine.
        query: String,
    },
    /// `(SELECT ...)` scalar subquery (v3.6) — SQL-only node. Resolved by the
    /// SQL executor (uncorrelated → literal, correlated → per outer row);
    /// eval returns NULL so a node that slips through cannot fake a value.
    ScalarSubquery {
        stmt: Box<crate::sql_engine::Stmt>,
    },
    /// `[NOT] EXISTS (SELECT ...)` (v3.6) — SQL-only node, resolved per row.
    Exists {
        stmt: Box<crate::sql_engine::Stmt>,
        negated: bool,
    },
    /// `expr [NOT] LIKE 'pattern'` — MySQL wildcard match (% = any run,
    /// _ = one char, \ escapes the next char), case-insensitive (ci
    /// collations). WordPress uses LIKE everywhere (transients, slugs,
    /// meta searches).
    Like {
        operand: Box<Expr>,
        pattern: String,
        negated: bool,
    },
    /// `CAST(expr AS SIGNED|UNSIGNED|CHAR)` — typed conversion used by
    /// WordPress meta queries (`CAST(meta_value AS SIGNED) > 100`).
    /// MySQL rules: the numeric PREFIX of the text is taken ("150abc" ->
    /// 150, "abc" -> 0), fractional values round half away from zero for
    /// SIGNED, NULL propagates.
    Cast {
        inner: Box<Expr>,
        ty: CastTy,
    },
    /// CMS edition: MySQL scalar function call — `CONCAT(a, b)`, `UPPER(x)`,
    /// `IFNULL(a, b)`, `ROUND(x, d)`, … evaluated row-by-row with MySQL NULL
    /// semantics (see `eval_scalar_fn`). The name is stored uppercase.
    Fn {
        name: String,
        args: Vec<Expr>,
    },
}

/// Target type of a `CAST(expr AS type)` conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastTy {
    Signed,
    Unsigned,
    Char,
}

impl CastTy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CastTy::Signed => "SIGNED",
            CastTy::Unsigned => "UNSIGNED",
            CastTy::Char => "CHAR",
        }
    }
}

/// The scalar functions the engine evaluates natively (CMS edition). The
/// set covers what WordPress / WooCommerce / Joomla / generic MySQL apps
/// call most: string building, case folding, length, trimming, substring
/// extraction, padding, numeric rounding and NULL handling.
pub fn is_scalar_fn(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "CONCAT"
            | "CONCAT_WS"
            | "UPPER" | "UCASE"
            | "LOWER" | "LCASE"
            | "LENGTH"
            | "CHAR_LENGTH" | "CHARACTER_LENGTH"
            | "TRIM" | "LTRIM" | "RTRIM"
            | "SUBSTRING" | "SUBSTR" | "MID"
            | "REPLACE"
            | "LEFT" | "RIGHT"
            | "REVERSE"
            | "LPAD" | "RPAD"
            | "REPEAT"
            | "ABS"
            | "ROUND"
            | "FLOOR" | "CEIL" | "CEILING"
            | "SQRT" | "POW" | "POWER"
            | "SIGN"
            | "MOD"
            | "COALESCE"
            | "IFNULL" | "NVL"
            | "NULLIF"
            | "IF"
            | "GREATEST" | "LEAST"
            // MySQL datetime functions + formatting (Joomla's scheduler
            // seed rows in extensions.sql use NOW()/DATE_FORMAT/
            // TIME_FORMAT inside INSERT VALUES).
            | "NOW" | "CURDATE" | "CURTIME" | "UNIX_TIMESTAMP"
            | "DATE_FORMAT" | "TIME_FORMAT"
            | "YEAR" | "MONTH" | "DAY" | "DAYOFMONTH" | "QUARTER"
            | "HOUR" | "MINUTE" | "SECOND" | "DAYOFWEEK" | "DAYOFYEAR"
            | "WEEK" | "DAYNAME" | "MONTHNAME"
    )
}

/// Sentinel pattern for `LIKE NULL` (parameter probes): every row yields
/// SQL NULL per MySQL three-valued logic. Never matches a real pattern —
/// a literal NUL byte cannot appear in a parsed string literal.
pub const NULL_LIKE_PATTERN: &str = "\u{0}like-null";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl ArithOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

mod eval;
mod scalar;
#[cfg(test)]
mod tests;

pub use eval::like_match;
pub use scalar::eval_for_translator;
pub(crate) use scalar::{eval_scalar_fn, shift_datetime};
