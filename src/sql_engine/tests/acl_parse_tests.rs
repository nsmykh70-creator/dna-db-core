// SPDX-License-Identifier: AGPL-3.0-only
//! Verbatim module from the pre-split monolith.

use super::*;

#[cfg(test)]
mod acl_parse_tests {
    use super::*;

    #[test]
    fn collects_select_join_and_subquery_tables() {
        let tables = referenced_tables(
            "SELECT u.username FROM chr_users u JOIN chr_orders o ON u.user_id = o.user_id \
             WHERE u.user_id IN (SELECT user_id FROM chr_bans)",
        )
        .unwrap();
        assert_eq!(
            tables,
            vec!["chr_bans".to_string(), "chr_orders".to_string(), "chr_users".to_string()]
        );
    }

    #[test]
    fn collects_create_table_foreign_keys() {
        let tables = referenced_tables(
            "CREATE TABLE chr_orders (id INT, user_id INT, PRIMARY KEY (id), \
             FOREIGN KEY (user_id) REFERENCES chr_users(user_id))",
        )
        .unwrap();
        assert!(tables.contains(&"chr_orders".to_string()));
        assert!(tables.contains(&"chr_users".to_string()));
    }
}
