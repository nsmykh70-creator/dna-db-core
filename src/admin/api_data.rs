// SPDX-License-Identifier: Apache-2.0
//! Admin panel API: data & operations endpoints (Prometheus metrics,
//! health, EXPLAIN, compaction, table listing, browse, SQL/DnaQL console).
//! Split from the former admin/mod.rs monolith — no behaviour change.

use super::*;

impl AdminPanel {
    /// v4.0: Prometheus text format (0.0.4) endpoint. Real counters from the
    /// engine — no fabricated values.
    pub(super) fn prometheus_metrics(&self) -> Response {
        let health = self.engine.health();
        let lesions = &health["lesions"];
        let mut body = String::new();
        let health_val = |k: &str| -> u64 {
            lesions
                .get(k)
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        body.push_str("# HELP dnadb_engine_health_status Engine health (1=GREEN).
# TYPE dnadb_engine_health_status gauge
dnadb_engine_health_status ");
        body.push_str(&(if health["status"] == "GREEN" { 1 } else { 0 }).to_string());
        body.push_str("\n");
        body.push_str("# HELP dnadb_lesions_found_total Lesions detected by scrubber/on-read checks.
# TYPE dnadb_lesions_found_total counter
dnadb_lesions_found_total ");
        body.push_str(&health_val("found").to_string());
        body.push_str("\n");
        body.push_str("# HELP dnadb_lesions_repaired_total Lesions repaired in-band (RS parity / mirror).
# TYPE dnadb_lesions_repaired_total counter
dnadb_lesions_repaired_total ");
        body.push_str(&health_val("repaired").to_string());
        body.push_str("\n");
        body.push_str("# HELP dnadb_lesions_unrecoverable_total Lesions beyond repair (quarantined).
# TYPE dnadb_lesions_unrecoverable_total counter
dnadb_lesions_unrecoverable_total ");
        body.push_str(&health_val("unrecoverable").to_string());
        body.push_str("\n");
        body.push_str("# HELP dnadb_memtable_bytes Active MemTable size in bytes.
# TYPE dnadb_memtable_bytes gauge
dnadb_memtable_bytes ");
        body.push_str(&self.engine.memtable_size().to_string());
        body.push_str("\n");
        body.push_str("# HELP dnadb_segments_count On-disk DNASST02 segments.
# TYPE dnadb_segments_count gauge
dnadb_segments_count ");
        body.push_str(&self.engine.segment_count().to_string());
        body.push_str("\n");
        // v4.1: compaction results — lifetime counter, the last completed run
        // (duration / bytes before-after / savings / completion timestamp) and
        // the on-disk footprint of the whole data directory.
        let stats = &self.engine.stats;
        let push_gauge = |body: &mut String, name: &str, help: &str, value: String| {
            body.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
            ));
        };
        body.push_str("# HELP dnadb_compactions_total Completed segment compactions since engine open.
# TYPE dnadb_compactions_total counter
dnadb_compactions_total ");
        body.push_str(&stats.compaction_count.load(Ordering::Relaxed).to_string());
        body.push_str("\n");
        push_gauge(
            &mut body,
            "dnadb_compaction_last_duration_seconds",
            "Wall-clock duration of the last completed compaction.",
            format!(
                "{:.3}",
                stats.compaction_last_duration_ms.load(Ordering::Relaxed) as f64 / 1000.0
            ),
        );
        push_gauge(
            &mut body,
            "dnadb_compaction_bytes_before",
            "Data-directory size on disk immediately before the last compaction.",
            stats
                .compaction_last_bytes_before
                .load(Ordering::Relaxed)
                .to_string(),
        );
        push_gauge(
            &mut body,
            "dnadb_compaction_bytes_after",
            "Data-directory size on disk immediately after the last compaction.",
            stats
                .compaction_last_bytes_after
                .load(Ordering::Relaxed)
                .to_string(),
        );
        push_gauge(
            &mut body,
            "dnadb_compaction_bytes_saved",
            "Bytes reclaimed by the last compaction (before - after).",
            stats.compaction_saved_bytes().to_string(),
        );
        push_gauge(
            &mut body,
            "dnadb_compaction_last_success_timestamp_seconds",
            "Unix time of the last completed compaction (0 = never).",
            stats.compaction_last_unix.load(Ordering::Relaxed).to_string(),
        );
        push_gauge(
            &mut body,
            "dnadb_data_dir_bytes",
            "Total size of the data directory on disk (recursive).",
            self.engine.dir_size_bytes().to_string(),
        );
        // Flat engine stats (puts/gets/deletes/wal_bytes/...) as gauges.
        for pair in self.engine.stats.summary().split_whitespace() {
            if let Some((k, v)) = pair.split_once('=') {
                if let Ok(n) = v.parse::<f64>() {
                    let key = k.trim_start_matches("__").replace('-', "_");
                    body.push_str(&format!(
                        "# TYPE dnadb_stat_{key} gauge
dnadb_stat_{key} {n}
"
                    ));
                }
            }
        }
        Response {
            status: 200,
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body,
        }
    }

    pub(super) fn api_health(&self) -> Response {
        let health = self.engine.health();
        // Parse the flat stats summary into an object for the dashboard.
        let mut stats = serde_json::Map::new();
        for pair in self.engine.stats.summary().split_whitespace() {
            if let Some((k, v)) = pair.split_once('=') {
                let val: J = v
                    .parse::<f64>()
                    .map(|f| json!(f))
                    .or_else(|_| v.parse::<u64>().map(|n| json!(n)))
                    .unwrap_or_else(|_| json!(v));
                stats.insert(k.to_string(), val);
            }
        }
        Response::json(
            200,
            &json!({
                "health": health,
                "stats": stats,
                "segments": self.engine.segment_count(),
                "memtable_bytes": self.engine.memtable_size(),
                "wal_bytes": self.engine.wal_size_bytes(),
                "data_dir": self.engine.data_dir().display().to_string(),
                "data_dir_bytes": self.engine.dir_size_bytes(),
            }),
        )
    }

    /// v4.1: SQL → DnaQL translation plus the physical storage map for a
    /// statement the operator is about to run — the panel-side counterpart of
    /// the CLI `sql --to-dnaql` mode. Read-only: nothing is executed.
    pub(super) fn api_explain(&self, session: &Session, req: &Request) -> Response {
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let query = body["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Response::json(400, &json!({"error": "empty query"}));
        }
        // Same table ACL as /api/query: the storage map reveals where rows of
        // a table live, so table-restricted users must not see foreign ones.
        if session.role != "admin" && session.allowed_tables.is_some() {
            let upper = query.to_ascii_uppercase();
            if upper.starts_with("SHOW") || upper.starts_with("DESCRIBE") {
                return Response::json(
                    403,
                    &json!({"error": "schema-wide queries are not available to table-restricted users"}),
                );
            }
            match crate::sql_engine::referenced_tables(&query) {
                Ok(tables) => {
                    if let Err(message) = crate::access::authorize_tables(
                        &session.role,
                        &session.allowed_tables,
                        &tables,
                    ) {
                        return Response::json(403, &json!({"error": message}));
                    }
                }
                Err(message) => return Response::json(400, &json!({"error": message})),
            }
        }
        // Both halves are best-effort: a statement may have no DnaQL
        // equivalent (JOIN, CREATE INDEX) while still mapping to storage.
        let (dnaql, dnaql_error) = match self.sql.sql_to_dnaql(&query) {
            Ok(text) => (Some(text), None),
            Err(e) => (None, Some(e)),
        };
        let (storage, storage_error) = match self.sql.explain_storage(&query) {
            Ok(text) if !text.trim().is_empty() => (Some(text), None),
            Ok(_) => (None, Some("no storage mapping for this statement".into())),
            Err(e) => (None, Some(e)),
        };
        if dnaql.is_none() && storage.is_none() {
            return Response::json(
                400,
                &json!({
                    "error": dnaql_error.clone().unwrap_or_else(|| "cannot explain query".into()),
                    "dnaql_error": dnaql_error,
                    "storage_error": storage_error,
                }),
            );
        }
        Response::json(
            200,
            &json!({
                "dnaql": dnaql,
                "dnaql_error": dnaql_error,
                "storage": storage,
                "storage_error": storage_error,
            }),
        )
    }

    /// v4.1: online full compaction (admin only). Safe under concurrency —
    /// readers use snapshots and the segment swap is atomic. Reports the data
    /// directory size before/after so the panel can show the real win. The
    /// completed run is tagged with the panel user, appended to
    /// `compaction_history.jsonl` and exported through `dnadb_compaction_*`.
    pub(super) fn api_compact(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        // An optional JSON body is accepted (and ignored) so the UI can POST
        // an empty object; nothing in it can change the compaction semantics.
        let _ = req.json_body();
        let trigger = format!("panel:{}", session.user);
        let started = Instant::now();
        match self.engine.compact_with_trigger(&trigger) {
            Ok(Some(record)) => Response::json(
                200,
                &json!({
                    "ok": true,
                    "ran": true,
                    "before_bytes": record.bytes_before,
                    "after_bytes": record.bytes_after,
                    "freed_bytes": record.saved_bytes,
                    "removed_files": record.deleted_files,
                    "time_us": started.elapsed().as_micros() as u64,
                    "record": record,
                }),
            ),
            Ok(None) => Response::json(
                200,
                &json!({
                    "ok": true,
                    "ran": false,
                    "message": "компакция не требуется: на диске один (или ноль) сегментов",
                    "before_bytes": self.engine.dir_size_bytes(),
                    "after_bytes": self.engine.dir_size_bytes(),
                    "freed_bytes": 0,
                    "removed_files": 0,
                    "time_us": started.elapsed().as_micros() as u64,
                }),
            ),
            Err(e) => Response::json(
                500,
                &json!({"error": format!("compaction failed: {e}")}),
            ),
        }
    }

    /// v4.1: completed compactions, newest first (on-disk history file).
    pub(super) fn api_compactions(&self, session: &Session) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let history = self.engine.compaction_history();
        Response::json(
            200,
            &json!({
                "compactions": history,
                "count": history.len(),
                "history_file": self
                    .engine
                    .data_dir()
                    .join(crate::engine::COMPACTION_HISTORY_FILE)
                    .display()
                    .to_string(),
            }),
        )
    }

    pub(super) fn api_tables(&self, session: &Session) -> Response {
        let tables: Vec<J> = self
            .sql
            .list_tables()
            .into_iter()
            .filter(|name| {
                crate::access::denied_table(&session.role, &session.allowed_tables, name).is_none()
            })
            .filter_map(|name| {
                let schema = self.sql.get_schema(&name)?;
                Some(json!({
                    "name": schema.name,
                    "columns": schema.columns,
                    "pk": schema.primary_locus_column,
                    "fks": schema.foreign_keys,
                    "indexes": schema.secondary_indexes,
                }))
            })
            .collect();
        Response::json(200, &json!({"tables": tables}))
    }

    pub(super) fn api_browse(&self, req: &Request, session: &Session) -> Response {
        let name = match req.query_param("table") {
            Some(n) => n,
            None => return Response::json(400, &json!({"error": "missing ?table="})),
        };
        if !is_safe_ident(&name) {
            return Response::json(400, &json!({"error": "invalid table name"}));
        }
        if let Some(denied) =
            crate::access::denied_table(&session.role, &session.allowed_tables, &name)
        {
            return Response::json(
                403,
                &json!({"error": format!("access denied for table '{denied}'")}),
            );
        }
        if self.sql.get_schema(&name).is_none() {
            return Response::json(
                404,
                &json!({"error": format!("table '{}' not found", name)}),
            );
        }
        let limit = req
            .query_param("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 500);
        let offset = req
            .query_param("offset")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let page = match self.sql.execute(&format!(
            "SELECT * FROM {} LIMIT {} OFFSET {}",
            name, limit, offset
        )) {
            Ok(r) => r,
            Err(e) => return Response::json(500, &json!({"error": e})),
        };
        let total = match self
            .sql
            .execute(&format!("SELECT COUNT(*) AS __c FROM {}", name))
        {
            Ok(r) => r
                .rows
                .first()
                .and_then(|row| row.get("__c"))
                .and_then(|v| match v {
                    V::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(0),
            Err(e) => return Response::json(500, &json!({"error": e})),
        };
        Response::json(
            200,
            &json!({
                "table": name,
                "columns": page.columns,
                "rows": rows_json(&page.rows),
                "total": total,
                "offset": offset,
                "limit": limit,
                "plan": page.execution_plan,
                "time_us": page.execution_time_us,
            }),
        )
    }

    pub(super) fn api_query(&self, session: &Session, req: &Request) -> Response {
        let body = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let query = body["query"].as_str().unwrap_or("").trim().to_string();
        if session.role == "viewer" && !crate::server::is_read_only_query(&query) {
            return Response::json(403, &json!({"error": "viewer role is read-only"}));
        }
        if query.is_empty() {
            return Response::json(400, &json!({"error": "empty query"}));
        }
        let kind = body["kind"].as_str().unwrap_or("sql");
        if session.role != "admin" && session.allowed_tables.is_some() {
            let upper = query.trim_start().to_ascii_uppercase();
            if kind != "dnaql" && (upper.starts_with("SHOW") || upper.starts_with("DESCRIBE")) {
                return Response::json(
                    403,
                    &json!({"error": "schema-wide queries are not available to table-restricted users"}),
                );
            }
            let tables = match kind {
                "dnaql" => crate::dna_ql::referenced_tables(&query),
                _ => crate::sql_engine::referenced_tables(&query),
            };
            match tables {
                Ok(tables) => {
                    if let Err(message) = crate::access::authorize_tables(
                        &session.role,
                        &session.allowed_tables,
                        &tables,
                    ) {
                        return Response::json(403, &json!({"error": message}));
                    }
                }
                Err(message) => return Response::json(400, &json!({"error": message})),
            }
        }
        match kind {
            "dnaql" => {
                // Fault guard (same rationale as the TCP path): a panic in
                // the query engine must not kill the whole panel process.
                match crate::server::catch_request_panic("panel:dnaql", || {
                    self.dnaql.execute(&query)
                }) {
                    Ok(Ok(res)) => {
                        let mut columns: Vec<String> = Vec::new();
                        for row in &res.rows {
                            for k in row.fields.keys() {
                                if !columns.contains(k) {
                                    columns.push(k.clone());
                                }
                            }
                        }
                        Response::json(
                            200,
                            &json!({
                                "kind": "dnaql",
                                "columns": columns,
                                "rows": dnaql_rows_json(&res.rows),
                                "plan": res.ast_summary,
                                "note": res.engine_note,
                                "time_us": res.execution_time_us,
                            }),
                        )
                    }
                    Ok(Err(e)) => Response::json(400, &json!({"error": e})),
                    Err(e) => Response::json(500, &json!({"error": e})),
                }
            }
            _ => {
                // CMS edition: the SQL console speaks the MySQL dialect
                // through the compat gate when one is attached (ephemeral
                // session per request — the HTTP transport is stateless).
                #[cfg(feature = "cms")]
                {
                    if let Some(gate) = &self.cms {
                        let sid = gate.open_session();
                        let outcome = crate::server::catch_request_panic("panel:cms-sql", || {
                            gate.execute(sid, &query)
                        });
                        gate.close_session(sid);
                        return match outcome {
                            Ok(Ok(res)) => Response::json(200, &cms_result_json(&res)),
                            Ok(Err(e)) => Response::json(
                                400,
                                &json!({
                                    "error": e.message,
                                    "errno": e.errno,
                                    "sqlstate": e.sqlstate(),
                                }),
                            ),
                            Err(e) => Response::json(500, &json!({"error": e})),
                        };
                    }
                }
                match crate::server::catch_request_panic("panel:sql", || self.sql.execute(&query)) {
                    Ok(Ok(res)) => Response::json(200, &sql_result_json(&res)),
                    Ok(Err(e)) => Response::json(400, &json!({"error": e})),
                    Err(e) => Response::json(500, &json!({"error": e})),
                }
            }
        }
    }
}
