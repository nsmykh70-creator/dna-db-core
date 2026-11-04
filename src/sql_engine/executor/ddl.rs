// SPDX-License-Identifier: AGPL-3.0-only
//! DDL execution: CREATE/DROP TABLE and INDEX, ALTER ADD/DROP COLUMN with
//! unique-violation checks, schema persistence and index batch rebuilds.

// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::*;
use crate::relational::{
    encode_ordered_value, ChromosomeTable, ColumnType, SecondaryIndexMeta, Value,
};
use std::collections::BTreeMap;

impl SqlEngine {
    // ---------- statements ----------

    pub(crate) fn exec_create_table(
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

    pub(crate) fn exec_create_index(
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

    pub(crate) fn exec_drop_index(
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
    pub(crate) fn persist_schema(&self, table: &ChromosomeTable) -> Result<(), String> {
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
    pub(crate) fn exec_add_column(
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
    pub(crate) fn exec_drop_column(
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
    pub(crate) fn check_unique_violations(
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
    pub(crate) fn index_delta_batch(
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
                    let row_key = Self::row_key_from_row(table, schema, row).ok()?;
                    let mut key = index_prefix(table, col);
                    key.extend_from_slice(&escape_index_part(&encode_ordered_value(v)));
                    key.extend_from_slice(row_key.as_bytes());
                    Some((key, row_key))
                })
            });
            let new_entry = new.and_then(|(table, row)| {
                row.get(col).filter(|v| !v.is_null()).and_then(|v| {
                    let row_key = Self::row_key_from_row(table, schema, row).ok()?;
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

    pub(crate) fn exec_drop_table(
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
                .filter_map(|row| Self::row_key_from_row(table, &schema, row).ok())
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
}
