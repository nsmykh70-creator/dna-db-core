// SPDX-License-Identifier: AGPL-3.0-only
//! INSERT / UPDATE / DELETE execution inside the transaction buffer:
//! upsert paths, AUTO_INCREMENT sequencing, unique-check accounting and
//! secondary-index deltas.

// SPDX-License-Identifier: AGPL-3.0-only
//! Statement execution: every `Stmt` variant is turned into engine reads /
//! writes with honest execution plans. Transaction buffering (SqlTxBuffer),
//! CTE materialisation, scalar subqueries and window functions live here too.

use super::*;
use crate::expr::Expr;
use crate::relational::{encode_ordered_value, ForeignKeyConstraint, RowCodec, Value};
use std::collections::BTreeMap;

impl SqlEngine {
    pub(crate) fn exec_insert_tx(
        &self,
        table: &str,
        rows: Vec<Vec<(String, Value)>>,
        on_conflict: Option<OnConflict>,
        ignore: bool,
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
            // Positional INSERT (no column list): unnamed values map to the
            // declared columns in order (MySQL semantics; Drupal and plain
            // mysqldump output rely on it).
            let row = if row.iter().any(|(c, _)| c.is_empty()) {
                let declared = schema.declared_columns();
                if row.len() != declared.len() {
                    return Err(format!(
                        "INSERT INTO {} VALUES: {} values for {} declared columns",
                        table,
                        row.len(),
                        declared.len()
                    ));
                }
                row.into_iter()
                    .zip(declared.iter())
                    .map(|((_, v), c)| (c.clone(), v))
                    .collect::<Vec<_>>()
            } else {
                row
            };
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
            let key = Self::row_key_from_row(table, &schema, &map)?;
            if !seen_keys.insert(key.clone()) {
                // v3.6: same statement hit the same PK — ON CONFLICT applies.
                match on_conflict {
                    Some(OnConflict::DoNothing) => {
                        skipped_conflicts += 1;
                        continue;
                    }
                    None if ignore => {
                        // INSERT IGNORE: uniqueness conflicts are skipped.
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
                        let mut existing: BTreeMap<String, Value> = RowCodec::decode(&raw)
                            .map_err(|e| format!("row {} unreadable: {}", key, e))?
                            .into_iter()
                            .collect();
                        for (col, val) in assignments {
                            let v = match val {
                                UpsertValue::Literal(v) => v.clone(),
                                UpsertValue::Excluded(source) => {
                                    map.get(source).cloned().ok_or_else(|| {
                                        format!(
                                            "SQL: EXCLUDED.{} not present in the conflicting row",
                                            source
                                        )
                                    })?
                                }
                                UpsertValue::Column(source) => {
                                    existing.get(source).cloned().ok_or_else(|| {
                                        format!(
                                            "SQL: column '{}' not present in the conflicting row",
                                            source
                                        )
                                    })?
                                }
                                UpsertValue::ColumnAdd { column, delta } => value_add(
                                    existing.get(column).cloned().unwrap_or(Value::Int(0)),
                                    delta.clone(),
                                )?,
                            };
                            existing.insert(col.clone(), v);
                        }
                        // PK must not change via DO UPDATE SET.
                        let new_table_key = Self::row_key_from_row(table, &schema, &existing)?;
                        if new_table_key != key {
                            return Err(
                                "SQL: ON CONFLICT DO UPDATE SET cannot change the primary key"
                                    .into(),
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
                    None if ignore => {
                        // INSERT IGNORE: existing row + no ON CONFLICT → skip.
                        skipped_conflicts += 1;
                        continue;
                    }
                    None => return Err(format!("duplicate primary key in INSERT: {}", key)),
                }
            }
            // UNIQUE index enforcement — also covers duplicates between rows
            // of this same statement (via stmt_unique) and against rows that
            // already exist in the engine (via index scan).
            let mut handled_by_unique_upsert = false;
            let mut ignore_skip = false;
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
                        if ignore {
                            ignore_skip = true;
                            break;
                        }
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
                                UpsertValue::Excluded(source) => {
                                    map.get(source).cloned().ok_or_else(|| {
                                        format!(
                                            "SQL: VALUES({}) not present in the inserted row",
                                            source
                                        )
                                    })?
                                }
                                UpsertValue::Column(source) => {
                                    existing.get(source).cloned().ok_or_else(|| {
                                        format!(
                                            "SQL: column '{}' not present in the conflicting row",
                                            source
                                        )
                                    })?
                                }
                                UpsertValue::ColumnAdd { column, delta } => value_add(
                                    existing.get(column).cloned().unwrap_or(Value::Int(0)),
                                    delta.clone(),
                                )?,
                            };
                            existing.insert(col.clone(), nv);
                        }
                        let new_key = Self::row_key_from_row(table, &schema, &existing)?;
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
                        batch.push((crate::wal::WalOp::Put, row_key_bytes.clone(), Some(payload)));
                        upserted += 1;
                        handled_by_unique_upsert = true;
                        break;
                    }
                    if ignore {
                        // INSERT IGNORE: unique value already taken → skip.
                        ignore_skip = true;
                        break;
                    }
                    return Err(format!(
                        "UNIQUE constraint failed: {}.{} = {} (index '{}')",
                        table, idx.column, v, idx.name
                    ));
                }
                stmt_unique.insert((idx.column.clone(), enc), key.clone());
            }
            if ignore_skip {
                // INSERT IGNORE hit an existing unique value → skip the row.
                skipped_conflicts += 1;
                continue;
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
            let pk = map
                .get(&schema.primary_locus_column)
                .cloned()
                .unwrap_or(Value::Int(0));
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
                if on_conflict.is_some() {
                    " + UPSERT"
                } else {
                    ""
                }
            ),
            Some(msg),
        ))
    }

    pub(crate) fn exec_update_tx(
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
            .and_then(|e| Self::pk_eq_literals(e, &Self::pk_cols(&schema)));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point_pk(table, &lit)? {
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
            let old_key = Self::row_key_from_row(table, &schema, &row)?;
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
            let key = Self::row_key_from_row(table, &schema, &new_row)?;
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
            let old_table_key = Self::row_key_from_row(&schema.name, &schema, old_row)?;
            let new_table_key = Self::row_key_from_row(&schema.name, &schema, new_row)?;
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
            Some(lits) if lits.len() == 1 && lits[0].len() == 1 => {
                format!("PointGet({} = {})", table, lits[0][0])
            }
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|t| t
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(":"))
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

    pub(crate) fn exec_delete_tx(
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
            .and_then(|e| Self::pk_eq_literals(e, &Self::pk_cols(&schema)));
        let rows: Vec<BTreeMap<String, Value>> = match &pk_lits {
            Some(lits) => {
                let mut out = Vec::with_capacity(lits.len());
                for lit in lits {
                    if let Some(row) = self.read_row_point_pk(table, &lit)? {
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
            // Full storage key: composite PKs are stored as
            // `table:<pk1>:<pk2>` (row_key_from_row) — a single-part key
            // silently deletes nothing (Drupal's DELETE users_field_data
            // WHERE uid = 1 before re-inserting user 1).
            let key = Self::row_key_from_row(table, &schema, &row)?;
            selected.push((key, pk, row));
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
                        let child_key = Self::row_key_from_row(child_table, &child_schema, &child)?;
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
            Some(lits) if lits.len() == 1 && lits[0].len() == 1 => {
                format!("PointGet({} = {})", table, lits[0][0])
            }
            Some(lits) => format!(
                "PointGet({} IN ({}))",
                table,
                lits.iter()
                    .map(|t| t
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(":"))
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
}
