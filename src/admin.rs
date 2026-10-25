//! Embedded HTTP admin panel (phpMyAdmin-style) — `serve --admin-port N`.
//!
//! Zero new dependencies: a small HTTP/1.1 implementation over std TcpListener
//! serving an embedded single-page app (src/admin.html) plus a JSON API.
//! Authentication reuses the TCP server's SecurityManager (salted SHA-256,
//! constant-time compare, per-IP lockout) and issues bearer tokens with a
//! 12-hour TTL. Binds to the same interface as the TCP server (loopback by
//! default) — the panel inherits that exposure, keep it local or put TLS in
//! front of it for remote access (documented in the README manual).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};

use crate::dna_ql::{DnaQlEngine, DnaQlRow};
use crate::engine::Engine;
use crate::relational::Value as V;
use crate::server::SecurityManager;
use crate::sql_engine::{QueryResult, SqlEngine};

const ADMIN_HTML: &str = include_str!("admin.html");
const TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_BODY: usize = 4 * 1024 * 1024; // 4 MiB request bodies

pub struct AdminPanel {
    engine: Arc<Engine>,
    sql: Arc<SqlEngine>,
    dnaql: Arc<DnaQlEngine>,
    security: Arc<SecurityManager>,
    /// token -> (user, issued_at)
    tokens: Mutex<HashMap<String, (String, Instant)>>,
    /// monotonically increasing nonce for token derivation
    counter: std::sync::atomic::AtomicU64,
}

/// Spawn the admin panel on its own thread. Bind failures are logged and
/// non-fatal (the TCP server keeps serving) — an occupied admin port must
/// never take the database down.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    bind: IpAddr,
    port: u16,
    engine: Arc<Engine>,
    sql: Arc<SqlEngine>,
    dnaql: Arc<DnaQlEngine>,
    admin_user: &str,
    admin_pass: &str,
) {
    let listener = match TcpListener::bind((bind, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ADMIN] cannot bind admin panel on {bind}:{port}: {e} (panel disabled)");
            return;
        }
    };
    let panel = Arc::new(AdminPanel {
        engine,
        sql,
        dnaql,
        security: Arc::new(SecurityManager::new(admin_user, admin_pass)),
        tokens: Mutex::new(HashMap::new()),
        counter: std::sync::atomic::AtomicU64::new(0),
    });
    eprintln!("[ADMIN] web panel ready on http://{bind}:{port}/ (user: {admin_user})");
    std::thread::Builder::new()
        .name("dnadb-admin".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let panel = panel.clone();
                std::thread::spawn(move || {
                    let _ = panel.handle_connection(stream);
                });
            }
        })
        .expect("spawn admin thread");
}

impl AdminPanel {
    fn handle_connection(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_nodelay(true);
        let peer_ip = stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "unknown".into());

        let Some(req) = read_request(&mut stream)? else {
            return Ok(());
        };
        let response = self.route(&peer_ip, &req);
        write_response(&mut stream, &response)
    }

    fn route(&self, ip: &str, req: &Request) -> Response {
        // Unauthenticated: the SPA shell and login.
        if req.method == "GET" && (req.path == "/" || req.path.is_empty()) {
            return Response::html(ADMIN_HTML);
        }
        if req.method == "POST" && req.path == "/api/login" {
            return self.api_login(ip, req);
        }
        // Everything else requires a bearer token.
        let Some(token) = req.bearer() else {
            return Response::json(401, &json!({"error": "not authenticated"}));
        };
        if !self.valid_token(&token) {
            return Response::json(401, &json!({"error": "invalid or expired token"}));
        }

        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/health") => self.api_health(),
            ("GET", "/api/tables") => self.api_tables(),
            ("GET", "/api/browse") => self.api_browse(req),
            ("POST", "/api/query") => self.api_query(req),
            _ => Response::json(404, &json!({"error": format!("no route {}", req.path)})),
        }
    }

    // ---------------- API handlers ----------------

    fn api_login(&self, ip: &str, req: &Request) -> Response {
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let pass = body["pass"].as_str().unwrap_or("");
        match self.security.verify_credentials(ip, user, pass) {
            Ok(true) => {
                let token = self.issue_token(user);
                Response::json(200, &json!({"token": token, "user": user}))
            }
            Ok(false) => Response::json(401, &json!({"error": "invalid credentials"})),
            Err(msg) => Response::json(423, &json!({"error": msg})),
        }
    }

    fn api_health(&self) -> Response {
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
            }),
        )
    }

    fn api_tables(&self) -> Response {
        let tables: Vec<J> = self
            .sql
            .list_tables()
            .into_iter()
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

    fn api_browse(&self, req: &Request) -> Response {
        let name = match req.query_param("table") {
            Some(n) => n,
            None => return Response::json(400, &json!({"error": "missing ?table="})),
        };
        if !is_safe_ident(&name) {
            return Response::json(400, &json!({"error": "invalid table name"}));
        }
        if self.sql.get_schema(&name).is_none() {
            return Response::json(404, &json!({"error": format!("table '{}' not found", name)}));
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

        let page = match self
            .sql
            .execute(&format!("SELECT * FROM {} LIMIT {} OFFSET {}", name, limit, offset))
        {
            Ok(r) => r,
            Err(e) => return Response::json(500, &json!({"error": e})),
        };
        let total = match self.sql.execute(&format!("SELECT COUNT(*) AS __c FROM {}", name)) {
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

    fn api_query(&self, req: &Request) -> Response {
        let body = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let query = body["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Response::json(400, &json!({"error": "empty query"}));
        }
        let kind = body["kind"].as_str().unwrap_or("sql");
        match kind {
            "dnaql" => match self.dnaql.execute(&query) {
                Ok(res) => {
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
                Err(e) => Response::json(400, &json!({"error": e})),
            },
            _ => match self.sql.execute(&query) {
                Ok(res) => Response::json(200, &sql_result_json(&res)),
                Err(e) => Response::json(400, &json!({"error": e})),
            },
        }
    }

    // ---------------- tokens ----------------

    fn issue_token(&self, user: &str) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(n.to_le_bytes());
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(user.as_bytes());
        let token = hex(&hasher.finalize());
        self.tokens
            .lock()
            .insert(token.clone(), (user.to_string(), Instant::now()));
        token
    }

    fn valid_token(&self, token: &str) -> bool {
        let mut tokens = self.tokens.lock();
        // Opportunistic expiry sweep on every authenticated request.
        let now = Instant::now();
        tokens.retain(|_, (_, issued)| now.duration_since(*issued) < TOKEN_TTL);
        tokens.contains_key(token)
    }
}

// ---------------- HTTP plumbing ----------------

struct Request {
    method: String,
    path: String,
    /// raw `?a=b&c=d` query string, if any
    query: Option<String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn bearer(&self) -> Option<String> {
        let auth = self.headers.get("authorization")?;
        auth.strip_prefix("Bearer ").map(|t| t.trim().to_string())
    }

    fn query_param(&self, key: &str) -> Option<String> {
        let q = self.query.as_ref()?;
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == key {
                    return Some(percent_decode(v));
                }
            }
        }
        None
    }

    fn json_body(&self) -> Option<J> {
        serde_json::from_slice(&self.body).ok()
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    // Read until end of headers.
    let header_end = loop {
        if let Some(pos) = find_headers_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "headers too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target, None),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(Some(Request { method, path, query, headers, body }))
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Response {
    fn json(status: u16, value: &J) -> Self {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: value.to_string(),
        }
    }

    fn html(body: &str) -> Self {
        Response { status: 200, content_type: "text/html; charset=utf-8", body: body.to_string() }
    }
}

fn write_response(stream: &mut TcpStream, res: &Response) -> std::io::Result<()> {
    let reason = match res.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        423 => "Locked",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        res.status,
        reason,
        res.content_type,
        res.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(res.body.as_bytes())?;
    stream.flush()
}

// ---------------- JSON helpers ----------------

fn value_json(v: &V) -> J {
    match v {
        V::Null => J::Null,
        V::Int(i) => json!(i),
        V::Float(f) if f.is_finite() => json!(f),
        V::Float(_) => J::Null,
        V::Text(s) | V::Dna(s) => json!(s),
    }
}

fn rows_json(rows: &[std::collections::BTreeMap<String, V>]) -> Vec<J> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (k, v) in row {
                obj.insert(k.clone(), value_json(v));
            }
            J::Object(obj)
        })
        .collect()
}

fn dnaql_rows_json(rows: &[DnaQlRow]) -> Vec<J> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (k, v) in &row.fields {
                obj.insert(k.clone(), value_json(v));
            }
            J::Object(obj)
        })
        .collect()
}

fn sql_result_json(res: &QueryResult) -> J {
    json!({
        "kind": "sql",
        "columns": res.columns,
        "rows": rows_json(&res.rows),
        "plan": res.execution_plan,
        "time_us": res.execution_time_us,
        "message": res.message,
    })
}

fn is_safe_ident(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::wal::SyncMode;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dnadb_admin_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn start_panel() -> (u16, Arc<SqlEngine>) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("panel"),
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = Arc::new(SqlEngine::new(engine.clone()));
        sql.execute("CREATE TABLE chr_t (id INT, name TEXT, PRIMARY KEY (id))").unwrap();
        sql.execute("INSERT INTO chr_t (id = 1, name = 'alpha')").unwrap();
        sql.execute("INSERT INTO chr_t (id = 2, name = 'beta')").unwrap();
        let dnaql = Arc::new(DnaQlEngine::new(engine.clone()));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let panel = Arc::new(AdminPanel {
            engine,
            sql: sql.clone(),
            dnaql,
            security: Arc::new(SecurityManager::new("admin", "s3cret-panel")),
            tokens: Mutex::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
        });
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let panel = panel.clone();
                std::thread::spawn(move || {
                    let _ = panel.handle_connection(stream);
                });
            }
        });
        (port, sql)
    }

    /// Raw HTTP/1.1 round-trip against the panel (real socket, real server).
    fn http(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!("{} {} HTTP/1.1\r\nHost: localhost\r\n", method, path);
        if let Some(t) = token {
            req.push_str(&format!("Authorization: Bearer {}\r\n", t));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).to_string();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        (status, body)
    }

    #[test]
    fn panel_requires_auth_and_serves_api() {
        let (port, _sql) = start_panel();

        // SPA shell is public.
        let (status, body) = http(port, "GET", "/", None, "");
        assert_eq!(status, 200);
        assert!(body.contains("DNA-DB"), "shell must contain the SPA");

        // API without token -> 401.
        let (status, _) = http(port, "GET", "/api/tables", None, "");
        assert_eq!(status, 401);

        // Wrong password -> 401.
        let (status, _) =
            http(port, "POST", "/api/login", None, r#"{"user":"admin","pass":"nope"}"#);
        assert_eq!(status, 401);

        // Correct password -> token.
        let (status, body) =
            http(port, "POST", "/api/login", None, r#"{"user":"admin","pass":"s3cret-panel"}"#);
        assert_eq!(status, 200);
        let token: J = serde_json::from_str(&body).unwrap();
        let token = token["token"].as_str().unwrap().to_string();

        // Tables list contains our table.
        let (status, body) = http(port, "GET", "/api/tables", Some(&token), "");
        assert_eq!(status, 200);
        assert!(body.contains("chr_t"));

        // Browse paginates and counts.
        let (status, body) = http(port, "GET", "/api/browse?table=chr_t&limit=1", Some(&token), "");
        assert_eq!(status, 200);
        let v: J = serde_json::from_str(&body).unwrap();
        assert_eq!(v["total"], json!(2));
        assert_eq!(v["rows"].as_array().unwrap().len(), 1);

        // SQL console works through HTTP.
        let (status, body) = http(
            port,
            "POST",
            "/api/query",
            Some(&token),
            r#"{"kind":"sql","query":"SELECT name FROM chr_t ORDER BY id"}"#,
        );
        assert_eq!(status, 200);
        let v: J = serde_json::from_str(&body).unwrap();
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert_eq!(v["rows"][0]["name"], json!("alpha"));

        // DnaQL console too.
        let (status, body) = http(
            port,
            "POST",
            "/api/query",
            Some(&token),
            r#"{"kind":"dnaql","query":"ANALYZE SEQUENCE 'ACGTACGT' |> GC_CONTENT()"}"#,
        );
        assert_eq!(status, 200);
        let v: J = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["rows"][0]["result"],
            json!("50.00%"),
            "GC of ACGTACGT must be 50%: {body}"
        );

        // Health endpoint reports segment stats.
        let (status, body) = http(port, "GET", "/api/health", Some(&token), "");
        assert_eq!(status, 200);
        let v: J = serde_json::from_str(&body).unwrap();
        assert!(v["health"].is_object());
        assert!(v["stats"].is_object());

        // Table name injection is rejected before any SQL is built.
        let (status, _) = http(
            port,
            "GET",
            "/api/browse?table=chr_t%20OR%201%3D1",
            Some(&token),
            "",
        );
        assert_eq!(status, 400);
    }
}
