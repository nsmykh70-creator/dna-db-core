// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the embedded admin panel: real socket round-trips
//! against auth, the SQL/DnaQL console, compaction and Prometheus metrics.

use super::*;
use crate::engine::EngineConfig;
use crate::wal::SyncMode;
use std::io::{Read, Write};
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

fn start_panel() -> (u16, Arc<SqlEngine>, Arc<SecurityManager>) {
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
    let security = Arc::new(SecurityManager::new("admin", "s3cret-panel"));
    let panel = Arc::new(AdminPanel {
        engine,
        sql: sql.clone(),
        dnaql,
        security: security.clone(),
        tls: None,
        proxy: None,
        tokens: Mutex::new(HashMap::new()),
        active: Arc::new(AtomicUsize::new(0)),
        #[cfg(feature = "cms")]
        cms: None,
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
    (port, sql, security)
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
    let (port, _sql, _security) = start_panel();

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

/// v4.1 panel features: disk-size health, SQL → DnaQL explain endpoint
/// and the admin-only online compaction endpoint.
#[test]
fn panel_explain_compact_and_disk_size() {
    let (port, sql, security) = start_panel();

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

    // /api/health now reports the data directory size.
    let (status, body) = http(port, "GET", "/api/health", Some(&token), "");
    assert_eq!(status, 200);
    let v: J = serde_json::from_str(&body).unwrap();
    let disk = v["data_dir_bytes"].as_u64().unwrap();
    assert!(disk > 0, "data dir must be non-empty: {body}");

    // Explain: INSERT maps to a DnaQL TX block and a storage map.
    let (status, body) = http(
        port,
        "POST",
        "/api/explain",
        Some(&token),
        r#"{"query":"INSERT INTO chr_t (id = 3, name = 'gamma')"}"#,
    );
    assert_eq!(status, 200);
    let v: J = serde_json::from_str(&body).unwrap();
    let dnaql = v["dnaql"].as_str().unwrap();
    assert!(dnaql.contains("TX"), "INSERT must transpile to TX: {dnaql}");
    assert!(dnaql.contains("chr_t"));
    let storage = v["storage"].as_str().unwrap();
    assert!(
        storage.contains("chr_t:3"),
        "storage map must name the locus key: {storage}"
    );
    assert!(storage.contains("stored:"));

    // Explain: SELECT — v4.3 transpiles JOIN onto the DnaQL JOIN stage
    // (SQL↔DnaQL conformance track); the panel reports both forms.
    let (status, body) = http(
        port,
        "POST",
        "/api/explain",
        Some(&token),
        r#"{"query":"SELECT * FROM chr_t a JOIN chr_t b ON a.id = b.id"}"#,
    );
    assert_eq!(status, 200);
    let v: J = serde_json::from_str(&body).unwrap();
    let dnaql = v["dnaql"].as_str().unwrap_or("");
    assert!(
        dnaql.contains("JOIN chr_t"),
        "JOIN must transpile to the DnaQL JOIN stage: {dnaql}"
    );
    assert!(v["storage"].as_str().unwrap().contains("chr_t"));

    // Explain: garbage query -> 400 with both errors reported.
    let (status, _) = http(
        port,
        "POST",
        "/api/explain",
        Some(&token),
        r#"{"query":"FROBNICATE chr_t"}"#,
    );
    assert_eq!(status, 400);

    // Compaction endpoint: admin may run it, response carries sizes and
    // (when a merge actually happened) the full history record.
    sql.execute("INSERT INTO chr_t (id = 4, name = 'delta')").unwrap();
    sql.execute("UPDATE chr_t SET name = 'beta2' WHERE id = 2").unwrap();
    sql.execute("DELETE FROM chr_t WHERE id = 1").unwrap();
    let (status, body) = http(port, "POST", "/api/compact", Some(&token), "");
    assert_eq!(status, 200);
    let v: J = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert!(v["before_bytes"].as_u64().unwrap() > 0);
    assert!(v["after_bytes"].as_u64().unwrap() > 0);
    assert!(v["removed_files"].as_u64().unwrap() <= 1);
    let ran = v["ran"].as_bool().unwrap_or(false);
    if ran {
        let rec = &v["record"];
        assert_eq!(rec["trigger"], json!("panel:admin"));
        assert!(rec["ts_unix"].as_u64().unwrap() > 0);
    }

    // Prometheus: compaction gauges are exported after the run.
    let (status, body) = http(port, "GET", "/metrics", Some(&token), "");
    assert_eq!(status, 200);
    for name in [
        "dnadb_compactions_total",
        "dnadb_compaction_bytes_before",
        "dnadb_compaction_bytes_after",
        "dnadb_compaction_bytes_saved",
        "dnadb_compaction_last_duration_seconds",
        "dnadb_compaction_last_success_timestamp_seconds",
        "dnadb_data_dir_bytes",
    ] {
        assert!(body.contains(name), "metrics must expose {name}");
    }
    if ran {
        let ts_line = body
            .lines()
            .find(|l| {
                l.starts_with("dnadb_compaction_last_success_timestamp_seconds ")
            })
            .expect("timestamp metric line");
        let ts: u64 = ts_line
            .split_whitespace()
            .last()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        assert!(ts > 0, "compaction timestamp must be set after a run");
    }

    // History endpoint: on-disk compaction journal, admin only.
    let (status, body) = http(port, "GET", "/api/compactions", Some(&token), "");
    assert_eq!(status, 200);
    let v: J = serde_json::from_str(&body).unwrap();
    let list = v["compactions"].as_array().unwrap();
    assert_eq!(list.len(), v["count"].as_u64().unwrap() as usize);
    assert!(v["history_file"]
        .as_str()
        .unwrap()
        .ends_with("compaction_history.jsonl"));
    if ran {
        assert!(
            !list.is_empty(),
            "a completed compaction must appear in the history"
        );
        assert_eq!(list[0]["trigger"], json!("panel:admin"));
        assert!(list[0]["bytes_before"].as_u64().unwrap() > 0);
    }

    // A viewer must be rejected from compaction.
    security
        .users
        .create_user_with_tables("view", "viewer-pass-1", "viewer", false, None)
        .unwrap();
    let (status, body) = http(
        port,
        "POST",
        "/api/login",
        None,
        r#"{"user":"view","pass":"viewer-pass-1"}"#,
    );
    assert_eq!(status, 200);
    let vtoken: J = serde_json::from_str(&body).unwrap();
    let vtoken = vtoken["token"].as_str().unwrap().to_string();
    let (status, _) = http(port, "POST", "/api/compact", Some(&vtoken), "");
    assert_eq!(status, 403);
    // …and from reading the compaction history.
    let (status, _) = http(port, "GET", "/api/compactions", Some(&vtoken), "");
    assert_eq!(status, 403);

    // …and from explaining statements on tables outside its ACL.
    security
        .users
        .set_allowed_tables("view", Some(vec!["chr_other".into()]))
        .unwrap();
    let (status, body) = http(
        port,
        "POST",
        "/api/login",
        None,
        r#"{"user":"view","pass":"viewer-pass-1"}"#,
    );
    assert_eq!(status, 200);
    let vtoken: J = serde_json::from_str(&body).unwrap();
    let vtoken = vtoken["token"].as_str().unwrap().to_string();
    let (status, _) = http(
        port,
        "POST",
        "/api/explain",
        Some(&vtoken),
        r#"{"query":"INSERT INTO chr_t (id = 9, name = 'x')"}"#,
    );
    assert_eq!(status, 403);
}
