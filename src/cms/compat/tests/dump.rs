// SPDX-License-Identifier: AGPL-3.0-only

//! mysqldump / mariadb-dump compatibility tests: versioned-comment
//! unwrapping, LOCK/UNLOCK TABLES, DISABLE/ENABLE KEYS, SQL_NO_CACHE and the
//! SHOW CREATE TABLE export path (the dump file embeds its text verbatim, so
//! it must re-parse into the same overlay meta).

use super::*;

#[test]
fn versioned_comments_unwrap_to_inner_statement() {
    // ≤ 10.11.11 (101111): the body executes.
    assert_eq!(
        dialect::unwrap_versioned_comments("/*!40101 SET @saved = @@x */"),
        " SET @saved = @@x "
    );
    // Above our compat level: dropped entirely.
    assert_eq!(
        dialect::unwrap_versioned_comments("/*!110400 FUTURE_FEATURE */"),
        ""
    );
    // No version at all: executes like MySQL's `/*! */`.
    assert_eq!(dialect::unwrap_versioned_comments("/*! SET NAMES */"), " SET NAMES ");
    // Inline form inside a statement (mariadb-dump LOCK TABLES / CREATE DATABASE).
    assert_eq!(
        dialect::unwrap_versioned_comments("LOCK TABLES `t` READ /*!32311 LOCAL */"),
        "LOCK TABLES `t` READ  LOCAL "
    );
    assert_eq!(
        dialect::unwrap_versioned_comments("CREATE DATABASE /*!32312 IF NOT EXISTS*/ `db`"),
        "CREATE DATABASE  IF NOT EXISTS `db`"
    );
    // Comments inside string literals survive.
    assert_eq!(
        dialect::unwrap_versioned_comments("INSERT INTO t VALUES ('/*!40101 not a comment */')"),
        "INSERT INTO t VALUES ('/*!40101 not a comment */')"
    );
    // Plain (non-versioned) comments are dropped outside strings.
    assert_eq!(dialect::unwrap_versioned_comments("SELECT /* noise */ 1"), "SELECT  1");
    // MariaDB-specific `/*M!NNNNN … */`: the sandbox-mode marker that
    // mariadb-dump (≥ 10.11.18) writes as the first line of every dump.
    // 999999 is above any real server version — the body is dropped and the
    // standalone statement collapses to a comment-only no-op (OK).
    assert_eq!(
        dialect::unwrap_versioned_comments("/*M!999999\\- enable the sandbox mode */"),
        ""
    );
    // Real MariaDB directives in the M! form (≤ compat level) still execute.
    assert_eq!(
        dialect::unwrap_versioned_comments("/*M!100100 SET @mariadb_thing=1 */"),
        " SET @mariadb_thing=1 "
    );
}

#[test]
fn dump_flow_statements_answer_ok() {
    let g = gate("dump_flow");
    let s = g.open_session();
    g.execute(s, "CREATE TABLE t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name varchar(20) NOT NULL DEFAULT '', data text)").unwrap();

    // Import direction: LOCK/UNLOCK and the KEYS maintenance hints.
    let r = g.execute(s, "LOCK TABLES `t` WRITE").unwrap();
    assert!(r.is_select == false);
    g.execute(s, "UNLOCK TABLES").unwrap();
    g.execute(s, "ALTER TABLE `t` DISABLE KEYS").unwrap();
    g.execute(s, "/*!40000 ALTER TABLE `t` ENABLE KEYS */").unwrap();
    g.execute(s, "FLUSH TABLES").unwrap();

    // mariadb-dump ≥ 10.11.18 opens every dump with the sandbox-mode marker:
    // a MariaDB `/*M!*/` comment as a standalone statement. Both the raw
    // wire form (with the `\-` escape) and what the client sends after
    // stripping it must answer OK, not errno 1064.
    g.execute(s, "/*M!999999\\- enable the sandbox mode */").unwrap();
    g.execute(s, "/*M!999999 enable the sandbox mode */").unwrap();

    // mysqldump's per-table copy query runs against the gate.
    g.execute(s, "INSERT INTO t (name, data) VALUES ('a', 'b')").unwrap();
    let r = g.execute(s, "SELECT /*!40001 SQL_NO_CACHE */ * FROM `t`").unwrap();
    assert_eq!(r.rows.len(), 1);

    // Comment-only statements (version above our level) answer OK.
    g.execute(s, "/*!110400 FUTURE_FEATURE */").unwrap();
}

#[test]
fn show_create_table_roundtrips_through_the_overlay() {
    let g = gate("sct");
    let s = g.open_session();
    let ddl = "CREATE TABLE `wp_demo` (\
\n  `ID` bigint(20) unsigned NOT NULL AUTO_INCREMENT,\
\n  `name` varchar(191) NOT NULL DEFAULT 'it''s',\
\n  `price` decimal(10,2) NOT NULL DEFAULT '0.00',\
\n  `body` text,\
\n  `created` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,\
\n  PRIMARY KEY (`ID`),\
\n  UNIQUE KEY `name` (`name`),\
\n  KEY `price_idx` (`price`)\
\n) ENGINE=InnoDB AUTO_INCREMENT=7 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci";
    g.execute(s, ddl).unwrap();

    let r = g.execute(s, "SHOW CREATE TABLE `wp_demo`").unwrap();
    assert_eq!(r.columns, vec!["Table".to_string(), "Create Table".to_string()]);
    let out = v(&r.rows[0], "Create Table");
    assert!(out.starts_with("CREATE TABLE `wp_demo` ("), "got: {}", out);
    assert!(out.contains("`ID` bigint(20) unsigned NOT NULL AUTO_INCREMENT"));
    assert!(out.contains("DEFAULT 'it''s'"), "escaped default: {}", out);
    assert!(out.contains("`price` decimal(10,2) NOT NULL DEFAULT '0.00'"));
    assert!(out.contains("`body` text DEFAULT NULL"));
    assert!(out.contains("DEFAULT CURRENT_TIMESTAMP"));
    assert!(out.contains("PRIMARY KEY (`ID`)"));
    assert!(out.contains("UNIQUE KEY `name` (`name`)"));
    assert!(out.contains("KEY `price_idx` (`price`)"));
    assert!(
        out.ends_with("ENGINE=InnoDB AUTO_INCREMENT=7 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
        "table options tail: {}", out
    );

    // Idempotency: re-parsing the emitted text reproduces the same meta —
    // this is what makes the exported dump re-importable.
    let again = crate::cms::ddl_meta::CmsTableMeta::parse_create_table(&out).unwrap();
    assert_eq!(again.show_create_table_ddl(), out);

    // Missing table → errno 1146 like real MySQL.
    let err = g.execute(s, "SHOW CREATE TABLE `nope`").unwrap_err();
    assert_eq!(err.errno, errno::ER_NO_SUCH_TABLE);
}

#[test]
fn fk_bearing_create_table_still_parses_in_the_engine() {
    let g = gate("fk_dump");
    let s = g.open_session();
    let ddl = "CREATE TABLE `child` (\
\n  `id` int(11) NOT NULL AUTO_INCREMENT,\
\n  `parent_id` int(11) DEFAULT NULL,\
\n  `tag` varchar(50) DEFAULT NULL,\
\n  PRIMARY KEY (`id`),\
\n  KEY `idx_tag` (`tag`),\
\n  CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`) ON DELETE CASCADE,\
\n  CHECK (`id` > 0)\
\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
    g.execute(s, ddl).unwrap();
    g.execute(
        s,
        "INSERT INTO `child` (`id`, `parent_id`, `tag`) VALUES (1, 10, 'x'), (2, NULL, 'y')",
    )
    .unwrap();
    let r = g.execute(s, "SELECT COUNT(*) AS c FROM `child`").unwrap();
    assert_eq!(v(&r.rows[0], "c"), "2");
    // The overlay kept the columns (FK segments are skipped there too).
    let r = g.execute(s, "SHOW CREATE TABLE `child`").unwrap();
    let out = v(&r.rows[0], "Create Table");
    assert!(out.contains("`parent_id` int(11) DEFAULT NULL"));
    assert!(out.contains("KEY `idx_tag` (`tag`)"));
}

#[test]
fn routine_catalog_probes_come_back_empty() {
    let g = gate("routines");
    let s = g.open_session();
    for q in [
        "SHOW TRIGGERS",
        "SHOW TRIGGERS LIKE 'wp\\_posts'",
        "SHOW EVENTS",
        "SHOW FUNCTION STATUS",
        "SHOW PROCEDURE STATUS WHERE Db = 'wordpress'",
    ] {
        let r = g.execute(s, q).unwrap();
        assert!(r.rows.is_empty(), "{} must list no routines", q);
        assert!(!r.columns.is_empty(), "{} must carry column metadata", q);
    }
    // SHOW TABLE STATUS answers InnoDB-shaped rows for known tables.
    g.execute(s, "CREATE TABLE ts_t (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)").unwrap();
    g.execute(s, "INSERT INTO ts_t (id) VALUES (0)").unwrap();
    let r = g.execute(s, "SHOW TABLE STATUS LIKE 'ts\\_t'").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(v(&r.rows[0], "Name"), "ts_t");
    assert_eq!(v(&r.rows[0], "Engine"), "InnoDB");
    assert_eq!(v(&r.rows[0], "Auto_increment"), "2");
}
