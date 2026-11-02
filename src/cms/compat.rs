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
use crate::cms::row_locks::{RowLockRegistry, RowLockWait};
use crate::cms::session::CmsSession;
use crate::cms::wordpress;
use crate::relational::{ColumnType, Value};
use crate::sql_engine::SqlEngine;

/// Version string reported to CMS clients (wpdb::db_version gate checks the
/// numeric prefix; the MariaDB form matches the reference engine used by the
/// majority of WordPress hosting and the differential test corpus).
pub const MYSQL_COMPAT_VERSION: &str = "10.11.11-MariaDB";

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
    } else if m.contains("can't drop") || m.contains("cannot drop") {
        errno::ER_CANT_DROP_FIELD_OR_KEY
    } else if m.contains("unknown column") || m.contains("no such column") {
        errno::ER_BAD_FIELD_ERROR
    } else if m.contains("already has an index") {
        1061 // ER_DUP_KEYNAME
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
    /// Row-level locks for `SELECT … FOR UPDATE` / blocked DML (§4 item 2):
    /// InnoDB-style waits (1205), `NOWAIT` (3572) and deadlock detection
    /// (1213) over a waits-for graph.
    pub row_locks: RowLockRegistry,
    sessions: Mutex<HashMap<u64, CmsSession>>,
    next_session: AtomicU64,
    /// Catalog overlay: MySQL-exact schema metadata per table (dbDelta).
    meta: RwLock<HashMap<String, CmsTableMeta>>,
    /// Active BEGIN..COMMIT buffers per session (taken out for each
    /// statement, put back after — the engine mutates it in place).
    txs: Mutex<HashMap<u64, crate::sql_engine::SqlTxBuffer>>,
}

impl CmsSqlGate {
    pub fn new(engine: Arc<SqlEngine>) -> Self {
        CmsSqlGate {
            engine,
            locks: LockRegistry::new(),
            row_locks: RowLockRegistry::new(),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            meta: RwLock::new(HashMap::new()),
            txs: Mutex::new(HashMap::new()),
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

    /// Releases per-connection state (connection close): named locks, row
    /// locks and the session record. Returns the number of locks freed.
    pub fn close_session(&self, id: u64) -> usize {
        // MySQL rolls an open transaction back on disconnect — row locks die
        // with it.
        let freed = self.locks.release_all(id) + self.row_locks.release_session(id);
        // Uncommitted transaction state dies with the connection (MySQL
        // rolls an open transaction back on disconnect).
        self.txs.lock().expect("tx map poisoned").remove(&id);
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
        // MySQL-dialect preprocessing before any dispatch: backtick-quoted
        // identifiers, `DESCRIBE` → SHOW FULL COLUMNS, `NOW()` in DML.
        let mut pre = preprocess_mysql_dialect(sql);
        // CMS edition: MySQL-only SELECT forms that the core engine does not
        // parse — SELECT DISTINCT (strip + post-dedupe) and comma-FROM lists
        // (`FROM a, b WHERE …`, rewritten to cross joins). Both are emitted
        // by WordPress core (update_object_term_cache, transient cleanup).
        let want_distinct = strip_select_distinct(&mut pre);
        if pre.trim_start()[..3.min(pre.trim_start().len())].eq_ignore_ascii_case("sel") {
            pre = rewrite_comma_from(&pre);
        }
        let sql = pre.trim();
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

        // MySQL: COMMIT / ROLLBACK without an open transaction are silent
        // no-ops. PHP drivers that run with autocommit=0 (pymysql, mysqli
        // defaults, wpdb in transactional plugins) send bare COMMITs after
        // every write, so rejecting them here breaks real clients with
        // errno 1064 where real MySQL answers OK.
        if upper == "COMMIT" || upper == "ROLLBACK" {
            let has_tx = self
                .txs
                .lock()
                .expect("tx map poisoned")
                .contains_key(&session);
            if !has_tx {
                self.row_locks.release_session(session);
                return Ok(empty_ok(format!(
                    "{}, no active transaction",
                    upper.to_ascii_lowercase()
                )));
            }
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

        // ---------- CREATE TABLE: register the MySQL-dialect overlay ----------
        // The engine parses the raw DDL itself (cms grammar); the overlay
        // keeps the exact MySQL types so SHOW FULL COLUMNS / SHOW INDEX are
        // byte-for-byte dbDelta-compatible for plugin tables too.
        if upper.starts_with("CREATE TABLE") {
            if let Ok(meta) = CmsTableMeta::parse_create_table(sql) {
                self.register_table_meta(meta);
            }
        }

        // ---------- SQL_CALC_FOUND_ROWS ----------
        let mut forwarded = sql.to_string();
        if let Some(pos) = find_ci(&forwarded, "SQL_CALC_FOUND_ROWS") {
            forwarded.replace_range(pos..pos + "SQL_CALC_FOUND_ROWS".len(), "");
            forwarded = forwarded.split_whitespace().collect::<Vec<_>>().join(" ");
        }

        // ---------- row locks (CMS_EDITION_RU.md §4 item 2) ----------
        // `SELECT … FOR UPDATE|SHARE` takes row locks on the returned rows;
        // `UPDATE`/`DELETE` wait for (or take, inside a tx) the same locks on
        // their target rows — WooCommerce stock serialization.
        let sel_locking = if upper.starts_with("SELECT") {
            find_locking_tail(sql)
        } else {
            None
        };
        let write_probe = if upper.starts_with("UPDATE") || upper.starts_with("DELETE ") {
            self.write_target_probe(sql)
        } else {
            None
        };
        // InnoDB: DML blocks while an open transaction holds a target row.
        if let Some((table, key_sql)) = &write_probe {
            self.wait_write_targets(session, table, key_sql)?;
        }

        // ---------- real execution ----------
        let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH");
        // The session owns the BEGIN..COMMIT buffer: while a transaction is
        // active, DML is buffered here and applied as one atomic WAL batch
        // on COMMIT (v3.6 transaction semantics, one buffer per connection).
        let mut tx_buf = self.txs.lock().expect("tx map poisoned").remove(&session);
        let mut res = match self.engine.execute_in_tx(&forwarded, &mut tx_buf) {
            Ok(res) => {
                if let Some(buf) = tx_buf.take() {
                    self.txs
                        .lock()
                        .expect("tx map poisoned")
                        .insert(session, buf);
                }
                res
            }
            Err(e) => {
                // MySQL parity: a failed statement does NOT abort the open
                // transaction (only the statement itself is rolled back) —
                // WooCommerce retry loops rely on the tx surviving 1213s.
                if let Some(buf) = tx_buf.take() {
                    self.txs
                        .lock()
                        .expect("tx map poisoned")
                        .insert(session, buf);
                }
                return Err(CmsError { errno: classify_error(&e), message: e });
            }
        };

        // InnoDB two-phase locking: COMMIT / ROLLBACK end the transaction and
        // release every row lock it holds.
        if upper == "COMMIT" || upper == "ROLLBACK" {
            self.row_locks.release_session(session);
        }
        // `SELECT … FOR UPDATE`: lock the returned rows. Inside a transaction
        // the holds survive until COMMIT/ROLLBACK; in autocommit they end
        // with the statement (MySQL semantics).
        let mut out_note = String::new();
        if let Some((tail_pos, nowait, skip_locked)) = sel_locking {
            let mut rows = std::mem::take(&mut res.rows);
            let locked = self.lock_select_rows(session, sql, tail_pos, nowait, skip_locked, &mut rows)?;
            res.rows = rows;
            if locked > 0 {
                out_note.push_str(&format!(" row_locks={}", locked));
            }
        }

        let mut out = CmsQueryResult {
            columns: res.columns,
            rows: res.rows,
            message: res.message.clone(),
            plan: format!("{}{}", res.execution_plan, out_note),
            affected_rows: 0,
            last_insert_id: 0,
            found_rows: 0,
            is_select,
        };
        // SELECT DISTINCT: MySQL returns the projection deduplicated; keep
        // the first occurrence so ORDER BY output stays stable.
        if want_distinct && is_select {
            use std::collections::HashSet;
            let mut seen: HashSet<String> = HashSet::new();
            out.rows.retain(|row| seen.insert(format!("{:?}", row)));
            out.message = Some(format!("{} row(s)", out.rows.len()));
        }
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
            // MySQL: LAST_INSERT_ID is the FIRST id generated by the most
            // recent INSERT — multi-row inserts return the first row's id,
            // and an ODKU-update leaves it unchanged (no pk rows).
            let mut first_pk: Option<u64> = None;
            for r in &out.rows {
                if let Some(v) = r.get("pk") {
                    let n = match v {
                        Value::Text(t) => t.parse::<u64>().ok(),
                        Value::Int(i) if *i > 0 => Some(*i as u64),
                        _ => None,
                    };
                    if n.is_some() {
                        first_pk = n;
                        break;
                    }
                }
            }
            if let Some(pk) = first_pk {
                out.last_insert_id = pk;
                self.with_session(session, |s| s.last_insert_id = pk);
            }
        }
        Ok(out)
    }

    // ---------------- SET / USE ----------------

    // ---------------- row locks (§4 item 2) ----------------

    /// Is a BEGIN..COMMIT transaction open for this session?
    fn tx_active(&self, session: u64) -> bool {
        self.txs
            .lock()
            .expect("tx map poisoned")
            .get(&session)
            .map(|b| b.active)
            .unwrap_or(false)
    }

    /// Runs a statement with the session's tx buffer (same read visibility
    /// as client statements — the probe must see the transaction's writes).
    fn run_with_tx(
        &self,
        session: u64,
        sql: &str,
    ) -> Result<crate::sql_engine::QueryResult, String> {
        let mut tx_buf = self.txs.lock().expect("tx map poisoned").remove(&session);
        let res = self.engine.execute_in_tx(sql, &mut tx_buf);
        if let Some(buf) = tx_buf.take() {
            self.txs
                .lock()
                .expect("tx map poisoned")
                .insert(session, buf);
        }
        res
    }

    /// Row-lock wait budget of the session (`innodb_lock_wait_timeout`);
    /// `NOWAIT` means zero — a held row errors immediately.
    fn lock_timeout_of(&self, session: u64, nowait: bool) -> Duration {
        if nowait {
            return Duration::ZERO;
        }
        let secs = self
            .with_session(session, |s| s.lock_wait_timeout)
            .unwrap_or(50.0);
        Duration::from_secs_f64(secs.clamp(0.0, 3600.0))
    }

    /// Locks the rows a `SELECT … FOR UPDATE` returned. Resolves the PK of
    /// the main table from the overlay metadata — from the result rows when
    /// the projection carries it (`SELECT *`, Action Scheduler), otherwise
    /// by re-querying the same FROM/WHERE for `SELECT <pk>`. Returns the
    /// number of rows locked; with `skip_locked`, foreign-held rows are
    /// dropped from the result instead of waited on (queue semantics).
    #[allow(clippy::too_many_arguments)]
    fn lock_select_rows(
        &self,
        session: u64,
        sql: &str,
        tail_pos: usize,
        nowait: bool,
        skip_locked: bool,
        rows: &mut Vec<BTreeMap<String, Value>>,
    ) -> Result<usize, CmsError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let Some(from_pos) = top_level_find_ci(sql, "FROM") else { return Ok(0) };
        if from_pos >= tail_pos {
            return Ok(0);
        }
        let tail = &sql[from_pos..tail_pos];
        // Aggregates change the row identity — InnoDB locks the scanned
        // rows; we conservatively skip (documented divergence).
        if top_level_find_ci(tail, "GROUP").is_some() {
            return Ok(0);
        }
        let Some(table) = first_table_ident(&tail[4..]) else { return Ok(0) };
        let Some(pk) = self.table_meta(&table).and_then(|m| m.primary_key.first().cloned())
        else {
            return Ok(0);
        };
        let in_tx = self.tx_active(session);
        // SKIP LOCKED never waits: try once, drop on foreign hold.
        let timeout = self.lock_timeout_of(session, nowait || skip_locked);
        // Fast path: the projection already carries the PK.
        let projected = rows[0].contains_key(&pk);
        let keys: Vec<String> = if projected {
            rows.iter().filter_map(|r| r.get(&pk).and_then(value_key)).collect()
        } else {
            let key_sql = format!("SELECT {} {}", pk, tail);
            match self.run_with_tx(session, &key_sql) {
                Ok(r) => {
                    r.rows
                        .iter()
                        .filter_map(|row| row.get(&pk).and_then(value_key))
                        .collect()
                }
                // Best-effort: a key query the engine cannot parse must
                // never fail the client statement.
                Err(_) => return Ok(0),
            }
        };
        let mut locked = 0usize;
        let mut skipped: Vec<usize> = Vec::new();
        for (idx, key) in keys.iter().enumerate() {
            if skip_locked && self.row_locks.probe(&table, key, session).is_some() {
                skipped.push(idx);
                continue;
            }
            match self.row_locks.acquire(&table, key, session, timeout) {
                Ok(_) => {
                    if in_tx {
                        locked += 1;
                    }
                }
                Err(w) => {
                    if skip_locked {
                        skipped.push(idx);
                        continue;
                    }
                    if !in_tx {
                        self.row_locks.release_session(session);
                    }
                    return Err(row_lock_error(w, nowait));
                }
            }
        }
        if !in_tx {
            // Autocommit: the statement's locks end with the statement.
            self.row_locks.release_session(session);
        }
        // SKIP LOCKED with a projected PK: excluded rows leave the result
        // (InnoDB returns only the rows it actually locked).
        if projected && !skipped.is_empty() {
            let mut i = 0usize;
            rows.retain(|_| {
                let keep = !skipped.contains(&i);
                i += 1;
                keep
            });
        }
        Ok(locked)
    }

    /// Textual probe plan for an UPDATE/DELETE: (main table, `SELECT <pk>`
    /// query over the same WHERE). `None` when the target set cannot be
    /// derived (no WHERE, multi-table DELETE, unknown table, subquery FROM).
    fn write_target_probe(&self, sql: &str) -> Option<(String, String)> {
        let up = sql.to_ascii_uppercase();
        if up.starts_with("UPDATE") {
            let set_pos = top_level_find_ci(sql, "SET")?;
            let table = first_table_ident(&sql[6..set_pos])?;
            let pk = self
                .table_meta(&table)
                .and_then(|m| m.primary_key.first().cloned())?;
            let where_pos = top_level_find_ci(sql, "WHERE")?;
            Some((
                table.clone(),
                format!("SELECT {} FROM {} {}", pk, table, &sql[where_pos..]),
            ))
        } else if up.starts_with("DELETE ") {
            let from_pos = top_level_find_ci(sql, "FROM")?;
            // Multi-table DELETE (`DELETE a, b FROM …`) — not targeted here.
            if sql[..from_pos].contains(',') {
                return None;
            }
            let table = first_table_ident(&sql[from_pos + 4..])?;
            let pk = self
                .table_meta(&table)
                .and_then(|m| m.primary_key.first().cloned())?;
            Some((table, format!("SELECT {} {}", pk, &sql[from_pos..])))
        } else {
            None
        }
    }

    /// InnoDB write-write conflict: while an open transaction holds a
    /// target row, the DML waits. Inside a transaction the DML *takes* the
    /// locks (X-locks held until COMMIT) so opposite-order updates deadlock
    /// with errno 1213 exactly like InnoDB.
    fn wait_write_targets(&self, session: u64, table: &str, key_sql: &str) -> Result<(), CmsError> {
        let rows = match self.run_with_tx(session, key_sql) {
            Ok(r) => r.rows,
            Err(_) => return Ok(()),
        };
        let Some(pk) = self.table_meta(table).and_then(|m| m.primary_key.first().cloned())
        else {
            return Ok(());
        };
        let timeout = self.lock_timeout_of(session, false);
        let in_tx = self.tx_active(session);
        for row in &rows {
            let Some(v) = row.get(&pk) else { continue };
            let Some(key) = value_key(v) else { continue };
            let res = if in_tx {
                self.row_locks.acquire(table, &key, session, timeout).map(|_| ())
            } else {
                self.row_locks
                    .wait_until_free(table, &key, session, timeout)
            };
            res.map_err(|w| row_lock_error(w, false))?;
        }
        Ok(())
    }

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
        // SET [SESSION|@@session.] innodb_lock_wait_timeout = <seconds>
        if body_up.contains("INNODB_LOCK_WAIT_TIMEOUT") {
            let val = after_eq(body)
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            match val.parse::<f64>() {
                Ok(v) if v.is_finite() && (0.0..=3600.0).contains(&v) => {
                    self.with_session(session, |s| s.lock_wait_timeout = v);
                    return Ok(empty_ok(format!("innodb_lock_wait_timeout = {}", v)));
                }
                _ => {
                    return Err(CmsError {
                        errno: errno::ER_WRONG_VALUE_FOR_VAR,
                        message: format!("Incorrect argument type to variable 'innodb_lock_wait_timeout': {}", val),
                    });
                }
            }
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
            // The pattern is a MySQL string literal: decode escapes first
            // ('wp\\_%' → wp\_% — one backslash, as LIKE expects).
            let like = find_like_pattern(sql).map(|p| decode_mysql_string(&p));
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
                    "Index_comment".into(), "Ignored".into(),
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
            r.insert("Index_comment".into(), Value::Text(String::new()));
            // MariaDB form: the last SHOW INDEX column is Ignored (MySQL 8
            // uses Is_visible — we follow the MariaDB shape like elsewhere).
            r.insert("Ignored".into(), Value::Text("NO".into()));
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

/// Finds `needle` (a keyword) as a *top-level* token in `hay`: outside
/// string literals, backtick identifiers, parentheses and SQL comments.
/// Locking-tail and table extraction must not trip over
/// `WHERE note = 'send for update'` or subqueries. Byte-index result.
fn top_level_find_ci(hay: &str, needle: &str) -> Option<usize> {
    let b = hay.as_bytes();
    let n = needle.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let (mut in_s, mut in_d, mut in_b) = (false, false, false);
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if in_s {
            match c {
                b'\\' => { i += 2; continue; }
                b'\'' => in_s = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_d {
            match c {
                b'\\' => { i += 2; continue; }
                b'"' => in_d = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_b {
            if c == b'`' {
                in_b = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_s = true,
            b'"' => in_d = true,
            b'`' => in_b = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' { i += 1; }
                continue;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
                i += 2;
                continue;
            }
            _ if depth == 0
                && is_word(c)
                && (i == 0 || !is_word(b[i - 1]))
                && i + n.len() <= b.len()
                && b[i..i + n.len()].eq_ignore_ascii_case(n)
                && (i + n.len() == b.len() || !is_word(b[i + n.len()])) =>
            {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Detects a MySQL row-locking tail: `FOR UPDATE|SHARE [OF …] [NOWAIT |
/// SKIP LOCKED]`. Returns `(byte pos of FOR, nowait, skip_locked)`.
fn find_locking_tail(sql: &str) -> Option<(usize, bool, bool)> {
    let mut search_from = 0usize;
    while let Some(p) = top_level_find_ci(&sql[search_from..], "FOR") {
        let abs = search_from + p;
        let rest = sql[abs + 3..].trim_start().to_ascii_uppercase();
        if rest.starts_with("UPDATE") || rest.starts_with("SHARE") {
            let nowait = rest.contains("NOWAIT");
            let skip_locked = rest.contains("SKIP LOCKED");
            return Some((abs, nowait, skip_locked));
        }
        search_from = abs + 3;
    }
    None
}

/// First identifier of a FROM/WHERE-head clause: `wp_options AS o`,
/// `` `wp_terms` t ``, `d.wp_options`. `None` for derived tables (`(…)`).
fn first_table_ident(clause: &str) -> Option<String> {
    let clause = clause.trim_start();
    if clause.starts_with('(') {
        return None; // subquery / derived table — no single row identity
    }
    let b: Vec<char> = clause.chars().collect();
    let mut i = 0usize;
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    if b[i] == '`' {
        i += 1;
        let start = i;
        while i < b.len() && b[i] != '`' {
            i += 1;
        }
        return Some(b[start..i].iter().collect());
    }
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_' || b[i] == '.') {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some(b[start..i].iter().collect())
}

/// Canonical string form of a row key (the PK value as stored).
fn value_key(v: &Value) -> Option<String> {
    match v {
        Value::Int(i) => Some(i.to_string()),
        Value::Float(x) => Some(x.to_string()),
        Value::Text(s) | Value::Dna(s) => Some(s.clone()),
        Value::Null => None,
    }
}

/// Maps a lock-wait outcome to the MySQL server error CMS code branches on:
/// deadlock → 1213 (SQLSTATE 40001), NOWAIT → 3572, wait expiry → 1205.
fn row_lock_error(wait: RowLockWait, nowait: bool) -> CmsError {
    match wait {
        RowLockWait::Deadlock { chain } => CmsError {
            errno: errno::ER_LOCK_DEADLOCK,
            message: format!(
                "Deadlock found when trying to get lock; try restarting transaction (session chain {:?})",
                chain
            ),
        },
        RowLockWait::Timeout { holder } if nowait => CmsError {
            errno: errno::ER_LOCK_NOWAIT,
            message: format!(
                "Statement aborted because lock(s) could not be acquired immediately and NOWAIT is set (held by session {})",
                holder
            ),
        },
        RowLockWait::Timeout { holder } => CmsError {
            errno: errno::ER_LOCK_WAIT_TIMEOUT,
            message: format!(
                "Lock wait timeout exceeded; try restarting transaction (row held by session {})",
                holder
            ),
        },
    }
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
/// The matcher lives in `expr` so the WHERE evaluator shares it.
pub fn like_match(pattern: &str, text: &str) -> bool {
    crate::expr::like_match(pattern, text)
}

/// MySQL-dialect preprocessing applied to every statement before dispatch:
/// 1) backtick-quoted identifiers are unquoted (the engine tokenizer has no
///    backtick token; CMS identifiers never need escaping);
/// 2) `DESCRIBE tbl` / `DESC tbl` → `SHOW FULL COLUMNS FROM tbl` (dbDelta
///    actually sends DESCRIBE);
/// 3) `NOW()` inside INSERT/UPDATE → the current timestamp as a literal
///    (engine INSERT values are literals; Woo order timestamps use NOW()).
fn preprocess_mysql_dialect(sql: &str) -> String {
    let stripped = strip_backticks(sql);
    let upper = stripped.trim_start().to_ascii_uppercase();
    let mut out = if upper.starts_with("DESCRIBE ") || upper.starts_with("DESC ") {
        let off = if upper.starts_with("DESCRIBE ") { 9 } else { 5 };
        let t = stripped.trim_start()[off..]
            .trim()
            .trim_end_matches(';')
            .trim()
            .trim_matches('`')
            .to_string();
        format!("SHOW FULL COLUMNS FROM {}", t)
    } else {
        stripped
    };
    let upper = out.to_ascii_uppercase();
    if upper.starts_with("INSERT") || upper.starts_with("UPDATE") {
        // The literal must be quoted — the engine values are SQL literals.
        out = replace_fn_ci_outside_strings(&out, "NOW()", &format!("'{}'", now_mysql()));
    }
    out
}

/// Removes MySQL backtick quoting from identifiers. String literals are
/// respected (single/double quoted, backslash escapes), doubled backticks
/// inside an identifier decode to a literal backtick.
fn strip_backticks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_string: Option<char> = None;
    let mut in_backtick = false;
    while let Some(c) = chars.next() {
        match in_string {
            Some(q) => {
                out.push(c);
                if c == '\\' {
                    if let Some(&n) = chars.peek() {
                        out.push(n);
                        chars.next();
                    }
                } else if c == q {
                    if chars.peek() == Some(&q) {
                        out.push(q);
                        chars.next();
                    } else {
                        in_string = None;
                    }
                }
            }
            None => {
                if in_backtick {
                    if c == '`' {
                        if chars.peek() == Some(&'`') {
                            out.push('`');
                            chars.next();
                        } else {
                            in_backtick = false;
                        }
                    } else {
                        out.push(c);
                    }
                } else if c == '`' {
                    in_backtick = true;
                } else {
                    if c == '\'' || c == '"' {
                        in_string = Some(c);
                    }
                    out.push(c);
                }
            }
        }
    }
    out
}

/// CMS edition: strips a leading `SELECT DISTINCT` (also `DISTINCTROW`),
/// returns true when the flag was present. The dedupe happens after
/// execution (see `execute`) because the engine has no DISTINCT projection.
fn strip_select_distinct(sql: &mut String) -> bool {
    let trimmed = sql.trim_start();
    let upper_head = trimmed.to_ascii_uppercase();
    for kw in ["SELECT DISTINCTROW", "SELECT DISTINCT"] {
        if upper_head.starts_with(kw) {
            let rest = &trimmed[kw.len()..];
            // Guard against a column actually named DISTINCT…: require the
            // next character to start a new token (space) or nothing.
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                *sql = format!("SELECT{}", rest);
                return true;
            }
        }
    }
    false
}

/// CMS edition: rewrites MySQL comma-FROM lists into the engine's join
/// grammar. `FROM a AS x, b AS y WHERE …` becomes
/// `FROM a AS x INNER JOIN b AS y ON 1=1 WHERE …` — MySQL evaluates the
/// comma form as a cross join filtered by WHERE; the engine does the same
/// with `ON 1=1` (see `SParser` / `cross_join`). Only the first (top-level)
/// FROM of a SELECT is rewritten; commas inside parens/strings are ignored.
fn rewrite_comma_from(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    let mut in_backtick = false;
    let mut depth = 0i32;
    // 1) find the top-level FROM keyword (word boundary, outside literals).
    let mut from_pos: Option<usize> = None;
    while i < chars.len() {
        let c = chars[i];
        match in_string {
            Some(q) => {
                if c == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if c == q {
                    in_string = None;
                }
            }
            None => {
                if in_backtick {
                    if c == '`' {
                        in_backtick = false;
                    }
                } else if c == '`' {
                    in_backtick = true;
                } else if c == '\'' || c == '"' {
                    in_string = Some(c);
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                } else if depth == 0
                    && c.to_ascii_uppercase() == 'F'
                    && is_word_at(&chars, i, "FROM")
                {
                    from_pos = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    let from_start = match from_pos {
        Some(p) => p,
        None => return sql.to_string(),
    };
    // 2) scan the table-ref list; stop at the first top-level keyword that
    //    terminates FROM (or end of statement). Track top-level commas.
    let stops: [&str; 13] = [
        "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "UNION", "JOIN",
        "INNER", "LEFT", "RIGHT", "FULL", "CROSS",
    ];
    let mut j = from_start + 4; // past "FROM"
    let mut depth2 = 0i32;
    let mut in_string2: Option<char> = None;
    let mut in_backtick2 = false;
    let mut commas: Vec<usize> = Vec::new();
    let mut end = chars.len();
    while j < chars.len() {
        let c = chars[j];
        match in_string2 {
            Some(q) => {
                if c == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if c == q {
                    in_string2 = None;
                }
            }
            None => {
                if in_backtick2 {
                    if c == '`' {
                        in_backtick2 = false;
                    }
                } else if c == '`' {
                    in_backtick2 = true;
                } else if c == '\'' || c == '"' {
                    in_string2 = Some(c);
                } else if c == '(' {
                    depth2 += 1;
                } else if c == ')' {
                    if depth2 == 0 {
                        end = j;
                        break;
                    }
                    depth2 -= 1;
                } else if depth2 == 0 {
                    if c == ',' {
                        commas.push(j);
                    } else if c.is_ascii_alphabetic() || c == '_' {
                        if let Some(w) = word_at(&chars, j) {
                            if stops.iter().any(|s| s.eq_ignore_ascii_case(&w)) {
                                end = j;
                                break;
                            }
                            // skip the rest of this word
                            j += w.len() - 1;
                        }
                    } else if c == ';' {
                        end = j;
                        break;
                    }
                }
            }
        }
        j += 1;
    }
    if commas.is_empty() {
        return sql.to_string();
    }
    // 3) rebuild: split on top-level commas inside [from+4, end) and rejoin
    //    every ref after the first with INNER JOIN … ON 1=1. The original
    //    spacing is preserved: refs keep their raw text (only trimmed for
    //    the emptiness check), and explicit separators are inserted where
    //    parts were joined back together.
    let mut refs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for k in from_start + 4..end {
        let c = chars[k];
        if commas.contains(&k) {
            refs.push(cur.clone());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    refs.push(cur);
    if refs.iter().any(|r| r.trim().is_empty()) {
        return sql.to_string();
    }
    let mut rebuilt = String::with_capacity(sql.len() + 24 * refs.len());
    rebuilt.push_str(&chars[..from_start + 4].iter().collect::<String>());
    let first = &refs[0];
    if !first.starts_with(char::is_whitespace) {
        rebuilt.push(' ');
    }
    rebuilt.push_str(first.trim_end());
    for r in &refs[1..] {
        rebuilt.push_str(" INNER JOIN ");
        rebuilt.push_str(r.trim());
        rebuilt.push_str(" ON 1=1");
    }
    let tail: String = chars[end..].iter().collect();
    if !tail.is_empty()
        && !tail.starts_with(char::is_whitespace)
        && !tail.starts_with(';')
        && !tail.starts_with(')')
    {
        rebuilt.push(' ');
    }
    rebuilt.push_str(&tail);
    rebuilt
}

/// The word at `pos` equals `word` (case-insensitive) and has a non-word
/// character (or boundary) on both sides.
fn is_word_at(chars: &[char], pos: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if pos > 0 {
        let prev = chars[pos - 1];
        if prev.is_ascii_alphanumeric() || prev == '_' {
            return false;
        }
    }
    if pos + w.len() > chars.len() {
        return false;
    }
    for (k, wc) in w.iter().enumerate() {
        if chars[pos + k].to_ascii_uppercase() != *wc {
            return false;
        }
    }
    let after = pos + w.len();
    if after < chars.len() {
        let next = chars[after];
        if next.is_ascii_alphanumeric() || next == '_' {
            return false;
        }
    }
    true
}

/// The alphanumeric word starting exactly at `pos`, or None.
fn word_at(chars: &[char], pos: usize) -> Option<String> {
    let mut out = String::new();
    let mut i = pos;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        out.push(chars[i]);
        i += 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Case-insensitive replacement of a function call token (`NOW()`) outside
/// string literals; string contents are never touched.
fn replace_fn_ci_outside_strings(s: &str, needle: &str, replacement: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let nchars: Vec<char> = needle.chars().collect();
    let mut out = String::with_capacity(s.len() + replacement.len());
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                if i + 1 < chars.len() && chars[i + 1] == q {
                    out.push(q);
                    i += 2;
                    continue;
                }
                in_string = None;
            }
            i += 1;
            continue;
        }
        let mut matched = c.to_ascii_lowercase() == nchars[0].to_ascii_lowercase();
        if matched {
            for (k, nc) in nchars.iter().enumerate() {
                if i + k >= chars.len()
                    || chars[i + k].to_ascii_lowercase() != nc.to_ascii_lowercase()
                {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            out.push_str(replacement);
            i += nchars.len();
            continue;
        }
        if c == '\'' || c == '"' {
            in_string = Some(c);
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Current server time in the MySQL literal format `YYYY-MM-DD HH:MM:SS`
/// (UTC; the CMS edition does not model per-server time zones yet).
fn now_mysql() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil_from_days: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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

/// "N row(s) inserted/updated/deleted" → N; also the engine's DELETE form
/// "N deleted, M cascaded".
fn extract_row_count(message: &str) -> u64 {
    let m = message.to_ascii_lowercase();
    for marker in [" row(s)", " deleted"] {
        if let Some(pos) = m.find(marker) {
            let start = m[..pos]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            if let Ok(n) = start.parse::<u64>() {
                return n;
            }
        }
    }
    0
}

/// Decodes a MySQL string literal body (the text between the quotes):
/// backslash escapes per the MySQL manual, `\%` and `\_` stay two chars.
fn decode_mysql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            match e {
                '0' => out.push('\0'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{0008}'),
                'Z' => out.push('\u{001a}'),
                '\'' | '"' | '\\' => out.push(e),
                '%' | '_' => {
                    out.push('\\');
                    out.push(e);
                }
                other => out.push(other),
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
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
    fn distinct_and_comma_from_and_numeric_coercion() {
        let g = gate("wp7gaps");
        let s = g.open_session();
        g.bootstrap_wordpress().unwrap();

        // Seed: two terms referenced by the same object (distinct pairs).
        g.execute(s, "INSERT INTO wp_terms (name, slug, term_group) VALUES ('T1','t1',0), ('T2','t2',0)").unwrap();

        // 1) SELECT DISTINCT — engine has no DISTINCT projection; the gate
        //    strips it and dedupes rows (WP update_object_term_cache shape).
        let r = g
            .execute(s, "SELECT DISTINCT term_group FROM wp_terms ORDER BY term_group")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "distinct single column collapses to 1 row");

        // 2) Comma-FROM list — rewritten to INNER JOIN … ON 1=1 (cross join)
        //    with the join predicate left in WHERE, MySQL style.
        g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('numcheck', '7', 'yes')").unwrap();
        let r = g
            .execute(
                s,
                "SELECT COUNT(*) AS c FROM wp_terms, wp_options WHERE wp_terms.term_id = 1 AND wp_options.option_name = 'numcheck'",
            )
            .unwrap();
        assert_eq!(v(&r.rows[0], "c"), "1", "comma-FROM cross join + WHERE filter");

        // 3) Numeric coercion: MySQL compares numbers and numeric strings
        //    numerically (`WHERE ID = '2'` — WP_User::get_data_by uses %s).
        let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_id = '1'")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "string '1' matches numeric option_id 1");

        // 4) …but non-numeric text does NOT silently match numbers (we do
        //    not replicate MySQL's "everything casts to 0" footgun).
        let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_id = 'abc'")
            .unwrap();
        assert_eq!(r.rows.len(), 0, "non-numeric text stays non-equal");

        // 5) NOT IN — WP core emits it everywhere (`comment_type NOT IN
        //    ('note')` breaks comments queries otherwise).
        g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('other1', 'x', 'yes'), ('other2', 'y', 'yes')").unwrap();
        let r = g
            .execute(
                s,
                "SELECT option_name FROM wp_options WHERE option_name NOT IN ('numcheck') AND autoload = 'yes' ORDER BY option_name",
            )
            .unwrap();
        let names: Vec<String> = r.rows.iter().map(|r| v(r, "option_name")).collect();
        assert!(!names.contains(&"numcheck".to_string()));
        assert_eq!(names, vec!["other1", "other2"], "NOT IN keeps the other rows");
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
        // filled with the effective DEFAULT (NOT NULL zero-date), exactly
        // what MySQL does for an omitted column on INSERT (§4.11 done).
        let r = g.execute(s, "SELECT post_date_gmt FROM wp_posts WHERE ID = 1").unwrap();
        assert_eq!(
            r.rows[0].get("post_date_gmt").map(|v| v.to_string()).as_deref(),
            Some("0000-00-00 00:00:00")
        );
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

        // CREATE TABLE through the gate registers the MySQL-dialect overlay
        // automatically, so SHOW FULL COLUMNS returns the EXACT original
        // types (dbDelta parity for plugin tables). Columns live in the
        // DDL's declared order; look rows up by Field regardless.
        let r = g.execute(s, "SHOW FULL COLUMNS FROM wp_myplugin_data").unwrap();
        assert_eq!(r.rows.len(), 5);
        let row = |name: &str| {
            r.rows
                .iter()
                .find(|row| v(row, "Field") == name)
                .unwrap_or_else(|| panic!("column {} not shown", name))
        };
        assert_eq!(v(row("id"), "Type"), "bigint(20) unsigned");
        assert_eq!(v(row("id"), "Key"), "PRI");
        assert_eq!(v(row("id"), "Extra"), "auto_increment");
        assert_eq!(v(row("value"), "Type"), "decimal(10,2)");
        assert_eq!(v(row("created"), "Type"), "datetime");
        assert_eq!(v(row("note"), "Default"), "");
    }

    #[test]
    fn hpos_string_insert_into_bigint_indexed_column() {
        // WooCommerce HPOS regression (E2E 2026-11-02): WC_Order_Item_Data_Store
        // uses wpdb::insert() WITHOUT a format array — every value travels as a
        // string, so `order_id` lands in `bigint unsigned` as '15'. MariaDB
        // coerces on write; we must too, else the secondary-index key (encoded
        // from Text) never matches `WHERE order_id = 15` (IndexScan → 0 rows),
        // get_items() returns [] and stock reduction silently no-ops.
        let g = gate("hpos_types");
        let s = g.open_session();
        g.execute(s, "CREATE TABLE wp_woocommerce_order_items (
            order_item_id bigint(20) unsigned NOT NULL AUTO_INCREMENT,
            order_item_name text NOT NULL,
            order_item_type varchar(200) NOT NULL DEFAULT '',
            order_id bigint(20) unsigned NOT NULL DEFAULT '0',
            PRIMARY KEY  (order_item_id),
            KEY order_id (order_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci").unwrap();

        // HPOS shape: wpdb->insert without format → all values quoted.
        g.execute(s, "INSERT INTO wp_woocommerce_order_items (order_item_name, order_item_type, order_id) VALUES ('Тестовый товар DNA', 'line_item', '15')").unwrap();
        // Classic wc_add_order_item shape: %s, %s, %d → id unquoted.
        g.execute(s, "INSERT INTO wp_woocommerce_order_items (order_item_name, order_item_type, order_id) VALUES ('Прямая вставка', 'line_item', 15)").unwrap();

        // Int-literal lookup must see BOTH rows (index keys coerced on write).
        let r = g.execute(s, "SELECT order_item_id FROM wp_woocommerce_order_items WHERE order_id = 15 ORDER BY order_item_id").unwrap();
        assert_eq!(r.rows.len(), 2, "string- and int-written rows both match = 15");
        assert_eq!(v(&r.rows[0], "order_item_id"), "1");
        // String-literal lookup too.
        let r = g.execute(s, "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = '15'").unwrap();
        assert_eq!(v(&r.rows[0], "c"), "2");
        // The stored value became numeric (MariaDB representation).
        let r = g.execute(s, "SELECT order_item_id FROM wp_woocommerce_order_items WHERE order_item_id = 1 AND order_id + 0 = 15").unwrap();
        assert_eq!(r.rows.len(), 1);

        // UPDATE coercion: string SET into an int column stores a number.
        g.execute(s, "UPDATE wp_woocommerce_order_items SET order_id = '16' WHERE order_item_id = 1").unwrap();
        let r = g.execute(s, "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = 16").unwrap();
        assert_eq!(v(&r.rows[0], "c"), "1", "UPDATE '16' coerced to 16 (index rebuilt)");
        // Non-numeric text into an int column → 0 (non-strict, sql_mode='').
        g.execute(s, "UPDATE wp_woocommerce_order_items SET order_id = 'abc' WHERE order_item_id = 2").unwrap();
        let r = g.execute(s, "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = 0").unwrap();
        assert_eq!(v(&r.rows[0], "c"), "1");
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

    #[test]
    fn transactions_and_mysql_dialect_via_gate() {
        let g = gate("dialect_tx");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();

        // Backticked identifiers (wpdb sends them everywhere).
        g.execute(
            s,
            "INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('bt', '1', 'yes')",
        )
        .unwrap();

        // ON DUPLICATE KEY UPDATE on the UNIQUE option_name (add_option path).
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('bt', '2', 'yes') \
             ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)",
        )
        .unwrap();
        let r = g
            .execute(s, "SELECT option_value FROM wp_options WHERE option_name = 'bt'")
            .unwrap();
        assert_eq!(v(&r.rows[0], "option_value"), "2");
        assert_eq!(r.rows.len(), 1, "upsert must not duplicate the row");

        // DESCRIBE is dbDelta's actual probe.
        let r = g.execute(s, "DESCRIBE wp_options").unwrap();
        assert_eq!(r.columns.first().map(String::as_str), Some("Field"));
        assert_eq!(r.rows.len(), 4);

        // LIMIT offset, count (WP_Query pagination form).
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('p1','1','yes'),('p2','2','yes'),('p3','3','yes'),('p4','4','yes'),('p5','5','yes')",
        )
        .unwrap();
        let r = g
            .execute(
                s,
                "SELECT option_name FROM wp_options WHERE option_name != 'bt' ORDER BY option_name ASC LIMIT 2, 2",
            )
            .unwrap();
        assert_eq!(v(&r.rows[0], "option_name"), "p3");
        assert_eq!(v(&r.rows[1], "option_name"), "p4");
        assert_eq!(r.rows.len(), 2);

        // NOW() literal rewrite in UPDATE.
        g.execute(
            s,
            "UPDATE wp_options SET option_value = NOW() WHERE option_name = 'p1'",
        )
        .unwrap();
        let r = g
            .execute(s, "SELECT option_value FROM wp_options WHERE option_name = 'p1'")
            .unwrap();
        let val = v(&r.rows[0], "option_value");
        assert_eq!(val.len(), 19, "NOW() must render as 'YYYY-MM-DD HH:MM:SS', got {val}");
        assert_eq!(&val[4..5], "-");

        // Transaction: ROLLBACK discards, COMMIT persists.
        g.execute(s, "START TRANSACTION").unwrap();
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('txrow', '1', 'yes')",
        )
        .unwrap();
        g.execute(s, "ROLLBACK").unwrap();
        let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_name = 'txrow'")
            .unwrap();
        assert_eq!(r.rows.len(), 0, "ROLLBACK must discard the INSERT");

        g.execute(s, "BEGIN").unwrap();
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('txrow2', '1', 'yes')",
        )
        .unwrap();
        g.execute(s, "COMMIT").unwrap();
        let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_name = 'txrow2'")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "COMMIT must persist the INSERT");

        // Escaped quotes inside string literals (MySQL backslash escapes).
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('esc', 'don\\'t \\\\ stop', 'yes')",
        )
        .unwrap();
        let r = g
            .execute(s, "SELECT option_value FROM wp_options WHERE option_name = 'esc'")
            .unwrap();
        assert_eq!(v(&r.rows[0], "option_value"), "don't \\ stop");
    }

    #[test]
    fn wp_like_transient_cleanup_pattern() {
        // The escaped-wildcard LIKE from delete_expired_transients: backslash
        // escapes must survive tokenization so '\_%' matches a literal '_'.
        let g = gate("transient");
        g.bootstrap_wordpress().unwrap();
        let s = g.open_session();
        g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('_transient_x', '1', 'yes'), ('_transient_timeout_x', '1', 'no'), ('_transient_y', '2', 'no')",
        )
        .unwrap();
        let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_name LIKE '\\_transient\\_%' AND option_name NOT LIKE '%timeout%' ORDER BY option_name ASC")
            .unwrap();
        assert_eq!(r.rows.len(), 2, "\\_ must anchor the pattern to the literal underscore");
    }

    #[test]
    fn order_by_arithmetic_numeric_sort_no_extra_column() {
        // WordPress/WooCommerce numeric-sort idiom: `ORDER BY meta_value+0`
        // over a varchar column (differential corpus #37, run #2 errno 1064).
        // MySQL reads the leading numeric prefix: '250.00'+0 -> 250,
        // 'abc'+0 -> 0; and the sort key emits NO output column.
        let g = gate("order_arith");
        let s = g.open_session();
        g.execute(
            s,
            "CREATE TABLE t_items (id bigint(20) unsigned NOT NULL auto_increment, label varchar(50) NOT NULL default '', weight varchar(20) NOT NULL default '', PRIMARY KEY (id))",
        )
        .unwrap();
        g.execute(
            s,
            "INSERT INTO t_items (label, weight) VALUES ('a','250.00'), ('b','110.50'), ('c','abc')",
        )
        .unwrap();
        let r = g
            .execute(s, "SELECT id, label FROM t_items ORDER BY weight+0 DESC")
            .unwrap();
        assert_eq!(r.columns, vec!["id", "label"], "sort key must not add an output column");
        let got: Vec<String> = r.rows.iter().map(|row| v(row, "label")).collect();
        assert_eq!(got, vec!["a", "b", "c"], "250 > 110.5 > 0('abc')");
        // ASC with a secondary key and LIMIT (MySQL multi-key sort).
        let r = g
            .execute(
                s,
                "SELECT id FROM t_items ORDER BY weight+0 ASC, id ASC LIMIT 2",
            )
            .unwrap();
        let got: Vec<String> = r.rows.iter().map(|row| v(row, "id")).collect();
        assert_eq!(got, vec!["3", "2"], "'abc'(0) first, then 110.50");
    }

    #[test]
    fn star_projections_use_declaration_order() {
        // MySQL reports `SELECT *` / `t.*` columns in DECLARATION order
        // (differential corpus #36/#37, run #2 label/value desync).
        let g = gate("decl_order");
        let s = g.open_session();
        g.execute(
            s,
            "CREATE TABLE wp_terms (term_id bigint(20) unsigned NOT NULL auto_increment, name varchar(200) NOT NULL default '', slug varchar(200) NOT NULL default '', term_group bigint(20) NOT NULL default 0, PRIMARY KEY (term_id))",
        )
        .unwrap();
        g.execute(s, "INSERT INTO wp_terms (name, slug) VALUES ('News','news')").unwrap();
        let r = g.execute(s, "SELECT * FROM wp_terms").unwrap();
        assert_eq!(r.columns, vec!["term_id", "name", "slug", "term_group"]);
        // `t.*, tt.*`: labels follow each table's declaration order, MySQL
        // keeps the duplicated label (term_id) one per projection item.
        g.execute(
            s,
            "CREATE TABLE wp_term_taxonomy (term_taxonomy_id bigint(20) unsigned NOT NULL auto_increment, term_id bigint(20) unsigned NOT NULL default 0, taxonomy varchar(32) NOT NULL default '', PRIMARY KEY (term_taxonomy_id))",
        )
        .unwrap();
        g.execute(
            s,
            "INSERT INTO wp_term_taxonomy (term_id, taxonomy) VALUES (1,'category')",
        )
        .unwrap();
        let r = g
            .execute(
                s,
                "SELECT t.*, tt.* FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id",
            )
            .unwrap();
        assert_eq!(
            r.columns,
            vec![
                "term_id", "name", "slug", "term_group", "term_taxonomy_id", "term_id", "taxonomy"
            ]
        );
        assert_eq!(v(&r.rows[0], "name"), "News");
        assert_eq!(v(&r.rows[0], "taxonomy"), "category");
        // ALTER TABLE ADD appends to the declaration order (MySQL).
        g.execute(
            s,
            "ALTER TABLE wp_terms ADD COLUMN new_col varchar(64) NOT NULL default ''",
        )
        .unwrap();
        let r = g.execute(s, "SELECT * FROM wp_terms").unwrap();
        assert_eq!(
            r.columns,
            vec!["term_id", "name", "slug", "term_group", "new_col"]
        );
    }

    // ---------------- row locks: SELECT … FOR UPDATE (§4 item 2) ----------------

    /// Small stock table for lock scenarios (Woo `_stock` meta shape).
    fn stock_gate(name: &str) -> CmsSqlGate {
        let g = gate(name);
        g.execute(
            g.open_session(),
            "CREATE TABLE wp_stock_meta (meta_id bigint(20) unsigned NOT NULL auto_increment, post_id bigint(20) unsigned NOT NULL default 0, meta_key varchar(255) NOT NULL default '', meta_value longtext, PRIMARY KEY (meta_id))",
        )
        .unwrap();
        g
    }

    #[test]
    fn for_update_blocks_second_session_until_commit() {
        let g = stock_gate("fu_block");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (7, '_stock', '10')").unwrap();

        // s1: read-and-lock inside a transaction.
        g.execute(s1, "BEGIN").unwrap();
        let r = g
            .execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "10");
        assert_eq!(g.row_locks.held_len(), 1, "one row lock held inside tx");

        // s2: the same FOR UPDATE waits → 1205 with a tiny session timeout.
        g.execute(s2, "SET innodb_lock_wait_timeout = 0.15").unwrap();
        g.execute(s2, "BEGIN").unwrap();
        let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "1205 expected, got {:?}", err);
        // s2's tx survived the failed statement (statement-level rollback).
        assert!(g.tx_active(s2));

        // NOWAIT on a held row → 3572 immediately.
        g.execute(s1, "SET innodb_lock_wait_timeout = 5").unwrap();
        let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE NOWAIT")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_NOWAIT, "3572 expected, got {:?}", err);

        // COMMIT releases; the waiter then succeeds (its tx still open).
        g.execute(s1, "COMMIT").unwrap();
        assert_eq!(g.row_locks.held_len(), 0, "commit releases row locks");
        let r = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "10");
        g.execute(s2, "ROLLBACK").unwrap();
        assert_eq!(g.row_locks.held_len(), 0);
    }

    #[test]
    fn for_update_reentrant_same_session_and_row() {
        let g = stock_gate("fu_reentrant");
        let s = g.open_session();
        g.execute(s, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (1, '_stock', '5')").unwrap();
        g.execute(s, "BEGIN").unwrap();
        g.execute(s, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 FOR UPDATE").unwrap();
        // Same transaction locks the row again — no self-deadlock.
        let r = g.execute(s, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 AND meta_key = '_stock' FOR UPDATE").unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "5");
        g.execute(s, "COMMIT").unwrap();
        assert_eq!(g.row_locks.held_len(), 0);
    }

    #[test]
    fn opposite_order_updates_deadlock_1213() {
        // InnoDB pattern: T1 locks A then wants B; T2 locks B then wants A →
        // one of them gets 1213 and WooCommerce retries.
        let g = stock_gate("fu_deadlock");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (100, '_stock', '1')").unwrap();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (200, '_stock', '2')").unwrap();

        g.execute(s1, "SET innodb_lock_wait_timeout = 3").unwrap();
        g.execute(s2, "SET innodb_lock_wait_timeout = 3").unwrap();

        // T1 holds row A (post_id=100); T2 holds row B (post_id=200).
        g.execute(s1, "BEGIN").unwrap();
        g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE").unwrap();
        g.execute(s2, "BEGIN").unwrap();
        g.execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE").unwrap();

        // T1 starts waiting for B on a background thread…
        let g2 = std::sync::Arc::new(g);
        let g3 = g2.clone();
        let t1 = std::thread::spawn(move || {
            g3.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE")
        });
        std::thread::sleep(std::time::Duration::from_millis(150));

        // …then T2 asks for A: cycle (s2→B→s1→A→s2) → 1213 immediately.
        let err = g2
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_DEADLOCK, "1213 expected, got {:?}", err);
        assert_eq!(err.sqlstate(), "40001");

        // Victim "rolled back its statement" (tx survives per MySQL); after
        // T2 rolls back fully, T1's wait completes.
        g2.execute(s2, "ROLLBACK").unwrap();
        let r = t1.join().unwrap().expect("T1 must acquire row B after T2 died");
        assert_eq!(v(&r.rows[0], "meta_value"), "2");
        g2.execute(s1, "COMMIT").unwrap();
    }

    #[test]
    fn autocommit_update_waits_for_held_row() {
        // Woo path: order A reduced stock in a tx (FOR UPDATE); order B's
        // autocommit UPDATE of the same row must wait, not stomp.
        let g = stock_gate("fu_autocommit");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (77, '_stock', '10')").unwrap();

        g.execute(s1, "BEGIN").unwrap();
        g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77 AND meta_key = '_stock' FOR UPDATE").unwrap();

        // Autocommit UPDATE blocked by the open tx → 1205 (short timeout).
        g.execute(s2, "SET innodb_lock_wait_timeout = 0.15").unwrap();
        let err = g
            .execute(s2, "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "1205 expected, got {:?}", err);
        // The row still holds the old value.
        let r = g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77").unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "10");

        // Commit, then the UPDATE goes through.
        g.execute(s1, "COMMIT").unwrap();
        let r = g.execute(s2, "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'").unwrap();
        assert_eq!(r.affected_rows, 1);
        let r = g.execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77").unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "8");
    }

    #[test]
    fn update_in_tx_takes_hold_until_commit() {
        let g = stock_gate("fu_upd_tx");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (9, '_stock', '4')").unwrap();

        g.execute(s1, "BEGIN").unwrap();
        g.execute(s1, "UPDATE wp_stock_meta SET meta_value = '3' WHERE post_id = 9 AND meta_key = '_stock'").unwrap();
        assert!(g.row_locks.held_len() >= 1, "UPDATE in tx holds an X lock");

        g.execute(s2, "SET innodb_lock_wait_timeout = 0.15").unwrap();
        g.execute(s2, "BEGIN").unwrap();
        let err = g
            .execute(s2, "UPDATE wp_stock_meta SET meta_value = '2' WHERE post_id = 9 AND meta_key = '_stock'")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "1205 expected, got {:?}", err);
        g.execute(s2, "ROLLBACK").unwrap();

        g.execute(s1, "COMMIT").unwrap();
        assert_eq!(g.row_locks.held_len(), 0);
        let r = g.execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 9").unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "3", "first tx's write committed");
    }

    #[test]
    fn delete_blocked_by_held_row_and_locking_tail_skips_literals() {
        let g = stock_gate("fu_delete");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (5, '_stock', '1')").unwrap();

        g.execute(s1, "BEGIN").unwrap();
        g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 5 FOR UPDATE").unwrap();

        // A literal 'for update' inside a string must NOT take the lock path.
        let r = g.execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE meta_value = 'wait for update'").unwrap();
        assert_eq!(r.rows.len(), 0, "literal containing 'for update' is not a locking tail");

        g.execute(s2, "SET innodb_lock_wait_timeout = 0.15").unwrap();
        let err = g.execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5").unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "DELETE waits too");

        g.execute(s1, "COMMIT").unwrap();
        let r = g.execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5").unwrap();
        assert_eq!(r.affected_rows, 1);
    }

    #[test]
    fn close_session_releases_row_locks() {
        let g = stock_gate("fu_close");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (3, '_stock', '9')").unwrap();
        g.execute(s1, "BEGIN").unwrap();
        g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3 FOR UPDATE").unwrap();
        assert_eq!(g.row_locks.held_len(), 1);
        // PHP-FPM worker dies mid-transaction → disconnect rolls the tx back.
        g.close_session(s1);
        assert_eq!(g.row_locks.held_len(), 0, "disconnect releases row locks");
        let r = g.execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3").unwrap();
        assert_eq!(v(&r.rows[0], "meta_value"), "9");
    }

    #[test]
    fn skip_locked_claims_only_free_rows() {
        // Action Scheduler: two queue workers claim pending jobs with
        // FOR UPDATE SKIP LOCKED — each must get a DIFFERENT row.
        let g = stock_gate("fu_skip");
        let s1 = g.open_session();
        let s2 = g.open_session();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (11, '_job', 'a')").unwrap();
        g.execute(s1, "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (12, '_job', 'b')").unwrap();

        // Worker 1 claims job 11 (projection carries the PK → exact rows).
        g.execute(s1, "BEGIN").unwrap();
        let r = g
            .execute(s1, "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id = 11 FOR UPDATE")
            .unwrap();
        assert_eq!(v(&r.rows[0], "post_id"), "11");

        // Worker 2 asks for both jobs, skipping held ones → only 12 left.
        g.execute(s2, "BEGIN").unwrap();
        let r = g
            .execute(s2, "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE SKIP LOCKED")
            .unwrap();
        assert_eq!(r.rows.len(), 1, "held job 11 excluded by SKIP LOCKED");
        assert_eq!(v(&r.rows[0], "post_id"), "12");

        // Plain FOR UPDATE on the same set still blocks (0.15s → 1205).
        let s3 = g.open_session();
        g.execute(s3, "SET innodb_lock_wait_timeout = 0.15").unwrap();
        let err = g
            .execute(s3, "SELECT meta_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE")
            .unwrap_err();
        assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT);

        g.execute(s1, "COMMIT").unwrap();
        g.execute(s2, "ROLLBACK").unwrap();
        assert_eq!(g.row_locks.held_len(), 0);
    }
}

