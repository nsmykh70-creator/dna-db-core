// SPDX-License-Identifier: AGPL-3.0-only
//! CTE and UNION execution plus the live-CTE registry probe.

use super::*;

impl SqlEngine {
    pub(crate) fn exec_cte(
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
            let (cols, rows, _cplan, _) = self.exec_stmt(def.clone(), &mut None)?;
            let mut store = self.cte_store.lock().unwrap();
            if store.contains_key(name) {
                return Err(format!("SQL: duplicate CTE name '{}'", name));
            }
            plan.push_str(&format!("{}[{} rows] ", name, rows.len()));
            store.insert(name.clone(), (cols, rows));
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
    pub(crate) fn exec_union(
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
            Some(format!(
                "{} row(s) after UNION {}",
                n,
                if all { "ALL" } else { "DISTINCT" }
            )),
        ))
    }

    /// v3.6: is `name` a live CTE of the statement currently running?
    pub(crate) fn cte_contains(&self, name: &str) -> bool {
        self.cte_store.lock().unwrap().contains_key(name)
    }
}
