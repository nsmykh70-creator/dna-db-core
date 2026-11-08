// SPDX-License-Identifier: AGPL-3.0-only
//! v3.6 "Unified Query Engine" SQL feature tests: RIGHT/FULL JOIN, UNION,
//! scalar & correlated subqueries, EXISTS, CTE, ON CONFLICT (UPSERT),
//! window functions, session transactions (BEGIN/COMMIT/ROLLBACK),
//! SHOW TABLES, EXPLAIN.

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::sql_engine::{SqlEngine, SqlTxBuffer};
use dna_db_core::relational::Value;
use dna_db_core::wal::SyncMode;
use std::sync::Arc;
fn cfg() -> EngineConfig {
    EngineConfig {
        dir_path: std::env::temp_dir().join(format!(
            "dna_sqlv36_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )),
        sync_mode: SyncMode::Batched,
        ..EngineConfig::default()
    }
}

fn setup() -> (Arc<Engine>, SqlEngine) {
    let engine = Engine::open(cfg()).unwrap();
    let sql = SqlEngine::new(engine.clone());
    sql.execute("CREATE TABLE chr_u (id INT, name TEXT, dept TEXT, PRIMARY KEY (id))")
        .unwrap();
    sql.execute("INSERT INTO chr_u (id, name, dept) VALUES (1, 'ada', 'eng'), (2, 'bob', 'eng'), (3, 'cyd', 'bio')")
        .unwrap();
    sql.execute("CREATE TABLE chr_o (oid INT, uid INT, total FLOAT, PRIMARY KEY (oid))")
        .unwrap();
    sql.execute(
        "INSERT INTO chr_o (oid, uid, total) VALUES (10, 1, 100.0), (11, 1, 250.0), (12, 9, 40.0)",
    )
    .unwrap();
    (engine, sql)
}

fn first_int(res: &dna_db_core::sql_engine::QueryResult, col: &str) -> i64 {
    res.rows[0]
        .get(col)
        .and_then(|v| match v {
            dna_db_core::relational::Value::Int(i) => Some(*i),
            dna_db_core::relational::Value::Float(f) => Some(*f as i64),
            _ => None,
        })
        .unwrap_or_else(|| panic!("column {col} missing"))
}

// ---------- JOINs ----------

#[test]
fn right_join_returns_all_right_rows() {
    let (_e, sql) = setup();
    // RIGHT JOIN chr_o: user 9 has no chr_u row → NULL name, still returned.
    let res = sql
        .execute("SELECT name, total FROM chr_u RIGHT JOIN chr_o ON id = uid ORDER BY oid ASC")
        .unwrap();
    assert_eq!(res.rows.len(), 3, "plan: {}", res.execution_plan);
    let null_names = res
        .rows
        .iter()
        .filter(|r| r.get("name").map(|v| v.is_null()).unwrap_or(true))
        .count();
    assert_eq!(null_names, 1, "user 9 must surface with NULL name");
}

#[test]
fn full_outer_join_returns_unmatched_from_both_sides() {
    let (_e, sql) = setup();
    let res = sql
        .execute("SELECT name, total FROM chr_u FULL OUTER JOIN chr_o ON id = uid")
        .unwrap();
    // Matches: ada→2 orders; unmatched left: bob, cyd; unmatched right: uid=9.
    assert_eq!(res.rows.len(), 5, "plan: {}", res.execution_plan);
    let null_total = res
        .rows
        .iter()
        .filter(|r| r.get("total").map(|v| v.is_null()).unwrap_or(true))
        .count();
    let null_name = res
        .rows
        .iter()
        .filter(|r| r.get("name").map(|v| v.is_null()).unwrap_or(true))
        .count();
    assert_eq!(null_total, 2, "bob and cyd have no orders");
    assert_eq!(null_name, 1, "order 12 (uid=9) has no user");
}

// ---------- UNION ----------

#[test]
fn union_dedups_and_union_all_keeps() {
    let (_e, sql) = setup();
    let u = sql
        .execute("SELECT dept FROM chr_u WHERE id = 1 UNION SELECT dept FROM chr_u WHERE id = 2")
        .unwrap();
    assert_eq!(u.rows.len(), 1, "UNION dedups equal rows");

    let ua = sql
        .execute(
            "SELECT dept FROM chr_u WHERE id = 1 UNION ALL SELECT dept FROM chr_u WHERE id = 2",
        )
        .unwrap();
    assert_eq!(ua.rows.len(), 2, "UNION ALL keeps duplicates");

    let chained = sql
        .execute(
            "SELECT id FROM chr_u WHERE id = 1 UNION SELECT id FROM chr_u WHERE id = 2 \
             UNION ALL SELECT id FROM chr_u WHERE id = 1 ORDER BY id ASC",
        )
        .unwrap();
    assert_eq!(chained.rows.len(), 2, "1 (deduped) + 1 (ALL tail)");
}

// ---------- Scalar / correlated subqueries ----------

#[test]
fn uncorrelated_scalar_subquery_in_where() {
    let (_e, sql) = setup();
    let res = sql
        .execute("SELECT name FROM chr_u WHERE id = (SELECT uid FROM chr_o WHERE oid = 11)")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0]["name"], Value::Text("ada".to_string()));
}

#[test]
fn correlated_scalar_subquery_in_projection() {
    let (_e, sql) = setup();
    // MAX(total) per user — the canonical correlated projection.
    let res = sql
        .execute(
            "SELECT name, (SELECT MAX(total) FROM chr_o WHERE chr_o.uid = chr_u.id) AS max_total \
             FROM chr_u WHERE id = 1",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 1, "plan: {}", res.execution_plan);
    let row = &res.rows[0];
    match row.get("max_total") {
        Some(dna_db_core::relational::Value::Float(f)) => assert!((f - 250.0).abs() < 1e-9),
        other => panic!("expected max_total=250.0, got {other:?}"),
    }
}

#[test]
fn exists_and_not_exists() {
    let (_e, sql) = setup();
    let res = sql
        .execute("SELECT name FROM chr_u WHERE EXISTS (SELECT 1 FROM chr_o WHERE chr_o.uid = chr_u.id)")
        .unwrap();
    assert_eq!(res.rows.len(), 1, "only ada has orders");

    let res2 = sql
        .execute(
            "SELECT name FROM chr_u WHERE NOT EXISTS (SELECT 1 FROM chr_o WHERE chr_o.uid = chr_u.id) \
             ORDER BY name ASC",
        )
        .unwrap();
    assert_eq!(res2.rows.len(), 2, "bob and cyd have no orders");
}

#[test]
fn correlated_in_subquery() {
    let (_e, sql) = setup();
    let res = sql
        .execute(
            "SELECT name FROM chr_u WHERE id IN (SELECT uid FROM chr_o WHERE total > 200.0)",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0]["name"], Value::Text("ada".to_string()));
}

// ---------- CTE ----------

#[test]
fn cte_materialises_and_joins() {
    let (_e, sql) = setup();
    let res = sql
        .execute(
            "WITH eng AS (SELECT id, name FROM chr_u WHERE dept = 'eng') \
             SELECT name FROM eng WHERE id = 2",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0]["name"], Value::Text("bob".to_string()));

    // Two CTEs + join between them.
    let res2 = sql
        .execute(
            "WITH big AS (SELECT oid, uid FROM chr_o WHERE total > 50.0), \
             people AS (SELECT id, name FROM chr_u) \
             SELECT name FROM big JOIN people ON uid = id ORDER BY oid ASC",
        )
        .unwrap();
    assert_eq!(res2.rows.len(), 2, "orders 10,11 belong to ada");
    assert_eq!(res2.rows[0]["name"], Value::Text("ada".to_string()));
}

// ---------- ON CONFLICT (UPSERT) ----------

#[test]
fn on_conflict_do_nothing_skips() {
    let (_e, sql) = setup();
    let res = sql
        .execute(
            "INSERT INTO chr_u (id, name, dept) VALUES (1, 'ignored', 'x') ON CONFLICT (id) DO NOTHING",
        )
        .unwrap();
    assert!(
        res.message.as_deref().unwrap_or("").contains("skipped"),
        "message: {:?}",
        res.message
    );
    let check = sql.execute("SELECT name FROM chr_u WHERE id = 1").unwrap();
    assert_eq!(check.rows[0]["name"], Value::Text("ada".to_string()), "row must be unchanged");
}

#[test]
fn on_conflict_do_update_upserts() {
    let (_e, sql) = setup();
    sql.execute(
        "INSERT INTO chr_u (id, name, dept) VALUES (1, 'ada2', 'ops') \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, dept = EXCLUDED.dept",
    )
    .unwrap();
    let check = sql.execute("SELECT name, dept FROM chr_u WHERE id = 1").unwrap();
    assert_eq!(check.rows[0]["name"], Value::Text("ada2".to_string()));
    assert_eq!(check.rows[0]["dept"], Value::Text("ops".to_string()));
}

// ---------- Window functions ----------

#[test]
fn window_row_number_rank_dense_rank() {
    let (_e, sql) = setup();
    // Two orders for user 1 (total 100, 250) + one unmatched (total 40):
    // partition by uid, order by total DESC.
    let res = sql
        .execute(
            "SELECT oid, ROW_NUMBER() OVER (PARTITION BY uid ORDER BY total DESC) AS rn, \
             RANK() OVER (ORDER BY total DESC) AS rk \
             FROM chr_o ORDER BY oid ASC",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 3, "plan: {}", res.execution_plan);
    let by_oid = |oid: i64| res.rows.iter().find(|r| matches!(r.get("oid"), Some(Value::Int(v)) if *v == oid)).unwrap();
    // Order 11 (250) is rank 1 everywhere; order 10 (100) rank 2; order 12 (40) rank 3.
    assert_eq!(by_oid(11)["rn"], Value::Int(1));
    assert_eq!(by_oid(10)["rn"], Value::Int(2));
    assert_eq!(by_oid(12)["rn"], Value::Int(1), "own partition");
    assert_eq!(by_oid(11)["rk"], Value::Int(1));
    assert_eq!(by_oid(10)["rk"], Value::Int(2));
    assert_eq!(by_oid(12)["rk"], Value::Int(3));
}

// ---------- Transactions ----------

#[test]
fn begin_rollback_discards_and_commit_applies() {
    let (_e, sql) = setup();
    let mut tx: Option<SqlTxBuffer> = None;
    sql.execute_in_tx("BEGIN", &mut tx).unwrap();
    sql.execute_in_tx("INSERT INTO chr_u (id, name, dept) VALUES (77, 'temp', 'tmp')", &mut tx)
        .unwrap();
    // ReadCommitted: uncommitted data is not visible to other paths.
    let visible = sql.execute("SELECT COUNT(*) AS n FROM chr_u WHERE id = 77").unwrap();
    assert_eq!(first_int(&visible, "n"), 0);
    sql.execute_in_tx("ROLLBACK", &mut tx).unwrap();
    let after = sql.execute("SELECT COUNT(*) AS n FROM chr_u WHERE id = 77").unwrap();
    assert_eq!(first_int(&after, "n"), 0, "rollback discards the write");

    sql.execute_in_tx("BEGIN", &mut tx).unwrap();
    sql.execute_in_tx("INSERT INTO chr_u (id, name, dept) VALUES (88, 'kept', 'ops')", &mut tx)
        .unwrap();
    sql.execute_in_tx("COMMIT", &mut tx).unwrap();
    let committed = sql.execute("SELECT COUNT(*) AS n FROM chr_u WHERE id = 88").unwrap();
    assert_eq!(first_int(&committed, "n"), 1, "commit applies the batch");
}

// ---------- Introspection ----------

#[test]
fn show_tables_and_version_and_explain() {
    let (_e, sql) = setup();
    let tables = sql.execute("SHOW TABLES").unwrap();
    let names: Vec<String> = tables
        .rows
        .iter()
        .filter_map(|r| r.get("table_name"))
        .filter_map(|v| match v {
            dna_db_core::relational::Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"chr_u".to_string()));
    assert!(names.contains(&"chr_o".to_string()));

    let ver = sql.execute("SELECT VERSION()").unwrap();
    assert!(
        ver.rows[0]["version"]
            .to_string()
            .contains("DNA-DB v4.2.0"),
        "version: {}",
        ver.rows[0]["version"]
    );

    let plan = sql.execute("EXPLAIN SELECT name FROM chr_u WHERE id = 1").unwrap();
    assert!(
        plan.rows[0]["plan"].to_string().contains("PointGet"),
        "EXPLAIN SELECT must use the PK path: {}",
        plan.rows[0]["plan"]
    );
    let upd = sql
        .execute("EXPLAIN UPDATE chr_u SET dept = 'x' WHERE id = 1")
        .unwrap();
    assert!(
        upd.rows[0]["plan"].to_string().contains("Update"),
        "EXPLAIN UPDATE describes without executing"
    );
    let check = sql.execute("SELECT dept FROM chr_u WHERE id = 1").unwrap();
    assert_eq!(check.rows[0]["dept"], Value::Text("eng".to_string()), "EXPLAIN must not execute");
}
