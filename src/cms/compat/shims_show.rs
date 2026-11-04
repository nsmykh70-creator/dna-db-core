// SPDX-License-Identifier: AGPL-3.0-only
//! SHOW-statement shims: SHOW TABLES [LIKE], SHOW FULL COLUMNS, SHOW INDEX,
//! SHOW ENGINES / CHARACTER SET / COLLATION / VARIABLES / WARNINGS and the
//! column/index catalog row builders backing them.

use std::collections::BTreeMap;

use super::*;
use crate::relational::Value;

impl CmsSqlGate {
    pub(crate) fn try_show_shims(
        &self,
        session: u64,
        sql: &str,
        upper: &str,
    ) -> Result<Option<CmsQueryResult>, CmsError> {
        // SHOW DATABASES [LIKE 'pat'] — generic MySQL clients (phpMyAdmin,
        // mysqldump, GUI tools) list catalogs this way before connecting.
        // DNA-DB is a single-catalog engine: the session database plus the
        // always-present information_schema pseudo-catalog.
        if upper.starts_with("SHOW DATABASES") {
            let like = find_like_pattern(sql).map(|p| decode_mysql_string(&p));
            let current = self
                .with_session(session, |s| s.database.clone())
                .unwrap_or(None)
                .unwrap_or_else(|| "wordpress".to_string());
            let mut names: std::collections::BTreeSet<String> = self
                .databases
                .lock()
                .expect("database set poisoned")
                .iter()
                .cloned()
                .collect();
            names.insert("information_schema".into());
            names.insert(current);
            let mut rows = Vec::new();
            for name in names {
                if let Some(pat) = &like {
                    if !like_match(pat, &name) {
                        continue;
                    }
                }
                let mut r = BTreeMap::new();
                r.insert("Database".into(), Value::Text(name));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec!["Database".into()],
                rows,
                message: Some(format!("{} database(s)", found)),
                plan: "cms: SHOW DATABASES shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW ENGINES — Drupal's requirements page probes engine support.
        if upper.starts_with("SHOW ENGINES") {
            let mut rows = Vec::new();
            for (engine, support, tx) in [
                ("InnoDB", "DEFAULT", "YES"),
                ("MyISAM", "YES", "NO"),
                ("MEMORY", "YES", "NO"),
                ("CSV", "YES", "NO"),
            ] {
                let mut r = BTreeMap::new();
                r.insert("Engine".into(), Value::Text(engine.into()));
                r.insert("Support".into(), Value::Text(support.into()));
                r.insert(
                    "Comment".into(),
                    Value::Text("Supported by DNA-DB compat".into()),
                );
                r.insert("Transactions".into(), Value::Text(tx.into()));
                r.insert("XA".into(), Value::Text("NO".into()));
                r.insert("Savepoints".into(), Value::Text("NO".into()));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Engine".into(),
                    "Support".into(),
                    "Comment".into(),
                    "Transactions".into(),
                    "XA".into(),
                    "Savepoints".into(),
                ],
                rows,
                message: Some(format!("{} engine(s)", found)),
                plan: "cms: SHOW ENGINES shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW CHARACTER SET / SHOW CHARSET [LIKE 'pat'] — Drupal and Joomla
        // read the default collation through this catalog.
        if upper.starts_with("SHOW CHARACTER SET") || upper.starts_with("SHOW CHARSET") {
            let like = find_like_pattern(sql).map(|p| decode_mysql_string(&p));
            let mut rows = Vec::new();
            for (cs, desc, coll, maxlen) in [
                ("utf8mb4", "UTF-8 Unicode", "utf8mb4_general_ci", 4),
                ("utf8", "UTF-8 Unicode", "utf8_general_ci", 3),
                ("latin1", "cp1252 West European", "latin1_swedish_ci", 1),
                ("binary", "Binary pseudo charset", "binary", 1),
            ] {
                if let Some(pat) = &like {
                    if !like_match(pat, cs) {
                        continue;
                    }
                }
                let mut r = BTreeMap::new();
                r.insert("Charset".into(), Value::Text(cs.into()));
                r.insert("Description".into(), Value::Text(desc.into()));
                r.insert("Default collation".into(), Value::Text(coll.into()));
                r.insert("Maxlen".into(), Value::Int(maxlen));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Charset".into(),
                    "Description".into(),
                    "Default collation".into(),
                    "Maxlen".into(),
                ],
                rows,
                message: Some(format!("{} charset(s)", found)),
                plan: "cms: SHOW CHARACTER SET shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW COLLATION [LIKE 'pat']
        if upper.starts_with("SHOW COLLATION") {
            let like = find_like_pattern(sql).map(|p| decode_mysql_string(&p));
            let mut rows = Vec::new();
            for (coll, cs, default) in [
                ("utf8mb4_general_ci", "utf8mb4", true),
                ("utf8mb4_bin", "utf8mb4", false),
                ("utf8_general_ci", "utf8", true),
                ("latin1_swedish_ci", "latin1", true),
                ("binary", "binary", true),
            ] {
                if let Some(pat) = &like {
                    if !like_match(pat, coll) {
                        continue;
                    }
                }
                let mut r = BTreeMap::new();
                r.insert("Collation".into(), Value::Text(coll.into()));
                r.insert("Charset".into(), Value::Text(cs.into()));
                r.insert("Id".into(), Value::Int(45));
                r.insert(
                    "Default".into(),
                    Value::Text(if default { "Yes" } else { "" }.into()),
                );
                r.insert("Compiled".into(), Value::Text("Yes".into()));
                r.insert("Sortlen".into(), Value::Int(1));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Collation".into(),
                    "Charset".into(),
                    "Id".into(),
                    "Default".into(),
                    "Compiled".into(),
                    "Sortlen".into(),
                ],
                rows,
                message: Some(format!("{} collation(s)", found)),
                plan: "cms: SHOW COLLATION shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW TABLES [LIKE 'pat'] — engine's own SHOW TABLES has no LIKE.
        if upper.starts_with("SHOW TABLES") {
            // The pattern is a MySQL string literal: decode escapes first
            // ('wp\\_%' → wp\_% — one backslash, as LIKE expects).
            let like = find_like_pattern(sql).map(|p| decode_mysql_string(&p));
            let db = self
                .with_session(session, |s| s.database.clone().unwrap_or_default())
                .unwrap_or_default();
            let label = if db.is_empty() {
                "Tables_in_wordpress".to_string()
            } else {
                format!("Tables_in_{}", db)
            };
            let mut rows = Vec::new();
            for t in self.engine.list_tables() {
                if let Some(pat) = &like {
                    if !like_match(pat, &t) {
                        continue;
                    }
                }
                let mut r = BTreeMap::new();
                r.insert(label.clone(), Value::Text(t));
                rows.push(r);
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![label],
                rows,
                message: Some(format!("{} table(s)", found)),
                plan: "cms: SHOW TABLES [LIKE] via catalog".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW [FULL] COLUMNS|FIELDS FROM <table>
        if let Some(table) = show_target(
            sql,
            upper,
            &[
                "SHOW FULL COLUMNS FROM",
                "SHOW COLUMNS FROM",
                "SHOW FULL FIELDS FROM",
                "SHOW FIELDS FROM",
            ],
        ) {
            let rows = self.show_columns_rows(&table);
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Field".into(),
                    "Type".into(),
                    "Collation".into(),
                    "Null".into(),
                    "Key".into(),
                    "Default".into(),
                    "Extra".into(),
                    "Privileges".into(),
                    "Comment".into(),
                ],
                rows,
                message: Some(format!("{} column(s)", found)),
                plan: format!("cms: SHOW COLUMNS overlay for {}", table),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW INDEX|INDEXES|KEYS FROM <table>
        if let Some(table) = show_target(
            sql,
            upper,
            &["SHOW INDEX FROM", "SHOW INDEXES FROM", "SHOW KEYS FROM"],
        ) {
            let rows = self.show_index_rows(&table);
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec![
                    "Table".into(),
                    "Non_unique".into(),
                    "Key_name".into(),
                    "Seq_in_index".into(),
                    "Column_name".into(),
                    "Collation".into(),
                    "Cardinality".into(),
                    "Sub_part".into(),
                    "Packed".into(),
                    "Null".into(),
                    "Index_type".into(),
                    "Comment".into(),
                    "Index_comment".into(),
                    "Ignored".into(),
                ],
                rows,
                message: Some(format!("{} index row(s)", found)),
                plan: format!("cms: SHOW INDEX overlay for {}", table),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }

        // SHOW VARIABLES LIKE 'pat' — a canned, CMS-relevant subset.
        if upper.starts_with("SHOW VARIABLES") {
            let pat = find_like_pattern(sql).unwrap_or("%".to_string());
            let vars: &[(&str, &str)] = &[
                ("version", MYSQL_COMPAT_VERSION),
                (
                    "version_comment",
                    "DNA-DB CMS Edition (MySQL 8.0 compatible)",
                ),
                ("character_set_client", "utf8mb4"),
                ("character_set_connection", "utf8mb4"),
                ("character_set_results", "utf8mb4"),
                ("character_set_server", "utf8mb4"),
                ("collation_connection", "utf8mb4_unicode_520_ci"),
                ("collation_server", "utf8mb4_unicode_520_ci"),
                ("max_allowed_packet", "1073741824"),
                ("sql_mode", ""),
                ("autocommit", "1"),
                ("innodb_lock_wait_timeout", "50"),
                ("wait_timeout", "28800"),
            ];
            let mut rows = Vec::new();
            for (k, v) in vars {
                if like_match(&pat, k) {
                    let mut r = BTreeMap::new();
                    r.insert("Variable_name".into(), Value::Text(k.to_string()));
                    r.insert("Value".into(), Value::Text(v.to_string()));
                    rows.push(r);
                }
            }
            let found = rows.len() as u64;
            return Ok(Some(CmsQueryResult {
                columns: vec!["Variable_name".into(), "Value".into()],
                rows,
                message: None,
                plan: "cms: SHOW VARIABLES canned subset".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }));
        }
        Ok(None)
    }

    /// SHOW FULL COLUMNS rows: overlay when registered, else derived from
    /// the engine schema (coarser types, documented fallback).
    pub(crate) fn show_columns_rows(&self, table: &str) -> Vec<BTreeMap<String, Value>> {
        if let Some(meta) = self.table_meta(table) {
            return meta.show_full_columns_rows();
        }
        match self.engine.get_schema(table) {
            Some(schema) => {
                let mut rows = Vec::new();
                for (name, ty) in &schema.columns {
                    let mut r = BTreeMap::new();
                    let mysql = fallback_type(ty);
                    r.insert("Field".into(), Value::Text(name.clone()));
                    r.insert("Type".into(), Value::Text(mysql.clone()));
                    r.insert(
                        "Collation".into(),
                        if mysql.contains("char") || mysql == "text" {
                            Value::Text("utf8mb4_unicode_520_ci".into())
                        } else {
                            Value::Null
                        },
                    );
                    r.insert("Null".into(), Value::Text("YES".into()));
                    r.insert(
                        "Key".into(),
                        Value::Text(
                            if *name == schema.primary_locus_column {
                                "PRI"
                            } else {
                                ""
                            }
                            .into(),
                        ),
                    );
                    r.insert("Default".into(), Value::Null);
                    r.insert("Extra".into(), Value::Text(String::new()));
                    r.insert(
                        "Privileges".into(),
                        Value::Text("select,insert,update,references".into()),
                    );
                    r.insert("Comment".into(), Value::Text(String::new()));
                    rows.push(r);
                }
                rows
            }
            None => Vec::new(),
        }
    }

    /// SHOW INDEX rows: overlay when registered, else derived from the
    /// engine schema (PRIMARY + unique secondary indexes).
    pub(crate) fn show_index_rows(&self, table: &str) -> Vec<BTreeMap<String, Value>> {
        if let Some(meta) = self.table_meta(table) {
            return meta.show_index_rows();
        }
        let Some(schema) = self.engine.get_schema(table) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let mut push = |key_name: &str, non_unique: i64, col: &str, seq: i64, sub: Option<u16>| {
            let mut r = BTreeMap::new();
            r.insert("Table".into(), Value::Text(table.to_string()));
            r.insert("Non_unique".into(), Value::Int(non_unique));
            r.insert("Key_name".into(), Value::Text(key_name.to_string()));
            r.insert("Seq_in_index".into(), Value::Int(seq));
            r.insert("Column_name".into(), Value::Text(col.to_string()));
            r.insert("Collation".into(), Value::Text("A".into()));
            r.insert("Cardinality".into(), Value::Int(0));
            r.insert(
                "Sub_part".into(),
                match sub {
                    Some(n) => Value::Int(n as i64),
                    None => Value::Null,
                },
            );
            r.insert("Packed".into(), Value::Null);
            r.insert("Null".into(), Value::Text(String::new()));
            r.insert("Index_type".into(), Value::Text("BTREE".into()));
            r.insert("Comment".into(), Value::Text(String::new()));
            r.insert("Index_comment".into(), Value::Text(String::new()));
            // MariaDB form: the last SHOW INDEX column is Ignored (MySQL 8
            // uses Is_visible — we follow the MariaDB shape like elsewhere).
            r.insert("Ignored".into(), Value::Text("NO".into()));
            rows.push(r);
        };
        push("PRIMARY", 0, &schema.primary_locus_column, 1, None);
        for idx in &schema.secondary_indexes {
            push(
                &idx.name,
                if idx.unique { 0 } else { 1 },
                &idx.column,
                1,
                None,
            );
        }
        rows
    }
}
