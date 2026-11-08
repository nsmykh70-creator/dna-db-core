// SPDX-License-Identifier: AGPL-3.0-only
//! SQL → DnaQL transpilation.
//!
//! Converts a parsed SQL statement (see [`crate::sql_engine`]) into the
//! equivalent DnaQL query, so SQL clients can be migrated to the native
//! pipeline/TX dialect without changing semantics.
//!
//! Since DnaQL v2.2 the two dialects have full surface parity — every SQL
//! statement translates:
//! * `SELECT` — projection (`EMIT`, including computed `expr AS alias`),
//!   `WHERE` (incl. `NOT` / `IN` / `IS [NOT] NULL` / arithmetic),
//!   `JOIN` / `LEFT` / `RIGHT` / `FULL [OUTER] JOIN` (hash-join stages with
//!   SQL-identical row semantics), `GROUP BY` + aggregates + `HAVING`
//!   (named-agg `GROUP` stage), `ORDER BY` (`SORT`, least-significant-first),
//!   `LIMIT` / `OFFSET`.
//! * `UNION [ALL]` — compound pipeline with SORT/LIMIT/OFFSET tail.
//! * `INSERT` — every row becomes `table:<pk> += { ... }` inside one atomic
//!   `TX { ... }` block; `ON CONFLICT DO NOTHING` → the `?=` insert-if-absent
//!   operator; `ON CONFLICT DO UPDATE` translates when the assignments cover
//!   every column (an exact upsert), otherwise an honest error.
//! * `UPDATE` / `DELETE` — strict `WHERE <pk> = <literal>` compiles to the
//!   locus-addressed TX form; any other predicate compiles to the v2.2
//!   row-set statements (`UPDATE chr SET ... WHERE expr`, `DELETE FROM chr
//!   WHERE expr`), which mirror SQL UPDATE/DELETE including FK CASCADE.
//! * `CREATE TABLE` → `CREATE CHROMOSOME`, `DROP TABLE` → `DROP CHROMOSOME`,
//!   `ALTER TABLE ADD COLUMN` → `ALTER CHROMOSOME ADD COLUMN`,
//!   `CREATE/DROP INDEX` → the same DnaQL DDL.
//! * `SHOW TABLES` → `SHOW CHROMOSOMES`; `EXPLAIN SELECT` → `EXPLAIN FROM …`.
//! * Session transactions (`BEGIN`/`COMMIT`/`ROLLBACK`), CTEs (`WITH`),
//!   scalar/`EXISTS` subqueries and window functions stay SQL-only and return
//!   an explicit error naming the construct — nothing is silently
//!   approximated.
//!
//! Literals: floats always keep a decimal point, strings are quoted with
//! `'` (or `"` when the value contains a single quote — DnaQL has no escape
//! sequences, so a value containing both quote types is rejected).

use crate::dna_ql::{float_token, AggFunc, JoinKind};
use crate::expr::{CmpOp, Expr};
use crate::relational::{ChromosomeTable, ColumnType, Value};
use crate::sql_engine::{JoinType, OnConflict, SelectItem, Stmt, UpsertValue};

/// Convert one parsed SQL statement into a DnaQL query (terminated with `;`).
///
/// `schema_of` resolves a table name to its schema; SELECT/INSERT/UPDATE/
/// DELETE and CREATE TABLE need the schema, DROP does not.
pub(crate) fn transpile(
    stmt: &Stmt,
    schema_of: &dyn Fn(&str) -> Option<ChromosomeTable>,
) -> Result<String, String> {
    match stmt {
        Stmt::Select { .. } => Ok(format!("{};", select_pipeline(stmt, schema_of)?)),
        Stmt::Union {
            left,
            right,
            all,
            order_by,
            limit,
            offset,
        } => {
            // The SQL parser leaves the compound ORDER BY/LIMIT/OFFSET on the
            // right-hand select as well — strip them so they apply to the
            // compound (the UNION tail), not to the right side alone.
            let mut right_clean = right.as_ref().clone();
            if let Stmt::Select {
                order_by: ob,
                limit: l,
                offset: o,
                ..
            } = &mut right_clean
            {
                ob.clear();
                *l = None;
                *o = 0;
            }
            let l = select_pipeline(left, schema_of)?;
            let r = select_pipeline(&right_clean, schema_of)?;
            let mut text = format!(
                "{} {} {}",
                l,
                if *all { "UNION ALL" } else { "UNION" },
                r
            );
            // Compound-result tail: SORT least-significant first, then
            // OFFSET, then LIMIT (SQL operator order).
            let mut tail = Vec::new();
            for (key, asc) in order_by.iter().rev() {
                tail.push(format!(
                    "|> SORT {} {}",
                    key,
                    if *asc { "ASC" } else { "DESC" }
                ));
            }
            if *offset > 0 {
                tail.push(format!("|> OFFSET {}", offset));
            }
            if let Some(n) = limit {
                tail.push(format!("|> LIMIT {}", n));
            }
            if !tail.is_empty() {
                text.push(' ');
                text.push_str(&tail.join(" "));
            }
            Ok(format!("{};", text))
        }

        Stmt::Insert {
            table,
            rows,
            on_conflict,
        } => {
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
                // ON CONFLICT (v3.6 UPSERT tail):
                // * DO NOTHING → the v2.2 `?=` insert-if-absent operator;
                // * DO UPDATE SET → an exact upsert (`+=`) when the
                //   assignments cover every non-PK column (the locus pins
                //   the PK, so the merged row is then fully determined);
                //   otherwise the partial keep-old-values semantics are not
                //   expressible and we say so.
                let op = match on_conflict {
                    None => "+=",
                    Some(OnConflict::DoNothing) => "?=",
                    Some(OnConflict::DoUpdate { assignments }) => {
                        // Full non-PK coverage: the PK is pinned by the locus
                        // itself (DO UPDATE SET cannot change it — the SQL
                        // engine enforces that), so covering every other
                        // column makes the merged row fully determined.
                        let covers_non_pk = !assignments.is_empty()
                            && assignments.len() == schema.columns.len() - 1
                            && assignments
                                .iter()
                                .all(|(col, _)| *col != schema.primary_locus_column);
                        if covers_non_pk {
                            "+="
                        } else {
                            return Err(
                                "SQL→DnaQL: ON CONFLICT DO UPDATE SET is expressible only when \
                                 the SET list covers every non-PK column of the table (partial \
                                 keep-old-values updates have no DnaQL spelling); run it as SQL"
                                    .into(),
                            );
                        }
                    }
                };
                let rendered_fields = match on_conflict {
                    Some(OnConflict::DoUpdate { assignments }) => {
                        // Full non-PK coverage: each assigned column takes its
                        // assignment value (literal or the conflicting row's
                        // inserted value via EXCLUDED.col); the PK comes from
                        // the row itself.
                        let mut out = vec![format!("{}: {}", pk, literal(&pk_val)?)];
                        for (col, val) in assignments {
                            let v = match val {
                                UpsertValue::Literal(v) => v.clone(),
                                UpsertValue::Excluded(src) => row
                                    .iter()
                                    .find(|(k, _)| k == src)
                                    .map(|(_, v)| v.clone())
                                    .ok_or_else(|| {
                                        format!(
                                            "SQL→DnaQL: EXCLUDED.{} not present in the \
                                             conflicting row",
                                            src
                                        )
                                    })?,
                            };
                            out.push(format!("{}: {}", col, literal(&v)?));
                        }
                        out
                    }
                    _ => fields,
                };
                stmts.push(format!(
                    "{}:{} {} {{ {} }}",
                    table,
                    locus,
                    op,
                    rendered_fields.join(", ")
                ));
            }
            if stmts.is_empty() {
                return Err("SQL→DnaQL: INSERT has no rows".into());
            }
            Ok(format!("TX {{ {} }};", stmts.join("; ")))
        }

        Stmt::Update {
            table,
            assignments,
            where_clause,
        } => {
            let schema = schema_of(table)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", table))?;
            let sets = assignments
                .iter()
                .map(|(col, v)| Ok(format!("{} = {}", col, literal(v)?)))
                .collect::<Result<Vec<_>, String>>()?;
            if sets.is_empty() {
                return Err("SQL→DnaQL: UPDATE has no assignments".into());
            }
            // Fast path: strict `WHERE <pk> = <literal>` → compact
            // locus-addressed TX form.
            if let Ok(locus) = pk_locus(where_clause, &schema.primary_locus_column, table) {
                let stmts = assignments
                    .iter()
                    .map(|(col, v)| Ok(format!("{}:{}.{} = {}", table, locus, col, literal(v)?)))
                    .collect::<Result<Vec<_>, String>>()?;
                return Ok(format!("TX {{ {} }};", stmts.join("; ")));
            }
            // General path: v2.2 row-set UPDATE (arbitrary WHERE, no WHERE =
            // every row — same semantics as SQL UPDATE).
            let where_text = match where_clause {
                Some(e) => format!(" WHERE {}", expr(e)?),
                None => String::new(),
            };
            Ok(format!(
                "UPDATE {} SET {}{};",
                table,
                sets.join(", "),
                where_text
            ))
        }

        Stmt::Delete {
            table,
            where_clause,
        } => {
            let schema = schema_of(table)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", table))?;
            // Fast path: locus-addressed compact form.
            if let Ok(locus) = pk_locus(where_clause, &schema.primary_locus_column, table) {
                return Ok(format!("TX {{ DROP LOCUS {}:{}; }};", table, locus));
            }
            // General path: v2.2 row-set DELETE (FK CASCADE/RESTRICT mirror
            // the SQL engine inside the DnaQL executor).
            match where_clause {
                Some(e) => Ok(format!("DELETE FROM {} WHERE {};", table, expr(e)?)),
                None => Ok(format!("DELETE FROM {};", table)),
            }
        }

        Stmt::CreateTable {
            table,
            if_not_exists,
        } => {
            if !table.secondary_indexes.is_empty() {
                return Err(
                    "SQL→DnaQL: this table has CREATE INDEX definitions; build them separately \
                     with DnaQL CREATE INDEX (one index per statement)"
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

        Stmt::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        } => Ok(format!(
            "CREATE {}INDEX {}{} ON {} ({});",
            if *unique { "UNIQUE " } else { "" },
            if *if_not_exists { "IF NOT EXISTS " } else { "" },
            name,
            table,
            column
        )),
        Stmt::DropIndex {
            name,
            if_exists,
            table,
        } => Ok(format!(
            "DROP INDEX {}{}{};",
            if *if_exists { "IF EXISTS " } else { "" },
            name,
            table
                .as_ref()
                .map(|t| format!(" ON {}", t))
                .unwrap_or_default()
        )),
        Stmt::AlterTable { table, action } => match action {
            crate::sql_engine::AlterAction::AddColumn { column, col_type } => Ok(format!(
                "ALTER CHROMOSOME {} ADD COLUMN {} {};",
                table,
                column,
                type_name(col_type)?
            )),
            crate::sql_engine::AlterAction::DropColumn { column } => Ok(format!(
                "ALTER CHROMOSOME {} DROP COLUMN {};",
                table, column
            )),
            crate::sql_engine::AlterAction::RenameColumn { old, new } => Ok(format!(
                "ALTER CHROMOSOME {} RENAME COLUMN {} TO {};",
                table, old, new
            )),
            crate::sql_engine::AlterAction::ModifyColumn { column, col_type } => Ok(format!(
                "ALTER CHROMOSOME {} MODIFY COLUMN {} {};",
                table,
                column,
                type_name(col_type)?
            )),
            crate::sql_engine::AlterAction::ChangeColumn { old, new, col_type } => Ok(format!(
                "ALTER CHROMOSOME {} CHANGE COLUMN {} {} {};",
                table,
                old,
                new,
                type_name(col_type)?
            )),
        },
        Stmt::ShowTables => Ok("SHOW CHROMOSOMES;".into()),
        Stmt::ShowStorage { table, .. } => Err(format!(
            "SQL→DnaQL: SHOW STORAGE is SQL-only; the DnaQL equivalent for one locus is \
             ANALYZE LOCUS <table>:<pk> |> STORAGE_MAP() (table '{}')",
            table
        )),
        Stmt::Explain(inner) => match inner.as_ref() {
            Stmt::Select { .. } => Ok(format!("EXPLAIN {};", select_pipeline(inner, schema_of)?)),
            other => Err(format!(
                "SQL→DnaQL: EXPLAIN of {} has no DnaQL spelling; DnaQL plans are reported in \
                 the execution note of every read",
                describe_kind(other)
            )),
        },
        Stmt::Tx { action } => Err(format!(
            "SQL→DnaQL: {:?} is a session concept (DnaQL wraps writes in TX {{ ... }} blocks)",
            action
        )),
        Stmt::Cte { .. } => Err(
            "SQL→DnaQL: WITH (CTE) is not expressible in DnaQL; materialise each CTE separately"
                .into(),
        ),
        Stmt::Tx { action } => Err(format!(
            "SQL→DnaQL: {:?} is a session concept (DnaQL wraps writes in TX {{ ... }} blocks)",
            action
        )),
        Stmt::Select { .. } | Stmt::Union { .. } => unreachable!("handled above"),
    }
}

/// Which kind of statement this is (for error messages).
fn describe_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Insert { .. } => "INSERT",
        Stmt::Update { .. } => "UPDATE",
        Stmt::Delete { .. } => "DELETE",
        Stmt::CreateTable { .. } => "CREATE TABLE",
        Stmt::DropTable { .. } => "DROP TABLE",
        Stmt::CreateIndex { .. } => "CREATE INDEX",
        Stmt::DropIndex { .. } => "DROP INDEX",
        Stmt::AlterTable { .. } => "ALTER TABLE",
        Stmt::ShowTables => "SHOW TABLES",
        Stmt::ShowStorage { .. } => "SHOW STORAGE",
        Stmt::Explain(_) => "EXPLAIN",
        Stmt::Tx { .. } => "transaction control",
        Stmt::Cte { .. } => "WITH (CTE)",
        Stmt::Union { .. } => "nested UNION",
        Stmt::Select { .. } => "SELECT",
    }
}

// ---------------------------------------------------------------------------
// SELECT → pipeline
// ---------------------------------------------------------------------------

/// Convert a SELECT-shaped statement into a DnaQL pipeline (no trailing
/// semicolon). `Stmt::Union` sides must be plain SELECTs — nested UNIONs are
/// rejected (the SQL parser only accepts top-level chains anyway).
fn select_pipeline(
    stmt: &Stmt,
    schema_of: &dyn Fn(&str) -> Option<ChromosomeTable>,
) -> Result<String, String> {
    let Stmt::Select {
        projection,
        from,
        joins,
        where_clause,
        group_by,
        grouping,
        having,
        order_by,
        limit,
        offset,
    } = stmt
    else {
        return Err(format!(
            "SQL→DnaQL: {} is not expressible as a DnaQL pipeline",
            describe_kind(stmt)
        ));
    };
    if !grouping.is_empty() {
        return Err(
            "SQL→DnaQL: GROUPING SETS (ROLLUP/CUBE/WITH ROLLUP) is SQL-only; run the \"
             GROUP BY\" levels as separate DnaQL GROUP pipelines".into(),
        );
    }

    let schema = schema_of(from)
        .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", from))?;

    // Visible columns: the FROM table plus every joined table (the SQL row
    // model is single-namespace, so all of them addressable by bare name).
    let mut visible: Vec<String> = schema.columns.keys().cloned().collect();
    for j in joins {
        if let Some(js) = schema_of(&j.table) {
            visible.extend(js.columns.keys().cloned());
        }
    }

    let mut parts = vec![format!("FROM {}", from)];

    // JOIN stages (v2.2): hash join with SQL-identical semantics. ON sides
    // are already bare (the SQL parser strips qualifiers); bare_column is a
    // safety net for future qualified forms.
    for j in joins {
        if schema_of(&j.table).is_none() {
            return Err(format!(
                "SQL→DnaQL: joined table '{}' does not exist",
                j.table
            ));
        }
        let kind = match j.join_type {
            JoinType::Inner => JoinKind::Inner,
            JoinType::Left => JoinKind::Left,
            JoinType::Right => JoinKind::Right,
            JoinType::Full => JoinKind::Full,
        };
        let kw = match kind {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
        };
        parts.push(format!(
            "|> {} {} ON {} = {}",
            kw,
            j.table,
            bare_column(&j.left_col),
            bare_column(&j.right_col)
        ));
    }

    // WHERE runs before grouping (SQL operator order).
    if let Some(w) = where_clause {
        parts.push(format!("|> WHERE {}", expr(w)?));
    }

    // Aggregates: named-agg GROUP stage. The registry is shared by the
    // projection, HAVING and ORDER BY; names prefer the SQL output alias
    // when it is a valid DnaQL identifier.
    let mut aggs = AggRegistry::default();
    for item in projection {
        if let SelectItem::Agg {
            canonical,
            rendered,
            ..
        } = item
        {
            aggs.assign(canonical, Some(rendered))?;
        }
    }
    if let Some(h) = having {
        collect_agg_refs(h, &mut |c| {
            aggs.assign(c, None).map(|_| ())
        })?;
    }
    for (key, _) in order_by {
        if parse_agg_canonical(key).is_some() {
            aggs.assign(key, None)?;
        }
    }

    if !group_by.is_empty() || !aggs.stage.is_empty() {
        let mut stage = "|> GROUP".to_string();
        if !group_by.is_empty() {
            stage.push_str(&format!(" BY {}", group_by.join(", ")));
        }
        if !aggs.stage.is_empty() {
            let items = aggs
                .stage
                .iter()
                .map(|(name, func, arg)| match arg {
                    Some(col) => format!("{} = {}({})", name, func.as_str(), col),
                    None => format!("{} = {}(*)", name, func.as_str()),
                })
                .collect::<Vec<_>>()
                .join(", ");
            stage.push_str(&format!(" AGG {}", items));
        }
        parts.push(stage);
    }

    // HAVING → WHERE over the aggregated rows (aggregate results are plain
    // fields after the GROUP stage; references rewritten to the agg names).
    if let Some(h) = having {
        let rewritten = rewrite_agg_refs(h, &aggs.names)?;
        parts.push(format!("|> WHERE {}", rewritten));
    }

    // Multi-key ORDER BY: sort keys least-significant first — each SORT is a
    // stable sort, so the last applied (most significant key) ends up
    // primary, exactly like SQL's single multi-key sort.
    for (key, asc) in order_by.iter().rev() {
        let name = resolve_order_key(key, &visible, &aggs.names, projection)?;
        parts.push(format!(
            "|> SORT {} {}",
            name,
            if *asc { "ASC" } else { "DESC" }
        ));
    }

    // OFFSET before LIMIT (SQL operator order).
    if *offset > 0 {
        parts.push(format!("|> OFFSET {}", offset));
    }
    if let Some(n) = limit {
        parts.push(format!("|> LIMIT {}", n));
    }

    // EMIT list.
    let emit = render_emit(projection, &schema, &visible, &aggs.names)?;
    parts.push(format!("|> EMIT {}", emit));

    Ok(parts.join(" "))
}

/// Named-aggregate registry for the GROUP stage: SQL canonical renderings
/// (`COUNT(*)`) map to DnaQL identifier names. Preferred names come from the
/// SQL output alias when it is identifier-safe and unused.
#[derive(Default)]
struct AggRegistry {
    names: Vec<(String, String)>,
    stage: Vec<(String, AggFunc, Option<String>)>,
}

impl AggRegistry {
    fn assign(&mut self, canonical: &str, preferred: Option<&str>) -> Result<String, String> {
        if let Some((_, name)) = self.names.iter().find(|(c, _)| c == canonical) {
            return Ok(name.clone());
        }
        let (func, arg) = parse_agg_canonical(canonical)
            .ok_or_else(|| format!("SQL→DnaQL: cannot parse aggregate '{}'", canonical))?;
        let is_ident = |s: &str| {
            !s.is_empty()
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !s.chars().next().unwrap().is_ascii_digit()
        };
        let preferred_name = preferred.filter(|p| is_ident(p)).map(|p| p.to_string());
        let mut idx = self.stage.len();
        let name = loop {
            let candidate = match &preferred_name {
                Some(p) if idx == self.stage.len() => p.clone(),
                _ => format!("agg_{}", idx),
            };
            if !self.stage.iter().any(|(n, _, _)| *n == candidate) {
                break candidate;
            }
            idx += 1;
        };
        self.names.push((canonical.to_string(), name.clone()));
        self.stage.push((name.clone(), func, arg));
        Ok(name)
    }
}

/// `u.user_id` → `user_id` (DnaQL rows are single-namespace).
fn bare_column(col: &str) -> &str {
    col.rsplit('.').next().unwrap_or(col)
}

/// Parse an aggregate canonical rendering: `COUNT(*)`, `SUM(col)`, ...
fn parse_agg_canonical(name: &str) -> Option<(AggFunc, Option<String>)> {
    let open = name.find('(')?;
    let close = name.rfind(')')?;
    if close != name.len() - 1 {
        return None;
    }
    let func = AggFunc::from_name(&name[..open])?;
    let arg = &name[open + 1..close];
    if arg.is_empty() || arg == "*" {
        if func == AggFunc::Count {
            return Some((func, None));
        }
        return None;
    }
    Some((func, Some(arg.to_string())))
}

/// Visit every aggregate canonical reference inside an expression.
fn collect_agg_refs(
    e: &Expr,
    f: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    match e {
        Expr::Column(c) => {
            if parse_agg_canonical(c).is_some() {
                f(c)?;
            }
            Ok(())
        }
        Expr::Literal(_) | Expr::InList { .. } => Ok(()),
        Expr::Not(i) | Expr::IsNull(i) | Expr::IsNotNull(i) => collect_agg_refs(i, f),
        Expr::And(a, b) | Expr::Or(a, b) => {
            collect_agg_refs(a, f)?;
            collect_agg_refs(b, f)
        }
        Expr::Cmp { left, right, .. } | Expr::Arith { left, right, .. } => {
            collect_agg_refs(left, f)?;
            collect_agg_refs(right, f)
        }
        Expr::InSubquery { .. } | Expr::ScalarSubquery { .. } | Expr::Exists { .. } => Err(
            "SQL→DnaQL: subqueries are not expressible in DnaQL; run the inner SELECT separately"
                .into(),
        ),
    }
}

/// Rewrite aggregate canonical references (e.g. `COUNT(*)`) to the assigned
/// DnaQL aggregate names.
fn rewrite_agg_refs(e: &Expr, agg_names: &[(String, String)]) -> Result<String, String> {
    match e {
        Expr::Literal(v) => literal(v),
        Expr::Column(c) => {
            if let Some((_, name)) = agg_names.iter().find(|(can, _)| can == c) {
                Ok(name.clone())
            } else {
                Ok(c.clone())
            }
        }
        Expr::Not(i) => Ok(format!("NOT {}", rewrite_agg_refs(i, agg_names)?)),
        Expr::And(a, b) => Ok(format!(
            "({} AND {})",
            rewrite_agg_refs(a, agg_names)?,
            rewrite_agg_refs(b, agg_names)?
        )),
        Expr::Or(a, b) => Ok(format!(
            "({} OR {})",
            rewrite_agg_refs(a, agg_names)?,
            rewrite_agg_refs(b, agg_names)?
        )),
        Expr::Cmp { op, left, right } => Ok(format!(
            "{} {} {}",
            rewrite_agg_refs(left, agg_names)?,
            op.as_str(),
            rewrite_agg_refs(right, agg_names)?
        )),
        Expr::IsNull(i) => Ok(format!("{} IS NULL", rewrite_agg_refs(i, agg_names)?)),
        Expr::IsNotNull(i) => Ok(format!("{} IS NOT NULL", rewrite_agg_refs(i, agg_names)?)),
        Expr::Arith { op, left, right } => Ok(format!(
            "({} {} {})",
            rewrite_agg_refs(left, agg_names)?,
            op.as_str(),
            rewrite_agg_refs(right, agg_names)?
        )),
        Expr::InList { column, values } => {
            let lits = values
                .iter()
                .map(literal)
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!("{} IN ({})", column, lits.join(", ")))
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
    }
}

/// Resolve an ORDER BY key: aggregate canonical → assigned name; plain
/// column (FROM or joined) → itself; projection alias → the aliased column /
/// computed name.
fn resolve_order_key(
    key: &str,
    visible: &[String],
    agg_names: &[(String, String)],
    projection: &[SelectItem],
) -> Result<String, String> {
    if parse_agg_canonical(key).is_some() {
        return agg_names
            .iter()
            .find(|(c, _)| c == key)
            .map(|(_, n)| n.clone())
            .ok_or_else(|| format!("SQL→DnaQL: ORDER BY aggregate '{}' is unknown", key));
    }
    if visible.iter().any(|c| c == key) {
        return Ok(key.to_string());
    }
    // Projection aliases (Column AS alias, Computed AS alias, Agg alias).
    for item in projection {
        match item {
            SelectItem::Column { source, alias } => {
                if alias.as_deref() == Some(key) {
                    return Ok(source.clone());
                }
            }
            SelectItem::Computed { rendered, .. } | SelectItem::Agg { rendered, .. } => {
                if rendered == key {
                    // Aggregated outputs carry their assigned name; computed
                    // outputs carry their DnaQL alias (see render_emit).
                    if let Some((_, n)) = agg_names.iter().find(|(c, _)| c == key) {
                        return Ok(n.clone());
                    }
                    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !key.is_empty()
                        && !key.chars().next().unwrap().is_ascii_digit()
                    {
                        return Ok(key.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    Err(format!(
        "SQL→DnaQL: ORDER BY column '{}' is not a column of the queried tables",
        key
    ))
}

/// Render the EMIT list for a projection.
fn render_emit(
    projection: &[SelectItem],
    schema: &ChromosomeTable,
    visible: &[String],
    agg_names: &[(String, String)],
) -> Result<String, String> {
    if projection.is_empty() || matches!(projection, [SelectItem::Star]) {
        return Ok(schema
            .columns
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "));
    }
    let mut items = Vec::new();
    let mut computed_idx = 0usize;
    for item in projection {
        match item {
            SelectItem::Star => items.push("*".to_string()),
            SelectItem::Column { source, alias } => {
                if !visible.iter().any(|c| c == source) {
                    return Err(format!(
                        "SQL→DnaQL: projected column '{}' does not exist on the queried tables",
                        source
                    ));
                }
                match alias {
                    Some(a) if a != source => {
                        items.push(format!("{} AS {}", source, a));
                    }
                    _ => items.push(source.clone()),
                }
            }
            SelectItem::Agg { canonical, .. } => {
                let name = agg_names
                    .iter()
                    .find(|(c, _)| c == canonical)
                    .map(|(_, n)| n.clone())
                    .ok_or_else(|| {
                        format!("SQL→DnaQL: aggregate '{}' was not assigned a name", canonical)
                    })?;
                items.push(name);
            }
            SelectItem::Computed { expr: e, rendered } => {
                // The output name must be a DnaQL identifier: use the SQL
                // alias when it is one, else generate expr_N.
                let name = if rendered.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !rendered.is_empty()
                    && !rendered.chars().next().unwrap().is_ascii_digit()
                    && !visible.iter().any(|c| c == rendered)
                {
                    rendered.clone()
                } else {
                    computed_idx += 1;
                    format!("expr_{}", computed_idx)
                };
                items.push(format!("{} AS {}", expr(e)?, name));
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
    Ok(items.join(", "))
}

// ---------------------------------------------------------------------------
// WHERE helpers for locus-addressed mutations
// ---------------------------------------------------------------------------

/// Extract the single-condition `WHERE <pk> = <literal>` predicate that a
/// locus-addressed DnaQL TX mutation wants. Any other predicate is an error
/// so the caller can fall back to the general row-set statement.
fn pk_locus(where_clause: &Option<Expr>, pk: &str, table: &str) -> Result<i64, String> {
    let w = where_clause.as_ref().ok_or_else(|| {
        format!(
            "SQL→DnaQL: UPDATE/DELETE on '{}' without WHERE has no locus fast path",
            table
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
                        "SQL→DnaQL: WHERE must be exactly `{} = <literal>` for a locus \
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
            "SQL→DnaQL: locus mutations require a strict `WHERE {} = <literal>` predicate, \
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
        Expr::InList { column, values } => {
            let lits = values
                .iter()
                .map(literal)
                .collect::<Result<Vec<_>, String>>()?;
            Ok(format!("{} IN ({})", column, lits.join(", ")))
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
    fn join_translates_to_join_stage() {
        let sql = setup("join");
        let out = sql
            .sql_to_dnaql(
                "SELECT u.username, o.total_usd FROM chr_users u \
                 INNER JOIN chr_orders o ON u.user_id = o.user_id \
                 WHERE o.status = 'paid' ORDER BY o.total_usd DESC LIMIT 5",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
             |> WHERE status = 'paid' |> SORT total_usd DESC |> LIMIT 5 \
             |> EMIT username, total_usd;"
        );
    }

    #[test]
    fn group_by_having_aggregates_translate() {
        let sql = setup("grp");
        let out = sql
            .sql_to_dnaql(
                "SELECT user_id, COUNT(*) AS cnt, SUM(total_usd) AS total FROM chr_orders \
                 GROUP BY user_id HAVING COUNT(*) > 2 ORDER BY cnt DESC",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_orders |> GROUP BY user_id AGG cnt = COUNT(*), total = SUM(total_usd) \
             |> WHERE cnt > 2 |> SORT cnt DESC |> EMIT user_id, cnt, total;"
        );
        // HAVING on an aggregate that is not in the projection.
        let out = sql
            .sql_to_dnaql(
                "SELECT user_id FROM chr_orders GROUP BY user_id HAVING COUNT(*) >= 1",
            )
            .unwrap();
        assert!(out.contains("AGG agg_0 = COUNT(*)"));
        assert!(out.contains("|> WHERE agg_0 >= 1"));
        // Whole-table aggregate (no GROUP BY).
        let out = sql
            .sql_to_dnaql("SELECT COUNT(*), MAX(total_usd) FROM chr_orders")
            .unwrap();
        assert!(out.starts_with("FROM chr_orders |> GROUP AGG "));
        assert!(out.contains("MAX(total_usd)") || out.contains("MAX("));
    }

    #[test]
    fn offset_union_show_explain_translate() {
        let sql = setup("misc");
        assert_eq!(
            sql.sql_to_dnaql("SELECT user_id FROM chr_users LIMIT 3 OFFSET 5")
                .unwrap(),
            "FROM chr_users |> OFFSET 5 |> LIMIT 3 |> EMIT user_id;"
        );
        assert_eq!(
            sql.sql_to_dnaql("SHOW TABLES").unwrap(),
            "SHOW CHROMOSOMES;"
        );
        let out = sql
            .sql_to_dnaql(
                "SELECT username FROM chr_users UNION SELECT status FROM chr_orders \
                 ORDER BY username DESC LIMIT 4",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> EMIT username UNION FROM chr_orders |> EMIT status \
             |> SORT username DESC |> LIMIT 4;"
        );
        let out = sql
            .sql_to_dnaql("SELECT user_id FROM chr_users UNION ALL SELECT order_id FROM chr_orders")
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> EMIT user_id UNION ALL FROM chr_orders |> EMIT order_id;"
        );
        let out = sql
            .sql_to_dnaql("EXPLAIN SELECT user_id FROM chr_users WHERE user_id = 1")
            .unwrap();
        assert_eq!(
            out,
            "EXPLAIN FROM chr_users |> WHERE user_id = 1 |> EMIT user_id;"
        );
    }

    #[test]
    fn ddl_index_alter_translate() {
        let sql = setup("ddl");
        assert_eq!(
            sql.sql_to_dnaql("CREATE INDEX idx_u ON chr_users (username)")
                .unwrap(),
            "CREATE INDEX idx_u ON chr_users (username);"
        );
        assert_eq!(
            sql.sql_to_dnaql("CREATE UNIQUE INDEX IF NOT EXISTS idx_b ON chr_users (balance)")
                .unwrap(),
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_b ON chr_users (balance);"
        );
        assert_eq!(
            sql.sql_to_dnaql("DROP INDEX idx_u ON chr_users").unwrap(),
            "DROP INDEX idx_u ON chr_users;"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users ADD COLUMN note TEXT")
                .unwrap(),
            "ALTER CHROMOSOME chr_users ADD COLUMN note TEXT;"
        );
        // v4.2: every ALTER action has a DnaQL spelling, and the DnaQL form
        // executes through the same online DDL (parity in both directions).
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users DROP COLUMN note")
                .unwrap(),
            "ALTER CHROMOSOME chr_users DROP COLUMN note;"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users RENAME COLUMN note TO memo")
                .unwrap(),
            "ALTER CHROMOSOME chr_users RENAME COLUMN note TO memo;"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users MODIFY COLUMN note INT")
                .unwrap(),
            "ALTER CHROMOSOME chr_users MODIFY COLUMN note INT;"
        );
        assert_eq!(
            sql.sql_to_dnaql("ALTER TABLE chr_users CHANGE COLUMN note memo INT")
                .unwrap(),
            "ALTER CHROMOSOME chr_users CHANGE COLUMN note memo INT;"
        );
        // Round-trip: DnaQL ALTER forms execute like their SQL twins.
        let dnaql = crate::dna_ql::DnaQlEngine::new(
            crate::engine::Engine::open(crate::engine::EngineConfig {
                dir_path: std::env::temp_dir().join(format!("dna_ddl_rt_{}", std::process::id())),
                memtable_capacity: 4 * 1024 * 1024,
                sync_mode: crate::wal::SyncMode::Strict,
                mirror_disk_path: None,
                ..Default::default()
            })
            .unwrap()
            .into(),
            sql.clone(),
        );
        dnaql.execute("CREATE CHROMOSOME chr_rt (id INT, name TEXT, PRIMARY KEY (id));").unwrap();
        dnaql.execute("ALTER CHROMOSOME chr_rt ADD COLUMN tag TEXT;").unwrap();
        dnaql.execute("ALTER CHROMOSOME chr_rt RENAME COLUMN tag TO label;").unwrap();
        dnaql.execute("ALTER CHROMOSOME chr_rt MODIFY COLUMN label INT;").unwrap();
        dnaql.execute("ALTER CHROMOSOME chr_rt CHANGE COLUMN label mark TEXT;").unwrap();
        dnaql.execute("ALTER CHROMOSOME chr_rt DROP COLUMN mark;").unwrap();
        let schema = dnaql.sql.get_schema("chr_rt").unwrap();
        assert!(schema.columns.contains_key("name"));
        assert!(!schema.columns.contains_key("mark"));
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
    fn on_conflict_translates() {
        let sql = setup("upsert");
        // DO NOTHING → the ?= insert-if-absent operator.
        let out = sql
            .sql_to_dnaql(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (12, 'x', 1.0) \
                 ON CONFLICT (user_id) DO NOTHING",
            )
            .unwrap();
        assert_eq!(
            out,
            "TX { chr_users:12 ?= { user_id: 12, username: 'x', balance: 1.0 } };"
        );
        // DO UPDATE covering every non-PK column → exact upsert via +=
        // (the PK is pinned by the locus and cannot change).
        let out = sql
            .sql_to_dnaql(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (12, 'x', 1.0) \
                 ON CONFLICT (user_id) DO UPDATE SET username = EXCLUDED.username, \
                 balance = EXCLUDED.balance",
            )
            .unwrap();
        assert_eq!(
            out,
            "TX { chr_users:12 += { user_id: 12, username: 'x', balance: 1.0 } };"
        );
        // Partial DO UPDATE → honest rejection.
        let err = sql
            .sql_to_dnaql(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (12, 'x', 1.0) \
                 ON CONFLICT (user_id) DO UPDATE SET balance = 5.0",
            )
            .unwrap_err();
        assert!(err.contains("non-PK column"), "{}", err);
    }

    #[test]
    fn update_delete_locus_and_rowset_forms() {
        let sql = setup("upd");
        let out = sql
            .sql_to_dnaql("UPDATE chr_users SET balance = -25.75 WHERE user_id = 12")
            .unwrap();
        assert_eq!(out, "TX { chr_users:12.balance = -25.75 };");

        let out = sql
            .sql_to_dnaql("DELETE FROM chr_orders WHERE order_id = 7")
            .unwrap();
        assert_eq!(out, "TX { DROP LOCUS chr_orders:7; };");

        // Broad or non-PK predicates now compile to the v2.2 row-set forms.
        assert_eq!(
            sql.sql_to_dnaql("UPDATE chr_users SET balance = 0")
                .unwrap(),
            "UPDATE chr_users SET balance = 0;"
        );
        assert_eq!(
            sql.sql_to_dnaql("UPDATE chr_users SET balance = 0 WHERE username = 'x'")
                .unwrap(),
            "UPDATE chr_users SET balance = 0 WHERE username = 'x';"
        );
        assert_eq!(
            sql.sql_to_dnaql(
                "UPDATE chr_users SET balance = 0 WHERE user_id = 12 AND balance > 0"
            )
            .unwrap(),
            "UPDATE chr_users SET balance = 0 WHERE (user_id = 12 AND balance > 0);"
        );
        assert_eq!(
            sql.sql_to_dnaql("DELETE FROM chr_users WHERE balance < 10.0")
                .unwrap(),
            "DELETE FROM chr_users WHERE balance < 10.0;"
        );
    }

    #[test]
    fn not_is_now_expressible() {
        let sql = setup("not");
        let out = sql
            .sql_to_dnaql(
                "SELECT user_id FROM chr_users WHERE NOT (balance > 10.0) AND username != 'x'",
            )
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> WHERE (NOT balance > 10.0 AND username != 'x') \
             |> EMIT user_id;"
        );
        let out = sql
            .sql_to_dnaql("SELECT user_id FROM chr_users WHERE balance IS NOT NULL")
            .unwrap();
        assert_eq!(
            out,
            "FROM chr_users |> WHERE balance IS NOT NULL |> EMIT user_id;"
        );
    }

    #[test]
    fn create_and_drop_table_round_trip() {
        let sql = setup("ddl2");
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

    /// The v2.2 headline guarantee: SQL and the transpiled DnaQL agree on
    /// JOIN, GROUP BY/aggregates, UNION, OFFSET and row-set UPDATE/DELETE.
    #[test]
    fn sql_and_dnaql_v22_parity() {
        let a = setup("v22_sql");
        let b = setup("v22_dnaql");
        let dnaql = crate::dna_ql::DnaQlEngine::new(b.engine.clone(), b.clone());
        let inserts = [
            "INSERT INTO chr_users (user_id, username, balance) VALUES (1, 'ada', 100.0)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (2, 'grace', 250.5)",
            "INSERT INTO chr_users (user_id, username, balance) VALUES (3, 'alan', 75.0)",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (10, 1, 50.0, 'paid')",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (11, 1, 70.0, 'open')",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (12, 2, 200.0, 'paid')",
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES (13, 2, 999.0, 'orphan')",
        ];
        for stmt in inserts {
            a.execute(stmt).unwrap();
            dnaql.execute(&b.sql_to_dnaql(stmt).unwrap()).unwrap();
        }

        // 1. INNER JOIN (with a conflict column: user_id exists on both
        // sides → joined_user_id on both engines).
        let q = "SELECT username, total_usd FROM chr_users u \
                 INNER JOIN chr_orders o ON u.user_id = o.user_id WHERE status = 'paid'";
        let want = a.execute(q).unwrap();
        let got = dnaql.execute(&b.sql_to_dnaql(q).unwrap()).unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "JOIN row count");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            for col in ["username", "total_usd"] {
                assert_eq!(
                    w.get(col),
                    g.fields.get(col),
                    "JOIN column {} diverged",
                    col
                );
            }
        }

        // 2. LEFT JOIN NULL-extension (user 3 has no orders).
        let q = "SELECT username, order_id FROM chr_users u \
                 LEFT JOIN chr_orders o ON u.user_id = o.user_id ORDER BY u.user_id ASC, order_id ASC";
        let want = a.execute(q).unwrap();
        let got = dnaql.execute(&b.sql_to_dnaql(q).unwrap()).unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "LEFT JOIN row count");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            assert_eq!(
                w.get("username"),
                g.fields.get("username"),
                "LEFT JOIN username"
            );
            assert_eq!(
                w.get("order_id"),
                g.fields.get("order_id"),
                "LEFT JOIN order_id (NULL-extension)"
            );
        }

        // 3. GROUP BY + HAVING + ORDER BY aggregate.
        let q = "SELECT user_id, COUNT(*) AS cnt, SUM(total_usd) AS total FROM chr_orders \
                 GROUP BY user_id HAVING COUNT(*) >= 1 ORDER BY cnt DESC, user_id ASC";
        let want = a.execute(q).unwrap();
        let got = dnaql.execute(&b.sql_to_dnaql(q).unwrap()).unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "GROUP BY row count");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            for col in ["user_id", "cnt", "total"] {
                assert_eq!(
                    w.get(col),
                    g.fields.get(col),
                    "GROUP BY column {} diverged",
                    col
                );
            }
        }

        // 4. UNION DISTINCT + ORDER/LIMIT on the compound.
        let q = "SELECT user_id FROM chr_users UNION SELECT user_id FROM chr_orders \
                 ORDER BY user_id ASC LIMIT 3";
        let want = a.execute(q).unwrap();
        let got = dnaql.execute(&b.sql_to_dnaql(q).unwrap()).unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "UNION row count");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            assert_eq!(w.get("user_id"), g.fields.get("user_id"), "UNION user_id");
        }

        // 5. Row-set UPDATE/DELETE with a broad WHERE.
        for (sql_q, dnaql_q) in [
            (
                "UPDATE chr_users SET balance = 0.0 WHERE balance < 100",
                "UPDATE chr_users SET balance = 0.0 WHERE balance < 100;",
            ),
            ("DELETE FROM chr_orders WHERE status = 'orphan'", "DELETE FROM chr_orders WHERE status = 'orphan';"),
        ] {
            a.execute(sql_q).unwrap();
            dnaql.execute(dnaql_q).unwrap();
        }
        let want = a
            .execute("SELECT user_id, username, balance FROM chr_users ORDER BY user_id ASC")
            .unwrap();
        let got = dnaql
            .execute("FROM chr_users |> EMIT user_id, username, balance |> SORT user_id ASC")
            .unwrap();
        assert_eq!(want.rows.len(), got.rows.len(), "after UPDATE/DELETE");
        for (w, g) in want.rows.iter().zip(got.rows.iter()) {
            for col in ["user_id", "username", "balance"] {
                assert_eq!(
                    w.get(col),
                    g.fields.get(col),
                    "column {} diverged after row-set DML",
                    col
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
}

