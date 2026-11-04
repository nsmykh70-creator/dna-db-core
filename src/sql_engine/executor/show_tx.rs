// SPDX-License-Identifier: AGPL-3.0-only
//! SHOW TABLES / SHOW STORAGE and transaction control (BEGIN, COMMIT,
//! ROLLBACK, SAVEPOINT) statement execution.

// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::*;
use crate::relational::{RowCodec, Value};
use std::collections::BTreeMap;

impl SqlEngine {
    // ---------- SHOW / transactions / CTE ----------

    pub(crate) fn exec_show_tables(
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
    pub(crate) fn exec_show_storage(
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
                    let locus = key_s.strip_prefix(&prefix).unwrap_or("?").to_string();
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

    pub(crate) fn exec_tx(
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
}
