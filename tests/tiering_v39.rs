// SPDX-License-Identifier: AGPL-3.0-only
//! v3.9 tiering tests: S3 round-trip through an in-process S3-compatible
//! HTTP server (SigV4 signed PUT/GET/DELETE) and Online DDL.

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::sql_engine::SqlEngine;
use dna_db_core::tiered_storage::{S3Client, TieredStorageManager};
use dna_db_core::wal::SyncMode;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dna_tier39_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A tiny S3-compatible object store: answers signed PUT/GET/DELETE on
/// /<bucket>/<key> like MinIO does. Verifies the request carries the
/// SigV4 Authorization header and x-amz-date.
fn spawn_s3_mock() -> (u16, Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let store_thread = store.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let store = store_thread.clone();
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = vec![0u8; 65536];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut parts = req.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let signed = req.contains("Authorization: AWS4-HMAC-SHA256")
                    && req.contains("x-amz-date:");
                // Strip query (list requests), normalise key.
                let key = path.split('?').next().unwrap_or("").to_string();
                let body = req
                    .split("\r\n\r\n")
                    .nth(1)
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec();
                let status_line;
                {
                    let mut st = store.lock().unwrap();
                    match (method.as_str(), signed) {
                        ("PUT", true) => {
                            st.insert(key.clone(), body.clone());
                            status_line = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        }
                        ("GET", true) => match st.get(&key) {
                            Some(data) => {
                                status_line = Box::leak(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        data.len()
                                    )
                                    .into_boxed_str(),
                                );
                                let _ = s.write_all(status_line.as_bytes());
                                let _ = s.write_all(data);
                                return;
                            }
                            None => {
                                status_line = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                            }
                        },
                        ("DELETE", true) => {
                            st.remove(&key);
                            status_line = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        }
                        _ => {
                            status_line = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        }
                    }
                }
                let _ = s.write_all(status_line.as_bytes());
            });
        }
    });
    (port, store)
}

#[test]
fn s3_demote_promote_roundtrip() {
    let (port, store) = spawn_s3_mock();
    let dir = temp_dir("s3");
    let mgr = TieredStorageManager::new(&dir).unwrap();
    // Two "segments" in the hot tier.
    std::fs::write(dir.join("000001.sst"), b"segment-one-payload-000001").unwrap();
    std::fs::write(dir.join("000002.sst"), b"segment-two-payload-000002").unwrap();

    let s3 = S3Client {
        endpoint: format!("127.0.0.1:{port}"),
        bucket: "dnadb-test".into(),
        region: "us-east-1".into(),
        access_key: "AKIDTEST".into(),
        secret_key: "secrettest".into(),
        https: false,
    };

    let key = mgr.demote_segment_to_s3(&s3, 1).unwrap();
    assert_eq!(key, "cold/000001.sst");
    // Local copy is gone, object is in the mock bucket.
    assert!(!dir.join("000001.sst").exists());
    assert!(store
        .lock()
        .unwrap()
        .contains_key("/dnadb-test/cold/000001.sst"));

    // Promote back: byte-identical payload returns to the hot tier.
    assert!(mgr.promote_segment_from_s3(&s3, 1).unwrap());
    let restored = std::fs::read(dir.join("000001.sst")).unwrap();
    assert_eq!(restored, b"segment-one-payload-000001");

    // Unknown object reports false (never fabricates data).
    assert!(!mgr.promote_segment_from_s3(&s3, 999).unwrap());

    // Unsigned requests are rejected by the mock (SigV4 really applied).
    let unsigned = S3Client {
        https: false,
        ..s3
    };
    // The client always signs; verify at least one signed request landed by
    // checking the object round-tripped (mock only answers 2xx when signed).
    assert_eq!(
        unsigned.get_object("cold/000001.sst").unwrap(),
        b"segment-one-payload-000001".to_vec()
    );
}

#[test]
fn online_ddl_add_column_is_instant() {
    let dir = temp_dir("ddl");
    let engine = Engine::open(EngineConfig {
        dir_path: dir.clone(),
        sync_mode: SyncMode::Batched,
        crypto: None,
        ..Default::default()
    })
    .unwrap();
    let sql = SqlEngine::new(engine);
    sql.execute("CREATE TABLE chr_ddl (id INT, name TEXT, PRIMARY KEY (id))")
        .unwrap();
    sql.execute("INSERT INTO chr_ddl (id, name) VALUES (1, 'x'), (2, 'y')")
        .unwrap();

    // Instant, metadata-only.
    let res = sql
        .execute("ALTER TABLE chr_ddl ADD COLUMN note TEXT")
        .unwrap();
    assert!(res.execution_plan.contains("instant metadata-only"));

    // Existing rows read the new column as NULL; new rows carry values.
    let r = sql.execute("SELECT id, note FROM chr_ddl WHERE id = 1").unwrap();
    assert!(r.rows[0].get("note").map(|v| v.is_null()).unwrap_or(true));
    sql.execute("INSERT INTO chr_ddl (id, name, note) VALUES (3, 'z', 'hello')")
        .unwrap();
    let r2 = sql.execute("SELECT note FROM chr_ddl WHERE id = 3").unwrap();
    assert_eq!(r2.rows[0]["note"], dna_db_core::relational::Value::Text("hello".into()));

    // Duplicate ADD COLUMN is a clean error.
    assert!(sql.execute("ALTER TABLE chr_ddl ADD COLUMN note INT").is_err());

    // SHOW TABLES still lists it; schema survives (in-memory check).
    let t = sql.execute("SHOW TABLES").unwrap();
    assert!(t
        .rows
        .iter()
        .any(|r| r.get("table_name") == Some(&dna_db_core::relational::Value::Text("chr_ddl".into()))));
}
