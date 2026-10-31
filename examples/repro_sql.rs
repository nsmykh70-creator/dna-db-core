// quick repro via examples dir
use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::sql_engine::SqlEngine;
use dna_db_core::wal::SyncMode;
use std::sync::Arc;
fn main() {
    let cfg = EngineConfig {
        dir_path: std::env::temp_dir().join(format!("dna_repro_{}", std::process::id())),
        sync_mode: SyncMode::Batched,
        ..Default::default()
    };
    let _ = std::fs::remove_dir_all(&cfg.dir_path);
    let engine = Engine::open(cfg).unwrap();
    let sql = SqlEngine::new(engine);
    sql.execute("CREATE TABLE chr_u (id INT, name TEXT, dept TEXT, PRIMARY KEY (id))").unwrap();
    sql.execute("INSERT INTO chr_u (id, name, dept) VALUES (1, 'ada', 'eng'), (2, 'bob', 'eng'), (3, 'cyd', 'bio')").unwrap();
    sql.execute("CREATE TABLE chr_o (oid INT, uid INT, total FLOAT, PRIMARY KEY (oid))").unwrap();
    sql.execute("INSERT INTO chr_o (oid, uid, total) VALUES (10, 1, 100.0), (11, 1, 250.0), (12, 9, 40.0)").unwrap();

    for q in [
        "SELECT name, (SELECT total FROM chr_o WHERE oid = 11) AS x FROM chr_u WHERE id = 1",
        "SELECT name, (SELECT MAX(total) FROM chr_o WHERE oid = 11) AS m FROM chr_u WHERE id = 1",
        "SELECT name, (SELECT MAX(total) FROM chr_o) AS m FROM chr_u WHERE id = 1",
        "SELECT (SELECT MAX(total) FROM chr_o) AS m",
        "SELECT MAX(total) FROM chr_o WHERE oid = 11",
    ] {
        match sql.execute(q) {
            Ok(r) => println!("OK   {:?} -> {}", q, r.rows.len()),
            Err(e) => println!("ERR  {:?} -> {}", q, e),
        }
    }
    println!("== T1 uncorrelated scalar in WHERE ==");
    match sql.execute("SELECT name FROM chr_u WHERE id = (SELECT uid FROM chr_o WHERE oid = 11)") {
        Ok(r) => println!("rows={:?}", r.rows),
        Err(e) => println!("ERR {e}"),
    }
    println!("== T2 correlated scalar in projection ==");
    match sql.execute("SELECT name, (SELECT MAX(total) FROM chr_o WHERE chr_o.uid = chr_u.id) AS max_total FROM chr_u WHERE id = 1") {
        Ok(r) => println!("rows={:?}", r.rows),
        Err(e) => println!("ERR {e}"),
    }
    println!("== T3 union ==");
    match sql.execute("SELECT dept FROM chr_u WHERE id = 1 UNION SELECT dept FROM chr_u WHERE id = 2") {
        Ok(r) => println!("rows={:?}", r.rows),
        Err(e) => println!("ERR {e}"),
    }
}
