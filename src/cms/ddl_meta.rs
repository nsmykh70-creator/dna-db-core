// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL CREATE TABLE DDL parser for the CMS catalog overlay.
//!
//! dbDelta (wp-admin/includes/upgrade.php) compares `SHOW FULL COLUMNS`
//! output against the plugin's CREATE TABLE **textually**: the `Type` string
//! must come back exactly as written in the DDL ("bigint(20) unsigned",
//! "varchar(191)", "longtext") or dbDelta will emit ALTER TABLE on every
//! run. The engine schema (relational::ColumnType) is far coarser than that,
//! so the CMS edition keeps a per-table overlay built by parsing the
//! original DDL once at CREATE time:
//!
//!   MySQL DDL ──parse──▶ CmsTableMeta ─┬─▶ normalized engine DDL (CREATE TABLE ...)
//!                                      ├─▶ SHOW FULL COLUMNS rows (dbDelta-exact)
//!                                      └─▶ SHOW INDEX rows
//!
//! Supported subset (WordPress core + WooCommerce DDL shapes): column types
//! with length/precision, `unsigned`, `NOT NULL`/`NULL`, `DEFAULT <literal>`,
//! `auto_increment`, `PRIMARY KEY (a, b)`, `UNIQUE KEY name (col(len))`,
//! `KEY name (col(len))`, backticked identifiers, table options after the
//! closing paren (ENGINE/CHARSET/COLLATE — ignored). `FOREIGN KEY` lines are
//! skipped in the overlay (engine-level FKs are declared separately).
//!
//! Known deviations (documented in CMS_EDITION_RU.md §4): the engine has a
//! single-column PK, so for composite PRIMARY KEYs the overlay keeps the
//! full key for SHOW output while the engine table keys on the first column.

use crate::relational::{ColumnType, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct CmsColumnMeta {
    pub field: String,
    /// Raw MySQL type text as written in the DDL (lowercased), e.g.
    /// "bigint(20) unsigned", "varchar(191)", "decimal(10,2)".
    pub mysql_type: String,
    pub engine_type: ColumnType,
    pub not_null: bool,
    /// DDL default as written (unquoted), `None` = no DEFAULT clause.
    pub default: Option<String>,
    pub auto_increment: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CmsIndexMeta {
    pub name: String,
    pub unique: bool,
    /// (column, optional prefix length) in index order.
    pub columns: Vec<(String, Option<u16>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CmsTableMeta {
    pub table: String,
    pub columns: Vec<CmsColumnMeta>,
    /// Full primary key (possibly composite; engine keys on the first col).
    pub primary_key: Vec<String>,
    pub indexes: Vec<CmsIndexMeta>,
    pub auto_increment_column: Option<String>,
}

/// Split a string into "top-level" comma segments, honouring parentheses
/// depth and single-quoted strings (with backslash and '' escapes).
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            cur.push(c);
            match c {
                '\\' => {
                    if let Some(&n) = chars.peek() {
                        cur.push(n);
                        chars.next();
                    }
                }
                '\'' => {
                    if chars.peek() == Some(&'\'') {
                        cur.push('\'');
                        chars.next();
                    } else {
                        in_quote = false;
                    }
                }
                _ => {}
            }
            continue;
        }
        match c {
            '\'' => {
                in_quote = true;
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

fn strip_backticks(s: &str) -> String {
    s.trim().trim_matches('`').trim().to_string()
}

/// MySQL type text → engine ColumnType.
fn engine_type_of(mysql_type: &str) -> ColumnType {
    let base = mysql_type.split('(').next().unwrap_or(mysql_type).trim().to_ascii_lowercase();
    match base.as_str() {
        "bigint" | "int" | "integer" | "mediumint" | "smallint" | "tinyint" | "bit" => ColumnType::Int64,
        "float" | "double" | "real" | "decimal" | "numeric" => ColumnType::Float64,
        "datetime" | "timestamp" | "date" | "time" => ColumnType::Timestamp,
        // Everything text-ish (varchar/char/tinytext/mediumtext/longtext/text)
        // maps to the engine TEXT.
        _ => ColumnType::Text,
    }
}

/// Extract `(args)` after a keyword: returns the trimmed inner text and
/// strips the consumed parens from `rest`.
fn take_paren_group(rest: &str) -> Option<String> {
    let start = rest.find('(')?;
    let mut depth = 0usize;
    let mut in_quote = false;
    for (i, c) in rest.char_indices().skip(start) {
        if in_quote {
            match c {
                '\\' => {}
                '\'' => in_quote = false,
                _ => {}
            }
            continue;
        }
        match c {
            '\'' => in_quote = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[start + 1..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse column prefix length like `col(191)` → (col, Some(191)).
fn parse_key_column(token: &str) -> (String, Option<u16>) {
    let token = token.trim();
    if let Some(open) = token.find('(') {
        let name = token[..open].trim().trim_matches('`').to_string();
        let len = token[open + 1..]
            .trim_end_matches(')')
            .trim()
            .parse::<u16>()
            .ok();
        (name, len)
    } else {
        (strip_backticks(token), None)
    }
}

/// Unquote a DEFAULT literal: 'it''s' → it's, 0 → 0, NULL → None.
fn unquote_default(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("NULL") {
        return None;
    }
    if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2 {
        let inner = &raw[1..raw.len() - 1];
        return Some(inner.replace("''", "'").replace("\\'", "'").replace("\\\\", "\\"));
    }
    Some(raw.to_string())
}

impl CmsTableMeta {
    /// Parses the WordPress/WooCommerce CREATE TABLE subset. Table options
    /// after the closing paren are ignored.
    pub fn parse_create_table(ddl: &str) -> Result<CmsTableMeta, String> {
        let ddl = ddl.trim().trim_end_matches(';');
        let upper = ddl.to_ascii_uppercase();
        let body_start = ddl
            .find('(')
            .ok_or_else(|| "DDL: no column list found".to_string())?;
        // Table name: between CREATE TABLE [IF NOT EXISTS] and the '('.
        let head = &ddl[..body_start];
        let head_lower = head.to_ascii_lowercase();
        let pos = head_lower
            .find("table")
            .ok_or_else(|| "DDL: not a CREATE TABLE statement".to_string())?;
        let mut name_part = head[pos + "table".len()..].trim();
        if name_part.to_ascii_lowercase().starts_with("if not exists") {
            name_part = name_part["if not exists".len()..].trim();
        }
        let table = strip_backticks(name_part);
        if table.is_empty() {
            return Err("DDL: empty table name".to_string());
        }
        let _ = upper; // (kept for future keyword dispatch)

        // Body: balanced parens from body_start.
        let mut depth = 0usize;
        let mut in_quote = false;
        let mut body_end = ddl.len();
        let bytes: Vec<char> = ddl.chars().collect();
        let mut byte_idx = 0usize;
        for (ci, c) in bytes.iter().enumerate() {
            byte_idx = ci;
            if ci < body_start {
                continue;
            }
            if in_quote {
                match c {
                    '\\' => {}
                    '\'' => in_quote = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '\'' => in_quote = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = ci;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &ddl[body_start + 1..body_end];

        let mut meta = CmsTableMeta {
            table,
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            auto_increment_column: None,
        };

        for seg in split_top_level(body) {
            let first_word = seg
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            match first_word.as_str() {
                "PRIMARY" => {
                    let group = take_paren_group(&seg)
                        .ok_or_else(|| format!("DDL {}: PRIMARY KEY without (cols)", meta.table))?;
                    meta.primary_key = group
                        .split(',')
                        .map(|c| strip_backticks(&parse_key_column(c).0))
                        .collect();
                }
                "UNIQUE" => {
                    // UNIQUE KEY name (cols) | UNIQUE name (cols) | UNIQUE (cols)
                    let after = seg["UNIQUE".len()..].trim();
                    let after = after
                        .strip_prefix("KEY")
                        .or_else(|| after.strip_prefix("INDEX"))
                        .unwrap_or(after)
                        .trim();
                    let group = take_paren_group(after)
                        .ok_or_else(|| format!("DDL {}: UNIQUE KEY without (cols)", meta.table))?;
                    let head = after[..after.find('(').unwrap_or(after.len())].trim();
                    let name = if head.is_empty() {
                        // MySQL would name it PRIMARY-ish; use first column.
                        parse_key_column(group.split(',').next().unwrap_or("")).0
                    } else {
                        strip_backticks(head.split_whitespace().next().unwrap_or("idx"))
                    };
                    let cols = group
                        .split(',')
                        .map(parse_key_column)
                        .collect::<Vec<_>>();
                    meta.indexes.push(CmsIndexMeta { name, unique: true, columns: cols });
                }
                "KEY" | "INDEX" | "SPATIAL" | "FULLTEXT" => {
                    let rest = seg.splitn(2, ' ').nth(1).unwrap_or("").trim();
                    let group = take_paren_group(rest)
                        .ok_or_else(|| format!("DDL {}: KEY without (cols)", meta.table))?;
                    let head = rest[..rest.find('(').unwrap_or(rest.len())].trim();
                    let name = strip_backticks(head.split_whitespace().next().unwrap_or("idx"));
                    let cols = group.split(',').map(parse_key_column).collect::<Vec<_>>();
                    meta.indexes.push(CmsIndexMeta { name, unique: false, columns: cols });
                }
                "FOREIGN" | "CONSTRAINT" | "CHECK" => {
                    // Skipped in the overlay (engine FKs declared separately).
                }
                _ => {
                    // Column definition.
                    let mut tokens = seg.split_whitespace();
                    let field = strip_backticks(tokens.next().unwrap_or(""));
                    if field.is_empty() {
                        return Err(format!("DDL {}: empty column name", meta.table));
                    }
                    let after_name = seg[seg.find(&field).unwrap_or(0) + field.len()..].trim();
                    // Type = identifier + optional (n[,m]).
                    let mut type_text = String::new();
                    let mut rest = after_name;
                    let type_word: String = after_name
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if type_word.is_empty() {
                        return Err(format!("DDL {}: column '{}' has no type", meta.table, field));
                    }
                    type_text.push_str(&type_word.to_ascii_lowercase());
                    rest = rest[type_word.len()..].trim_start();
                    if let Some(group) = take_paren_group(rest) {
                        type_text.push_str(&format!("({})", group.replace(' ', "")));
                        // Close of the FIRST paren group (skipping its own
                        // ')'): rfind would jump to a ')' inside a later
                        // quoted DEFAULT value.
                        let open = rest.find('(').unwrap_or(0);
                        let close = open + 1 + group.len() + 1;
                        rest = rest[close..].trim_start();
                    }
                    let mut col = CmsColumnMeta {
                        field,
                        mysql_type: type_text,
                        engine_type: ColumnType::Text, // refined below
                        not_null: false,
                        default: None,
                        auto_increment: false,
                    };
                    // Flags loop.
                    loop {
                        let upper_rest = rest.to_ascii_uppercase();
                        if upper_rest.starts_with("UNSIGNED") {
                            col.mysql_type.push_str(" unsigned");
                            rest = rest["UNSIGNED".len()..].trim_start();
                        } else if upper_rest.starts_with("NOT NULL") {
                            col.not_null = true;
                            rest = rest["NOT NULL".len()..].trim_start();
                        } else if upper_rest.starts_with("NULL") {
                            rest = rest["NULL".len()..].trim_start();
                        } else if upper_rest.starts_with("DEFAULT") {
                            rest = rest["DEFAULT".len()..].trim_start();
                            let (raw, consumed) = take_default_literal(rest)?;
                            col.default = unquote_default(&raw);
                            rest = rest[consumed..].trim_start();
                        } else if upper_rest.starts_with("AUTO_INCREMENT") {
                            col.auto_increment = true;
                            rest = rest["AUTO_INCREMENT".len()..].trim_start();
                        } else if upper_rest.starts_with("UNIQUE") {
                            // Inline UNIQUE — model as a unique index.
                            rest = rest["UNIQUE".len()..].trim_start();
                            let iname = format!("{}_{}_uniq", meta.table, col.field);
                            meta.indexes.push(CmsIndexMeta {
                                name: iname,
                                unique: true,
                                columns: vec![(col.field.clone(), None)],
                            });
                        } else if upper_rest.starts_with("CHARACTER SET")
                            || upper_rest.starts_with("COLLATE")
                            || upper_rest.starts_with("COMMENT")
                            || upper_rest.starts_with("REFERENCES")
                        {
                            // consume to end — none of these appear before
                            // another flag in the supported subset.
                            rest = "";
                        } else {
                            break;
                        }
                    }
                    if col.auto_increment {
                        meta.auto_increment_column = Some(col.field.clone());
                    }
                    col.engine_type = engine_type_of(&col.mysql_type);
                    meta.columns.push(col);
                }
            }
        }

        if meta.columns.is_empty() {
            return Err(format!("DDL {}: no columns parsed", meta.table));
        }
        // MySQL implicitly PKs the auto_increment column when only it is marked.
        if meta.primary_key.is_empty() {
            if let Some(ac) = &meta.auto_increment_column {
                meta.primary_key.push(ac.clone());
            }
        }
        Ok(meta)
    }

    /// SHOW FULL COLUMNS rows, dbDelta-exact: Field, Type (raw DDL text),
    /// Collation, Null, Key, Default, Extra, Privileges, Comment.
    pub fn show_full_columns_rows(&self) -> Vec<BTreeMap<String, Value>> {
        let mut rows = Vec::new();
        for col in &self.columns {
            let is_text = matches!(col.engine_type, ColumnType::Text);
            let key = if self.primary_key.iter().any(|c| c == &col.field) {
                "PRI"
            } else {
                let uniq = self
                    .indexes
                    .iter()
                    .filter(|i| i.unique)
                    .any(|i| i.columns.first().map(|(c, _)| c) == Some(&col.field));
                let plain = self
                    .indexes
                    .iter()
                    .filter(|i| !i.unique)
                    .any(|i| i.columns.first().map(|(c, _)| c) == Some(&col.field));
                if uniq {
                    "UNI"
                } else if plain {
                    "MUL"
                } else {
                    ""
                }
            };
            let mut r = BTreeMap::new();
            r.insert("Field".into(), Value::Text(col.field.clone()));
            r.insert("Type".into(), Value::Text(col.mysql_type.clone()));
            r.insert(
                "Collation".into(),
                if is_text {
                    Value::Text("utf8mb4_unicode_520_ci".into())
                } else {
                    Value::Null
                },
            );
            r.insert("Null".into(), Value::Text(if col.not_null { "NO" } else { "YES" }.into()));
            r.insert("Key".into(), Value::Text(key.into()));
            r.insert(
                "Default".into(),
                match &col.default {
                    Some(d) => Value::Text(d.clone()),
                    None => Value::Null,
                },
            );
            r.insert(
                "Extra".into(),
                Value::Text(if col.auto_increment { "auto_increment" } else { "" }.into()),
            );
            r.insert(
                "Privileges".into(),
                Value::Text("select,insert,update,references".into()),
            );
            r.insert("Comment".into(), Value::Text(String::new()));
            rows.push(r);
        }
        rows
    }

    /// SHOW INDEX rows (subset MySQL reports; dbDelta matches Key_name,
    /// Non_unique, Seq_in_index, Column_name).
    pub fn show_index_rows(&self) -> Vec<BTreeMap<String, Value>> {
        let mut rows = Vec::new();
        let mut push_rows = |name: &str, unique: bool, cols: &[(String, Option<u16>)]| {
            for (seq, (col, sub_part)) in cols.iter().enumerate() {
                let mut r = BTreeMap::new();
                r.insert("Table".into(), Value::Text(self.table.clone()));
                r.insert("Non_unique".into(), Value::Int(if unique { 0 } else { 1 }));
                r.insert("Key_name".into(), Value::Text(name.to_string()));
                r.insert("Seq_in_index".into(), Value::Int(seq as i64 + 1));
                r.insert("Column_name".into(), Value::Text(col.clone()));
                r.insert("Collation".into(), Value::Text("A".into()));
                r.insert("Cardinality".into(), Value::Int(0));
                r.insert(
                    "Sub_part".into(),
                    match sub_part {
                        Some(n) => Value::Int(*n as i64),
                        None => Value::Null,
                    },
                );
                r.insert("Packed".into(), Value::Null);
                r.insert("Null".into(), Value::Text(String::new()));
                r.insert("Index_type".into(), Value::Text("BTREE".into()));
                r.insert("Comment".into(), Value::Text(String::new()));
                // MariaDB reports these two on every index row (the last
                // column is Ignored in the MariaDB shape, Is_visible in
                // MySQL 8); dbDelta and Site Health read the full shape.
                r.insert("Index_comment".into(), Value::Text(String::new()));
                r.insert("Ignored".into(), Value::Text("NO".into()));
                rows.push(r);
            }
        };
        if !self.primary_key.is_empty() {
            let cols = self.primary_key.iter().map(|c| (c.clone(), None)).collect::<Vec<_>>();
            push_rows("PRIMARY", true, &cols);
        }
        for idx in &self.indexes {
            push_rows(&idx.name, idx.unique, &idx.columns);
        }
        rows
    }

    /// Normalized engine-dialect DDL: base types, single PK (first column of
    /// a composite), AUTO_INCREMENT kept (engine records the sequence),
    /// lengths/unsigned/defaults/engine-options dropped.
    pub fn normalized_engine_ddl(&self) -> String {
        let mut cols = Vec::new();
        for col in &self.columns {
            let ty = match col.engine_type {
                ColumnType::Int64 => "INT64",
                ColumnType::Float64 => "FLOAT64",
                ColumnType::Timestamp => "TIMESTAMP",
                ColumnType::Text => "TEXT",
                ColumnType::Dna2Bit => "DNA",
                ColumnType::ForeignLink { .. } => "INT64",
            };
            let mut def = format!("{} {}", col.field, ty);
            if col.not_null {
                def.push_str(" NOT NULL");
            }
            if col.auto_increment {
                def.push_str(" AUTO_INCREMENT");
            }
            cols.push(def);
        }
        if let Some(pk) = self.primary_key.first() {
            cols.push(format!("PRIMARY KEY ({})", pk));
        }
        format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            self.table,
            cols.join(", ")
        )
    }
}

/// Consumes a DEFAULT literal: quoted string (with '' doubling) or a bare
/// token. Returns (raw, consumed chars).
fn take_default_literal(rest: &str) -> Result<(String, usize), String> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next().ok_or_else(|| "DDL: DEFAULT without value".to_string())?;
    if first == '\'' {
        let mut consumed = 1usize;
        let mut out = String::from("'");
        let mut it = rest[1..].char_indices().peekable();
        while let Some((i, c)) = it.next() {
            consumed += c.len_utf8();
            match c {
                '\\' => {
                    if let Some((_, n)) = it.next() {
                        consumed += n.len_utf8();
                        out.push(c);
                        out.push(n);
                    }
                }
                '\'' => {
                    if it.peek() == Some(&(i + 1, '\'')) {
                        it.next();
                        consumed += 1;
                        out.push_str("''");
                    } else {
                        out.push('\'');
                        return Ok((out, consumed));
                    }
                }
                _ => out.push(c),
            }
        }
        Err("DDL: unterminated DEFAULT string".into())
    } else {
        let end = rest
            .find(char::is_whitespace)
            .unwrap_or(rest.len());
        Ok((rest[..end].to_string(), end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WP_OPTIONS_DDL: &str = "CREATE TABLE wp_options (
        option_id bigint(20) unsigned NOT NULL auto_increment,
        option_name varchar(191) NOT NULL default '',
        option_value longtext NOT NULL,
        autoload varchar(20) NOT NULL default 'yes',
        PRIMARY KEY  (option_id),
        UNIQUE KEY option_name (option_name)
    ) ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_520_ci";

    #[test]
    fn parses_wp_options() {
        let m = CmsTableMeta::parse_create_table(WP_OPTIONS_DDL).unwrap();
        assert_eq!(m.table, "wp_options");
        assert_eq!(m.columns.len(), 4);
        assert_eq!(m.primary_key, vec!["option_id"]);
        assert_eq!(m.auto_increment_column.as_deref(), Some("option_id"));

        let oid = &m.columns[0];
        assert_eq!(oid.field, "option_id");
        assert_eq!(oid.mysql_type, "bigint(20) unsigned");
        assert_eq!(oid.engine_type, ColumnType::Int64);
        assert!(oid.auto_increment);

        let oname = &m.columns[1];
        assert_eq!(oname.mysql_type, "varchar(191)");
        assert_eq!(oname.default, Some("".into()));

        let oval = &m.columns[2];
        assert_eq!(oval.mysql_type, "longtext");
        assert_eq!(oval.engine_type, ColumnType::Text);

        // UNIQUE KEY option_name.
        assert_eq!(m.indexes.len(), 1);
        assert!(m.indexes[0].unique);
        assert_eq!(m.indexes[0].columns[0].0, "option_name");
    }

    #[test]
    fn show_full_columns_is_dbdelta_exact() {
        let m = CmsTableMeta::parse_create_table(WP_OPTIONS_DDL).unwrap();
        let rows = m.show_full_columns_rows();
        let field = |i: usize, k: &str| -> String {
            rows[i].get(k).map(|v| v.to_string()).unwrap_or_default()
        };
        // option_id.
        assert_eq!(field(0, "Field"), "option_id");
        assert_eq!(field(0, "Type"), "bigint(20) unsigned"); // raw DDL text!
        assert_eq!(field(0, "Null"), "NO");
        assert_eq!(field(0, "Key"), "PRI");
        assert!(rows[0].get("Default").unwrap().is_null());
        assert_eq!(field(0, "Extra"), "auto_increment");
        // option_name.
        assert_eq!(field(1, "Type"), "varchar(191)");
        assert_eq!(field(1, "Key"), "UNI");
        assert_eq!(field(1, "Default"), ""); // default '' — empty string, not NULL
        assert_eq!(field(1, "Extra"), "");
    }

    #[test]
    fn show_index_rows_match_mysql_shape() {
        let m = CmsTableMeta::parse_create_table(WP_OPTIONS_DDL).unwrap();
        let rows = m.show_index_rows();
        let get = |i: usize, k: &str| rows[i].get(k).map(|v| v.to_string()).unwrap_or_default();
        // Row 0: PRIMARY (option_id); row 1: option_name unique.
        assert_eq!(get(0, "Key_name"), "PRIMARY");
        assert_eq!(get(0, "Seq_in_index"), "1");
        assert_eq!(get(0, "Column_name"), "option_id");
        assert_eq!(get(0, "Non_unique"), "0");
        assert_eq!(get(1, "Key_name"), "option_name");
        assert_eq!(get(1, "Non_unique"), "0");
        assert_eq!(get(1, "Index_type"), "BTREE");
    }

    #[test]
    fn composite_primary_key_wide_table() {
        let ddl = "CREATE TABLE wp_term_relationships (
            object_id bigint(20) unsigned NOT NULL default 0,
            term_taxonomy_id bigint(20) unsigned NOT NULL default 0,
            term_order int(11) NOT NULL default 0,
            PRIMARY KEY  (object_id, term_taxonomy_id),
            KEY term_taxonomy_id (term_taxonomy_id)
        ) ENGINE=InnoDB";
        let m = CmsTableMeta::parse_create_table(ddl).unwrap();
        assert_eq!(m.primary_key, vec!["object_id", "term_taxonomy_id"]);
        // Engine keys on the first column (documented deviation).
        assert!(m.normalized_engine_ddl().contains("PRIMARY KEY (object_id)"));
        // SHOW INDEX keeps the composite truth: PRIMARY seq 1..2.
        let rows = m.show_index_rows();
        assert_eq!(rows[0].get("Key_name").unwrap(), &Value::Text("PRIMARY".into()));
        assert_eq!(rows[0].get("Seq_in_index").unwrap(), &Value::Int(1));
        assert_eq!(rows[1].get("Key_name").unwrap(), &Value::Text("PRIMARY".into()));
        assert_eq!(rows[1].get("Seq_in_index").unwrap(), &Value::Int(2));
        assert_eq!(rows[1].get("Column_name").unwrap(), &Value::Text("term_taxonomy_id".into()));
        // Both PK columns report Key=PRI.
        let cols = m.show_full_columns_rows();
        assert_eq!(cols[0].get("Key").unwrap(), &Value::Text("PRI".into()));
        assert_eq!(cols[1].get("Key").unwrap(), &Value::Text("PRI".into()));
    }

    #[test]
    fn defaults_with_quotes_and_parens() {
        let ddl = "CREATE TABLE t (
            id bigint(20) unsigned NOT NULL auto_increment,
            a varchar(50) NOT NULL default 'it''s (ok)',
            b decimal(10,2) NOT NULL default 0.00,
            c datetime NOT NULL default '0000-00-00 00:00:00',
            PRIMARY KEY  (id)
        )";
        let m = CmsTableMeta::parse_create_table(ddl).unwrap();
        assert_eq!(m.columns[1].default.as_deref(), Some("it's (ok)"));
        assert_eq!(m.columns[2].mysql_type, "decimal(10,2)");
        assert_eq!(m.columns[2].engine_type, ColumnType::Float64);
        assert_eq!(m.columns[3].engine_type, ColumnType::Timestamp);
        assert_eq!(m.columns[3].default.as_deref(), Some("0000-00-00 00:00:00"));
    }

    #[test]
    fn normalized_engine_ddl_shape() {
        let m = CmsTableMeta::parse_create_table(WP_OPTIONS_DDL).unwrap();
        let ddl = m.normalized_engine_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS wp_options ("));
        assert!(ddl.contains("option_id INT64 NOT NULL AUTO_INCREMENT"));
        assert!(ddl.contains("option_name TEXT NOT NULL"));
        assert!(ddl.contains("PRIMARY KEY (option_id)"));
        assert!(!ddl.contains("ENGINE="));
    }
}
