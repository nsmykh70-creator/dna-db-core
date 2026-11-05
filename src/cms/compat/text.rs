// SPDX-License-Identifier: AGPL-3.0-only
//! Small shared helpers: reply builders, SQL text scanning (top-level
//! keyword search, LIKE pattern extraction), MySQL string decoding and
//! type-name fallbacks.

use std::collections::BTreeMap;

use super::*;
use crate::relational::{ColumnType, Value};

// ---------------- free helpers ----------------

pub(crate) fn empty_ok(message: impl Into<String>) -> CmsQueryResult {
    CmsQueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: Some(message.into()),
        plan: "cms: session statement".into(),
        affected_rows: 0,
        last_insert_id: 0,
        found_rows: 0,
        is_select: false,
        decimal_scales: Vec::new(),
    }
}

pub(crate) fn one_value_reply(column: &str, value: Value) -> CmsQueryResult {
    let mut r = BTreeMap::new();
    r.insert(column.to_string(), value);
    CmsQueryResult {
        columns: vec![column.to_string()],
        rows: vec![r],
        message: None,
        plan: "cms: function shim".into(),
        affected_rows: 0,
        last_insert_id: 0,
        found_rows: 1,
        is_select: true,
        decimal_scales: Vec::new(),
    }
}

/// Column name for a single-value shim reply. Real MySQL names the result
/// column after the expression (`SELECT VERSION()` → `VERSION()`) or after
/// the client-provided alias (`SELECT VERSION() AS v` → `v`). Shims used to
/// always report the expression, which made aliased metadata probes return
/// the wrong column name.
pub(crate) fn shim_column(sql: &str, expr: &str) -> String {
    // ASCII uppercase keeps byte offsets aligned with `sql`.
    let up = sql.to_ascii_uppercase();
    if let Some(epos) = up.find(expr) {
        let rest = &up[epos + expr.len()..];
        if let Some(apos) = rest.find(" AS ") {
            let tail = sql[epos + expr.len() + apos + 4..].trim();
            let alias = tail
                .split(|c: char| c.is_whitespace() || c == ',')
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '`' || c == '\'' || c == '"');
            if !alias.is_empty() {
                return alias.to_string();
            }
        }
    }
    expr.to_string()
}

/// Column name for a single-function shim reply, MySQL-style: the unaliased
/// form is the FULL expression text (`SELECT GET_LOCK('x',0)` names the
/// column `GET_LOCK('x',0)`), an `AS alias` tail renames it. The expression
/// is sliced with quote- and paren-aware balancing so calls with arguments
/// (lock functions) are captured exactly as the client wrote them.
pub(crate) fn fn_column(sql: &str, fname: &str) -> String {
    let up = sql.to_ascii_uppercase();
    if let Some(epos) = up.find(fname) {
        if let Some(open_rel) = up[epos..].find('(') {
            let open = epos + open_rel;
            let mut depth = 0usize;
            let mut in_q: Option<char> = None;
            for (i, ch) in sql[open..].char_indices() {
                match in_q {
                    Some(q) => {
                        if ch == q {
                            in_q = None;
                        }
                    }
                    None => match ch {
                        '\'' => in_q = Some('\''),
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                let end = open + i + 1;
                                let rest_up = &up[end..];
                                if let Some(a) = rest_up.find(" AS ") {
                                    let tail = sql[end + a + 4..].trim();
                                    let alias = tail
                                        .split(|c: char| c.is_whitespace() || c == ',')
                                        .next()
                                        .unwrap_or("")
                                        .trim_matches(|c| c == '`' || c == '\'' || c == '"');
                                    if !alias.is_empty() {
                                        return alias.to_string();
                                    }
                                }
                                return sql[epos..end].to_string();
                            }
                        }
                        _ => {}
                    },
                }
            }
        }
    }
    fname.to_string()
}

/// Value after '=' (for SET sql_mode = '…', autocommit=0).
pub(crate) fn after_eq(s: &str) -> String {
    match s.find('=') {
        Some(pos) => s[pos + 1..].trim().to_string(),
        None => String::new(),
    }
}

/// Case-insensitive substring search.
pub(crate) fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.find(&n)
}

/// Finds `needle` (a keyword) as a *top-level* token in `hay`: outside
/// string literals, backtick identifiers, parentheses and SQL comments.
/// Locking-tail and table extraction must not trip over
/// `WHERE note = 'send for update'` or subqueries. Byte-index result.
pub(crate) fn top_level_find_ci(hay: &str, needle: &str) -> Option<usize> {
    let b = hay.as_bytes();
    let n = needle.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let (mut in_s, mut in_d, mut in_b) = (false, false, false);
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if in_s {
            match c {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'\'' => in_s = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_d {
            match c {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'"' => in_d = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_b {
            if c == b'`' {
                in_b = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_s = true,
            b'"' => in_d = true,
            b'`' => in_b = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            _ if depth == 0
                && is_word(c)
                && (i == 0 || !is_word(b[i - 1]))
                && i + n.len() <= b.len()
                && b[i..i + n.len()].eq_ignore_ascii_case(n)
                && (i + n.len() == b.len() || !is_word(b[i + n.len()])) =>
            {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// First identifier of a FROM/WHERE-head clause: `wp_options AS o`,
/// `` `wp_terms` t ``, `d.wp_options`. `None` for derived tables (`(…)`).
pub(crate) fn first_table_ident(clause: &str) -> Option<String> {
    let clause = clause.trim_start();
    if clause.starts_with('(') {
        return None; // subquery / derived table — no single row identity
    }
    let b: Vec<char> = clause.chars().collect();
    let mut i = 0usize;
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    if i >= b.len() {
        return None;
    }
    if b[i] == '`' {
        i += 1;
        let start = i;
        while i < b.len() && b[i] != '`' {
            i += 1;
        }
        return Some(b[start..i].iter().collect());
    }
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_' || b[i] == '.') {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some(b[start..i].iter().collect())
}

/// Canonical string form of a row key (the PK value as stored).
pub(crate) fn value_key(v: &Value) -> Option<String> {
    match v {
        Value::Int(i) => Some(i.to_string()),
        Value::Float(x) => Some(x.to_string()),
        Value::Text(s) | Value::Dna(s) => Some(s.clone()),
        Value::Null => None,
    }
}

/// Extracts the argument list of `FN(...)` in `sql` (first occurrence).
pub(crate) fn fn_args(sql: &str, fname: &str) -> Option<String> {
    let pos = find_ci(sql, fname)?;
    let rest = &sql[pos + fname.len()..];
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let end = rest.find(')')?;
    Some(rest[1..end].to_string())
}

/// Parses GET_LOCK args: 'name' [, timeout]. Defaults: name='', timeout=0.
pub(crate) fn lock_args(args: &str) -> (String, u64) {
    let mut parts = args.split(',');
    let name = parts
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .to_string();
    let timeout = parts
        .next()
        .unwrap_or("0")
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    (name, timeout)
}

/// LIKE pattern from `LIKE '<pat>'`.
pub(crate) fn find_like_pattern(sql: &str) -> Option<String> {
    let pos = find_ci(sql, "LIKE")?;
    let rest = sql[pos + 4..].trim_start();
    if !rest.starts_with('\'') {
        return None;
    }
    let end = rest[1..].find('\'')? + 1;
    Some(rest[1..end].to_string())
}

/// MySQL LIKE with % (any run) and _ (one char), case-insensitive.
/// The matcher lives in `expr` so the WHERE evaluator shares it.
pub fn like_match(pattern: &str, text: &str) -> bool {
    crate::expr::like_match(pattern, text)
}

/// Table name after one of the SHOW … FROM <table> heads.
pub(crate) fn show_target(sql: &str, upper: &str, heads: &[&str]) -> Option<String> {
    for head in heads {
        if upper.starts_with(head) {
            let rest = sql[head.len()..].trim();
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// "N row(s) inserted/updated/deleted" → N; also the engine's DELETE form
/// "N deleted, M cascaded".
pub(crate) fn extract_row_count(message: &str) -> u64 {
    let m = message.to_ascii_lowercase();
    for marker in [" row(s)", " deleted"] {
        if let Some(pos) = m.find(marker) {
            let start = m[..pos]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            if let Ok(n) = start.parse::<u64>() {
                return n;
            }
        }
    }
    0
}

/// Decodes a MySQL string literal body (the text between the quotes):
/// backslash escapes per the MySQL manual, `\%` and `\_` stay two chars.
pub(crate) fn decode_mysql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let e = chars[i + 1];
            match e {
                '0' => out.push('\0'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{0008}'),
                'Z' => out.push('\u{001a}'),
                '\'' | '"' | '\\' => out.push(e),
                '%' | '_' => {
                    out.push('\\');
                    out.push(e);
                }
                other => out.push(other),
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Fallback Type rendering for tables without overlay metadata.
pub(crate) fn fallback_type(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Int64 => "bigint(20)".to_string(),
        ColumnType::Float64 => "double".to_string(),
        ColumnType::Timestamp => "datetime".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Dna2Bit => "text".to_string(),
        ColumnType::ForeignLink { .. } => "bigint(20)".to_string(),
    }
}
