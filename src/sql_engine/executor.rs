// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::index_scan::*;
use super::refs::*;
use super::lexer::STok;
use super::parser::SParser;
use super::*;
use crate::expr::{CmpOp, Expr};
use crate::relational::{
    encode_index_value, encode_ordered_value, ChromosomeTable, ColumnType, ForeignKeyConstraint,
    RowCodec, SecondaryIndexMeta, Value,
};
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
                let pk = Self::pk_of(table, &row)?;
                let key = Self::row_key(&table.name, &pk);
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
            m.insert(
                "version".to_string(),
                Value::Text(Self::server_version()),
            );
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
                "SQL: unexpected trailing tokens starting at {:?} (unsupported syntax?)",
                p.peek()
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
            Stmt::Insert { table, rows, on_conflict } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_insert_tx(&table, rows, on_conflict, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_insert_tx(&table, rows, on_conflict, None),
                }
            }
            Stmt::InsertSelect { table, cols, select } => {
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
                        let res = self.exec_insert_tx(&table, rows, None, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_insert_tx(&table, rows, None, None),
                }
            }
            Stmt::Update {
                table,
                assignments,
                where_clause,
            } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_update_tx(&table, &assignments, where_clause, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_update_tx(&table, &assignments, where_clause, None),
                }
            }
            Stmt::Delete { table, where_clause } => {
                match tx {
                    Some(buf) if buf.active => {
                        let mut sink = SqlTxBuffer::default();
                        let res = self.exec_delete_tx(&table, where_clause, Some(&mut sink))?;
                        tx.as_mut().unwrap().ops.extend(sink.ops);
                        Ok(res)
                    }
                    _ => self.exec_delete_tx(&table, where_clause, None),
                }
            }
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
            Stmt::Explain(inner) => self.exec_explain(*inner),
        }
    }

    // ---------- SHOW / transactions / CTE ----------

    fn exec_show_tables(
        &self,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schemas = self.schemas.read();
        let mut rows = Vec::new();
        for (name, schema) in schemas.iter() {
            let mut r = BTreeMap::new();
            r.insert("table_name".to_string(), Value::Text(name.clone()));
            r.insert(
                "engine".to_string(),
                Value::Text("LSM+ReedSolomon RS(4+2)".to_string()),
            );
            r.insert(
                "columns".to_string(),
                Value::Int(schema.columns.len() as i64),
            );
            rows.push(r);
        }
        // CTEs are statement-scoped, never listed here.
        let n = rows.len();
        Ok((
            vec!["table_name".into(), "engine".into(), "columns".into()],
            rows,
            format!("ShowTables({} table(s))", n),
            Some(format!("{} table(s)", n)),
        ))
    }

    /// v4.1: SHOW STORAGE — the SQL→DnaQL→DNA storage map. Read-only:
    /// explains where rows physically live, never mutates anything.
    /// Without a PK predicate: one summary row per stored locus. With
    /// `WHERE <pk-col> = <literal>`: per-column detail for that locus,
    /// values decoded to the human-readable form next to the physical
    /// RowCodec representation (nucleotide chains included).
    fn exec_show_storage(
        &self,
        table: &str,
        pk: Option<&(String, Value)>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self.get_schema(table).ok_or_else(|| {
            format!(
                "SQL: relation '{}' does not exist (CREATE TABLE first)",
                table
            )
        })?;
        match pk {
            Some((col, lit)) => {
                if *col != schema.primary_locus_column {
                    return Err(format!(
                        "SQL: SHOW STORAGE filters by the primary key '{}' — '{}' is not the PK of '{}'",
                        schema.primary_locus_column, col, table
                    ));
                }
                let key = Self::row_key(table, lit);
                let payload = self
                    .engine
                    .get(key.as_bytes())
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("SQL: locus {} not found", key))?;
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                let mut rows = Vec::new();
                for (fname, value) in fields {
                    let mut r = BTreeMap::new();
                    r.insert("chromosome".to_string(), Value::Text(table.to_string()));
                    r.insert("locus".to_string(), Value::Text(lit.to_string()));
                    r.insert("storage_key".to_string(), Value::Text(key.clone()));
                    r.insert("column".to_string(), Value::Text(fname.clone()));
                    r.insert(
                        "sql_type".to_string(),
                        Value::Text(schema.column_type_name(&fname)),
                    );
                    r.insert("value".to_string(), Value::Text(value.to_string()));
                    r.insert("stored_as".to_string(), Value::Text(value.stored_as()));
                    rows.push(r);
                }
                let n = rows.len();
                Ok((
                    vec![
                        "chromosome".into(),
                        "locus".into(),
                        "storage_key".into(),
                        "column".into(),
                        "sql_type".into(),
                        "value".into(),
                        "stored_as".into(),
                    ],
                    rows,
                    format!("ShowStorage(point {})", key),
                    Some(format!(
                        "locus {}:{}: {} column(s); values are shown decoded, \
                         stored_as describes the physical RowCodec bytes",
                        table, lit, n
                    )),
                ))
            }
            None => {
                let prefix = format!("{}:", table);
                let raw = self
                    .engine
                    .scan_prefix(prefix.as_bytes())
                    .map_err(|e| e.to_string())?;
                let mut rows = Vec::new();
                for (key, payload) in raw {
                    if String::from_utf8_lossy(&key).ends_with(":schema") {
                        continue;
                    }
                    let fields = RowCodec::decode(&payload).map_err(|e| {
                        format!("row {} unreadable: {}", String::from_utf8_lossy(&key), e)
                    })?;
                    let dna_columns = fields
                        .iter()
                        .filter(|(_, v)| matches!(v, Value::Dna(_)))
                        .count();
                    let key_s = String::from_utf8_lossy(&key).to_string();
                    let locus = key_s
                        .strip_prefix(&prefix)
                        .unwrap_or("?")
                        .to_string();
                    let mut r = BTreeMap::new();
                    r.insert("chromosome".to_string(), Value::Text(table.to_string()));
                    r.insert("locus".to_string(), Value::Text(locus));
                    r.insert("storage_key".to_string(), Value::Text(key_s));
                    r.insert("fields".to_string(), Value::Int(fields.len() as i64));
                    r.insert(
                        "payload_bytes".to_string(),
                        Value::Int(payload.len() as i64),
                    );
                    r.insert("dna_columns".to_string(), Value::Int(dna_columns as i64));
                    rows.push(r);
                }
                let n = rows.len();
                Ok((
                    vec![
                        "chromosome".into(),
                        "locus".into(),
                        "storage_key".into(),
                        "fields".into(),
                        "payload_bytes".into(),
                        "dna_columns".into(),
                    ],
                    rows,
                    format!("ShowStorage(scan {})", table),
                    Some(format!(
                        "chromosome '{}': {} stored locus/loci; add \
                         WHERE <pk> = <literal> for the per-column detail",
                        table, n
                    )),
                ))
            }
        }
    }

    fn exec_tx(
        &self,
        action: TxAction,
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
        match action {
            TxAction::Begin => {
                if tx.as_ref().map(|t| t.active).unwrap_or(false) {
                    return Err("SQL: transaction already active (COMMIT or ROLLBACK first)".into());
                }
                *tx = Some(SqlTxBuffer {
                    ops: Vec::new(),
                    active: true,
                });
                Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("BEGIN".into()))])],
                    "TxBegin".into(),
                    Some("transaction started; DML is buffered until COMMIT".into()),
                ))
            }
            TxAction::Commit => {
                let buf = tx.take();
                match buf {
                    Some(buf) if buf.active => {
                        let n = buf.ops.len();
                        if n > 0 {
                            self.engine
                                .write_batch(&buf.ops)
                                .map_err(|e| format!("COMMIT failed: {}", e))?;
                        }
                        Ok((
                            vec!["status".into()],
                            vec![mk_row(&[("status", Value::Text("COMMIT".into()))])],
                            format!("TxCommit(ops={})", n),
                            Some(format!(
                                "{} buffered operation(s) applied in ONE atomic WAL frame",
                                n
                            )),
                        ))
                    }
                    _ => Err("SQL: COMMIT without an active transaction".into()),
                }
            }
            TxAction::Rollback => {
                let buf = tx.take();
                let n = buf.map(|b| b.ops.len()).unwrap_or(0);
                Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("ROLLBACK".into()))])],
                    format!("TxRollback(discarded={})", n),
                    Some(format!("{} buffered operation(s) discarded", n)),
                ))
            }
        }
    }

    fn exec_cte(
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
            let (_, rows, _cplan, _) = self.exec_stmt(def.clone(), &mut None)?;
            let mut store = self.cte_store.lock().unwrap();
            if store.contains_key(name) {
                return Err(format!("SQL: duplicate CTE name '{}'", name));
            }
            plan.push_str(&format!("{}[{} rows] ", name, rows.len()));
            store.insert(name.clone(), rows);
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
    fn exec_union(
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
            Some(format!("{} row(s) after UNION {}", n, if all { "ALL" } else { "DISTINCT" })),
        ))
    }

    /// Run a SELECT-shaped statement (Select or nested Union/CTE) and return
    /// its result tuple. Used by UNION and EXPLAIN.
    fn exec_select_stmt(
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
    fn stmt_outer_refs(&self, stmt: &Stmt) -> Result<Vec<String>, String> {
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
    fn resolve_expr_for_row(&self, e: &Expr, row: &BTreeMap<String, Value>) -> Result<Expr, String> {
        Ok(match e {
            Expr::Like { operand, pattern, negated } => Expr::Like {
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
            Expr::IsNull(inner) => {
                Expr::IsNull(Box::new(self.resolve_expr_for_row(inner, row)?))
            }
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
    fn bind_outer_refs_select(
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
                Expr::And(a, b) => {
                    Expr::And(Box::new(rewrite(a, row, refs)), Box::new(rewrite(b, row, refs)))
                }
                Expr::Or(a, b) => {
                    Expr::Or(Box::new(rewrite(a, row, refs)), Box::new(rewrite(b, row, refs)))
                }
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
    fn eval_scalar_sub_for_row(
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
        Ok(res
            .rows
            .first()
            .and_then(|r| r.values().next().cloned()))
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
            Stmt::Insert { table, rows, on_conflict: _ } => {
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
            Stmt::Union { left, right, all, .. } => format!(
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
            Stmt::Tx { action } => format!("Tx({:?})", action),
            Stmt::Cte { ctes, body } => format!(
                "CTE({}) {}",
                ctes.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>().join(", "),
                Self::describe_stmt(body)
            ),
            Stmt::Explain(_) => "Explain(...)".into(),
        }
    }

    // ---------- storage helpers ----------

    fn row_key(table: &str, pk: &Value) -> String {
        format!("{}:{}", table, pk)
    }

    fn read_table_rows(&self, table: &str) -> Result<Vec<BTreeMap<String, Value>>, String> {
        // v3.6: statement-scoped CTE tables take precedence over disk.
        if let Some(rows) = self.cte_store.lock().unwrap().get(table) {
            return Ok(rows.clone());
        }
        let prefix = format!("{}:", table);
        let raw = self
            .engine
            .scan_prefix(prefix.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (key, payload) in raw {
            // skip the schema meta-row
            if key.ends_with(b":schema") || String::from_utf8_lossy(&key).ends_with(":schema") {
                continue;
            }
            let fields = RowCodec::decode(&payload)
                .map_err(|e| format!("row {} unreadable: {}", String::from_utf8_lossy(&key), e))?;
            out.push(fields.into_iter().collect());
        }
        Ok(out)
    }

    fn pk_of(schema: &ChromosomeTable, row: &BTreeMap<String, Value>) -> Result<Value, String> {
        row.get(&schema.primary_locus_column)
            .cloned()
            .ok_or_else(|| format!("row missing PK column '{}'", schema.primary_locus_column))
    }

    /// Fetch a single row by full storage key '<table>:<pk>'.
    fn read_row_by_key(&self, row_key: &str) -> Result<Option<BTreeMap<String, Value>>, String> {
        if row_key.ends_with(":schema") || row_key.starts_with("__idx:") {
            return Ok(None);
        }
        match self
            .engine
            .get(row_key.as_bytes())
            .map_err(|e| e.to_string())?
        {
            Some(payload) => {
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", row_key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    // -------- primary-key point lookup (no full scan) --------
    // Rows are keyed '<table>:<pk>', so a WHERE that pins the primary key can
    // resolve via the engine's keyed get (memtable skiplist + segment sparse
    // index + bloom filter) instead of decoding the whole chromosome.
    //
    // Safety rule: we only take this path when EVERY logical branch of the
    // predicate pins the pk to a literal — i.e. OR of AND-conjunctions, each
    // containing '<pk> = <literal>'. The full WHERE is still evaluated on the
    // fetched row(s) afterwards, so results are identical to SeqScan; only the
    // candidate set shrinks.
    fn pk_eq_literals(expr: &Expr, pk_col: &str) -> Option<Vec<Value>> {
        match expr {
            Expr::Or(a, b) => {
                let mut out = Self::pk_eq_literals(a, pk_col)?;
                out.extend(Self::pk_eq_literals(b, pk_col)?);
                Some(out)
            }
            _ => {
                fn find(e: &Expr, pk_col: &str) -> Option<Value> {
                    match e {
                        Expr::And(a, b) => find(a, pk_col).or_else(|| find(b, pk_col)),
                        Expr::Cmp {
                            op: CmpOp::Eq,
                            left,
                            right,
                        } => {
                            let lit = match (left.as_ref(), right.as_ref()) {
                                (Expr::Column(c), Expr::Literal(v)) if c == pk_col => Some(v),
                                (Expr::Literal(v), Expr::Column(c)) if c == pk_col => Some(v),
                                _ => None,
                            }?;
                            (!lit.is_null()).then(|| lit.clone())
                        }
                        _ => None,
                    }
                }
                find(expr, pk_col).map(|lit| vec![lit])
            }
        }
    }

    /// Fetch a single row by storage key '<table>:<pk>' (keyed get, no scan).
    fn read_row_point(
        &self,
        table: &str,
        pk: &Value,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let key = Self::row_key(table, pk);
        // Same visibility rule as read_table_rows: '<table>:schema' is meta.
        if key.ends_with(":schema") {
            return Ok(None);
        }
        match self.engine.get(key.as_bytes()).map_err(|e| e.to_string())? {
            Some(payload) => {
                let fields = RowCodec::decode(&payload)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    // ---------- statements ----------

    fn exec_create_table(
        &self,
        table: ChromosomeTable,
        if_not_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        {
            let schemas = self.schemas.read();
            if schemas.contains_key(&table.name) {
                if if_not_exists {
                    return Ok((
                        vec!["status".into()],
                        vec![mk_row(&[("status", Value::Text("EXISTS_OK".into()))])],
                        format!("CreateTable({}) [if-not-exists]", table.name),
                        Some(format!("table '{}' already exists", table.name)),
                    ));
                }
                return Err(format!("table '{}' already exists", table.name));
            }
        }
        let payload = serde_json::to_vec(&table).map_err(|e| e.to_string())?;
        self.engine
            .put(format!("__schema:{}", table.name).as_bytes(), &payload)
            .map_err(|e| e.to_string())?;

        for fk in &table.foreign_keys {
            self.complement_index
                .write()
                .register_column(&table.name, &fk.column);
        }
        self.schemas
            .write()
            .insert(table.name.clone(), table.clone());

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("CREATED".into()))])],
            format!(
                "CreateTable({}) cols={} fks={}",
                table.name,
                table.columns.len(),
                table.foreign_keys.len()
            ),
            None,
        ))
    }

    fn exec_create_index(
        &self,
        name: &str,
        table: &str,
        column: &str,
        unique: bool,
        if_not_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut table_schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' does not exist", table))?;
        if !table_schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: cannot create index '{}': column '{}' does not exist on '{}'",
                name, column, table
            ));
        }
        if table_schema
            .secondary_indexes
            .iter()
            .any(|i| i.name == name)
        {
            if if_not_exists {
                return Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("EXISTS_OK".into()))])],
                    format!("CreateIndex({}) [if-not-exists]", name),
                    Some(format!("index '{}' already exists", name)),
                ));
            }
            return Err(format!("index '{}' already exists", name));
        }
        if table_schema.secondary_index_on(column).is_some() {
            // CMS edition: MySQL happily keeps several indexes over one
            // column (dbDelta churn creates them); allow duplicates there.
            #[cfg(not(feature = "cms"))]
            return Err(format!(
                "SQL: column '{}.{}' already has an index (rename it there or drop it first)",
                table, column
            ));
            #[cfg(feature = "cms")]
            let _ = (table, column);
        }

        // Backfill: one WAL batch of index entries over the existing rows.
        // A UNIQUE index must verify existing data first — duplicates abort
        // before anything is written.
        let rows = self.read_table_rows(table)?;
        let base = index_prefix(table, column);
        let mut batch = Vec::with_capacity(rows.len());
        let mut indexed = 0usize;
        if unique {
            let mut seen: std::collections::HashMap<Vec<u8>, Value> =
                std::collections::HashMap::new();
            for row in &rows {
                let Some(v) = row.get(column) else { continue };
                if v.is_null() {
                    continue; // SQL: NULLs never conflict in a UNIQUE index
                }
                let enc = encode_ordered_value(v);
                if seen.contains_key(&enc) {
                    return Err(format!(
                        "SQL: cannot create UNIQUE index '{}': duplicate value {} on column '{}.{}'",
                        name, v, table, column
                    ));
                }
                seen.insert(enc, v.clone());
            }
        }
        for row in &rows {
            let Some(v) = row.get(column) else { continue };
            if v.is_null() {
                continue; // NULLs are never index hits (SQL semantics)
            }
            let pk = Self::pk_of(&table_schema, row)?;
            let row_key = Self::row_key(table, &pk);
            let mut key = base.clone();
            key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
            key.extend_from_slice(row_key.as_bytes());
            batch.push((crate::wal::WalOp::Put, key, Some(row_key.into_bytes())));
            indexed += 1;
        }
        if !batch.is_empty() {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("index backfill failed: {}", e))?;
        }

        table_schema.secondary_indexes.push(SecondaryIndexMeta {
            name: name.to_string(),
            column: column.to_string(),
            unique,
        });
        self.persist_schema(&table_schema)?;

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("CREATED".into()))])],
            format!(
                "CreateIndex({}{} ON {}.{} rows={})",
                if unique { "UNIQUE " } else { "" },
                name,
                table,
                column,
                indexed
            ),
            Some(format!(
                "{}index '{}' created on {}.{} ({} entries)",
                if unique { "UNIQUE " } else { "" },
                name,
                table,
                column,
                indexed
            )),
        ))
    }

    fn exec_drop_index(
        &self,
        name: &str,
        if_exists: bool,
        table: Option<&str>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        // Locate the index: within the `ON <table>` table when qualified,
        // otherwise across all schemas (legacy bare `DROP INDEX name`).
        let target = {
            let schemas = self.schemas.read();
            match table {
                Some(want) => schemas.get(want).and_then(|t| {
                    t.secondary_indexes
                        .iter()
                        .find(|i| i.name == name)
                        .map(|i| (t.name.clone(), i.column.clone()))
                }),
                None => schemas.values().find_map(|t| {
                    t.secondary_indexes
                        .iter()
                        .find(|i| i.name == name)
                        .map(|i| (t.name.clone(), i.column.clone()))
                }),
            }
        };
        let Some((table, column)) = target else {
            if if_exists {
                return Ok((
                    vec!["status".into()],
                    vec![mk_row(&[("status", Value::Text("DROP_OK".into()))])],
                    format!("DropIndex({}) [if-exists]", name),
                    None,
                ));
            }
            return Err(match table {
                Some(want) => format!("index '{}' does not exist on table '{}'", name, want),
                None => format!("index '{}' does not exist", name),
            });
        };

        // Delete every entry of this column's index in one batch.
        let base = index_prefix(&table, &column);
        let entries = self.engine.scan_prefix(&base).map_err(|e| e.to_string())?;
        let batch: Vec<_> = entries
            .into_iter()
            .map(|(k, _)| (crate::wal::WalOp::Delete, k, None))
            .collect();
        let removed = batch.len();
        if !batch.is_empty() {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("index drop failed: {}", e))?;
        }

        let mut schema = self
            .get_schema(&table)
            .ok_or_else(|| format!("table '{}' vanished", table))?;
        schema.secondary_indexes.retain(|i| i.name != name);
        self.persist_schema(&schema)?;

        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!(
                "DropIndex({} on {}.{}) entries={}",
                name, table, column, removed
            ),
            Some(format!(
                "index '{}' dropped ({} entries removed)",
                name, removed
            )),
        ))
    }

    /// Persist a table schema row (CREATE INDEX / DROP INDEX mutate it).
    fn persist_schema(&self, table: &ChromosomeTable) -> Result<(), String> {
        let payload = serde_json::to_vec(table).map_err(|e| e.to_string())?;
        self.engine
            .put(format!("__schema:{}", table.name).as_bytes(), &payload)
            .map_err(|e| e.to_string())?;
        self.schemas
            .write()
            .insert(table.name.clone(), table.clone());
        Ok(())
    }

    /// v3.9 Online DDL: `ALTER TABLE t ADD COLUMN c T` — metadata-only and
    /// instant: the schema row is rewritten (one WAL batch), no existing
    /// row is touched, reads of the new column on old rows yield NULL.
    fn exec_add_column(
        &self,
        table: &str,
        column: &str,
        col_type: ColumnType,
        col_default: Option<&str>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut schema = self
            .get_schema(table)
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        if schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: column '{}' already exists in table '{}'",
                column, table
            ));
        }
        schema.add_column(column.to_string(), col_type);
        if let Some(def) = col_default {
            schema
                .column_defaults
                .insert(column.to_string(), def.to_string());
        }
        let start = std::time::Instant::now();
        self.persist_schema(&schema)?;
        let us = start.elapsed().as_micros() as u64;
        Ok((
            vec!["status".into(), "table".into(), "column".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("table", Value::Text(table.to_string())),
                ("column", Value::Text(column.to_string())),
            ])],
            format!(
                "AlterTable({}, instant metadata-only, {} µs, no row rewrite)",
                table, us
            ),
            Some(format!(
                "column '{}' added to '{}' without blocking reads or writes",
                column, table
            )),
        ))
    }

    /// CMS edition / generic: `ALTER TABLE t DROP COLUMN c` — metadata-only
    /// drop (schema row rewritten; stored payloads keep the old field but it
    /// is no longer visible in queries).
    fn exec_drop_column(
        &self,
        table: &str,
        column: &str,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let mut schema = self
            .get_schema(table)
            .ok_or_else(|| format!("SQL: relation '{}' does not exist", table))?;
        if !schema.columns.contains_key(column) {
            return Err(format!(
                "SQL: can't DROP COLUMN '{}'; check that it exists in table '{}'",
                column, table
            ));
        }
        schema.columns.remove(column);
        schema.column_defaults.remove(column);
        // CMS edition: keep the declaration-order list in sync.
        schema.column_order.retain(|c| c != column);
        schema.secondary_indexes.retain(|idx| idx.column != column);
        if schema.auto_increment_column.as_deref() == Some(column) {
            schema.auto_increment_column = None;
        }
        self.persist_schema(&schema)?;
        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!("AlterTable({} DROP COLUMN {})", table, column),
            Some(format!("column '{}' dropped from '{}'", column, table)),
        ))
    }

    /// Enforce every UNIQUE secondary index of `table` for the row `new`
    /// (full row map + its row key). `old_key`, when present, is the key of
    /// the row being replaced: its own current index entries don't count as
    /// conflicts. A non-NULL `new` value must appear in the index at most
    /// once besides the row itself.
    fn check_unique_violations(
        &self,
        table: &str,
        schema: &ChromosomeTable,
        row_key: &str,
        new: &BTreeMap<String, Value>,
        old_key: Option<&str>,
    ) -> Result<(), String> {
        for idx in schema.secondary_indexes.iter().filter(|i| i.unique) {
            let Some(v) = new.get(&idx.column) else {
                continue;
            };
            if v.is_null() {
                continue; // SQL: multiple NULLs never conflict
            }
            let mut prefix = index_prefix(table, &idx.column);
            prefix.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
            let entries = self
                .engine
                .scan_prefix(&prefix)
                .map_err(|e| e.to_string())?;
            for (_, payload) in entries {
                let Ok(holder) = String::from_utf8(payload) else {
                    continue;
                };
                if holder == row_key {
                    continue; // this row's own (unchanged) entry
                }
                if Some(holder.as_str()) == old_key {
                    continue; // entry being replaced by this very update
                }
                return Err(format!(
                    "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                    table, idx.column, v, idx.name
                ));
            }
        }
        Ok(())
    }

    /// Secondary-index write batch for a row change: entries whose key/value
    /// differs between `old` and `new` (empty when no indexed column moved).
    fn index_delta_batch(
        schema: &ChromosomeTable,
        old: Option<(&str, &BTreeMap<String, Value>)>,
        new: Option<(&str, &BTreeMap<String, Value>)>,
    ) -> Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)> {
        let mut batch = Vec::new();
        if schema.secondary_indexes.is_empty() {
            return batch;
        }
        for idx in &schema.secondary_indexes {
            let col = &idx.column;
            let old_entry = old.and_then(|(table, row)| {
                row.get(col).filter(|v| !v.is_null()).and_then(|v| {
                    let row_key = Self::row_key(table, &Self::pk_of(schema, row).ok()?);
                    let mut key = index_prefix(table, col);
                    key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    key.extend_from_slice(row_key.as_bytes());
                    Some((key, row_key))
                })
            });
            let new_entry = new.and_then(|(table, row)| {
                row.get(col).filter(|v| !v.is_null()).and_then(|v| {
                    let row_key = Self::row_key(table, &Self::pk_of(schema, row).ok()?);
                    let mut key = index_prefix(table, col);
                    key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    key.extend_from_slice(row_key.as_bytes());
                    Some((key, row_key))
                })
            });
            match (old_entry, new_entry) {
                (Some((old_key, _)), Some((new_key, _))) if old_key == new_key => {}
                (old_e, new_e) => {
                    if let Some((old_key, _)) = old_e {
                        batch.push((crate::wal::WalOp::Delete, old_key, None));
                    }
                    if let Some((new_key, new_row_key)) = new_e {
                        batch.push((
                            crate::wal::WalOp::Put,
                            new_key,
                            Some(new_row_key.into_bytes()),
                        ));
                    }
                }
            }
        }
        batch
    }

    fn exec_drop_table(
        &self,
        table: &str,
        if_exists: bool,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        {
            let schemas = self.schemas.read();
            if !schemas.contains_key(table) {
                if if_exists {
                    return Ok((
                        vec!["status".into()],
                        vec![mk_row(&[("status", Value::Text("DROP_OK".into()))])],
                        format!("DropTable({}) [if-exists]", table),
                        None,
                    ));
                }
                return Err(format!("table '{}' does not exist", table));
            }
        }
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' does not exist", table))?;
        let rows = self.read_table_rows(table)?;
        if !schema.secondary_indexes.is_empty() || !rows.is_empty() {
            let mut batch: Vec<_> = rows
                .iter()
                .filter_map(|row| {
                    Self::pk_of(&schema, row)
                        .ok()
                        .map(|pk| Self::row_key(table, &pk))
                })
                .map(|key| (crate::wal::WalOp::Delete, key.into_bytes(), None))
                .collect();
            for row in &rows {
                batch.extend(Self::index_delta_batch(&schema, Some((table, row)), None));
            }
            self.engine.write_batch(&batch).map_err(|e| e.to_string())?;
        }
        // Sweep any leftover index entries (e.g. rows added out-of-band).
        let base = index_table_prefix(table);
        let leftovers = self.engine.scan_prefix(&base).map_err(|e| e.to_string())?;
        if !leftovers.is_empty() {
            let sweep: Vec<_> = leftovers
                .into_iter()
                .map(|(k, _)| (crate::wal::WalOp::Delete, k, None))
                .collect();
            self.engine.write_batch(&sweep).map_err(|e| e.to_string())?;
        }
        self.engine
            .delete(format!("__schema:{}", table).as_bytes())
            .map_err(|e| e.to_string())?;
        self.schemas.write().remove(table);
        // CMS edition: drop the AUTO_INCREMENT counter with the table —
        // a recreated table must start from 1 again (MySQL parity).
        #[cfg(feature = "cms")]
        self.cms_sequences.reset(table);
        Ok((
            vec!["status".into()],
            vec![mk_row(&[("status", Value::Text("DROPPED".into()))])],
            format!("DropTable({})", table),
            None,
        ))
    }

    fn exec_insert_tx(
        &self,
        table: &str,
        rows: Vec<Vec<(String, Value)>>,
        on_conflict: Option<OnConflict>,
        mut sink: Option<&mut SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered (CREATE TABLE first)", table))?;

        // FK validation
        for row in &rows {
            for fk in &schema.foreign_keys {
                if let Some((_, v)) = row.iter().find(|(c, _)| c == &fk.column) {
                    if !v.is_null() {
                        let target_key = Self::row_key(&fk.target_chromosome, v);
                        let exists = self
                            .engine
                            .get(target_key.as_bytes())
                            .map_err(|e| e.to_string())?
                            .is_some();
                        if !exists {
                            return Err(format!(
                                "FK violation: {}={} not found in '{}'",
                                fk.column, v, fk.target_chromosome
                            ));
                        }
                    }
                }
            }
        }

        let mut batch = Vec::new();
        let mut inserted_keys = Vec::new();
        let mut skipped_conflicts = 0usize;
        let mut upserted = 0usize;
        let mut seen_keys = std::collections::HashSet::new();
        // UNIQUE indexes: (column, encoded value) -> owning row key, so two
        // rows of one multi-row INSERT cannot slip the same value past us.
        let mut stmt_unique: std::collections::HashMap<(String, Vec<u8>), String> =
            std::collections::HashMap::new();
        for row in rows {
            let map: BTreeMap<String, Value> = row.into_iter().collect();
            // CMS edition: AUTO_INCREMENT assignment. A missing / NULL / 0
            // value for the sequence column gets the next generated id;
            // an explicit id > 0 moves the counter forward (MySQL keeps
            // monotonicity after explicit inserts). Seeded lazily from
            // MAX(pk) so a restarted server resumes the sequence.
            #[cfg(feature = "cms")]
            let map = {
                let mut map = map;
                if let Some(auto_col) = schema.auto_increment_column.clone() {
                    let needs_id = match map.get(&auto_col) {
                        None | Some(Value::Null) => true,
                        Some(Value::Int(0)) => true,
                        _ => false,
                    };
                    if needs_id {
                        let prefix = format!("{}:", table);
                        let seed = || -> Result<u64, String> {
                            let raw = self
                                .engine
                                .scan_prefix(prefix.as_bytes())
                                .map_err(|e| e.to_string())?;
                            let mut max = 0u64;
                            for (k, _) in raw {
                                let key = String::from_utf8_lossy(&k).to_string();
                                if let Some(sfx) = key.strip_prefix(prefix.as_str()) {
                                    if let Ok(v) = sfx.parse::<u64>() {
                                        max = max.max(v);
                                    }
                                }
                            }
                            Ok(max)
                        };
                        let next_id = self.cms_sequences.next(table, seed)?;
                        map.insert(auto_col, Value::Int(next_id as i64));
                    } else if let Some(Value::Int(v)) = map.get(&auto_col) {
                        if *v > 0 {
                            self.cms_sequences.bump_to(table, *v as u64);
                        }
                    }
                }
                map
            };
            // CMS edition: DEFAULT fill — omitted columns get their effective
            // default (explicit DEFAULT or NOT NULL zero-value), matching
            // MySQL non-strict INSERT behavior (CMS_EDITION_RU.md §4.11).
            #[cfg(feature = "cms")]
            let map = {
                let mut map = map;
                for (col, def) in &schema.column_defaults {
                    if map.contains_key(col) {
                        continue;
                    }
                    let v = if def.eq_ignore_ascii_case("NULL") {
                        Value::Null
                    } else if let Ok(i) = def.parse::<i64>() {
                        Value::Int(i)
                    } else if let Ok(f) = def.parse::<f64>() {
                        Value::Float(f)
                    } else {
                        Value::Text(def.clone())
                    };
                    map.insert(col.clone(), v);
                }
                map
            };
            // CMS edition: column-type coercion on write. MySQL stores the
            // value with the COLUMN's type, not the literal's — wpdb::insert()
            // without a format array sends every value as a string, so an
            // `order_id` reaches `bigint unsigned` as '15'. Without the same
            // conversion the secondary-index key (encoded from the stored
            // Text) never matches lookups by an Int literal (IndexScan →
            // 0 rows) — WooCommerce order items/stock broke exactly this way.
            #[cfg(feature = "cms")]
            let map = {
                let mut map = map;
                for (col, v) in map.iter_mut() {
                    if let Some(ct) = schema.columns.get(col.as_str()) {
                        *v = Value::coerce_for_column(ct, v.clone());
                    }
                }
                map
            };
            let pk = Self::pk_of(&schema, &map)?;
            let key = Self::row_key(table, &pk);
            if !seen_keys.insert(key.clone()) {
                // v3.6: same statement hit the same PK — ON CONFLICT applies.
                match on_conflict {
                    Some(OnConflict::DoNothing) => {
                        skipped_conflicts += 1;
                        continue;
                    }
                    _ => return Err(format!("duplicate primary key in INSERT: {}", key)),
                }
            }
            if self
                .engine
                .get(key.as_bytes())
                .map_err(|error| error.to_string())?
                .is_some()
            {
                // v3.6: existing row + ON CONFLICT → UPSERT / skip.
                match &on_conflict {
                    Some(OnConflict::DoNothing) => {
                        skipped_conflicts += 1;
                        continue;
                    }
                    Some(OnConflict::DoUpdate { assignments }) => {
                        let raw = self
                            .engine
                            .get(key.as_bytes())
                            .map_err(|error| error.to_string())?
                            .expect("row existence checked above");
                        let mut existing: BTreeMap<String, Value> =
                            RowCodec::decode(&raw)
                                .map_err(|e| format!("row {} unreadable: {}", key, e))?
                                .into_iter()
                                .collect();
                        for (col, val) in assignments {
                            let v = match val {
                                UpsertValue::Literal(v) => v.clone(),
                                UpsertValue::Excluded(source) => map
                                    .get(source)
                                    .cloned()
                                    .ok_or_else(|| {
                                        format!(
                                            "SQL: EXCLUDED.{} not present in the conflicting row",
                                            source
                                        )
                                    })?,
                            };
                            existing.insert(col.clone(), v);
                        }
                        // PK must not change via DO UPDATE SET.
                        let new_pk = Self::pk_of(&schema, &existing)?;
                        if Self::row_key(table, &new_pk) != key {
                            return Err(
                                "SQL: ON CONFLICT DO UPDATE SET cannot change the primary key".into()
                            );
                        }
                        let payload = RowCodec::encode(
                            &existing
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<Vec<_>>(),
                        );
                        batch.extend(Self::index_delta_batch(
                            &schema,
                            None,
                            Some((table, &existing)),
                        ));
                        batch.push((
                            crate::wal::WalOp::Put,
                            key.clone().into_bytes(),
                            Some(payload),
                        ));
                        upserted += 1;
                        continue;
                    }
                    None => return Err(format!("duplicate primary key in INSERT: {}", key)),
                }
            }
            // UNIQUE index enforcement — also covers duplicates between rows
            // of this same statement (via stmt_unique) and against rows that
            // already exist in the engine (via index scan).
            let mut handled_by_unique_upsert = false;
            for idx in schema.secondary_indexes.iter().filter(|i| i.unique) {
                let Some(v) = map.get(&idx.column) else {
                    continue;
                };
                if v.is_null() {
                    continue;
                }
                let enc = encode_ordered_value(v);
                if let Some(earlier) = stmt_unique.get(&(idx.column.clone(), enc.clone())) {
                    if earlier != &key {
                        return Err(format!(
                            "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                            table, idx.column, v, idx.name
                        ));
                    }
                }
                let mut prefix = index_prefix(table, &idx.column);
                prefix.extend_from_slice(&escape_index_part(&enc));
                let entries = self
                    .engine
                    .scan_prefix(&prefix)
                    .map_err(|e| e.to_string())?;
                if !entries.is_empty() {
                    // CMS edition: `ON DUPLICATE KEY UPDATE` fires on ANY
                    // unique-key conflict (not only the PK). Resolve the
                    // conflicting row through the index entry (the entry
                    // value holds the row key) and apply DO UPDATE.
                    #[cfg(feature = "cms")]
                    if let Some(OnConflict::DoUpdate { assignments }) = &on_conflict {
                        let row_key_bytes = entries[0].1.clone();
                        let raw = self
                            .engine
                            .get(&row_key_bytes)
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| {
                                "UNIQUE index entry points to a missing row".to_string()
                            })?;
                        let mut existing: BTreeMap<String, Value> = RowCodec::decode(&raw)
                            .map_err(|e| format!("row unreadable via unique index: {}", e))?
                            .into_iter()
                            .collect();
                        for (col, val) in assignments {
                            let nv = match val {
                                UpsertValue::Literal(l) => l.clone(),
                                UpsertValue::Excluded(source) => map
                                    .get(source)
                                    .cloned()
                                    .ok_or_else(|| {
                                        format!(
                                            "SQL: VALUES({}) not present in the inserted row",
                                            source
                                        )
                                    })?,
                            };
                            existing.insert(col.clone(), nv);
                        }
                        let new_pk = Self::pk_of(&schema, &existing)?;
                        let new_key = Self::row_key(table, &new_pk);
                        if new_key.as_bytes() != row_key_bytes.as_slice() {
                            return Err(
                                "SQL: ON DUPLICATE KEY UPDATE cannot change the primary key".into(),
                            );
                        }
                        let payload = RowCodec::encode(
                            &existing
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect::<Vec<_>>(),
                        );
                        batch.extend(Self::index_delta_batch(
                            &schema,
                            None,
                            Some((table, &existing)),
                        ));
                        batch.push((
                            crate::wal::WalOp::Put,
                            row_key_bytes.clone(),
                            Some(payload),
                        ));
                        upserted += 1;
                        handled_by_unique_upsert = true;
                        break;
                    }
                    return Err(format!(
                        "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                        table, idx.column, v, idx.name
                    ));
                }
                stmt_unique.insert((idx.column.clone(), enc), key.clone());
            }
            if handled_by_unique_upsert {
                continue;
            }
            let payload = RowCodec::encode(
                &map.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            // Secondary-index entries ride the same WAL batch (atomic with
            // the row: a crash can never leave a dangling index entry).
            batch.extend(Self::index_delta_batch(&schema, None, Some((table, &map))));
            batch.push((
                crate::wal::WalOp::Put,
                key.clone().into_bytes(),
                Some(payload),
            ));
            inserted_keys.push((key, map, pk));
        }

        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("insert failed: {}", e))?;
        }

        // maintain complement index
        {
            let mut idx = self.complement_index.write();
            for (key, map, _) in &inserted_keys {
                for fk in &schema.foreign_keys {
                    if let Some(v) = map.get(&fk.column) {
                        idx.upsert(table, &fk.column, &key.clone(), v);
                    }
                }
            }
        }

        let n = inserted_keys.len();
        let mut out_rows = Vec::new();
        for (key, _, pk) in &inserted_keys {
            let mut r = BTreeMap::new();
            r.insert("inserted_key".to_string(), Value::Text(key.clone()));
            r.insert("pk".to_string(), pk.clone());
            out_rows.push(r);
        }

        let msg = match (upserted, skipped_conflicts) {
            (0, 0) => format!("{} row(s) inserted", n),
            (u, s) => format!(
                "{} row(s) inserted, {} upserted, {} skipped (ON CONFLICT)",
                n, u, s
            ),
        };
        Ok((
            vec!["inserted_key".into(), "pk".into()],
            out_rows,
            format!(
                "BatchInsert({}) rows={} via single WAL frame{}",
                table,
                n + upserted,
                if on_conflict.is_some() { " + UPSERT" } else { "" }
            ),
            Some(msg),
        ))
    }

    fn exec_update_tx(
        &self,
        table: &str,
        assignments: &[(String, Expr)],
        where_clause: Option<Expr>,
        mut sink: Option<&mut SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered", table))?;
        // Materialise IN-subqueries, then point-get when WHERE pins the PK.
        let where_clause = self.materialize_subqueries(where_clause)?;
        let pk_lits = where_clause
            .as_ref()
            .and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point(table, lit)? {
                        out.push(row);
                    }
                }
                out
            }
            None => self.read_table_rows(table)?,
        };

        for (column, _) in assignments {
            if !schema.columns.contains_key(column) {
                return Err(format!("unknown column '{}' in UPDATE", column));
            }
        }

        let mut changes = Vec::new();
        let mut affected_old_keys = std::collections::HashSet::new();
        for row in rows {
            let matches = where_clause
                .as_ref()
                .map(|e| e.eval_bool(&row))
                .unwrap_or(true);
            if !matches {
                continue;
            }
            let old_pk = Self::pk_of(&schema, &row)?;
            let old_key = Self::row_key(table, &old_pk);
            let mut new_row = row.clone();
            for (col, e) in assignments {
                // MySQL semantics: every SET rhs sees the row's OLD values
                // (`stock_quantity = stock_quantity - 1`). Eval failure
                // (unknown column/subquery) yields NULL — three-valued logic.
                let mut v = e.eval(&row).unwrap_or(Value::Null);
                // CMS edition: store with the COLUMN's type (MariaDB coerces
                // on write; wpdb sends strings everywhere — see exec_insert_tx).
                #[cfg(feature = "cms")]
                if let Some(ct) = schema.columns.get(col.as_str()) {
                    v = Value::coerce_for_column(ct, v);
                }
                new_row.insert(col.clone(), v);
            }
            let pk = Self::pk_of(&schema, &new_row)?;
            let key = Self::row_key(table, &pk);
            affected_old_keys.insert(old_key.clone());
            changes.push((old_key, row, key, new_row));
        }

        let mut new_keys = std::collections::HashSet::new();
        for (old_key, _, new_key, new_row) in &changes {
            if !new_keys.insert(new_key.clone()) {
                return Err(format!(
                    "UPDATE would create duplicate primary key: {}",
                    new_key
                ));
            }
            if !affected_old_keys.contains(new_key)
                && self
                    .engine
                    .get(new_key.as_bytes())
                    .map_err(|error| error.to_string())?
                    .is_some()
            {
                return Err(format!(
                    "UPDATE conflicts with existing primary key: {}",
                    new_key
                ));
            }

            // UNIQUE secondary-index constraints (target row excluded).
            self.check_unique_violations(table, &schema, new_key, new_row, Some(old_key.as_str()))?;

            for fk in &schema.foreign_keys {
                if let Some(value) = new_row.get(&fk.column) {
                    if !value.is_null() {
                        let parent_key = Self::row_key(&fk.target_chromosome, value);
                        if self
                            .engine
                            .get(parent_key.as_bytes())
                            .map_err(|error| error.to_string())?
                            .is_none()
                        {
                            return Err(format!(
                                "FK violation: {}={} not found in '{}'",
                                fk.column, value, fk.target_chromosome
                            ));
                        }
                    }
                }
            }
        }

        let mut batch = Vec::new();
        for (old_key, _, new_key, _) in &changes {
            if old_key != new_key {
                batch.push((
                    crate::wal::WalOp::Delete,
                    old_key.clone().into_bytes(),
                    None,
                ));
            }
        }
        for (old_key, old_row, new_key, new_row) in &changes {
            // Index delta: old value entry removed / new one added in the
            // same atomic batch as the row rewrite.
            let old_pk = Self::pk_of(&schema, old_row)?;
            let old_table_key = Self::row_key(&schema.name, &old_pk);
            let new_pk = Self::pk_of(&schema, new_row)?;
            let new_table_key = Self::row_key(&schema.name, &new_pk);
            let _ = (old_key, new_key);
            batch.extend(Self::index_delta_batch(
                &schema,
                Some((schema.name.as_str(), old_row)),
                Some((schema.name.as_str(), new_row)),
            ));
            // Row-key move (pk change) also moves index suffixes: the delta
            // above already computed fresh keys from the new row/pk.
            let _ = (&old_table_key, &new_table_key);
            let payload = RowCodec::encode(
                &new_row
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            batch.push((
                crate::wal::WalOp::Put,
                new_key.clone().into_bytes(),
                Some(payload),
            ));
        }
        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("update failed: {}", e))?;
        }

        {
            let mut index = self.complement_index.write();
            for (old_key, old_row, _, _) in &changes {
                for fk in &schema.foreign_keys {
                    if let Some(value) = old_row.get(&fk.column) {
                        index.remove(&schema.name, &fk.column, old_key, value);
                    }
                }
            }
            for (_, _, new_key, new_row) in &changes {
                for fk in &schema.foreign_keys {
                    if let Some(value) = new_row.get(&fk.column) {
                        if !value.is_null() {
                            index.upsert(&schema.name, &fk.column, new_key, value);
                        }
                    }
                }
            }
        }

        let mut updated = changes.len();
        // CMS edition (MySQL parity): UPDATE's affected count is the number
        // of rows whose values actually CHANGED — wpdb branches on it.
        #[cfg(feature = "cms")]
        {
            updated = changes
                .iter()
                .filter(|(_, old, _, new)| !row_maps_equal(old, new))
                .count();
        }
        let scan_op = match &pk_lits {
            Some(lits) if lits.len() == 1 => format!("PointGet({} = {})", table, lits[0]),
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!("SeqScan({})", table),
        };
        Ok((
            vec!["affected_rows".into()],
            vec![mk_row(&[("affected_rows", Value::Int(updated as i64))])],
            format!(
                "{} -> Update({:?})",
                scan_op,
                assignments.iter().map(|(c, _)| c).collect::<Vec<_>>()
            ),
            Some(format!("{} row(s) updated", updated)),
        ))
    }

    fn exec_delete_tx(
        &self,
        table: &str,
        where_clause: Option<Expr>,
        mut sink: Option<&mut SqlTxBuffer>,
    ) -> Result<
        (
            Vec<String>,
            Vec<BTreeMap<String, Value>>,
            String,
            Option<String>,
        ),
        String,
    > {
        let schema = self
            .get_schema(table)
            .ok_or_else(|| format!("table '{}' not registered", table))?;
        // Materialise IN-subqueries, then point-get when WHERE pins the PK.
        let where_clause = self.materialize_subqueries(where_clause)?;
        let pk_lits = where_clause
            .as_ref()
            .and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point(table, lit)? {
                        out.push(row);
                    }
                }
                out
            }
            None => self.read_table_rows(table)?,
        };

        // Which tables reference this one (for CASCADE)?
        let referencing: Vec<(String, ForeignKeyConstraint)> = {
            let schemas = self.schemas.read();
            schemas
                .iter()
                .flat_map(|(tname, t)| {
                    t.foreign_keys
                        .iter()
                        .filter(|fk| fk.target_chromosome == table)
                        .map(move |fk| (tname.clone(), fk.clone()))
                })
                .collect()
        };

        let mut selected = Vec::new();
        for row in rows {
            let matches = where_clause
                .as_ref()
                .map(|e| e.eval_bool(&row))
                .unwrap_or(true);
            if !matches {
                continue;
            }
            let pk = Self::pk_of(&schema, &row)?;
            selected.push((Self::row_key(table, &pk), pk, row));
        }

        let selected_keys: std::collections::HashSet<String> =
            selected.iter().map(|(key, _, _)| key.clone()).collect();
        let mut deletions: BTreeMap<String, (String, BTreeMap<String, Value>)> = BTreeMap::new();
        for (key, _, row) in &selected {
            deletions.insert(key.clone(), (table.to_string(), row.clone()));
        }
        for (_, parent_pk, _) in &selected {
            for (child_table, fk) in &referencing {
                let child_schema = self
                    .get_schema(child_table)
                    .ok_or_else(|| format!("referencing table '{}' has no schema", child_table))?;
                let child_rows = self.read_table_rows(child_table)?;
                for child in child_rows {
                    if child
                        .get(&fk.column)
                        .map(|value| value.compare(parent_pk) == std::cmp::Ordering::Equal)
                        .unwrap_or(false)
                    {
                        let child_pk = Self::pk_of(&child_schema, &child)?;
                        let child_key = Self::row_key(child_table, &child_pk);
                        if !fk.on_delete_cascade && !selected_keys.contains(&child_key) {
                            return Err(format!(
                                "RESTRICT violation: {} row {} is referenced by {}",
                                table, parent_pk, child_table
                            ));
                        }
                        if fk.on_delete_cascade {
                            deletions.insert(child_key, (child_table.clone(), child));
                        }
                    }
                }
            }
        }

        let deleted = selected.len();
        let cascaded = deletions.len().saturating_sub(deleted);
        let scan_op = match &pk_lits {
            Some(lits) if lits.len() == 1 => format!("PointGet({} = {})", table, lits[0]),
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => format!("SeqScan({})", table),
        };
        let mut batch: Vec<_> = deletions
            .keys()
            .map(|key| (crate::wal::WalOp::Delete, key.as_bytes().to_vec(), None))
            .collect();
        // Remove secondary-index entries for every deleted row (parents and
        // cascaded children alike).
        for (key, (del_table, row)) in &deletions {
            if let Some(del_schema) = self.get_schema(del_table) {
                batch.extend(Self::index_delta_batch(
                    &del_schema,
                    Some((del_table.as_str(), row)),
                    None,
                ));
                let _ = key;
            }
        }
        if let Some(buf) = sink.as_deref_mut() {
            buf.ops.extend(batch);
        } else {
            self.engine
                .write_batch(&batch)
                .map_err(|e| format!("delete failed: {}", e))?;
        }

        {
            let mut index = self.complement_index.write();
            for (key, (deleted_table, row)) in &deletions {
                if let Some(deleted_schema) = self.get_schema(deleted_table) {
                    for fk in &deleted_schema.foreign_keys {
                        if let Some(value) = row.get(&fk.column) {
                            index.remove(deleted_table, &fk.column, key, value);
                        }
                    }
                }
            }
        }

        Ok((
            vec!["status".into(), "deleted".into(), "cascaded".into()],
            vec![mk_row(&[
                ("status", Value::Text("OK".into())),
                ("deleted", Value::Int(deleted as i64)),
                ("cascaded", Value::Int(cascaded as i64)),
            ])],
            format!(
                "{} -> Delete{} ",
                scan_op,
                if referencing.is_empty() {
                    String::new()
                } else {
                    " + FK cascade scan".to_string()
                }
            ),
            Some(format!("{} deleted, {} cascaded", deleted, cascaded)),
        ))
    }

    fn exec_select(
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
                    let schema = self.get_schema(&table).ok_or_else(|| {
                        format!("SQL: relation '{}' does not exist", table)
                    })?;
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
            return Err(format!("SQL: relation '{}' does not exist (CREATE TABLE first)", from));
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
                match j.join_type {
                    JoinType::Inner => hash_join(&current, &right, &j.left_col, &j.right_col),
                    JoinType::Left => hash_join_left(&current, &right, &j.left_col, &j.right_col),
                    // RIGHT JOIN == LEFT JOIN with the sides swapped (the row
                    // model is single-namespace, so column naming is unchanged).
                    JoinType::Right => {
                        hash_join_left(&right, &current, &j.right_col, &j.left_col)
                    }
                    // FULL OUTER JOIN: left join + every right row that never
                    // matched, padded with NULLs on the left columns.
                    JoinType::Full => {
                        hash_join_full(&current, &right, &j.left_col, &j.right_col)
                    }
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
                SelectItem::ScalarSub { stmt, canonical, .. } => Some((canonical, stmt.as_ref())),
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
            self.stmt_outer_refs(s).map(|r| !r.is_empty()).unwrap_or(false)
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
                        SelectItem::ScalarSub { stmt, canonical, rendered } => {
                            Some((canonical.clone(), rendered.clone(), stmt.as_ref()))
                        }
                        _ => None,
                    })
                    .filter(|(_, _, s)| {
                        self.stmt_outer_refs(s).map(|r| !r.is_empty()).unwrap_or(false)
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
                        SelectItem::ScalarSub { canonical, rendered, .. } => {
                            specs.push(format!("{} AS {}", canonical, rendered));
                            columns.push((canonical.clone(), rendered.clone()));
                        }
                        SelectItem::Window { canonical, rendered, .. } => {
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
                        SelectItem::Window { rendered, .. } | SelectItem::ScalarSub { rendered, .. } => {
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
    fn cte_contains(&self, name: &str) -> bool {
        self.cte_store.lock().unwrap().contains_key(name)
    }

    /// Candidate rows for a FROM table: PK point lookup > secondary index
    /// (eq / IN-list / range) > SeqScan. The full WHERE is always re-applied
    /// afterwards, so a conservative candidate set only costs performance.
    fn fetch_candidates(
        &self,
        from: &str,
        where_clause: Option<&Expr>,
    ) -> Result<(Vec<BTreeMap<String, Value>>, String), String> {
        let schema = self.get_schema(from);
        if let Some(schema) = &schema {
            if let Some(lits) =
                where_clause.and_then(|e| Self::pk_eq_literals(e, &schema.primary_locus_column))
            {
                let mut lits = lits;
                // CMS edition: PointGet keys must be encoded with the stored
                // PK representation (read-side coercion mirrors write-side).
                #[cfg(feature = "cms")]
                if let Some(ct) = schema.columns.get(schema.primary_locus_column.as_str()) {
                    for lit in lits.iter_mut() {
                        *lit = Value::coerce_for_column(ct, lit.clone());
                    }
                }
                let mut out = Vec::with_capacity(lits.len());
                for lit in &lits {
                    if let Some(row) = self.read_row_point(from, lit)? {
                        out.push(row);
                    }
                }
                let plan = if lits.len() == 1 {
                    format!("PointGet({} = {})", from, lits[0])
                } else {
                    format!(
                        "PointGet({} IN ({}))",
                        from,
                        lits.iter()
                            .map(|v| v.to_string())
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
    fn index_candidate_keys(
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
    fn materialize_subqueries(&self, expr: Option<Expr>) -> Result<Option<Expr>, String> {
        fn walk(sql: &SqlEngine, e: &Expr) -> Result<Expr, String> {
            match e {
                Expr::Like { operand, pattern, negated } => Ok(Expr::Like {
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
    fn aggregate_rows(
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

// ---------------------------------------------------------------------------
// Secondary-index planning helpers
// ---------------------------------------------------------------------------

