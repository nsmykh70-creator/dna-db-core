// SPDX-License-Identifier: AGPL-3.0-only
//! Candidate fetch: PK point lookup > secondary index (eq / IN / range)
//! > sequential scan, and index-entry → row-key resolution.

use super::*;

impl SqlEngine {
    /// Candidate rows for a FROM table: PK point lookup > secondary index
    /// (eq / IN-list / range) > SeqScan. The full WHERE is always re-applied
    /// afterwards, so a conservative candidate set only costs performance.
    pub(crate) fn fetch_candidates(
        &self,
        from: &str,
        where_clause: Option<&Expr>,
    ) -> Result<(Vec<BTreeMap<String, Value>>, String), String> {
        let schema = self.get_schema(from);
        if let Some(schema) = &schema {
            if let Some(lits) =
                where_clause.and_then(|e| Self::pk_eq_literals(e, &Self::pk_cols(&schema)))
            {
                let mut lits = lits;
                // CMS edition: PointGet keys must be encoded with the stored
                // PK representation (read-side coercion mirrors write-side).
                #[cfg(feature = "cms")]
                for lit in lits.iter_mut() {
                    if let Some(ct) = schema.columns.get(schema.primary_locus_column.as_str()) {
                        lit[0] = Value::coerce_for_column(ct, lit[0].clone());
                    }
                }
                let mut out = Vec::with_capacity(lits.len());
                for lit in &lits {
                    if let Some(row) = self.read_row_point_pk(from, &lit)? {
                        out.push(row);
                    }
                }
                let plan = if lits.len() == 1 && lits[0].len() == 1 {
                    format!("PointGet({} = {})", from, lits[0][0])
                } else {
                    format!(
                        "PointGet({} IN ({}))",
                        from,
                        lits.iter()
                            .map(|t| t
                                .iter()
                                .map(|v| v.to_string())
                                .collect::<Vec<_>>()
                                .join(":"))
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
    pub(crate) fn index_candidate_keys(
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

}
