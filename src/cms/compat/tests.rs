// SPDX-License-Identifier: AGPL-3.0-only

//! Gate-level tests: session state, catalog overlay, dialect rewrites.

use crate::cms::wordpress;

use super::*;
use crate::engine::{Engine, EngineConfig};
use crate::wal::SyncMode;
use std::sync::Arc;

fn gate(name: &str) -> CmsSqlGate {
    let dir = std::env::temp_dir().join(format!(
        "dnadb_cms_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cfg = EngineConfig {
        dir_path: dir,
        sync_mode: SyncMode::Batched,
        ..EngineConfig::default()
    };
    let engine = Engine::open(cfg).unwrap();
    CmsSqlGate::new(Arc::new(SqlEngine::new(engine)))
}

fn v(row: &BTreeMap<String, Value>, k: &str) -> String {
    row.get(k).map(|v| v.to_string()).unwrap_or_default()
}

#[test]
fn wordpress_bootstrap_and_autoincrement_flow() {
    let g = gate("ai_flow");
    let created = g.bootstrap_wordpress().unwrap();
    assert_eq!(created, 12, "12 core tables created");
    let s = g.open_session();

    // INSERT without the sequence column → id 1.
    let r = g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('foo','bar','yes')").unwrap();
    assert_eq!(r.affected_rows, 1);
    assert_eq!(r.last_insert_id, 1);

    // wpdb reads it back via LAST_INSERT_ID().
    let r = g.execute(s, "SELECT LAST_INSERT_ID()").unwrap();
    assert_eq!(v(&r.rows[0], "LAST_INSERT_ID()"), "1");

    // Second insert → 2.
    g.execute(
        s,
        "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('baz','qux','no')",
    )
    .unwrap();
    let r = g.execute(s, "SELECT LAST_INSERT_ID()").unwrap();
    assert_eq!(v(&r.rows[0], "LAST_INSERT_ID()"), "2");

    // Explicit id = 50 moves the counter.
    g.execute(s, "INSERT INTO wp_options (option_id, option_name, option_value, autoload) VALUES (50, 'big','1','yes')").unwrap();
    let r = g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('after','x','yes')").unwrap();
    assert_eq!(r.last_insert_id, 51);

    // The row really exists under the generated key.
    let r = g
        .execute(s, "SELECT option_name FROM wp_options WHERE option_id = 51")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "option_name"), "after");
}

#[test]
fn session_statements_and_functions() {
    let g = gate("session");
    let s = g.open_session();

    g.execute(s, "SET NAMES utf8mb4").unwrap();
    g.execute(s, "SET sql_mode = 'STRICT_TRANS_TABLES,NO_ZERO_DATE'")
        .unwrap();
    let r = g.execute(s, "SELECT @@SESSION.sql_mode").unwrap();
    let mode = v(&r.rows[0], "@@sql_mode");
    assert!(mode.contains("STRICT_TRANS_TABLES") && mode.contains("NO_ZERO_DATE"));

    g.execute(s, "SET autocommit = 0").unwrap();
    assert!(!g.with_session(s, |x| x.autocommit).unwrap());
    g.execute(s, "SET autocommit = 1").unwrap();

    let r = g.execute(s, "SELECT VERSION()").unwrap();
    assert_eq!(v(&r.rows[0], "VERSION()"), MYSQL_COMPAT_VERSION);

    // FOUND_ROWS() across two SELECTs.
    g.bootstrap_wordpress().unwrap();
    g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('a','1','yes'),('b','2','yes'),('c','3','no')").unwrap();
    g.execute(
        s,
        "SELECT option_name FROM wp_options WHERE autoload = 'yes'",
    )
    .unwrap();
    let r = g.execute(s, "SELECT FOUND_ROWS()").unwrap();
    assert_eq!(v(&r.rows[0], "FOUND_ROWS()"), "2");

    // USE / SHOW TABLES LIKE.
    g.execute(s, "USE wordpress").unwrap();
    assert_eq!(
        g.with_session(s, |x| x.database.clone())
            .unwrap()
            .as_deref(),
        Some("wordpress")
    );
    let r = g.execute(s, "SHOW TABLES LIKE 'wp_opt%'").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "Tables_in_wordpress"), "wp_options");
}

#[test]
fn distinct_and_comma_from_and_numeric_coercion() {
    let g = gate("wp7gaps");
    let s = g.open_session();
    g.bootstrap_wordpress().unwrap();

    // Seed: two terms referenced by the same object (distinct pairs).
    g.execute(
        s,
        "INSERT INTO wp_terms (name, slug, term_group) VALUES ('T1','t1',0), ('T2','t2',0)",
    )
    .unwrap();

    // 1) SELECT DISTINCT — engine has no DISTINCT projection; the gate
    //    strips it and dedupes rows (WP update_object_term_cache shape).
    let r = g
        .execute(
            s,
            "SELECT DISTINCT term_group FROM wp_terms ORDER BY term_group",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1, "distinct single column collapses to 1 row");

    // 2) Comma-FROM list — rewritten to INNER JOIN … ON 1=1 (cross join)
    //    with the join predicate left in WHERE, MySQL style.
    g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('numcheck', '7', 'yes')").unwrap();
    let r = g
            .execute(
                s,
                "SELECT COUNT(*) AS c FROM wp_terms, wp_options WHERE wp_terms.term_id = 1 AND wp_options.option_name = 'numcheck'",
            )
            .unwrap();
    assert_eq!(
        v(&r.rows[0], "c"),
        "1",
        "comma-FROM cross join + WHERE filter"
    );

    // 3) Numeric coercion: MySQL compares numbers and numeric strings
    //    numerically (`WHERE ID = '2'` — WP_User::get_data_by uses %s).
    let r = g
        .execute(
            s,
            "SELECT option_name FROM wp_options WHERE option_id = '1'",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1, "string '1' matches numeric option_id 1");

    // 4) …but non-numeric text does NOT silently match numbers (we do
    //    not replicate MySQL's "everything casts to 0" footgun).
    let r = g
        .execute(
            s,
            "SELECT option_name FROM wp_options WHERE option_id = 'abc'",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 0, "non-numeric text stays non-equal");

    // 5) NOT IN — WP core emits it everywhere (`comment_type NOT IN
    //    ('note')` breaks comments queries otherwise).
    g.execute(s, "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('other1', 'x', 'yes'), ('other2', 'y', 'yes')").unwrap();
    let r = g
            .execute(
                s,
                "SELECT option_name FROM wp_options WHERE option_name NOT IN ('numcheck') AND autoload = 'yes' ORDER BY option_name",
            )
            .unwrap();
    let names: Vec<String> = r.rows.iter().map(|r| v(r, "option_name")).collect();
    assert!(!names.contains(&"numcheck".to_string()));
    assert_eq!(
        names,
        vec!["other1", "other2"],
        "NOT IN keeps the other rows"
    );
}

#[test]
fn named_locks_via_sql() {
    let g = gate("locks");
    let a = g.open_session();
    let b = g.open_session();

    let r = g.execute(a, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(1)));

    // Re-entrant for the holder.
    let r = g.execute(a, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(1)));

    // Other session times out.
    let r = g.execute(b, "SELECT GET_LOCK('wp_cron', 0)").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(0)));

    let r = g.execute(b, "SELECT IS_FREE_LOCK('wp_cron')").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(0)));
    let r = g.execute(b, "SELECT IS_USED_LOCK('wp_cron')").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(a as i64)));

    let r = g.execute(a, "SELECT RELEASE_LOCK('wp_cron')").unwrap();
    assert_eq!(r.rows[0].values().next(), Some(&Value::Int(1)));
    let r = g.execute(b, "SELECT RELEASE_LOCK('wp_cron')").unwrap();
    assert!(r.rows[0].values().next().unwrap().is_null());

    // Session close frees remaining locks.
    g.execute(a, "SELECT GET_LOCK('again', 0)").unwrap();
    assert_eq!(g.close_session(a), 1);
}

#[test]
fn show_full_columns_is_dbdelta_ready() {
    let g = gate("columns");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();
    let r = g.execute(s, "SHOW FULL COLUMNS FROM wp_options").unwrap();
    assert_eq!(r.columns.first().map(String::as_str), Some("Field"));
    assert_eq!(r.rows.len(), 4);

    assert_eq!(v(&r.rows[0], "Field"), "option_id");
    assert_eq!(v(&r.rows[0], "Type"), "bigint(20) unsigned"); // raw DDL text
    assert_eq!(v(&r.rows[0], "Null"), "NO");
    assert_eq!(v(&r.rows[0], "Key"), "PRI");
    assert!(r.rows[0].get("Default").unwrap().is_null());
    assert_eq!(v(&r.rows[0], "Extra"), "auto_increment");

    assert_eq!(v(&r.rows[1], "Field"), "option_name");
    assert_eq!(v(&r.rows[1], "Type"), "varchar(191)");
    assert_eq!(v(&r.rows[1], "Key"), "UNI");
    assert_eq!(v(&r.rows[1], "Default"), "");

    let r = g.execute(s, "SHOW INDEX FROM wp_options").unwrap();
    let key = |i: usize| v(&r.rows[i], "Key_name");
    let seq = |i: usize| v(&r.rows[i], "Seq_in_index");
    assert_eq!(key(0), "PRIMARY");
    assert_eq!(seq(0), "1");
    assert_eq!(key(1), "option_name");
    assert_eq!(v(&r.rows[1], "Non_unique"), "0");
}

#[test]
fn dbdelta_style_comparison_is_idempotent() {
    // Mirrors the dbDelta loop: canonical "Field Type" + NOT NULL +
    // DEFAULT + EXTRA strings from the DDL must equal those from SHOW
    // FULL COLUMNS, on every run (no eternal ALTERs).
    let g = gate("dbdelta");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();
    let show = g.execute(s, "SHOW FULL COLUMNS FROM wp_options").unwrap();

    let ddl = wordpress::WP_CORE_TABLES
        .iter()
        .find(|(n, _)| *n == "wp_options")
        .map(|(_, d)| d.to_string())
        .unwrap();
    let meta = CmsTableMeta::parse_create_table(&ddl).unwrap();

    for (i, col) in meta.columns.iter().enumerate() {
        let from_ddl = format!(
            "{} {} {} default {} {}",
            col.field,
            col.mysql_type,
            if col.not_null { "NOT NULL" } else { "NULL" },
            col.default.clone().unwrap_or_else(|| "NULL".into()),
            if col.auto_increment {
                "auto_increment"
            } else {
                ""
            }
        );
        let row = &show.rows[i];
        let from_show = format!(
            "{} {} {} default {} {}",
            v(row, "Field"),
            v(row, "Type"),
            if v(row, "Null") == "NO" {
                "NOT NULL"
            } else {
                "NULL"
            },
            match row.get("Default") {
                Some(Value::Text(t)) => t.clone(),
                _ => "NULL".to_string(),
            },
            v(row, "Extra")
        );
        assert_eq!(
            from_ddl, from_show,
            "dbDelta would ALTER after column {}",
            col.field
        );
    }
}

#[test]
fn errors_carry_mysql_errno() {
    let g = gate("errors");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();

    // No such table → 1146.
    let e = g.execute(s, "SELECT * FROM wp_missing").unwrap_err();
    assert_eq!(e.errno, errno::ER_NO_SUCH_TABLE);
    assert_eq!(e.sqlstate(), "42S02");

    // Duplicate unique key (option_name) → 1062.
    g.execute(
        s,
        "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('dup','1','yes')",
    )
    .unwrap();
    let e = g
        .execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('dup','2','yes')",
        )
        .unwrap_err();
    assert_eq!(e.errno, errno::ER_DUP_ENTRY);
    assert_eq!(e.sqlstate(), "23000");

    // Parse error → 1064.
    let e = g.execute(s, "SELECT * FRM wp_options").unwrap_err();
    assert_eq!(e.errno, errno::ER_PARSE_ERROR);
}

#[test]
fn show_variables_and_sql_calc_found_rows() {
    let g = gate("vars");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();

    let r = g
        .execute(s, "SHOW VARIABLES LIKE 'character_set%'")
        .unwrap();
    assert!(r.rows.len() >= 3);
    assert_eq!(v(&r.rows[0], "Variable_name"), "character_set_client");

    // SQL_CALC_FOUND_ROWS is stripped transparently and still counts.
    g.execute(s, "INSERT INTO wp_posts (post_title, post_status, post_type, post_name) VALUES ('T1','publish','post','t1'),('T2','publish','post','t2')").unwrap();
    let r = g
        .execute(
            s,
            "SELECT SQL_CALC_FOUND_ROWS ID FROM wp_posts WHERE post_status = 'publish' LIMIT 1",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1, "LIMIT 1 still applies");
    let r = g.execute(s, "SELECT FOUND_ROWS()").unwrap();
    assert_eq!(v(&r.rows[0], "FOUND_ROWS()"), "1");

    // Row exists with generated ID; post_date_gmt was never provided →
    // filled with the effective DEFAULT (NOT NULL zero-date), exactly
    // what MySQL does for an omitted column on INSERT (§4.11 done).
    let r = g
        .execute(s, "SELECT post_date_gmt FROM wp_posts WHERE ID = 1")
        .unwrap();
    assert_eq!(
        r.rows[0]
            .get("post_date_gmt")
            .map(|v| v.to_string())
            .as_deref(),
        Some("0000-00-00 00:00:00")
    );
    let r = g
        .execute(s, "SELECT ID FROM wp_posts WHERE post_name = 't1'")
        .unwrap();
    assert_eq!(v(&r.rows[0], "ID"), "1");
}

#[test]
fn plugin_ddl_passes_through_gate() {
    // Plugin activation (dbDelta) sends raw MySQL DDL — no overlay.
    // The engine's cms grammar must swallow types/lengths/DEFAULTs and
    // the trailing ENGINE/CHARSET/COLLATE options.
    let g = gate("plugin_ddl");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE wp_myplugin_data (
            id bigint(20) unsigned NOT NULL AUTO_INCREMENT,
            post_id bigint(20) unsigned NOT NULL DEFAULT '0',
            value decimal(10,2) NOT NULL DEFAULT '0.00',
            note varchar(255) DEFAULT '',
            created datetime NOT NULL DEFAULT '0000-00-00 00:00:00',
            PRIMARY KEY  (id),
            KEY post_id (post_id)
        ) ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci",
    )
    .unwrap();

    let r = g
        .execute(
            s,
            "INSERT INTO wp_myplugin_data (post_id, value) VALUES (5, 19.99)",
        )
        .unwrap();
    assert_eq!(r.last_insert_id, 1);
    let r = g
        .execute(s, "SELECT value FROM wp_myplugin_data WHERE id = 1")
        .unwrap();
    assert_eq!(v(&r.rows[0], "value"), "19.99");

    // CREATE TABLE through the gate registers the MySQL-dialect overlay
    // automatically, so SHOW FULL COLUMNS returns the EXACT original
    // types (dbDelta parity for plugin tables). Columns live in the
    // DDL's declared order; look rows up by Field regardless.
    let r = g
        .execute(s, "SHOW FULL COLUMNS FROM wp_myplugin_data")
        .unwrap();
    assert_eq!(r.rows.len(), 5);
    let row = |name: &str| {
        r.rows
            .iter()
            .find(|row| v(row, "Field") == name)
            .unwrap_or_else(|| panic!("column {} not shown", name))
    };
    assert_eq!(v(row("id"), "Type"), "bigint(20) unsigned");
    assert_eq!(v(row("id"), "Key"), "PRI");
    assert_eq!(v(row("id"), "Extra"), "auto_increment");
    assert_eq!(v(row("value"), "Type"), "decimal(10,2)");
    assert_eq!(v(row("created"), "Type"), "datetime");
    assert_eq!(v(row("note"), "Default"), "");
}

#[test]
fn hpos_string_insert_into_bigint_indexed_column() {
    // WooCommerce HPOS regression (E2E 2026-11-02): WC_Order_Item_Data_Store
    // uses wpdb::insert() WITHOUT a format array — every value travels as a
    // string, so `order_id` lands in `bigint unsigned` as '15'. MariaDB
    // coerces on write; we must too, else the secondary-index key (encoded
    // from Text) never matches `WHERE order_id = 15` (IndexScan → 0 rows),
    // get_items() returns [] and stock reduction silently no-ops.
    let g = gate("hpos_types");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE wp_woocommerce_order_items (
            order_item_id bigint(20) unsigned NOT NULL AUTO_INCREMENT,
            order_item_name text NOT NULL,
            order_item_type varchar(200) NOT NULL DEFAULT '',
            order_id bigint(20) unsigned NOT NULL DEFAULT '0',
            PRIMARY KEY  (order_item_id),
            KEY order_id (order_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci",
    )
    .unwrap();

    // HPOS shape: wpdb->insert without format → all values quoted.
    g.execute(s, "INSERT INTO wp_woocommerce_order_items (order_item_name, order_item_type, order_id) VALUES ('Тестовый товар DNA', 'line_item', '15')").unwrap();
    // Classic wc_add_order_item shape: %s, %s, %d → id unquoted.
    g.execute(s, "INSERT INTO wp_woocommerce_order_items (order_item_name, order_item_type, order_id) VALUES ('Прямая вставка', 'line_item', 15)").unwrap();

    // Int-literal lookup must see BOTH rows (index keys coerced on write).
    let r = g.execute(s, "SELECT order_item_id FROM wp_woocommerce_order_items WHERE order_id = 15 ORDER BY order_item_id").unwrap();
    assert_eq!(
        r.rows.len(),
        2,
        "string- and int-written rows both match = 15"
    );
    assert_eq!(v(&r.rows[0], "order_item_id"), "1");
    // String-literal lookup too.
    let r = g
        .execute(
            s,
            "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = '15'",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "c"), "2");
    // The stored value became numeric (MariaDB representation).
    let r = g.execute(s, "SELECT order_item_id FROM wp_woocommerce_order_items WHERE order_item_id = 1 AND order_id + 0 = 15").unwrap();
    assert_eq!(r.rows.len(), 1);

    // UPDATE coercion: string SET into an int column stores a number.
    g.execute(
        s,
        "UPDATE wp_woocommerce_order_items SET order_id = '16' WHERE order_item_id = 1",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = 16",
        )
        .unwrap();
    assert_eq!(
        v(&r.rows[0], "c"),
        "1",
        "UPDATE '16' coerced to 16 (index rebuilt)"
    );
    // Non-numeric text into an int column → 0 (non-strict, sql_mode='').
    g.execute(
        s,
        "UPDATE wp_woocommerce_order_items SET order_id = 'abc' WHERE order_item_id = 2",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT COUNT(*) AS c FROM wp_woocommerce_order_items WHERE order_id = 0",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "c"), "1");
}

#[test]
fn fallback_catalog_for_non_cms_tables() {
    let g = gate("fallback");
    g.engine
        .execute("CREATE TABLE chr_plain (id INT, name TEXT, PRIMARY KEY (id))")
        .unwrap();
    let s = g.open_session();
    let r = g.execute(s, "SHOW FULL COLUMNS FROM chr_plain").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(v(&r.rows[0], "Field"), "id");
    assert_eq!(v(&r.rows[0], "Type"), "bigint(20)"); // engine-derived
    assert_eq!(v(&r.rows[0], "Key"), "PRI");
    let r = g.execute(s, "SHOW INDEX FROM chr_plain").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "Key_name"), "PRIMARY");
}

#[test]
fn transactions_and_mysql_dialect_via_gate() {
    let g = gate("dialect_tx");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();

    // Backticked identifiers (wpdb sends them everywhere).
    g.execute(
            s,
            "INSERT INTO `wp_options` (`option_name`, `option_value`, `autoload`) VALUES ('bt', '1', 'yes')",
        )
        .unwrap();

    // ON DUPLICATE KEY UPDATE on the UNIQUE option_name (add_option path).
    g.execute(
        s,
        "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('bt', '2', 'yes') \
             ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT option_value FROM wp_options WHERE option_name = 'bt'",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "option_value"), "2");
    assert_eq!(r.rows.len(), 1, "upsert must not duplicate the row");

    // DESCRIBE is dbDelta's actual probe.
    let r = g.execute(s, "DESCRIBE wp_options").unwrap();
    assert_eq!(r.columns.first().map(String::as_str), Some("Field"));
    assert_eq!(r.rows.len(), 4);

    // LIMIT offset, count (WP_Query pagination form).
    g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('p1','1','yes'),('p2','2','yes'),('p3','3','yes'),('p4','4','yes'),('p5','5','yes')",
        )
        .unwrap();
    let r = g
            .execute(
                s,
                "SELECT option_name FROM wp_options WHERE option_name != 'bt' ORDER BY option_name ASC LIMIT 2, 2",
            )
            .unwrap();
    assert_eq!(v(&r.rows[0], "option_name"), "p3");
    assert_eq!(v(&r.rows[1], "option_name"), "p4");
    assert_eq!(r.rows.len(), 2);

    // NOW() literal rewrite in UPDATE.
    g.execute(
        s,
        "UPDATE wp_options SET option_value = NOW() WHERE option_name = 'p1'",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT option_value FROM wp_options WHERE option_name = 'p1'",
        )
        .unwrap();
    let val = v(&r.rows[0], "option_value");
    assert_eq!(
        val.len(),
        19,
        "NOW() must render as 'YYYY-MM-DD HH:MM:SS', got {val}"
    );
    assert_eq!(&val[4..5], "-");

    // Transaction: CMS edition — read-your-writes. The gate applies the
    // tx buffer after EVERY statement (MediaWiki page save does
    // INSERT+UPDATE in one BEGIN..COMMIT), so ROLLBACK cannot un-write
    // DML that already ran; BEGIN/COMMIT/ROLLBACK stay accepted markers.
    g.execute(s, "START TRANSACTION").unwrap();
    g.execute(
        s,
        "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('txrow', '1', 'yes')",
    )
    .unwrap();
    g.execute(s, "ROLLBACK").unwrap();
    let r = g
        .execute(
            s,
            "SELECT option_name FROM wp_options WHERE option_name = 'txrow'",
        )
        .unwrap();
    assert_eq!(
        r.rows.len(),
        1,
        "CMS tx: the INSERT was applied on execute (read-your-writes; ROLLBACK is a marker)"
    );

    g.execute(s, "BEGIN").unwrap();
    g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('txrow2', '1', 'yes')",
        )
        .unwrap();
    g.execute(s, "COMMIT").unwrap();
    let r = g
        .execute(
            s,
            "SELECT option_name FROM wp_options WHERE option_name = 'txrow2'",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1, "COMMIT must persist the INSERT");

    // Escaped quotes inside string literals (MySQL backslash escapes).
    g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('esc', 'don\\'t \\\\ stop', 'yes')",
        )
        .unwrap();
    let r = g
        .execute(
            s,
            "SELECT option_value FROM wp_options WHERE option_name = 'esc'",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "option_value"), "don't \\ stop");
}

#[test]
fn wp_like_transient_cleanup_pattern() {
    // The escaped-wildcard LIKE from delete_expired_transients: backslash
    // escapes must survive tokenization so '\_%' matches a literal '_'.
    let g = gate("transient");
    g.bootstrap_wordpress().unwrap();
    let s = g.open_session();
    g.execute(
            s,
            "INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('_transient_x', '1', 'yes'), ('_transient_timeout_x', '1', 'no'), ('_transient_y', '2', 'no')",
        )
        .unwrap();
    let r = g
            .execute(s, "SELECT option_name FROM wp_options WHERE option_name LIKE '\\_transient\\_%' AND option_name NOT LIKE '%timeout%' ORDER BY option_name ASC")
            .unwrap();
    assert_eq!(
        r.rows.len(),
        2,
        "\\_ must anchor the pattern to the literal underscore"
    );
}

#[test]
fn order_by_arithmetic_numeric_sort_no_extra_column() {
    // WordPress/WooCommerce numeric-sort idiom: `ORDER BY meta_value+0`
    // over a varchar column (differential corpus #37, run #2 errno 1064).
    // MySQL reads the leading numeric prefix: '250.00'+0 -> 250,
    // 'abc'+0 -> 0; and the sort key emits NO output column.
    let g = gate("order_arith");
    let s = g.open_session();
    g.execute(
            s,
            "CREATE TABLE t_items (id bigint(20) unsigned NOT NULL auto_increment, label varchar(50) NOT NULL default '', weight varchar(20) NOT NULL default '', PRIMARY KEY (id))",
        )
        .unwrap();
    g.execute(
        s,
        "INSERT INTO t_items (label, weight) VALUES ('a','250.00'), ('b','110.50'), ('c','abc')",
    )
    .unwrap();
    let r = g
        .execute(s, "SELECT id, label FROM t_items ORDER BY weight+0 DESC")
        .unwrap();
    assert_eq!(
        r.columns,
        vec!["id", "label"],
        "sort key must not add an output column"
    );
    let got: Vec<String> = r.rows.iter().map(|row| v(row, "label")).collect();
    assert_eq!(got, vec!["a", "b", "c"], "250 > 110.5 > 0('abc')");
    // ASC with a secondary key and LIMIT (MySQL multi-key sort).
    let r = g
        .execute(
            s,
            "SELECT id FROM t_items ORDER BY weight+0 ASC, id ASC LIMIT 2",
        )
        .unwrap();
    let got: Vec<String> = r.rows.iter().map(|row| v(row, "id")).collect();
    assert_eq!(got, vec!["3", "2"], "'abc'(0) first, then 110.50");
}

#[test]
fn star_projections_use_declaration_order() {
    // MySQL reports `SELECT *` / `t.*` columns in DECLARATION order
    // (differential corpus #36/#37, run #2 label/value desync).
    let g = gate("decl_order");
    let s = g.open_session();
    g.execute(
            s,
            "CREATE TABLE wp_terms (term_id bigint(20) unsigned NOT NULL auto_increment, name varchar(200) NOT NULL default '', slug varchar(200) NOT NULL default '', term_group bigint(20) NOT NULL default 0, PRIMARY KEY (term_id))",
        )
        .unwrap();
    g.execute(
        s,
        "INSERT INTO wp_terms (name, slug) VALUES ('News','news')",
    )
    .unwrap();
    let r = g.execute(s, "SELECT * FROM wp_terms").unwrap();
    assert_eq!(r.columns, vec!["term_id", "name", "slug", "term_group"]);
    // `t.*, tt.*`: labels follow each table's declaration order, MySQL
    // keeps the duplicated label (term_id) one per projection item.
    g.execute(
            s,
            "CREATE TABLE wp_term_taxonomy (term_taxonomy_id bigint(20) unsigned NOT NULL auto_increment, term_id bigint(20) unsigned NOT NULL default 0, taxonomy varchar(32) NOT NULL default '', PRIMARY KEY (term_taxonomy_id))",
        )
        .unwrap();
    g.execute(
        s,
        "INSERT INTO wp_term_taxonomy (term_id, taxonomy) VALUES (1,'category')",
    )
    .unwrap();
    let r = g
            .execute(
                s,
                "SELECT t.*, tt.* FROM wp_terms AS t INNER JOIN wp_term_taxonomy AS tt ON t.term_id = tt.term_id",
            )
            .unwrap();
    assert_eq!(
        r.columns,
        vec![
            "term_id",
            "name",
            "slug",
            "term_group",
            "term_taxonomy_id",
            "term_id",
            "taxonomy"
        ]
    );
    assert_eq!(v(&r.rows[0], "name"), "News");
    assert_eq!(v(&r.rows[0], "taxonomy"), "category");
    // ALTER TABLE ADD appends to the declaration order (MySQL).
    g.execute(
        s,
        "ALTER TABLE wp_terms ADD COLUMN new_col varchar(64) NOT NULL default ''",
    )
    .unwrap();
    let r = g.execute(s, "SELECT * FROM wp_terms").unwrap();
    assert_eq!(
        r.columns,
        vec!["term_id", "name", "slug", "term_group", "new_col"]
    );
}

// ---------------- row locks: SELECT … FOR UPDATE (§4 item 2) ----------------

/// Small stock table for lock scenarios (Woo `_stock` meta shape).
fn stock_gate(name: &str) -> CmsSqlGate {
    let g = gate(name);
    g.execute(
            g.open_session(),
            "CREATE TABLE wp_stock_meta (meta_id bigint(20) unsigned NOT NULL auto_increment, post_id bigint(20) unsigned NOT NULL default 0, meta_key varchar(255) NOT NULL default '', meta_value longtext, PRIMARY KEY (meta_id))",
        )
        .unwrap();
    g
}

#[test]
fn for_update_blocks_second_session_until_commit() {
    let g = stock_gate("fu_block");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (7, '_stock', '10')",
    )
    .unwrap();

    // s1: read-and-lock inside a transaction.
    g.execute(s1, "BEGIN").unwrap();
    let r = g
            .execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");
    assert_eq!(g.row_locks.held_len(), 1, "one row lock held inside tx");

    // s2: the same FOR UPDATE waits → 1205 with a tiny session timeout.
    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    // s2's tx survived the failed statement (statement-level rollback).
    assert!(g.tx_active(s2));

    // NOWAIT on a held row → 3572 immediately.
    g.execute(s1, "SET innodb_lock_wait_timeout = 5").unwrap();
    let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE NOWAIT")
            .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_NOWAIT,
        "3572 expected, got {:?}",
        err
    );

    // COMMIT releases; the waiter then succeeds (its tx still open).
    g.execute(s1, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0, "commit releases row locks");
    let r = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");
    g.execute(s2, "ROLLBACK").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}

#[test]
fn for_update_reentrant_same_session_and_row() {
    let g = stock_gate("fu_reentrant");
    let s = g.open_session();
    g.execute(
        s,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (1, '_stock', '5')",
    )
    .unwrap();
    g.execute(s, "BEGIN").unwrap();
    g.execute(
        s,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 FOR UPDATE",
    )
    .unwrap();
    // Same transaction locks the row again — no self-deadlock.
    let r = g.execute(s, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 AND meta_key = '_stock' FOR UPDATE").unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "5");
    g.execute(s, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}

#[test]
fn opposite_order_updates_deadlock_1213() {
    // InnoDB pattern: T1 locks A then wants B; T2 locks B then wants A →
    // one of them gets 1213 and WooCommerce retries.
    let g = stock_gate("fu_deadlock");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (100, '_stock', '1')",
    )
    .unwrap();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (200, '_stock', '2')",
    )
    .unwrap();

    g.execute(s1, "SET innodb_lock_wait_timeout = 3").unwrap();
    g.execute(s2, "SET innodb_lock_wait_timeout = 3").unwrap();

    // T1 holds row A (post_id=100); T2 holds row B (post_id=200).
    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE",
    )
    .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    g.execute(
        s2,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE",
    )
    .unwrap();

    // T1 starts waiting for B on a background thread…
    let g2 = std::sync::Arc::new(g);
    let g3 = g2.clone();
    let t1 = std::thread::spawn(move || {
        g3.execute(
            s1,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE",
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(150));

    // …then T2 asks for A: cycle (s2→B→s1→A→s2) → 1213 immediately.
    let err = g2
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_DEADLOCK,
        "1213 expected, got {:?}",
        err
    );
    assert_eq!(err.sqlstate(), "40001");

    // Victim "rolled back its statement" (tx survives per MySQL); after
    // T2 rolls back fully, T1's wait completes.
    g2.execute(s2, "ROLLBACK").unwrap();
    let r = t1
        .join()
        .unwrap()
        .expect("T1 must acquire row B after T2 died");
    assert_eq!(v(&r.rows[0], "meta_value"), "2");
    g2.execute(s1, "COMMIT").unwrap();
}

#[test]
fn autocommit_update_waits_for_held_row() {
    // Woo path: order A reduced stock in a tx (FOR UPDATE); order B's
    // autocommit UPDATE of the same row must wait, not stomp.
    let g = stock_gate("fu_autocommit");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (77, '_stock', '10')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77 AND meta_key = '_stock' FOR UPDATE").unwrap();

    // Autocommit UPDATE blocked by the open tx → 1205 (short timeout).
    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    // The row still holds the old value.
    let r = g
        .execute(
            s1,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");

    // Commit, then the UPDATE goes through.
    g.execute(s1, "COMMIT").unwrap();
    let r = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'",
        )
        .unwrap();
    assert_eq!(r.affected_rows, 1);
    let r = g
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "8");
}

#[test]
fn update_in_tx_takes_hold_until_commit() {
    let g = stock_gate("fu_upd_tx");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (9, '_stock', '4')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "UPDATE wp_stock_meta SET meta_value = '3' WHERE post_id = 9 AND meta_key = '_stock'",
    )
    .unwrap();
    assert!(g.row_locks.held_len() >= 1, "UPDATE in tx holds an X lock");

    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    let err = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '2' WHERE post_id = 9 AND meta_key = '_stock'",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    g.execute(s2, "ROLLBACK").unwrap();

    g.execute(s1, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
    let r = g
        .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 9")
        .unwrap();
    assert_eq!(
        v(&r.rows[0], "meta_value"),
        "3",
        "first tx's write committed"
    );
}

#[test]
fn delete_blocked_by_held_row_and_locking_tail_skips_literals() {
    let g = stock_gate("fu_delete");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (5, '_stock', '1')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 5 FOR UPDATE",
    )
    .unwrap();

    // A literal 'for update' inside a string must NOT take the lock path.
    let r = g
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE meta_value = 'wait for update'",
        )
        .unwrap();
    assert_eq!(
        r.rows.len(),
        0,
        "literal containing 'for update' is not a locking tail"
    );

    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
        .execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5")
        .unwrap_err();
    assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "DELETE waits too");

    g.execute(s1, "COMMIT").unwrap();
    let r = g
        .execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5")
        .unwrap();
    assert_eq!(r.affected_rows, 1);
}

#[test]
fn close_session_releases_row_locks() {
    let g = stock_gate("fu_close");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (3, '_stock', '9')",
    )
    .unwrap();
    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3 FOR UPDATE",
    )
    .unwrap();
    assert_eq!(g.row_locks.held_len(), 1);
    // PHP-FPM worker dies mid-transaction → disconnect rolls the tx back.
    g.close_session(s1);
    assert_eq!(g.row_locks.held_len(), 0, "disconnect releases row locks");
    let r = g
        .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3")
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "9");
}

#[test]
fn skip_locked_claims_only_free_rows() {
    // Action Scheduler: two queue workers claim pending jobs with
    // FOR UPDATE SKIP LOCKED — each must get a DIFFERENT row.
    let g = stock_gate("fu_skip");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (11, '_job', 'a')",
    )
    .unwrap();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (12, '_job', 'b')",
    )
    .unwrap();

    // Worker 1 claims job 11 (projection carries the PK → exact rows).
    g.execute(s1, "BEGIN").unwrap();
    let r = g
        .execute(
            s1,
            "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id = 11 FOR UPDATE",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "post_id"), "11");

    // Worker 2 asks for both jobs, skipping held ones → only 12 left.
    g.execute(s2, "BEGIN").unwrap();
    let r = g
            .execute(s2, "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE SKIP LOCKED")
            .unwrap();
    assert_eq!(r.rows.len(), 1, "held job 11 excluded by SKIP LOCKED");
    assert_eq!(v(&r.rows[0], "post_id"), "12");

    // Plain FOR UPDATE on the same set still blocks (0.15s → 1205).
    let s3 = g.open_session();
    g.execute(s3, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
            .execute(s3, "SELECT meta_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE")
            .unwrap_err();
    assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT);

    g.execute(s1, "COMMIT").unwrap();
    g.execute(s2, "ROLLBACK").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}
#[test]
fn replace_into_is_full_row_upsert() {
    let g = gate("replace_into");
    let s = g.open_session();
    g.execute(
            s,
            "CREATE TABLE chr_cfg ( id bigint(20) unsigned NOT NULL AUTO_INCREMENT, k varchar(64) NOT NULL, v varchar(255) NOT NULL, PRIMARY KEY (id), UNIQUE KEY uq_k (k) )",
        )
        .unwrap();
    g.execute(s, "INSERT INTO chr_cfg (k, v) VALUES ('a','1'), ('b','2')")
        .unwrap();
    // REPLACE with a conflicting unique key: the row is rewritten.
    let r = g
        .execute(s, "REPLACE INTO chr_cfg (k, v) VALUES ('a', '999')")
        .unwrap();
    assert!(
        r.affected_rows <= 2,
        "REPLACE reports insert or delete+insert"
    );
    let r = g.execute(s, "SELECT COUNT(*) AS n FROM chr_cfg").unwrap();
    assert_eq!(v(&r.rows[0], "n"), "2", "REPLACE must not duplicate rows");
    let r = g.execute(s, "SELECT v FROM chr_cfg WHERE k = 'a'").unwrap();
    assert_eq!(v(&r.rows[0], "v"), "999");
    // Non-conflicting REPLACE behaves like plain INSERT.
    let r = g
        .execute(s, "REPLACE INTO chr_cfg (k, v) VALUES ('c', '3')")
        .unwrap();
    assert_eq!(r.affected_rows, 1);
    let r = g.execute(s, "SELECT COUNT(*) AS n FROM chr_cfg").unwrap();
    assert_eq!(v(&r.rows[0], "n"), "3");
}

#[test]
fn insert_select_from_dual_action_scheduler_pattern() {
    let g = gate("insert_dual");
    let s = g.open_session();
    g.execute(
            s,
            "CREATE TABLE chr_actions ( id bigint(20) unsigned NOT NULL AUTO_INCREMENT, hook varchar(191) NOT NULL, status varchar(20) NOT NULL, args text, PRIMARY KEY (id) )",
        )
        .unwrap();
    // Exactly the shape WooCommerce/ActionScheduler sends.
    let q = "INSERT INTO chr_actions ( `hook`, `status`, `args` )\nSELECT 'wc_admin_import_orders', 'pending', '[11]' FROM DUAL\nWHERE ( SELECT NULL FROM DUAL ) IS NULL";
    let r = g.execute(s, q).unwrap();
    assert_eq!(r.affected_rows, 1, "guard is always true → one row");
    assert_eq!(r.last_insert_id, 1);
    let r = g.execute(s, "SELECT hook, args FROM chr_actions").unwrap();
    assert_eq!(v(&r.rows[0], "hook"), "wc_admin_import_orders");
    assert_eq!(v(&r.rows[0], "args"), "[11]");
    // Without the WHERE guard.
    let r = g
        .execute(
            s,
            "INSERT INTO chr_actions (hook, status, args) SELECT 'h2', 'pending', NULL FROM DUAL",
        )
        .unwrap();
    assert_eq!(r.affected_rows, 1);
    // A non-canonical guard goes through the real INSERT..SELECT path:
    // `WHERE 1 = 0` selects no rows, so nothing is inserted (MySQL).
    let r = g
        .execute(
            s,
            "INSERT INTO chr_actions (hook, status) SELECT 'h3', 'x' FROM DUAL WHERE 1 = 0",
        )
        .unwrap();
    assert_eq!(r.affected_rows, 0, "guard false → no rows");
    let r = g
        .execute(s, "SELECT COUNT(*) AS n FROM chr_actions")
        .unwrap();
    assert_eq!(v(&r.rows[0], "n"), "2");
}

#[test]
fn compound_join_on_and_qualified_aggregates() {
    let g = gate("join_on_and");
    let s = g.open_session();
    g.execute(
            s,
            "CREATE TABLE chr_posts ( id bigint(20) unsigned NOT NULL AUTO_INCREMENT, slug varchar(64), status varchar(20), PRIMARY KEY (id) )",
        )
        .unwrap();
    g.execute(
            s,
            "CREATE TABLE chr_meta ( id bigint(20) unsigned NOT NULL AUTO_INCREMENT, post_id bigint(20) unsigned, k varchar(64), val int, PRIMARY KEY (id) )",
        )
        .unwrap();
    g.execute(s, "INSERT INTO chr_posts (id, slug, status) VALUES (1, 'hello', 'publish'), (2, 'x', 'draft')").unwrap();
    g.execute(
            s,
            "INSERT INTO chr_meta (post_id, k, val) VALUES (1, 'price', '250'), (1, 'price', '50'), (2, 'price', '10')",
        )
        .unwrap();
    // Compound ON: first equality drives the join, AND adds the filter.
    let q = "SELECT p.slug, m.val FROM chr_posts p INNER JOIN chr_meta m ON p.id = m.post_id AND p.slug = 'hello'";
    let r = g.execute(s, q).unwrap();
    assert_eq!(r.rows.len(), 2, "draft post filtered by the ON condition");
    // Qualified aggregate argument SUM(m.val) — Woo refund query shape.
    let q2 = "SELECT SUM(m.val) FROM chr_meta AS m INNER JOIN chr_posts AS p ON ( m.post_id = p.id ) WHERE p.slug = 'hello'";
    let r2 = g.execute(s, q2).unwrap();
    assert_eq!(
        r2.rows[0].values().next(),
        Some(&Value::Int(300)),
        "SUM over qualified column"
    );
}

#[test]
fn preprocess_rewrites_insert_select_dual() {
    let q = "INSERT INTO chr_actions ( `hook`, `status`, `args` )\nSELECT 'wc_admin_import_orders', 'pending', '[11]' FROM DUAL\nWHERE ( SELECT NULL FROM DUAL ) IS NULL";
    let out = preprocess_mysql_dialect(q);
    assert_eq!(
            out,
            "INSERT INTO chr_actions ( hook, status, args ) VALUES ('wc_admin_import_orders', 'pending', '[11]')",
            "exact Woo shape must rewrite to INSERT..VALUES"
        );
    // without the WHERE guard
    let out2 = preprocess_mysql_dialect("INSERT INTO t (a, b) SELECT 'x', 1 FROM DUAL");
    assert_eq!(out2, "INSERT INTO t (a, b) VALUES ('x', 1)");
    // non-canonical guard is not rewritten
    let out3 = preprocess_mysql_dialect("INSERT INTO t (a) SELECT 'x' FROM DUAL WHERE 1 = 0");
    assert!(
        out3.contains("FROM DUAL"),
        "unknown guard must be left untouched"
    );
    // INSERT..SELECT from a real table is not touched
    let out4 = preprocess_mysql_dialect("INSERT INTO t (a) SELECT b FROM other");
    assert_eq!(out4, "INSERT INTO t (a) SELECT b FROM other");
}

#[test]
fn strip_index_hints_forms() {
    use super::dialect::strip_index_hints;
    // MediaWiki RevisionStore::findIdenticalRevision derived table
    let q = "SELECT * FROM (SELECT rev_id FROM revision FORCE INDEX (rev_page_timestamp) \
             JOIN actor ON (1)) `recent_revs` WHERE rev_sha1 = 'x' LIMIT 1";
    assert_eq!(
        strip_index_hints(q),
        "SELECT * FROM (SELECT rev_id FROM revision JOIN actor ON (1)) `recent_revs` WHERE rev_sha1 = 'x' LIMIT 1"
    );
    assert_eq!(
        strip_index_hints("SELECT a FROM t USE INDEX (i1) WHERE a = 1"),
        "SELECT a FROM t WHERE a = 1"
    );
    assert_eq!(
        strip_index_hints("SELECT a FROM t IGNORE INDEX FOR ORDER BY (i2) GROUP BY a"),
        "SELECT a FROM t GROUP BY a"
    );
    // word-boundary safety: identifiers containing the words are untouched
    assert_eq!(
        strip_index_hints("SELECT force_indexed FROM use_indexes"),
        "SELECT force_indexed FROM use_indexes"
    );
    // inside string literals untouched
    assert_eq!(
        strip_index_hints("SELECT 'FORCE INDEX (x)' AS a"),
        "SELECT 'FORCE INDEX (x)' AS a"
    );
}

#[test]
fn catalog_probe_multi_condition_where() {
    use super::catalog::{split_on_and, where_eq_pairs};
    // Drupal 11 tableExists(): parenthesised AND conditions, double-quoted
    // identifiers, single-quoted values.
    let q = "SELECT 1 FROM information_schema.tables WHERE (\"table_schema\" = 'drupaldna') \
             AND (\"table_name\" = 'config') AND (\"table_type\" = 'BASE TABLE')";
    let pairs = where_eq_pairs(q);
    assert_eq!(
        pairs,
        vec![
            ("table_schema".to_string(), "drupaldna".to_string()),
            ("table_name".to_string(), "config".to_string()),
            ("table_type".to_string(), "BASE TABLE".to_string()),
        ]
    );
    assert_eq!(split_on_and("a = 1 AND b = 2").len(), 2);
    assert_eq!(split_on_and("('x' = 'a AND b') AND (y = 2)").len(), 2);
    // WordPress-style unparenthesised probe keeps working
    let wp = "SELECT 1 FROM information_schema.tables WHERE table_schema = 'wp' AND table_name = 'wp_options'";
    assert_eq!(where_eq_pairs(wp).len(), 2);
}
