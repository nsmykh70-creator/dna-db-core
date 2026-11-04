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

impl Expr {
    pub fn eval(&self, row: &std::collections::BTreeMap<String, Value>) -> Option<Value> {
        match self {
            Expr::Literal(v) => Some(v.clone()),
            Expr::Column(name) => row.get(name).cloned(),
            Expr::Not(e) => {
                let v = e.eval(row)?;
                Some(Value::Int(
                    !(v.compare(&Value::Int(0)) != std::cmp::Ordering::Equal
                        && !matches!(v, Value::Null)) as i64,
                ))
            }
            Expr::And(a, b) => {
                let (a, b) = (a.eval(row)?, b.eval(row)?);
                Some(Value::Int((is_truthy(&a) && is_truthy(&b)) as i64))
            }
            Expr::Or(a, b) => {
                let (a, b) = (a.eval(row)?, b.eval(row)?);
                Some(Value::Int((is_truthy(&a) || is_truthy(&b)) as i64))
            }
            Expr::IsNull(e) => {
                let v = e.eval(row)?;
                Some(Value::Int(v.is_null() as i64))
            }
            Expr::IsNotNull(e) => {
                let v = e.eval(row)?;
                Some(Value::Int((!v.is_null()) as i64))
            }
            Expr::Arith { op, left, right } => {
                let l = left.eval(row)?;
                let r = right.eval(row)?;
                if l.is_null() || r.is_null() {
                    return Some(Value::Null);
                }
                arith(*op, &l, &r)
            }
            Expr::InList { column, values, negated } => {
                let v = row.get(column).cloned()?;
                if v.is_null() {
                    return Some(Value::Null);
                }
                let matched = values
                    .iter()
                    .any(|c| c.compare(&v) == std::cmp::Ordering::Equal);
                if *negated {
                    // MySQL: x NOT IN (…) with NULL in the list is UNKNOWN
                    // when x matches nothing, so the row is excluded.
                    if matched {
                        Some(Value::Int(0))
                    } else if values.iter().any(|c| c.is_null()) {
                        Some(Value::Null)
                    } else {
                        Some(Value::Int(1))
                    }
                } else {
                    Some(Value::Int(matched as i64))
                }
            }
            Expr::InSubquery { .. }
            | Expr::ScalarSubquery { .. }
            | Expr::Exists { .. } => Some(Value::Null),
            Expr::Like { operand, pattern, negated } => {
                if pattern == NULL_LIKE_PATTERN {
                    return Some(Value::Null);
                }
                let v = operand.eval(row)?;
                if v.is_null() {
                    return Some(Value::Null);
                }
                let hay = match v {
                    Value::Text(s) | Value::Dna(s) => s,
                    other => other.to_string(),
                };
                let hit = like_match(pattern, &hay);
                Some(Value::Int((hit != *negated) as i64))
            }
            Expr::Cast { inner, ty } => {
                let v = inner.eval(row)?;
                if v.is_null() {
                    return Some(Value::Null);
                }
                Some(cast_value(*ty, &v))
            }
            Expr::Cmp { op, left, right } => {
                let l = left.eval(row)?;
                let r = right.eval(row)?;
                if l.is_null() || r.is_null() {
                    return Some(Value::Null);
                }
                let ord = l.compare(&r);
                let res = match op {
                    CmpOp::Eq => ord == std::cmp::Ordering::Equal,
                    CmpOp::Ne => ord != std::cmp::Ordering::Equal,
                    CmpOp::Lt => ord == std::cmp::Ordering::Less,
                    CmpOp::Le => ord != std::cmp::Ordering::Greater,
                    CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                    CmpOp::Ge => ord != std::cmp::Ordering::Less,
                };
                Some(Value::Int(res as i64))
            }
            Expr::Fn { name, args } => {
                let vals: Vec<Option<Value>> = args.iter().map(|a| a.eval(row)).collect();
                // MySQL functions surface SQL NULL as a value (never as the
                // engine's internal "no value" None): CONCAT('a', NULL) is
                // NULL, not an absent cell.
                Some(eval_scalar_fn(name, &vals).unwrap_or(Value::Null))
            }
        }
    }

    pub fn eval_bool(&self, row: &std::collections::BTreeMap<String, Value>) -> bool {
        matches!(self.eval(row), Some(Value::Int(1)))
    }

    /// Human-readable rendering for execution plans.
    pub fn describe(&self) -> String {
        match self {
            Expr::Literal(v) => format!("{}", v),
            Expr::Column(c) => c.clone(),
            Expr::Not(e) => format!("NOT({})", e.describe()),
            Expr::And(a, b) => format!("({} AND {})", a.describe(), b.describe()),
            Expr::Or(a, b) => format!("({} OR {})", a.describe(), b.describe()),
            Expr::IsNull(e) => format!("{} IS NULL", e.describe()),
            Expr::IsNotNull(e) => format!("{} IS NOT NULL", e.describe()),
            Expr::Arith { op, left, right } => {
                format!("{} {} {}", left.describe(), op.as_str(), right.describe())
            }
            Expr::InList { column, values, negated } => format!(
                "{}{} IN ({})",
                if *negated { "NOT " } else { "" },
                column,
                values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::InSubquery { column, query } => {
                format!("{} IN ({})", column, query)
            }
            Expr::ScalarSubquery { stmt } => {
                format!("({})", crate::sql_engine::render_stmt_sql(stmt))
            }
            Expr::Exists { stmt, negated } => {
                format!(
                    "{}EXISTS ({})",
                    if *negated { "NOT " } else { "" },
                    crate::sql_engine::render_stmt_sql(stmt)
                )
            }
            Expr::Cmp { op, left, right } => {
                format!("{} {} {}", left.describe(), op.as_str(), right.describe())
            }
            Expr::Like { operand, pattern, negated } => format!(
                "{} {}LIKE '{}'",
                operand.describe(),
                if *negated { "NOT " } else { "" },
                pattern
            ),
            Expr::Cast { inner, ty } => {
                format!("CAST({} AS {})", inner.describe(), ty.as_str())
            }
            Expr::Fn { name, args } => format!(
                "{}({})",
                name,
                args.iter()
                    .map(|a| a.describe())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Columns referenced by this expression (for projection checks).
    pub fn collect_columns(&self, out: &mut Vec<String>) {
        match self {
            Expr::Column(c) => {
                if !out.contains(c) {
                    out.push(c.clone());
                }
            }
            Expr::Literal(_) => {}
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => e.collect_columns(out),
            Expr::Arith { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
            Expr::InList { column, .. } | Expr::InSubquery { column, .. } => {
                if !out.contains(column) {
                    out.push(column.clone());
                }
            }
            // Subquery nodes: their columns live in the subquery scope, not
            // in the outer projection, so nothing to collect here.
            Expr::ScalarSubquery { .. } | Expr::Exists { .. } => {}
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.collect_columns(out);
                b.collect_columns(out);
            }
            Expr::Cmp { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
            Expr::Like { operand, .. } => operand.collect_columns(out),
            Expr::Cast { inner, .. } => inner.collect_columns(out),
            Expr::Fn { args, .. } => {
                for a in args {
                    a.collect_columns(out);
                }
            }
        }
    }
}

/// MySQL CAST semantics: numeric conversion reads the leading numeric
/// prefix of the text ("150abc" -> 150, "abc" -> 0); SIGNED rounds
/// fractional values half away from zero; CHAR stringifies.
fn cast_value(ty: CastTy, v: &Value) -> Value {
    match ty {
        CastTy::Char => Value::Text(match v {
            Value::Text(s) | Value::Dna(s) => s.clone(),
            other => other.to_string(),
        }),
        CastTy::Signed | CastTy::Unsigned => {
            let n = numeric_prefix(v);
            let n = match n {
                Some(f) => f,
                None => return Value::Int(0),
            };
            // Round half away from zero (MySQL), saturating at i64 bounds.
            let rounded = n.round();
            let i = if rounded >= i64::MAX as f64 {
                i64::MAX
            } else if rounded <= i64::MIN as f64 {
                i64::MIN
            } else {
                rounded as i64
            };
            if ty == CastTy::Unsigned && i < 0 {
                Value::Int(0)
            } else {
                Value::Int(i)
            }
        }
    }
}

/// Leading numeric prefix of a value per MySQL string-to-number rules.
/// Returns None when there is no numeric prefix at all (caller maps it to
/// 0); for text, parses an optional sign, digits, fraction and exponent.
/// Delegates to `Value::mysql_numeric_prefix` (relational.rs) — the single
/// source of truth shared with the CMS insert/update column-type coercion.
fn numeric_prefix(v: &Value) -> Option<f64> {
    v.mysql_numeric_prefix()
}

/// MySQL LIKE with % (any run) and _ (one char), case-insensitive (ci
/// collations). A backslash escapes the next wildcard character, so
/// '\_transient\_%' anchors on a literal underscore.
pub fn like_match(pattern: &str, text: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    like_rec(p.as_bytes(), t.as_bytes())
}

fn like_rec(p: &[u8], t: &[u8]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some(b'%'), _) => like_rec(&p[1..], t) || (!t.is_empty() && like_rec(p, &t[1..])),
        // A backslash in the pattern is authoritative: it always escapes the
        // NEXT pattern char (no fall-through), so '\_' matches a literal
        // underscore and never the two characters '\','_'.
        (Some(b'\\'), _) if p.len() >= 2 => {
            !t.is_empty() && p[1] == t[0] && like_rec(&p[2..], &t[1..])
        }
        (Some(b'_'), Some(_)) => like_rec(&p[1..], &t[1..]),
        (Some(&c), Some(&d)) => c == d && like_rec(&p[1..], &t[1..]),
        _ => false,
    }
}

/// Numeric arithmetic with Int/Float promotion. Division always produces a
/// Float; x / 0 (and x / 0.0) produces NULL instead of panicking.
/// Integer overflow produces NULL (SQL three-valued logic) instead of the
/// silently wrapped value `wrapping_*` used to return: a WHERE predicate
/// must never compare against a bogus number that happens to fit.
fn arith(op: ArithOp, l: &Value, r: &Value) -> Option<Value> {
    fn num(v: &Value) -> Option<(f64, bool)> {
        match v {
            Value::Int(i) => Some((*i as f64, true)),
            Value::Float(f) => Some((*f, false)),
            // CMS edition (branch cms-edition): MySQL string-to-number
            // coercion reads the leading numeric prefix — '250.00' + 0
            // -> 250, 'abc' + 0 -> 0, NULL stays NULL. The WordPress/
            // WooCommerce `ORDER BY meta_value+0` numeric-sort idiom
            // depends on it. Plain builds keep the strict NULL semantics
            // for text arithmetic.
            #[cfg(feature = "cms")]
            Value::Text(_) | Value::Dna(_) => Some((numeric_prefix(v).unwrap_or(0.0), false)),
            _ => None,
        }
    }
    let (a, ai) = num(l)?;
    let (b, bi) = num(r)?;
    if op == ArithOp::Div && b == 0.0 {
        return Some(Value::Null);
    }
    // Exact integer path: keep i64 precision and surface overflow as NULL
    // (three-valued logic) instead of the silently wrapped value that
    // wrapping_* used to return. A WHERE predicate must never compare
    // against a bogus number that merely happens to fit.
    if ai && bi && !matches!(op, ArithOp::Div) {
        let x = match l {
            Value::Int(x) => *x,
            _ => 0,
        };
        let y = match r {
            Value::Int(y) => *y,
            _ => 0,
        };
        let checked = match op {
            ArithOp::Add => x.checked_add(y),
            ArithOp::Sub => x.checked_sub(y),
            ArithOp::Mul => x.checked_mul(y),
            ArithOp::Div => None,
        };
        return checked.map(Value::Int);
    }
    Some(match op {
        ArithOp::Add => Value::Float(a + b),
        ArithOp::Sub => Value::Float(a - b),
        ArithOp::Mul => Value::Float(a * b),
        ArithOp::Div => Value::Float(a / b),
    })
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::Text(s) => !s.is_empty(),
        Value::Dna(s) => !s.is_empty(),
        Value::Null => false,
    }
}

/// Textual rendering used by the string functions (MySQL converts numbers
/// to their shortest text form inside CONCAT/UPPER/…).
fn fn_to_text(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Text(s) | Value::Dna(s) => s.clone(),
        Value::Null => String::new(),
    }
}

fn fn_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        // MySQL coerces numeric-looking text in numeric contexts.
        Value::Text(s) | Value::Dna(s) => s.trim().parse::<f64>().ok(),
        Value::Null => None,
    }
}

/// Public wrapper for the SQL→DnaQL translator: evaluate a function over
/// already-computed argument values (constant folding path).
pub fn eval_for_translator(name: &str, vals: &[Option<Value>]) -> Option<Value> {
    eval_scalar_fn(name, vals)
}

/// MySQL scalar function evaluation (CMS edition). NULL semantics follow
/// the server: most functions return NULL on any NULL input; COALESCE /
/// IFNULL / CONCAT_WS / IF have their documented exceptions.
#[allow(clippy::too_many_lines)]
pub(crate) fn eval_scalar_fn(name: &str, vals: &[Option<Value>]) -> Option<Value> {
    // eval() yields Option<Value> where BOTH None (no value) and
    // Some(Value::Null) mean SQL NULL here — function NULL semantics must
    // treat them identically (CONCAT('a', NULL) → NULL etc.).
    let any_null = vals
        .iter()
        .any(|v| matches!(v, None | Some(Value::Null)));
    let sql_non_null = |v: &Option<Value>| !matches!(v, None | Some(Value::Null));
    let txt = |i: usize| vals.get(i).cloned().flatten().map(|v| fn_to_text(&v));
    let num = |i: usize| vals.get(i).cloned().flatten().and_then(|v| fn_as_f64(&v));
    let int = |i: usize| match vals.get(i).cloned().flatten() {
        Some(Value::Int(i)) => Some(i),
        Some(v) => fn_as_f64(&v).map(|f| f as i64),
        None => None,
    };
    match name {
        "CONCAT" => {
            if any_null {
                return Some(Value::Null);
            }
            Some(Value::Text(
                (0..vals.len()).filter_map(txt).collect::<Vec<_>>().join(""),
            ))
        }
        "CONCAT_WS" => {
            // CONCAT_WS(NULL, ...) → NULL; NULL arguments are skipped.
            let sep = txt(0)?;
            let parts: Vec<String> = (1..vals.len())
                .filter_map(|i| vals.get(i).cloned().flatten())
                .filter(|v| !matches!(v, Value::Null))
                .map(|v| fn_to_text(&v))
                .collect();
            Some(Value::Text(parts.join(&sep)))
        }
        "UPPER" | "UCASE" => txt(0).map(|s| Value::Text(s.to_uppercase())),
        "LOWER" | "LCASE" => txt(0).map(|s| Value::Text(s.to_lowercase())),
        "LENGTH" => txt(0).map(|s| Value::Int(s.len() as i64)),
        "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
            txt(0).map(|s| Value::Int(s.chars().count() as i64))
        }
        "TRIM" => txt(0).map(|s| Value::Text(s.trim().to_string())),
        "LTRIM" => txt(0).map(|s| Value::Text(s.trim_start().to_string())),
        "RTRIM" => txt(0).map(|s| Value::Text(s.trim_end().to_string())),
        "SUBSTRING" | "SUBSTR" | "MID" => {
            let s = txt(0)?;
            let chars: Vec<char> = s.chars().collect();
            let pos = int(1)?;
            // MySQL 1-based; negative pos counts from the end; 0 → ''.
            let start = if pos > 0 {
                (pos - 1) as usize
            } else if pos < 0 {
                let back = (-pos) as usize;
                if back >= chars.len() {
                    return Some(Value::Text(String::new()));
                }
                chars.len() - back
            } else {
                return Some(Value::Text(String::new()));
            };
            let end = match int(2) {
                Some(len) if len <= 0 => start,
                Some(len) => (start + len as usize).min(chars.len()),
                None => chars.len(),
            };
            if start >= chars.len() || start >= end {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(chars[start..end].iter().collect()))
        }
        "REPLACE" => {
            let (s, from, to) = (txt(0)?, txt(1)?, txt(2)?);
            if from.is_empty() {
                return Some(Value::Text(s));
            }
            Some(Value::Text(s.replace(&from, &to)))
        }
        "LEFT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(
                s.chars().take(n as usize).collect::<String>(),
            ))
        }
        "RIGHT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            let chars: Vec<char> = s.chars().collect();
            let take = (n as usize).min(chars.len());
            Some(Value::Text(
                chars[chars.len() - take..].iter().collect::<String>(),
            ))
        }
        "REVERSE" => txt(0).map(|s| Value::Text(s.chars().rev().collect::<String>())),
        "LPAD" | "RPAD" => {
            let s = txt(0)?;
            let target = int(1)? as usize;
            let pad = txt(2)?;
            if pad.is_empty() {
                return Some(Value::Text(s));
            }
            let cur = s.chars().count();
            if target <= cur {
                let out: String = s.chars().take(target).collect();
                return Some(Value::Text(out));
            }
            let fill: String = pad.chars().cycle().take(target - cur).collect();
            Some(Value::Text(if name == "LPAD" {
                format!("{fill}{s}")
            } else {
                format!("{s}{fill}")
            }))
        }
        "REPEAT" => {
            let s = txt(0)?;
            let n = int(1)?;
            if n <= 0 {
                return Some(Value::Text(String::new()));
            }
            Some(Value::Text(s.repeat(n as usize)))
        }
        "ABS" => match vals.first().cloned().flatten() {
            Some(Value::Int(i)) => Some(Value::Int(i.abs())),
            Some(Value::Float(f)) => Some(Value::Float(f.abs())),
            Some(v) => fn_as_f64(&v).map(|f| Value::Float(f.abs())),
            None => None,
        },
        "ROUND" => {
            let x = num(0)?;
            let d = int(1).unwrap_or(0);
            // MySQL rounds half away from zero.
            let factor = 10f64.powi(d as i32);
            let r = (x * factor).round() / factor;
            if vals.first().cloned().flatten().is_some_and(|v| matches!(v, Value::Int(_))) && d <= 0 {
                Some(Value::Int(r as i64))
            } else {
                Some(Value::Float(r))
            }
        }
        "FLOOR" => num(0).map(|f| Value::Int(f.floor() as i64)),
        "CEIL" | "CEILING" => num(0).map(|f| Value::Int(f.ceil() as i64)),
        "SQRT" => num(0).and_then(|f| {
            if f < 0.0 {
                None // MySQL: negative input → NULL
            } else {
                Some(Value::Float(f.sqrt()))
            }
        }),
        "POW" | "POWER" => num(0)?
            .powf(num(1)?)
            .try_into()
            .ok()
            .map(Value::Float),
        "SIGN" => num(0).map(|f| Value::Int(f.signum() as i64)),
        "MOD" => {
            let (a, b) = (num(0)?, num(1)?);
            if b == 0.0 {
                return None;
            }
            if matches!(vals.first(), Some(Some(Value::Int(_))))
                && matches!(vals.get(1), Some(Some(Value::Int(_))))
            {
                Some(Value::Int(a as i64 % b as i64))
            } else {
                Some(Value::Float(a % b))
            }
        }
        "COALESCE" => vals
            .iter()
            .find(|v| sql_non_null(v))
            .cloned()
            .flatten()
            .or(Some(Value::Null)),
        "IFNULL" | "NVL" => {
            let a = vals.first().cloned().flatten();
            match a {
                Some(v) if !matches!(v, Value::Null) => Some(v),
                _ => Some(vals.get(1).cloned().flatten().unwrap_or(Value::Null)),
            }
        }
        "NULLIF" => {
            let (a, b) = (vals.first().cloned().flatten()?, vals.get(1).cloned().flatten()?);
            if !a.is_null() && !b.is_null() && a.compare(&b) == std::cmp::Ordering::Equal {
                Some(Value::Null)
            } else {
                Some(a)
            }
        }
        "IF" => {
            let cond = vals.first().cloned().flatten()?;
            let branch = if is_truthy(&cond) { 1 } else { 2 };
            vals.get(branch).cloned().flatten()
        }
        "GREATEST" | "LEAST" => {
            if any_null {
                return None;
            }
            let mut best = vals.first().cloned().flatten()?;
            for v in &vals[1..] {
                let v = v.clone()?;
                let ord = v.compare(&best);
                let take = match name {
                    "GREATEST" => ord == std::cmp::Ordering::Greater,
                    _ => ord == std::cmp::Ordering::Less,
                };
                if take {
                    best = v;
                }
            }
            Some(best)
        }
        "NOW" => Some(Value::Text(datetime_text(now_parts()))),
        "CURDATE" => {
            let (y, m, d, _, _, _) = now_parts();
            Some(Value::Text(format!("{y:04}-{m:02}-{d:02}")))
        }
        "CURTIME" => {
            let (_, _, _, hh, mm, ss) = now_parts();
            Some(Value::Text(format!("{hh:02}:{mm:02}:{ss:02}")))
        }
        "UNIX_TIMESTAMP" => Some(Value::Int(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        )),
        "DATE_FORMAT" | "TIME_FORMAT" => {
            let dt = txt(0)?;
            let fmt = txt(1)?;
            Some(Value::Text(format_datetime(&parse_datetime(&dt), &fmt)))
        }
        _ => None, // is_scalar_fn gate keeps this unreachable
    }
}

// --- MySQL datetime helpers (scalar fn support) --------------------------

/// Current UTC time as (year, month, day, hour, minute, second).
pub(crate) fn now_parts() -> (i64, i64, i64, i64, i64, i64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m as i64, d as i64, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-since-epoch → (year, month, day).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since epoch for a proleptic Gregorian date (inverse of civil_from_days).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = m as i64;
    let doy = (153 * (if mp > 2 { mp - 3 } else { mp + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DD[ HH:MM:SS[:fraction]]` (MySQL DATE/DATETIME text).
/// Missing time parts default to 00:00:00.
pub(crate) fn parse_datetime(s: &str) -> (i64, i64, i64, i64, i64, i64) {
    let s = s.trim();
    let mut date = s;
    let mut time = "";
    if let Some(sp) = s.find(' ') {
        date = &s[..sp];
        time = s[sp + 1..].trim_end_matches('Z');
    }
    let nums: Vec<i64> = date
        .split('-')
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    let (y, m, d) = match nums.as_slice() {
        [y, m, d] => (*y, *m, *d),
        _ => {
            let (y, m, d, _, _, _) = now_parts();
            (y, m, d)
        }
    };
    let tnums: Vec<i64> = time
        .split(':')
        .map(|p| p.split('.').next().unwrap_or(p))
        .filter_map(|p| p.parse::<i64>().ok())
        .collect();
    let (hh, mm, ss) = match tnums.as_slice() {
        [a, b, c] => (*a, *b, *c),
        [a, b] => (*a, *b, 0),
        [a] => (*a, 0, 0),
        _ => (0, 0, 0),
    };
    (y, m, d, hh, mm, ss)
}

/// (y, m, d, hh, mm, ss) → `YYYY-MM-DD HH:MM:SS`.
pub(crate) fn datetime_text(p: (i64, i64, i64, i64, i64, i64)) -> String {
    let (y, m, d, hh, mm, ss) = p;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        p.0, p.1, p.2, p.3, p.4, p.5
    )
}

/// Shift a datetime value by `n` units (MySQL `+/- INTERVAL n UNIT`).
/// Date parts carry (months for MONTH/YEAR, days for the rest).
pub(crate) fn shift_datetime(
    v: &Value,
    n: i64,
    unit: &str,
) -> Value {
    let (y, mo, d, hh, mm, ss) = parse_datetime(&fn_to_text(v));
    match unit.to_ascii_uppercase().as_str() {
        "YEAR" | "MONTH" => {
            let months = y * 12 + mo as i64 - 1
                + if unit.eq_ignore_ascii_case("YEAR") { n * 12 } else { n };
            let ny = months.div_euclid(12);
            let nmo = months.rem_euclid(12) as u32 + 1;
            // clamp day to the target month's length
            let dim = days_in_month(ny, nmo);
            let nd = (d as i64).min(dim) as u32;
            Value::Text(datetime_text((ny, nmo as i64, nd as i64, hh, mm, ss)))
        }
        "SECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" => {
            let mult = match unit.to_ascii_uppercase().as_str() {
                "SECOND" => 1,
                "MINUTE" => 60,
                "HOUR" => 3600,
                "DAY" => 86_400,
                _ => 604_800,
            };
            let total = days_from_civil(y, mo as u32, d as u32) * 86_400
                + hh * 3600
                + mm * 60
                + ss
                + n * mult;
            let days = total.div_euclid(86_400);
            let rem = total.rem_euclid(86_400);
            let (ny, nmo, nd) = civil_from_days(days);
            Value::Text(datetime_text((
                ny,
                nmo as i64,
                nd as i64,
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60,
            )))
        }
        _ => v.clone(),
    }
}

fn days_in_month(y: i64, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// MySQL DATE_FORMAT subset: the specifiers real CMS dumps use.
fn format_datetime(p: &(i64, i64, i64, i64, i64, i64), fmt: &str) -> String {
    let (y, mo, d, hh, mm, ss) = *p;
    let weekday = (days_from_civil(y, mo as u32, d as u32).rem_euclid(7) + 4) % 7; // 1970-01-01 = Thursday
    let months = [
        "January", "February", "March", "April", "May", "June", "July",
        "August", "September", "October", "November", "December",
    ];
    let days_n = [
        "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday",
    ];
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{mo:02}")),
            Some('c') => out.push_str(&mo.to_string()),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('e') => out.push_str(&d.to_string()),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('k') => out.push_str(&hh.to_string()),
            Some('h') | Some('I') => {
                let h12 = if hh % 12 == 0 { 12 } else { hh % 12 };
                out.push_str(&format!("{h12:02}"))
            }
            Some('i') => out.push_str(&format!("{mm:02}")),
            Some('s') | Some('S') => out.push_str(&format!("{ss:02}")),
            Some('f') => out.push_str("000000"),
            Some('p') => out.push_str(if hh < 12 { "AM" } else { "PM" }),
            Some('W') => out.push_str(days_n[weekday as usize]),
            Some('a') => out.push_str(&days_n[weekday as usize][..3]),
            Some('M') => out.push_str(months[(mo as usize - 1).min(11)]),
            Some('b') => out.push_str(&months[(mo as usize - 1).min(11)][..3]),
            Some('j') => out.push_str(&format!(
                "{:03}",
                days_from_civil(y, mo as u32, d as u32) - days_from_civil(y, 1, 1) + 1
            )),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_null_semantics() {
        let f = |args: Vec<Option<Value>>| {
            eval_scalar_fn("CONCAT", &args).map(|v| match v {
                Value::Text(s) => s,
                Value::Null => "NULL".into(),
                other => other.to_string(),
            })
        };
        assert_eq!(
            f(vec![Some(Value::Text("a".into())), None, Some(Value::Text("b".into()))]),
            Some("NULL".into()),
            "None arg → NULL"
        );
        assert_eq!(
            f(vec![
                Some(Value::Text("a".into())),
                Some(Value::Null),
                Some(Value::Text("b".into()))
            ]),
            Some("NULL".into()),
            "Some(Null) arg → NULL (MySQL parity)"
        );
        assert_eq!(
            f(vec![
                Some(Value::Text("a".into())),
                Some(Value::Int(1)),
                Some(Value::Float(2.5))
            ]),
            Some("a12.5".into())
        );
    }

    #[test]
    fn fn_eval_through_expr() {
        use std::collections::BTreeMap;
        let r: BTreeMap<String, Value> = BTreeMap::new();
        // CONCAT('днк', '-', 'тест')
        let e = Expr::Fn {
            name: "CONCAT".into(),
            args: vec![
                Expr::Literal(Value::Text("днк".into())),
                Expr::Literal(Value::Text("-".into())),
                Expr::Literal(Value::Text("тест".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("днк-тест".into())));
        // CONCAT with a NULL literal → NULL
        let e = Expr::Fn {
            name: "CONCAT".into(),
            args: vec![
                Expr::Literal(Value::Text("a".into())),
                Expr::Literal(Value::Null),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Null));
        // IFNULL(NULL, 'x') → 'x'; COALESCE(NULL, 'x', 'y') → 'x';
        // IF(1>0, 'yes', 'no') → 'yes'
        let e = Expr::Fn {
            name: "IFNULL".into(),
            args: vec![
                Expr::Literal(Value::Null),
                Expr::Literal(Value::Text("x".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("x".into())));
        let e = Expr::Fn {
            name: "COALESCE".into(),
            args: vec![
                Expr::Literal(Value::Null),
                Expr::Literal(Value::Text("x".into())),
                Expr::Literal(Value::Text("y".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("x".into())));
        let e = Expr::Fn {
            name: "IF".into(),
            args: vec![
                Expr::Cmp {
                    op: CmpOp::Gt,
                    left: Box::new(Expr::Literal(Value::Int(1))),
                    right: Box::new(Expr::Literal(Value::Int(0))),
                },
                Expr::Literal(Value::Text("yes".into())),
                Expr::Literal(Value::Text("no".into())),
            ],
        };
        assert_eq!(e.eval(&r), Some(Value::Text("yes".into())));
    }

    #[test]
    fn fn_string_numeric_basics() {
        use std::collections::BTreeMap;
        let r: BTreeMap<String, Value> = BTreeMap::new();
        let ev = |name: &str, args: Vec<Expr>| -> Option<Value> {
            Expr::Fn {
                name: name.into(),
                args,
            }
            .eval(&r)
        };
        let t = |s: &str| Expr::Literal(Value::Text(s.into()));
        let i = |n: i64| Expr::Literal(Value::Int(n));
        assert_eq!(ev("UPPER", vec![t("abc")]), Some(Value::Text("ABC".into())));
        assert_eq!(ev("LOWER", vec![t("ABC")]), Some(Value::Text("abc".into())));
        assert_eq!(ev("CHAR_LENGTH", vec![t("днк")]), Some(Value::Int(3)));
        assert_eq!(ev("LENGTH", vec![t("днк")]), Some(Value::Int(6)));
        assert_eq!(ev("TRIM", vec![t("  x  ")]), Some(Value::Text("x".into())));
        assert_eq!(
            ev("SUBSTRING", vec![t("wordpress"), i(1), i(3)]),
            Some(Value::Text("wor".into()))
        );
        assert_eq!(
            ev("SUBSTRING", vec![t("wordpress"), i(-5)]),
            Some(Value::Text("press".into()))
        );
        assert_eq!(
            ev("REPLACE", vec![t("a-b-c"), t("-"), t("+")]),
            Some(Value::Text("a+b+c".into()))
        );
        assert_eq!(ev("LEFT", vec![t("hello"), i(2)]), Some(Value::Text("he".into())));
        assert_eq!(ev("RIGHT", vec![t("hello"), i(2)]), Some(Value::Text("lo".into())));
        assert_eq!(ev("ABS", vec![i(-5)]), Some(Value::Int(5)));
        assert_eq!(ev("ROUND", vec![Expr::Literal(Value::Float(2.5))]), Some(Value::Float(3.0)));
        assert_eq!(ev("FLOOR", vec![Expr::Literal(Value::Float(2.9))]), Some(Value::Int(2)));
        assert_eq!(ev("CEIL", vec![Expr::Literal(Value::Float(2.1))]), Some(Value::Int(3)));
        assert_eq!(ev("SQRT", vec![i(-1)]), Some(Value::Null));
        assert_eq!(
            ev("CONCAT_WS", vec![t(","), t("a"), Value::Null.into_lit(), t("b")]),
            Some(Value::Text("a,b".into()))
        );
        assert_eq!(
            ev("GREATEST", vec![i(3), i(9), i(1)]),
            Some(Value::Int(9))
        );
        assert_eq!(ev("LEAST", vec![i(3), i(9), i(1)]), Some(Value::Int(1)));
        // NULLIF('a', 'a') → NULL; NULLIF('a', 'b') → 'a'
        assert_eq!(ev("NULLIF", vec![t("a"), t("a")]), Some(Value::Null));
        assert_eq!(ev("NULLIF", vec![t("a"), t("b")]), Some(Value::Text("a".into())));
        assert_eq!(eval_scalar_fn("NOPE", &[]), None);
    }

    trait IntoLit {
        fn into_lit(self) -> Expr;
    }
    impl IntoLit for Value {
        fn into_lit(self) -> Expr {
            Expr::Literal(self)
        }
    }

    use super::*;
    use std::collections::BTreeMap;

    fn row() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert("total_usd".to_string(), Value::Float(350.0));
        m.insert("user_id".to_string(), Value::Int(14));
        m.insert("name".to_string(), Value::Text("crick".to_string()));
        m.insert("missing".to_string(), Value::Null);
        m
    }

    #[test]
    fn comparisons() {
        let r = row();
        let e = Expr::Cmp {
            op: CmpOp::Gt,
            left: Box::new(Expr::Column("total_usd".into())),
            right: Box::new(Expr::Literal(Value::Float(300.0))),
        };
        assert!(e.eval_bool(&r));

        let e2 = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("name".into())),
            right: Box::new(Expr::Literal(Value::Text("crick".into()))),
        };
        assert!(e2.eval_bool(&r));

        let e3 = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("user_id".into())),
            right: Box::new(Expr::Literal(Value::Int(99))),
        };
        assert!(!e3.eval_bool(&r));
    }

    #[test]
    fn and_or_not() {
        let r = row();
        let e = Expr::And(
            Box::new(Expr::Cmp {
                op: CmpOp::Gt,
                left: Box::new(Expr::Column("total_usd".into())),
                right: Box::new(Expr::Literal(Value::Float(100.0))),
            }),
            Box::new(Expr::Cmp {
                op: CmpOp::Lt,
                left: Box::new(Expr::Column("user_id".into())),
                right: Box::new(Expr::Literal(Value::Int(20))),
            }),
        );
        assert!(e.eval_bool(&r));
        assert!(e.describe().contains("AND"));

        // NULL comparison -> not truthy
        let en = Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(Expr::Column("missing".into())),
            right: Box::new(Expr::Literal(Value::Int(0))),
        };
        assert!(!en.eval_bool(&r));
    }

    #[test]
    fn cast_mysql_semantics() {
        let signed = |lit: Value| Expr::Cast {
            inner: Box::new(Expr::Literal(lit)),
            ty: CastTy::Signed,
        };
        let r = BTreeMap::new();
        // Numeric prefix: "150" -> 150, "19.9" -> 20 (rounds), "abc" -> 0.
        assert_eq!(
            signed(Value::Text("150".into())).eval(&r),
            Some(Value::Int(150))
        );
        assert_eq!(
            signed(Value::Text("19.9".into())).eval(&r),
            Some(Value::Int(20))
        );
        assert_eq!(
            signed(Value::Text("abc".into())).eval(&r),
            Some(Value::Int(0))
        );
        // NULL propagates (SQL NULL, not "no value").
        assert_eq!(signed(Value::Null).eval(&r), Some(Value::Null));
        // CHAR stringifies.
        let c = Expr::Cast {
            inner: Box::new(Expr::Literal(Value::Int(42))),
            ty: CastTy::Char,
        };
        assert_eq!(c.eval(&r), Some(Value::Text("42".into())));
    }
}
