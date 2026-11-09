// SPDX-License-Identifier: AGPL-3.0-only
//! SQL → DnaQL transpilation.
//!
//! Converts a parsed SQL statement (see [`crate::sql_engine`]) into the
//! equivalent DnaQL query, so SQL clients can be migrated to the native
//! pipeline/TX dialect without changing semantics.
//!
//! Statement coverage (v4.3 — DnaQL grammar parity track):
//! * `SELECT` — column/computed projections (`EMIT item AS alias`), `WHERE`
//!   (incl. `NOT` / `LIKE` / `IN`), INNER/LEFT `JOIN` (`|> JOIN` stages),
//!   `GROUP BY` + aggregates + `HAVING` (`|> GROUP`), `ORDER BY` (`SORT`,
//!   keys emitted least-significant-first so the stable final sort matches
//!   SQL's multi-key order; computed keys become `SORT <expr>`), `OFFSET`
//!   (`|> SKIP n`) and `LIMIT`. Scalar subqueries, window functions and
//!   RIGHT/FULL joins remain SQL-only and are rejected with a message
//!   naming the construct (RIGHT/FULL: swap the sides into LEFT JOIN).
//! * `INSERT` — every row becomes `table:<pk> += { ... }` inside one atomic
//!   `TX { ... }` block (DnaQL addresses rows by integer locus). UPSERT
//!   tails (`ON CONFLICT`, `ON DUPLICATE KEY UPDATE`, `INSERT IGNORE`) have
//!   no DnaQL TX grammar and are rejected instead of silently dropped.
//! * `UPDATE` / `DELETE` — require a strict `WHERE <pk> = <literal>`
//!   predicate (DnaQL mutations are locus-addressed); anything else is
//!   rejected rather than silently widened.
//! * `CREATE TABLE` → `CREATE CHROMOSOME` (column types, `PRIMARY KEY`,
//!   `FOREIGN KEY ... ON DELETE ...` clauses are passed through verbatim);
//!   `DROP TABLE` → `DROP CHROMOSOME`.
//! * `CREATE/DROP INDEX`, `ALTER TABLE ADD/DROP COLUMN` → the DnaQL DDL
//!   passthrough statements (`CREATE INDEX`, `ALTER CHROMOSOME`, ...) that
//!   execute on the shared SQL engine.
//! * `SHOW TABLES` → `SHOW CHROMOSOMES` (same schema cache, same list).
//! * `EXPLAIN`, `WITH` (CTE), `UNION`, `INSERT..SELECT`, `SHOW STORAGE`,
//!   `BEGIN/COMMIT/ROLLBACK` — SQL-only, rejected with the DnaQL-side
//!   guidance in the message.
//!
//! Literals: floats always keep a decimal point, strings are quoted with
//! `'` (or `"` when the value contains a single quote — DnaQL has no escape
//! sequences, so a value containing both quote types is rejected).

use crate::dna_ql::float_token;
use crate::expr::{CmpOp, Expr};
use crate::relational::{ChromosomeTable, ColumnType, Value};
use crate::sql_engine::stmt::{JoinType, SelectItem};
use crate::sql_engine::Stmt;

/// Convert one parsed SQL statement into a DnaQL query (terminated with `;`).
///
/// `schema_of` resolves a table name to its schema; SELECT/INSERT/UPDATE/
/// DELETE and CREATE TABLE need the schema, DROP does not.
pub(crate) fn transpile(
    stmt: &Stmt,
    schema_of: &dyn Fn(&str) -> Option<ChromosomeTable>,
) -> Result<String, String> {
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
        } => {
            for j in joins {
                if matches!(j.join_type, JoinType::Right | JoinType::Full) {
                    return Err(format!(
                        "SQL→DnaQL: {} JOIN over '{}' is not expressible in DnaQL; rewrite it \
                         as LEFT JOIN with the sides swapped (the SQL engine executes it \
                         exactly that way)",
                        if matches!(j.join_type, JoinType::Full) {
                            "FULL"
                        } else {
                            "RIGHT"
                        },
                        j.table
                    ));
                }
            }
            let schema = schema_of(from)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", from))?;

            // Aggregate shape: any aggregate projection, GROUP BY / HAVING
            // or an aggregate ORDER BY key puts the pipeline into the
            // GROUP stage form.
            let aggs = collect_aggs(projection)?;
            let has_aggs =
                !aggs.is_empty() || !group_by.is_empty() || having.is_some();

            // Alias stripping: SQL resolves `u.col` against the FROM table
            // (single-namespace row model); DnaQL rows carry bare fields for
            // the FROM side and `<table>.field` for joined rows.
            let strip = |e: &Expr| strip_from_alias(e, from_alias.as_deref());

            let mut parts = vec![format!("FROM {}", from)];

            // JOINs: INNER/LEFT equi-join stages. The ON pair must be
            // oriented FROM-side = JOINED-side (the reversed spelling is
            // rewritten, not rejected); non-equi conjuncts ride in
            // `AND ...` — they match DURING the join, not after.
            let mut joined_schemas: Vec<ChromosomeTable> = Vec::new();
            for j in joins {
                let joined_name = j.alias.as_deref().unwrap_or(j.table.as_str());
                let joined_schema = schema_of(&j.table);
                let (l_raw, r_raw) = orient_on_pair(
                    &j.left_col,
                    &j.right_col,
                    joined_name,
                    &schema,
                    joined_schema.as_ref(),
                );
                let left = bare_column(&l_raw, from_alias.as_deref());
                let right = bare_column(&r_raw, None);
                let mut stage = format!(
                    "|> {}JOIN {} ON {} = {}",
                    if matches!(j.join_type, JoinType::Left) {
                        "LEFT "
                    } else {
                        ""
                    },
                    j.table,
                    left,
                    right
                );
                for e in &j.on_extra {
                    stage.push_str(&format!(" AND {}", expr(&strip(e))?));
                }
                parts.push(stage);
                if let Some(js) = joined_schema {
                    joined_schemas.push(js);
                }
            }

            if let Some(w) = where_clause {
                parts.push(format!("|> WHERE {}", expr(&strip(w))?));
            }

            let group_cols: std::collections::BTreeSet<String> =
                group_by.iter().cloned().collect();

            if has_aggs {
                // GROUP stage: group columns first, then every aggregate.
                // Aggregates land under their canonical SQL keys (and their
                // output aliases), so HAVING / ORDER BY references resolve
                // exactly like they do inside the SQL executor.
                let mut items: Vec<String> = group_by.clone();
                for a in &aggs {
                    if a.distinct {
                        return Err(format!(
                            "SQL→DnaQL: DISTINCT aggregate '{}' has no DnaQL GROUP stage; \
                             run the aggregation as SQL",
                            a.canonical
                        ));
                    }
                    match &a.arg {
                        Some(arg) => items.push(format!("{}({})", a.func, arg)),
                        None => items.push(format!("{}(*)", a.func)),
                    }
                    // Output alias is a plain identifier when the SQL text
                    // carried `AS n`; rows then carry both keys. (The AS
                    // belongs to the aggregate item, not a list element.)
                    if a.output != a.canonical {
                        let last = items.len() - 1;
                        items[last] = format!("{} AS {}", items[last], a.output);
                    }
                }
                let mut stage = format!("|> GROUP {}", items.join(", "));
                if let Some(h) = having {
                    stage.push_str(&format!(" HAVING {}", expr(&strip(h))?));
                }
                parts.push(stage);
            }

            // Multi-key ORDER BY: sort keys least-significant first — each
            // SORT is a stable sort, so the last applied (most significant
            // key) ends up primary, exactly like SQL's single multi-key sort.
            for (key, asc) in order_by.iter().rev() {
                let dir = if *asc { "ASC" } else { "DESC" };
                if has_aggs {
                    let known = group_cols.contains(key)
                        || aggs.iter().any(|a| a.canonical == *key || a.output == *key);
                    if !known {
                        return Err(format!(
                            "SQL→DnaQL: ORDER BY '{}' must name a group column or aggregate",
                            key
                        ));
                    }
                    parts.push(format!("|> SORT {} {}", key, dir));
                } else if key.contains('(') {
                    // Computed sort key: the parser materialises it as an
                    // OrderKeyOnly projection item — reuse its expression.
                    match projection.iter().find(|i| {
                        matches!(i, SelectItem::OrderKeyOnly { rendered, .. } if rendered == key)
                            || matches!(i, SelectItem::Agg { canonical, .. } if canonical == key)
                    }) {
                        Some(SelectItem::OrderKeyOnly { expr: inner, .. }) => {
                            parts.push(format!("|> SORT {} {}", expr(&strip(inner))?, dir));
                        }
                        _ => {
                            return Err(format!(
                                "SQL→DnaQL: sorting by aggregate '{}' is only expressible \
                                 with GROUP BY in DnaQL",
                                key
                            ));
                        }
                    }
                } else {
                    let col = bare_column(key, from_alias.as_deref());
                    let known = schema.columns.contains_key(&col)
                        || joined_schemas
                            .iter()
                            .any(|js| js.columns.contains_key(&col));
                    if !known {
                        return Err(format!(
                            "SQL→DnaQL: ORDER BY column '{}' is not a column of '{}'",
                            col, from
                        ));
                    }
                    parts.push(format!("|> SORT {} {}", col, dir));
                }
            }

            if *offset > 0 {
                parts.push(format!("|> SKIP {}", offset));
            }
            if let Some(n) = limit {
                parts.push(format!("|> LIMIT {}", n));
            }
            parts.push(format!(
                "|> EMIT {}",
                emit_items(
                    projection,
                    &schema,
                    &joined_schemas,
                    &group_cols,
                    &aggs,
                    has_aggs,
                    from_alias.as_deref()
                )?
            ));
            Ok(format!("{};", parts.join(" ")))
        }

        Stmt::Insert {
            table,
            rows,
            on_conflict,
            ignore,
        } => {
            // Honest coverage: DnaQL TX has no upsert grammar. Silently
            // dropping an ON CONFLICT / INSERT IGNORE tail would change
            // semantics (conflicting rows would abort the TX instead of
            // being skipped/updated).
            if on_conflict.is_some() {
                return Err(
                    "SQL→DnaQL: ON CONFLICT / ON DUPLICATE KEY UPDATE has no DnaQL TX \
                     equivalent; run the statement as SQL"
                        .into(),
                );
            }
            if *ignore {
                return Err(
                    "SQL→DnaQL: INSERT IGNORE has no DnaQL TX equivalent; run the statement \
                     as SQL"
                        .into(),
                );
            }
            let schema = schema_of(table)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", table))?;
            let pk = &schema.primary_locus_column;
            let mut stmts = Vec::with_capacity(rows.len());
            for row in rows {
                let pk_val = row
                    .iter()
                    .find(|(k, _)| k == pk)
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| {
                        format!(
                            "SQL→DnaQL: INSERT row is missing primary key column '{}'",
                            pk
                        )
                    })?;
                let locus = match pk_val {
                    Value::Int(i) if i >= 0 => i,
                    other => {
                        return Err(format!(
                            "SQL→DnaQL: DnaQL addresses loci by a non-negative integer primary \
                             key, but '{}' = '{}'; non-integer primary keys cannot be written \
                             through DnaQL TX",
                            pk, other
                        ))
                    }
                };
                let fields = row
                    .iter()
                    .map(|(k, v)| Ok(format!("{}: {}", k, literal(v)?)))
                    .collect::<Result<Vec<_>, String>>()?;
                stmts.push(format!(
                    "{}:{} += {{ {} }}",
                    table,
                    locus,
                    fields.join(", ")
                ));
            }
            if stmts.is_empty() {
                return Err("SQL→DnaQL: INSERT has no rows".into());
            }
            Ok(format!("TX {{ {} }};", stmts.join("; ")))
        }

        Stmt::InsertSelect { .. } => {
            Err("SQL→DnaQL: INSERT..SELECT is not expressible in DnaQL (CMS gate handles it before transpile)".into())
        }


        Stmt::Update {
            table,
            assignments,
            where_clause,
        } => {
            let schema = schema_of(table)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", table))?;
            let locus = pk_locus(where_clause, &schema.primary_locus_column, table)?;
            let stmts = assignments
                .iter()
                .map(|(col, e)| match e {
                    // Pipeline form carries literals only; expression rhs
                    // (`SET stock = stock - 1`) is engine-executable but has
                    // no DnaQL pipeline rendering.
                    Expr::Literal(v) => {
                        Ok(format!("{}:{}.{} = {}", table, locus, col, literal(v)?))
                    }
                    _ => Err(format!(
                        "SQL→DnaQL: UPDATE SET '{}' must be a literal (expression rhs is engine-only)",
                        col
                    )),
                })
                .collect::<Result<Vec<_>, String>>()?;
            if stmts.is_empty() {
                return Err("SQL→DnaQL: UPDATE has no assignments".into());
            }
            Ok(format!("TX {{ {} }};", stmts.join("; ")))
        }

        Stmt::Delete {
            table,
            where_clause,
        } => {
            let schema = schema_of(table)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", table))?;
            let locus = pk_locus(where_clause, &schema.primary_locus_column, table)?;
            Ok(format!("TX {{ DROP LOCUS {}:{}; }};", table, locus))
        }

        Stmt::CreateTable {
            table,
            if_not_exists,
        } => {
            if !table.secondary_indexes.is_empty() {
                return Err(
                    "SQL→DnaQL: this table has CREATE INDEX definitions; build them separately \
                     with SQL (DnaQL has no CREATE INDEX)"
                        .into(),
                );
            }
            let mut clauses: Vec<String> = table
                .columns
                .iter()
                .map(|(name, ty)| Ok(format!("{} {}", name, type_name(ty)?)))
                .collect::<Result<Vec<_>, String>>()?;
            clauses.push(format!("PRIMARY KEY ({})", table.primary_locus_column));
            for fk in &table.foreign_keys {
                clauses.push(format!(
                    "FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {}",
                    fk.column,
                    fk.target_chromosome,
                    fk.target_locus_column,
                    if fk.on_delete_cascade {
                        "CASCADE"
                    } else {
                        "RESTRICT"
                    }
                ));
            }
            let nie = if *if_not_exists { "IF NOT EXISTS " } else { "" };
            Ok(format!(
                "CREATE CHROMOSOME {}{} ({});",
                nie,
                table.name,
                clauses.join(", ")
            ))
        }

        Stmt::DropTable { table, if_exists } => {
            let ie = if *if_exists { " IF EXISTS" } else { "" };
            Ok(format!("DROP CHROMOSOME {}{};", table, ie))
        }

        // v4.3: index / column DDL — DnaQL renders the passthrough forms
        // that execute on the shared SQL engine (same path CREATE
        // CHROMOSOME takes), so both consoles accept the full DDL set.
        Stmt::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        } => {
            let mut q = String::from("CREATE ");
            if *unique {
                q.push_str("UNIQUE ");
            }
            q.push_str("INDEX");
            if *if_not_exists {
                q.push_str(" IF NOT EXISTS");
            }
            q.push_str(&format!(" {} ON {} ({});", name, table, column));
            Ok(q)
        }
        Stmt::DropIndex { name, if_exists, table } => {
            let mut q = format!("DROP INDEX{} {}", if *if_exists { " IF EXISTS" } else { "" }, name);
            if let Some(t) = table {
                q.push_str(&format!(" ON {}", t));
            }
            Ok(format!("{};", q))
        }
        Stmt::AddColumn {
            table,
            column,
            col_type,
            col_default,
        } => {
            let mut q = format!(
                "ALTER CHROMOSOME {} ADD COLUMN {} {}",
                table,
                column,
                type_name(col_type)?
            );
            if let Some(d) = col_default {
                q.push_str(" DEFAULT ");
                q.push_str(&render_default(d));
            }
            Ok(format!("{};", q))
        }
        Stmt::DropColumn { table, column } => {
            Ok(format!("ALTER CHROMOSOME {} DROP COLUMN {};", table, column))
        }
        Stmt::Explain(_) => Err(
            "SQL→DnaQL: EXPLAIN is SQL-only — the equivalent DnaQL plan is reported in the \
             execution note of every DnaQL result"
                .into(),
        ),
        // v3.6 statements with no DnaQL equivalent.
        Stmt::Union { .. } => Err(
            "SQL→DnaQL: UNION is not expressible in DnaQL; run each SELECT separately"
                .into(),
        ),
        Stmt::Cte { .. } => Err(
            "SQL→DnaQL: WITH (CTE) is not expressible in DnaQL; materialise each CTE separately"
                .into(),
        ),
        Stmt::ShowTables => Ok("SHOW CHROMOSOMES;".to_string()),
        Stmt::ShowStorage { table, .. } => Err(format!(
            "SQL→DnaQL: SHOW STORAGE is SQL-only; the DnaQL equivalent for one locus is \
             ANALYZE LOCUS <table>:<pk> |> STORAGE_MAP() (table '{}')",
            table
        )),
        Stmt::Tx { action } => Err(format!(
            "SQL→DnaQL: {:?} is SQL-only (DnaQL wraps writes in TX {{ ... }} blocks)",
            action
        )),
    }
}

// ---------------------------------------------------------------------------
// SELECT helpers
// ---------------------------------------------------------------------------

/// One aggregate of a grouped SELECT, prepared for the DnaQL GROUP stage.
struct AggSql {
    /// Canonical SQL rendering (`COUNT(*)`, `SUM(balance + 1)`) — the key
    /// HAVING / ORDER BY references resolve under.
    canonical: String,
    /// Output column name (alias if given, else the canonical).
    output: String,
    /// Aggregate function name as DnaQL spells it (same UPPER_CASE set).
    func: &'static str,
    /// Argument rendered in DnaQL expression syntax (describe style, so the
    /// recomputed canonical key matches); None = COUNT(*).
    arg: Option<String>,
    /// DISTINCT aggregates have no DnaQL GROUP equivalent.
    distinct: bool,
}

fn agg_func_name(f: crate::sql_engine::stmt::AggFunc) -> &'static str {
    match f {
        crate::sql_engine::stmt::AggFunc::Count => "COUNT",
        crate::sql_engine::stmt::AggFunc::Sum => "SUM",
        crate::sql_engine::stmt::AggFunc::Avg => "AVG",
        crate::sql_engine::stmt::AggFunc::Min => "MIN",
        crate::sql_engine::stmt::AggFunc::Max => "MAX",
    }
}

/// Collect the aggregates of a grouped projection ( Agg / AggOverExpr items).
fn collect_aggs(projection: &[SelectItem]) -> Result<Vec<AggSql>, String> {
    let mut out = Vec::new();
    for item in projection {
        match item {
            SelectItem::Agg {
                func,
                arg,
                rendered,
                canonical,
            } => out.push(AggSql {
                canonical: canonical.clone(),
                output: rendered.clone(),
                func: agg_func_name(*func),
                arg: arg.clone(),
                distinct: canonical.contains("(DISTINCT"),
            }),
            SelectItem::AggOverExpr {
                expr,
                func,
                rendered,
                canonical,
                ..
            } => out.push(AggSql {
                canonical: canonical.clone(),
                output: rendered.clone(),
                func: agg_func_name(*func),
                arg: Some(expr.describe()),
                distinct: canonical.contains("(DISTINCT"),
            }),
            _ => {}
        }
    }
    Ok(out)
}

/// `EMIT` item list for a projection, validated against the schema.
fn emit_items(
    projection: &[SelectItem],
    schema: &ChromosomeTable,
    joined_schemas: &[ChromosomeTable],
    group_cols: &std::collections::BTreeSet<String>,
    aggs: &[AggSql],
    has_aggs: bool,
    from_alias: Option<&str>,
) -> Result<String, String> {
    if projection.is_empty() || matches!(projection, [SelectItem::Star]) {
        if has_aggs {
            // Grouped star: group columns + aggregate keys.
            let mut items: Vec<String> = group_cols.iter().cloned().collect();
            items.extend(aggs.iter().map(|a| a.canonical.clone()));
            return Ok(items.join(", "));
        }
        return Ok(schema
            .columns
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "));
    }
    let mut cols = Vec::new();
    for item in projection {
        match item {
            SelectItem::Star => {
                if has_aggs {
                    cols.push("*".to_string());
                } else {
                    cols.extend(schema.columns.keys().cloned());
                }
            }
            SelectItem::AliasStar(qual) => {
                return Err(format!(
                    "SQL→DnaQL: '{}' projection alias .* is not expressible in DnaQL EMIT",
                    qual
                ));
            }
            // ORDER BY-only keys are materialised for sorting and are not
            // part of the SQL output — skip them.
            SelectItem::OrderKeyOnly { .. } => {}
            // CMS edition: expression aggregates render as their canonical
            // aggregate column (same shape as plain aggregates).
            SelectItem::AggOverExpr { rendered, canonical, .. }
            | SelectItem::Agg {
                rendered, canonical, ..
            } => {
                if has_aggs {
                    if rendered == canonical {
                        cols.push(canonical.clone());
                    } else {
                        cols.push(format!("{} AS {}", canonical, rendered));
                    }
                } else {
                    return Err(format!(
                        "SQL→DnaQL: aggregate '{}' is only expressible with GROUP BY in DnaQL",
                        canonical
                    ));
                }
            }
            SelectItem::Column { source, alias } => {
                let source = bare_column(source, from_alias);
                if has_aggs && !group_cols.contains(&source) {
                    return Err(format!(
                        "SQL→DnaQL: projected column '{}' must appear in GROUP BY (MySQL \
                         only_full_group_by)",
                        source
                    ));
                }
                if !has_aggs
                    && !schema.columns.contains_key(&source)
                    && !joined_schemas
                        .iter()
                        .any(|js| js.columns.contains_key(&source))
                {
                    return Err(format!(
                        "SQL→DnaQL: projected column '{}' does not exist on '{}'",
                        source, schema.name
                    ));
                }
                match alias {
                    Some(a) if a != &source => cols.push(format!("{} AS {}", source, a)),
                    _ => cols.push(source),
                }
            }
            SelectItem::Computed { expr: inner, rendered } => {
                let text = expr(inner)?;
                // SQL names the column by its alias, else by the rendered
                // expression text — DnaQL EMIT does the same via `AS`.
                if !rendered.is_empty()
                    && rendered.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !rendered.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true)
                {
                    cols.push(format!("{} AS {}", text, rendered));
                } else {
                    cols.push(text);
                }
            }
            SelectItem::ScalarSub { rendered, .. } => {
                return Err(format!(
                    "SQL→DnaQL: scalar subquery '{}' is not expressible in DnaQL EMIT",
                    rendered
                ));
            }
            SelectItem::Window { canonical, .. } => {
                return Err(format!(
                    "SQL→DnaQL: window function '{}' is not expressible in DnaQL EMIT",
                    canonical
                ));
            }
        }
    }
    cols.dedup();
    Ok(cols.join(", "))
}

/// Strip the FROM-table alias qualifier from every column of an expression:
/// `u.balance` → `balance` when the alias is `u`. Joined-table qualifiers
/// (`o.user_id`) are kept — DnaQL merged rows carry them qualified.
fn strip_from_alias(e: &Expr, alias: Option<&str>) -> Expr {
    let Some(alias) = alias else {
        return e.clone();
    };
    match e {
        Expr::Column(c) => Expr::Column(bare_column(c, Some(alias))),
        Expr::Not(inner) => Expr::Not(Box::new(strip_from_alias(inner, Some(alias)))),
        Expr::And(a, b) => Expr::And(
            Box::new(strip_from_alias(a, Some(alias))),
            Box::new(strip_from_alias(b, Some(alias))),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(strip_from_alias(a, Some(alias))),
            Box::new(strip_from_alias(b, Some(alias))),
        ),
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(strip_from_alias(left, Some(alias))),
            right: Box::new(strip_from_alias(right, Some(alias))),
        },
        Expr::IsNull(inner) => Expr::IsNull(Box::new(strip_from_alias(inner, Some(alias)))),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(strip_from_alias(inner, Some(alias)))),
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(strip_from_alias(left, Some(alias))),
            right: Box::new(strip_from_alias(right, Some(alias))),
        },
        Expr::InList { column, values, negated } => Expr::InList {
            column: bare_column(column, Some(alias)),
            values: values.clone(),
            negated: *negated,
        },
        Expr::Like {
            operand,
            pattern,
            negated,
        } => Expr::Like {
            operand: Box::new(strip_from_alias(operand, Some(alias))),
            pattern: pattern.clone(),
            negated: *negated,
        },
        Expr::Cast { inner, ty } => Expr::Cast {
            inner: Box::new(strip_from_alias(inner, Some(alias))),
            ty: *ty,
        },
        other => other.clone(),
    }
}

/// Bare column name: strip a leading `<qualifier>.` — the FROM alias when
/// resolving SQL alias-qualified references, any qualifier for join-side
/// columns (DnaQL merged rows carry joined fields both qualified and bare).
fn bare_column(col: &str, from_alias: Option<&str>) -> String {
    match col.split_once('.') {
        Some((qual, rest)) if Some(qual) == from_alias || from_alias.is_none() => {
            rest.to_string()
        }
        _ => col.to_string(),
    }
}

/// Orient an ON pair as (FROM-side, JOINED-side). The parser strips column
/// qualifiers (single-namespace row model), so the sides are detected from
/// the explicit qualifier first, then from schema membership — the reversed
/// spelling (`ON o.user_id = u.user_id`) is rewritten, not silently broken.
fn orient_on_pair<'a>(
    left: &'a str,
    right: &'a str,
    joined_name: &str,
    from_schema: &ChromosomeTable,
    joined_schema: Option<&ChromosomeTable>,
) -> (&'a str, &'a str) {
    let bare = |c: &str| -> String { c.split_once('.').map(|(_, r)| r.to_string()).unwrap_or_else(|| c.to_string()) };
    if let Some((q, _)) = left.split_once('.') {
        if q.eq_ignore_ascii_case(joined_name) {
            return (right, left);
        }
        return (left, right);
    }
    if let Some(js) = joined_schema {
        let l = bare(left);
        let in_from = from_schema.columns.contains_key(&l);
        let in_joined = js.columns.contains_key(&l);
        if !in_from && in_joined {
            return (right, left);
        }
    }
    (left, right)
}

/// Render an ADD COLUMN default: numeric/keyword values stay bare, text is
/// single-quoted (the parser stores the raw literal text without quotes).
fn render_default(d: &str) -> String {
    if d.is_empty() {
        return "''".to_string();
    }
    if d.parse::<i64>().is_ok() || d.parse::<f64>().is_ok() {
        return d.to_string();
    }
    let kw = d.eq_ignore_ascii_case("NULL") || d.eq_ignore_ascii_case("CURRENT_TIMESTAMP");
    if kw {
        return d.to_string();
    }
    format!("'{}'", d.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// WHERE helpers for locus-addressed mutations
// ---------------------------------------------------------------------------

/// Extract the single-condition `WHERE <pk> = <literal>` predicate that a
/// DnaQL TX mutation requires. Any other predicate is rejected: DnaQL
/// addresses rows by locus alone, so silently accepting `pk = 1 AND x = 2`
/// would update rows SQL would have skipped.
fn pk_locus(where_clause: &Option<Expr>, pk: &str, table: &str) -> Result<i64, String> {
    let w = where_clause.as_ref().ok_or_else(|| {
        format!(
            "SQL→DnaQL: UPDATE/DELETE on '{}' without WHERE would touch every row; DnaQL \
             requires WHERE {} = <literal>",
            table, pk
        )
    })?;
    match w {
        Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } => {
            let (col, lit) = match (&**left, &**right) {
                (Expr::Column(c), Expr::Literal(v)) => (c, v),
                (Expr::Literal(v), Expr::Column(c)) => (c, v),
                _ => {
                    return Err(format!(
                        "SQL→DnaQL: WHERE must be exactly `{} = <literal>` for a DnaQL \
                         mutation, got: {}",
                        pk,
                        crate::expr::Expr::describe(w)
                    ))
                }
            };
            if col != pk {
                return Err(format!(
                    "SQL→DnaQL: WHERE must pin the primary key '{}', got column '{}'",
                    pk, col
                ));
            }
            match lit {
                Value::Int(i) if *i >= 0 => Ok(*i),
                other => Err(format!(
                    "SQL→DnaQL: primary key value '{}' is not a non-negative integer; DnaQL \
                     loci are integer addresses",
                    other
                )),
            }
        }
        _ => Err(format!(
            "SQL→DnaQL: DnaQL mutations require a strict `WHERE {} = <literal>` predicate, \
             got: {}",
            pk,
            crate::expr::Expr::describe(w)
        )),
    }
}

// ---------------------------------------------------------------------------
// Expression rendering
// ---------------------------------------------------------------------------

/// Render an expression in DnaQL syntax (identical grammar to SQL WHERE for
/// the subset both dialects share).
fn expr(e: &Expr) -> Result<String, String> {
    match e {
        Expr::Literal(v) => literal(v),
        Expr::Column(c) => Ok(c.clone()),
        Expr::Not(inner) => Ok(format!("NOT {}", expr(inner)?)),
        Expr::And(a, b) => Ok(format!("({} AND {})", expr(a)?, expr(b)?)),
        Expr::Or(a, b) => Ok(format!("({} OR {})", expr(a)?, expr(b)?)),
        Expr::Cmp { op, left, right } => {
            Ok(format!("{} {} {}", expr(left)?, op.as_str(), expr(right)?))
        }
        Expr::IsNull(inner) => Ok(format!("{} IS NULL", expr(inner)?)),
        Expr::IsNotNull(inner) => Ok(format!("{} IS NOT NULL", expr(inner)?)),
        Expr::Arith { op, left, right } => Ok(format!(
            "({} {} {})",
            expr(left)?,
            op.as_str(),
            expr(right)?
        )),
        Expr::InList { column, values, negated } => {
            let lits = values
                .iter()
                .map(literal)
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!(
                "{}{} IN ({})",
                if *negated { "NOT " } else { "" },
                column,
                lits.join(", ")
            ))
        }
        Expr::InSubquery { column, .. } => Err(format!(
            "SQL→DnaQL: `{} IN (SELECT ...)` needs the subquery materialised first — DnaQL \
             has no subqueries; run the inner SELECT separately",
            column
        )),
        Expr::ScalarSubquery { .. } | Expr::Exists { .. } => Err(
            "SQL→DnaQL: scalar/EXISTS subqueries are not expressible in DnaQL; run the inner \
             SELECT separately"
                .into(),
        ),
        Expr::Like {
            operand,
            pattern,
            negated,
        } => Ok(format!(
            "{}{}LIKE {}",
            expr(operand)?,
            if *negated { " NOT " } else { " " },
            quote(pattern)?
        )),
        Expr::Cast { inner, ty } => Ok(format!(
            "CAST({} AS {})",
            expr(inner)?,
            ty.as_str()
        )),
        Expr::Fn { name, args } => {
            // DnaQL has no function grammar: constant-only calls are folded
            // to their value; anything referencing columns is refused with
            // the standard "not expressible in DnaQL" guidance.
            let vals: Vec<Option<Value>> = args.iter().map(|a| a.eval(&Default::default())).collect();
            if !vals.iter().any(|v| v.is_none()) {
                if let Some(v) = crate::expr::eval_for_translator(name, &vals) {
                    return literal(&v);
                }
            }
            Err(format!(
                "SQL→DnaQL: function {name}() over columns is not expressible in DnaQL;                  compute it in SQL or fold constant arguments"
            ))
        }
    }
}

/// Render a literal as a DnaQL value token.
fn literal(v: &Value) -> Result<String, String> {
    Ok(match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(format!(
                    "SQL→DnaQL: non-finite float {} has no DnaQL literal",
                    f
                ));
            }
            float_token(f)
        }
        Value::Text(s) | Value::Dna(s) => quote(s)?,
    })
}

/// Quote a string for DnaQL (which has no escape sequences): prefer `'`,
/// fall back to `"` when the value contains a single quote, and refuse a
/// value containing both quote characters.
fn quote(s: &str) -> Result<String, String> {
    if !s.contains('\'') {
        return Ok(format!("'{}'", s));
    }
    if !s.contains('"') {
        return Ok(format!("\"{}\"", s));
    }
    Err(format!(
        "SQL→DnaQL: string {:?} contains both quote types; DnaQL has no escape sequences",
        s
    ))
}

// ---------------------------------------------------------------------------
// DDL type names
// ---------------------------------------------------------------------------

fn type_name(ty: &ColumnType) -> Result<&'static str, String> {
    Ok(match ty {
        ColumnType::Int64 => "INT",
        ColumnType::Float64 => "FLOAT",
        ColumnType::Text => "TEXT",
        ColumnType::Dna2Bit => "DNA",
        ColumnType::Timestamp => "TIMESTAMP",
        ColumnType::ForeignLink { .. } => {
            return Err(
                "SQL→DnaQL: ForeignLink column types have no CREATE CHROMOSOME spelling".into(),
            )
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::engine::{Engine, EngineConfig};
    use crate::sql_engine::SqlEngine;
    use crate::wal::SyncMode;
    use std::sync::Arc;

    fn engine(name: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!("dna_s2d_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap()
    }

    fn setup(name: &str) -> Arc<SqlEngine> {
        let sql = Arc::new(SqlEngine::new(engine(name)));
        sql.execute(
            "CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, \
             PRIMARY KEY (user_id))",
        )
        .unwrap();
        sql.execute(
            "CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, \
             PRIMARY KEY (order_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id) \
             ON DELETE CASCADE)",
        )
        .unwrap();
        sql
    }

    #[test]
    fn select_where_order_limit() {
        let sql = setup("sel");
        let out = sql
            .sql_to_dnaql(
                "SELECT user_id, username FROM chr_users WHERE balance > 100.5 \
                 ORDER BY username DESC, user_id ASC LIMIT 10",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> WHERE balance > 100.5 |> SORT user_id ASC \
             |> SORT username DESC |> LIMIT 10 |> EMIT user_id, username;"
        );
    }

    #[test]
    fn select_star_uses_schema_columns() {
        let sql = setup("star");
        let out = sql.sql_to_dnaql("SELECT * FROM chr_users").unwrap();
        assert_eq!(out, "FROM chr_users |> EMIT balance, user_id, username;");
    }

    #[test]
    fn formerly_rejected_constructs_now_transpile() {
        let sql = setup("now");
        // Aggregates map onto the GROUP stage (canonical keys + alias).
        let out = sql
            .sql_to_dnaql(
                "SELECT status, COUNT(*) AS n, SUM(total_usd) FROM chr_orders \
                 GROUP BY status HAVING COUNT(*) > 1 ORDER BY n DESC LIMIT 3",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_orders |> GROUP status, COUNT(*) AS n, SUM(total_usd) \
             HAVING COUNT(*) > 1 |> SORT n DESC |> LIMIT 3 \
             |> EMIT status, COUNT(*) AS n, SUM(total_usd);"
        );
        // INNER JOIN (both ON orderings) + WHERE + OFFSET. The SQL parser
        // strips qualifiers, so the DnaQL references are bare.
        let out = sql
            .sql_to_dnaql(
                "SELECT u.username FROM chr_users u INNER JOIN chr_orders o \
                 ON u.user_id = o.user_id WHERE o.status = 'paid' \
                 ORDER BY u.username LIMIT 5 OFFSET 2",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
             |> WHERE status = 'paid' |> SORT username ASC \
             |> SKIP 2 |> LIMIT 5 |> EMIT username;"
        );
        // Reversed ON spelling is oriented, not broken.
        let out = sql
            .sql_to_dnaql(
                "SELECT username FROM chr_users u JOIN chr_orders o \
                 ON o.user_id = u.user_id",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
             |> EMIT username;"
        );
        // Computed projection + alias, LIKE / NOT in WHERE.
        let out = sql
            .sql_to_dnaql(
                "SELECT username, balance * 2 AS double_balance FROM chr_users \
                 WHERE username LIKE 'dr_%' AND NOT balance IS NULL",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> WHERE (username LIKE 'dr_%' AND NOT balance IS NULL) \
             |> EMIT username, (balance * 2) AS double_balance;"
        );
        // LEFT JOIN with a compound ON (non-equi conjunct matches during
        // the join; the parser already stripped the `o.` qualifiers).
        let out = sql
            .sql_to_dnaql(
                "SELECT username FROM chr_users u LEFT JOIN chr_orders o \
                 ON u.user_id = o.user_id AND o.status = 'paid'",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> LEFT JOIN chr_orders ON user_id = user_id \
             AND status = 'paid' |> EMIT username;"
        );
    }

    #[test]
    fn ddl_and_show_map_onto_dnaql() {
        let sql = setup("ddlmap");
        assert_eq!(
            sql.sql_to_dnaql("CREATE INDEX idx_u ON chr_users (username)")
                .unwrap(),
            "CREATE INDEX idx_u ON chr_users (username);"
        );
        assert_eq!(
            sql.sql_to_dnaql("CREATE UNIQUE INDEX IF NOT EXISTS idx_e ON chr_users (user_id)")
                .unwrap(),
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_e ON chr_users (user_id);"
        );
        assert_eq!(
            sql.sql_to_dnaql("DROP INDEX idx_u ON chr_users").unwrap(),
            "DROP INDEX idx_u ON chr_users;"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users ADD COLUMN note TEXT DEFAULT 'x'")
                .unwrap(),
            "ALTER CHROMOSOME chr_users ADD COLUMN note TEXT DEFAULT 'x';"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users DROP COLUMN note").unwrap(),
            "ALTER CHROMOSOME chr_users DROP COLUMN note;"
        );
        assert_eq!(
            sql.sql_to_dnaql("SHOW TABLES").unwrap(),
            "SHOW CHROMOSOMES;"
        );
        // Every rendered DDL statement must execute on the DnaQL engine.
        let dnaql = crate::dna_ql::DnaQlEngine::new(sql.engine.clone(), sql.clone());
        dnaql
            .execute("CREATE INDEX idx_users_name ON chr_users (username);")
            .unwrap();
        assert!(sql
            .execute("SELECT username FROM chr_users WHERE username = 'nobody'")
            .is_ok());
        dnaql.execute("SHOW CHROMOSOMES;").unwrap();
        let shown = dnaql.execute("SHOW CHROMOSOMES LIKE 'chr_%';").unwrap();
        assert!(shown
            .rows
            .iter()
            .all(|r| r.fields["chromosome"].to_string().starts_with("chr_")));
    }

    #[test]
    fn still_honest_rejections() {
        let sql = setup("rej2");
        // UPSERT tails must not be silently dropped.
        assert!(sql
            .sql_to_dnaql(
                "INSERT INTO chr_users (user_id, username) VALUES (1, 'a') \
                 ON CONFLICT (user_id) DO NOTHING"
            )
            .unwrap_err()
            .contains("ON CONFLICT"));
        assert!(sql
            .sql_to_dnaql("INSERT IGNORE INTO chr_users (user_id, username) VALUES (1, 'a')")
            .unwrap_err()
            .contains("INSERT IGNORE"));
        // Subqueries and window functions stay SQL-only.
        assert!(sql
            .sql_to_dnaql(
                "SELECT username FROM chr_users WHERE user_id IN (SELECT user_id FROM chr_orders)"
            )
            .unwrap_err()
            .contains("subquer"));
        // RIGHT JOIN: sides must be swapped.
        assert!(sql
            .sql_to_dnaql(
                "SELECT username FROM chr_users u RIGHT JOIN chr_orders o ON u.user_id = o.user_id"
            )
            .unwrap_err()
            .contains("LEFT JOIN"));
        // Non-aggregate projection under GROUP BY (MySQL rule).
        assert!(sql
            .sql_to_dnaql("SELECT username, COUNT(*) FROM chr_users GROUP BY user_id")
            .unwrap_err()
            .contains("GROUP BY"));
    }

    #[test]
    fn insert_becomes_atomic_tx() {
        let sql = setup("ins");
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 1500.5)",
        )
        .unwrap();
        let out = sql
            .sql_to_dnaql(
                "INSERT INTO chr_users (user_id, username, balance) VALUES \
                 (12, 'dr_watson', 1500.5), (14, 'prof_crick', 890.0)",
            )
            .unwrap();
        assert_eq!(
            out,
            "TX { chr_users:12 += { user_id: 12, username: 'dr_watson', balance: 1500.5 }; \
             chr_users:14 += { user_id: 14, username: 'prof_crick', balance: 890.0 } };"
        );
    }

    #[test]
    fn update_and_delete_are_locus_addressed() {
        let sql = setup("upd");
        let out = sql
            .sql_to_dnaql("UPDATE chr_users SET balance = -25.75 WHERE user_id = 12")
            .unwrap();
        assert_eq!(out, "TX { chr_users:12.balance = -25.75 };");

        let out = sql
            .sql_to_dnaql("DELETE FROM chr_orders WHERE order_id = 7")
            .unwrap();
        assert_eq!(out, "TX { DROP LOCUS chr_orders:7; };");

        // Broad or non-PK predicates must be refused, not widened.
        assert!(sql
            .sql_to_dnaql("UPDATE chr_users SET balance = 0")
            .unwrap_err()
            .contains("without WHERE"));
        assert!(sql
            .sql_to_dnaql("UPDATE chr_users SET balance = 0 WHERE username = 'x'")
            .unwrap_err()
            .contains("primary key"));
        assert!(sql
            .sql_to_dnaql("UPDATE chr_users SET balance = 0 WHERE user_id = 12 AND balance > 0")
            .unwrap_err()
            .contains("strict"));
    }

    #[test]
    fn create_and_drop_table_round_trip() {
        let sql = setup("ddl");
        let out = sql
            .sql_to_dnaql(
                "CREATE TABLE chr_genes (gene_id INT, symbol TEXT, PRIMARY KEY (gene_id), \
                 FOREIGN KEY (gene_id) REFERENCES chr_users(user_id) ON DELETE RESTRICT)",
            )
            .unwrap();
        assert_eq!(
            out,
            "CREATE CHROMOSOME chr_genes (gene_id INT, symbol TEXT, PRIMARY KEY (gene_id), \
             FOREIGN KEY (gene_id) REFERENCES chr_users (user_id) ON DELETE RESTRICT);"
        );
        // The emitted DnaQL must be executable by the DnaQL engine on the
        // same storage, and produce a schema the SQL side can see.
        let dnaql = crate::dna_ql::DnaQlEngine::new(sql.engine.clone(), sql.clone());
        dnaql.execute(&out).unwrap();
        assert!(sql.get_schema("chr_genes").is_some());
        sql.execute("DROP TABLE chr_genes").unwrap();

        assert_eq!(
            sql.sql_to_dnaql("DROP TABLE IF EXISTS chr_old;").unwrap(),
            "DROP CHROMOSOME chr_old IF EXISTS;"
        );
        // CREATE INDEX now maps onto the DnaQL DDL passthrough (see
        // ddl_and_show_map_onto_dnaql); a plain DROP INDEX too.
        assert_eq!(
            sql.sql_to_dnaql("CREATE INDEX idx_u ON chr_users (username)")
                .unwrap(),
            "CREATE INDEX idx_u ON chr_users (username);"
        );
    }

    #[test]
    fn strings_and_numbers_render_faithfully() {
        let sql = setup("lit");
        // negative literal, NULL, quote inside the value, float without
        // fractional digits must stay a float token.
        let out = sql
            .sql_to_dnaql(
                "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES \
                 (3, 12, -2.0, 'pending')",
            )
            .unwrap();
        assert_eq!(
            out,
            "TX { chr_orders:3 += { order_id: 3, user_id: 12, total_usd: -2.0, \
             status: 'pending' } };"
        );
    }

    /// End-to-end semantic check: execute the original SQL on one engine and
    /// the converted DnaQL on another, then compare the final SELECT results.
    #[test]
    fn sql_and_dnaql_produce_identical_results() {
        let a = setup("e2e_sql");
        a.execute("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 100.0)")
            .unwrap();
        a.execute("INSERT INTO chr_users (user_id = 2, username = 'grace', balance = 250.5)")
            .unwrap();
        a.execute("INSERT INTO chr_users (user_id = 3, username = 'alan', balance = 75.0)")
            .unwrap();
        let want = a
            .execute(
                "SELECT user_id, username FROM chr_users WHERE balance >= 90 \
                 ORDER BY username ASC",
            )
            .unwrap();

        let b = setup("e2e_dnaql");
        let dnaql = crate::dna_ql::DnaQlEngine::new(b.engine.clone(), b.clone());
        for stmt in [
            "INSERT INTO chr_users (user_id, username, balance) VALUES (1, 'ada', 100.0)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (2, 'grace', 250.5)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (3, 'alan', 75.0)",
        ] {
            let dnaql_src = b.sql_to_dnaql(stmt).unwrap();
            dnaql.execute(&dnaql_src).unwrap();
        }
        let query = b
            .sql_to_dnaql(
                "SELECT user_id, username FROM chr_users WHERE balance >= 90 \
                 ORDER BY username ASC",
            )
            .unwrap();
        let got = dnaql.execute(&query).unwrap();

        assert_eq!(want.rows.len(), got.rows.len());
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            for col in ["user_id", "username"] {
                assert_eq!(
                    w.get(col),
                    g.fields.get(col),
                    "column {} diverged: SQL {:?} vs DnaQL {:?}",
                    col,
                    w.get(col),
                    g.fields.get(col)
                );
            }
        }
    }

    #[test]
    fn multi_statement_conversion() {
        let sql = setup("multi");
        let out = sql
            .sql_to_dnaql(
                "CREATE TABLE chr_t (id INT, tag TEXT, PRIMARY KEY (id)); \
                 DROP TABLE chr_t;",
            )
            .unwrap();
        assert!(out.contains("CREATE CHROMOSOME chr_t"));
        assert!(out.contains("DROP CHROMOSOME chr_t;"));
    }

    /// Semantic parity for the v4.3 constructs: the same data through SQL
    /// and through the transpiled DnaQL must produce identical rows for
    /// INNER/LEFT JOIN, GROUP BY + HAVING + aggregates, and OFFSET.
    #[test]
    fn sql_and_dnaql_parity_join_group_offset() {
        let a = setup("p2_sql");
        for (u, n, b) in [(1, "ada", 100.0), (2, "grace", 250.5), (3, "alan", 75.0)] {
            a.execute(&format!(
                "INSERT INTO chr_users (user_id, username, balance) VALUES ({u}, '{n}', {b:?})"
            ))
            .unwrap();
        }
        for (o, u, t, s) in [
            (10, 1, 50.0, "paid"),
            (11, 1, 20.0, "paid"),
            (12, 2, 75.0, "open"),
        ] {
            a.execute(&format!(
                "INSERT INTO chr_orders (order_id, user_id, total_usd, status) \
                 VALUES ({o}, {u}, {t:?}, '{s}')"
            ))
            .unwrap();
        }

        // --- INNER JOIN + WHERE + ORDER BY ---
        let want = a
            .execute(
                "SELECT u.username, o.total_usd FROM chr_users u \
                 INNER JOIN chr_orders o ON u.user_id = o.user_id \
                 WHERE o.status = 'paid' ORDER BY u.username, o.total_usd",
            )
            .unwrap();

        let b = setup("p2_dnaql");
        let dnaql = crate::dna_ql::DnaQlEngine::new(b.engine.clone(), b.clone());
        for stmt in [
            "INSERT INTO chr_users (user_id, username, balance) VALUES (1, 'ada', 100.0)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (2, 'grace', 250.5)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (3, 'alan', 75.0)",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (10, 1, 50.0, 'paid')",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (11, 1, 20.0, 'paid')",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (12, 2, 75.0, 'open')",
        ] {
            let q = b.sql_to_dnaql(stmt).unwrap();
            dnaql.execute(&q).unwrap();
        }
        let got = dnaql
            .execute(
                &b.sql_to_dnaql(
                    "SELECT u.username, o.total_usd FROM chr_users u \
                     INNER JOIN chr_orders o ON u.user_id = o.user_id \
                     WHERE o.status = 'paid' ORDER BY u.username, o.total_usd",
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "join row count diverged");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            assert_eq!(w.get("username"), g.fields.get("username"), "{:?}", g.fields);
            assert_eq!(w.get("total_usd"), g.fields.get("total_usd"), "{:?}", g.fields);
        }

        // --- GROUP BY + HAVING + aggregate ORDER BY ---
        let want = a
            .execute(
                "SELECT user_id, COUNT(*) AS n, SUM(total_usd) AS total \
                 FROM chr_orders GROUP BY user_id HAVING COUNT(*) > 1 ORDER BY n DESC",
            )
            .unwrap();
        let got = dnaql
            .execute(
                &b.sql_to_dnaql(
                    "SELECT user_id, COUNT(*) AS n, SUM(total_usd) AS total \
                     FROM chr_orders GROUP BY user_id HAVING COUNT(*) > 1 ORDER BY n DESC",
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "group row count diverged");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            assert_eq!(w.get("user_id"), g.fields.get("user_id"), "{:?}", g.fields);
            assert_eq!(w.get("n"), g.fields.get("n"), "{:?}", g.fields);
            assert_eq!(w.get("total"), g.fields.get("total"), "{:?}", g.fields);
        }

        // --- OFFSET / LIMIT pagination ---
        let want = a
            .execute(
                "SELECT username FROM chr_users ORDER BY username LIMIT 1 OFFSET 1",
            )
            .unwrap();
        let got = dnaql
            .execute(
                &b.sql_to_dnaql(
                    "SELECT username FROM chr_users ORDER BY username LIMIT 1 OFFSET 1",
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(want.rows.len(), got.rows.len());
        assert_eq!(
            want.rows[0].get("username"),
            got.rows[0].fields.get("username")
        );

    }
}

