// SPDX-License-Identifier: AGPL-3.0-only
//! Subquery materialisation: IN-subqueries → IN-lists, uncorrelated
//! scalar/EXISTS → literals; correlated nodes resolved per row later.

use super::*;

impl SqlEngine {
    /// Replace IN-subquery nodes with materialised IN-lists; resolve
    /// uncorrelated scalar subqueries / EXISTS to literals (v3.6). Correlated
    /// nodes stay in place and are resolved per row by the executor.
    pub(crate) fn materialize_subqueries(
        &self,
        expr: Option<Expr>,
    ) -> Result<Option<Expr>, String> {
        fn walk(sql: &SqlEngine, e: &Expr) -> Result<Expr, String> {
            match e {
                Expr::Like {
                    operand,
                    pattern,
                    negated,
                } => Ok(Expr::Like {
                    operand: Box::new(walk(sql, operand)?),
                    pattern: pattern.clone(),
                    negated: *negated,
                }),
                Expr::Fn { name, args } => Ok(Expr::Fn {
                    name: name.clone(),
                    args: args
                        .iter()
                        .map(|a| walk(sql, a))
                        .collect::<Result<Vec<_>, _>>()?,
                }),
                Expr::InSubquery { column, query } => {
                    // Correlated? Render and re-parse the subquery, then ask
                    // for outer references. Correlated nodes resolve per row.
                    let mut p = SParser::new(query)?;
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

}
