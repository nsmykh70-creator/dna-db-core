// SPDX-License-Identifier: AGPL-3.0-only
//! WordPress core schema preset (12 tables, WP 4.4+ / 6.x utf8mb4 shape).
//!
//! The DDL strings are the faithful MySQL definitions from
//! `wp-admin/includes/schema.php` — they feed the catalog overlay
//! ([`crate::cms::ddl_meta::CmsTableMeta`]) so `SHOW FULL COLUMNS` /
//! `SHOW INDEX` answer exactly what dbDelta expects. The engine tables are
//! created from `normalized_engine_ddl()` (base types, single-column PK,
//! AUTO_INCREMENT recorded); composite keys keep full truth in the overlay
//! only (see CMS_EDITION_RU.md §4).
//!
//! WooCommerce/HPOS tables are NOT part of this preset — phase 2.

use crate::cms::ddl_meta::CmsTableMeta;

/// (table name, faithful MySQL DDL) for the WordPress core schema.
pub const WP_CORE_TABLES: &[(&str, &str)] = &[
    ("wp_termmeta", "CREATE TABLE wp_termmeta (
        meta_id bigint(20) unsigned NOT NULL auto_increment,
        term_id bigint(20) unsigned NOT NULL default '0',
        meta_key varchar(255) default NULL,
        meta_value longtext,
        PRIMARY KEY  (meta_id),
        KEY term_id (term_id),
        KEY meta_key (meta_key(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_terms", "CREATE TABLE wp_terms (
        term_id bigint(20) unsigned NOT NULL auto_increment,
        name varchar(200) NOT NULL default '',
        slug varchar(200) NOT NULL default '',
        term_group bigint(10) NOT NULL default 0,
        PRIMARY KEY  (term_id),
        KEY slug (slug(191)),
        KEY name (name(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_term_taxonomy", "CREATE TABLE wp_term_taxonomy (
        term_taxonomy_id bigint(20) unsigned NOT NULL auto_increment,
        term_id bigint(20) unsigned NOT NULL default 0,
        taxonomy varchar(32) NOT NULL default '',
        description longtext NOT NULL,
        parent bigint(20) unsigned NOT NULL default 0,
        count bigint(10) NOT NULL default 0,
        PRIMARY KEY  (term_taxonomy_id),
        UNIQUE KEY term_id_taxonomy (term_id, taxonomy),
        KEY taxonomy (taxonomy)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_term_relationships", "CREATE TABLE wp_term_relationships (
        object_id bigint(20) unsigned NOT NULL default 0,
        term_taxonomy_id bigint(20) unsigned NOT NULL default 0,
        term_order int(11) NOT NULL default 0,
        PRIMARY KEY  (object_id, term_taxonomy_id),
        KEY term_taxonomy_id (term_taxonomy_id)
    ) ENGINE=InnoDB"),
    ("wp_commentmeta", "CREATE TABLE wp_commentmeta (
        meta_id bigint(20) unsigned NOT NULL auto_increment,
        comment_id bigint(20) unsigned NOT NULL default '0',
        meta_key varchar(255) default NULL,
        meta_value longtext,
        PRIMARY KEY  (meta_id),
        KEY comment_id (comment_id),
        KEY meta_key (meta_key(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_comments", "CREATE TABLE wp_comments (
        comment_ID bigint(20) unsigned NOT NULL auto_increment,
        comment_post_ID bigint(20) unsigned NOT NULL default '0',
        comment_author tinytext NOT NULL,
        comment_author_email varchar(100) NOT NULL default '',
        comment_author_url varchar(200) NOT NULL default '',
        comment_author_IP varchar(100) NOT NULL default '',
        comment_date datetime NOT NULL default '0000-00-00 00:00:00',
        comment_date_gmt datetime NOT NULL default '0000-00-00 00:00:00',
        comment_content text NOT NULL,
        comment_karma int(11) NOT NULL default 0,
        comment_approved varchar(20) NOT NULL default '1',
        comment_agent varchar(255) NOT NULL default '',
        comment_type varchar(20) NOT NULL default 'comment',
        comment_parent bigint(20) unsigned NOT NULL default 0,
        user_id bigint(20) unsigned NOT NULL default 0,
        PRIMARY KEY  (comment_ID),
        KEY comment_post_ID (comment_post_ID),
        KEY comment_approved_date_gmt (comment_approved, comment_date_gmt),
        KEY comment_date_gmt (comment_date_gmt),
        KEY comment_parent (comment_parent),
        KEY comment_author_email (comment_author_email(10))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_links", "CREATE TABLE wp_links (
        link_id bigint(20) unsigned NOT NULL auto_increment,
        link_url varchar(255) NOT NULL default '',
        link_name varchar(255) NOT NULL default '',
        link_image varchar(255) NOT NULL default '',
        link_target varchar(25) NOT NULL default '',
        link_description varchar(255) NOT NULL default '',
        link_visible varchar(20) NOT NULL default 'Y',
        link_owner bigint(20) unsigned NOT NULL default 1,
        link_rating int(11) NOT NULL default 0,
        link_updated datetime NOT NULL default '0000-00-00 00:00:00',
        link_rel varchar(255) NOT NULL default '',
        link_notes mediumtext NOT NULL,
        link_rss varchar(255) NOT NULL default '',
        PRIMARY KEY  (link_id),
        KEY link_visible (link_visible)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_options", "CREATE TABLE wp_options (
        option_id bigint(20) unsigned NOT NULL auto_increment,
        option_name varchar(191) NOT NULL default '',
        option_value longtext NOT NULL,
        autoload varchar(20) NOT NULL default 'yes',
        PRIMARY KEY  (option_id),
        UNIQUE KEY option_name (option_name)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_postmeta", "CREATE TABLE wp_postmeta (
        meta_id bigint(20) unsigned NOT NULL auto_increment,
        post_id bigint(20) unsigned NOT NULL default '0',
        meta_key varchar(255) default NULL,
        meta_value longtext,
        PRIMARY KEY  (meta_id),
        KEY post_id (post_id),
        KEY meta_key (meta_key(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_posts", "CREATE TABLE wp_posts (
        ID bigint(20) unsigned NOT NULL auto_increment,
        post_author bigint(20) unsigned NOT NULL default '0',
        post_date datetime NOT NULL default '0000-00-00 00:00:00',
        post_date_gmt datetime NOT NULL default '0000-00-00 00:00:00',
        post_content longtext NOT NULL,
        post_title text NOT NULL,
        post_excerpt text NOT NULL,
        post_status varchar(20) NOT NULL default 'publish',
        comment_status varchar(20) NOT NULL default 'open',
        ping_status varchar(20) NOT NULL default 'open',
        post_password varchar(255) NOT NULL default '',
        post_name varchar(200) NOT NULL default '',
        to_ping text NOT NULL,
        pinged text NOT NULL,
        post_modified datetime NOT NULL default '0000-00-00 00:00:00',
        post_modified_gmt datetime NOT NULL default '0000-00-00 00:00:00',
        post_content_filtered longtext NOT NULL,
        post_parent bigint(20) unsigned NOT NULL default '0',
        guid varchar(255) NOT NULL default '',
        menu_order int(11) NOT NULL default '0',
        post_type varchar(20) NOT NULL default 'post',
        post_mime_type varchar(100) NOT NULL default '',
        comment_count bigint(20) NOT NULL default '0',
        PRIMARY KEY  (ID),
        KEY post_name (post_name(191)),
        KEY type_status_date (post_type, post_status, post_date, ID),
        KEY post_parent (post_parent),
        KEY post_author (post_author)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_users", "CREATE TABLE wp_users (
        ID bigint(20) unsigned NOT NULL auto_increment,
        user_login varchar(60) NOT NULL default '',
        user_pass varchar(255) NOT NULL default '',
        user_nicename varchar(50) NOT NULL default '',
        user_email varchar(100) NOT NULL default '',
        user_url varchar(100) NOT NULL default '',
        user_registered datetime NOT NULL default '0000-00-00 00:00:00',
        user_activation_key varchar(255) NOT NULL default '',
        user_status int(11) NOT NULL default '0',
        display_name varchar(250) NOT NULL default '',
        PRIMARY KEY  (ID),
        KEY user_login_key (user_login),
        KEY user_nicename (user_nicename),
        KEY user_email (user_email)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
    ("wp_usermeta", "CREATE TABLE wp_usermeta (
        umeta_id bigint(20) unsigned NOT NULL auto_increment,
        user_id bigint(20) unsigned NOT NULL default '0',
        meta_key varchar(255) default NULL,
        meta_value longtext,
        PRIMARY KEY  (umeta_id),
        KEY user_id (user_id),
        KEY meta_key (meta_key(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci"),
];

/// Expected AUTO_INCREMENT columns of the core schema (sanity anchor).
pub const WP_AUTO_INC_COLUMNS: &[(&str, &str)] = &[
    ("wp_termmeta", "meta_id"),
    ("wp_terms", "term_id"),
    ("wp_term_taxonomy", "term_taxonomy_id"),
    ("wp_commentmeta", "meta_id"),
    ("wp_comments", "comment_ID"),
    ("wp_links", "link_id"),
    ("wp_options", "option_id"),
    ("wp_postmeta", "meta_id"),
    ("wp_posts", "ID"),
    ("wp_users", "ID"),
    ("wp_usermeta", "umeta_id"),
];

/// `true` for the 12 WordPress core tables.
pub fn is_core_table(name: &str) -> bool {
    WP_CORE_TABLES.iter().any(|(n, _)| *n == name)
}

/// Parses a core table's DDL into overlay metadata.
pub fn core_table_meta(table: &str) -> Option<CmsTableMeta> {
    WP_CORE_TABLES
        .iter()
        .find(|(n, _)| *n == table)
        .and_then(|(_, ddl)| CmsTableMeta::parse_create_table(ddl).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_core_tables_parse() {
        assert_eq!(WP_CORE_TABLES.len(), 12);
        for (name, ddl) in WP_CORE_TABLES {
            let meta = CmsTableMeta::parse_create_table(ddl)
                .unwrap_or_else(|e| panic!("{}: {}", name, e));
            assert_eq!(meta.table, *name);
            assert!(!meta.columns.is_empty());
        }
    }

    #[test]
    fn auto_increment_columns_match_schema_php() {
        for (table, col) in WP_AUTO_INC_COLUMNS {
            let meta = core_table_meta(table).expect(table);
            assert_eq!(
                meta.auto_increment_column.as_deref(),
                Some(*col),
                "auto_increment column of {}",
                table
            );
        }
    }

    #[test]
    fn composite_keys_survive_in_overlay() {
        let rel = core_table_meta("wp_term_relationships").unwrap();
        assert_eq!(rel.primary_key, vec!["object_id", "term_taxonomy_id"]);
        // Engine DDL keeps only the first PK column.
        assert!(rel.normalized_engine_ddl().contains("PRIMARY KEY (object_id)"));

        let tt = core_table_meta("wp_term_taxonomy").unwrap();
        let uniq = tt.indexes.iter().find(|i| i.name == "term_id_taxonomy").unwrap();
        assert!(uniq.unique);
        assert_eq!(uniq.columns.len(), 2);
    }

    #[test]
    fn posts_ddl_has_wide_index_set() {
        let p = core_table_meta("wp_posts").unwrap();
        let names: Vec<&str> = p.indexes.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"post_name"));
        assert!(names.contains(&"type_status_date"));
        assert!(names.contains(&"post_parent"));
        assert!(names.contains(&"post_author"));
        // PK column is the MySQL-cased `ID`.
        assert_eq!(p.primary_key, vec!["ID"]);
        assert_eq!(p.auto_increment_column.as_deref(), Some("ID"));
    }

    #[test]
    fn meta_tables_prefix_index_at_191() {
        for t in ["wp_postmeta", "wp_usermeta", "wp_termmeta", "wp_commentmeta"] {
            let m = core_table_meta(t).unwrap();
            let mk = m.indexes.iter().find(|i| i.name == "meta_key").unwrap();
            assert_eq!(mk.columns[0].1, Some(191), "{}.meta_key sub_part", t);
        }
    }
}
