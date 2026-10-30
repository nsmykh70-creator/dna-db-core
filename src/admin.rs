// SPDX-License-Identifier: Apache-2.0
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
use std::net::{IpAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value as J};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::dna_ql::{DnaQlEngine, DnaQlRow};
use crate::engine::Engine;
use crate::relational::Value as V;
use crate::server::{AuthError, Connection, SecurityManager};
use crate::sql_engine::{QueryResult, SqlEngine};

const ADMIN_HTML: &str = include_str!("admin.html");
const TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_BODY: usize = 4 * 1024 * 1024; // 4 MiB request bodies

#[derive(Clone)]
struct Session {
    user: String,
    role: String,
    allowed_tables: Option<Vec<String>>,
    must_change: bool,
    issued: Instant,
}

/// Authentication delegated to a reverse proxy (oauth2-proxy, Authentik,
/// nginx `auth_request`, ...). The proxy terminates the user-facing login,
/// strips any client-supplied identity header, and injects the authenticated
/// username. Only requests from an explicitly trusted peer IP are accepted;
/// an optional shared secret makes a misconfigured proxy fail closed.
#[derive(Clone, Debug)]
pub struct ProxyAuthConfig {
    pub trusted_ips: Vec<IpAddr>,
    pub user_header: String,
    pub secret_header: String,
    pub secret: Option<String>,
}

pub struct AdminPanel {
    engine: Arc<Engine>,
    sql: Arc<SqlEngine>,
    dnaql: Arc<DnaQlEngine>,
    security: Arc<SecurityManager>,
    tls: Option<Arc<rustls::ServerConfig>>,
    proxy: Option<ProxyAuthConfig>,
    /// token -> session
    tokens: Mutex<HashMap<String, Session>>,
    active: Arc<AtomicUsize>,
}

const MAX_ADMIN_CONNECTIONS: usize = 64;
const ADMIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn the admin panel on its own thread. Bind failures are logged and
/// non-fatal (the TCP server keeps serving) — an occupied admin port must
/// never take the database down.
pub fn spawn(
    bind: IpAddr,
    port: u16,
    engine: Arc<Engine>,
    sql: Arc<SqlEngine>,
    dnaql: Arc<DnaQlEngine>,
    security: Arc<SecurityManager>,
    tls: Option<Arc<rustls::ServerConfig>>,
    proxy: Option<ProxyAuthConfig>,
) {
    let listener = match TcpListener::bind((bind, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[ADMIN] cannot bind admin panel on {bind}:{port}: {e} (panel disabled)");
            return;
        }
    };
    let secure = tls.is_some();
    let panel = Arc::new(AdminPanel {
        engine,
        sql,
        dnaql,
        security,
        tls,
        proxy,
        tokens: Mutex::new(HashMap::new()),
        active: Arc::new(AtomicUsize::new(0)),
    });
    eprintln!(
        "[ADMIN] web panel ready on {}://{bind}:{port}/ (multi-user Argon2id auth{})",
        if secure { "https" } else { "http" },
        if secure {
            ", TLS"
        } else {
            ", WARNING: cleartext"
        }
    );
    std::thread::Builder::new()
        .name("dnadb-admin".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if panel.active.fetch_add(1, Ordering::AcqRel) >= MAX_ADMIN_CONNECTIONS {
                    panel.active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let panel = panel.clone();
                std::thread::spawn(move || {
                    struct Guard(Arc<AtomicUsize>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _guard = Guard(panel.active.clone());
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_read_timeout(Some(ADMIN_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(ADMIN_TIMEOUT));
                    let tls = panel.tls.clone();
                    let result = match tls {
                        Some(config) => match crate::tls::accept(config, stream) {
                            Ok(stream) => panel.handle_connection(Box::new(stream)),
                            Err(e) => Err(e),
                        },
                        None => panel.handle_connection(Box::new(stream)),
                    };
                    if let Err(e) = result {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof
                            && e.kind() != std::io::ErrorKind::ConnectionReset
                            && e.kind() != std::io::ErrorKind::BrokenPipe
                            && e.kind() != std::io::ErrorKind::TimedOut
                        {
                            eprintln!("[ADMIN] connection error: {e}");
                        }
                    }
                });
            }
        })
        .expect("spawn admin thread");
}

impl AdminPanel {
    fn handle_connection(&self, mut stream: Box<dyn Connection>) -> std::io::Result<()> {
        let _ = stream.set_read_timeout(Some(ADMIN_TIMEOUT));
        let _ = stream.set_write_timeout(Some(ADMIN_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let peer_ip = match stream.peer_ip() {
            Some(ip) => ip,
            None => "unknown".into(),
        };

        let Some(req) = read_request(&mut *stream)? else {
            return Ok(());
        };
        let response = self.route(&peer_ip, &req);
        write_response(&mut *stream, &response, self.tls.is_some())
    }

    fn route(&self, ip: &str, req: &Request) -> Response {
        // Unauthenticated: the SPA shell and login.
        if req.method == "GET" && (req.path == "/" || req.path.is_empty()) {
            return Response::html(ADMIN_HTML);
        }
        if req.method == "POST" && req.path == "/api/login" {
            return self.api_login(ip, req);
        }
        if req.method == "POST" && req.path == "/api/login/proxy" {
            return self.api_login_proxy(ip, req);
        }
        if req.method == "GET" && req.path == "/api/csrf" {
            return Response::json(200, &json!({"token": "not-required-bearer-only"}));
        }
        // Everything else requires a bearer token.
        let Some(token) = req.bearer() else {
            return Response::json(401, &json!({"error": "not authenticated"}));
        };
        let Some(session) = self.session(&token) else {
            return Response::json(401, &json!({"error": "invalid or expired token"}));
        };
        if session.must_change
            && req.path != "/api/password"
            && req.path != "/api/logout"
            && req.path != "/api/me"
        {
            return Response::json(
                403,
                &json!({"error": "password_change_required", "must_change": true}),
            );
        }

        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/api/health") => self.api_health(),
            ("GET", "/api/tables") => self.api_tables(&session),
            ("GET", "/api/browse") => self.api_browse(req, &session),
            ("POST", "/api/query") => self.api_query(&session, req),
            ("GET", "/api/me") => Response::json(
                200,
                &json!({
                    "user": session.user,
                    "role": session.role,
                    "allowed_tables": session.allowed_tables,
                    "must_change": session.must_change,
                }),
            ),
            ("POST", "/api/logout") => {
                self.tokens.lock().remove(&token);
                Response::json(200, &json!({"ok": true}))
            }
            ("POST", "/api/password") => self.api_change_password(&session, req),
            ("GET", "/api/users") => self.api_list_users(&session),
            ("POST", "/api/users") => self.api_create_user(&session, req),
            ("POST", "/api/users/password") => self.api_reset_password(&session, req),
            ("POST", "/api/users/disable") => self.api_disable_user(&session, req),
            ("POST", "/api/users/access") => self.api_set_access(&session, req),
            ("POST", "/api/users/delete") => self.api_delete_user(&session, req),
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
            Ok(identity) => {
                let token = self.issue_token(&identity);
                Response::json(
                    200,
                    &json!({
                        "token": token,
                        "user": identity.username,
                        "role": identity.role,
                        "must_change": identity.must_change,
                    }),
                )
            }
            Err(AuthError::InvalidCredentials) => {
                Response::json(401, &json!({"error": "invalid credentials"}))
            }
            Err(AuthError::Locked(msg)) | Err(AuthError::Internal(msg)) => {
                Response::json(423, &json!({"error": msg}))
            }
        }
    }

    fn api_login_proxy(&self, ip: &str, req: &Request) -> Response {
        let Some(config) = &self.proxy else {
            return Response::json(403, &json!({"error": "trusted proxy authentication is not enabled"}));
        };
        let peer = match ip.parse::<IpAddr>() {
            Ok(peer) => normalize_ip(peer),
            Err(_) => {
                return Response::json(403, &json!({"error": "untrusted proxy peer"}));
            }
        };
        if !config
            .trusted_ips
            .iter()
            .any(|trusted| normalize_ip(*trusted) == peer)
        {
            return Response::json(403, &json!({"error": "untrusted proxy peer"}));
        }
        if let Some(expected) = &config.secret {
            let supplied = req
                .headers
                .get(&config.secret_header.to_ascii_lowercase())
                .map(String::as_str)
                .unwrap_or("");
            if !secret_matches(expected, supplied) {
                return Response::json(403, &json!({"error": "invalid proxy authentication secret"}));
            }
        }
        let user = req
            .headers
            .get(&config.user_header.to_ascii_lowercase())
            .map(|value| value.trim())
            .unwrap_or("");
        if user.is_empty() {
            return Response::json(401, &json!({"error": "proxy did not supply an authenticated user"}));
        }
        match self.security.verify_trusted_identity(user) {
            Ok(identity) => {
                let token = self.issue_token(&identity);
                eprintln!(
                    "[ADMIN] trusted-proxy login user={} peer={}",
                    identity.username, peer
                );
                Response::json(
                    200,
                    &json!({
                        "token": token,
                        "user": identity.username,
                        "role": identity.role,
                        "allowed_tables": identity.allowed_tables,
                        "must_change": false,
                    }),
                )
            }
            Err(_) => Response::json(401, &json!({"error": "proxy user is unknown or disabled"})),
        }
    }

    fn require_admin(&self, session: &Session) -> Option<Response> {
        if session.role == "admin" && self.security.user_is_admin(&session.user) {
            None
        } else {
            Some(Response::json(
                403,
                &json!({"error": "administrator role required"}),
            ))
        }
    }

    fn api_change_password(&self, session: &Session, req: &Request) -> Response {
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let old_pass = body["old_password"].as_str().unwrap_or("");
        let new_pass = body["new_password"].as_str().unwrap_or("");
        match self
            .security
            .users
            .change_password(&session.user, old_pass, new_pass)
        {
            Ok(()) => {
                self.revoke_user_tokens(&session.user);
                Response::json(200, &json!({"ok": true, "re_login": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    fn api_list_users(&self, session: &Session) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        Response::json(200, &json!({"users": self.security.list_users()}))
    }

    fn api_create_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let pass = body["pass"].as_str().unwrap_or("");
        let role = body["role"].as_str().unwrap_or("viewer");
        let must_change = body["must_change"].as_bool().unwrap_or(true);
        let allowed_tables = match body.get("allowed_tables") {
            None | Some(J::Null) => None,
            Some(J::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
            ),
            Some(J::String(csv)) => match crate::access::normalize_csv(csv) {
                Ok(tables) => tables,
                Err(e) => return Response::json(400, &json!({"error": e})),
            },
            Some(_) => {
                return Response::json(
                    400,
                    &json!({"error": "allowed_tables must be an array of strings"}),
                );
            }
        };
        match self
            .security
            .users
            .create_user_with_tables(user, pass, role, must_change, allowed_tables)
        {
            Ok(()) => Response::json(201, &json!({"ok": true, "user": user, "role": role})),
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    fn api_set_access(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let allowed_tables = match body.get("allowed_tables") {
            None | Some(J::Null) => None,
            Some(J::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
            ),
            Some(J::String(csv)) => match crate::access::normalize_csv(csv) {
                Ok(tables) => tables,
                Err(e) => return Response::json(400, &json!({"error": e})),
            },
            Some(_) => {
                return Response::json(
                    400,
                    &json!({"error": "allowed_tables must be an array of strings"}),
                );
            }
        };
        match self.security.users.set_allowed_tables(user, allowed_tables) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    fn api_reset_password(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let new_pass = body["new_password"].as_str().unwrap_or("");
        match self.security.users.reset_password(user, new_pass) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    fn api_disable_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        let disabled = body["disabled"].as_bool().unwrap_or(false);
        match self.security.users.set_disabled(user, disabled) {
            Ok(()) => {
                if disabled {
                    self.revoke_user_tokens(user);
                }
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
        }
    }

    fn api_delete_user(&self, session: &Session, req: &Request) -> Response {
        if let Some(response) = self.require_admin(session) {
            return response;
        }
        let body: J = match req.json_body() {
            Some(b) => b,
            None => return Response::json(400, &json!({"error": "invalid JSON body"})),
        };
        let user = body["user"].as_str().unwrap_or("");
        match self.security.users.delete_user(user) {
            Ok(()) => {
                self.revoke_user_tokens(user);
                Response::json(200, &json!({"ok": true}))
            }
            Err(e) => Response::json(400, &json!({"error": e})),
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

    fn api_tables(&self, session: &Session) -> Response {
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

    fn api_browse(&self, req: &Request, session: &Session) -> Response {
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

    fn api_query(&self, session: &Session, req: &Request) -> Response {
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

    fn issue_token(&self, identity: &crate::server::AuthenticatedUser) -> String {
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let token = hex(&raw);
        self.tokens.lock().insert(
            token.clone(),
            Session {
                user: identity.username.clone(),
                role: identity.role.clone(),
                allowed_tables: identity.allowed_tables.clone(),
                must_change: identity.must_change,
                issued: Instant::now(),
            },
        );
        token
    }

    fn session(&self, token: &str) -> Option<Session> {
        let mut tokens = self.tokens.lock();
        let now = Instant::now();
        tokens.retain(|_, session| now.duration_since(session.issued) < TOKEN_TTL);
        tokens.get(token).cloned()
    }

    fn revoke_user_tokens(&self, user: &str) {
        self.tokens.lock().retain(|_, session| session.user != user);
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

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        IpAddr::V4(_) => ip,
    }
}

fn secret_matches(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len()
        && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}

fn read_request<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    // Read until end of headers.
    let header_end = loop {
        if let Some(pos) = find_headers_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            ));
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
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
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

    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
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
        Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.to_string(),
        }
    }
}

fn write_response<W: Write + ?Sized>(
    stream: &mut W,
    res: &Response,
    tls: bool,
) -> std::io::Result<()> {
    let reason = match res.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        423 => "Locked",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let script_hashes = if res.content_type.starts_with("text/html") {
        inline_script_hashes(&res.body)
    } else {
        Vec::new()
    };
    let script_src = if script_hashes.is_empty() {
        "'self'".to_string()
    } else {
        format!("'self' {}", script_hashes.join(" "))
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\nPermissions-Policy: camera=(), microphone=(), geolocation=()\r\n\
         Cache-Control: no-store\r\nContent-Security-Policy: default-src 'self'; \
         script-src {}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'self'; object-src 'none'\r\n\
         Connection: close\r\n",
        res.status,
        reason,
        res.content_type,
        res.body.len(),
        script_src
    );
    if tls {
        head.push_str("Strict-Transport-Security: max-age=31536000; includeSubDomains\r\n");
    }
    head.push_str("\r\n");
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
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// CSP hashes for inline `<script>` blocks. Hash-based allowlisting keeps
/// `script-src` free of `'unsafe-inline'` even though the SPA ships as one
/// embedded HTML document.
fn inline_script_hashes(body: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("<script>") {
        let after = &rest[open + "<script>".len()..];
        let Some(close) = after.find("</script>") else {
            break;
        };
        let after_bytes = after.as_bytes();
        let digest = Sha256::digest(&after_bytes[..close]);
        hashes.push(format!("'sha256-{}'", base64_encode(&digest)));
        rest = &after[close + "</script>".len()..];
    }
    hashes
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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
    use std::net::TcpStream;

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
        sql.execute("CREATE TABLE chr_t (id INT, name TEXT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("INSERT INTO chr_t (id = 1, name = 'alpha')")
            .unwrap();
        sql.execute("INSERT INTO chr_t (id = 2, name = 'beta')")
            .unwrap();
        let dnaql = Arc::new(DnaQlEngine::new(engine.clone(), sql.clone()));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let panel = Arc::new(AdminPanel {
            engine,
            sql: sql.clone(),
            dnaql,
            security: Arc::new(SecurityManager::new("admin", "s3cret-panel")),
            tls: None,
            proxy: None,
            tokens: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicUsize::new(0)),
        });
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let panel = panel.clone();
                std::thread::spawn(move || {
                    let _ = panel.handle_connection(Box::new(stream));
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
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
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
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
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
        let (status, _) = http(
            port,
            "POST",
            "/api/login",
            None,
            r#"{"user":"admin","pass":"nope"}"#,
        );
        assert_eq!(status, 401);

        // Correct password -> token.
        let (status, body) = http(
            port,
            "POST",
            "/api/login",
            None,
            r#"{"user":"admin","pass":"s3cret-panel"}"#,
        );
        assert_eq!(status, 200);
        let token: J = serde_json::from_str(&body).unwrap();
        let token = token["token"].as_str().unwrap().to_string();

        // Tables list contains our table.
        let (status, body) = http(port, "GET", "/api/tables", Some(&token), "");
        assert_eq!(status, 200);
        assert!(body.contains("chr_t"));

        // Browse paginates and counts.
        let (status, body) = http(
            port,
            "GET",
            "/api/browse?table=chr_t&limit=1",
            Some(&token),
            "",
        );
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
