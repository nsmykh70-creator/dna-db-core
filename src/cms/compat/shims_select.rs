// SPDX-License-Identifier: AGPL-3.0-only
//! Scalar SELECT shims: VERSION(), DATABASE(), @@system variables,
//! LAST_INSERT_ID / FOUND_ROWS / ROW_COUNT, named advisory locks and the
//! JSON/geometry function probes CMS installers use for feature detection.

use std::time::Duration;

use super::*;
use crate::cms::locks::ReleaseResult;
use crate::relational::Value;

impl CmsSqlGate {
    // ---------------- SELECT shims ----------------

    pub(crate) fn try_select_shims(
        &self,
        session: u64,
        sql: &str,
        upper: &str,
    ) -> Result<Option<CmsQueryResult>, CmsError> {
        let compact = upper.split_whitespace().collect::<Vec<_>>().join(" ");

        if compact.contains("LAST_INSERT_ID()") {
            let id = self
                .with_session(session, |s| s.last_insert_id)
                .unwrap_or(0);
            return Ok(Some(one_value_reply(
                shim_column(sql, "LAST_INSERT_ID()").as_str(),
                Value::Int(id as i64),
            )));
        }
        if compact.contains("FOUND_ROWS()") {
            let n = self.with_session(session, |s| s.found_rows).unwrap_or(0);
            return Ok(Some(one_value_reply(
                shim_column(sql, "FOUND_ROWS()").as_str(),
                Value::Int(n as i64),
            )));
        }
        if compact.contains("ROW_COUNT()") {
            let n = self.with_session(session, |s| s.affected_rows).unwrap_or(0);
            return Ok(Some(one_value_reply(
                shim_column(sql, "ROW_COUNT()").as_str(),
                Value::Int(n as i64),
            )));
        }
        if compact == "SELECT VERSION()" || compact.starts_with("SELECT VERSION() AS") {
            return Ok(Some(one_value_reply(
                shim_column(sql, "VERSION()").as_str(),
                Value::Text(MYSQL_COMPAT_VERSION.to_string()),
            )));
        }
        if compact.contains("@@SQL_MODE") || compact.contains("@@SESSION.SQL_MODE") {
            let mode = self
                .with_session(session, |s| s.sql_mode.render())
                .unwrap_or_default();
            return Ok(Some(one_value_reply("@@sql_mode", Value::Text(mode))));
        }
        if compact.contains("@@GLOBAL.READ_ONLY") {
            // MediaWiki's serverIsReadOnly probe. DNA-DB is always writable.
            return Ok(Some(one_value_reply(
                shim_column(sql, "@@GLOBAL.READ_ONLY").as_str(),
                Value::Int(0),
            )));
        }
        // mariadb-dump asks for the effective database collation/charset and
        // the packet budget at connect; canned CMS-edition values.
        if compact.contains("@@COLLATION_DATABASE") {
            return Ok(Some(one_value_reply(
                shim_column(sql, "@@collation_database").as_str(),
                Value::Text("utf8mb4_unicode_520_ci".into()),
            )));
        }
        if compact.contains("@@CHARACTER_SET_DATABASE") {
            return Ok(Some(one_value_reply(
                shim_column(sql, "@@character_set_database").as_str(),
                Value::Text("utf8mb4".into()),
            )));
        }
        if compact.contains("@@MAX_ALLOWED_PACKET") {
            return Ok(Some(one_value_reply(
                shim_column(sql, "@@max_allowed_packet").as_str(),
                Value::Int(1_073_741_824),
            )));
        }
        // NOTE: @@VERSION_COMMENT must be checked BEFORE @@VERSION — the
        // longer name contains the shorter one as a substring, and
        // `SELECT @@version_comment` (mysqldump, PHPMyAdmin) would
        // otherwise be answered with the version number.
        if compact.contains("@@VERSION_COMMENT") {
            return Ok(Some(one_value_reply(
                "@@version_comment",
                Value::Text("DNA-DB CMS Edition (MySQL 8.0 compatible)".into()),
            )));
        }
        if compact.contains("@@VERSION") {
            return Ok(Some(one_value_reply(
                "@@version",
                Value::Text(MYSQL_COMPAT_VERSION.to_string()),
            )));
        }
        // JSON functions — Drupal's requirements check runs
        // `SELECT JSON_TYPE('1')` to detect JSON support.
        if compact.contains("JSON_TYPE(") {
            return Ok(Some(one_value_reply(
                fn_column(sql, "JSON_TYPE").as_str(),
                Value::Text("INTEGER".into()),
            )));
        }
        // Named advisory locks. Aliased forms (`SELECT GET_LOCK(...) AS
        // acquired`) must name the column after the alias — MediaWiki reads
        // `$row->acquired`.
        if let Some(args) = fn_args(sql, "GET_LOCK") {
            let (name, timeout) = lock_args(&args);
            let got = self
                .locks
                .get_lock(&name, session, Duration::from_secs(timeout));
            return Ok(Some(one_value_reply(
                fn_column(sql, "GET_LOCK").as_str(),
                Value::Int(if got { 1 } else { 0 }),
            )));
        }
        if let Some(args) = fn_args(sql, "RELEASE_LOCK") {
            let (name, _) = lock_args(&args);
            let res = self.locks.release_lock(&name, session);
            return Ok(Some(one_value_reply(
                fn_column(sql, "RELEASE_LOCK").as_str(),
                match res {
                    ReleaseResult::Released => Value::Int(1),
                    ReleaseResult::NotOwner => Value::Int(0),
                    ReleaseResult::NotExists => Value::Null,
                },
            )));
        }
        if let Some(args) = fn_args(sql, "IS_FREE_LOCK") {
            let (name, _) = lock_args(&args);
            return Ok(Some(one_value_reply(
                fn_column(sql, "IS_FREE_LOCK").as_str(),
                Value::Int(if self.locks.is_free_lock(&name) { 1 } else { 0 }),
            )));
        }
        if let Some(args) = fn_args(sql, "IS_USED_LOCK") {
            let (name, _) = lock_args(&args);
            return Ok(Some(one_value_reply(
                fn_column(sql, "IS_USED_LOCK").as_str(),
                match self.locks.is_used_lock(&name) {
                    Some(holder) => Value::Int(holder as i64),
                    None => Value::Null,
                },
            )));
        }
        Ok(None)
    }
}
