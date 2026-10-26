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
    /// `col IN (v1, v2, ...)` — evaluated here; NULL semantics: no match
    /// when the column value is NULL or the list contains NULL only.
    InList {
        column: String,
        values: Vec<Value>,
    },
    /// `col IN (SELECT ...)` — SQL-only node. Must be materialised into an
    /// InList by the executor before evaluation (eval returns NULL).
    InSubquery {
        column: String,
        /// Rendered sub-SELECT, re-executed by the SQL engine.
        query: String,
    },
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
            Expr::InList { column, values } => {
                let v = row.get(column).cloned()?;
                if v.is_null() {
                    return Some(Value::Null);
                }
                Some(Value::Int(
                    values
                        .iter()
                        .any(|c| c.compare(&v) == std::cmp::Ordering::Equal)
                        as i64,
                ))
            }
            Expr::InSubquery { .. } => Some(Value::Null),
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
            Expr::InList { column, values } => format!(
                "{} IN ({})",
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
            Expr::Cmp { op, left, right } => {
                format!("{} {} {}", left.describe(), op.as_str(), right.describe())
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
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.collect_columns(out);
                b.collect_columns(out);
            }
            Expr::Cmp { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
        }
    }
}

/// Numeric arithmetic with Int/Float promotion. Division always produces a
/// Float; x / 0 (and x / 0.0) produces NULL instead of panicking.
fn arith(op: ArithOp, l: &Value, r: &Value) -> Option<Value> {
    fn num(v: &Value) -> Option<(f64, bool)> {
        match v {
            Value::Int(i) => Some((*i as f64, true)),
            Value::Float(f) => Some((*f, false)),
            _ => None,
        }
    }
    let (a, ai) = num(l)?;
    let (b, bi) = num(r)?;
    if op == ArithOp::Div && b == 0.0 {
        return Some(Value::Null);
    }
    Some(match op {
        ArithOp::Add if ai && bi => Value::Int((a as i64).wrapping_add(b as i64)),
        ArithOp::Sub if ai && bi => Value::Int((a as i64).wrapping_sub(b as i64)),
        ArithOp::Mul if ai && bi => Value::Int((a as i64).wrapping_mul(b as i64)),
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
}
