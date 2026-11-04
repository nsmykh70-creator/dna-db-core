// SPDX-License-Identifier: AGPL-3.0-only
//! Server test suite (split from the former server.rs monolith).

use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
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
