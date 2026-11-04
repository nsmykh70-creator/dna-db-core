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
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::wal::SyncMode;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_sql_{}_{}",
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

    fn setup() -> (Arc<Engine>, SqlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("sql"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine.clone());
        sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))").unwrap();
        sql.execute("CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, PRIMARY KEY (order_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id) ON DELETE CASCADE)").unwrap();
        (engine, sql)
    }

    #[test]
    fn create_insert_select() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 1500.50)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 890.00)",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1042, user_id = 12, total_usd = 350.00, status = 'PENDING')").unwrap();

        let res = sql
            .execute("SELECT user_id, username FROM chr_users WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("dr_watson".into()))
        );
        assert!(res.execution_plan.contains("Filter"));
    }

    #[test]
    fn select_where_pk_uses_point_get() {
        let (_e, sql) = setup();
        for i in 0..200 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.5)",
                i, i, i
            ))
            .unwrap();
        }

        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("u137".into()))
        );
        assert!(
            res.execution_plan.contains("PointGet"),
            "plan: {}",
            res.execution_plan
        );
        assert!(
            !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // nonexistent pk: point get misses without scanning
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 99999")
            .unwrap();
        assert!(res.rows.is_empty());
        assert!(res.execution_plan.contains("PointGet"));

        // OR of two pk equalities -> two point gets
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 3 OR user_id = 7")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(
            res.execution_plan.contains("PointGet") && res.execution_plan.contains("IN (3, 7)"),
            "plan: {}",
            res.execution_plan
        );

        // extra predicate beyond the pk still filters correctly
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137 AND balance > 200")
            .unwrap();
        assert!(res.rows.is_empty());
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = 137 AND balance > 100")
            .unwrap();
        assert_eq!(res.rows.len(), 1);

        // non-pk predicate -> honest SeqScan fallback
        let res = sql
            .execute("SELECT username FROM chr_users WHERE username = 'u50'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // NULL literal must not take the point path (pk = NULL is never true)
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id = NULL")
            .unwrap();
        assert!(res.rows.is_empty());
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn point_get_with_text_pk_and_joins() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_genes (gene_id TEXT, name TEXT, PRIMARY KEY (gene_id))")
            .unwrap();
        sql.execute("INSERT INTO chr_genes (gene_id = 'BRCA1', name = 'repair')")
            .unwrap();
        sql.execute("INSERT INTO chr_genes (gene_id = 'TP53', name = 'apoptosis')")
            .unwrap();

        let res = sql
            .execute("SELECT name FROM chr_genes WHERE gene_id = 'TP53'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("name"),
            Some(&Value::Text("apoptosis".into()))
        );
        assert!(res.execution_plan.contains("PointGet"));

        // pk point get as the left side of a JOIN stays correct
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 5.0, status = 'OK')",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 2, user_id = 12, total_usd = 7.0, status = 'OK')",
        )
        .unwrap();
        let res = sql
            .execute(
                "SELECT order_id, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE order_id = 2",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1, "rows: {:?}", res.rows);
        assert_eq!(res.rows[0].get("total_usd"), Some(&Value::Float(7.0)));
        assert!(
            res.execution_plan.contains("PointGet") && res.execution_plan.contains("HashJoin"),
            "plan: {}",
            res.execution_plan
        );
    }

    #[test]
    fn update_and_delete_where_pk_use_point_get() {
        let (e, sql) = setup();
        for i in 0..50 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.0)",
                i, i, i
            ))
            .unwrap();
        }

        let res = sql
            .execute("UPDATE chr_users SET balance = 999.0 WHERE user_id = 25")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));
        assert!(
            res.execution_plan.contains("PointGet") && !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );
        let raw = e.get(b"chr_users:25").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(999.0)));

        // extra predicate: point get fetches, WHERE still filters
        let res = sql
            .execute("UPDATE chr_users SET balance = 1.0 WHERE user_id = 25 AND balance > 2000")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(0)));

        let res = sql
            .execute("DELETE FROM chr_users WHERE user_id = 25")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert!(
            res.execution_plan.contains("PointGet"),
            "plan: {}",
            res.execution_plan
        );
        assert!(e.get(b"chr_users:25").unwrap().is_none());

        // non-pk WHERE keeps SeqScan semantics
        let res = sql
            .execute("DELETE FROM chr_users WHERE username = 'u30'")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn tokenizer_negative_numbers_and_escaped_quotes() {
        // negative int / float literals
        let toks = stokenize("UPDATE t SET a = -1, b = -2.5").unwrap();
        assert!(toks.iter().any(|t| matches!(t, STok::Int(i) if *i == -1)));
        assert!(toks
            .iter()
            .any(|t| matches!(t, STok::Float(f) if (*f - -2.5).abs() < f64::EPSILON)));

        // doubled single quote inside a string is a literal quote
        let toks = stokenize("UPDATE t SET name = 'O''Brien'").unwrap();
        assert!(toks
            .iter()
            .any(|t| matches!(t, STok::Str(s) if s == "O'Brien")));
    }

    #[test]
    fn update_with_negative_and_quoted_values() {
        let (e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 7, username = 'plain', balance = 100.0)")
            .unwrap();

        let res = sql
            .execute(
                "UPDATE chr_users SET balance = -25.5, username = 'O''Brien' WHERE user_id = 7",
            )
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));

        let raw = e.get(b"chr_users:7").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(-25.5)));
        assert!(fields
            .iter()
            .any(|(k, v)| k == "username" && *v == Value::Text("O'Brien".into())));
    }

    #[test]
    fn unique_index_rejects_duplicate_insert() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alice', balance = 10.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.0)")
            .unwrap();

        sql.execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap();

        // duplicate value rejected
        let err = sql
            .execute("INSERT INTO chr_users (user_id = 3, username = 'alice', balance = 30.0)")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");
        // nothing was written
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE user_id = 3")
            .unwrap();
        assert!(res.rows.is_empty());

        // distinct value accepted
        sql.execute("INSERT INTO chr_users (user_id = 4, username = 'carol', balance = 40.0)")
            .unwrap();

        // multi-row INSERT with an in-statement duplicate rejected
        let err = sql
            .execute(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (5, 'dave', 1.0), (6, 'dave', 2.0)",
            )
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");

        // same statement with distinct values accepted atomically
        sql.execute(
            "INSERT INTO chr_users (user_id, username, balance) VALUES (7, 'eve', 1.0), (8, 'frank', 2.0)",
        )
        .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'eve'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn unique_index_rejects_duplicate_update_and_allows_nulls() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alice', balance = 10.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.0)")
            .unwrap();
        sql.execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap();

        // updating one row onto another's unique value is rejected
        let err = sql
            .execute("UPDATE chr_users SET username = 'alice' WHERE user_id = 2")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");

        // moving to a free value works
        sql.execute("UPDATE chr_users SET username = 'robert' WHERE user_id = 2")
            .unwrap();

        // NULL is not a value: multiple NULL usernames never conflict
        sql.execute("CREATE TABLE chr_notes (id INT, owner TEXT, note TEXT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 1, owner = NULL, note = 'a')")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 2, owner = NULL, note = 'b')")
            .unwrap();
        sql.execute("CREATE UNIQUE INDEX uq_notes_owner ON chr_notes (owner)")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (id = 3, owner = NULL, note = 'c')")
            .unwrap();
        // but a real duplicate still fails
        sql.execute("INSERT INTO chr_notes (id = 4, owner = 'zed', note = 'd')")
            .unwrap();
        let err = sql
            .execute("INSERT INTO chr_notes (id = 5, owner = 'zed', note = 'e')")
            .unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"), "err: {err}");
    }

    #[test]
    fn unique_index_creation_fails_on_existing_duplicates() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'dup', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'dup', balance = 2.0)")
            .unwrap();

        let err = sql
            .execute("CREATE UNIQUE INDEX uq_users_username ON chr_users (username)")
            .unwrap_err();
        assert!(err.contains("cannot create UNIQUE index"), "err: {err}");
        // index must not have been registered
        let schema = sql.get_schema("chr_users").unwrap();
        assert!(schema.secondary_index_on("username").is_none());

        // plain CREATE INDEX on the same data still succeeds
        sql.execute("CREATE INDEX idx_users_username ON chr_users (username)")
            .unwrap();
        let schema = sql.get_schema("chr_users").unwrap();
        let idx = schema.secondary_index_on("username").unwrap();
        assert!(!idx.unique);
    }

    #[test]
    fn hash_join_select() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 1500.50)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 890.00)",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1042, user_id = 12, total_usd = 350.00, status = 'OK')").unwrap();
        sql.execute("INSERT INTO chr_orders (order_id = 1089, user_id = 14, total_usd = 740.00, status = 'OK')").unwrap();

        let res = sql.execute(
            "SELECT order_id, username, total_usd FROM chr_orders JOIN chr_users ON user_id = user_id WHERE total_usd > 300"
        ).unwrap();
        assert_eq!(res.rows.len(), 2, "rows: {:?}", res.rows);
        assert!(res.execution_plan.contains("HashJoin"));
        assert!(res.execution_plan.contains("Filter"));
        assert!(res.execution_plan.contains("Project"));
    }

    #[test]
    fn fk_violation_rejected() {
        let (_e, sql) = setup();
        let err = sql.execute("INSERT INTO chr_orders (order_id = 1, user_id = 999, total_usd = 10.0, status = 'X')").unwrap_err();
        assert!(err.contains("FK violation"), "{}", err);
    }

    #[test]
    fn update_and_delete_with_cascade() {
        let (e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 100.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 50.0, status = 'X')",
        )
        .unwrap();

        let res = sql
            .execute("UPDATE chr_users SET balance = 200.00 WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows[0].get("affected_rows"), Some(&Value::Int(1)));

        let raw = e.get(b"chr_users:12").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert!(fields
            .iter()
            .any(|(k, v)| k == "balance" && *v == Value::Float(200.0)));

        // DELETE with cascade removes user + orders
        let res = sql
            .execute("DELETE FROM chr_users WHERE user_id = 12")
            .unwrap();
        assert_eq!(res.rows[0].get("deleted"), Some(&Value::Int(1)));
        assert_eq!(res.rows[0].get("cascaded"), Some(&Value::Int(1)));
        assert!(e.get(b"chr_users:12").unwrap().is_none());
        assert!(e.get(b"chr_orders:1").unwrap().is_none());
    }

    #[test]
    fn update_primary_key_moves_row_without_leaving_stale_key() {
        let (engine, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("UPDATE chr_users SET user_id = 13 WHERE user_id = 12")
            .unwrap();
        assert!(engine.get(b"chr_users:12").unwrap().is_none());
        assert!(engine.get(b"chr_users:13").unwrap().is_some());
    }

    #[test]
    fn delete_restrict_blocks_referenced_parent() {
        let (engine, sql) = setup();
        sql.execute(
            "CREATE TABLE chr_notes (note_id INT, user_id INT, PRIMARY KEY (note_id), FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_notes (note_id = 1, user_id = 12)")
            .unwrap();

        let error = sql
            .execute("DELETE FROM chr_users WHERE user_id = 12")
            .unwrap_err();
        assert!(error.contains("RESTRICT"), "{error}");
        assert!(engine.get(b"chr_users:12").unwrap().is_some());
        assert!(engine.get(b"chr_notes:1").unwrap().is_some());
    }

    #[test]
    fn complement_index_updates_removes_and_rebuilds() {
        let (engine, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 14, username = 'b', balance = 2.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 5, user_id = 12, total_usd = 9.0, status = 'S')",
        )
        .unwrap();
        sql.execute("UPDATE chr_orders SET user_id = 14 WHERE order_id = 5")
            .unwrap();

        {
            let mut index = sql.complement_index.write();
            assert!(index
                .lookup_all("chr_orders", "user_id", &Value::Int(12))
                .is_empty());
            assert_eq!(
                index.lookup_all("chr_orders", "user_id", &Value::Int(14)),
                vec!["chr_orders:5".to_string()]
            );
        }
        sql.execute("DELETE FROM chr_orders WHERE order_id = 5")
            .unwrap();
        {
            let mut index = sql.complement_index.write();
            assert!(index
                .lookup_all("chr_orders", "user_id", &Value::Int(14))
                .is_empty());
        }

        sql.execute(
            "INSERT INTO chr_orders (order_id = 6, user_id = 12, total_usd = 3.0, status = 'S')",
        )
        .unwrap();
        let reloaded = SqlEngine::new(engine);
        let mut index = reloaded.complement_index.write();
        assert_eq!(
            index.lookup_all("chr_orders", "user_id", &Value::Int(12)),
            vec!["chr_orders:6".to_string()]
        );
    }

    #[test]
    fn persistence_across_restart() {
        let dir = tempdir("persist");
        {
            let engine = Engine::open(EngineConfig {
                dir_path: dir.clone(),
                memtable_capacity: 1024 * 1024,
                sync_mode: SyncMode::Strict,
                mirror_disk_path: None,
                ..Default::default()
            })
            .unwrap();
            let sql = SqlEngine::new(engine.clone());
            sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))").unwrap();
            sql.execute(
                "INSERT INTO chr_users (user_id = 7, username = 'rosalind_f', balance = 999.99)",
            )
            .unwrap();
        }
        // Reopen: schema + rows must reload
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine);
        assert_eq!(sql.list_tables(), vec!["chr_users".to_string()]);
        let res = sql
            .execute("SELECT username, balance FROM chr_users WHERE user_id = 7")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(
            res.rows[0].get("username"),
            Some(&Value::Text("rosalind_f".into()))
        );
    }

    #[test]
    fn left_join_keeps_unmatched() {
        let (_e, sql) = setup();
        sql.execute(
            "INSERT INTO chr_users (user_id = 12, username = 'dr_watson', balance = 100.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_users (user_id = 14, username = 'prof_crick', balance = 200.0)",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 1, user_id = 12, total_usd = 10.0, status = 'A')",
        )
        .unwrap();

        let res = sql.execute(
            "SELECT username, order_id FROM chr_users LEFT JOIN chr_orders ON user_id = user_id"
        ).unwrap();
        assert_eq!(res.rows.len(), 2); // both users present
        let crick = res
            .rows
            .iter()
            .find(|r| r.get("username") == Some(&Value::Text("prof_crick".into())))
            .unwrap();
        assert_eq!(crick.get("order_id"), Some(&Value::Null));
    }

    #[test]
    fn limit_and_multi_insert() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 13, username = 'b', balance = 2.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 14, username = 'c', balance = 3.0)")
            .unwrap();
        let res = sql
            .execute("SELECT username FROM chr_users LIMIT 2")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(res.execution_plan.contains("Limit(2)"));
    }

    #[test]
    fn dna_values_stored_typed() {
        let (e, sql) = setup();
        sql.execute("CREATE TABLE chr_variants (variant_id INT, variant_name TEXT, sequence DNA, PRIMARY KEY (variant_id))").unwrap();
        sql.execute("INSERT INTO chr_variants (variant_id = 1, variant_name = 'BRCA1', sequence = 'ACGTACGT')").unwrap();
        let raw = e.get(b"chr_variants:1").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        let seq = fields.iter().find(|(k, _)| k == "sequence").unwrap();
        assert_eq!(seq.1, Value::Dna("ACGTACGT".into()));
    }

    #[test]
    fn complement_index_maintained() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 12, username = 'a', balance = 1.0)")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id = 5, user_id = 12, total_usd = 9.0, status = 'S')",
        )
        .unwrap();
        // O(1) navigation via index
        let key = sql
            .complement_index
            .write()
            .lookup("chr_orders", "user_id", &Value::Int(12))
            .map(|s| s.to_string());
        assert_eq!(key, Some("chr_orders:5".to_string()));
    }

    // ---- v3.1: secondary indexes, ORDER BY, IN, aggregates ----

    #[test]
    fn create_index_equijoin_lookup_uses_index_scan() {
        let (_e, sql) = setup();
        for i in 0..100 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {}.5)",
                i, i, i
            ))
            .unwrap();
        }
        sql.execute("CREATE INDEX idx_users_name ON chr_users (username)")
            .unwrap();

        // Equality on an indexed column -> IndexScan, not SeqScan.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'u42'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("user_id"), Some(&Value::Int(42)));
        assert!(
            res.execution_plan.contains("IndexScan"),
            "plan: {}",
            res.execution_plan
        );
        assert!(
            !res.execution_plan.contains("SeqScan"),
            "plan: {}",
            res.execution_plan
        );

        // The index catalog persists inside the schema row.
        let schema = sql.get_schema("chr_users").unwrap();
        assert!(schema.secondary_index_on("username").is_some());
    }

    #[test]
    fn index_range_scan_bounded() {
        let (_e, sql) = setup();
        for i in 0..50 {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                i, i, i
            ))
            .unwrap();
        }
        sql.execute("CREATE INDEX idx_bal ON chr_users (balance)")
            .unwrap();

        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE balance >= 10 AND balance < 20")
            .unwrap();
        assert_eq!(res.rows.len(), 10, "rows: {:?}", res.rows);
        for r in &res.rows {
            let uid = match r.get("user_id") {
                Some(Value::Int(i)) => *i,
                _ => -1,
            };
            assert!((10..20).contains(&uid), "uid {uid} out of range");
        }
        assert!(
            res.execution_plan.contains("IndexScan") && res.execution_plan.contains(">="),
            "plan: {}",
            res.execution_plan
        );

        // Reverse-direction bounds and exclusive edges.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE 40 >= balance AND balance > 30")
            .unwrap();
        assert_eq!(res.rows.len(), 10);

        // Open-ended range.
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE balance > 45")
            .unwrap();
        assert_eq!(res.rows.len(), 4);
    }

    #[test]
    fn index_maintained_on_update_and_delete() {
        let (engine, sql) = setup();
        sql.execute("CREATE INDEX idx_name ON chr_users (username)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'alpha', balance = 1.0)")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id = 2, username = 'beta', balance = 2.0)")
            .unwrap();

        // UPDATE moves the index entry with the row.
        sql.execute("UPDATE chr_users SET username = 'gamma' WHERE user_id = 1")
            .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'alpha'")
            .unwrap();
        assert!(res.rows.is_empty(), "stale alpha entry: {:?}", res.rows);
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'gamma'")
            .unwrap();
        assert_eq!(res.rows.len(), 1);

        // DELETE removes entries.
        sql.execute("DELETE FROM chr_users WHERE user_id = 2")
            .unwrap();
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'beta'")
            .unwrap();
        assert!(res.rows.is_empty());
        // The physical index key for 'gamma' must exist (payload = row key).
        let entries = engine.scan_prefix(b"__idx:chr_users:username:").unwrap();
        assert_eq!(entries.len(), 1, "entries: {:?}", entries.len());
        assert_eq!(
            String::from_utf8(entries[0].1.clone()).unwrap(),
            "chr_users:1"
        );

        // DROP INDEX removes the catalog entry and all entries.
        sql.execute("DROP INDEX idx_name").unwrap();
        assert!(sql
            .get_schema("chr_users")
            .unwrap()
            .secondary_indexes
            .is_empty());
        let res = sql
            .execute("SELECT user_id FROM chr_users WHERE username = 'gamma'")
            .unwrap();
        assert_eq!(res.rows.len(), 1); // falls back to SeqScan, same results
        assert!(res.execution_plan.contains("SeqScan"));
    }

    #[test]
    fn index_survives_restart_and_cascade_delete_cleans_entries() {
        let dir = tempdir("idx_persist");
        {
            let engine = Engine::open(EngineConfig {
                dir_path: dir.clone(),
                memtable_capacity: 1024 * 1024,
                sync_mode: SyncMode::Strict,
                mirror_disk_path: None,
                ..Default::default()
            })
            .unwrap();
            let sql = SqlEngine::new(engine.clone());
            sql.execute("CREATE TABLE chr_t (id INT, tag TEXT, PRIMARY KEY (id))")
                .unwrap();
            sql.execute("CREATE INDEX idx_tag ON chr_t (tag)").unwrap();
            sql.execute("INSERT INTO chr_t (id = 1, tag = 'x')")
                .unwrap();
        }
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            memtable_capacity: 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = SqlEngine::new(engine);
        // Schema (and index catalog) reloaded from disk.
        assert!(sql
            .get_schema("chr_t")
            .unwrap()
            .secondary_index_on("tag")
            .is_some());
        let res = sql.execute("SELECT id FROM chr_t WHERE tag = 'x'").unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("IndexScan"),
            "plan: {}",
            res.execution_plan
        );
    }

    #[test]
    fn order_by_asc_desc_and_limit() {
        let (_e, sql) = setup();
        for (id, bal) in [(3, 30.0), (1, 10.0), (4, 40.0), (2, 20.0)] {
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                id, id, bal
            ))
            .unwrap();
        }
        let res = sql
            .execute("SELECT username FROM chr_users ORDER BY balance DESC")
            .unwrap();
        let names: Vec<_> = res
            .rows
            .iter()
            .map(|r| r.get("username").cloned().unwrap_or(Value::Null))
            .collect();
        assert_eq!(
            names,
            vec![
                Value::Text("u4".into()),
                Value::Text("u3".into()),
                Value::Text("u2".into()),
                Value::Text("u1".into())
            ]
        );
        assert!(res.execution_plan.contains("Sort"));

        let res = sql
            .execute("SELECT username FROM chr_users ORDER BY balance ASC LIMIT 2")
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.rows[0].get("username"), Some(&Value::Text("u1".into())));
        assert_eq!(res.rows[1].get("username"), Some(&Value::Text("u2".into())));
    }

    #[test]
    fn in_list_and_in_subquery() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_tags (tag_id INT, label TEXT, PRIMARY KEY (tag_id))")
            .unwrap();
        for i in 0..10 {
            sql.execute(&format!(
                "INSERT INTO chr_tags (tag_id = {}, label = 'L{}')",
                i, i
            ))
            .unwrap();
            sql.execute(&format!(
                "INSERT INTO chr_users (user_id = {}, username = 'u{}', balance = {})",
                i, i, i
            ))
            .unwrap();
        }

        // IN with a literal list.
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id IN (2, 4, 6)")
            .unwrap();
        assert_eq!(res.rows.len(), 3);

        // IN with a subquery: users whose id appears in chr_tags... users 0..10,
        // tags 0..10 -> all; restrict the subquery to prove it runs.
        let res = sql
            .execute(
                "SELECT username FROM chr_users WHERE user_id IN (SELECT tag_id FROM chr_tags WHERE tag_id >= 7)",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 3, "rows: {:?}", res.rows);

        // The plan shows the materialised IN-list (subquery executed once).
        let res = sql
            .execute(
                "SELECT username FROM chr_users WHERE user_id IN (SELECT tag_id FROM chr_tags WHERE tag_id = 3)",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert!(
            res.execution_plan.contains("IN (3)"),
            "plan: {}",
            res.execution_plan
        );

        // NULL semantics: NULL column never matches an IN list.
        let res = sql
            .execute("SELECT username FROM chr_users WHERE user_id IN (1, NULL)")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    #[test]
    fn aggregates_count_sum_avg_min_max() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'u1', balance = 1.0)")
            .unwrap();
        for (id, bal, status) in [
            (1, 10.5, "OK"),
            (2, 20.5, "OK"),
            (3, 30.5, "FAIL"),
            (4, 40.5, "FAIL"),
            (5, 50.5, "FAIL"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = 1, total_usd = {}, status = '{}')",
                id, bal, status
            ))
            .unwrap();
        }

        // Global aggregates (no GROUP BY): one row even over the full set.
        let res = sql
            .execute(
                "SELECT COUNT(*) AS c, SUM(total_usd) AS s, AVG(total_usd) AS a, MIN(total_usd) AS lo, MAX(total_usd) AS hi FROM chr_orders",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 1, "rows: {:?}", res.rows);
        let r = &res.rows[0];
        assert_eq!(r.get("c"), Some(&Value::Int(5)));
        assert_eq!(r.get("s"), Some(&Value::Float(152.5)));
        assert_eq!(r.get("a"), Some(&Value::Float(30.5)));
        assert_eq!(r.get("lo"), Some(&Value::Float(10.5)));
        assert_eq!(r.get("hi"), Some(&Value::Float(50.5)));
        assert!(
            res.execution_plan.contains("Aggregate"),
            "plan: {}",
            res.execution_plan
        );

        // COUNT over an empty filtered set is 0, not an error.
        let res = sql
            .execute("SELECT COUNT(*) FROM chr_orders WHERE order_id = 999")
            .unwrap();
        assert_eq!(res.rows[0].get("COUNT(*)"), Some(&Value::Int(0)));
    }

    #[test]
    fn group_by_with_having_and_order_of_aggregates() {
        let (_e, sql) = setup();
        sql.execute("INSERT INTO chr_users (user_id = 1, username = 'u1', balance = 1.0)")
            .unwrap();
        for (id, bal, status) in [
            (1, 10.5, "OK"),
            (2, 20.5, "OK"),
            (3, 30.5, "FAIL"),
            (4, 40.5, "FAIL"),
            (5, 50.5, "FAIL"),
        ] {
            sql.execute(&format!(
                "INSERT INTO chr_orders (order_id = {}, user_id = 1, total_usd = {}, status = '{}')",
                id, bal, status
            ))
            .unwrap();
        }

        let res = sql
            .execute(
                "SELECT status, COUNT(*) AS n, SUM(total_usd) AS s FROM chr_orders GROUP BY status ORDER BY s DESC",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 2, "rows: {:?}", res.rows);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        assert_eq!(res.rows[0].get("n"), Some(&Value::Int(3)));
        assert_eq!(res.rows[0].get("s"), Some(&Value::Float(121.5)));
        assert_eq!(res.rows[1].get("status"), Some(&Value::Text("OK".into())));
        assert_eq!(res.rows[1].get("n"), Some(&Value::Int(2)));

        // HAVING filters groups by aggregate value.
        let res = sql
            .execute("SELECT status, COUNT(*) FROM chr_orders GROUP BY status HAVING COUNT(*) > 2")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        assert_eq!(res.rows[0].get("COUNT(*)"), Some(&Value::Int(3)));

        // Aggregate over a WHERE-filtered subset.
        let res = sql
            .execute(
                "SELECT status, SUM(total_usd) FROM chr_orders WHERE total_usd >= 20 GROUP BY status",
            )
            .unwrap();
        assert_eq!(res.rows.len(), 2);
        // HAVING on SUM
        let res = sql
            .execute("SELECT status FROM chr_orders GROUP BY status HAVING SUM(total_usd) > 100")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("status"), Some(&Value::Text("FAIL".into())));
        // SUM of whole-int columns stays Int (typed arithmetic, no float drift).
        let res = sql
            .execute("SELECT SUM(order_id) AS total_ids FROM chr_orders")
            .unwrap();
        assert_eq!(res.rows[0].get("total_ids"), Some(&Value::Int(15)));
    }

    #[test]
    fn null_values_stay_out_of_the_index() {
        let (_e, sql) = setup();
        sql.execute("CREATE TABLE chr_n (id INT, tag TEXT, PRIMARY KEY (id))")
            .unwrap();
        sql.execute("CREATE INDEX idx_n_tag ON chr_n (tag)")
            .unwrap();
        sql.execute("INSERT INTO chr_n (id = 1, tag = NULL)")
            .unwrap();
        sql.execute("INSERT INTO chr_n (id = 2, tag = 't')")
            .unwrap();

        let res = sql.execute("SELECT id FROM chr_n WHERE tag = 't'").unwrap();
        assert_eq!(res.rows.len(), 1);
        // NULL tag row: IS NULL still found (index only accelerates, never changes results).
        let res = sql
            .execute("SELECT id FROM chr_n WHERE tag IS NULL")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
    }

    // ---------- v4.1: SHOW STORAGE ----------

    #[test]
    fn show_storage_maps_row_to_locus_and_decodes_values() {
        let (_e, sql) = setup();
        sql.execute(
            "CREATE TABLE chr_samples (sample_id INT, label TEXT, seq DNA, \
             PRIMARY KEY (sample_id))",
        )
        .unwrap();
        sql.execute(
            "INSERT INTO chr_samples (sample_id = 7, label = 'demo', seq = 'CGCGCGATCGATCGCGC')",
        )
        .unwrap();

        // Point mode: one row per column, values decoded, chains included.
        let res = sql
            .execute("SHOW STORAGE FOR chr_samples WHERE sample_id = 7")
            .unwrap();
        assert_eq!(
            res.columns,
            vec![
                "chromosome".to_string(),
                "locus".to_string(),
                "storage_key".to_string(),
                "column".to_string(),
                "sql_type".to_string(),
                "value".to_string(),
                "stored_as".to_string(),
            ]
        );
        let by_col = |name: &str| {
            res.rows
                .iter()
                .find(|r| r.get("column") == Some(&Value::Text(name.into())))
                .unwrap()
                .clone()
        };
        assert_eq!(by_col("sample_id").get("storage_key"),
                   Some(&Value::Text("chr_samples:7".into())));
        assert_eq!(by_col("sample_id").get("sql_type"),
                   Some(&Value::Text("INT".into())));
        assert_eq!(by_col("sample_id").get("value"),
                   Some(&Value::Text("7".into())));
        assert_eq!(by_col("label").get("value"),
                   Some(&Value::Text("demo".into())));
        assert_eq!(by_col("seq").get("sql_type"),
                   Some(&Value::Text("DNA".into())));
        // The chain is shown decoded AND its physical form is described.
        assert_eq!(by_col("seq").get("value"),
                   Some(&Value::Text("CGCGCGATCGATCGCGC".into())));
        assert!(matches!(
            by_col("seq").get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 4") && s.contains("17 nt")
        ));

        // Scan mode: one summary row per stored locus.
        let res2 = sql.execute("SHOW STORAGE FOR chr_samples").unwrap();
        assert_eq!(res2.rows.len(), 1);
        assert_eq!(res2.rows[0].get("storage_key"),
                   Some(&Value::Text("chr_samples:7".into())));
        assert_eq!(res2.rows[0].get("dna_columns"),
                   Some(&Value::Int(1)));
        assert!(matches!(
            res2.rows[0].get("payload_bytes"),
            Some(Value::Int(n)) if *n > 0
        ));

        // Filtering by a non-PK column is rejected, unknown locus errors.
        assert!(sql
            .execute("SHOW STORAGE FOR chr_samples WHERE label = 'demo'")
            .is_err());
        assert!(sql
            .execute("SHOW STORAGE FOR chr_samples WHERE sample_id = 999")
            .is_err());
        assert!(sql.execute("SHOW STORAGE FOR no_such_table").is_err());
    }

    #[test]
    fn explain_storage_maps_insert_before_execution() {
        let (_e, sql) = setup();
        let map = sql
            .explain_storage(
                "INSERT INTO chr_users (user_id, username, balance) VALUES (5, 'ada', 100.0)",
            )
            .unwrap();
        assert!(map.contains("chromosome 'chr_users'"), "{}", map);
        assert!(map.contains("locus 5"), "{}", map);
        assert!(map.contains("chr_users:5"), "{}", map);
        assert!(map.contains("balance [FLOAT] = 100"), "{}", map);
        assert!(map.contains("RowCodec tag 2"), "{}", map);
    }

#[test]
fn composite_pk_delete_removes_row() {
    // Drupal re-inserts user 1 (users_field_data PK = (uid, langcode))
    // after DELETE ... WHERE uid = 1 — the delete must hit the full
    // storage key table:uid:langcode, not the truncated table:uid.
    let (_engine, sql) = setup();
    sql.execute("CREATE TABLE cpk (uid INT NOT NULL, langcode VARCHAR(12) NOT NULL, name VARCHAR(60), PRIMARY KEY (uid, langcode))").unwrap();
    sql.execute("INSERT INTO cpk (uid, langcode, name) VALUES (1, 'en', 'placeholder')").unwrap();
    sql.execute("DELETE FROM cpk WHERE uid = '1'").unwrap();
    let r = sql.execute("SELECT COUNT(*) AS n FROM cpk").unwrap();
    assert_eq!(r.rows[0]["n"].to_string(), "0", "quoted-literal delete missed");
    sql.execute("INSERT INTO cpk (uid, langcode, name) VALUES (1, 'en', 'real')").unwrap();
    let r = sql.execute("SELECT name FROM cpk WHERE uid = 1").unwrap();
    assert_eq!(r.rows[0]["name"].to_string(), "real");
}

#[test]
fn insert_without_column_list_maps_positionally() {
    let (_engine, sql) = setup();
    sql.execute("CREATE TABLE posl (a INT, b TEXT, c INT, PRIMARY KEY (a))").unwrap();
    sql.execute("INSERT INTO posl VALUES (1, 'x', 7)").unwrap();
    let r = sql.execute("SELECT a, b, c FROM posl WHERE a = 1").unwrap();
    assert_eq!(r.rows[0]["b"].to_string(), "x");
    assert_eq!(r.rows[0]["c"].to_string(), "7");
    // count mismatch must error
    assert!(sql.execute("INSERT INTO posl VALUES (2, 'y')").is_err());
}

#[test]
fn derived_table_zero_rows_still_reports_columns() {
    // MediaWiki RevisionStore::findIdenticalRevision: a zero-row derived
    // table (LIMIT offset skips everything) must still produce a
    // RowDescription — mysqli dies with "numRows() on true" otherwise.
    let (_engine, sql) = setup();
    let _ = sql.execute("CREATE TABLE dtz (id INT PRIMARY KEY, v INT)");
    let _ = sql.execute("INSERT INTO dtz VALUES (1, 10)");
    let r = sql
        .execute(
            "SELECT * FROM (SELECT id, v FROM dtz ORDER BY id DESC LIMIT 1,15) recent \
             WHERE id = 1 LIMIT 1",
        )
        .unwrap();
    assert_eq!(r.columns, vec!["id".to_string(), "v".to_string()]);
    assert!(r.rows.is_empty());
}
}

#[cfg(test)]
mod acl_parse_tests {
    use super::referenced_tables;

    #[test]
    fn collects_select_join_and_subquery_tables() {
        let tables = referenced_tables(
            "SELECT u.username FROM chr_users u JOIN chr_orders o ON u.user_id = o.user_id \
             WHERE u.user_id IN (SELECT user_id FROM chr_bans)",
        )
        .unwrap();
        assert_eq!(
            tables,
            vec!["chr_bans".to_string(), "chr_orders".to_string(), "chr_users".to_string()]
        );
    }

    #[test]
    fn collects_create_table_foreign_keys() {
        let tables = referenced_tables(
            "CREATE TABLE chr_orders (id INT, user_id INT, PRIMARY KEY (id), \
             FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        assert!(tables.contains(&"chr_orders".to_string()));
        assert!(tables.contains(&"chr_users".to_string()));
    }
}

/// Regression tests for the v3.5.0 audit fixes (P0-1, P0-2, P1).
#[cfg(test)]
mod security_regression_tests {
    use super::*;


    #[test]
    fn parser_rejects_pathologically_nested_expressions() {
        // 100k nested parentheses used to recurse once per '(' and smash the
        // stack (with panic=abort that killed the WHOLE server). Now it is a
        // plain parse error.
        let deep = format!(
            "SELECT * FROM chr_t WHERE {} 1 = 1 {}",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        let err = referenced_tables(&deep)
            .err()
            .expect("pathologically deep query must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parser_rejects_deeply_nested_subqueries() {
        let mut query = String::new();
        for _ in 0..2_000 {
            query.push_str("SELECT a FROM chr_t WHERE a IN (");
        }
        query.push_str("1 = 1");
        for _ in 0..2_000 {
            query.push(')');
        }
        let err = referenced_tables(&query)
            .err()
            .expect("deep subquery nesting must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bare_drop_index_is_rejected_for_permission_checking() {
        // The bare form does not name its table, so it cannot be
        // permission-checked without storage access; the ACL resolver must
        // reject it instead of silently authorising a drop on ANY table.
        let err = referenced_tables("DROP INDEX idx_x")
            .err()
            .expect("bare DROP INDEX must not pass the ACL resolver");
        assert!(err.contains("ON <table>"), "unexpected error: {err}");
        // The documented ON form keeps working.
        let tables = referenced_tables("DROP INDEX idx_x ON chr_t").unwrap();
        assert_eq!(tables, vec!["chr_t".to_string()]);
    }

    #[test]
    fn expr_to_sql_keeps_operator_precedence() {
        // a * (b + c) must not render as `a * b + c` (silently wrong
        // materialised subquery results).
        let e = Expr::Arith {
            op: ArithOp::Mul,
            left: Box::new(Expr::Column("a".into())),
            right: Box::new(Expr::Arith {
                op: ArithOp::Add,
                left: Box::new(Expr::Column("b".into())),
                right: Box::new(Expr::Column("c".into())),
            }),
        };
        assert_eq!(expr_to_sql(&e), "a * (b + c)");

        // Left-associative same-precedence chain stays parenthesised.
        let e2 = Expr::Arith {
            op: ArithOp::Sub,
            left: Box::new(Expr::Arith {
                op: ArithOp::Sub,
                left: Box::new(Expr::Column("a".into())),
                right: Box::new(Expr::Column("b".into())),
            }),
            right: Box::new(Expr::Column("c".into())),
        };
        assert_eq!(expr_to_sql(&e2), "(a - b) - c");

        // Higher-precedence child needs no parens.
        let e3 = Expr::Arith {
            op: ArithOp::Add,
            left: Box::new(Expr::Column("a".into())),
            right: Box::new(Expr::Arith {
                op: ArithOp::Mul,
                left: Box::new(Expr::Column("b".into())),
                right: Box::new(Expr::Column("c".into())),
            }),
        };
        assert_eq!(expr_to_sql(&e3), "a + b * c");
    }

    #[test]
    fn hash_join_null_never_matches_and_int_matches_float() {
        fn row(col: &str, v: Value) -> BTreeMap<String, Value> {
            let mut m = BTreeMap::new();
            m.insert(col.to_string(), v);
            m
        }
        let left = vec![
            row("id", Value::Null),
            row("id", Value::Int(5)),
            row("id", Value::Int(7)),
        ];
        let right = vec![
            row("id", Value::Null),
            row("id", Value::Float(5.0)),
            row("id", Value::Int(5)),
        ];
        // Int(5) joins Float(5.0) and Int(5) => 2 result rows; NULL never
        // joins anything (not even NULL = NULL), Int(7) has no rival.
        let joined = hash_join(&left, &right, "id", "id");
        assert_eq!(joined.len(), 2);

        // LEFT join = 2 matched pairs + the NULL-keyed row + the Int(7) row
        // (both kept with NULLs on the right side).
        let left_joined = hash_join_left(&left, &right, "id", "id");
        assert_eq!(left_joined.len(), 4);
    }
}
