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
fn numeric_prefix(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Text(s) | Value::Dna(s) => {
            let t = s.trim_start();
            let bytes = t.as_bytes();
            let mut i = 0;
            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                i += 1;
            }
            let digits_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i == digits_start {
                return None; // no digits at all
            }
            // Optional exponent (only if at least one digit precedes it).
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let save = i;
                i += 1;
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
                let exp_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == exp_start {
                    i = save; // not a valid exponent, rewind
                }
            }
            t[..i].parse::<f64>().ok()
        }
        Value::Null => None,
    }
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

#[cfg(test)]
mod tests {
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
