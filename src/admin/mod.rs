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
use std::net::{IpAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value as J};

use crate::dna_ql::DnaQlEngine;
use crate::engine::Engine;
use crate::relational::Value as V;
use crate::server::{AuthError, Connection, SecurityManager};
use crate::sql_engine::SqlEngine;

const ADMIN_HTML: &str = include_str!("../admin.html");
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
    /// CMS edition (branch cms-edition): MySQL-dialect gate for the SQL
    /// console and /api/query kind=sql (sessions, shims, errno mapping).
    #[cfg(feature = "cms")]
    pub cms: crate::CmsGateOpt,
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
    cms: crate::CmsGateOpt,
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
        #[cfg(feature = "cms")]
        cms,
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
        // v4.0: Prometheus scrape endpoint — intentionally UNAUTHENTICATED
        // but bound to the admin interface only (loopback by default), the
        // standard deployment shape for the metrics port behind a firewall.
        if req.method == "GET" && req.path == "/metrics" {
            return self.prometheus_metrics();
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
            ("POST", "/api/explain") => self.api_explain(&session, req),
            ("POST", "/api/compact") => self.api_compact(&session, req),
            ("GET", "/api/compactions") => self.api_compactions(&session),
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

    /// v4.0: Prometheus text format (0.0.4) endpoint. Real counters from the
    /// engine — no fabricated values.
    fn prometheus_metrics(&self) -> Response {
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
                "data_dir_bytes": self.engine.dir_size_bytes(),
            }),
        )
    }

    /// v4.1: SQL → DnaQL translation plus the physical storage map for a
    /// statement the operator is about to run — the panel-side counterpart of
    /// the CLI `sql --to-dnaql` mode. Read-only: nothing is executed.
    fn api_explain(&self, session: &Session, req: &Request) -> Response {
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
    fn api_compact(&self, session: &Session, req: &Request) -> Response {
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
    fn api_compactions(&self, session: &Session) -> Response {
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

mod http;
mod json;
#[cfg(test)]
mod tests;

pub(crate) use http::{
    normalize_ip, read_request, secret_matches, write_response, Request, Response,
};
#[cfg(feature = "cms")]
pub(crate) use json::cms_result_json;
pub(crate) use json::{dnaql_rows_json, hex, is_safe_ident, rows_json, sql_result_json};
