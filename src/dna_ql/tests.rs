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
