// SPDX-License-Identifier: AGPL-3.0-only
//! Embedded web panel for the CMS edition `mysqld` mode — databases and
//! spiral storage management (`serve`-mode AdminPanel remains the
//! phpMyAdmin-style table console).
//!
//! Zero new dependencies: a minimal HTTP/1.1 server over std TcpListener
//! serving an embedded single-page app (`panel.html`) and a JSON API:
//!
//! * `POST /api/login` — `{user, pass}` → bearer token (12 h TTL). Reuses
//!   the wire listener's `--user USER:PASS` map; trust mode (no users)
//!   accepts any login with a startup warning, like the wire itself.
//!   5 failed logins per IP per minute → 429.
//! * `GET  /api/databases` — every database with its dual-spiral pair.
//! * `POST /api/databases` — `{name, strand_a:{label|path,kind},
//!   strand_b:{…}}` → creates the pair (two real spirals on the chosen
//!   storage) and registers the catalog name.
//! * `DELETE /api/databases/{name}` — drops both spirals + catalog record.
//! * `POST /api/databases/{name}/relocate` — `{role:"A"|"B", label|path,
//!   kind}` → moves a spiral to another storage (copy → verify → swap).
//! * `GET  /api/storage` — registered roots + writable local-disk/network
//!   candidates detected from `/proc/mounts`.
//! * `POST /api/storage` / `DELETE /api/storage/{label}` — manage roots.
//! * `GET  /metrics` — Prometheus text format 0.0.4: wire counters
//!   (sessions, queries, errors, uptime) + spiral-catalog gauges
//!   (databases, spirals, storage roots, per-root placement). Read-only
//!   and unauthenticated like every scrape target; the panel binds
//!   loopback by default, so expose it to the scraper deliberately.
//!
//! Bind is loopback by default (`--admin-bind`); put TLS in front for
//! remote access. Everything except `GET /`, `POST /api/login` and
//! `GET /metrics` requires the bearer token.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as J};

use crate::cms::spirals::{StorageKind, StorageRoot};
use crate::cms::{CmsSqlGate, MySqlWireStats};

const PANEL_HTML: &str = include_str!("panel.html");
const MAX_BODY: usize = 256 * 1024;
const TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

type Users = Arc<HashMap<String, String>>;

pub struct CmsPanel {
    gate: Arc<CmsSqlGate>,
    stats: Arc<MySqlWireStats>,
    users: Users,
    sessions: Mutex<HashMap<String, (String, Instant)>>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl CmsPanel {
    /// Accept loop. Runs forever; the caller spawns it on a thread.
    pub fn run(
        listener: TcpListener,
        gate: Arc<CmsSqlGate>,
        stats: Arc<MySqlWireStats>,
        users: Users,
    ) {
        let panel = Arc::new(CmsPanel {
            gate,
            stats,
            users,
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        });
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let p = panel.clone();
                    std::thread::spawn(move || {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
                        p.handle(s);
                    });
                }
                Err(e) => eprintln!("[PANEL] accept error: {e}"),
            }
        }
    }

    fn handle(&self, mut stream: TcpStream) {
        let Some(req) = read_request(&mut stream) else {
            return;
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.ip())
            .unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
        let (method, path) = (req.method.clone(), req.path.clone());
        let mut segments = path.split('/').filter(|s| !s.is_empty());
        let _root = segments.next(); // "api"
        let area = segments.next().unwrap_or("").to_string();
        let tail = segments.next().unwrap_or("").to_string();
        let tail2 = segments.next().unwrap_or("").to_string();

        // Static UI + login are the only unauthenticated routes.
        if method == "GET" && (path == "/" || path == "/panel.html") {
            return respond(&mut stream, 200, "text/html; charset=utf-8", PANEL_HTML);
        }
        if method == "POST" && area == "login" {
            let out = self.api_login(&req.body, peer);
            return respond_json(&mut stream, out.0, &out.1);
        }
        // Scrape target: read-only, no bearer token (Prometheus cannot do
        // the login dance); content-type is the 0.0.4 text format.
        if method == "GET" && path == "/metrics" {
            return respond(
                &mut stream,
                200,
                METRICS_CONTENT_TYPE,
                &self.render_metrics(),
            );
        }

        // Everything below needs a valid bearer token.
        if !self.authorized(req.header("authorization")) {
            return respond_json(
                &mut stream,
                401,
                &json!({"error": "unauthorized: POST /api/login first"}),
            );
        }

        let reply = match (method.as_str(), area.as_str(), tail.as_str(), tail2.as_str()) {
            ("GET", "status", _, _) => self.api_status(),
            ("GET", "databases", _, _) => self.api_list_databases(),
            ("POST", "databases", "", _) => self.api_create_database(&req.body),
            ("DELETE", "databases", name, "") => self.api_drop_database(&percent_decode(name)),
            ("POST", "databases", name, "relocate") => {
                self.api_relocate(&percent_decode(name), &req.body)
            }
            ("GET", "storage", _, _) => self.api_list_storage(),
            ("POST", "storage", "", _) => self.api_add_root(&req.body),
            ("DELETE", "storage", label, "") => self.api_remove_root(&percent_decode(label)),
            _ => (404, json!({"error": format!("no route {method} {path}")})),
        };
        respond_json(&mut stream, reply.0, &reply.1);
    }

    // ----------------------------- auth -----------------------------

    fn api_login(&self, body: &str, peer: IpAddr) -> (u16, J) {
        // Brute-force lockout per IP.
        {
            let mut f = self.failures.lock().expect("failures poisoned");
            if let Some((n, until)) = f.get(&peer) {
                if *n >= MAX_FAILURES && Instant::now() < *until {
                    return (
                        429,
                        json!({"error": "too many failed logins; wait a minute"}),
                    );
                }
            }
        }
        let Ok(v) = serde_json::from_str::<J>(body) else {
            return self.record_failure(peer, 400, "invalid JSON body");
        };
        let user = v["user"].as_str().unwrap_or("").trim().to_string();
        let pass = v["pass"].as_str().unwrap_or("").to_string();
        if user.is_empty() {
            return self.record_failure(peer, 400, "user required");
        }
        let ok = if self.users.is_empty() {
            // Trust mode (wire runs --skip-grant-tables too).
            true
        } else {
            self.users
                .get(&user)
                .map(|expected| ct_eq(expected.as_bytes(), pass.as_bytes()))
                .unwrap_or(false)
        };
        if !ok {
            return self.record_failure(peer, 401, "bad credentials");
        }
        let token = new_token();
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .insert(token.clone(), (user.clone(), Instant::now()));
        (200, json!({"token": token, "user": user}))
    }

    fn record_failure(&self, peer: IpAddr, status: u16, msg: &str) -> (u16, J) {
        let mut f = self.failures.lock().expect("failures poisoned");
        let e = f.entry(peer).or_insert((0, Instant::now()));
        e.0 += 1;
        e.1 = Instant::now() + FAILURE_WINDOW;
        (status, json!({"error": msg}))
    }

    fn authorized(&self, authorization: Option<&str>) -> bool {
        let Some(tok) = authorization
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string())
        else {
            return false;
        };
        let mut s = self.sessions.lock().expect("sessions poisoned");
        match s.get(&tok) {
            Some((_, issued)) if issued.elapsed() < TOKEN_TTL => true,
            Some(_) => {
                s.remove(&tok);
                false
            }
            None => false,
        }
    }

    // ----------------------------- metrics -----------------------------

    /// Prometheus text format 0.0.4 body: wire counters plus the
    /// spiral-catalog gauges (databases / spirals / storage roots /
    /// per-root placement). Pure function of the stats + catalog, so the
    /// unit tests assert it without sockets.
    fn render_metrics(&self) -> String {
        let s = &self.stats;
        let (databases, spirals, roots, per_root) = self
            .gate
            .with_spirals(|c| {
                let pairs: Vec<&crate::cms::spirals::SpiralPair> = c.pairs().collect();
                let mut per_root: std::collections::BTreeMap<String, u64> =
                    std::collections::BTreeMap::new();
                for p in &pairs {
                    for strand in [&p.strand_a, &p.strand_b] {
                        let label = strand
                            .root_label
                            .clone()
                            .unwrap_or_else(|| "ad-hoc".to_string());
                        *per_root.entry(label).or_insert(0) += 1;
                    }
                }
                (pairs.len() as u64, pairs.len() as u64 * 2, c.roots().len() as u64, per_root)
            })
            .unwrap_or((0, 0, 0, std::collections::BTreeMap::new()));

        let mut out = String::with_capacity(2048);
        metric(
            &mut out,
            "dnadb_build_info",
            "Build metadata (value is always 1).",
            "gauge",
            &format!(
                "dnadb_build_info{{version=\"{}\",edition=\"cms\"}} 1",
                env!("CARGO_PKG_VERSION")
            ),
        );
        metric(
            &mut out,
            "dnadb_cms_wire_sessions_active",
            "Currently open MySQL-wire sessions.",
            "gauge",
            &format!(
                "dnadb_cms_wire_sessions_active {}",
                s.active.load(Ordering::Relaxed)
            ),
        );
        metric(
            &mut out,
            "dnadb_cms_wire_sessions_total",
            "MySQL-wire sessions opened since server start.",
            "counter",
            &format!(
                "dnadb_cms_wire_sessions_total {}",
                s.total_sessions.load(Ordering::Relaxed)
            ),
        );
        metric(
            &mut out,
            "dnadb_cms_wire_queries_total",
            "COM_QUERY / COM_STMT_EXECUTE statements executed.",
            "counter",
            &format!(
                "dnadb_cms_wire_queries_total {}",
                s.total_queries.load(Ordering::Relaxed)
            ),
        );
        metric(
            &mut out,
            "dnadb_cms_wire_errors_total",
            "Statements answered with an ERR packet.",
            "counter",
            &format!(
                "dnadb_cms_wire_errors_total {}",
                s.total_errors.load(Ordering::Relaxed)
            ),
        );
        metric(
            &mut out,
            "dnadb_cms_wire_uptime_seconds",
            "Seconds since the wire listener came up.",
            "gauge",
            &format!("dnadb_cms_wire_uptime_seconds {}", s.uptime_seconds()),
        );
        metric(
            &mut out,
            "dnadb_cms_databases_total",
            "Databases registered in the spiral catalog (each = Strand A + Strand B).",
            "gauge",
            &format!("dnadb_cms_databases_total {databases}"),
        );
        metric(
            &mut out,
            "dnadb_cms_spirals_total",
            "Spiral strands under management (2 per database).",
            "gauge",
            &format!("dnadb_cms_spirals_total {spirals}"),
        );
        metric(
            &mut out,
            "dnadb_cms_storage_roots_total",
            "Registered spiral storage roots (local disks / network mounts).",
            "gauge",
            &format!("dnadb_cms_storage_roots_total {roots}"),
        );
        metric(
            &mut out,
            "dnadb_cms_spirals_per_root",
            "Spiral strands placed on each storage root (label may be 'ad-hoc').",
            "gauge",
            &per_root
                .iter()
                .map(|(label, n)| {
                    format!(
                        "dnadb_cms_spirals_per_root{{root=\"{}\"}} {n}",
                        escape_label(label)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        out
    }

    // ----------------------------- data -----------------------------

    fn api_status(&self) -> (u16, J) {
        let (pairs, roots) = self
            .gate
            .with_spirals(|c| (c.pairs().count(), c.roots().len()))
            .unwrap_or((0, 0));
        (
            200,
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "edition": "cms",
                "databases": pairs,
                "storage_roots": roots,
                "engine": "DNA-DB (двойная спираль: Strand A + Strand B на каждую базу)",
            }),
        )
    }

    fn api_list_databases(&self) -> (u16, J) {
        let Some(view) = self.gate.with_spirals(|c| {
            let mut dbs: Vec<crate::cms::spirals::SpiralPair> = c.pairs().cloned().collect();
            dbs.sort_by(|a, b| a.database.cmp(&b.database));
            dbs
        }) else {
            return (500, json!({"error": "spiral catalog is not attached"}));
        };
        (
            200,
            json!({
                "default_database": "wordpress",
                "databases": view,
            }),
        )
    }

    /// `{name, strand_a:{label?,path?,kind?}, strand_b:{…}}`
    fn api_create_database(&self, body: &str) -> (u16, J) {
        let Ok(v) = serde_json::from_str::<J>(body) else {
            return (400, json!({"error": "invalid JSON body"}));
        };
        let Some(name) = v["name"].as_str().map(|s| s.trim().to_string()) else {
            return (400, json!({"error": "field 'name' is required"}));
        };
        let a = placement_from_json(&v["strand_a"]);
        let b = placement_from_json(&v["strand_b"]);
        match self.gate.panel_create_database(&name, a, b) {
            Ok(pair) => (200, json!({"ok": true, "pair": pair})),
            Err(e) => (400, json!({"error": e})),
        }
    }

    fn api_drop_database(&self, name: &str) -> (u16, J) {
        match self.gate.panel_drop_database(name) {
            Ok(()) => (200, json!({"ok": true, "dropped": name})),
            Err(e) => (400, json!({"error": e})),
        }
    }

    /// `{role:"A"|"B", label?|path?, kind?}`
    fn api_relocate(&self, name: &str, body: &str) -> (u16, J) {
        let Ok(v) = serde_json::from_str::<J>(body) else {
            return (400, json!({"error": "invalid JSON body"}));
        };
        let role = match v["role"].as_str().unwrap_or("").to_ascii_uppercase().as_str() {
            "A" => 'A',
            "B" => 'B',
            _ => return (400, json!({"error": "field 'role' must be \"A\" or \"B\""})),
        };
        let (label, path, kind) = {
            let p = &v;
            (
                p["label"].as_str().map(|s| s.trim().to_string()),
                p["path"].as_str().map(|s| s.trim().to_string()),
                p["kind"].as_str().and_then(StorageKind::parse),
            )
        };
        match self.gate.panel_relocate_spiral(
            name,
            role,
            label.as_deref(),
            path.as_deref(),
            kind,
        ) {
            Ok(p) => (200, json!({"ok": true, "placement": p})),
            Err(e) => (400, json!({"error": e})),
        }
    }

    fn api_list_storage(&self) -> (u16, J) {
        let Some((roots, candidates)) = self.gate.with_spirals(|c| {
            let roots = c.roots().to_vec();
            let candidates: Vec<(StorageRoot, bool)> =
                crate::cms::spirals::SpiralCatalog::local_candidates()
                    .into_iter()
                    .filter(|(root, writable)| {
                        // Only mounts that are not already registered roots.
                        *writable && !roots.iter().any(|r| r.path == root.path)
                    })
                    .collect();
            (roots, candidates)
        }) else {
            return (500, json!({"error": "spiral catalog is not attached"}));
        };
        let mut out = Vec::new();
        for (root, writable) in candidates {
            out.push(json!({
                "label": root.label,
                "path": root.path,
                "kind": root.kind.as_str(),
                "writable": writable,
            }));
        }
        (
            200,
            json!({
                "roots": roots,
                "candidates": out,
            }),
        )
    }

    /// `{label, path, kind}`
    fn api_add_root(&self, body: &str) -> (u16, J) {
        let Ok(v) = serde_json::from_str::<J>(body) else {
            return (400, json!({"error": "invalid JSON body"}));
        };
        let Some(label) = v["label"].as_str().map(|s| s.trim().to_string()) else {
            return (400, json!({"error": "field 'label' is required"}));
        };
        let Some(path) = v["path"].as_str().map(|s| s.trim().to_string()) else {
            return (400, json!({"error": "field 'path' is required"}));
        };
        let kind = v["kind"]
            .as_str()
            .and_then(StorageKind::parse)
            .unwrap_or(StorageKind::Local);
        let res = self
            .gate
            .with_spirals(|c| c.add_root(&label, &path, kind))
            .ok_or_else(|| "spiral catalog is not attached".to_string());
        match res {
            Ok(Ok(())) => (200, json!({"ok": true, "label": label})),
            Ok(Err(e)) => (400, json!({"error": e})),
            Err(e) => (500, json!({"error": e})),
        }
    }

    fn api_remove_root(&self, label: &str) -> (u16, J) {
        let res = self
            .gate
            .with_spirals(|c| c.remove_root(label))
            .ok_or_else(|| "spiral catalog is not attached".to_string());
        match res {
            Ok(Ok(())) => (200, json!({"ok": true, "removed": label})),
            Ok(Err(e)) => (400, json!({"error": e})),
            Err(e) => (500, json!({"error": e})),
        }
    }
}

fn placement_from_json(
    v: &J,
) -> (Option<&str>, Option<&str>, Option<StorageKind>) {
    (
        v["label"].as_str().map(|s| s.trim()),
        v["path"].as_str().map(|s| s.trim()),
        v["kind"].as_str().and_then(StorageKind::parse),
    )
}

// ---------------------------------------------------------------------------
// HTTP plumbing (minimal, request-per-connection)
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request<R: Read>(stream: &mut R) -> Option<Request> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    // Read until the header block is complete (or the stream ends).
    let header_end = loop {
        if let Some(h) = find_header_end(&buf) {
            break h;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.len() > MAX_BODY {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return None;
    }
    // Drain the declared body (stop early if the stream closes).
    let mut body_bytes = buf[header_end + 4..].to_vec();
    while body_bytes.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if body_bytes.len() > MAX_BODY {
            return None;
        }
    }
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body_bytes[..content_length.min(body_bytes.len())])
            .to_string(),
    })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n"))
}

fn percent_decode(s: &str) -> String {
    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, v: &J) {
    respond(
        stream,
        status,
        "application/json; charset=utf-8",
        &serde_json::to_string(v).unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
    )
}

/// Appends one `# HELP` / `# TYPE` header pair plus the pre-rendered sample
/// line(s) of a Prometheus metric family. `sample` is empty for families
/// with no series yet (a bare TYPE block is valid exposition).
fn metric(out: &mut String, name: &str, help: &str, typ: &str, sample: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push_str("\n# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(typ);
    out.push('\n');
    if !sample.is_empty() {
        out.push_str(sample);
        out.push('\n');
    }
}

/// Escapes a Prometheus label value: backslash, double quote and newline.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "cms"))]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_and_ct() {
        assert_eq!(percent_decode("shop%2Fdb"), "shop/db");
        assert_eq!(percent_decode("plain_name"), "plain_name");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"secretX"));
    }

    #[test]
    fn request_parse_header_and_body() {
        let raw = "POST /api/databases HTTP/1.1\r\nHost: x\r\nContent-Length: 15\r\n\r\n{\"name\":\"db1\"}\n";
        let mut fake = FakeStream(raw.as_bytes().to_vec());
        let req = read_request(&mut fake).expect("parse");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/databases");
        assert_eq!(req.body, "{\"name\":\"db1\"}\n");
        assert_eq!(req.header("content-length"), Some("15"));
    }

    /// std::io::Read/Write over an in-memory buffer.
    struct FakeStream(Vec<u8>);
    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }
    impl Write for FakeStream {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // --------------------------- /metrics ---------------------------

    /// Real engine + attached spiral catalog on a private temp dir.
    fn test_gate(name: &str) -> Arc<CmsSqlGate> {
        let dir = std::env::temp_dir().join(format!(
            "dnadb_panel_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = crate::engine::Engine::open(crate::engine::EngineConfig {
            dir_path: dir.clone(),
            sync_mode: crate::wal::SyncMode::Batched,
            ..crate::engine::EngineConfig::default()
        })
        .unwrap();
        let gate = Arc::new(CmsSqlGate::new(Arc::new(
            crate::sql_engine::SqlEngine::new(engine),
        )));
        gate.attach_spiral_catalog(&dir).unwrap();
        gate
    }

    fn stats_with(totals: (u64, u64, u64), active: u32) -> Arc<MySqlWireStats> {
        let s = Arc::new(MySqlWireStats::default());
        s.total_queries.store(totals.0, Ordering::Relaxed);
        s.total_sessions.store(totals.1, Ordering::Relaxed);
        s.total_errors.store(totals.2, Ordering::Relaxed);
        s.active.store(active, Ordering::Relaxed);
        s
    }

    fn empty_panel(gate: Arc<CmsSqlGate>, stats: Arc<MySqlWireStats>) -> CmsPanel {
        CmsPanel {
            gate,
            stats,
            users: Arc::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn metrics_body_reports_cms_families() {
        let panel = empty_panel(test_gate("families"), stats_with((42, 7, 1), 2));
        let body = panel.render_metrics();
        for needle in [
            "# TYPE dnadb_cms_wire_queries_total counter",
            "dnadb_cms_wire_queries_total 42",
            "dnadb_cms_wire_sessions_total 7",
            "dnadb_cms_wire_errors_total 1",
            "dnadb_cms_wire_sessions_active 2",
            "dnadb_cms_wire_uptime_seconds ",
            "# TYPE dnadb_cms_databases_total gauge",
            "dnadb_cms_databases_total 0",
            "dnadb_cms_spirals_total 0",
            "dnadb_cms_storage_roots_total 2", // local-a / local-b seeded by open()
            "# TYPE dnadb_cms_spirals_per_root gauge",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
        assert!(body.contains(&format!(
            "dnadb_build_info{{version=\"{}\",edition=\"cms\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
        // No pairs yet: the per-root family is declared, has no samples.
        assert!(!body.contains("dnadb_cms_spirals_per_root{"));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn metrics_track_spiral_pair_lifecycle() {
        let gate = test_gate("lifecycle");
        let panel = empty_panel(gate.clone(), Arc::new(MySqlWireStats::default()));
        let pair = gate
            .panel_create_database("shop", (None, None, None), (None, None, None))
            .expect("create pair on default roots");
        assert_eq!(pair.strand_a.root_label.as_deref(), Some("local-a"));
        assert_eq!(pair.strand_b.root_label.as_deref(), Some("local-b"));
        let body = panel.render_metrics();
        assert!(body.contains("dnadb_cms_databases_total 1"));
        assert!(body.contains("dnadb_cms_spirals_total 2"));
        assert!(body.contains("dnadb_cms_spirals_per_root{root=\"local-a\"} 1"));
        assert!(body.contains("dnadb_cms_spirals_per_root{root=\"local-b\"} 1"));
        gate.panel_drop_database("shop").unwrap();
        let body = panel.render_metrics();
        assert!(body.contains("dnadb_cms_databases_total 0"));
        assert!(body.contains("dnadb_cms_spirals_total 0"));
        assert!(!body.contains("dnadb_cms_spirals_per_root{"));
    }

    #[test]
    fn metrics_endpoint_roundtrip_without_auth() {
        let gate = test_gate("roundtrip");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let users: Users = Arc::new(HashMap::new());
        std::thread::spawn(move || {
            CmsPanel::run(listener, gate, Arc::new(MySqlWireStats::default()), users)
        });
        // Scrape: 200 + text format, no bearer token.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {}", &resp[..40]);
        assert!(resp.contains(METRICS_CONTENT_TYPE));
        assert!(resp.contains("dnadb_cms_wire_sessions_total 0"));
        // API routes stay token-protected.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /api/databases HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 401"));
    }

    #[test]
    fn escape_label_escapes_prometheus_specials() {
        assert_eq!(escape_label("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_label("x\ny"), "x\\ny");
        assert_eq!(escape_label("plain"), "plain");
    }
}
