// SPDX-License-Identifier: AGPL-3.0-only
//! Row access primitives: storage row decoding, primary-key extraction,
//! point reads and key encoding used by point-get planning.

// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::*;
use crate::expr::{CmpOp, Expr};
use crate::relational::{ChromosomeTable, RowCodec, Value};
use std::collections::BTreeMap;

impl SqlEngine {
    // ---------- storage helpers ----------

    pub(crate) fn row_key(table: &str, pk: &Value) -> String {
        format!("{}:{}", table, pk)
    }

    /// CMS edition: full PK column list of a table (composite-aware).
    pub(crate) fn pk_cols(schema: &ChromosomeTable) -> Vec<String> {
        if schema.primary_key_cols.is_empty() {
            vec![schema.primary_locus_column.clone()]
        } else {
            schema.primary_key_cols.clone()
        }
    }

    /// Storage key of a row: single-column PKs keep the historical
    /// `<table>:<pk>` shape; composite PKs encode every key part
    /// (`mw_l10n_cache:en:fallback`), so each key part pair is distinct.
    pub(crate) fn row_key_from_row(
        table: &str,
        schema: &ChromosomeTable,
        row: &BTreeMap<String, Value>,
    ) -> Result<String, String> {
        let cols = Self::pk_cols(schema);
        if cols.len() == 1 {
            let pk = row
                .get(&cols[0])
                .cloned()
                .ok_or_else(|| format!("row missing PK column '{}'", cols[0]))?;
            Ok(Self::row_key(table, &pk))
        } else {
            let mut parts = Vec::with_capacity(cols.len());
            for c in &cols {
                parts.push(
                    row.get(c)
                        .cloned()
                        .ok_or_else(|| format!("row missing PK column '{}'", c))?
                        .to_string(),
                );
            }
            Ok(format!("{}:{}", table, parts.join(":")))
        }
    }

    pub(crate) fn read_table_rows(
        &self,
        table: &str,
    ) -> Result<Vec<BTreeMap<String, Value>>, String> {
        // v3.6: statement-scoped CTE tables take precedence over disk.
        if let Some((_, rows)) = self.cte_store.lock().unwrap().get(table) {
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

    pub(crate) fn pk_of(
        schema: &ChromosomeTable,
        row: &BTreeMap<String, Value>,
    ) -> Result<Value, String> {
        row.get(&schema.primary_locus_column)
            .cloned()
            .ok_or_else(|| format!("row missing PK column '{}'", schema.primary_locus_column))
    }

    /// Fetch a single row by full storage key '<table>:<pk>'.
    pub(crate) fn read_row_by_key(
        &self,
        row_key: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
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
    pub(crate) fn pk_eq_literals(expr: &Expr, pk_cols: &[String]) -> Option<Vec<Vec<Value>>> {
        match expr {
            Expr::Or(a, b) => {
                let mut out = Self::pk_eq_literals(a, pk_cols)?;
                out.extend(Self::pk_eq_literals(b, pk_cols)?);
                Some(out)
            }
            _ => {
                fn find(e: &Expr, pk_cols: &[String], acc: &mut Vec<Value>) -> bool {
                    match e {
                        Expr::And(a, b) => find(a, pk_cols, acc) | find(b, pk_cols, acc),
                        Expr::Cmp {
                            op: CmpOp::Eq,
                            left,
                            right,
                        } => {
                            for c in pk_cols {
                                let lit = match (left.as_ref(), right.as_ref()) {
                                    (Expr::Column(col), Expr::Literal(v)) if col == c => Some(v),
                                    (Expr::Literal(v), Expr::Column(col)) if col == c => Some(v),
                                    _ => None,
                                };
                                if let Some(lit) = lit {
                                    if !lit.is_null() {
                                        acc.push(lit.clone());
                                        return true;
                                    }
                                }
                            }
                            false
                        }
                        _ => false,
                    }
                }
                let mut acc = Vec::new();
                if find(expr, pk_cols, &mut acc) && acc.len() == pk_cols.len() {
                    Some(vec![acc])
                } else {
                    None
                }
            }
        }
    }

    /// Fetch a single row by storage key '<table>:<pk>' (keyed get, no scan).
    pub(crate) fn read_row_point(
        &self,
        table: &str,
        pk: &Value,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let key = Self::row_key(table, pk);
        self.read_row_by_storage_key(table, &key)
    }

    /// Point lookup by a full PK tuple (composite-aware).
    pub(crate) fn read_row_point_pk(
        &self,
        table: &str,
        pks: &[Value],
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let key = if pks.len() == 1 {
            Self::row_key(table, &pks[0])
        } else {
            format!(
                "{}:{}",
                table,
                pks.iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(":")
            )
        };
        self.read_row_by_storage_key(table, &key)
    }

    pub(crate) fn read_row_by_storage_key(
        &self,
        table: &str,
        key: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
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
}
