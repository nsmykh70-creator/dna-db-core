// SPDX-License-Identifier: AGPL-3.0-only
//! Verbatim module from the pre-split monolith.

use super::*;

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

    #[test]
    fn hash_line_comments_are_whitespace() {
        use crate::engine::{Engine, EngineConfig};
        use crate::wal::SyncMode;
        let dir = std::env::temp_dir().join(format!(
            "dna_sql_hash_comments_{}",
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
        // Joomla's base.sql (like classic mysqldump output) uses `#` line
        // comments; the lexer must skip them like `--` and `/* */`.
        sql.execute(
            "# header comment\nCREATE TABLE chr_hash_comments (id INT PRIMARY KEY, note TEXT) # table opts tail\n",
        )
        .unwrap();
        sql.execute("INSERT INTO chr_hash_comments (id, note) VALUES (1, 'ok') # done").unwrap();
        let res = sql
            .execute("SELECT note FROM chr_hash_comments WHERE id = 1")
            .unwrap();
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].get("note"), Some(&Value::Text("ok".into())));
    }
}
