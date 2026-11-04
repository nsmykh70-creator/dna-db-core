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

        // Expression aggregates (`SUM(CASE WHEN … END)`) materialise their
        // argument expressions into the rows BEFORE aggregation, so the
        // aggregate reads a real column (same mechanism as ORDER BY keys).
        for item in projection {
            let mk = match item {
                SelectItem::OrderKeyOnly { expr, rendered } => Some((expr, rendered)),
                SelectItem::AggOverExpr {
                    expr, arg_name, ..
                } => Some((expr, arg_name)),
                _ => None,
            };
            if let Some((expr, rendered)) = mk {
                for row in current.iter_mut() {
                    if !row.contains_key(rendered) {
                        let v = expr.eval(row).unwrap_or(Value::Null);
                        row.insert(rendered.clone(), v);
                    }
                }
            }
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
                        SelectItem::AggOverExpr { rendered, .. } => {
                            specs.push(rendered.clone())
                        }
                        // CMS edition: sort keys emit no output column.
                        SelectItem::OrderKeyOnly { .. }
                        | SelectItem::AliasStar(_)
                        | SelectItem::Star => {}
                    }
                }
                // A computed expression may only reference real columns —
                // resolved against the JOIN universe (FROM + every JOIN
                // table), because rows are flattened into one namespace
                // after the joins: Joomla's CASE projections reference
                // `c.published` / `ua.name` from joined tables. Plain
                // columns keep the legacy behaviour of being skipped when
                // absent; a column in NO reachable schema is errno 1054.
                if !computed.is_empty() {
                    let mut universe: Vec<String> = vec![from.to_string()];
                    universe.extend(joins.iter().map(|j| j.table.clone()));
                    let mut refs = Vec::new();
                    for (_, expr) in &computed {
                        expr.collect_columns(&mut refs);
                    }
                    let mut any_schema = false;
                    for c in refs {
                        let mut found = false;
                        for t in &universe {
                            if let Some(schema) = self.get_schema(t) {
                                any_schema = true;
                                if schema.columns.contains_key(&c)
                                    || schema
                                        .columns
                                        .keys()
                                        .any(|k| k.eq_ignore_ascii_case(&c))
                                {
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found && any_schema {
                            return Err(format!(
                                "SQL: unknown column '{}' in projection",
                                c
                            ));
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
                        SelectItem::AggOverExpr { rendered, .. } => {
                            if !keep.contains(rendered) {
                                keep.push(rendered.clone());
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
                    | SelectItem::AggOverExpr { rendered, .. }
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
}

mod aggregate;
mod cte;
mod index;
mod outer_refs;
mod subquery;
