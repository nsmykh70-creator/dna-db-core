// SPDX-License-Identifier: AGPL-3.0-only
//! CMS wire-compat regression tests (REPLACE INTO, INSERT..SELECT, index hints, Joomla quirks).

use super::*;

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

#[test]
fn key_named_with_type_keyword_is_a_constraint() {
    // Joomla base.sql: `KEY time (time)` / `KEY userid (userid)` — the
    // index name may be ANY identifier, including type keywords; only the
    // paren content (identifiers = columns vs numbers = type length)
    // distinguishes it from a column named `key` with a length-carrying
    // type (`KEY char(32)`).
    let g = gate("key_time");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE jsml_session (
            session_id varbinary(192) NOT NULL,
            client_id tinyint unsigned DEFAULT NULL,
            guest tinyint unsigned DEFAULT 1,
            time int NOT NULL DEFAULT 0,
            userid int DEFAULT 0,
            PRIMARY KEY (session_id),
            KEY userid (userid),
            KEY time (time),
            KEY client_id_guest (client_id, guest)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 DEFAULT COLLATE=utf8mb4_unicode_ci",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_session (session_id, time, userid) VALUES ('abc123', 1700000000, 42)",
    )
    .unwrap();
    let r = g
        .execute(s, "SELECT time, userid FROM jsml_session WHERE session_id = 'abc123'")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "time"), "1700000000");
    assert_eq!(v(&r.rows[0], "userid"), "42");

    // A column named `key` whose type carries a paren stays a column.
    g.execute(
        s,
        "CREATE TABLE jsml_keycol (id int PRIMARY KEY, `key` char(32) NOT NULL DEFAULT '')",
    )
    .unwrap();
    g.execute(s, "INSERT INTO jsml_keycol (id, `key`) VALUES (1, 'k1')").unwrap();
    let r = g.execute(s, "SELECT `key` FROM jsml_keycol WHERE id = 1").unwrap();
    assert_eq!(v(&r.rows[0], "key"), "k1");
}

#[test]
fn alter_database_is_a_noop_ok() {
    // Joomla's installer: ALTER DATABASE ... CHARACTER SET ... COLLATE ...
    // right after CREATE DATABASE — answered with a plain OK packet.
    let g = gate("alter_db");
    let s = g.open_session();
    let r = g
        .execute(
            s,
            "ALTER DATABASE `dnajoomla` CHARACTER SET `utf8mb4` COLLATE `utf8mb4_unicode_ci`",
        )
        .unwrap();
    assert!(r.message.is_some());
}

#[test]
fn prefix_indexes_in_pk_and_nameless_index() {
    // Joomla supports.sql: webauthn_credentials uses `PRIMARY KEY (id(100))`
    // (prefix length on the FIRST PK column) and a nameless `INDEX (col(100))`.
    let g = gate("prefix_idx");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE jsml_webauthn_credentials (
            id VARCHAR(1000) NOT NULL COMMENT 'Credential ID',
            user_id VARCHAR(128) NOT NULL COMMENT 'User handle',
            label VARCHAR(190) NOT NULL COMMENT 'Human readable label',
            credential MEDIUMTEXT NOT NULL COMMENT 'Credential source data, JSON format',
            PRIMARY KEY (id(100)),
            INDEX (user_id(100))
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 DEFAULT COLLATE=utf8mb4_unicode_ci",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_webauthn_credentials (id, user_id, label, credential) VALUES ('cred-1', 'u42', 'yubikey', '{}')",
    )
    .unwrap();
    // PK point lookup by the full column value (prefix index ≈ whole-column).
    let r = g
        .execute(s, "SELECT label FROM jsml_webauthn_credentials WHERE id = 'cred-1'")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "label"), "yubikey");
}

#[test]
fn joomla_extensions_sql_seed_quirks() {
    // Joomla extensions.sql: negative DEFAULTs, scalar functions
    // (CONCAT/NOW/DATE_FORMAT/TIME_FORMAT + INTERVAL arithmetic) in
    // INSERT VALUES, and a PK-less table (finder_terms_common) whose rows
    // are keyed by the implicit hidden __rowid.
    let g = gate("ext_quirks");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE jsml_banners (id int unsigned NOT NULL AUTO_INCREMENT, purchase_type tinyint NOT NULL DEFAULT -1, track_clicks tinyint NOT NULL DEFAULT -1, PRIMARY KEY (id))",
    )
    .unwrap();
    g.execute(s, "INSERT INTO jsml_banners (id) VALUES (1)").unwrap();
    let r = g
        .execute(s, "SELECT purchase_type FROM jsml_banners WHERE id = 1")
        .unwrap();
    assert_eq!(v(&r.rows[0], "purchase_type"), "-1");

    // scheduler rows: function calls + INTERVAL arithmetic in VALUES
    g.execute(
        s,
        "CREATE TABLE jsml_scheduler_tasks (id int unsigned NOT NULL AUTO_INCREMENT, rule_type varchar(50) NOT NULL DEFAULT '', next_execution datetime, last_execution datetime, PRIMARY KEY (id))",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_scheduler_tasks (id, rule_type, next_execution, last_execution) VALUES (1, CONCAT('interval', '-days'), DATE_FORMAT(NOW() + INTERVAL 30 DAY, '%Y-%m-%d %H:00:00'), DATE_FORMAT(NOW(), '%e')), (2, 'session.gc', DATE_FORMAT(NOW() + INTERVAL 24 HOUR, '%Y-%m-%d %H:00:00'), TIME_FORMAT(NOW(), '%H:00'))",
    )
    .unwrap();
    let r = g
        .execute(s, "SELECT rule_type, next_execution FROM jsml_scheduler_tasks WHERE id = 1")
        .unwrap();
    assert_eq!(v(&r.rows[0], "rule_type"), "interval-days");
    let next1 = v(&r.rows[0], "next_execution");
    // NOW()+30 days: same clock time, date shifted by a month-ish
    assert_eq!(next1.len(), 19, "DATETIME shape: {next1}");
    let r = g
        .execute(s, "SELECT next_execution FROM jsml_scheduler_tasks WHERE id = 2")
        .unwrap();
    assert_eq!(v(&r.rows[0], "next_execution").len(), 19);

    // PK-less table: rows are keyed by the hidden __rowid; SELECT * does
    // NOT expose it, inserts without any PK succeed.
    g.execute(
        s,
        "CREATE TABLE jsml_finder_terms_common (term varchar(75) NOT NULL DEFAULT '', language char(7) NOT NULL DEFAULT '', custom int NOT NULL DEFAULT 0, UNIQUE KEY idx_term_language (term, language), KEY idx_lang (language))",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_finder_terms_common (term, language, custom) VALUES ('joomla', 'en-GB', 0)",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT term, language, custom FROM jsml_finder_terms_common WHERE term = 'joomla'",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "language"), "en-GB");
    // SELECT * never leaks the hidden key column
    let r = g.execute(s, "SELECT * FROM jsml_finder_terms_common").unwrap();
    assert_eq!(r.columns, vec!["term", "language", "custom"]);
    // duplicate rows are legal in a PK-less MySQL table
    g.execute(
        s,
        "INSERT INTO jsml_finder_terms_common (term, language, custom) VALUES ('joomla', 'de-DE', 1)",
    )
    .unwrap();
    let r = g
        .execute(s, "SELECT COUNT(*) c FROM jsml_finder_terms_common")
        .unwrap();
    assert_eq!(v(&r.rows[0], "c"), "2");
}

#[test]
fn case_expression_through_gate() {
    let g = gate("case_gate");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE jsml_content (id int PRIMARY KEY, title varchar(100), state int, publish_up datetime, created datetime)",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_content (id, title, state, publish_up, created) VALUES (1, 't', 1, NULL, '2026-01-01 10:00:00')",
    )
    .unwrap();
    // the exact Joomla ArticlesModel projection shape (first CASE item)
    let r = g
        .execute(
            s,
            "SELECT `a`.`id`,`a`.`title`,CASE WHEN `a`.`publish_up` IS NULL THEN `a`.`created` ELSE `a`.`publish_up` END AS `publish_up` FROM jsml_content AS `a` WHERE `a`.`state` = 1",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "publish_up"), "2026-01-01 10:00:00");
}

#[test]
fn order_by_case_expression() {
    // Joomla frontpage: ORDER BY c.lft, a.featured DESC, fp.ordering,
    // CASE WHEN a.publish_up IS NULL THEN a.created ELSE a.publish_up END DESC
    let g = gate("order_case");
    let s = g.open_session();
    g.execute(
        s,
        "CREATE TABLE jsml_articles (id int PRIMARY KEY, title varchar(100), publish_up datetime, created datetime, state int)",
    )
    .unwrap();
    g.execute(
        s,
        "INSERT INTO jsml_articles (id, title, publish_up, created, state) VALUES (1, 'old-published', '2026-01-01 10:00:00', '2026-01-01 09:00:00', 1), (2, 'null-published', NULL, '2026-02-01 09:00:00', 1)",
    )
    .unwrap();
    let r = g
        .execute(
            s,
            "SELECT id, CASE WHEN publish_up IS NULL THEN created ELSE publish_up END AS sortkey FROM jsml_articles WHERE state = 1 ORDER BY CASE WHEN publish_up IS NULL THEN created ELSE publish_up END DESC",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    // the NULL publish_up row falls back to created (2026-02-01) — newer
    assert_eq!(v(&r.rows[0], "id"), "2");
    assert_eq!(v(&r.rows[1], "id"), "1");
}
