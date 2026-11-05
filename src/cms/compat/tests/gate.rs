// SPDX-License-Identifier: AGPL-3.0-only
//! Gate-level CMS dialect/session tests.

use super::*;

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

#[test]
fn decimal_scale_display_parity() {
    // MySQL transmits DECIMAL(p,s) as fixed-scale strings in both
    // protocols: an inserted 250 into decimal(10,2) reads back "250.00"
    // with MYSQL_TYPE_NEWDECIMAL/decimals=2 column metadata. The gate
    // re-renders resolved output columns (decimal.rs) and records the
    // per-column scale in CmsQueryResult::decimal_scales.
    let g = gate("decimal_scale");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE wp_totals (
            id bigint(20) unsigned NOT NULL AUTO_INCREMENT,
            label varchar(50) DEFAULT '',
            total decimal(10,2) NOT NULL DEFAULT '0.00',
            ratio decimal(12,8) DEFAULT NULL,
            PRIMARY KEY  (id)
        ) ENGINE=InnoDB",
    )
    .unwrap();

    // Int / string / zero / NULL inserts all read back with scale 2.
    g.execute(s, "INSERT INTO wp_totals (label, total) VALUES ('a', 250)").unwrap();
    g.execute(s, "INSERT INTO wp_totals (label, total) VALUES ('b', '250.5')").unwrap();
    g.execute(s, "INSERT INTO wp_totals (label, total) VALUES ('c', 0)").unwrap();
    g.execute(s, "INSERT INTO wp_totals (label, total, ratio) VALUES ('d', 7, 0.125)").unwrap();

    let r = g
        .execute(s, "SELECT id, label, total, ratio FROM wp_totals ORDER BY id")
        .unwrap();
    assert_eq!(r.decimal_scales, vec![None, None, Some(2), Some(8)]);
    assert_eq!(v(&r.rows[0], "total"), "250.00");
    assert_eq!(v(&r.rows[1], "total"), "250.50");
    assert_eq!(v(&r.rows[2], "total"), "0.00");
    assert_eq!(v(&r.rows[3], "total"), "7.00");
    assert_eq!(v(&r.rows[3], "ratio"), "0.12500000");
    // Non-decimal columns keep their native rendering.
    assert_eq!(v(&r.rows[0], "id"), "1");
    assert_eq!(v(&r.rows[0], "label"), "a");

    // SELECT * keeps the declared scale per column.
    let r = g.execute(s, "SELECT * FROM wp_totals WHERE id = 1").unwrap();
    assert_eq!(v(&r.rows[0], "total"), "250.00");
    assert_eq!(r.decimal_scales, vec![None, None, Some(2), Some(8)]);

    // Arithmetic stays numeric in WHERE; display is re-scaled on read.
    let r = g
        .execute(s, "SELECT total FROM wp_totals WHERE total > 300")
        .unwrap();
    assert_eq!(r.rows.len(), 0, "250.00 must not compare as 25000");
    let r = g
        .execute(s, "SELECT total FROM wp_totals WHERE total >= 250.50")
        .unwrap();
    assert_eq!(r.rows.len(), 1);

    // UPDATE arithmetic round-trips through the scale.
    g.execute(s, "UPDATE wp_totals SET total = total + 1 WHERE id = 1").unwrap();
    let r = g.execute(s, "SELECT total FROM wp_totals WHERE id = 1").unwrap();
    assert_eq!(v(&r.rows[0], "total"), "251.00");

    // SUM aliased to the decimal column keeps the scale (MySQL: SUM over
    // decimal(p,s) is decimal with scale s).
    let r = g
        .execute(s, "SELECT SUM(total) AS total FROM wp_totals")
        .unwrap();
    assert_eq!(r.decimal_scales, vec![Some(2)]);
    assert_eq!(v(&r.rows[0], "total"), "508.50");

    // Expressions without an overlay binding keep DOUBLE rendering —
    // documented divergence (MySQL would size the decimal from the op).
    let r = g
        .execute(s, "SELECT total * 2 AS doubled FROM wp_totals WHERE id = 1")
        .unwrap();
    assert_eq!(r.decimal_scales, vec![None]);
    assert_eq!(v(&r.rows[0], "doubled"), "502");

    // Scale-0 decimals render without a fractional part.
    g.execute(s, "CREATE TABLE wp_counts (id bigint(20) unsigned NOT NULL AUTO_INCREMENT, hits decimal(10) NOT NULL DEFAULT 0, PRIMARY KEY (id)) ENGINE=InnoDB").unwrap();
    g.execute(s, "INSERT INTO wp_counts (hits) VALUES (5)").unwrap();
    let r = g.execute(s, "SELECT hits FROM wp_counts").unwrap();
    assert_eq!(r.decimal_scales, vec![Some(0)]);
    assert_eq!(v(&r.rows[0], "hits"), "5");
}

#[test]
fn decimal_scale_join_resolution() {
    // Unqualified output names resolve across the FROM tables when every
    // decimal match agrees on the scale; qualified names follow the alias.
    let g = gate("decimal_join");
    let s = g.open_session();
    g.execute(s, "CREATE TABLE wp_a (id bigint(20) unsigned NOT NULL AUTO_INCREMENT, k int NOT NULL DEFAULT 0, amount decimal(10,2) NOT NULL DEFAULT 0.00, PRIMARY KEY (id)) ENGINE=InnoDB").unwrap();
    g.execute(s, "CREATE TABLE wp_b (id bigint(20) unsigned NOT NULL AUTO_INCREMENT, k int NOT NULL DEFAULT 0, amount decimal(10,2) NOT NULL DEFAULT 0.00, name varchar(20) DEFAULT '', PRIMARY KEY (id)) ENGINE=InnoDB").unwrap();
    g.execute(s, "INSERT INTO wp_a (k, amount) VALUES (1, 10.5)").unwrap();
    g.execute(s, "INSERT INTO wp_b (k, amount, name) VALUES (1, 20.25, 'x')").unwrap();

    let r = g
        .execute(
            s,
            "SELECT a.amount, b.name FROM wp_a a JOIN wp_b b ON a.k = b.k",
        )
        .unwrap();
    assert_eq!(r.decimal_scales, vec![Some(2), None]);
    assert_eq!(v(&r.rows[0], "amount"), "10.50");
    assert_eq!(v(&r.rows[0], "name"), "x");

    // Unqualified `amount` matches both tables at scale 2 → applies.
    let r = g
        .execute(s, "SELECT amount FROM wp_a WHERE id = 1")
        .unwrap();
    assert_eq!(v(&r.rows[0], "amount"), "10.50");
}
