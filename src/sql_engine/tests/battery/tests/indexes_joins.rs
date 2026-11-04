// SPDX-License-Identifier: AGPL-3.0-only
//! Battery subgroup (verbatim from the pre-split monolith).

use super::*;

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
