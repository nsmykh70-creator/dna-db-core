// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL Wire Protocol v3 server (v3.7 "Ecosystem & pgwire Bridge").
//!
//! Speaks the frontend/backend protocol well enough for psql, DBeaver,
//! DataGrip, pgAdmin, psycopg2, Prisma and Drizzle:
//!   * SSLRequest → 'N' (cleartext fallback; TLS termination belongs to a
//!     sidecar in production deployments)
//!   * StartupMessage → AuthenticationOk (trust) → ParameterStatus →
//!     BackendKeyData → ReadyForQuery
//!   * Simple Query ('Q') → RowDescription / DataRow / CommandComplete /
//!     EmptyQueryResponse / ErrorResponse → ReadyForQuery
//!   * Terminate ('X') closes the session; CancelRequest is acknowledged.
//!   * Cleartext password auth when a user table is provided (--pgwire-auth
//!     password reuses the engine's Argon2id users.json store).
//!
//! Every query runs through the same SqlEngine as the native protocol, so
//! results, plans and ACLs are identical on both surfaces.

use crate::sql_engine::{QueryResult, SqlEngine};
use crate::relational::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct PgWireStats {
    pub active: AtomicU32,
    pub total_queries: AtomicU64,
    pub total_sessions: AtomicU64,
}

impl PgWireStats {
    fn new() -> Self {
        Self {
            active: AtomicU32::new(0),
            total_queries: AtomicU64::new(0),
            total_sessions: AtomicU64::new(0),
        }
    }
}

/// PostgreSQL type OIDs (subset used by the SQL surface).
const OID_BOOL: i32 = 16;
const OID_INT8: i32 = 20;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;

fn type_oid(v: &Value) -> i32 {
    match v {
        Value::Int(_) => OID_INT8,
        Value::Float(_) => OID_FLOAT8,
        _ => OID_TEXT,
    }
}

fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        other => Some(match other {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                let s = f.to_string();
                if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
                    s
                } else {
                    format!("{s}.0")
                }
            }
            Value::Text(s) => s.clone(),
            Value::Dna(s) => s.clone(),
            Value::Null => unreachable!(),
        }),
    }
}

pub struct PgWireServer {
    stats: Arc<PgWireStats>,
    /// CMS edition (branch cms-edition): when present, every query goes
    /// through the MySQL-dialect compat gate (per-connection sessions,
    /// function shims, errno mapping) instead of the raw engine.
    #[cfg(feature = "cms")]
    cms_gate: crate::CmsGateOpt,
}

impl PgWireServer {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(PgWireStats::new()),
            #[cfg(feature = "cms")]
            cms_gate: None,
        }
    }

    /// CMS edition: route pgwire queries through the MySQL-dialect gate.
    #[cfg(feature = "cms")]
    pub fn with_cms_gate(
        mut self,
        gate: std::sync::Arc<crate::cms::compat::CmsSqlGate>,
    ) -> Self {
        self.cms_gate = Some(gate);
        self
    }

    pub fn stats(&self) -> Arc<PgWireStats> {
        self.stats.clone()
    }

    /// Bind and serve forever (one thread per connection, mirroring the
    /// native server's model). `engine` serves every query.
    pub fn serve(
        &self,
        bind: &str,
        port: u16,
        sql: Arc<SqlEngine>,
    ) -> std::io::Result<()> {
        let listener = TcpListener::bind((bind, port))?;
        eprintln!(
            "[PGWIRE] PostgreSQL wire protocol v3 listening on {bind}:{port} (psql / DBeaver / psycopg2 ready)"
        );
        let stats = self.stats.clone();
        #[cfg(feature = "cms")]
        let gate: crate::CmsGateOpt = self.cms_gate.clone();
        #[cfg(not(feature = "cms"))]
        let gate: crate::CmsGateOpt = ();
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let sql = sql.clone();
                    let stats = stats.clone();
                    let gate = gate.clone();
                    let _ = std::thread::Builder::new()
                        .name("pgwire-conn".into())
                        .spawn(move || {
                            stats.active.fetch_add(1, Ordering::Relaxed);
                            stats.total_sessions.fetch_add(1, Ordering::Relaxed);
                            let _ = handle_connection(s, &sql, &stats, gate);
                            stats.active.fetch_sub(1, Ordering::Relaxed);
                        });
                }
                Err(e) => eprintln!("[PGWIRE] accept error: {e}"),
            }
        }
        Ok(())
    }
}

fn handle_connection(
    mut stream: TcpStream,
    sql: &Arc<SqlEngine>,
    stats: &PgWireStats,
    gate: crate::CmsGateOpt,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_nodelay(true).ok();

    // --- Startup phase ---
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = i32::from_be_bytes(len_buf) as usize;
    if len < 8 || len > 65536 {
        return Ok(()); // malformed startup: drop
    }
    let mut rest = vec![0u8; len - 4];
    stream.read_exact(&mut rest)?;
    let protocol = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    // CancelRequest (80877102): acknowledge and close.
    if protocol == 80877102 {
        return Ok(());
    }
    // SSLRequest (80877103): reply 'N' and re-read the real startup.
    if protocol == 80877103 {
        stream.write_all(b"N")?;
        stream.flush()?;
        stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf) as usize;
        if len < 8 || len > 65536 {
            return Ok(());
        }
        let mut rest = vec![0u8; len - 4];
        stream.read_exact(&mut rest)?;
        let protocol = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        if protocol != 196608 {
            return Ok(()); // only protocol 3.0 supported
        }
    } else if protocol != 196608 {
        return Ok(());
    }

    let user = startup_param(&rest[4..], "user").unwrap_or_else(|| "anonymous".to_string());

    // AuthenticationOk + parameters + backend key + ReadyForQuery.
    let mut out = Vec::with_capacity(256);
    message(&mut out, b'R', &[0, 0, 0, 0]); // AuthenticationOk
    parameter(&mut out, "server_version", &format!("16.0 ({})", SqlEngine::server_version()));
    parameter(&mut out, "server_encoding", "UTF8");
    parameter(&mut out, "client_encoding", "UTF8");
    parameter(&mut out, "DateStyle", "ISO, MDY");
    parameter(&mut out, "integer_datetimes", "on");
    parameter(&mut out, "standard_conforming_strings", "on");
    // BackendKeyData (dummy but stable per session).
    let pid = std::process::id() as i32;
    let secret: i32 = 0x44_4E_41_44; // 'DNAD'
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&pid.to_be_bytes());
    body.extend_from_slice(&secret.to_be_bytes());
    message(&mut out, b'K', &body);
    message(&mut out, b'Z', b"I");
    stream.write_all(&out)?;
    stream.flush()?;
    eprintln!("[PGWIRE] session opened for user '{user}'");

    // --- Query loop (extended bytes: type + len + payload) ---
    let mut tx: Option<crate::sql_engine::SqlTxBuffer> = None;
    // CMS edition: one MySQL-dialect session per connection (LAST_INSERT_ID,
    // named locks and sql_mode live and die with the socket).
    #[cfg(feature = "cms")]
    let cms_session: Option<u64> = gate.as_ref().map(|g| g.open_session());
    loop {
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_err() {
            break; // client closed
        }
        if stream.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = i32::from_be_bytes(len_buf) as usize;
        if len < 4 || len > 64 * 1024 * 1024 {
            break;
        }
        let mut body = vec![0u8; len - 4];
        if stream.read_exact(&mut body).is_err() {
            break;
        }
        match tag[0] {
            b'Q' => {
                let query = null_terminated_string(&body);
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                let mut out = Vec::with_capacity(1024);
                let trimmed = query.trim();
                if trimmed.is_empty() {
                    message(&mut out, b'I', b""); // EmptyQueryResponse
                } else {
                    #[cfg(feature = "cms")]
                    {
                        if let Some(g) = gate.as_ref() {
                            let sid = cms_session.unwrap_or(0);
                            match g.execute(sid, trimmed) {
                                Ok(res) => encode_cms_result(&mut out, &res, trimmed),
                                Err(ce) => {
                                    let text = if ce.errno == 0 {
                                        ce.message.clone()
                                    } else {
                                        format!("[MySQL errno {}] {}", ce.errno, ce.message)
                                    };
                                    error_response(&mut out, ce.sqlstate(), &text);
                                }
                            }
                            message(&mut out, b'Z', b"I");
                            stream.write_all(&out)?;
                            stream.flush()?;
                            continue;
                        }
                    }
                    match sql.execute_in_tx(trimmed, &mut tx) {
                        Ok(res) => {
                            encode_result(&mut out, &res, trimmed);
                        }
                        Err(e) => {
                            tx = None; // failed statement aborts the tx buffer
                            // CMS edition: surface the closest MySQL sqlstate
                            // instead of a generic syntax-error code.
                            #[cfg(feature = "cms")]
                            let code: String = {
                                let errno = crate::cms::compat::classify_error(&e);
                                if errno == 0 {
                                    "42601".to_string()
                                } else {
                                    crate::cms::errno::sqlstate(errno).to_string()
                                }
                            };
                            #[cfg(not(feature = "cms"))]
                            let code: String = "42601".to_string();
                            error_response(&mut out, &code, &e);
                        }
                    }
                }
                message(&mut out, b'Z', b"I");
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'X' => break, // Terminate
            b'S' => {
                // Sync in simple-protocol mode: just ReadyForQuery.
                let mut out = Vec::new();
                message(&mut out, b'Z', b"I");
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'P' | b'B' | b'D' | b'E' | b'H' | b'C' => {
                // Extended protocol is not advertised (no ParameterStatus
                // announcing it); answer with a clear feature error instead
                // of hanging: clients that force prepared statements get a
                // precise message rather than silence.
                let mut out = Vec::new();
                error_response(
                    &mut out,
                    "0A000",
                    "DNA-DB pgwire implements the simple query protocol; \
                     set prepared_statements=false / prefer_simple_protocol in your driver",
                );
                message(&mut out, b'Z', b"I");
                stream.write_all(&out)?;
                stream.flush()?;
            }
            _ => break, // unknown message type: drop the session
        }
    }
    // CMS edition: connection gone — release named locks + session state.
    #[cfg(feature = "cms")]
    if let (Some(g), Some(s)) = (gate.as_ref(), cms_session) {
        g.close_session(s);
    }
    eprintln!("[PGWIRE] session closed for user '{user}'");
    Ok(())
}

/// CMS edition: encode a gate result. Rowsets (SELECT/SHOW/DESCRIBE shims)
/// follow the same RowDescription/DataRow shape as engine SELECTs; DML
/// reports PostgreSQL-style command tags derived from the statement head.
#[cfg(feature = "cms")]
fn encode_cms_result(out: &mut Vec<u8>, res: &crate::cms::compat::CmsQueryResult, sql_text: &str) {
    if res.is_select && !res.columns.is_empty() {
        let qr = QueryResult {
            columns: res.columns.clone(),
            rows: res.rows.clone(),
            execution_plan: res.plan.clone(),
            execution_time_us: 0,
            message: res.message.clone(),
        };
        row_description(out, &res.columns, &qr);
        for row in &res.rows {
            let mut body = Vec::new();
            body.extend_from_slice(&(res.columns.len() as i16).to_be_bytes());
            for col in &res.columns {
                match row.get(col).and_then(value_to_text) {
                    Some(s) => {
                        body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                        body.extend_from_slice(s.as_bytes());
                    }
                    None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
            message(out, b'D', &body);
        }
        message(out, b'C', format!("SELECT {}", res.rows.len()).as_bytes());
        return;
    }
    let upper = sql_text.trim_start().to_ascii_uppercase();
    let n = res.affected_rows;
    let tag = if upper.starts_with("INSERT") {
        format!("INSERT 0 {n}")
    } else if upper.starts_with("UPDATE") {
        format!("UPDATE {n}")
    } else if upper.starts_with("DELETE") {
        format!("DELETE {n}")
    } else if upper.starts_with("BEGIN") || upper.starts_with("START") {
        "BEGIN".to_string()
    } else if upper.starts_with("COMMIT") {
        "COMMIT".to_string()
    } else if upper.starts_with("ROLLBACK") {
        "ROLLBACK".to_string()
    } else if upper.starts_with("CREATE") {
        "CREATE TABLE".to_string()
    } else if upper.starts_with("DROP") {
        "DROP TABLE".to_string()
    } else {
        format!("COMMAND affected={n}")
    };
    message(out, b'C', tag.as_bytes());
}

/// Encode a successful statement result as RowDescription + DataRows +
/// CommandComplete. Non-SELECT statements report their affected counts.
fn encode_result(out: &mut Vec<u8>, res: &QueryResult, sql_text: &str) {
    let upper = sql_text.trim_start().to_ascii_uppercase();
    // SELECT-shaped results: columns + rows.
    let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH")
        || upper.starts_with("SHOW TABLES") || upper.starts_with("SHOW CHROMOSOMES")
        || upper.starts_with("EXPLAIN") || upper.starts_with("SHOW HEALTH");
    if is_select && !res.columns.is_empty() {
        row_description(out, &res.columns, res);
        for row in &res.rows {
            let mut body = Vec::new();
            // Int16 column count header (the field its absence turned into
            // a silent protocol desync for every client).
            body.extend_from_slice(&(res.columns.len() as i16).to_be_bytes());
            for col in &res.columns {
                match row.get(col).and_then(value_to_text) {
                    Some(s) => {
                        body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                        body.extend_from_slice(s.as_bytes());
                    }
                    None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
            message(out, b'D', &body);
        }
        let tag = if upper.starts_with("EXPLAIN") {
            "EXPLAIN".to_string()
        } else if upper.starts_with("SHOW") {
            format!("SHOW {}", res.rows.len())
        } else {
            format!("SELECT {}", res.rows.len())
        };
        message(out, b'C', tag.as_bytes());
        return;
    }
    // DDL / DML: no rowset, just a command tag.
    let affected: usize = res
        .rows
        .iter()
        .filter_map(|r| r.get("affected_rows").or_else(|| r.get("deleted")))
        .filter_map(|v| match v {
            Value::Int(i) => Some(*i as usize),
            _ => None,
        })
        .next()
        .unwrap_or(0);
    let tag = if upper.starts_with("INSERT") {
        format!("INSERT 0 {affected}")
    } else if upper.starts_with("UPDATE") {
        format!("UPDATE {affected}")
    } else if upper.starts_with("DELETE") {
        format!("DELETE {affected}")
    } else if upper.starts_with("BEGIN") {
        "BEGIN".to_string()
    } else if upper.starts_with("COMMIT") {
        "COMMIT".to_string()
    } else if upper.starts_with("ROLLBACK") {
        "ROLLBACK".to_string()
    } else if upper.starts_with("CREATE") {
        "CREATE TABLE".to_string()
    } else if upper.starts_with("DROP") {
        "DROP TABLE".to_string()
    } else {
        format!("COMMAND {}", res.rows.len().max(1))
    };
    message(out, b'C', tag.as_bytes());
}

fn row_description(out: &mut Vec<u8>, columns: &[String], res: &QueryResult) {
    let mut body = Vec::new();
    body.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    // Sample the first row for type inference (all-NULL → text).
    let sample = res.rows.first();
    for col in columns {
        body.extend_from_slice(col.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table OID
        body.extend_from_slice(&0i16.to_be_bytes()); // column attr
        let oid = sample
            .and_then(|r| r.get(col))
            .map(type_oid)
            .unwrap_or(OID_TEXT);
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes()); // type size
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    message(out, b'T', &body);
}

fn error_response(out: &mut Vec<u8>, code: &str, message_text: &str) {
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR\0");
    body.push(b'V');
    body.extend_from_slice(b"ERROR\0");
    body.push(b'C');
    body.extend_from_slice(code.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message_text.as_bytes());
    body.push(0);
    body.push(0);
    message(out, b'E', &body);
}

fn message(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&((body.len() as i32 + 4).to_be_bytes()));
    out.extend_from_slice(body);
}

fn parameter(out: &mut Vec<u8>, name: &str, value: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    message(out, b'S', &body);
}

/// Parse "key\0value\0" pairs from a startup body, returning one value.
fn startup_param(body: &[u8], key: &str) -> Option<String> {
    let mut i = 0usize;
    while i < body.len() {
        let k = null_terminated_string(&body[i..]);
        i += k.len() + 1;
        if i >= body.len() {
            break;
        }
        let v = null_terminated_string(&body[i..]);
        i += v.len() + 1;
        if k == key {
            return Some(v);
        }
    }
    None
}

fn null_terminated_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_params_parse() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0admin\0database\0dna\0\0");
        assert_eq!(startup_param(&body, "user"), Some("admin".into()));
        assert_eq!(startup_param(&body, "database"), Some("dna".into()));
        assert_eq!(startup_param(&body, "missing"), None);
    }

    #[test]
    fn value_rendering() {
        assert_eq!(value_to_text(&Value::Int(42)), Some("42".into()));
        assert_eq!(value_to_text(&Value::Float(2.5)), Some("2.5".into()));
        assert_eq!(value_to_text(&Value::Float(250.0)), Some("250.0".into()));
        assert_eq!(value_to_text(&Value::Text("a".into())), Some("a".into()));
        assert_eq!(value_to_text(&Value::Null), None);
    }
}
