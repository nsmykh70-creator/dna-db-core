// SPDX-License-Identifier: AGPL-3.0-only
//! TCP Server — real multi-threaded server with authentication,
//! rate limiting, frame validation, and durable request handling.
//!
//! Thread-per-connection over std::net: each request's WAL fsync blocks only
//! its own connection thread, which is exactly the durability semantics we
//! want per client (Strict mode = zero acknowledged loss under kill -9).
//!
//! Module map (split from the former server.rs monolith, no behaviour change):
//! * [`auth`]     — `SecurityManager`, lockout tracking, `AuthenticatedUser`
//! * [`framing`]  — length-prefixed frame codec (`read_frame` / `write_frame`)

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::dna_ql::DnaQlEngine;
use crate::engine::Engine;
use crate::protocol::{parse_payload, Request, MAX_FRAME_SIZE};
use crate::sql_engine::SqlEngine;

mod auth;
mod framing;
#[cfg(test)]
mod tests;

pub use auth::{AuthenticatedUser, AuthError, SecurityManager};
pub use auth::PRE_AUTH_MAX_FRAME_SIZE;
pub use framing::{read_frame, read_frame_limited, write_frame, write_raw};

use auth::{AUTHENTICATED_TIMEOUT, PRE_AUTH_TIMEOUT, PRE_AUTH_TOTAL_TIMEOUT};

/// A connection accepted by the server: plain TCP or a rustls stream.
/// Keeping this as a trait object lets the protocol handler stay identical
/// for cleartext (tests/legacy) and TLS (production).
pub trait Connection: Read + Write + Send {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_nodelay(&self, nodelay: bool) -> io::Result<()>;
    fn peer_ip(&self) -> Option<String>;
}

impl Connection for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
    fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        TcpStream::set_nodelay(self, nodelay)
    }
    fn peer_ip(&self) -> Option<String> {
        self.peer_addr().ok().map(|addr| addr.ip().to_string())
    }
}

impl Connection for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.sock.set_write_timeout(timeout)
    }
    fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.sock.set_nodelay(nodelay)
    }
    fn peer_ip(&self) -> Option<String> {
        self.sock.peer_addr().ok().map(|addr| addr.ip().to_string())
    }
}

pub struct ServerConfig {
    pub bind_address: IpAddr,
    pub port: u16,
    pub admin_user: String,
    pub admin_pass: String,
    pub max_connections: usize,
    pub lockout_duration: Duration,
    pub max_failed_attempts: usize,
    /// Production TLS configuration. `None` is intentionally supported for
    /// unit tests and local benchmark harnesses; the CLI enables it by default.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Shared with the web panel so a password change immediately affects
    /// both listeners.
    pub security: Option<Arc<SecurityManager>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 9999,
            admin_user: "admin".into(),
            admin_pass: String::new(),
            max_connections: 128,
            lockout_duration: Duration::from_secs(60),
            max_failed_attempts: 5,
            tls: None,
            security: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct DnaDbServer {
    pub engine: Arc<Engine>,
    pub dnaql: Arc<DnaQlEngine>,
    pub sql: Arc<SqlEngine>,
    pub security: Arc<SecurityManager>,
    pub config: ServerConfig,
    active_connections: Arc<std::sync::atomic::AtomicUsize>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl DnaDbServer {
    pub fn new(
        engine: Arc<Engine>,
        dnaql: Arc<DnaQlEngine>,
        sql: Arc<SqlEngine>,
        config: ServerConfig,
    ) -> Self {
        let security = config.security.clone().unwrap_or_else(|| {
            let mut security = SecurityManager::new(&config.admin_user, &config.admin_pass);
            security.lockout_duration = config.lockout_duration;
            security.max_failed_attempts = config.max_failed_attempts;
            Arc::new(security)
        });
        Self {
            engine,
            dnaql,
            sql,
            security,
            config,
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Blocking accept loop. Returns when shutdown() is called or on bind error.
    pub fn serve(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind((self.config.bind_address, self.config.port))?;
        eprintln!(
            "[SERVER] DNA-DB listening on {}:{} ({}, data dir: {:?})",
            self.config.bind_address,
            self.config.port,
            if self.config.tls.is_some() {
                "TLS"
            } else {
                "cleartext"
            },
            self.engine.data_dir()
        );
        listener.set_nonblocking(true)?;

        while !self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    if !reserve_connection(&self.active_connections, self.config.max_connections) {
                        let mut s = stream;
                        let _ = write_frame(&mut s, b"server busy");
                        continue;
                    }
                    let security = self.security.clone();
                    let engine = self.engine.clone();
                    let dnaql = self.dnaql.clone();
                    let sql = self.sql.clone();
                    let active_counter = self.active_connections.clone();
                    let tls = self.config.tls.clone();
                    if let Err(error) = std::thread::Builder::new().spawn(move || {
                        let _active_guard = ActiveConnectionGuard(active_counter);
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_read_timeout(Some(PRE_AUTH_TIMEOUT));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                        let result = match tls {
                            Some(config) => match crate::tls::accept(config, stream) {
                                Ok(tls_stream) => handle_connection(
                                    Box::new(tls_stream),
                                    addr,
                                    security,
                                    engine,
                                    dnaql,
                                    sql,
                                ),
                                Err(e) => Err(e),
                            },
                            None => handle_connection(
                                Box::new(stream),
                                addr,
                                security,
                                engine,
                                dnaql,
                                sql,
                            ),
                        };
                        if let Err(error) = result {
                            if error.kind() != std::io::ErrorKind::UnexpectedEof
                                && error.kind() != std::io::ErrorKind::ConnectionReset
                                && error.kind() != std::io::ErrorKind::BrokenPipe
                                && error.kind() != std::io::ErrorKind::TimedOut
                                && error.kind() != std::io::ErrorKind::WouldBlock
                            {
                                eprintln!("[SERVER] connection {}: {}", addr, error);
                            }
                        }
                    }) {
                        self.active_connections
                            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                        return Err(error);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
        eprintln!("[SERVER] shutdown complete");
        Ok(())
    }

    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn active_connections(&self) -> usize {
        self.active_connections
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn reserve_connection(active: &std::sync::atomic::AtomicUsize, limit: usize) -> bool {
    let mut current = active.load(std::sync::atomic::Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

struct ActiveConnectionGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn handle_connection(
    mut stream: Box<dyn Connection>,
    addr: SocketAddr,
    security: Arc<SecurityManager>,
    engine: Arc<Engine>,
    dnaql: Arc<DnaQlEngine>,
    sql: Arc<SqlEngine>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(PRE_AUTH_TIMEOUT))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    stream.set_nodelay(true).ok();
    let ip = addr.ip().to_string();
    let started_at = std::time::Instant::now();
    let mut authenticated = false;
    let mut role = String::new();
    let mut allowed_tables: Option<Vec<String>> = None;
    // v3.6: per-session SQL transaction (BEGIN ... COMMIT/ROLLBACK).
    let mut sql_tx: Option<crate::sql_engine::SqlTxBuffer> = None;

    while let Some(payload) = read_frame_limited(
        &mut *stream,
        if authenticated {
            MAX_FRAME_SIZE
        } else {
            PRE_AUTH_MAX_FRAME_SIZE
        },
    )? {
        // Slowloris guard: absolute deadline for the unauthenticated phase.
        if !authenticated && started_at.elapsed() > PRE_AUTH_TOTAL_TIMEOUT {
            write_frame(&mut *stream, b"ERR: authentication deadline exceeded")?;
            return Ok(());
        }
        let req = match parse_payload(&payload) {
            Ok(r) => r,
            Err(e) => {
                write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?;
                continue;
            }
        };

        match req {
            Request::Auth { user, pass } => match security.verify_credentials(&ip, &user, &pass) {
                Ok(identity) => {
                    authenticated = true;
                    role = identity.role;
                    allowed_tables = identity.allowed_tables;
                    stream.set_read_timeout(Some(AUTHENTICATED_TIMEOUT)).ok();
                    write_raw(&mut *stream, b"OK")?;
                }
                Err(AuthError::InvalidCredentials) => {
                    write_raw(&mut *stream, b"NO")?;
                }
                Err(AuthError::Locked(msg)) | Err(AuthError::Internal(msg)) => {
                    write_frame(&mut *stream, format!("ERR: {}", msg).as_bytes())?;
                    return Ok(()); // drop the connection on lockout
                }
            },
            _ if !authenticated => {
                write_frame(&mut *stream, b"ERR: not authenticated")?;
            }
            Request::Ping => {
                write_frame(
                    &mut *stream,
                    &serde_json::json!({
                        "status": "ok",
                        "auth": authenticated,
                        "stats": engine.stats.summary(),
                        "segments": engine.segment_count(),
                        "health": engine.health()
                    })
                    .to_string()
                    .into_bytes(),
                )?;
            }
            Request::Put { key, value } => {
                if !crate::access::raw_kv_allowed(&role, &allowed_tables) {
                    write_frame(
                        &mut *stream,
                        b"ERR: raw key/value access is disabled for table-restricted users",
                    )?;
                    continue;
                }
                if role == "viewer" {
                    write_frame(&mut *stream, b"ERR: viewer role is read-only")?;
                    continue;
                }
                match engine.put(&key, &value) {
                    Ok(()) => write_raw(&mut *stream, b"OK")?,
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::Get { key } => {
                if !crate::access::raw_kv_allowed(&role, &allowed_tables) {
                    write_frame(
                        &mut *stream,
                        b"ERR: raw key/value access is disabled for table-restricted users",
                    )?;
                    continue;
                }
                match engine.get(&key) {
                    Ok(Some(value)) => write_frame(&mut *stream, &value)?,
                    Ok(None) => write_frame(&mut *stream, &[])?,
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::Delete { key } => {
                if !crate::access::raw_kv_allowed(&role, &allowed_tables) {
                    write_frame(
                        &mut *stream,
                        b"ERR: raw key/value access is disabled for table-restricted users",
                    )?;
                    continue;
                }
                if role == "viewer" {
                    write_frame(&mut *stream, b"ERR: viewer role is read-only")?;
                    continue;
                }
                match engine.delete(&key) {
                    Ok(()) => write_raw(&mut *stream, b"OK")?,
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::Checkpoint => {
                if role == "viewer" || !crate::access::raw_kv_allowed(&role, &allowed_tables) {
                    write_frame(&mut *stream, b"ERR: administrator role required")?;
                    continue;
                }
                match engine.checkpoint() {
                    Ok(()) => write_raw(&mut *stream, b"OK")?,
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::DnaQlQuery { query } => {
                if role != "admin" && allowed_tables.is_some() {
                    match crate::dna_ql::referenced_tables(&query) {
                        Ok(tables) => {
                            if let Err(message) = crate::access::authorize_tables(
                                &role,
                                &allowed_tables,
                                &tables,
                            ) {
                                write_frame(&mut *stream, format!("ERR: {message}").as_bytes())?;
                                continue;
                            }
                        }
                        Err(message) => {
                            write_frame(&mut *stream, format!("ERR: {message}").as_bytes())?;
                            continue;
                        }
                    }
                }
                if role == "viewer" && !is_read_only_query(&query) {
                    write_frame(&mut *stream, b"ERR: viewer role is read-only")?;
                    continue;
                }
                // Fault guard: a panic inside the query engine (parser bug,
                // unexpected codec state) costs this one request, not the
                // whole server. Requires panic = unwind (see Cargo.toml).
                let result = catch_request_panic("dnaql", || dnaql.execute(&query));
                match result {
                    Ok(Ok(res)) => {
                        let json = format_dnaql_result(&res);
                        write_frame(&mut *stream, json.as_bytes())?;
                    }
                    Ok(Err(e)) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                    Err(message) => {
                        write_frame(&mut *stream, format!("ERR: {}", message).as_bytes())?
                    }
                }
            }
            Request::SqlQuery { query } => {
                if role != "admin" && allowed_tables.is_some() {
                    let upper = query.trim_start().to_ascii_uppercase();
                    if upper.starts_with("SHOW") || upper.starts_with("DESCRIBE") {
                        write_frame(
                            &mut *stream,
                            b"ERR: schema-wide queries are not available to table-restricted users",
                        )?;
                        continue;
                    }
                    match crate::sql_engine::referenced_tables(&query) {
                        Ok(tables) => {
                            if let Err(message) = crate::access::authorize_tables(
                                &role,
                                &allowed_tables,
                                &tables,
                            ) {
                                write_frame(&mut *stream, format!("ERR: {message}").as_bytes())?;
                                continue;
                            }
                        }
                        Err(message) => {
                            write_frame(&mut *stream, format!("ERR: {message}").as_bytes())?;
                            continue;
                        }
                    }
                }
                if role == "viewer" && !is_read_only_query(&query) {
                    write_frame(&mut *stream, b"ERR: viewer role is read-only")?;
                    continue;
                }
                // Fault guard, same as the DnaQL path above.
                let result = catch_request_panic("sql", || sql.execute_in_tx(&query, &mut sql_tx));
                match result {
                    Ok(Ok(res)) => {
                        let json = format_sql_result(&res);
                        write_frame(&mut *stream, json.as_bytes())?;
                    }
                    Ok(Err(e)) => {
                        // A failed statement must not poison the session:
                        // v3.6 auto-aborts a broken transaction buffer the way
                        // a serializable conflict aborts a tx session.
                        sql_tx = None;
                        write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?;
                    }
                    Err(message) => {
                        sql_tx = None;
                        write_frame(&mut *stream, format!("ERR: {}", message).as_bytes())?
                    }
                }
            }
        }
    }
    Ok(())
}

/// Run a request handler, converting a panic into an ERR response.
/// With `panic = unwind` a bug must cost one request, not the whole server
/// (the release profile used to be `panic = abort`: any parser/eval panic
/// killed every connected client at once).
pub(crate) fn catch_request_panic<T>(
    label: &str,
    f: impl FnOnce() -> T,
) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|panic| {
        let detail = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        eprintln!("[PANIC] {} handler: {}", label, detail);
        "internal error: request aborted by the fault guard".to_string()
    })
}

fn format_dnaql_result(res: &crate::dna_ql::DnaQlResult) -> String {
    let rows: Vec<serde_json::Value> = res
        .rows
        .iter()
        .map(|row| {
            let fields = row
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), value_to_json(value)))
                .collect();
            serde_json::Value::Object(fields)
        })
        .collect();
    serde_json::json!({
        "ast": res.ast_summary,
        "rows": rows,
        "note": res.engine_note,
        "time_us": res.execution_time_us
    })
    .to_string()
}

/// Conservative read-only classification used for the `viewer` role on both
/// the TCP and panel surfaces. Anything ambiguous is treated as a write.
pub fn is_read_only_query(query: &str) -> bool {
    let trimmed = query.trim().trim_end_matches(';').trim();
    if trimmed.contains(';') {
        return false;
    }
    let Some(first) = trimmed.split_whitespace().next() else {
        return false;
    };
    // v3.6: BEGIN/COMMIT/ROLLBACK control statements do not mutate data
    // themselves (buffered DML is blocked separately by the viewer path —
    // INSERT/UPDATE/DELETE are not in this list), so they stay allowed.
    matches!(
        first.to_ascii_uppercase().as_str(),
        "SELECT"
            | "SHOW"
            | "DESCRIBE"
            | "DESC"
            | "EXPLAIN"
            | "PRAGMA"
            | "ANALYZE"
            | "FIND"
            | "SEARCH"
            | "VERIFY"
            | "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
    )
}

fn format_sql_result(res: &crate::sql_engine::QueryResult) -> String {
    let rows: Vec<serde_json::Value> = res
        .rows
        .iter()
        .map(|row| {
            let fields: serde_json::Map<String, serde_json::Value> = res
                .columns
                .iter()
                .filter_map(|column| {
                    row.get(column)
                        .map(|value| (column.clone(), value_to_json(value)))
                })
                .collect();
            serde_json::Value::Object(fields)
        })
        .collect();
    serde_json::json!({
        "columns": res.columns,
        "rows": rows,
        "plan": res.execution_plan,
        "time_us": res.execution_time_us
    })
    .to_string()
}

fn value_to_json(value: &crate::relational::Value) -> serde_json::Value {
    match value {
        crate::relational::Value::Null => serde_json::Value::Null,
        crate::relational::Value::Int(value) => serde_json::json!(value),
        crate::relational::Value::Float(value) if value.is_finite() => serde_json::json!(value),
        crate::relational::Value::Float(_) => serde_json::Value::Null,
        crate::relational::Value::Text(value) | crate::relational::Value::Dna(value) => {
            serde_json::Value::String(value.clone())
        }
    }
}
