// SPDX-License-Identifier: AGPL-3.0-only
//! GROUP BY / HAVING / aggregate evaluation and output-name mapping.

use super::*;

impl SqlEngine {
    /// GROUP BY + aggregates + HAVING over an already-filtered row set.
    /// Aggregates are computed under their canonical name (`COUNT(*)`),
    /// HAVING evaluates against those canonical keys, then each surviving
    /// row is renamed to the projection's output alias before returning.
    pub(crate) fn aggregate_rows(
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
