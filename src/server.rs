// SPDX-License-Identifier: AGPL-3.0-only
//! TCP Server — real multi-threaded server with authentication,
//! rate limiting, frame validation, and durable request handling.
//!
//! Thread-per-connection over std::net: each request's WAL fsync blocks only
//! its own connection thread, which is exactly the durability semantics we
//! want per client (Strict mode = zero acknowledged loss under kill -9).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::engine::Engine;
use crate::dna_ql::DnaQlEngine;
use crate::protocol::{parse_payload, Request, MAX_FRAME_SIZE};
use crate::sql_engine::SqlEngine;

pub struct ServerConfig {
    pub bind_address: IpAddr,
    pub port: u16,
    pub admin_user: String,
    pub admin_pass: String,
    pub max_connections: usize,
    pub lockout_duration: Duration,
    pub max_failed_attempts: usize,
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
        }
    }
}

// ---------------------------------------------------------------------------
// SecurityManager — salted SHA-256 + constant-time verify + IP lockout
// ---------------------------------------------------------------------------

pub struct SecurityManager {
    failed_attempts: Mutex<HashMap<String, (usize, Instant)>>,
    pub lockout_duration: Duration,
    pub max_failed_attempts: usize,
    admin_user: Vec<u8>,
    /// SHA-256(salt || password) — plaintext never stored
    admin_pass_hash: Vec<u8>,
    salt: Vec<u8>,
}

fn derive_hash(salt: &[u8], pass: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(pass.as_bytes());
    hasher.finalize().to_vec()
}

impl SecurityManager {
    pub fn new(admin_user: &str, admin_pass: &str) -> Self {
        // Random-ish per-process salt (crash-safe: creds are re-derived at startup)
        let salt = std::process::id().to_le_bytes().to_vec();
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut salt2 = salt.clone();
        salt2.extend_from_slice(&t.to_le_bytes());

        Self {
            failed_attempts: Mutex::new(HashMap::new()),
            lockout_duration: Duration::from_secs(60),
            max_failed_attempts: 5,
            admin_user: admin_user.as_bytes().to_vec(),
            admin_pass_hash: derive_hash(&salt2, admin_pass),
            salt: salt2,
        }
    }

    pub fn verify_credentials(&self, ip: &str, user: &str, pass: &str) -> Result<bool, &'static str> {
        let mut attempts = self.failed_attempts.lock();
        if let Some((count, last_try)) = attempts.get(ip) {
            if *count >= self.max_failed_attempts {
                if last_try.elapsed() < self.lockout_duration {
                    return Err("IP temporarily locked due to excessive failed attempts");
                }
                attempts.remove(ip);
            }
        }

        let user_ok = user.as_bytes().ct_eq(&self.admin_user).into();
        let pass_ok = derive_hash(&self.salt, pass).ct_eq(&self.admin_pass_hash).into();

        if user_ok && pass_ok {
            attempts.remove(ip);
            Ok(true)
        } else {
            let entry = attempts.entry(ip.to_string()).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

pub fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];

    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_SIZE",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}

pub fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)
}

pub fn write_raw(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
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
        let mut security = SecurityManager::new(&config.admin_user, &config.admin_pass);
        security.lockout_duration = config.lockout_duration;
        security.max_failed_attempts = config.max_failed_attempts;
        Self {
            engine,
            dnaql,
            sql,
            security: Arc::new(security),
            config,
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Blocking accept loop. Returns when shutdown() is called or on bind error.
    pub fn serve(&self) -> std::io::Result<()> {
        if self.config.admin_pass.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an admin password is required before the server can listen",
            ));
        }
        let listener = TcpListener::bind((self.config.bind_address, self.config.port))?;
        eprintln!(
            "[SERVER] DNA-DB listening on {}:{} (data dir: {:?})",
            self.config.bind_address,
            self.config.port,
            self.engine.data_dir()
        );
        listener.set_nonblocking(true)?;

        while !self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    if !reserve_connection(
                        &self.active_connections,
                        self.config.max_connections,
                    ) {
                        let mut s = stream;
                        let _ = write_frame(&mut s, b"server busy");
                        continue;
                    }
                    let security = self.security.clone();
                    let engine = self.engine.clone();
                    let dnaql = self.dnaql.clone();
                    let sql = self.sql.clone();
                    let active_counter = self.active_connections.clone();
                    if let Err(error) = std::thread::Builder::new().spawn(move || {
                        let _active_guard = ActiveConnectionGuard(active_counter);
                        let _ = handle_connection(stream, addr, security, engine, dnaql, sql);
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

fn reserve_connection(
    active: &std::sync::atomic::AtomicUsize,
    limit: usize,
) -> bool {
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
    mut stream: TcpStream,
    addr: SocketAddr,
    security: Arc<SecurityManager>,
    engine: Arc<Engine>,
    dnaql: Arc<DnaQlEngine>,
    sql: Arc<SqlEngine>,
) -> std::io::Result<()> {
    // On Windows accept() inherits the listener's non-blocking mode; the
    // accepted socket must be switched back to blocking before framed reads
    // (otherwise read_exact fails with WouldBlock between requests).
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    stream.set_nodelay(true).ok();
    let ip = addr.ip().to_string();
    let mut authenticated = false;

    while let Some(payload) = read_frame(&mut stream)? {
        let req = match parse_payload(&payload) {
            Ok(r) => r,
            Err(e) => {
                write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?;
                continue;
            }
        };

        match req {
            Request::Auth { user, pass } => match security.verify_credentials(&ip, &user, &pass) {
                Ok(true) => {
                    authenticated = true;
                    write_raw(&mut stream, b"OK")?;
                }
                Ok(false) => {
                    write_raw(&mut stream, b"NO")?;
                }
                Err(msg) => {
                    write_frame(&mut stream, format!("ERR: {}", msg).as_bytes())?;
                    return Ok(()); // drop the connection on lockout
                }
            },
            _ if !authenticated => {
                write_frame(&mut stream, b"ERR: not authenticated")?;
            }
            Request::Ping => {
                write_frame(
                    &mut stream,
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
                match engine.put(&key, &value) {
                    Ok(()) => write_raw(&mut stream, b"OK")?,
                    Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::Get { key } => {
                match engine.get(&key) {
                    Ok(Some(value)) => write_frame(&mut stream, &value)?,
                    Ok(None) => write_frame(&mut stream, &[])?,
                    Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
                }
            }
            Request::Delete { key } => match engine.delete(&key) {
                Ok(()) => write_raw(&mut stream, b"OK")?,
                Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
            },
            Request::Checkpoint => match engine.checkpoint() {
                Ok(()) => write_raw(&mut stream, b"OK")?,
                Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
            },
            Request::DnaQlQuery { query } => match dnaql.execute(&query) {
                Ok(res) => {
                    let json = format_dnaql_result(&res);
                    write_frame(&mut stream, json.as_bytes())?;
                }
                Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
            },
            Request::SqlQuery { query } => match sql.execute(&query) {
                Ok(res) => {
                    let json = format_sql_result(&res);
                    write_frame(&mut stream, json.as_bytes())?;
                }
                Err(e) => write_frame(&mut stream, format!("ERR: {}", e).as_bytes())?,
            },
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
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()
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
        stream
            .write_all(&(msg.len() as u32).to_le_bytes())
            .unwrap();
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
        let q = format!("\x10chr1 ? 'ACGT' ~0");
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
        assert_eq!(
            successes.load(std::sync::atomic::Ordering::Relaxed),
            4
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 4);
    }

    #[test]
    fn server_refuses_to_listen_without_a_configured_password() {
        let dir = std::env::temp_dir().join(format!(
            "dna_srv_no_pass_{}",
            std::process::id()
        ));
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            scrub: None,
            ..Default::default()
        })
        .unwrap();
        let sql = Arc::new(SqlEngine::new(engine.clone()));
        let dnaql = Arc::new(DnaQlEngine::new(engine.clone(), sql.clone()));
        let server = DnaDbServer::new(
            engine.clone(),
            dnaql,
            sql,
            ServerConfig::default(),
        );
        assert_eq!(
            server.serve().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
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
        let json: serde_json::Value =
            serde_json::from_str(&format_sql_result(&result)).unwrap();
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
            assert_eq!(sec.verify_credentials(ip_local, "admin", "bad").unwrap(), false);
        }
        assert!(sec.verify_credentials(ip_local, "admin", "password").is_err());
    }
}
