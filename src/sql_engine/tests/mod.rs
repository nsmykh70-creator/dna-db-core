// SPDX-License-Identifier: AGPL-3.0-only
//! sql_engine test battery, moved verbatim from the pre-split monolith.
//! Re-exports keep the original `use super::*;` inside each module working.

pub(super) use super::index_scan::*;
pub(super) use super::lexer::{stokenize, STok};
pub(super) use super::refs::*;
pub(super) use super::*;
pub(super) use crate::expr::{ArithOp, Expr};
pub(super) use crate::relational::RowCodec;

#[cfg(test)]
mod acl_parse_tests;
#[cfg(test)]
mod battery;
#[cfg(test)]
mod security_regression_tests;

#[test]
fn case_expression_searched_and_simple() {
    use crate::engine::{Engine, EngineConfig};
    use crate::wal::SyncMode;
    let dir = std::env::temp_dir().join(format!(
        "dna_sql_case_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let engine = Engine::open(EngineConfig {
        dir_path: dir,
        memtable_capacity: 4 * 1024 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        ..Default::default()
    })
    .unwrap();
    let sql = SqlEngine::new(engine);
    sql.execute("CREATE TABLE chr_case (id INT PRIMARY KEY, balance FLOAT)").unwrap();
    sql.execute("INSERT INTO chr_case (id, balance) VALUES (1, 10.0), (2, 0.0), (3, NULL)").unwrap();
    // searched form (Joomla ArticlesModel projection)
    let res = sql
        .execute("SELECT id, CASE WHEN balance > 5.0 THEN 'rich' ELSE 'poor' END AS tier FROM chr_case ORDER BY id")
        .unwrap();
    assert_eq!(res.rows[0].get("tier"), Some(&Value::Text("rich".into())));
    assert_eq!(res.rows[1].get("tier"), Some(&Value::Text("poor".into())));
    // NULL condition falls to ELSE (MySQL IF semantics)
    assert_eq!(res.rows[2].get("tier"), Some(&Value::Text("poor".into())));
    // simple form with several arms
    let res = sql
        .execute("SELECT id, CASE id WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END AS n FROM chr_case ORDER BY id")
        .unwrap();
    assert_eq!(res.rows[0].get("n"), Some(&Value::Text("one".into())));
    assert_eq!(res.rows[1].get("n"), Some(&Value::Text("two".into())));
    assert_eq!(res.rows[2].get("n"), Some(&Value::Text("other".into())));
}
