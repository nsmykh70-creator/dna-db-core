// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL server error codes and SQLSTATE classes used by CMS workloads.
//!
//! WordPress (`wpdb`) exposes `mysql_errno()` to plugins and treats specific
//! codes specially; WooCommerce retries transactions on deadlock/lock-timeout;
//! `dbDelta()` inspects duplicate-table / duplicate-column situations. The
//! CMS compat layer therefore has to surface *these* codes — never a generic
//! "engine error". Codes follow MySQL 5.7/8.0 server-error numbering.

/// `Access denied for user` — wrong credentials in `wp-config.php`.
pub const ER_ACCESS_DENIED_ERROR: u16 = 1045;
/// `No database selected`.
pub const ER_NO_DB_ERROR: u16 = 1046;
/// `Unknown database` — the site's DB name does not exist.
pub const ER_BAD_DB_ERROR: u16 = 1049;
/// `Table '…' already exists` — plugin activation re-runs.
pub const ER_TABLE_EXISTS_ERROR: u16 = 1050;
/// `Unknown column '…' in '…'`.
pub const ER_BAD_FIELD_ERROR: u16 = 1054;
/// `Duplicate entry '…' for key '…'` — options autoload, transients, coupons.
pub const ER_DUP_ENTRY: u16 = 1062;
/// `You have an error in your SQL syntax` — dialect gap; must never be
/// reported to CMS clients once the compat layer is complete.
pub const ER_PARSE_ERROR: u16 = 1064;
/// `Specified key was too long; max key length is …` — the classic
/// utf8mb4 index-prefix (191 bytes) migration pitfall.
pub const ER_TOO_LONG_KEY: u16 = 1071;
/// `Column '…' cannot be null`.
pub const ER_BAD_NULL_ERROR: u16 = 1048;
/// `Can't DROP '…'; check that column/key exists` — `dbDelta()` drops.
pub const ER_CANT_DROP_FIELD_OR_KEY: u16 = 1091;
/// `Row size/data mismatch on INSERT` (column count).
pub const ER_WRONG_VALUE_COUNT_ON_ROW: u16 = 1136;
/// `Table '…' doesn't exist` — missing schema before activation.
pub const ER_NO_SUCH_TABLE: u16 = 1146;
/// `Incorrect string value: '\xF0\x9F…'` — 4-byte emoji against utf8mb3;
/// CMS edition always speaks utf8mb4, so this must be impossible.
pub const ER_INCORRECT_STRING_VALUE: u16 = 1366;
/// `Incorrect … value: '0000-00-00 …'` — legacy zero-dates in drafts.
pub const ER_TRUNCATED_WRONG_VALUE: u16 = 1292;
/// `Unknown system variable '…'` — `SET sql_mode`/`SET NAMES` dialect.
pub const ER_UNKNOWN_SYSTEM_VARIABLE: u16 = 1193;
/// `Incorrect argument type to variable '…'` — bad SET value.
pub const ER_WRONG_VALUE_FOR_VAR: u16 = 1231;
/// `Lock wait timeout exceeded` — innodb_lock_wait_timeout analogue.
pub const ER_LOCK_WAIT_TIMEOUT: u16 = 1205;
/// `Deadlock found when trying to get lock` — WooCommerce retry loop target.
pub const ER_LOCK_DEADLOCK: u16 = 1213;
/// `Statement aborted because lock(s) could not be acquired immediately and
/// NOWAIT is set` — `SELECT … FOR UPDATE NOWAIT` on a held row.
pub const ER_LOCK_NOWAIT: u16 = 3572;
/// `FUNCTION … does not exist` — plugins calling MySQL built-ins we lack.
pub const ER_SP_DOES_NOT_EXIST: u16 = 1305;
/// `Data too long for column '…'`.
pub const ER_DATA_TOO_LONG: u16 = 1406;
/// `Cannot add or update a child row: a foreign key constraint fails`.
pub const ER_NO_REFERENCED_ROW_2: u16 = 1452;
/// `Cannot delete or update a parent row: a foreign key constraint fails`.
pub const ER_ROW_IS_REFERENCED_2: u16 = 1451;

/// Returns the canonical MySQL error name for a code (subset).
pub fn name(code: u16) -> Option<&'static str> {
    let s = match code {
        ER_ACCESS_DENIED_ERROR => "ER_ACCESS_DENIED_ERROR",
        ER_NO_DB_ERROR => "ER_NO_DB_ERROR",
        ER_BAD_DB_ERROR => "ER_BAD_DB_ERROR",
        ER_TABLE_EXISTS_ERROR => "ER_TABLE_EXISTS_ERROR",
        ER_BAD_FIELD_ERROR => "ER_BAD_FIELD_ERROR",
        ER_DUP_ENTRY => "ER_DUP_ENTRY",
        ER_PARSE_ERROR => "ER_PARSE_ERROR",
        ER_TOO_LONG_KEY => "ER_TOO_LONG_KEY",
        ER_BAD_NULL_ERROR => "ER_BAD_NULL_ERROR",
        ER_CANT_DROP_FIELD_OR_KEY => "ER_CANT_DROP_FIELD_OR_KEY",
        ER_WRONG_VALUE_COUNT_ON_ROW => "ER_WRONG_VALUE_COUNT_ON_ROW",
        ER_NO_SUCH_TABLE => "ER_NO_SUCH_TABLE",
        ER_INCORRECT_STRING_VALUE => "ER_INCORRECT_STRING_VALUE",
        ER_TRUNCATED_WRONG_VALUE => "ER_TRUNCATED_WRONG_VALUE",
        ER_UNKNOWN_SYSTEM_VARIABLE => "ER_UNKNOWN_SYSTEM_VARIABLE",
        ER_WRONG_VALUE_FOR_VAR => "ER_WRONG_VALUE_FOR_VAR",
        ER_LOCK_WAIT_TIMEOUT => "ER_LOCK_WAIT_TIMEOUT",
        ER_LOCK_DEADLOCK => "ER_LOCK_DEADLOCK",
        ER_LOCK_NOWAIT => "ER_LOCK_NOWAIT",
        ER_SP_DOES_NOT_EXIST => "ER_SP_DOES_NOT_EXIST",
        ER_DATA_TOO_LONG => "ER_DATA_TOO_LONG",
        ER_NO_REFERENCED_ROW_2 => "ER_NO_REFERENCED_ROW_2",
        ER_ROW_IS_REFERENCED_2 => "ER_ROW_IS_REFERENCED_2",
        _ => return None,
    };
    Some(s)
}

/// Maps a MySQL server error code to the SQLSTATE class the MySQL server
/// reports for it (subset; `HY000` is the general class).
pub fn sqlstate(code: u16) -> &'static str {
    match code {
        // Integrity constraint violations.
        ER_DUP_ENTRY | ER_BAD_NULL_ERROR | ER_NO_REFERENCED_ROW_2 | ER_ROW_IS_REFERENCED_2 => "23000",
        // Table/column not found ("42" = syntax error and rule violation).
        ER_NO_SUCH_TABLE => "42S02",
        ER_BAD_FIELD_ERROR => "42S22",
        ER_TABLE_EXISTS_ERROR => "42S01",
        ER_CANT_DROP_FIELD_OR_KEY | ER_TOO_LONG_KEY | ER_PARSE_ERROR | ER_UNKNOWN_SYSTEM_VARIABLE => "42000",
        ER_BAD_DB_ERROR => "42000",
        // Transaction deadlock — J2C/ISO class for serialization failures.
        ER_LOCK_DEADLOCK => "40001",
        ER_LOCK_WAIT_TIMEOUT | ER_LOCK_NOWAIT => "HY000",
        // Data-length violations.
        ER_DATA_TOO_LONG => "22001",
        ER_TRUNCATED_WRONG_VALUE => "22007",
        ER_ACCESS_DENIED_ERROR => "28000",
        ER_NO_DB_ERROR => "3D000",
        ER_INCORRECT_STRING_VALUE | ER_SP_DOES_NOT_EXIST | ER_WRONG_VALUE_COUNT_ON_ROW => "HY000",
        _ => "HY000",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cms_critical_codes_are_named() {
        // Codes that CMS code branches on must be present.
        for c in [ER_DUP_ENTRY, ER_NO_SUCH_TABLE, ER_BAD_FIELD_ERROR, ER_LOCK_DEADLOCK, ER_LOCK_WAIT_TIMEOUT] {
            assert!(name(c).is_some(), "missing name for {c}");
        }
    }

    #[test]
    fn sqlstate_classes_match_mysql() {
        assert_eq!(sqlstate(ER_DUP_ENTRY), "23000");
        assert_eq!(sqlstate(ER_NO_SUCH_TABLE), "42S02");
        assert_eq!(sqlstate(ER_BAD_FIELD_ERROR), "42S22");
        assert_eq!(sqlstate(ER_LOCK_DEADLOCK), "40001");
        assert_eq!(sqlstate(ER_PARSE_ERROR), "42000");
        // Unknown codes fall back to the general class.
        assert_eq!(sqlstate(65_535), "HY000");
    }
}
