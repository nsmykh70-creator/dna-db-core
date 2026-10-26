// SPDX-License-Identifier: AGPL-3.0-only
//! SQL → DnaQL transpilation.
//!
//! Converts a parsed SQL statement (see [`crate::sql_engine`]) into the
//! equivalent DnaQL query, so SQL clients can be migrated to the native
//! pipeline/TX dialect without changing semantics.
//!
//! Statement coverage:
//! * `SELECT` — column projection (`EMIT`), `WHERE`, `ORDER BY` (`SORT`,
//!   keys emitted least-significant-first so the stable final sort matches
//!   SQL's multi-key order), `LIMIT`. `JOIN`, `GROUP BY`/`HAVING`,
//!   aggregates, computed projections and `OFFSET` have no DnaQL equivalent
//!   and return an explicit error naming the construct.
//! * `INSERT` — every row becomes `table:<pk> += { ... }` inside one atomic
//!   `TX { ... }` block (DnaQL addresses rows by integer locus).
//! * `UPDATE` / `DELETE` — require a strict `WHERE <pk> = <literal>`
//!   predicate (DnaQL mutations are locus-addressed); anything else is
//!   rejected rather than silently widened.
//! * `CREATE TABLE` → `CREATE CHROMOSOME` (column types, `PRIMARY KEY`,
//!   `FOREIGN KEY ... ON DELETE ...` clauses are passed through verbatim);
//!   `DROP TABLE` → `DROP CHROMOSOME`.
//! * `CREATE/DROP INDEX`, `EXPLAIN` — SQL-only, rejected with a message
//!   telling the user to keep the statement as SQL.
//!
//! Literals: floats always keep a decimal point, strings are quoted with
//! `'` (or `"` when the value contains a single quote — DnaQL has no escape
//! sequences, so a value containing both quote types is rejected).

use crate::dna_ql::float_token;
use crate::expr::{CmpOp, Expr};
use crate::relational::{ChromosomeTable, ColumnType, Value};
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
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            if !joins.is_empty() {
                return Err(
                    "SQL→DnaQL: JOIN is not expressible in DnaQL (use FIND ... -> link())".into(),
                );
            }
            if !group_by.is_empty() || having.is_some() {
                return Err(
                    "SQL→DnaQL: GROUP BY / HAVING (aggregates) are not expressible in DnaQL".into(),
                );
            }
            if *offset > 0 {
                return Err("SQL→DnaQL: OFFSET has no DnaQL pipeline stage".into());
            }
            let schema = schema_of(from)
                .ok_or_else(|| format!("SQL→DnaQL: table '{}' does not exist", from))?;

            let mut parts = vec![format!("FROM {}", from)];
            if let Some(w) = where_clause {
                parts.push(format!("|> WHERE {}", expr(w)?));
            }
            // Multi-key ORDER BY: sort keys least-significant first — each
            // SORT is a stable sort, so the last applied (most significant
            // key) ends up primary, exactly like SQL's single multi-key sort.
            for (key, asc) in order_by.iter().rev() {
                if key.contains('(') {
                    return Err(format!(
                        "SQL→DnaQL: sorting by aggregate '{}' is not expressible in DnaQL",
                        key
                    ));
                }
                if !schema.columns.contains_key(key) {
                    return Err(format!(
                        "SQL→DnaQL: ORDER BY column '{}' is not a column of '{}'",
                        key, from
                    ));
                }
                parts.push(format!(
                    "|> SORT {} {}",
                    key,
                    if *asc { "ASC" } else { "DESC" }
                ));
            }
            if let Some(n) = limit {
                parts.push(format!("|> LIMIT {}", n));
            }
            parts.push(format!("|> EMIT {}", emit_columns(projection, &schema)?));
            Ok(format!("{};", parts.join(" ")))
        }

        Stmt::Insert { table, rows } => {
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
                .map(|(col, v)| Ok(format!("{}:{}.{} = {}", table, locus, col, literal(v)?)))
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

        Stmt::CreateIndex { table, .. } => Err(format!(
            "SQL→DnaQL: CREATE INDEX has no DnaQL equivalent; run it as SQL \
             (CREATE INDEX ... ON {})",
            table
        )),
        Stmt::DropIndex { name, .. } => Err(format!(
            "SQL→DnaQL: DROP INDEX has no DnaQL equivalent; run it as SQL (DROP INDEX {})",
            name
        )),
        Stmt::Explain(_) => Err(
            "SQL→DnaQL: EXPLAIN is SQL-only — the equivalent DnaQL plan is reported in the \
             execution note of every DnaQL result"
                .into(),
        ),
    }
}

// ---------------------------------------------------------------------------
// SELECT helpers
// ---------------------------------------------------------------------------

/// `EMIT` column list for a projection, validated against the schema.
fn emit_columns(
    projection: &[crate::sql_engine::SelectItem],
    schema: &ChromosomeTable,
) -> Result<String, String> {
    use crate::sql_engine::SelectItem;
    if projection.is_empty() || matches!(projection, [SelectItem::Star]) {
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
                cols.extend(schema.columns.keys().cloned());
            }
            SelectItem::Column { source, alias } => {
                if let Some(a) = alias {
                    if a != source {
                        return Err(format!(
                            "SQL→DnaQL: projection alias '{} AS {}' is not expressible in DnaQL \
                             EMIT",
                            source, a
                        ));
                    }
                }
                if !schema.columns.contains_key(source) {
                    return Err(format!(
                        "SQL→DnaQL: projected column '{}' does not exist on '{}'",
                        source, schema.name
                    ));
                }
                cols.push(source.clone());
            }
            SelectItem::Agg { rendered, .. } => {
                return Err(format!(
                    "SQL→DnaQL: aggregate '{}' is not expressible in DnaQL (no GROUP BY stage)",
                    rendered
                ));
            }
            SelectItem::Computed { rendered, .. } => {
                return Err(format!(
                    "SQL→DnaQL: computed projection '{}' is not expressible in DnaQL EMIT",
                    rendered
                ));
            }
        }
    }
    cols.dedup();
    Ok(cols.join(", "))
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
        Expr::Not(_) => Err(
            "SQL→DnaQL: NOT is not part of the DnaQL expression grammar (use <> comparisons)"
                .into(),
        ),
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
    fn aggregates_and_joins_are_rejected() {
        let sql = setup("rej");
        assert!(sql
            .sql_to_dnaql("SELECT COUNT(*) FROM chr_users")
            .unwrap_err()
            .contains("aggregate"));
        assert!(sql
            .sql_to_dnaql(
                "SELECT u.username FROM chr_users u INNER JOIN chr_orders o ON u.user_id = \
                 o.user_id"
            )
            .unwrap_err()
            .contains("JOIN"));
        assert!(sql
            .sql_to_dnaql("SELECT user_id FROM chr_users OFFSET 5")
            .unwrap_err()
            .contains("OFFSET"));
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
        assert!(sql
            .sql_to_dnaql("CREATE INDEX idx_u ON chr_users (username)")
            .unwrap_err()
            .contains("CREATE INDEX"));
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
}
