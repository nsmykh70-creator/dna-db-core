// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::index_scan::*;
use super::lexer::STok;
use super::parser::SParser;
use super::refs::*;
use super::*;
use crate::expr::{CmpOp, Expr};
use crate::relational::{ChromosomeTable, RowCodec, Value};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;

impl SqlEngine {
    pub fn new(engine: Arc<Engine>) -> Self {
        let s = Self {
            engine,
            schemas: RwLock::new(BTreeMap::new()),
            complement_index: RwLock::new(ComplementIndex::new()),
            cte_store: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "cms")]
            cms_sequences: std::sync::Arc::new(crate::cms::sequence::AutoIncrementRegistry::new()),
        };
        if let Err(error) = s.load_schemas_from_disk() {
            eprintln!("[SQL] Could not rebuild persisted schema/index state: {error}");
        }
        s
    }

    fn load_schemas_from_disk(&self) -> Result<(), String> {
        let rows = self
            .engine
            .scan_prefix(b"__schema:")
            .map_err(|e| e.to_string())?;
        let mut loaded = Vec::new();
        for (_, payload) in rows {
            if let Ok(t) = serde_json::from_slice::<ChromosomeTable>(&payload) {
                loaded.push(t);
            }
        }
        {
            let mut schemas = self.schemas.write();
            let mut index = self.complement_index.write();
            for table in &loaded {
                for fk in &table.foreign_keys {
                    index.register_column(&table.name, &fk.column);
                }
                schemas.insert(table.name.clone(), table.clone());
            }
        }
        for table in &loaded {
            let rows = self.read_table_rows(&table.name)?;
            let mut index = self.complement_index.write();
            for row in rows {
                let key = Self::row_key_from_row(&table.name, table, &row)?;
                for fk in &table.foreign_keys {
                    if let Some(value) = row.get(&fk.column) {
                        if !value.is_null() {
                            index.upsert(&table.name, &fk.column, &key, value);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.schemas.read().keys().cloned().collect()
    }

    pub fn get_schema(&self, table: &str) -> Option<ChromosomeTable> {
        self.schemas.read().get(table).cloned()
    }

    /// Convert one or more SQL statements (separated by `;`) into the
    /// equivalent DnaQL query text. Statements without a DnaQL equivalent
    /// (JOIN, aggregates, CREATE INDEX, EXPLAIN, ...) return an explicit
    /// error naming the construct — nothing is silently approximated.
    ///
    /// ```text
    /// let dnaql = engine.sql_to_dnaql(
    ///     "SELECT user_id, username FROM chr_users WHERE balance > 100 LIMIT 10"
    /// ).unwrap();
    /// assert_eq!(
    ///     dnaql,
    ///     "FROM chr_users |> WHERE balance > 100 |> LIMIT 10 |> EMIT user_id, username;"
    /// );
    /// ```
    pub fn sql_to_dnaql(&self, sql: &str) -> Result<String, String> {
        let mut p = SParser::new(sql)?;
        let mut out = Vec::new();
        while p.peek() != &STok::Eof {
            let stmt = p.parse_stmt()?;
            out.push(crate::sql_to_dnaql::transpile(&stmt, &|t| {
                self.get_schema(t)
            })?);
        }
        if out.is_empty() {
            return Err("SQL: nothing to convert (empty statement)".into());
        }
        Ok(out.join("\n"))
    }

    /// v4.1: human-readable storage map for one or more parsed SQL
    /// statements: which chromosome (table) and locus (PK) every row lands
    /// at, the exact storage key, and how each value is physically
    /// represented. Read-only — nothing is executed. Used by
    /// `dna-db-core sql2dnaql` (the "storage map" section) and documented
    /// in SQL_MANUAL.
    pub fn explain_storage(&self, sql: &str) -> Result<String, String> {
        let mut p = SParser::new(sql)?;
        let mut out = Vec::new();
        while p.peek() != &STok::Eof {
            let stmt = p.parse_stmt()?;
            out.push(self.storage_map_stmt(&stmt)?);
        }
        if out.is_empty() {
            return Err("SQL: nothing to map (empty statement)".into());
        }
        Ok(out.join("\n\n"))
    }

    fn storage_map_stmt(&self, stmt: &Stmt) -> Result<String, String> {
        match stmt {
            Stmt::Insert { table, rows, .. } => {
                let schema = self.get_schema(table).ok_or_else(|| {
                    format!(
                        "SQL: relation '{}' does not exist (CREATE TABLE first)",
                        table
                    )
                })?;
                let mut lines = vec![format!(
                    "INSERT into chromosome '{}' — every row becomes one locus \
                     under the key '<table>:<pk>' (DnaQL: <chromosome>:<locus>):",
                    table
                )];
                for row in rows {
                    let pk = row
                        .iter()
                        .find(|(c, _)| *c == schema.primary_locus_column)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| {
                            format!(
                                "SQL: INSERT is missing the primary key column '{}'",
                                schema.primary_locus_column
                            )
                        })?;
                    let key = Self::row_key(table, &pk);
                    lines.push(format!(
                        "  locus {} -> key \"{}\" ({} B payload):",
                        pk,
                        key,
                        RowCodec::encode(row).len()
                    ));
                    for (col, value) in row {
                        lines.push(format!(
                            "    {} [{}] = {} | stored: {}",
                            col,
                            schema.column_type_name(col),
                            value,
                            value.stored_as()
                        ));
                    }
                }
                Ok(lines.join("\n"))
            }
            Stmt::Update {
                table,
                where_clause,
                ..
            }
            | Stmt::Delete {
                table,
                where_clause,
                ..
            } => {
                let schema = self.get_schema(table).ok_or_else(|| {
                    format!(
                        "SQL: relation '{}' does not exist (CREATE TABLE first)",
                        table
                    )
                })?;
                let pkcol = &schema.primary_locus_column;
                let locus = where_clause.as_ref().and_then(|e| match e {
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        left,
                        right,
                    } => match (&**left, &**right) {
                        (Expr::Column(c), Expr::Literal(v)) if c == pkcol => Some(v.clone()),
                        (Expr::Literal(v), Expr::Column(c)) if c == pkcol => Some(v.clone()),
                        _ => None,
                    },
                    _ => None,
                });
                let verb = if matches!(stmt, Stmt::Update { .. }) {
                    "UPDATE"
                } else {
                    "DELETE"
                };
                match locus {
                    Some(v) => {
                        let key = Self::row_key(table, &v);
                        Ok(format!(
                            "{} addresses exactly one locus: key \"{}\" \
                             (chromosome '{}', locus {})",
                            verb, key, table, v
                        ))
                    }
                    None => Ok(format!(
                        "{} scans chromosome '{}' (key prefix \"{}:\") and \
                         touches every locus matching the predicate; add \
                         WHERE {} = <literal> to address exactly one locus",
                        verb, table, table, pkcol
                    )),
                }
            }
            Stmt::Select { from, .. } if !from.is_empty() => Ok(format!(
                "SELECT reads loci of chromosome '{}' — point gets on \
                     \"{}:<pk>\" or a prefix scan \"{}:\"",
                from, from, from
            )),
            Stmt::CreateTable { table, .. } => Ok(format!(
                "table '{}' -> chromosome '{}'; the schema itself is a \
                     meta-row under the key \"{}:schema\"",
                table.name, table.name, table.name
            )),
            Stmt::DropTable { table, .. } => Ok(format!(
                "DROP removes chromosome '{}' with all its loci \
                     (keys \"{}:<pk>\") and the meta-row \"{}:schema\"",
                table, table, table
            )),
            other => Ok(format!(
                "({} — does not address row storage directly)",
                Self::describe_stmt(other)
            )),
        }
    }

    pub fn execute(&self, sql: &str) -> Result<QueryResult, String> {
        let mut tx = None;
        self.execute_in_tx(sql, &mut tx)
    }

    /// Execute a statement with an optional session transaction (v3.6).
    /// `tx` carries the `BEGIN`/`COMMIT`/`ROLLBACK` state across calls:
    /// while active, DML operations are buffered and applied as one atomic
    /// WAL batch on COMMIT; ROLLBACK discards them.
    pub fn execute_in_tx(
        &self,
        sql: &str,
        tx: &mut Option<SqlTxBuffer>,
    ) -> Result<QueryResult, String> {
        let start = std::time::Instant::now();
        // SHOW HEALTH — engine health snapshot (P5.2), handled before the
        // regular SQL grammar so it works everywhere SQL does.
        if sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SHOW HEALTH")
        {
            let health = self.engine.health();
            let rows = vec![{
                let mut m = std::collections::BTreeMap::new();
                m.insert("health".to_string(), Value::Text(health.to_string()));
                m
            }];
            return Ok(QueryResult {
                columns: vec!["health".to_string()],
                rows,
                execution_plan: "SHOW HEALTH: engine.health() snapshot".to_string(),
                execution_time_us: start.elapsed().as_micros() as u64,
                message: Some(format!(
                    "v3.0 Repairosome — lesions: {} found / {} repaired / {} unrecoverable",
                    health["lesions"]["found"],
                    health["lesions"]["repaired"],
                    health["lesions"]["unrecoverable"]
                )),
            });
        }
        let trimmed_upper = sql.trim_start().to_ascii_uppercase();
        // SELECT VERSION() — server identity string (v3.6; used by pgwire
        // clients, DBeaver, psql and the LiveConsole preview).
        if trimmed_upper == "SELECT VERSION()"
            || trimmed_upper.starts_with("SELECT VERSION()")
                && (trimmed_upper.len() == "SELECT VERSION()".len()
                    || trimmed_upper["SELECT VERSION()".len()..].starts_with(';'))
        {
            let mut m = std::collections::BTreeMap::new();
            m.insert("version".to_string(), Value::Text(Self::server_version()));
            return Ok(QueryResult {
                columns: vec!["version".to_string()],
                rows: vec![m],
                execution_plan: "ScalarEvaluate(VERSION())".to_string(),
                execution_time_us: start.elapsed().as_micros() as u64,
                message: None,
            });
        }
        let mut p = SParser::new(sql)?;
        let stmt = p.parse_stmt()?;
        if p.peek() != &STok::Eof {
            return Err(format!(
                "SQL: unexpected trailing tokens starting at {:?} near {} (unsupported syntax?)",
                p.peek(),
                p.debug_context()
            ));
        }
        let (columns, rows, plan, message) = self.exec_stmt(stmt, tx)?;
        Ok(QueryResult {
            columns,
            rows,
            execution_plan: plan,
            execution_time_us: start.elapsed().as_micros() as u64,
            message,
        })
    }

    /// Server identity string reported by VERSION() and pgwire.
    pub fn server_version() -> String {
        format!(
            "DNA-DB v{} (Linux x86_64, LSM RS(4+2), pgwire 5432)",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn exec_stmt(
        &self,
        stmt: Stmt,
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
                &projection,
                &from,
                from_alias.as_deref(),
                &joins,
                where_clause,
                &group_by,
                having,
                &order_by,
                limit,
                offset,
            ),
            Stmt::Union {
                left,
                right,
                all,
                order_by,
                limit,
                offset,
            } => self.exec_union(*left, *right, all, order_by, limit, offset),
            Stmt::Cte { ctes, body } => self.exec_cte(ctes, *body, tx),
            Stmt::ShowTables => self.exec_show_tables(),
            Stmt::ShowStorage { table, pk } => self.exec_show_storage(&table, pk.as_ref()),
            Stmt::Tx { action } => self.exec_tx(action, tx),
            Stmt::Insert {
                table,
                rows,
                on_conflict,
                ignore,
            } => match tx {
                Some(buf) if buf.active => {
                    let mut sink = SqlTxBuffer::default();
                    let res =
                        self.exec_insert_tx(&table, rows, on_conflict, ignore, Some(&mut sink))?;
                    tx.as_mut().unwrap().ops.extend(sink.ops);
                    Ok(res)
                }
                _ => self.exec_insert_tx(&table, rows, on_conflict, ignore, None),
            },
            Stmt::InsertSelect {
                table,
                cols,
                select,
            } => {
                // Run the source SELECT (same tx view), map its columns
                // positionally onto the target column list and reuse the
                // regular insert path.
                let (scols, srows, _, _) = self.exec_stmt(*select, tx)?;
                if scols.len() != cols.len() {
                    return Err(format!(
                        "SQL: INSERT..SELECT: SELECT returns {} column(s), target list has {}",
                        scols.len(),
                        cols.len()
                    ));
                }
                let mut rows = Vec::with_capacity(srows.len());
                for r in &srows {
                    let mut out_row = Vec::with_capacity(cols.len());
                    for (name, col) in scols.iter().zip(cols.iter()) {
                        let v = r.get(name).cloned().unwrap_or(Value::Null);
                        out_row.push((col.clone(), v));
                    }
                    rows.push(out_row);
                }
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res =
                            self.exec_insert_tx(&table, rows, None, false, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_insert_tx(&table, rows, None, false, None),
                }
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => match tx {
                Some(buf) if buf.active => {
                    let mut sink = SqlTxBuffer::default();
                    let res =
                        self.exec_update_tx(&table, &assignments, where_clause, Some(&mut sink))?;
                    tx.as_mut().unwrap().ops.extend(sink.ops);
                    Ok(res)
                }
                _ => self.exec_update_tx(&table, &assignments, where_clause, None),
            },
            Stmt::Delete {
                table,
                where_clause,
            } => match tx {
                Some(buf) if buf.active => {
                    let mut sink = SqlTxBuffer::default();
                    let res = self.exec_delete_tx(&table, where_clause, Some(&mut sink))?;
                    tx.as_mut().unwrap().ops.extend(sink.ops);
                    Ok(res)
                }
                _ => self.exec_delete_tx(&table, where_clause, None),
            },
            Stmt::CreateTable {
                table,
                if_not_exists,
            } => self.exec_create_table(table, if_not_exists),
            Stmt::DropTable { table, if_exists } => self.exec_drop_table(&table, if_exists),
            Stmt::CreateIndex {
                name,
                table,
                column,
                unique,
                if_not_exists,
            } => self.exec_create_index(&name, &table, &column, unique, if_not_exists),
            Stmt::DropIndex {
                name,
                if_exists,
                table,
            } => self.exec_drop_index(&name, if_exists, table.as_deref()),
            Stmt::AddColumn {
                table,
                column,
                col_type,
                col_default,
            } => self.exec_add_column(&table, &column, col_type, col_default.as_deref()),
            Stmt::DropColumn { table, column } => self.exec_drop_column(&table, &column),
            Stmt::ModifyColumn {
                table,
                column,
                new_name,
                col_type,
                col_default,
            } => self.exec_modify_column(
                &table,
                &column,
                new_name.as_deref(),
                col_type,
                col_default.as_deref(),
            ),
            Stmt::Explain(inner) => self.exec_explain(*inner),
        }
    }

    // ---------- EXPLAIN ----------

    /// EXPLAIN: DML/DDL are described without running (no side effects); a
    /// SELECT runs read-only so the reported access paths are the real ones,
    /// but only the plan is returned — result rows are discarded.
    fn exec_explain(
        &self,
        inner: Stmt,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        match inner {
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
                let (_, rows, plan, _) = self.exec_select(
                    &projection,
                    &from,
                    from_alias.as_deref(),
                    &joins,
                    where_clause,
                    &group_by,
                    having,
                    &order_by,
                    limit,
                    offset,
                )?;
                let n = rows.len();
                Ok((
                    vec!["plan".into()],
                    vec![mk_row(&[("plan", Value::Text(plan.clone()))])],
                    format!("Explain(Select rows={})", n),
                    Some(format!(
                        "{} row(s); the SELECT ran read-only so the plan is real, rows discarded",
                        n
                    )),
                ))
            }
            // v3.6: compound/CTE selects also run read-only so their plan is
            // the real one.
            Stmt::Union { .. } | Stmt::Cte { .. } => {
                let (_, rows, plan, _) = self.exec_select_stmt(&inner)?;
                let n = rows.len();
                Ok((
                    vec!["plan".into()],
                    vec![mk_row(&[("plan", Value::Text(plan.clone()))])],
                    format!("Explain(Select rows={})", n),
                    Some(format!(
                        "{} row(s); the SELECT ran read-only so the plan is real, rows discarded",
                        n
                    )),
                ))
            }
            other => {
                let desc = Self::describe_stmt(&other);
                Ok((
                    vec!["plan".into()],
                    vec![mk_row(&[("plan", Value::Text(desc.clone()))])],
                    format!("Explain({})", desc),
                    Some("statement NOT executed".into()),
                ))
            }
        }
    }

    /// Static plan description for a non-SELECT statement (EXPLAIN without
    /// execution — nothing is mutated).
    fn describe_stmt(stmt: &Stmt) -> String {
        match stmt {
            Stmt::Select { from, .. } => format!("Select(FROM {})", from),
            Stmt::DropColumn { table, column } => {
                format!("AlterTable({} DROP COLUMN {})", table, column)
            }
            Stmt::Insert {
                table,
                rows,
                on_conflict: _,
                ignore: _,
            } => {
                format!("Insert({}, {} row(s))", table, rows.len())
            }
            Stmt::InsertSelect { table, select, .. } => {
                format!("InsertSelect({}, SELECT)", table)
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => {
                let set = assignments
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k, v.describe()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let w = where_clause
                    .as_ref()
                    .map(|e| format!(" WHERE {}", e.describe()))
                    .unwrap_or_default();
                format!("Update({} SET {}{})", table, set, w)
            }
            Stmt::Delete {
                table,
                where_clause,
            } => format!(
                "Delete({}{})",
                table,
                where_clause
                    .as_ref()
                    .map(|e| format!(" WHERE {}", e.describe()))
                    .unwrap_or_default()
            ),
            Stmt::CreateTable {
                table,
                if_not_exists,
            } => format!(
                "CreateTable({}{})",
                table.name,
                if *if_not_exists {
                    " [if-not-exists]"
                } else {
                    ""
                }
            ),
            Stmt::DropTable { table, if_exists } => format!(
                "DropTable({}{})",
                table,
                if *if_exists { " [if-exists]" } else { "" }
            ),
            Stmt::CreateIndex {
                name,
                table,
                column,
                unique,
                ..
            } => format!(
                "CreateIndex({} ON {}({}){})",
                name,
                table,
                column,
                if *unique { " UNIQUE" } else { "" }
            ),
            Stmt::DropIndex { name, table, .. } => format!(
                "DropIndex({}{})",
                name,
                table
                    .as_ref()
                    .map(|t| format!(" ON {}", t))
                    .unwrap_or_default()
            ),
            Stmt::Union {
                left, right, all, ..
            } => format!(
                "Union({}) {} UNION{} {}",
                if *all { "ALL" } else { "DISTINCT" },
                Self::describe_stmt(left),
                if *all { " ALL" } else { "" },
                Self::describe_stmt(right)
            ),
            Stmt::ShowTables => "ShowTables()".into(),
            Stmt::ShowStorage { table, pk } => match pk {
                Some(_) => format!("ShowStorage(point {})", table),
                None => format!("ShowStorage(scan {})", table),
            },
            Stmt::AddColumn { table, column, .. } => {
                format!("AlterTable({} ADD COLUMN {})", table, column)
            }
            Stmt::ModifyColumn {
                table,
                column,
                new_name,
                ..
            } => match new_name {
                Some(n) => format!("AlterTable({} CHANGE COLUMN {} {})", table, column, n),
                None => format!("AlterTable({} MODIFY COLUMN {})", table, column),
            },
            Stmt::Tx { action } => format!("Tx({:?})", action),
            Stmt::Cte { ctes, body } => format!(
                "CTE({}) {}",
                ctes.iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                Self::describe_stmt(body)
            ),
            Stmt::Explain(_) => "Explain(...)".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Secondary-index planning helpers
// ---------------------------------------------------------------------------

/// Arithmetic for `ON DUPLICATE KEY UPDATE col = col + delta` counters
/// (MediaWiki SiteStatsUpdate, WooCommerce stock/counter upserts).
/// Int+Int stays Int; any Float operand promotes the result to Float.
pub(crate) fn value_add(base: Value, delta: Value) -> Result<Value, String> {
    match (base, delta) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a.wrapping_add(b))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
        (Value::Int(a), Value::Float(b)) => Ok(Value::Float(a as f64 + b)),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + b as f64)),
        (Value::Text(a), b) => {
            // MySQL coerces numeric strings: '5'+1 -> 6.
            let a = a.trim().parse::<i64>().ok();
            match (a, b) {
                (Some(a), Value::Int(d)) => Ok(Value::Int(a.wrapping_add(d))),
                (Some(a), Value::Float(d)) => Ok(Value::Float(a as f64 + d)),
                _ => Err("SQL: cannot add non-numeric values in upsert".into()),
            }
        }
        (Value::Null, b) => Ok(b),
        (a, Value::Null) => Ok(a),
        _ => Err("SQL: cannot add non-numeric values in upsert".into()),
    }
}

mod ddl;
mod dml;
mod rows;
mod select;
mod show_tx;
