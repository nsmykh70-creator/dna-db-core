// SPDX-License-Identifier: AGPL-3.0-only
//! CMS dialect gate: the single entry point WordPress / Joomla clients use.
//!
//! Every MySQL-dialect statement goes through [`CmsSqlGate::execute`], which
//! intercepts what the engine must NOT see (session state, named locks,
//! catalog overlay queries) and forwards the rest to [`SqlEngine`]:
//!
//!   wpdb / DatabaseDriver ──▶ CmsSqlGate ──┬─ session (SET NAMES / sql_mode / autocommit)
//!                                          ├─ LAST_INSERT_ID() / FOUND_ROWS() / ROW_COUNT()
//!                                          ├─ GET_LOCK / RELEASE_LOCK / IS_FREE_LOCK / IS_USED_LOCK
//!                                          ├─ SHOW TABLES LIKE / SHOW FULL COLUMNS / SHOW INDEX
//!                                          ├─ VERSION() / @@system vars / SHOW VARIABLES
//!                                          ├─ SQL_CALC_FOUND_ROWS strip + accounting
//!                                          └─▶ SqlEngine (real SQL)
//!
//! Error reporting follows MySQL: every failure carries the closest MySQL
//! server error code ([`crate::cms::errno`]) — wpdb and WooCommerce branch
//! on errno (retry on 1213, dedupe on 1062), not on message text.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::cms::ddl_meta::CmsTableMeta;
use crate::cms::errno;
use crate::cms::locks::{LockRegistry, ReleaseResult};
use crate::cms::session::CmsSession;
use crate::cms::wordpress;
use crate::relational::{ColumnType, Value};
use crate::sql_engine::SqlEngine;

/// Version string reported to CMS clients (wpdb::db_version gate checks the
/// numeric prefix; "8.0.36" keeps every version_compare branch happy).
pub const MYSQL_COMPAT_VERSION: &str = "8.0.36";

/// Result envelope for the gate — a superset of what wpdb consumes.
#[derive(Debug, Clone)]
pub struct CmsQueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub message: Option<String>,
    pub plan: String,
    /// DML affected rows (INSERT/UPDATE/DELETE), 0 for SELECT.
    pub affected_rows: u64,
    /// LAST_INSERT_ID() after this statement (INSERT with a sequence).
    pub last_insert_id: u64,
    /// FOUND_ROWS() after this statement (row count of SELECTs).
    pub found_rows: u64,
    pub is_select: bool,
}

/// An error with a MySQL errno + SQLSTATE, as CMS clients expect.
#[derive(Debug, Clone)]
pub struct CmsError {
    pub errno: u16,
    pub message: String,
}

impl CmsError {
    pub fn sqlstate(&self) -> &'static str {
        errno::sqlstate(self.errno)
    }
}

/// Maps engine error text to the closest MySQL server error code.
pub fn classify_error(message: &str) -> u16 {
    let m = message.to_ascii_lowercase();
    if m.contains("unique constraint failed") || m.contains("duplicate primary key") {
        errno::ER_DUP_ENTRY
    } else if m.contains("does not exist (create table first)") || m.contains("not registered") {
        errno::ER_NO_SUCH_TABLE
    } else if m.contains("missing pk column") {
        errno::ER_BAD_NULL_ERROR
    } else if m.contains("fk violation") {
        errno::ER_NO_REFERENCED_ROW_2
    } else if m.starts_with("sql:") {
        errno::ER_PARSE_ERROR
    } else {
        // General class (MySQL HY000); message text still travels to the client.
        0
    }
}

pub struct CmsSqlGate {
    pub engine: Arc<SqlEngine>,
    pub locks: LockRegistry,
    sessions: Mutex<HashMap<u64, CmsSession>>,
    next_session: AtomicU64,
    /// Catalog overlay: MySQL-exact schema metadata per table (dbDelta).
    meta: RwLock<HashMap<String, CmsTableMeta>>,
}

impl CmsSqlGate {
    pub fn new(engine: Arc<SqlEngine>) -> Self {
        CmsSqlGate {
            engine,
            locks: LockRegistry::new(),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            meta: RwLock::new(HashMap::new()),
        }
    }

    /// Opens (or returns) a per-connection session. CMS hosting is
    /// connection-churn heavy (PHP-FPM), so sessions are cheap handles.
    pub fn open_session(&self) -> u64 {
        let id = self.next_session.fetch_add(1, Ordering::SeqCst);
        self.sessions
            .lock()
            .expect("session map poisoned")
            .insert(id, CmsSession::new(id));
        id
    }

    /// Releases per-connection state (connection close): named locks and
    /// the session record. Returns the number of locks freed.
    pub fn close_session(&self, id: u64) -> usize {
        let freed = self.locks.release_all(id);
        self.sessions.lock().expect("session map poisoned").remove(&id);
        freed
    }

    fn with_session<R>(&self, id: u64, f: impl FnOnce(&mut CmsSession) -> R) -> Option<R> {
        let mut map = self.sessions.lock().expect("session map poisoned");
        map.get_mut(&id).map(f)
    }

    /// Registers overlay metadata for a table (CREATE TABLE through the
    /// gate does this automatically; presets call it directly).
    pub fn register_table_meta(&self, meta: CmsTableMeta) {
        self.meta
            .write()
            .expect("meta map poisoned")
            .insert(meta.table.clone(), meta);
    }

    pub fn table_meta(&self, table: &str) -> Option<CmsTableMeta> {
        self.meta.read().expect("meta map poisoned").get(table).cloned()
    }

    /// Creates the WordPress core schema: overlay metadata for all 12 core
    /// tables + engine tables where the engine has none yet. Single-column
    /// secondary indexes are created in the engine; composite keys stay
    /// overlay-only (engine single-PK limitation, CMS_EDITION_RU.md §4).
    /// Returns the number of engine tables actually created.
    pub fn bootstrap_wordpress(&self) -> Result<usize, String> {
        let mut created = 0usize;
        for (name, ddl) in wordpress::WP_CORE_TABLES {
            let meta = CmsTableMeta::parse_create_table(ddl)?;
            if self.engine.get_schema(name).is_none() {
                self.engine.execute(&meta.normalized_engine_ddl())?;
                created += 1;
                // Engine-level indexes for single-column keys only.
                for idx in &meta.indexes {
                    if idx.columns.len() == 1 {
                        let col = &idx.columns[0].0;
                        let kind = if idx.unique { "UNIQUE INDEX" } else { "INDEX" };
                        let stmt = format!(
                            "CREATE {} IF NOT EXISTS {} ON {} ({})",
                            kind, idx.name, name, col
                        );
                        if let Err(e) = self.engine.execute(&stmt) {
                            // Index creation is best-effort at bootstrap;
                            // overlays keep the catalog truth either way.
                            eprintln!("[cms] index {} on {}: {}", idx.name, name, e);
                        }
                    }
                }
            }
            self.register_table_meta(meta);
        }
        Ok(created)
    }

    /// Executes one statement in the CMS dialect.
    pub fn execute(&self, session: u64, sql: &str) -> Result<CmsQueryResult, CmsError> {
        let sql = sql.trim();
        let sql = sql.strip_suffix(';').unwrap_or(sql).trim();
        if sql.is_empty() {
            return Err(CmsError { errno: errno::ER_PARSE_ERROR, message: "empty query".into() });
        }
        let upper = sql.to_ascii_uppercase();

        // ---------- session-level statements ----------
        if upper.starts_with("SET ") {
            return self.exec_set(session, sql, &upper);
        }
        if upper.starts_with("USE ") {
            let db = sql[4..].trim().trim_matches('`').trim_matches('\'').to_string();
            self.with_session(session, |s| s.database = Some(db.clone()));
            return Ok(empty_ok(format!("database changed to '{}'", db)));
        }

        // ---------- SELECT function shims ----------
        if upper.starts_with("SELECT ") || upper.starts_with("SELECT") {
            if let Some(res) = self.try_select_shims(session, sql, &upper)? {
                return Ok(res);
            }
        }

        // ---------- catalog overlay ----------
        if upper.starts_with("SHOW ") {
            if let Some(res) = self.try_show_shims(session, sql, &upper)? {
                return Ok(res);
            }
        }

        // ---------- SQL_CALC_FOUND_ROWS ----------
        let mut calc_found = false;
        let mut forwarded = sql.to_string();
        if let Some(pos) = find_ci(&forwarded, "SQL_CALC_FOUND_ROWS") {
            calc_found = true;
            forwarded.replace_range(pos..pos + "SQL_CALC_FOUND_ROWS".len(), "");
            forwarded = forwarded.split_whitespace().collect::<Vec<_>>().join(" ");
        }

        // ---------- real execution ----------
        let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH");
        let res = self
            .engine
            .execute(&forwarded)
            .map_err(|e| CmsError { errno: classify_error(&e), message: e })?;

        let mut out = CmsQueryResult {
            columns: res.columns,
            rows: res.rows,
            message: res.message.clone(),
            plan: res.execution_plan,
            affected_rows: 0,
            last_insert_id: 0,
            found_rows: 0,
            is_select,
        };
        // MySQL accounting.
        if is_select || upper.starts_with("SHOW") || upper.starts_with("DESCRIBE") {
            out.found_rows = out.rows.len() as u64;
            // MySQL: FOUND_ROWS() reflects the rows of the most recent
            // SELECT; with SQL_CALC_FOUND_ROWS it is the *unlimited* count —
            // we approximate it with the returned page (phase-1 note).
            self.with_session(session, |s| s.found_rows = out.found_rows);
        } else if let Some(msg) = &res.message {
            out.affected_rows = extract_row_count(msg);
            self.with_session(session, |s| s.affected_rows = out.affected_rows);
        }
        // LAST_INSERT_ID bookkeeping after INSERT.
        if upper.starts_with("INSERT") {
            let mut max_pk = 0u64;
            for r in &out.rows {
                if let Some(v) = r.get("pk") {
                    if let Value::Text(t) = v {
                        if let Ok(n) = t.parse::<u64>() {
                            max_pk = max_pk.max(n);
                        }
                    }
                    if let Value::Int(i) = v {
                        if *i > 0 {
                            max_pk = max_pk.max(*i as u64);
                        }
                    }
                }
            }
            if max_pk > 0 {
                out.last_insert_id = max_pk;
                self.with_session(session, |s| s.last_insert_id = max_pk);
            }
        }
        Ok(out)
    }

    // ---------------- SET / USE ----------------

    fn exec_set(&self, session: u64, sql: &str, upper: &str) -> Result<CmsQueryResult, CmsError> {
        let body = sql[4..].trim();
        let upper_body = upper[4..].trim();
        if upper_body.starts_with("NAMES") {
            let name = body["NAMES".len()..].trim().trim_matches('\'').trim_matches('"').trim().to_string();
            let name = name.split_whitespace().next().unwrap_or("utf8mb4").to_string();
            self.with_session(session, |s| s.set_names(&name));
            return Ok(empty_ok(format!("names set to {}", name)));
        }
        // SET [SESSION | @@session.] sql_mode = '...'
        let body_up = upper_body.replace("@@SESSION.", "").replace("@@GLOBAL.", "");
        if body_up.starts_with("SESSION ") {
            let trimmed = body_up["SESSION ".len()..].trim_start();
            let body_up2 = body["SESSION ".len()..].trim_start();
            if trimmed.starts_with("SQL_MODE") {
                let val = after_eq(body_up2).trim_matches('\'').trim_matches('"').to_string();
                self.with_session(session, |s| s.set_sql_mode(&val));
                return Ok(empty_ok("sql_mode set"));
            }
        }
        if body_up.starts_with("SQL_MODE") || body_up.starts_with("SQL_MODE=") {
            let val = after_eq(body).trim_matches('\'').trim_matches('"').to_string();
            self.with_session(session, |s| s.set_sql_mode(&val));
            return Ok(empty_ok("sql_mode set"));
        }
        if body_up.starts_with("AUTOCOMMIT") {
            let val = after_eq(body);
            let on = val == "1" || val.eq_ignore_ascii_case("ON") || val.eq_ignore_ascii_case("TRUE");
            self.with_session(session, |s| s.autocommit = on);
            return Ok(empty_ok(format!("autocommit = {}", if on { 1 } else { 0 })));
        }
        // Other SETs (character_set_client, collation_connection, ...) are
        // accepted and ignored: the CMS edition always speaks utf8mb4.
        Ok(empty_ok("compat: accepted"))
    }

    // ---------------- SELECT shims ----------------

    fn try_select_shims(
        &self,
        session: u64,
        sql: &str,
        upper: &str,
    ) -> Result<Option<CmsQueryResult>, CmsError> {
        let compact = upper.split_whitespace().collect::<Vec<_>>().join(" ");

        if compact.contains("LAST_INSERT_ID()") {
            let id = self.with_session(session, |s| s.last_insert_id).unwrap_or(0);
            return Ok(Some(one_value_reply("LAST_INSERT_ID()", Value::Int(id as i64))));
        }
        if compact.contains("FOUND_ROWS()") {
            let n = self.with_session(session, |s| s.found_rows).unwrap_or(0);
            return Ok(Some(one_value_reply("FOUND_ROWS()", Value::Int(n as i64))));
        }
        if compact.contains("ROW_COUNT()") {
            let n = self.with_session(session, |s| s.affected_rows).unwrap_or(0);
            return Ok(Some(one_value_reply("ROW_COUNT()", Value::Int(n as i64))));
        }
        if compact == "SELECT VERSION()" || compact.starts_with("SELECT VERSION() AS") {
            return Ok(Some(one_value_reply(
                "VERSION()",
                Value::Text(MYSQL_COMPAT_VERSION.to_string()),
            )));
        }
        if compact.contains("@@SQL_MODE") || compact.contains("@@SESSION.SQL_MODE") {
            let mode = self
                .with_session(session, |s| s.sql_mode.render())
                .unwrap_or_default();
            return Ok(Some(one_value_reply("@@sql_mode", Value::Text(mode))));
        }
        if compact.contains("@@VERSION") {
            return Ok(Some(one_value_reply(
                "@@version",
                Value::Text(MYSQL_COMPAT_VERSION.to_string()),
            )));
        }
        // Named advisory locks.
        if let Some(args) = fn_args(sql, "GET_LOCK") {
            let (name, timeout) = lock_args(&args);
            let got = self.locks.get_lock(&name, session, Duration::from_secs(timeout));
            return Ok(Some(one_value_reply(
                "GET_LOCK()",
                Value::Int(if got { 1 } else { 0 }),
            )));
        }
        if let Some(args) = fn_args(sql, "RELEASE_LOCK") {
            let (name, _) = lock_args(&args);
            let res = self.locks.release_lock(&name, session);
            return Ok(Some(one_value_reply(
                "RELEASE_LOCK()",
                match res {
                    ReleaseResult::Released => Value::Int(1),
                    ReleaseResult::NotOwner => Value::Int(0),
                    ReleaseResult::NotExists => Value::Null,
                },
            )));
        }
        if let Some(args) = fn_args(sql, "IS_FREE_LOCK") {
            let (name, _) = lock_args(&args);
            return Ok(Some(one_value_reply(
                "IS_FREE_LOCK()",
                Value::Int(if self.locks.is_free_lock(&name) { 1 } else { 0 }),
            )));
        }
        if let Some(args) = fn_args(sql, "IS_USED_LOCK") {
            let (name, _) = lock_args(&args);
            return Ok(Some(one_value_reply(
                "IS_USED_LOCK()",
                match self.locks.is_used_lock(&name) {
                    Some(holder) => Value::Int(holder as i64),
                    None => Value::Null,
                },
            )));
        }
        Ok(None)
    }

    // ---------------- SHOW shims ----------------

    fn try_show_shims(
        &self,
        session: u64,
        sql: &str,
        upper: &str,
    ) -> Result<Option<CmsQueryResult>, CmsError> {
        // SHOW TABLES [LIKE 'pat'] — engine's own SHOW TABLES has no LIKE.
        if upper.starts_with("SHOW TABLES") {
            let like = find_like_pattern(sql);
            let db = self
                .with_session(session, |s| s.database.clone().unwrap_or_default())
                .unwrap_or_default();
            let label = if db.is_empty() { "Tables_in_wordpress".to_string() } else { format!("Tables_in_{}", db) };
            let mut rows = Vec::new();
            for t in self.engine.list_tables() {
                if let Some(pat) = &like {
                    if !like_match(pat, &t) {
                        continue;
                    }
                }
                let mut r = BTreeMap::new();
                r.insert(label.clone(), Value::Text(t));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![label],
                rows,
                message: Some(format!("{} table(s)", found)),
                plan: "cms: SHOW TABLES [LIKE] via catalog".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW [FULL] COLUMNS|FIELDS FROM <table>
        if let Some(table) = show_target(sql, upper, &["SHOW FULL COLUMNS FROM", "SHOW COLUMNS FROM", "SHOW FULL FIELDS FROM", "SHOW FIELDS FROM"]) {
            let rows = self.show_columns_rows(&table);
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Field".into(), "Type".into(), "Collation".into(), "Null".into(),
                    "Key".into(), "Default".into(), "Extra".into(), "Privileges".into(),
                    "Comment".into(),
                ],
                rows,
                message: Some(format!("{} column(s)", found)),
                plan: format!("cms: SHOW COLUMNS overlay for {}", table),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW INDEX|INDEXES|KEYS FROM <table>
        if let Some(table) = show_target(sql, upper, &["SHOW INDEX FROM", "SHOW INDEXES FROM", "SHOW KEYS FROM"]) {
            let rows = self.show_index_rows(&table);
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Table".into(), "Non_unique".into(), "Key_name".into(), "Seq_in_index".into(),
                    "Column_name".into(), "Collation".into(), "Cardinality".into(), "Sub_part".into(),
                    "Packed".into(), "Null".into(), "Index_type".into(), "Comment".into(),
                ],
                rows,
                message: Some(format!("{} index row(s)", found)),
                plan: format!("cms: SHOW INDEX overlay for {}", table),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW VARIABLES LIKE 'pat' — a canned, CMS-relevant subset.
        if upper.starts_with("SHOW VARIABLES") {
            let pat = find_like_pattern(sql).unwrap_or("%".to_string());
            let vars: &[(&str, &str)] = &[
                ("version", MYSQL_COMPAT_VERSION),
                ("version_comment", "DNA-DB CMS Edition (MySQL 8.0 compatible)"),
                ("character_set_client", "utf8mb4"),
                ("character_set_connection", "utf8mb4"),
                ("character_set_results", "utf8mb4"),
                ("character_set_server", "utf8mb4"),
                ("collation_connection", "utf8mb4_unicode_520_ci"),
                ("collation_server", "utf8mb4_unicode_520_ci"),
                ("max_allowed_packet", "1073741824"),
                ("sql_mode", ""),
                ("autocommit", "1"),
                ("innodb_lock_wait_timeout", "50"),
                ("wait_timeout", "28800"),
            ];
            let mut rows = Vec::new();
            for (k, v) in vars {
                if like_match(&pat, k) {
                    let mut r = BTreeMap::new();
                    r.insert("Variable_name".into(), Value::Text(k.to_string()));
                    r.insert("Value".into(), Value::Text(v.to_string()));
                    rows.push(r);
                }
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec!["Variable_name".into(), "Value".into()],
                rows,
                message: None,
                plan: "cms: SHOW VARIABLES canned subset".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }
        Ok(None)
    }

    /// SHOW FULL COLUMNS rows: overlay when registered, else derived from
    /// the engine schema (coarser types, documented fallback).
    fn show_columns_rows(&self, table: &str) -> Vec<BTreeMap<String, Value>> {
        if let Some(meta) = self.table_meta(table) {
            return meta.show_full_columns_rows();
        }
        match self.engine.get_schema(table) {
            Some(schema) => {
                let mut rows = Vec::new();
                for (name, ty) in &schema.columns {
                    let mut r = BTreeMap::new();
                    let mysql = fallback_type(ty);
                    r.insert("Field".into(), Value::Text(name.clone()));
                    r.insert("Type".into(), Value::Text(mysql.clone()));
                    r.insert(
                        "Collation".into(),
                        if mysql.contains("char") || mysql == "text" {
                            Value::Text("utf8mb4_unicode_520_ci".into())
                        } else {
                            Value::Null
                        },
                    );
                    r.insert("Null".into(), Value::Text("YES".into()));
                    r.insert(
                        "Key".into(),
                        Value::Text(
                            if *name == schema.primary_locus_column { "PRI" } else { "" }.into(),
                        ),
                    );
                    r.insert("Default".into(), Value::Null);
                    r.insert("Extra".into(), Value::Text(String::new()));
                    r.insert(
                        "Privileges".into(),
                        Value::Text("select,insert,update,references".into()),
                    );
                    r.insert("Comment".into(), Value::Text(String::new()));
                    rows.push(r);
                }
                rows
            }
            None => Vec::new(),
        }
    }

    /// SHOW INDEX rows: overlay when registered, else derived from the
    /// engine schema (PRIMARY + unique secondary indexes).
    fn show_index_rows(&self, table: &str) -> Vec<BTreeMap<String, Value>> {
        if let Some(meta) = self.table_meta(table) {
            return meta.show_index_rows();
        }
        let Some(schema) = self.engine.get_schema(table) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut push = |key_name: &str, non_unique: i64, col: &str, seq: i64, sub: Option<u16>| {
            let mut r = BTreeMap::new();
            r.insert("Table".into(), Value::Text(table.to_string()));
            r.insert("Non_unique".into(), Value::Int(non_unique));
            r.insert("Key_name".into(), Value::Text(key_name.to_string()));
            r.insert("Seq_in_index".into(), Value::Int(seq));
            r.insert("Column_name".into(), Value::Text(col.to_string()));
            r.insert("Collation".into(), Value::Text("A".into()));
            r.insert("Cardinality".into(), Value::Int(0));
            r.insert(
                "Sub_part".into(),
                match sub {
                    Some(n) => Value::Int(n as i64),
                    None => Value::Null,
                },
            );
            r.insert("Packed".into(), Value::Null);
            r.insert("Null".into(), Value::Text(String::new()));
            r.insert("Index_type".into(), Value::Text("BTREE".into()));
            r.insert("Comment".into(), Value::Text(String::new()));
            rows.push(r);
        };
        push("PRIMARY", 0, &schema.primary_locus_column, 1, None);
        for idx in &schema.secondary_indexes {
            push(&idx.name, if idx.unique { 0 } else { 1 }, &idx.column, 1, None);
        }
        rows
    }
}

// ---------------- free helpers ----------------

fn empty_ok(message: impl Into<String>) -> CmsQueryResult {
    CmsQueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: Some(message.into()),
        plan: "cms: session statement".into(),
        affected_rows: 0,
        last_insert_id: 0,
        found_rows: 0,
        is_select: false,
    }
}

fn one_value_reply(column: &str, value: Value) -> CmsQueryResult {
    let mut r = BTreeMap::new();
    r.insert(column.to_string(), value);
    CmsQueryResult {
        columns: vec![column.to_string()],
        rows: vec![r],
        message: None,
        plan: "cms: function shim".into(),
        affected_rows: 0,
        last_insert_id: 0,
        found_rows: 1,
        is_select: true,
    }
}

/// Value after '=' (for SET sql_mode = '…', autocommit=0).
fn after_eq(s: &str) -> String {
    match s.find('=') {
        Some(pos) => s[pos + 1..].trim().to_string(),
        None => String::new(),
    }
}

/// Case-insensitive substring search.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.find(&n)
}

/// Extracts the argument list of `FN(...)` in `sql` (first occurrence).
fn fn_args(sql: &str, fname: &str) -> Option<String> {
    let pos = find_ci(sql, fname)?;
    let rest = &sql[pos + fname.len()..];
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let end = rest.find(')')?;
    Some(rest[1..end].to_string())
}

/// Parses GET_LOCK args: 'name' [, timeout]. Defaults: name='', timeout=0.
fn lock_args(args: &str) -> (String, u64) {
    let mut parts = args.split(',');
    let name = parts
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_string();
    let timeout = parts
        .next()
        .unwrap_or("0")
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    (name, timeout)
}

/// LIKE pattern from `LIKE '<pat>'`.
fn find_like_pattern(sql: &str) -> Option<String> {
    let pos = find_ci(sql, "LIKE")?;
    let rest = sql[pos + 4..].trim_start();
    if !rest.starts_with('\'') {
        return None;
    }
    let end = rest[1..].find('\'')? + 1;
    Some(rest[1..end].to_string())
}

/// MySQL LIKE with % (any run) and _ (one char), case-insensitive.
pub fn like_match(pattern: &str, text: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    like_rec(p.as_bytes(), t.as_bytes())
}

fn like_rec(p: &[u8], t: &[u8]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (Some(b'%'), _) => like_rec(&p[1..], t) || (!t.is_empty() && like_rec(p, &t[1..])),
        (Some(b'\\'), Some(&c)) if p.len() >= 2 => c == t[0] && like_rec(&p[2..], &t[1..]),
        (Some(b'_'), Some(_)) => like_rec(&p[1..], &t[1..]),
        (Some(&c), Some(&d)) => c == d && like_rec(&p[1..], &t[1..]),
        _ => false,
    }
}

/// Table name after one of the SHOW … FROM <table> heads.
fn show_target(sql: &str, upper: &str, heads: &[&str]) -> Option<String> {
    for head in heads {
        if upper.starts_with(head) {
            let rest = sql[head.len()..].trim();
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// "N row(s) inserted/updated/deleted" → N.
fn extract_row_count(message: &str) -> u64 {
    let m = message.to_ascii_lowercase();
    if let Some(pos) = m.find(" row(s)") {
        let start = m[..pos]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        return start.parse::<u64>().unwrap_or(0);
    }
    0
}

/// Fallback Type rendering for tables without overlay metadata.
fn fallback_type(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Int64 => "bigint(20)".to_string(),
        ColumnType::Float64 => "double".to_string(),
        ColumnType::Timestamp => "datetime".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Dna2Bit => "text".to_string(),
        ColumnType::ForeignLink { .. } => "bigint(20)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};
    use crate::wal::SyncMode;
    use std::sync::Arc;

    fn gate(name: &str) -> CmsSqlGate {
        let dir = std::env::temp_dir().join(format!(
            "dnadb_cms_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cfg = EngineConfig {
            dir_path: dir,
            sync_mode: SyncMode::Batched,
            ..EngineConfig::default()
        };
        let engine = Engine::open(cfg).unwrap();
        CmsSqlGate::new(Arc::new(SqlEngine::new(engine)))
    }

    fn v(row: &BTreeMap<String, Value>, k: &str) -> String {
        row.get(k).map(|v| v.to_string()).unwrap_or_default()
    }

    #[test]
    fn wordpress_bootstrap_and_autoincrement_flow() {
        let g = gate("ai_flow");
        let created = g.bootstrap_wordpress().unwrap();
        assert_eq!(created, 12, "12 core tables created");
        let s = g.open_session();

        // INSERT without the sequence column → id 1.
        let r = g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('foo','bar','yes')").unwrap();
        assert_eq!(r.affected_rows, 1);
        assert_eq!(r.last_insert_id, 1);

        // wpdb reads it back via LAST_INSERT_ID().
        let r = g.execute(s, "SELECT LAST_INSERT_ID()").unwrap();
        assert_eq!(v(&r.rows[0], "LAST_INSERT_ID()"), "1");

        // Second insert → 2.
        g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('baz','qux','no')").unwrap();
        let r = g.execute(s, "SELECT LAST_INSERT_ID()").unwrap();
        assert_eq!(v(&r.rows[0], "LAST_INSERT_ID()"), "2");

        // Explicit id = 50 moves the counter.
        g.execute(s, "INSERT INTO wp_options (option_id, option_name, option_value, autoload) VALUES (50, 'big','1','yes')").unwrap();
        let r = g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('after','x','yes')").unwrap();
        assert_eq!(r.last_insert_id, 51);

        // The row really exists under the generated key.
        let r = g.execute(s, "SELECT option_name FROM wp_options WHERE option_id = 51").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(v(&r.rows[0], "option_name"), "after");
    }

    #[test]
    fn session_statements_and_functions() {
        let g = gate("session");
        let s = g.open_session();

        g.execute(s, "SET NAMES utf8mb4").unwrap();
        g.execute(s, "SET sql_mode = 'STRICT_TRANS_TABLES,NO_ZERO_DATE'").unwrap();
        let r = g.execute(s, "SELECT @@SESSION.sql_mode").unwrap();
        let mode = v(&r.rows[0], "@@sql_mode");
        assert!(mode.contains("STRICT_TRANS_TABLES") && mode.contains("NO_ZERO_DATE"));

        g.execute(s, "SET autocommit = 0").unwrap();
        assert!(!g.with_session(s, |x| x.autocommit).unwrap());
        g.execute(s, "SET autocommit = 1").unwrap();

        let r = g.execute(s, "SELECT VERSION()").unwrap();
        assert_eq!(v(&r.rows[0], "VERSION()"), MYSQL_COMPAT_VERSION);

        // FOUND_ROWS() across two SELECTs.
        g.bootstrap_wordpress().unwrap();
        g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('a','1','yes'),('b','2','yes'),('c','3','no')").unwrap();
        g.execute(s, "SELECT option_name FROM wp_options WHERE autoload = 'yes'").unwrap();
        let r = g.execute(s, "SELECT FOUND_ROWS()").unwrap();
        assert_eq!(v(&r.rows[0], "FOUND_ROWS()"), "2");

        // USE / SHOW TABLES LIKE.
        g.execute(s, "USE wordpress").unwrap();
        assert_eq!(g.with_session(s, |x| x.database.clone()).unwrap().as_deref(), Some("wordpress"));
        let r = g.execute(s, "SHOW TABLES LIKE 'wp_opt%'").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(v(&r.rows[0], "Tables_in_wordpress"), "wp_options");
    }

    #[test]
    fn named_locks_via_sql() {
        let g = gate("locks");
        let a = g.open_session();
        let b = g.open_session();

        let r = g.execute(a, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
        assert_eq!(v(&r.rows[0], "GET_LOCK()"), "1");

        // Re-entrant for the holder.
        let r = g.execute(a, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
        assert_eq!(v(&r.rows[0], "GET_LOCK()"), "1");

        // Other session times out.
        let r = g.execute(b, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
        assert_eq!(v(&r.rows[0], "GET_LOCK()"), "0");

        let r = g.execute(b, "SELECT IS_FREE_LOCK('wp_cron')").unwrap();
        assert_eq!(v(&r.rows[0], "IS_FREE_LOCK()"), "0");
        let r = g.execute(b, "SELECT IS_USED_LOCK('wp_cron')").unwrap();
        assert_eq!(v(&r.rows[0], "IS_USED_LOCK()"), a.to_string());

        let r = g.execute(a, "SELECT RELEASE_LOCK('wp_cron')").unwrap();
        assert_eq!(v(&r.rows[0], "RELEASE_LOCK()"), "1");
        let r = g.execute(b, "SELECT RELEASE_LOCK('wp_cron')").unwrap();
        assert!(r.rows[0].get("RELEASE_LOCK()").unwrap().is_null());

        // Session close frees remaining locks.
        g.execute(a, "SELECT GET_LOCK('again', 0)").unwrap();
        assert_eq!(g.close_session(a), 1);
    }

    #[test]
    fn show_full_columns_is_dbdelta_ready() {
        let g = gate("columns");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();
        let r = g.execute(s, "SHOW FULL COLUMNS FROM wp_options").unwrap();
        assert_eq!(r.columns.first().map(String::as_str), Some("Field"));
        assert_eq!(r.rows.len(), 4);

        assert_eq!(v(&r.rows[0], "Field"), "option_id");
        assert_eq!(v(&r.rows[0], "Type"), "bigint(20) unsigned"); // raw DDL text
        assert_eq!(v(&r.rows[0], "Null"), "NO");
        assert_eq!(v(&r.rows[0], "Key"), "PRI");
        assert!(r.rows[0].get("Default").unwrap().is_null());
        assert_eq!(v(&r.rows[0], "Extra"), "auto_increment");

        assert_eq!(v(&r.rows[1], "Field"), "option_name");
        assert_eq!(v(&r.rows[1], "Type"), "varchar(191)");
        assert_eq!(v(&r.rows[1], "Key"), "UNI");
        assert_eq!(v(&r.rows[1], "Default"), "");

        let r = g.execute(s, "SHOW INDEX FROM wp_options").unwrap();
        let key = |i: usize| v(&r.rows[i], "Key_name");
        let seq = |i: usize| v(&r.rows[i], "Seq_in_index");
        assert_eq!(key(0), "PRIMARY");
        assert_eq!(seq(0), "1");
        assert_eq!(key(1), "option_name");
        assert_eq!(v(&r.rows[1], "Non_unique"), "0");
    }

    #[test]
    fn dbdelta_style_comparison_is_idempotent() {
        // Mirrors the dbDelta loop: canonical "Field Type" + NOT NULL +
        // DEFAULT + EXTRA strings from the DDL must equal those from SHOW
        // FULL COLUMNS, on every run (no eternal ALTERs).
        let g = gate("dbdelta");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();
        let show = g.execute(s, "SHOW FULL COLUMNS FROM wp_options").unwrap();

        let ddl = wordpress::WP_CORE_TABLES
            .iter()
            .find(|(n, _)| *n == "wp_options")
            .map(|(_, d)| d.to_string())
            .unwrap();
        let meta = CmsTableMeta::parse_create_table(&ddl).unwrap();

        for (i, col) in meta.columns.iter().enumerate() {
            let from_ddl = format!(
                "{} {} {} default {} {}",
                col.field,
                col.mysql_type,
                if col.not_null { "NOT NULL" } else { "NULL" },
                col.default.clone().unwrap_or_else(|| "NULL".into()),
                if col.auto_increment { "auto_increment" } else { "" }
            );
            let row = &show.rows[i];
            let from_show = format!(
                "{} {} {} default {} {}",
                v(row, "Field"),
                v(row, "Type"),
                if v(row, "Null") == "NO" { "NOT NULL" } else { "NULL" },
                match row.get("Default") {
                    Some(Value::Text(t)) => t.clone(),
                    _ => "NULL".to_string(),
                },
                v(row, "Extra")
            );
            assert_eq!(from_ddl, from_show, "dbDelta would ALTER after column {}", col.field);
        }
    }

    #[test]
    fn errors_carry_mysql_errno() {
        let g = gate("errors");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();

        // No such table → 1146.
        let e = g.execute(s, "SELECT * FROM wp_missing").unwrap_err();
        assert_eq!(e.errno, errno::ER_NO_SUCH_TABLE);
        assert_eq!(e.sqlstate(), "42S02");

        // Duplicate unique key (option_name) → 1062.
        g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('dup','1','yes')").unwrap();
        let e = g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('dup','2','yes')").unwrap_err();
        assert_eq!(e.errno, errno::ER_DUP_ENTRY);
        assert_eq!(e.sqlstate(), "23000");

        // Parse error → 1064.
        let e = g.execute(s, "SELECT * FRM wp_options").unwrap_err();
        assert_eq!(e.errno, errno::ER_PARSE_ERROR);
    }

    #[test]
    fn show_variables_and_sql_calc_found_rows() {
        let g = gate("vars");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();

        let r = g.execute(s, "SHOW VARIABLES LIKE 'character_set%'").unwrap();
        assert!(r.rows.len() >= 3);
        assert_eq!(v(&r.rows[0], "Variable_name"), "character_set_client");

        // SQL_CALC_FOUND_ROWS is stripped transparently and still counts.
        g.execute(s, "INSERT INTO wp_posts (post_title, post_status, post_type, post_name) VALUES ('T1','publish','post','t1'),('T2','publish','post','t2')").unwrap();
        let r = g
            .execute(s, "SELECT SQL_CALC_FOUND_ROWS ID FROM wp_posts WHERE post_status = 'publish' LIMIT 1")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "LIMIT 1 still applies");
        let r = g.execute(s, "SELECT FOUND_ROWS()").unwrap();
        assert_eq!(v(&r.rows[0], "FOUND_ROWS()"), "1");

        // Row exists with generated ID; post_date_gmt was never provided →
        // stored sparse (NULL/absent). Phase-1 note: engine-level DEFAULT
        // fill for omitted columns is backlog (CMS_EDITION_RU.md §4, item 11);
        // WordPress core always sends date columns explicitly.
        let r = g.execute(s, "SELECT post_date_gmt FROM wp_posts WHERE ID = 1").unwrap();
        assert!(matches!(
            r.rows[0].get("post_date_gmt"),
            None | Some(Value::Null)
        ));
        let r = g.execute(s, "SELECT ID FROM wp_posts WHERE post_name = 't1'").unwrap();
        assert_eq!(v(&r.rows[0], "ID"), "1");
    }

    #[test]
    fn plugin_ddl_passes_through_gate() {
        // Plugin activation (dbDelta) sends raw MySQL DDL — no overlay.
        // The engine's cms grammar must swallow types/lengths/DEFAULTs and
        // the trailing ENGINE/CHARSET/COLLATE options.
        let g = gate("plugin_ddl");
        let s = g.open_session();
        g.execute(s, "CREATE TABLE wp_myplugin_data (
            id bigint(20) unsigned NOT NULL AUTO_INCREMENT,
            post_id bigint(20) unsigned NOT NULL DEFAULT '0',
            value decimal(10,2) NOT NULL DEFAULT '0.00',
            note varchar(255) DEFAULT '',
            created datetime NOT NULL DEFAULT '0000-00-00 00:00:00',
            PRIMARY KEY  (id),
            KEY post_id (post_id)
        ) ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci").unwrap();

        let r = g.execute(s, "INSERT INTO wp_myplugin_data (post_id, value) VALUES (5, 19.99)").unwrap();
        assert_eq!(r.last_insert_id, 1);
        let r = g.execute(s, "SELECT value FROM wp_myplugin_data WHERE id = 1").unwrap();
        assert_eq!(v(&r.rows[0], "value"), "19.99");

        // Fallback catalog (no overlay): engine-derived types. Note: the
        // engine stores columns in BTreeMap (alphabetical) order, so look
        // rows up by Field, not by index.
        let r = g.execute(s, "SHOW FULL COLUMNS FROM wp_myplugin_data").unwrap();
        assert_eq!(r.rows.len(), 5);
        let row = |name: &str| {
            r.rows
                .iter()
                .find(|row| v(row, "Field") == name)
                .unwrap_or_else(|| panic!("column {} not shown", name))
        };
        assert_eq!(v(row("id"), "Type"), "bigint(20)");
        assert_eq!(v(row("id"), "Key"), "PRI");
        assert_eq!(v(row("value"), "Type"), "double");
        assert_eq!(v(row("created"), "Type"), "datetime");
    }

    #[test]
    fn fallback_catalog_for_non_cms_tables() {
        let g = gate("fallback");
        g.engine
            .execute("CREATE TABLE chr_plain (id INT, name TEXT, PRIMARY KEY (id))")
            .unwrap();
        let s = g.open_session();
        let r = g.execute(s, "SHOW FULL COLUMNS FROM chr_plain").unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(v(&r.rows[0], "Field"), "id");
        assert_eq!(v(&r.rows[0], "Type"), "bigint(20)"); // engine-derived
        assert_eq!(v(&r.rows[0], "Key"), "PRI");
        let r = g.execute(s, "SHOW INDEX FROM chr_plain").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(v(&r.rows[0], "Key_name"), "PRIMARY");
    }
}
