// SPDX-License-Identifier: AGPL-3.0-only
//! TCP Server — real multi-threaded server with authentication,
//! rate limiting, frame validation, and durable request handling.
//!
//! Thread-per-connection over std::net: each request's WAL fsync blocks only
//! its own connection thread, which is exactly the durability semantics we
//! want per client (Strict mode = zero acknowledged loss under kill -9).

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::auth::{UserStore, UserView};
use crate::dna_ql::DnaQlEngine;
use crate::engine::Engine;
use crate::protocol::{parse_payload, Request, MAX_FRAME_SIZE};
use crate::sql_engine::SqlEngine;

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
// SecurityManager: Argon2id users, persistent per-IP + per-username lockout
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub username: String,
    pub role: String,
    pub allowed_tables: Option<Vec<String>>,
    pub must_change: bool,
}

#[derive(Debug, Clone)]
pub enum AuthError {
    InvalidCredentials,
    Locked(String),
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidCredentials => write!(f, "invalid credentials"),
            AuthError::Locked(msg) => write!(f, "{msg}"),
            AuthError::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedAttempt {
    failures: usize,
    last_unix: u64,
}

pub struct SecurityManager {
    pub users: Arc<UserStore>,
    failed_attempts: Mutex<HashMap<String, PersistedAttempt>>,
    state_path: Option<PathBuf>,
    global_failures: Mutex<VecDeque<Instant>>,
    /// Serialise Argon2 verification. Each hash needs ~19 MiB; without this
    /// gate 128 pre-auth connections could reserve several gigabytes.
    auth_gate: Mutex<()>,
    /// Serialise atomic replacement of `security_state.json` so concurrent
    /// failures cannot rename an older snapshot over a newer one.
    state_write: Mutex<()>,
    pub lockout_duration: Duration,
    pub max_failed_attempts: usize,
}

const GLOBAL_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_FAILURE_LIMIT: usize = 2_000;
const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const AUTHENTICATED_TIMEOUT: Duration = Duration::from_secs(300);
static TEMP_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Unauthenticated clients may only send an AUTH frame. Capping it at 64 KiB
/// removes the 128 x 16 MiB pre-auth allocation amplification.
pub const PRE_AUTH_MAX_FRAME_SIZE: usize = 64 * 1024;

impl SecurityManager {
    /// In-memory manager for unit tests and embedded callers.
    pub fn new(admin_user: &str, admin_pass: &str) -> Self {
        let users = UserStore::in_memory(admin_user, admin_pass)
            .expect("invalid test bootstrap credentials");
        Self::with_store(Arc::new(users), None)
    }

    /// Open the persistent user database and lockout state under `data_dir`.
    pub fn open(data_dir: &Path, admin_user: &str, admin_pass: &str) -> io::Result<Self> {
        let users = UserStore::open(
            &crate::auth::user_store_path(data_dir),
            admin_user,
            admin_pass,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let state_path = data_dir.join("security_state.json");
        Ok(Self::with_store(Arc::new(users), Some(state_path)))
    }

    pub fn with_store(users: Arc<UserStore>, state_path: Option<PathBuf>) -> Self {
        let failed_attempts = state_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| {
                serde_json::from_slice::<HashMap<String, PersistedAttempt>>(&bytes).ok()
            })
            .unwrap_or_default();
        Self {
            users,
            failed_attempts: Mutex::new(failed_attempts),
            state_path,
            global_failures: Mutex::new(VecDeque::new()),
            auth_gate: Mutex::new(()),
            state_write: Mutex::new(()),
            lockout_duration: Duration::from_secs(60),
            max_failed_attempts: 5,
        }
    }

    pub fn verify_credentials(
        &self,
        ip: &str,
        user: &str,
        pass: &str,
    ) -> Result<AuthenticatedUser, AuthError> {
        {
            let mut global = self.global_failures.lock();
            let now = Instant::now();
            while global
                .front()
                .is_some_and(|t| now.duration_since(*t) > GLOBAL_FAILURE_WINDOW)
            {
                global.pop_front();
            }
            if global.len() >= GLOBAL_FAILURE_LIMIT {
                return Err(AuthError::Locked(
                    "authentication temporarily throttled; retry in a minute".into(),
                ));
            }
        }

        let ip_key = format!("ip:{ip}");
        let user_key = format!("user:{}", user.to_ascii_lowercase());
        if let Some(msg) = self
            .lock_message(&ip_key)
            .or_else(|| self.lock_message(&user_key))
        {
            return Err(AuthError::Locked(msg));
        }

        let verified = {
            let _gate = self.auth_gate.lock();
            self.users.verify(user, pass)
        };
        match verified {
            Some(view) => {
                {
                    let mut attempts = self.failed_attempts.lock();
                    attempts.remove(&ip_key);
                    attempts.remove(&user_key);
                }
                self.save_state();
                Ok(AuthenticatedUser {
                    username: view.username,
                    role: view.role,
                    allowed_tables: view.allowed_tables,
                    must_change: view.must_change,
                })
            }
            None => {
                self.record_failure(&ip_key);
                self.record_failure(&user_key);
                self.global_failures.lock().push_back(Instant::now());
                if self.lock_message(&ip_key).is_some() || self.lock_message(&user_key).is_some() {
                    Err(AuthError::Locked(
                        "too many failed attempts; account/IP temporarily locked".into(),
                    ))
                } else {
                    Err(AuthError::InvalidCredentials)
                }
            }
        }
    }

    /// Resolve an identity already authenticated by a trusted reverse proxy.
    /// No password or lockout accounting is involved: the proxy is the
    /// authentication boundary and must strip client-supplied headers.
    pub fn verify_trusted_identity(&self, username: &str) -> Result<AuthenticatedUser, AuthError> {
        let view = self
            .users
            .identity(username)
            .ok_or(AuthError::InvalidCredentials)?;
        Ok(AuthenticatedUser {
            username: view.username,
            role: view.role,
            allowed_tables: view.allowed_tables,
            must_change: false,
        })
    }

    fn lock_message(&self, key: &str) -> Option<String> {
        let mut attempts = self.failed_attempts.lock();
        let entry = attempts.get(key)?;
        if entry.failures < self.max_failed_attempts {
            return None;
        }
        let now = unix_now();
        let elapsed = now.saturating_sub(entry.last_unix);
        if elapsed < self.lockout_duration.as_secs() {
            return Some(format!(
                "temporarily locked after {} failed attempts; retry in {}s",
                entry.failures,
                self.lockout_duration.as_secs().saturating_sub(elapsed)
            ));
        }
        attempts.remove(key);
        None
    }

    fn record_failure(&self, key: &str) {
        let now = unix_now();
        let mut attempts = self.failed_attempts.lock();
        if attempts.len() > 20_000 {
            attempts.retain(|_, entry| now.saturating_sub(entry.last_unix) < 86_400);
        }
        let entry = attempts.entry(key.to_string()).or_insert(PersistedAttempt {
            failures: 0,
            last_unix: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_unix = now;
        drop(attempts);
        self.save_state();
    }

    fn save_state(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let _write_guard = self.state_write.lock();
        let state = self.failed_attempts.lock().clone();
        let Ok(payload) = serde_json::to_vec(&state) else {
            return;
        };
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}.{}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if std::fs::write(&tmp, payload).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    pub fn list_users(&self) -> Vec<UserView> {
        self.users.list()
    }

    pub fn user_is_admin(&self, username: &str) -> bool {
        self.users.is_admin(username)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

pub fn read_frame<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    read_frame_limited(stream, MAX_FRAME_SIZE)
}

pub fn read_frame_limited<R: Read + ?Sized>(
    stream: &mut R,
    maximum: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];

    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame exceeds limit ({len} > {maximum} bytes)"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_frame<W: Write + ?Sized>(stream: &mut W, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)
}

pub fn write_raw<W: Write + ?Sized>(stream: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(bytes)
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
    let mut authenticated = false;
    let mut role = String::new();
    let mut allowed_tables: Option<Vec<String>> = None;

    while let Some(payload) = read_frame_limited(
        &mut *stream,
        if authenticated {
            MAX_FRAME_SIZE
        } else {
            PRE_AUTH_MAX_FRAME_SIZE
        },
    )? {
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
                match dnaql.execute(&query) {
                    Ok(res) => {
                        let json = format_dnaql_result(&res);
                        write_frame(&mut *stream, json.as_bytes())?;
                    }
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
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
                match sql.execute(&query) {
                    Ok(res) => {
                        let json = format_sql_result(&res);
                        write_frame(&mut *stream, json.as_bytes())?;
                    }
                    Err(e) => write_frame(&mut *stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};
    use crate::wal::SyncMode;

    fn spawn_server(port: u16) -> Arc<DnaDbServer> {
        let dir = std::env::temp_dir().join(format!(
            "dna_srv_{}_{}",
            port,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            scrub: None,
            ..Default::default()
        })
        .unwrap();
        let sql = Arc::new(SqlEngine::new(engine.clone()));
        let dnaql = Arc::new(DnaQlEngine::new(engine.clone(), sql.clone()));
        let server = Arc::new(DnaDbServer::new(
            engine,
            dnaql,
            sql,
            ServerConfig {
                port,
                admin_pass: "password".into(),
                ..Default::default()
            },
        ));
        let s = server.clone();
        std::thread::spawn(move || s.serve().unwrap());
        // wait for bind
        std::thread::sleep(std::time::Duration::from_millis(150));
        server
    }

    fn connect(port: u16) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_nodelay(true).ok();
        s
    }

    fn auth(stream: &mut TcpStream) {
        let msg = b"\x01admin:password";
        stream.write_all(&(msg.len() as u32).to_le_bytes()).unwrap();
        stream.write_all(msg).unwrap();
        let mut ok = [0u8; 2];
        stream.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"OK");
    }

    #[test]
    fn full_client_flow() {
        let port = 19200 + (std::process::id() % 1000) as u16;
        let _srv = spawn_server(port);

        let mut s = connect(port);
        auth(&mut s);

        // PUT
        let mut payload = vec![0x03u8];
        payload.extend_from_slice(&4u16.to_le_bytes());
        payload.extend_from_slice(b"chr1");
        payload.extend_from_slice(b"ACGTACGT");
        s.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
        s.write_all(&payload).unwrap();
        let mut ok = [0u8; 2];
        s.read_exact(&mut ok).unwrap();
        assert_eq!(&ok, b"OK");

        // GET
        let g = b"\x02chr1";
        s.write_all(&(g.len() as u32).to_le_bytes()).unwrap();
        s.write_all(g).unwrap();
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut val = vec![0u8; len];
        s.read_exact(&mut val).unwrap();
        assert_eq!(val, b"ACGTACGT");

        // PING
        let p = b"\x00";
        s.write_all(&(p.len() as u32).to_le_bytes()).unwrap();
        s.write_all(p).unwrap();
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut val = vec![0u8; len];
        s.read_exact(&mut val).unwrap();
        assert!(String::from_utf8_lossy(&val).contains("status"));

        // DNAQL
        let q = "\x10chr1 ? 'ACGT' ~0".to_string();
        s.write_all(&(q.len() as u32).to_le_bytes()).unwrap();
        s.write_all(q.as_bytes()).unwrap();
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut val = vec![0u8; len];
        s.read_exact(&mut val).unwrap();
        let text = String::from_utf8_lossy(&val).to_string();
        assert!(text.contains("\"rows\""), "got: {}", text);
    }

    #[test]
    fn auth_rejected_and_ops_blocked() {
        let port = 20200 + (std::process::id() % 1000) as u16;
        let _srv = spawn_server(port);

        let mut s = connect(port);
        // PING is also protected by authentication.
        let ping = b"\x00";
        s.write_all(&(ping.len() as u32).to_le_bytes()).unwrap();
        s.write_all(ping).unwrap();
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut ping_response = vec![0u8; len];
        s.read_exact(&mut ping_response).unwrap();
        assert!(String::from_utf8_lossy(&ping_response).contains("not authenticated"));

        // wrong password
        let msg = b"\x01admin:wrongpass";
        s.write_all(&(msg.len() as u32).to_le_bytes()).unwrap();
        s.write_all(msg).unwrap();
        let mut resp = [0u8; 2];
        s.read_exact(&mut resp).unwrap();
        assert_eq!(&resp, b"NO");

        // PUT without auth must be rejected
        let payload = b"\x03\x01\x00k"; // PUT, klen=1, key="k"
        s.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
        s.write_all(payload).unwrap();
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut val = vec![0u8; len];
        s.read_exact(&mut val).unwrap();
        assert!(String::from_utf8_lossy(&val).contains("not authenticated"));
    }

    #[test]
    fn connection_limit_reservation_is_atomic() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let successes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..16 {
            let counter = counter.clone();
            let barrier = barrier.clone();
            let successes = successes.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                if reserve_connection(&counter, 4) {
                    successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(successes.load(std::sync::atomic::Ordering::Relaxed), 4);
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    #[test]
    fn server_bootstraps_default_credentials() {
        let sec = SecurityManager::new("admin", "password");
        let identity = sec
            .verify_credentials("127.0.0.1", "admin", "password")
            .unwrap();
        assert_eq!(identity.username, "admin");
        assert_eq!(identity.role, "admin");
    }

    #[test]
    fn query_results_are_valid_json_with_escaped_values() {
        let result = crate::sql_engine::QueryResult {
            columns: vec!["name".into(), "count".into()],
            rows: vec![std::collections::BTreeMap::from([
                (
                    "name".into(),
                    crate::relational::Value::Text("line \"one\"\n\\two".into()),
                ),
                ("count".into(), crate::relational::Value::Int(7)),
            ])],
            execution_plan: "Project(\"name\")".into(),
            execution_time_us: 12,
            message: None,
        };
        let json: serde_json::Value = serde_json::from_str(&format_sql_result(&result)).unwrap();
        assert_eq!(json["rows"][0]["name"], "line \"one\"\n\\two");
        assert_eq!(json["rows"][0]["count"], 7);
    }

    #[test]
    fn lockout_after_max_attempts() {
        let port = 21200 + (std::process::id() % 1000) as u16;
        let _srv = spawn_server(port);
        let ip_local = "127.0.0.1";

        // Direct SecurityManager test for deterministic lockout behavior
        let sec = SecurityManager::new("admin", "password");
        for _ in 0..5 {
            assert!(sec.verify_credentials(ip_local, "admin", "bad").is_err());
        }
        assert!(sec
            .verify_credentials(ip_local, "admin", "password")
            .is_err());
    }

    #[test]
    fn viewer_read_only_classifier_rejects_writes_and_multistatement() {
        assert!(is_read_only_query("SELECT * FROM t"));
        assert!(is_read_only_query("  select 1;  "));
        assert!(is_read_only_query("ANALYZE SEQUENCE 'ATG'"));
        assert!(!is_read_only_query("INSERT INTO t VALUES (1)"));
        assert!(!is_read_only_query("SELECT 1; DROP TABLE t"));
        assert!(!is_read_only_query("REPAIR"));
        assert!(!is_read_only_query(""));
    }

    #[test]
    fn lockout_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "dna_srv_lock_state_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        {
            let sec = SecurityManager::open(&dir, "admin", "password").unwrap();
            for _ in 0..5 {
                let _ = sec.verify_credentials("127.0.0.2", "admin", "wrong");
            }
            assert!(dir.join("security_state.json").exists());
        }

        let sec = SecurityManager::open(&dir, "admin", "password").unwrap();
        let err = sec
            .verify_credentials("127.0.0.2", "admin", "password")
            .unwrap_err();
        assert!(matches!(err, AuthError::Locked(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
