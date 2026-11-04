// SPDX-License-Identifier: AGPL-3.0-only
//! Correlated subqueries: outer-reference collection, per-row resolution
//! of scalar/EXISTS/IN subqueries and WHERE rebinding.

use super::*;

impl SqlEngine {
    /// A column counts as "own" when it exists in the schema of one of the
    /// statement's tables; unknown tables (CTEs) treat all their WHERE
    /// columns as own columns to avoid false correlation.
    pub(crate) fn stmt_outer_refs(&self, stmt: &Stmt) -> Result<Vec<String>, String> {
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
    pub(crate) fn resolve_expr_for_row(
        &self,
        e: &Expr,
        row: &BTreeMap<String, Value>,
    ) -> Result<Expr, String> {
        Ok(match e {
            Expr::Like {
                operand,
                pattern,
                negated,
            } => Expr::Like {
                operand: Box::new(self.resolve_expr_for_row(operand, row)?),
                pattern: pattern.clone(),
                negated: *negated,
            },
            Expr::Fn { name, args } => Expr::Fn {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|a| self.resolve_expr_for_row(a, row))
                    .collect::<Result<Vec<_>, _>>()?,
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
                let mut p = SParser::new(query)?;
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
            Expr::IsNull(inner) => Expr::IsNull(Box::new(self.resolve_expr_for_row(inner, row)?)),
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
    pub(crate) fn bind_outer_refs_select(
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
                Expr::And(a, b) => Expr::And(
                    Box::new(rewrite(a, row, refs)),
                    Box::new(rewrite(b, row, refs)),
                ),
                Expr::Or(a, b) => Expr::Or(
                    Box::new(rewrite(a, row, refs)),
                    Box::new(rewrite(b, row, refs)),
                ),
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
    pub(crate) fn eval_scalar_sub_for_row(
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
        Ok(res.rows.first().and_then(|r| r.values().next().cloned()))
    }
}
