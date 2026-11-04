// SPDX-License-Identifier: AGPL-3.0-only
//! SELECT execution: candidate fetch, joins, aggregation, CTE and UNION
//! materialisation, scalar subqueries, outer-reference binding and window
//! functions.

// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::*;
use crate::expr::Expr;
use crate::relational::{encode_index_value, encode_ordered_value, Value};
use crate::sql_engine::parser::SParser;
use std::collections::BTreeMap;

impl SqlEngine {
    pub(crate) fn exec_cte(
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
            let (cols, rows, _cplan, _) = self.exec_stmt(def.clone(), &mut None)?;
            let mut store = self.cte_store.lock().unwrap();
            if store.contains_key(name) {
                return Err(format!("SQL: duplicate CTE name '{}'", name));
            }
            plan.push_str(&format!("{}[{} rows] ", name, rows.len()));
            store.insert(name.clone(), (cols, rows));
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
    pub(crate) fn exec_union(
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
            Some(format!(
                "{} row(s) after UNION {}",
                n,
                if all { "ALL" } else { "DISTINCT" }
            )),
        ))
    }

    /// Run a SELECT-shaped statement (Select or nested Union/CTE) and return
    /// its result tuple. Used by UNION and EXPLAIN.
    pub(crate) fn exec_select_stmt(
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

    pub(crate) fn exec_select(
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
                    let schema = self
                        .get_schema(&table)
                        .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
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
        // evaluate a single empty row. MySQL DUAL is a virtual one-row table.
        if !from.is_empty()
            && !from.eq_ignore_ascii_case("DUAL")
            && self.get_schema(from).is_none()
            && !self.cte_contains(from)
        {
            return Err(format!(
                "SQL: relation '{}' does not exist (CREATE TABLE first)",
                from
            ));
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
        // A FROM-less SELECT sources exactly one (empty) row; MySQL DUAL is
        // the same virtual one-row table (`SELECT 1 FROM DUAL`).
        let (mut current, mut plan) = if from.is_empty() || from.eq_ignore_ascii_case("DUAL") {
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
                // MySQL allows either orientation in ON (`a.x = b.y` or
                // `b.y = a.x`); orient the pair by where the columns actually
                // live. MediaWiki joins write the joined table first:
                // `JOIN mw_actor user_actor ON user_actor.actor_user = user_id`
                // — the reversed order used to produce an empty result.
                let left_keys: std::collections::HashSet<String> = current
                    .first()
                    .map(|r| r.keys().cloned().collect())
                    .unwrap_or_default();
                let right_keys: std::collections::HashSet<String> = right
                    .first()
                    .map(|r| r.keys().cloned().collect())
                    .unwrap_or_default();
                let (lc, rc) = if left_keys.is_empty() || right_keys.is_empty() {
                    (j.left_col.clone(), j.right_col.clone())
                } else {
                    fn bare(c: &str) -> &str {
                        c.rsplit('.').next().unwrap_or(c)
                    }
                    let (lc, rc) = (bare(&j.left_col), bare(&j.right_col));
                    let l_ok = left_keys.contains(lc) && right_keys.contains(rc);
                    let r_ok = left_keys.contains(rc) && right_keys.contains(lc);
                    if !l_ok && r_ok {
                        (rc.to_string(), lc.to_string())
                    } else {
                        (lc.to_string(), rc.to_string())
                    }
                };
                match j.join_type {
                    JoinType::Inner => hash_join(&current, &right, &lc, &rc),
                    JoinType::Left => hash_join_left(&current, &right, &lc, &rc),
                    // RIGHT JOIN == LEFT JOIN with the sides swapped (the row
                    // model is single-namespace, so column naming is unchanged).
                    JoinType::Right => hash_join_left(&right, &current, &rc, &lc),
                    // FULL OUTER JOIN: left join + every right row that never
                    // matched, padded with NULLs on the left columns.
                    JoinType::Full => hash_join_full(&current, &right, &lc, &rc),
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
                SelectItem::ScalarSub {
                    stmt, canonical, ..
                } => Some((canonical, stmt.as_ref())),
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
            self.stmt_outer_refs(s)
                .map(|r| !r.is_empty())
                .unwrap_or(false)
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
                        SelectItem::ScalarSub {
                            stmt,
                            canonical,
                            rendered,
                        } => Some((canonical.clone(), rendered.clone(), stmt.as_ref())),
                        _ => None,
                    })
                    .filter(|(_, _, s)| {
                        self.stmt_outer_refs(s)
                            .map(|r| !r.is_empty())
                            .unwrap_or(false)
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
                        SelectItem::ScalarSub {
                            canonical,
                            rendered,
                            ..
                        } => {
                            specs.push(format!("{} AS {}", canonical, rendered));
                            columns.push((canonical.clone(), rendered.clone()));
                        }
                        SelectItem::Window {
                            canonical,
                            rendered,
                            ..
                        } => {
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
                        SelectItem::Window { rendered, .. }
                        | SelectItem::ScalarSub { rendered, .. } => {
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
                // `SELECT *` over a derived table / CTE: the materialised
                // body's output columns (declaration order) — works even for
                // zero-row bodies, where the row-key fallback is empty.
                if let Some((cte_cols, _)) = self.cte_store.lock().unwrap().get(from) {
                    cols.extend(cte_cols.iter().cloned());
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
    pub(crate) fn cte_contains(&self, name: &str) -> bool {
        self.cte_store.lock().unwrap().contains_key(name)
    }

    /// Candidate rows for a FROM table: PK point lookup > secondary index
    /// (eq / IN-list / range) > SeqScan. The full WHERE is always re-applied
    /// afterwards, so a conservative candidate set only costs performance.
    pub(crate) fn fetch_candidates(
        &self,
        from: &str,
        where_clause: Option<&Expr>,
    ) -> Result<(Vec<BTreeMap<String, Value>>, String), String> {
        let schema = self.get_schema(from);
        if let Some(schema) = &schema {
            if let Some(lits) =
                where_clause.and_then(|e| Self::pk_eq_literals(e, &Self::pk_cols(&schema)))
            {
                let mut lits = lits;
                // CMS edition: PointGet keys must be encoded with the stored
                // PK representation (read-side coercion mirrors write-side).
                #[cfg(feature = "cms")]
                for lit in lits.iter_mut() {
                    if let Some(ct) = schema.columns.get(schema.primary_locus_column.as_str()) {
                        lit[0] = Value::coerce_for_column(ct, lit[0].clone());
                    }
                }
                let mut out = Vec::with_capacity(lits.len());
                for lit in &lits {
                    if let Some(row) = self.read_row_point_pk(from, &lit)? {
                        out.push(row);
                    }
                }
                let plan = if lits.len() == 1 && lits[0].len() == 1 {
                    format!("PointGet({} = {})", from, lits[0][0])
                } else {
                    format!(
                        "PointGet({} IN ({}))",
                        from,
                        lits.iter()
                            .map(|t| t
                                .iter()
                                .map(|v| v.to_string())
                                .collect::<Vec<_>>()
                                .join(":"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return Ok((out, plan));
            }
            if let Some(expr) = where_clause {
                if let Some((col, mut access)) = index_access_for(schema, expr) {
                    // CMS edition: lookup literals coerce to the column type
                    // so the encoded scan key matches the stored one.
                    #[cfg(feature = "cms")]
                    if let Some(ct) = schema.columns.get(col.as_str()) {
                        access.coerce_values(ct);
                    }
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
    pub(crate) fn index_candidate_keys(
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

    /// GROUP BY + aggregates + HAVING over an already-filtered row set.
    /// Aggregates are computed under their canonical name (`COUNT(*)`),
    /// HAVING evaluates against those canonical keys, then each surviving
    /// row is renamed to the projection's output alias before returning.
    pub(crate) fn aggregate_rows(
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
