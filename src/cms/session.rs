// SPDX-License-Identifier: AGPL-3.0-only
//! Per-connection session state expected by MySQL-dialect CMS clients.
//!
//! WordPress `wpdb` and Joomla `DatabaseDriver` assume a MySQL connection is
//! a stateful session: `LAST_INSERT_ID()` reads the id produced by the last
//! INSERT *on this connection*, `FOUND_ROWS()` mirrors the previous result
//! size, `SET NAMES` fixes the charset, `SET sql_mode` changes validation
//! strictness, and autocommit defaults to ON. None of this is global state —
//! the compat layer keeps one [`CmsSession`] per connection.

use std::collections::BTreeSet;

/// sql_mode flags that change CMS-visible validation semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqlModeFlag {
    /// STRICT_TRANS_TABLES / STRICT_ALL_TABLES — reject instead of coerce.
    Strict,
    /// NO_ZERO_DATE — `'0000-00-00'` becomes an error (legacy WP drafts!).
    NoZeroDate,
    /// NO_ZERO_IN_DATE — `'2026-13-00'` becomes an error.
    NoZeroInDate,
    /// ONLY_FULL_GROUP_BY — non-aggregated columns must appear in GROUP BY.
    OnlyFullGroupBy,
    /// ERROR_FOR_DIVISION_BY_ZERO.
    ErrorForDivisionByZero,
    /// NO_ENGINE_SUBSTITUTION.
    NoEngineSubstitution,
    /// PIPES_AS_CONCAT (`||` becomes string concat).
    PipesAsConcat,
    /// ANSI shorthand.
    Ansi,
    /// TRADITIONAL shorthand (implies strict + zero-date rejects).
    Traditional,
}

/// Current sql_mode: recognized flags plus any raw tokens we do not model
/// yet (kept verbatim so `SELECT @@sql_mode` round-trips).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlMode {
    pub flags: BTreeSet<SqlModeFlag>,
    pub raw: BTreeSet<String>,
}

impl SqlMode {
    /// Parses a `SET sql_mode = '…'` value. Empty string clears the mode
    /// (the "relaxed" mode many CMS installs run with).
    pub fn parse(value: &str) -> Self {
        let mut mode = SqlMode::default();
        for token in value.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let upper = token.to_ascii_uppercase();
            mode.raw.insert(upper.clone());
            match upper.as_str() {
                "STRICT_TRANS_TABLES" | "STRICT_ALL_TABLES" => {
                    mode.flags.insert(SqlModeFlag::Strict);
                }
                "NO_ZERO_DATE" => {
                    mode.flags.insert(SqlModeFlag::NoZeroDate);
                }
                "NO_ZERO_IN_DATE" => {
                    mode.flags.insert(SqlModeFlag::NoZeroInDate);
                }
                "ONLY_FULL_GROUP_BY" => {
                    mode.flags.insert(SqlModeFlag::OnlyFullGroupBy);
                }
                "ERROR_FOR_DIVISION_BY_ZERO" => {
                    mode.flags.insert(SqlModeFlag::ErrorForDivisionByZero);
                }
                "NO_ENGINE_SUBSTITUTION" => {
                    mode.flags.insert(SqlModeFlag::NoEngineSubstitution);
                }
                "PIPES_AS_CONCAT" => {
                    mode.flags.insert(SqlModeFlag::PipesAsConcat);
                }
                "ANSI" => {
                    mode.flags.insert(SqlModeFlag::Ansi);
                }
                "TRADITIONAL" => {
                    mode.flags.insert(SqlModeFlag::Traditional);
                }
                _ => {}
            }
        }
        mode
    }

    /// `TRADITIONAL` implies strict validation and zero-date rejection.
    pub fn is_strict(&self) -> bool {
        self.flags.contains(&SqlModeFlag::Strict) || self.flags.contains(&SqlModeFlag::Traditional)
    }

    pub fn forbids_zero_dates(&self) -> bool {
        self.flags.contains(&SqlModeFlag::NoZeroDate)
            || self.flags.contains(&SqlModeFlag::NoZeroInDate)
            || self.flags.contains(&SqlModeFlag::Traditional)
    }

    /// Renders back a stable canonical string for `SELECT @@sql_mode`.
    pub fn render(&self) -> String {
        let mut parts: Vec<String> = self.raw.iter().cloned().collect();
        parts.sort();
        parts.join(",")
    }
}

/// Connection character set from `SET NAMES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Charset {
    /// WordPress default since 4.2: `SET NAMES utf8mb4`.
    Utf8Mb4,
    /// Legacy WP installs (`utf8` = utf8mb3 in MySQL terms).
    Utf8Mb3,
    Latin1,
    /// Any other charset name we accept verbatim for now.
    Other(String),
}

impl Charset {
    pub fn parse(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "utf8mb4" => Charset::Utf8Mb4,
            "utf8" | "utf8mb3" => Charset::Utf8Mb3,
            "latin1" | "cp1251-alias" | "cp1252" => Charset::Latin1,
            other => Charset::Other(other.to_string()),
        }
    }
}

/// Per-connection MySQL-compatible state.
#[derive(Debug, Clone)]
pub struct CmsSession {
    /// Opaque session id (also the key for named advisory locks).
    pub id: u64,
    /// Currently selected database (`USE …` / `COM_INIT_DB`).
    pub database: Option<String>,
    /// MySQL autocommit defaults to ON.
    pub autocommit: bool,
    /// `LAST_INSERT_ID()` — id of the first auto-generated row of the last
    /// INSERT on this session (survives until the next INSERT).
    pub last_insert_id: u64,
    /// `ROW_COUNT()` — affected rows of the previous DML.
    pub affected_rows: u64,
    /// `FOUND_ROWS()` — row count of the previous SELECT.
    pub found_rows: u64,
    /// sql_mode, see [`SqlMode`].
    pub sql_mode: SqlMode,
    /// Connection charset, see [`Charset`].
    pub charset: Charset,
}

impl CmsSession {
    pub fn new(id: u64) -> Self {
        CmsSession {
            id,
            database: None,
            autocommit: true,
            last_insert_id: 0,
            affected_rows: 0,
            found_rows: 0,
            sql_mode: SqlMode::default(),
            // WordPress sets utf8mb4 explicitly, but MySQL 8 defaults to
            // utf8mb4 too — a safe CMS-edition default.
            charset: Charset::Utf8Mb4,
        }
    }

    /// `SET NAMES <charset>` — on MySQL this also implies collation reset.
    pub fn set_names(&mut self, name: &str) {
        self.charset = Charset::parse(name);
    }

    /// `SET sql_mode = '…'`.
    pub fn set_sql_mode(&mut self, value: &str) {
        self.sql_mode = SqlMode::parse(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_mysql_session() {
        let s = CmsSession::new(1);
        assert!(s.autocommit, "autocommit must default to ON");
        assert_eq!(s.last_insert_id, 0);
        assert_eq!(s.charset, Charset::Utf8Mb4);
    }

    #[test]
    fn set_names_roundtrip() {
        let mut s = CmsSession::new(1);
        s.set_names("utf8mb4");
        assert_eq!(s.charset, Charset::Utf8Mb4);
        s.set_names("utf8");
        assert_eq!(s.charset, Charset::Utf8Mb3);
        s.set_names("Latin1");
        assert_eq!(s.charset, Charset::Latin1);
    }

    #[test]
    fn sql_mode_parse_and_helpers() {
        let mut s = CmsSession::new(1);
        s.set_sql_mode("STRICT_TRANS_TABLES,NO_ZERO_DATE,NO_ENGINE_SUBSTITUTION");
        assert!(s.sql_mode.is_strict());
        assert!(s.sql_mode.forbids_zero_dates());
        assert!(s.sql_mode.render().contains("NO_ZERO_DATE"));

        // Empty sql_mode = relaxed mode (zero-dates allowed).
        s.set_sql_mode("");
        assert!(!s.sql_mode.is_strict());
        assert!(!s.sql_mode.forbids_zero_dates());
        assert_eq!(s.sql_mode.render(), "");

        // TRADITIONAL implies both.
        s.set_sql_mode("TRADITIONAL");
        assert!(s.sql_mode.is_strict());
        assert!(s.sql_mode.forbids_zero_dates());
    }
}
