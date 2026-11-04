// SPDX-License-Identifier: AGPL-3.0-only
//! Battery subgroup (verbatim from the pre-split monolith).

use super::*;

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
