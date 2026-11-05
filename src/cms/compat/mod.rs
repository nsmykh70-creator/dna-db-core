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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::cms::ddl_meta::CmsTableMeta;
use crate::cms::errno;
use crate::cms::locks::LockRegistry;
use crate::cms::row_locks::RowLockRegistry;
use crate::cms::session::CmsSession;
use crate::relational::Value;
use crate::sql_engine::SqlEngine;

pub(crate) mod decimal;

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
    /// Per output column: `Some(scale)` when the column resolves to a
    /// decimal(p,s)-declared table column (values are pre-formatted with
    /// that scale) — the wire layer reports MYSQL_TYPE_NEWDECIMAL with
    /// `decimals=scale`. Empty for non-SELECT replies.
    pub decimal_scales: Vec<Option<u8>>,
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
    /// Databases created through the gate (`CREATE DATABASE`). The engine
    /// itself is single-catalog; the gate pretends catalogs exist so
    /// generic MySQL clients (phpMyAdmin, mysqldump --databases, CMS
    /// installers that pre-create the DB) keep working.
    databases: Mutex<BTreeSet<String>>,
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
            databases: Mutex::new(BTreeSet::new()),
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
        self.sessions
            .lock()
            .expect("session map poisoned")
            .remove(&id);
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
        self.meta
            .read()
            .expect("meta map poisoned")
            .get(table)
            .cloned()
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
            return Err(CmsError {
                errno: errno::ER_PARSE_ERROR,
                message: "empty query".into(),
            });
        }
        let upper = sql.to_ascii_uppercase();

        // ---------- session-level statements ----------
        if upper.starts_with("SET ") {
            return self.exec_set(session, sql, &upper);
        }
        if upper.starts_with("USE ") {
            let db = sql[4..]
                .trim()
                .trim_matches('`')
                .trim_matches('\'')
                .to_string();
            self.with_session(session, |s| s.database = Some(db.clone()));
            return Ok(empty_ok(format!("database changed to '{}'", db)));
        }

        // CREATE DATABASE [IF NOT EXISTS] name — the engine is
        // single-catalog, so this records the name in the gate's catalog
        // set (SHOW DATABASES lists it) and switches the session over,
        // mirroring MySQL's implicit USE. CMS installers (Drupal's
        // "Create new database" path, phpMyAdmin) send this first.
        if upper.starts_with("CREATE DATABASE") {
            let tail = sql["CREATE DATABASE".len()..].trim();
            let tail = tail
                .strip_prefix("IF NOT EXISTS")
                .unwrap_or(tail)
                .trim();
            let db = tail
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if db.is_empty() {
                return Err(CmsError {
                    errno: errno::ER_PARSE_ERROR,
                    message: "CREATE DATABASE: missing name".into(),
                });
            }
            self.databases
                .lock()
                .expect("database set poisoned")
                .insert(db.clone());
            self.with_session(session, |s| s.database = Some(db.clone()));
            return Ok(empty_ok(format!("database created: '{}'", db)));
        }

        // DROP DATABASE [IF EXISTS] name — removes the gate's record; the
        // engine's single catalog is untouched (tables live under it).
        if upper.starts_with("DROP DATABASE") {
            let tail = sql["DROP DATABASE".len()..].trim();
            let tail = tail.strip_prefix("IF EXISTS").unwrap_or(tail).trim();
            let db = tail
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            self.databases
                .lock()
                .expect("database set poisoned")
                .remove(&db);
            return Ok(empty_ok(format!("database dropped: '{}'", db)));
        }

        // ALTER DATABASE [name] CHARACTER SET / COLLATE / … — per-database
        // charset metadata the engine does not model. Joomla's installer
        // sends this right after CREATE DATABASE; real MySQL answers OK.
        if upper.starts_with("ALTER DATABASE") {
            return Ok(empty_ok("database altered".to_string()));
        }

        // MySQL: COMMIT / ROLLBACK without an open transaction are silent
        // no-ops. PHP drivers that run with autocommit=0 (pymysql, mysqli
        // defaults, wpdb in transactional plugins) send bare COMMITs after
        // every write, so rejecting them here breaks real clients with
        // errno 1064 where real MySQL answers OK.
        if upper == "COMMIT" || upper == "ROLLBACK" {
            self.with_session(session, |s| s.savepoints.clear());
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

        // ---------- SAVEPOINT markers (Drupal nested transactions) ----------
        // `SAVEPOINT name` / `RELEASE SAVEPOINT name` / `ROLLBACK TO name`.
        // See CmsSession::savepoints for the semantics.
        {
            let head = upper.split_whitespace().next().unwrap_or("");
            match head {
                "SAVEPOINT" => {
                    let name = sql["SAVEPOINT".len()..].trim().to_string();
                    self.with_session(session, |s| s.savepoints.push(name));
                    return Ok(empty_ok("savepoint set"));
                }
                "RELEASE" => {
                    let rest = sql["RELEASE".len()..].trim();
                    let name = rest
                        .strip_prefix("SAVEPOINT")
                        .unwrap_or(rest)
                        .trim()
                        .trim_matches('`')
                        .to_string();
                    self.with_session(session, |s| {
                        if let Some(pos) = s.savepoints.iter().position(|x| *x == name) {
                            s.savepoints.truncate(pos);
                        }
                    });
                    return Ok(empty_ok(format!("savepoint {} released", name)));
                }
                "ROLLBACK" => {
                    let rest = sql["ROLLBACK".len()..].trim_start();
                    if rest
                        .to_ascii_uppercase()
                        .strip_prefix("TO")
                        .map(|r| r.trim_start())
                        .map(|r| r.strip_prefix("SAVEPOINT").unwrap_or(r).trim())
                        .map(|name| !name.is_empty())
                        .unwrap_or(false)
                    {
                        let raw = rest
                            .trim_start()
                            .strip_prefix("TO")
                            .unwrap()
                            .trim_start()
                            .strip_prefix("SAVEPOINT")
                            .unwrap_or(rest.trim_start().strip_prefix("TO").unwrap().trim_start())
                            .trim()
                            .trim_matches('`')
                            .to_string();
                        self.with_session(session, |s| {
                            if let Some(pos) = s.savepoints.iter().position(|x| *x == raw) {
                                s.savepoints.truncate(pos + 1);
                            }
                        });
                        return Ok(empty_ok(format!("rolled back to savepoint {}", raw)));
                    }
                }
                _ => {}
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

        // ---------- information_schema catalog probes ----------
        // MediaWiki's installer (`SELECT 1 FROM information_schema.schemata
        // WHERE schema_name = …`) and Drupal's schema introspection
        // (`information_schema.tables` / `.columns` / `.statistics`) read the
        // ANSI catalog instead of SHOW statements. Answered from the CMS
        // metadata overlay; never forwarded to the SQL engine.
        if upper.starts_with("SELECT") {
            if let Some(res) = self.try_information_schema(session, sql)? {
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
        // `LOCK IN SHARE MODE` (MediaWiki User::addToDatabase) is a
        // gate-recognised locking tail but not part of the engine grammar —
        // strip it before forwarding; the shared row locks below still apply.
        if upper.starts_with("SELECT") {
            if let Some(p) = top_level_find_ci(&forwarded, "LOCK IN SHARE MODE") {
                forwarded.truncate(p);
                forwarded = forwarded.trim_end().to_string();
            }
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
        // MySQL: issuing BEGIN / START TRANSACTION while a transaction is
        // active performs an implicit COMMIT of the current one, then starts
        // a fresh transaction. Drupal 11's schema installer does exactly
        // that (nested startTransaction during DDL); rejecting with
        // "transaction already active" aborts the whole install. The CMS
        // read-your-writes model already flushed the buffer after every
        // statement, so dropping it here IS the implicit commit.
        if upper == "BEGIN" || upper == "BEGIN WORK" || upper == "START TRANSACTION" {
            self.txs
                .lock()
                .expect("tx map poisoned")
                .remove(&session);
        }
        let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH");
        // The session owns the BEGIN..COMMIT buffer. Since the CMS edition
        // guarantees read-your-writes (see below), the buffer only lives for
        // the duration of ONE statement.
        let mut tx_buf = self.txs.lock().expect("tx map poisoned").remove(&session);
        let mut res = match self.engine.execute_in_tx(&forwarded, &mut tx_buf) {
            Ok(res) => {
                // CMS edition — read-your-writes inside transactions.
                // MediaWiki's page save does INSERT page + UPDATE page inside
                // one BEGIN..COMMIT; with the ops kept buffered until COMMIT
                // the UPDATE cannot see its own INSERT (affected_rows 0,
                // "Failed to update page row"). The CMS gate therefore applies
                // the buffered ops to the engine after EVERY statement: a
                // transaction stays a durability marker (each statement is
                // one WAL batch), and ROLLBACK no longer un-writes DML that
                // already ran. The single-writer LSM engine has no MVCC
                // snapshot to roll back to.
                if let Some(buf) = tx_buf.as_mut() {
                    if buf.active && !buf.ops.is_empty() {
                        let ops = std::mem::take(&mut buf.ops);
                        self.engine.engine.write_batch(&ops).map_err(|e| CmsError {
                            errno: errno::ER_SP_DOES_NOT_EXIST,
                            message: format!("tx apply failed: {}", e),
                        })?;
                    }
                }
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
                return Err(CmsError {
                    errno: classify_error(&e),
                    message: e,
                });
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
            let locked =
                self.lock_select_rows(session, sql, tail_pos, nowait, skip_locked, &mut rows)?;
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
            decimal_scales: Vec::new(),
        };
        // DECIMAL(p,s) display scale: re-render resolved output columns with
        // the declared scale before DISTINCT dedupes on the rendered rows
        // (MySQL transmits DECIMAL as fixed-scale strings, see decimal.rs).
        if is_select {
            decimal::apply_decimal_scale(self, &mut out, sql);
        }
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

    fn exec_set(&self, session: u64, sql: &str, upper: &str) -> Result<CmsQueryResult, CmsError> {
        let body = sql[4..].trim();
        let upper_body = upper[4..].trim();
        if upper_body.starts_with("NAMES") {
            let name = body["NAMES".len()..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"')
                .trim()
                .to_string();
            let name = name
                .split_whitespace()
                .next()
                .unwrap_or("utf8mb4")
                .to_string();
            self.with_session(session, |s| s.set_names(&name));
            return Ok(empty_ok(format!("names set to {}", name)));
        }
        // SET [SESSION | @@session.] sql_mode = '...'
        let body_up = upper_body
            .replace("@@SESSION.", "")
            .replace("@@GLOBAL.", "");
        if body_up.starts_with("SESSION ") {
            let trimmed = body_up["SESSION ".len()..].trim_start();
            let body_up2 = body["SESSION ".len()..].trim_start();
            if trimmed.starts_with("SQL_MODE") {
                let val = after_eq(body_up2)
                    .trim_matches('\'')
                    .trim_matches('"')
                    .to_string();
                self.with_session(session, |s| s.set_sql_mode(&val));
                return Ok(empty_ok("sql_mode set"));
            }
        }
        if body_up.starts_with("SQL_MODE") || body_up.starts_with("SQL_MODE=") {
            let val = after_eq(body)
                .trim_matches('\'')
                .trim_matches('"')
                .to_string();
            self.with_session(session, |s| s.set_sql_mode(&val));
            return Ok(empty_ok("sql_mode set"));
        }
        if body_up.starts_with("AUTOCOMMIT") {
            let val = after_eq(body);
            let on =
                val == "1" || val.eq_ignore_ascii_case("ON") || val.eq_ignore_ascii_case("TRUE");
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
                        message: format!(
                            "Incorrect argument type to variable 'innodb_lock_wait_timeout': {}",
                            val
                        ),
                    });
                }
            }
        }
        // Other SETs (character_set_client, collation_connection, ...) are
        // accepted and ignored: the CMS edition always speaks utf8mb4.
        Ok(empty_ok("compat: accepted"))
    }
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
    // MySQL also spells a shared lock as `LOCK IN SHARE MODE`
    // (MediaWiki's User::addToDatabase name-check SELECT uses it).
    if let Some(p) = top_level_find_ci(sql, "LOCK IN SHARE MODE") {
        return Some((p, false, false));
    }
    None
}

mod bootstrap;
mod catalog;
mod dialect;
mod locks;
mod shims_select;
mod shims_show;
mod text;

pub(crate) use dialect::*;
pub(crate) use text::*;

pub use text::like_match;

#[cfg(test)]
mod tests;
