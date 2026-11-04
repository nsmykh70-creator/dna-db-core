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
}

impl AdminPanel {
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

mod api_data;
mod api_users;
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
