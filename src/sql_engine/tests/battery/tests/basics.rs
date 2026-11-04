// SPDX-License-Identifier: AGPL-3.0-only
//! Battery subgroup (verbatim from the pre-split monolith).

use super::*;

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
