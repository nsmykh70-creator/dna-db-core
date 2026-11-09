// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL tests: end-to-end engine execution, ACL table extraction and the
//! v3.5.0 security regression tests (parser depth guard, TX overflow).

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_ql_{}_{}",
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

    fn setup() -> (Arc<Engine>, DnaQlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("ql"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: crate::wal::SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();

        // chr_variants: loci with sequences
        let variants = vec![
            (1, "BRCA1_exon2", "ATGCGATCGATCGACCATGCGATCGATCATGCGATCAATC"),
            (
                2,
                "TP53_promoter",
                "GGCCTTAAGGCCTTAACCGGATGCATCGAATTCGAGCTAG",
            ),
            (
                3,
                "CYP2D6_poly",
                "TTAACCGGATCGGATCCGTAAGCTTCGGATCCAAGCTTACG",
            ),
        ];
        for (id, name, seq) in variants {
            let mut row = BTreeMap::new();
            row.insert("variant_id".to_string(), Value::Int(id));
            row.insert("variant_name".to_string(), Value::Text(name.to_string()));
            row.insert("sequence".to_string(), Value::Dna(seq.to_string()));
            row.insert("patient_id".to_string(), Value::Int(id * 10));
            let key = format!("chr_variants:{}", id);
            let payload = RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            engine.put(key.as_bytes(), &payload).unwrap();
        }

        // chr_patients: link targets
        for i in 10..40i64 {
            let mut row = BTreeMap::new();
            row.insert("patient_id".to_string(), Value::Int(i));
            row.insert("name".to_string(), Value::Text(format!("patient_{}", i)));
            let key = format!("chr_patients:{}", i);
            let payload = RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );
            engine.put(key.as_bytes(), &payload).unwrap();
        }

        let sql = Arc::new(SqlEngine::new(engine.clone()));
        let ql = DnaQlEngine::new(engine.clone(), sql);
        (engine, ql)
    }

    #[test]
    fn quick_search_finds_exact_and_fuzzy() {
        let (_e, ql) = setup();
        // exact match: GATCGATC appears in variant 1 sequence
        let res = ql.execute("chr_variants ? 'GATCGATC' ~0").unwrap();
        assert!(
            res.rows.len() >= 1,
            "expected matches, got {:?}",
            res.ast_summary
        );
        assert!(res.engine_note.contains("real scan"));

        // with 1 mismatch, should find MORE or equal
        let res2 = ql.execute("chr_variants ? 'GATCGATC' ~1").unwrap();
        assert!(res2.rows.len() >= res.rows.len());

        // Hamming mode
        let res3 = ql.execute("chr_variants ? 'GATCGATC' ~1").unwrap();
        assert!(!res3.rows.is_empty());
    }

    // ---------- v4.1: STORAGE_MAP ----------

    #[test]
    fn storage_map_decodes_locus_without_sequence() {
        let (_e, ql) = setup();
        // chr_patients rows have NO sequence field — STORAGE_MAP must still
        // produce the full per-column map (unlike the sequence bio functions).
        let res = ql
            .execute("ANALYZE LOCUS chr_patients:12 |> STORAGE_MAP();")
            .unwrap();
        assert_eq!(res.rows.len(), 2, "{:?}", res.engine_note);
        let by_col = |name: &str| {
            res.rows
                .iter()
                .find(|r| r.fields.get("column") == Some(&Value::Text(name.into())))
                .unwrap()
        };
        assert_eq!(
            by_col("patient_id").fields.get("value"),
            Some(&Value::Text("12".into()))
        );
        assert_eq!(
            by_col("patient_id").fields.get("sql_type"),
            Some(&Value::Text("INT".into()))
        );
        assert_eq!(
            by_col("patient_id").fields.get("storage_key"),
            Some(&Value::Text("chr_patients:12".into()))
        );
        assert!(matches!(
            by_col("patient_id").fields.get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 1")
        ));
        assert_eq!(
            by_col("name").fields.get("value"),
            Some(&Value::Text("patient_12".into()))
        );
        assert!(res.engine_note.contains("storage map of locus chr_patients:12"));
    }

    #[test]
    fn storage_map_shows_dna_chain_and_rejects_misuse() {
        let (_e, ql) = setup();
        let res = ql
            .execute("ANALYZE LOCUS chr_variants:1 |> STORAGE_MAP();")
            .unwrap();
        let seq_row = res
            .rows
            .iter()
            .find(|r| r.fields.get("column") == Some(&Value::Text("sequence".into())))
            .unwrap();
        assert_eq!(
            seq_row.fields.get("value"),
            Some(&Value::Text("ATGCGATCGATCGACCATGCGATCGATCATGCGATCAATC".into()))
        );
        assert!(matches!(
            seq_row.fields.get("stored_as"),
            Some(Value::Text(s)) if s.contains("tag 4")
        ));

        // Needs a LOCUS, not a literal SEQUENCE.
        assert!(ql
            .execute("ANALYZE SEQUENCE 'ACGT' |> STORAGE_MAP();")
            .is_err());
        // Cannot be combined with the sequence functions.
        assert!(ql
            .execute("ANALYZE LOCUS chr_variants:1 |> STORAGE_MAP() |> GC_CONTENT();")
            .is_err());
        // Unknown locus is a clean error.
        assert!(ql.execute("ANALYZE LOCUS chr_variants:999 |> STORAGE_MAP();").is_err());
    }

    #[test]
    fn standard_search_with_link() {
        let (_e, ql) = setup();
        let q = "FIND PATTERN 'ATGCGATCGATC' \
                 IN CHROMOSOME chr_variants \
                 WHERE HAMMING_DISTANCE <= 2 \
                 LINK chr_patients VIA COMPLEMENT(patient_id) \
                 EMIT locus, offset_bp, mismatches, name";
        let res = ql.execute(q).unwrap();
        assert!(!res.rows.is_empty());
        // linked patient name present
        let has_link = res.rows.iter().any(|r| r.fields.contains_key("name"));
        assert!(
            has_link,
            "link fields missing: {:?}",
            res.rows[0].fields.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn pipeline_where_and_emit() {
        let (_e, ql) = setup();
        let q = "FROM chr_variants |> WHERE variant_id >= 2 |> EMIT variant_id, variant_name";
        let res = ql.execute(q).unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(res.engine_note.contains("Filter"));
        // projection applied
        for r in &res.rows {
            assert!(r.fields.contains_key("variant_id"));
            assert!(!r.fields.contains_key("sequence"));
        }
    }

    #[test]
    fn tx_block_mutates_atomically() {
        let (e, ql) = setup();
        // seed a balance row
        let mut row = BTreeMap::new();
        row.insert("balance".to_string(), Value::Float(1000.0));
        let payload = RowCodec::encode(&[("balance".to_string(), Value::Float(1000.0))]);
        e.put(b"chr_accounts:14", &payload).unwrap();

        let q = "TX { \
                   chr_accounts:14.balance -= 350.00; \
                   chr_orders += { order_id: 9021, user_id: 14, total_usd: 350.00 }; \
                 }";
        let res = ql.execute(q).unwrap();
        assert!(
            res.engine_note.contains("atomically"),
            "{}",
            res.engine_note
        );

        // balance updated
        let raw = e.get(b"chr_accounts:14").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        let bal = fields.iter().find(|(k, _)| k == "balance").unwrap();
        assert_eq!(bal.1, Value::Float(650.0));

        // order created
        let raw2 = e.get(b"chr_orders:9021").unwrap().unwrap();
        let fields2 = RowCodec::decode(&raw2).unwrap();
        assert!(fields2
            .iter()
            .any(|(k, v)| k == "order_id" && *v == Value::Int(9021)));
    }

    #[test]
    fn bio_analysis_real_values() {
        let (_e, ql) = setup();
        let q = "ANALYZE SEQUENCE 'ATGGCCTAA' |> GC_CONTENT() |> MELTING_TEMP() |> TRANSLATE()";
        let res = ql.execute(q).unwrap();
        assert_eq!(res.rows.len(), 3);
        let gc = &res.rows[0];
        assert_eq!(
            gc.fields.get("result"),
            Some(&Value::Text("44.44%".to_string()))
        );
        let tr = &res.rows[2];
        assert_eq!(
            tr.fields.get("result"),
            Some(&Value::Text("MA".to_string()))
        );
    }

    #[test]
    fn parse_errors_are_informative() {
        let (_e, ql) = setup();
        assert!(ql.execute("BOGUS STATEMENT").is_err());
        assert!(ql.execute("chr ? 'ATGCGN' ~1").is_err()); // N not allowed
        assert!(ql.execute("chr ? 'ATGC").is_err()); // unterminated
    }

    #[test]
    fn drop_locus_removes_row() {
        let (e, ql) = setup();
        let q = "TX { DROP LOCUS chr_variants:3 }";
        ql.execute(q).unwrap();
        assert!(e.get(b"chr_variants:3").unwrap().is_none());
        // survives restart
        drop(ql);
        let e2 = Engine::open(EngineConfig {
            dir_path: e.data_dir().to_path_buf(),
            ..Default::default()
        })
        .unwrap();
        assert!(e2.get(b"chr_variants:3").unwrap().is_none());
        assert!(e2.get(b"chr_variants:1").unwrap().is_some());
    }
}

/// v4.3 pipeline conformance stages: SKIP, JOIN, GROUP (aggregates+HAVING),
/// EMIT expressions with aliases, NOT/LIKE, SHOW CHROMOSOMES and DDL
/// passthrough — the SQL-parity track of the DnaQL grammar.
#[cfg(test)]
mod pipeline_conformance_tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_qlp_{}_{}",
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

    fn setup() -> (Arc<Engine>, Arc<SqlEngine>, DnaQlEngine) {
        let engine = Engine::open(EngineConfig {
            dir_path: tempdir("qlp"),
            memtable_capacity: 4 * 1024 * 1024,
            sync_mode: crate::wal::SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let sql = Arc::new(SqlEngine::new(engine.clone()));
        sql.execute("CREATE TABLE chr_users (user_id INT, username TEXT, PRIMARY KEY (user_id))")
            .unwrap();
        sql.execute("CREATE TABLE chr_orders (order_id INT, user_id INT, total_usd FLOAT, status TEXT, PRIMARY KEY (order_id))")
            .unwrap();
        sql.execute("INSERT INTO chr_users (user_id, username) VALUES (1, 'ada'), (2, 'grace'), (3, 'alan')")
            .unwrap();
        sql.execute(
            "INSERT INTO chr_orders (order_id, user_id, total_usd, status) VALUES \
             (10, 1, 50.0, 'paid'), (11, 1, 20.0, 'paid'), (12, 2, 75.0, 'open')",
        )
        .unwrap();
        let ql = DnaQlEngine::new(engine.clone(), sql.clone());
        (engine, sql, ql)
    }

    #[test]
    fn skip_stage_is_sql_offset() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute("FROM chr_users |> SORT user_id ASC |> SKIP 1 |> EMIT user_id;")
            .unwrap();
        let ids: Vec<i64> = r
            .rows
            .iter()
            .map(|r| match r.fields.get("user_id") {
                Some(Value::Int(i)) => *i,
                other => panic!("bad user_id: {:?}", other),
            })
            .collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn inner_join_merges_and_filters() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute(
                "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
                 |> WHERE status = 'paid' |> EMIT username, total_usd;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2, "ada has two paid orders");
        for row in &r.rows {
            assert!(row.fields.contains_key("username"));
            assert!(row.fields.contains_key("total_usd"));
        }
        // Spec §4.2 naming: bare name first, then chr.field, then link_.
        // Non-colliding columns resolve bare (SQL row model parity).
        let r = ql
            .execute(
                "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
                 |> WHERE total_usd > 60 |> EMIT username;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("username"), Some(&Value::Text("grace".into())));
        // Collided joined columns are addressable under the qualified
        // fallback (user_id exists on both sides).
        let r = ql
            .execute(
                "FROM chr_users |> JOIN chr_orders ON user_id = user_id \
                 |> WHERE chr_orders.user_id = 2 |> EMIT username;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("username"), Some(&Value::Text("grace".into())));
    }

    #[test]
    fn left_join_null_extends_unmatched() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute(
                "FROM chr_users |> LEFT JOIN chr_orders ON user_id = user_id \
                 |> EMIT username;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 4, "ada twice + grace + unmatched alan");
        assert!(r
            .rows
            .iter()
            .any(|r| r.fields.get("username") == Some(&Value::Text("alan".into()))));
    }

    #[test]
    fn group_stage_aggregates_and_having() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute(
                "FROM chr_orders |> GROUP user_id, COUNT(*) AS n, SUM(total_usd) AS total \
                 HAVING COUNT(*) > 1 |> EMIT user_id, n;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("user_id"), Some(&Value::Int(1)));
        assert_eq!(r.rows[0].fields.get("n"), Some(&Value::Int(2)));
        // Global aggregation without group columns.
        let r = ql.execute("FROM chr_orders |> GROUP COUNT(*), AVG(total_usd);").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("COUNT(*)"), Some(&Value::Int(3)));
        match r.rows[0].fields.get("AVG(total_usd)") {
            Some(Value::Float(f)) => assert!((f - (50.0 + 20.0 + 75.0) / 3.0).abs() < 1e-9),
            other => panic!("bad avg: {:?}", other),
        }
    }

    #[test]
    fn emit_expressions_and_aliases() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute(
                "FROM chr_users |> WHERE user_id = 1 |> EMIT username AS name, user_id * 10 AS dec_id;",
            )
            .unwrap();
        assert_eq!(r.rows[0].fields.get("name"), Some(&Value::Text("ada".into())));
        assert_eq!(r.rows[0].fields.get("dec_id"), Some(&Value::Int(10)));
        // Missing columns project as NULL (SQL row shapes).
        let r = ql
            .execute("FROM chr_users |> WHERE user_id = 1 |> EMIT no_such_col;")
            .unwrap();
        assert_eq!(r.rows[0].fields.get("no_such_col"), Some(&Value::Null));
    }

    #[test]
    fn not_like_and_sortby_expressions() {
        let (_e, _s, ql) = setup();
        let r = ql
            .execute(
                "FROM chr_users |> WHERE NOT username LIKE 'a%' |> EMIT username;",
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].fields.get("username"), Some(&Value::Text("grace".into())));
        // NOT IN + computed sort (descending by arithmetic key).
        let r = ql
            .execute(
                "FROM chr_users |> WHERE user_id NOT IN (3) |> SORT user_id * -1 ASC |> EMIT username;",
            )
            .unwrap();
        let names: Vec<&Value> = r.rows.iter().map(|r| r.fields.get("username").unwrap()).collect();
        assert_eq!(names, vec![&Value::Text("grace".into()), &Value::Text("ada".into())]);
    }

    #[test]
    fn show_chromosomes_and_ddl_passthrough() {
        let (_e, sql, ql) = setup();
        let r = ql.execute("SHOW CHROMOSOMES;").unwrap();
        let mut names: Vec<String> = r
            .rows
            .iter()
            .map(|r| match r.fields.get("chromosome") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("bad chromosome: {:?}", other),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["chr_orders", "chr_users"]);
        // Same list as SQL SHOW TABLES — the consoles can never disagree.
        let mut sql_names = sql.list_tables();
        sql_names.sort();
        assert_eq!(names, sql_names);

        let r = ql.execute("SHOW CHROMOSOMES LIKE 'chr_o%';").unwrap();
        assert_eq!(r.rows.len(), 1);

        // DDL passthrough executes on the shared SQL engine.
        ql.execute("CREATE INDEX idx_orders_user ON chr_orders (user_id);").unwrap();
        let plan = sql
            .execute("SELECT * FROM chr_orders WHERE user_id = 1")
            .unwrap();
        assert!(plan.execution_plan.contains("Index"), "index must be visible: {}", plan.execution_plan);
        ql.execute("DROP INDEX idx_orders_user ON chr_orders;").unwrap();
        let plan = sql
            .execute("SELECT * FROM chr_orders WHERE user_id = 1")
            .unwrap();
        assert!(plan.execution_plan.contains("Seq"), "index dropped: {}", plan.execution_plan);
        // ALTER passthrough.
        ql.execute("ALTER CHROMOSOME chr_users ADD COLUMN note TEXT DEFAULT 'x';").unwrap();
        let schema = sql.get_schema("chr_users").unwrap();
        assert!(schema.columns.contains_key("note"));
        ql.execute("ALTER CHROMOSOME chr_users DROP COLUMN note;").unwrap();
        assert!(sql.get_schema("chr_users").unwrap().columns.get("note").is_none());
    }

    #[test]
    fn acl_sees_join_target_and_ddl_tables() {
        use super::referenced_tables;
        assert_eq!(
            referenced_tables("FROM chr_users |> JOIN chr_orders ON user_id = user_id")
                .unwrap(),
            vec!["chr_orders".to_string(), "chr_users".to_string()]
        );
        assert_eq!(
            referenced_tables("CREATE INDEX i ON chr_orders (user_id);").unwrap(),
            vec!["chr_orders".to_string()]
        );
    }
}
#[cfg(test)]
mod acl_parse_tests {
    use super::referenced_tables;

    #[test]
    fn collects_pipeline_and_link_tables() {
        let tables = referenced_tables(
            "FROM chr_users |> LINK chr_orders VIA COMPLEMENT(user_id)",
        )
        .unwrap();
        assert_eq!(tables, vec!["chr_orders".to_string(), "chr_users".to_string()]);
    }

    #[test]
    fn collects_transaction_tables() {
        let tables = referenced_tables("TX { chr_variants += { id: 1 } }").unwrap();
        assert_eq!(tables, vec!["chr_variants".to_string()]);
    }
}

/// Regression tests for the v3.5.0 audit fixes (P0-1, P0-2, TX += overflow).
#[cfg(test)]
mod security_regression_tests {
    use super::*;

    #[test]
    fn parser_rejects_pathologically_nested_expressions() {
        let deep = format!("({}a = 1{})", "(".repeat(100_000), ")".repeat(100_000));
        let toks = tokenize(&deep).unwrap();
        let mut p = Parser::new(toks);
        let err = p
            .parse_expr()
            .err()
            .expect("pathologically deep expression must be rejected");
        assert!(
            err.contains("nested too deeply"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn referenced_tables_includes_bioanalysis_locus_table() {
        // ANALYZE LOCUS reads a row of the named table, so the per-user ACL
        // must see it (previously an empty list => ACL bypass).
        let tables = referenced_tables("ANALYZE LOCUS chr_virus:42 |> GC_CONTENT()").unwrap();
        assert_eq!(tables, vec!["chr_virus".to_string()]);
        // A literal-sequence analysis touches no table at all.
        let tables = referenced_tables("ANALYZE SEQUENCE 'ACGT' |> GC_CONTENT()").unwrap();
        assert!(tables.is_empty());
    }

    #[test]
    fn tx_assign_overflow_is_rejected_not_wrapped() {
        // A durable mutation must never silently wrap i64: MAX + 1 would
        // turn a persisted counter/balance negative.
        assert!(apply_assign(&Value::Int(i64::MAX), AssignOp::Add, &Value::Int(1)).is_err());
        assert!(apply_assign(&Value::Int(i64::MIN), AssignOp::Sub, &Value::Int(1)).is_err());
        assert_eq!(
            apply_assign(&Value::Int(2), AssignOp::Add, &Value::Int(3)).unwrap(),
            Value::Int(5)
        );
        assert_eq!(
            apply_assign(&Value::Int(5), AssignOp::Sub, &Value::Int(2)).unwrap(),
            Value::Int(3)
        );
    }
}

