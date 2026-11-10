// SPDX-License-Identifier: AGPL-3.0-only
//! DECIMAL(p,s) display scale — MySQL parity for fixed-point columns.
//!
//! MySQL transmits DECIMAL values in BOTH protocols as fixed-scale strings:
//! an inserted `250` into a `decimal(10,2)` column reads back as `"250.00"`
//! with column metadata `MYSQL_TYPE_NEWDECIMAL, decimals=2`. The engine
//! stores the numeric value as Float64, so the gate post-processes SELECT
//! output columns that resolve to a decimal-declared table column: values
//! are re-rendered with the declared scale and the per-column wire metadata
//! is carried in `CmsQueryResult::decimal_scales` (None = not a decimal
//! column — the wire layer falls back to value inference).
//!
//! Resolution rules (scope: plain CMS reads — WordPress meta/options,
//! WooCommerce HPOS totals, Drupal config values, MediaWiki counters):
//! - qualified output name `t.col` → column `col` of the FROM table aliased `t`;
//! - unqualified name → searched across the FROM tables; applied when every
//!   decimal-declared match agrees on the scale (ambiguous or non-decimal
//!   matches are left untouched);
//! - aggregates aliased to a decimal column name (`SUM(total) AS total`)
//!   keep the column scale, which matches MySQL's SUM over DECIMAL(p,s);
//! - unaliased aggregates (`SUM(price)`, `MIN(t.total)`, `MAX(t.total)`,
//!   `AVG(price)`) resolve through the expression text itself: SUM/MIN/MAX
//!   preserve the argument scale, AVG widens it by 4 (MySQL rule).
//!
//! Documented divergences: expressions without an overlay binding
//! (`value * 2`, `SUM(a + b)`) keep the DOUBLE rendering; float storage
//! rounds at read time (`format!` half-away-from-zero on the binary
//! representation) instead of MySQL's exact decimal rounding with notes.

use super::CmsQueryResult;
use crate::cms::ddl_meta::decimal_scale;
use crate::relational::Value;

/// Renders `x` with a fixed number of decimals, MySQL DECIMAL style.
pub(crate) fn fixed_decimal(x: f64, scale: u8) -> String {
    format!("{:.*}", scale as usize, x)
}

/// (alias, table) for every top-level FROM item (comma list + JOINs).
pub(crate) fn select_from_tables(sql: &str) -> Vec<(String, String)> {
    let Some(from_pos) = super::text::top_level_find_ci(sql, "FROM") else {
        return Vec::new();
    };
    let tail = &sql[from_pos + 4..];
    // The FROM list ends at the first top-level clause keyword. The locking
    // tail (`FOR UPDATE`) and aggregates (`GROUP BY`) must not leak into
    // table names; JOIN/ON fragments between items are stripped per item.
    let mut end = tail.len();
    for kw in [
        "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "PROCEDURE", "INTO", "FOR", "WINDOW",
    ] {
        if let Some(p) = super::text::top_level_find_ci(tail, kw) {
            end = end.min(p);
        }
    }
    let body = &tail[..end];
    // Split on top-level commas and the JOIN keyword; the pieces keep their
    // ON/USING tails, which parse_from_item skips.
    let mut items: Vec<String> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let b = body.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let (mut in_s, mut in_d, mut in_b) = (false, false, false);
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if in_s {
            match c {
                b'\\' => {
                    cur.push(c);
                    if i + 1 < b.len() {
                        cur.push(b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                b'\'' => in_s = false,
                _ => {}
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if in_d {
            match c {
                b'\\' => {
                    cur.push(c);
                    if i + 1 < b.len() {
                        cur.push(b[i + 1]);
                    }
                    i += 2;
                    continue;
                }
                b'"' => in_d = false,
                _ => {}
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if in_b {
            if c == b'`' {
                in_b = false;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_s = true;
                cur.push(c);
            }
            b'"' => {
                in_d = true;
                cur.push(c);
            }
            b'`' => {
                in_b = true;
                cur.push(c);
            }
            b'(' => {
                depth += 1;
                cur.push(c);
            }
            b')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            b',' if depth == 0 => {
                items.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
            }
            _ if depth == 0
                && is_word(c)
                && (i == 0 || !is_word(b[i - 1]))
                && i + 4 <= b.len()
                && b[i..i + 4].eq_ignore_ascii_case(b"JOIN")
                && (i + 4 == b.len() || !is_word(b[i + 4])) =>
            {
                items.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
                i += 4;
                continue;
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    if cur.iter().any(|c| !c.is_ascii_whitespace()) {
        items.push(String::from_utf8_lossy(&cur).into_owned());
    }
    items.iter().filter_map(|p| parse_from_item(p)).collect()
}

/// Words that may precede the table ident of a JOIN piece.
const JOIN_QUALIFIERS: [&str; 7] = [
    "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "NATURAL", "STRAIGHT_JOIN",
];

/// Words that can follow `<table>` when no alias is present.
const NON_ALIAS_WORDS: [&str; 18] = [
    "ON", "USING", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "CROSS", "NATURAL",
    "GROUP", "HAVING", "ORDER", "LIMIT", "FOR", "USE", "FORCE", "IGNORE",
];

/// Reads one identifier; backtick-quoted bodies may contain anything.
fn read_ident(s: &str, start: usize, allow_dot: bool) -> (String, usize) {
    let b = s.as_bytes();
    if b.get(start) == Some(&b'`') {
        let mut end = start + 1;
        while end < b.len() && b[end] != b'`' {
            end += 1;
        }
        let close = (end + 1).min(b.len());
        return (s[start + 1..end.min(b.len())].to_string(), close);
    }
    let mut end = start;
    while end < b.len()
        && (b[end].is_ascii_alphanumeric()
            || b[end] == b'_'
            || b[end] == b'$'
            || (allow_dot && b[end] == b'.'))
    {
        end += 1;
    }
    (s[start..end].to_string(), end)
}

fn skip_ws(s: &str, mut pos: usize) -> usize {
    let b = s.as_bytes();
    while pos < b.len() && (b[pos] as char).is_whitespace() {
        pos += 1;
    }
    pos
}

/// `<ident> [[AS] alias] [ON …|USING …]` → (alias, table). The db qualifier
/// (`db.tbl`) is dropped — the gate keys tables by their bare name.
fn parse_from_item(piece: &str) -> Option<(String, String)> {
    let mut pos = skip_ws(piece, 0);
    loop {
        let (word, end) = read_ident(piece, pos, false);
        if word.is_empty() {
            return None;
        }
        if JOIN_QUALIFIERS.iter().any(|k| word.eq_ignore_ascii_case(k)) {
            pos = skip_ws(piece, end);
            continue;
        }
        break;
    }
    let (raw, mut end) = read_ident(piece, pos, true);
    if raw.is_empty() {
        return None;
    }
    let table = raw
        .rsplit('.')
        .next()
        .unwrap_or(&raw)
        .trim_matches('`')
        .to_string();
    end = skip_ws(piece, end);
    let (word, wend) = read_ident(piece, end, false);
    let alias = if word.eq_ignore_ascii_case("AS") {
        let (alias, _) = read_ident(piece, skip_ws(piece, wend), false);
        alias
    } else if !word.is_empty()
        && !NON_ALIAS_WORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
    {
        word
    } else {
        table.clone()
    };
    Some((alias, table))
}

/// Scale of the output column `name`, or None when it cannot be uniquely
/// resolved to a decimal-declared table column.
fn resolve_scale(
    gate: &super::CmsSqlGate,
    tables: &[(String, String)],
    name: &str,
) -> Option<u8> {
    // Unaliased aggregates: the output column name IS the expression text
    // (`SUM(price)`, `max(t.total)`), and MySQL keeps the argument's scale
    // for SUM/MIN/MAX while AVG widens it by 4. Must run BEFORE the
    // qualified-name rule — `SUM(t.price)` contains a dot that would
    // otherwise be mistaken for a `qualifier.column` split.
    if let Some(s) = aggregate_inner_scale(gate, tables, name) {
        return Some(s);
    }
    if let Some((qual, col)) = name.rsplit_once('.') {
        let qual = qual.trim_matches('`');
        let col = col.trim_matches('`');
        if let Some((_, table)) = tables
            .iter()
            .find(|(al, _)| al.eq_ignore_ascii_case(qual))
        {
            return column_scale(gate, table, col);
        }
        // Qualified but unknown alias (`db.tbl.col` output naming) — fall
        // through to the unqualified rule on the last identifier.
        return unqualified_scale(gate, tables, col);
    }
    unqualified_scale(gate, tables, name)
}

/// MySQL scale rules for aggregates over a decimal column: SUM/MIN/MAX
/// preserve the argument scale, AVG widens it by 4
/// (`AVG(decimal(p,s)) → decimal(…, s+4)`). `SUM(DISTINCT col)` shares the
/// column's scale. Expression arguments (`SUM(a+b)`) resolve to the MAX
/// scale among the decimal columns referenced in the expression (MySQL:
/// result scale of an expression is the max of its decimal operands) —
/// unresolved or non-decimal-only expressions stay DOUBLE.
fn aggregate_inner_scale(
    gate: &super::CmsSqlGate,
    tables: &[(String, String)],
    name: &str,
) -> Option<u8> {
    for (word, delta) in [("SUM(", 0u8), ("MIN(", 0), ("MAX(", 0), ("AVG(", 4)] {
        let Some(pos) = super::text::find_ci(name, word) else {
            continue;
        };
        // Balanced close of the aggregate's argument (nested calls allowed):
        // the slice re-includes the opening `(` consumed by `word`.
        let Some(close) = balanced_paren_close(&name[pos + word.len() - 1..]) else {
            continue;
        };
        let rest = &name[pos + word.len()..];
        let inner = rest[..close - 1].trim();
        let inner = inner
            .strip_prefix("DISTINCT")
            .map(|r| r.trim())
            .unwrap_or(inner);
        // Plain-column fast path: `SUM(price)` / `SUM(t.price)`.
        if let Some(scale) = resolve_column_in_tables(gate, tables, inner) {
            return Some(scale.saturating_add(delta));
        }
        // Expression path: every identifier that is NOT a function call or a
        // keyword is a column reference. MySQL widens the result scale to
        // the max scale among decimal operands; references that resolve to
        // non-decimal columns don't contribute (int + decimal → decimal).
        let mut max_scale: Option<u8> = None;
        for (ident, is_function) in expression_identifiers(inner) {
            if is_function {
                continue;
            }
            if is_sql_keyword(&ident.to_ascii_uppercase()) {
                continue;
            }
            let (ident, qualifier) = match ident.rsplit_once('.') {
                Some((q, c)) => (c, Some(q)),
                None => (ident.as_str(), None),
            };
            let scale = match qualifier {
                Some(q) => {
                    let q = q.trim_matches('`');
                    match tables.iter().find(|(al, _)| al.eq_ignore_ascii_case(q)) {
                        Some((_, table)) => column_scale(gate, table, ident),
                        None => None,
                    }
                }
                None => unqualified_scale(gate, tables, ident),
            };
            if let Some(s) = scale {
                max_scale = Some(match max_scale {
                    None => s,
                    Some(prev) => prev.max(s),
                });
            }
        }
        return max_scale.map(|s| s.saturating_add(delta));
    }
    None
}

/// Split an expression into identifier tokens with a `is_function` flag
/// (identifier immediately followed by `(`). String literals, numbers and
/// operators are skipped; backticks are stripped.
fn expression_identifiers(expr: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\'' {
            // Skip string literal (backslash escapes, '' doubling).
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' || c == '`' {
            let start = i;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '`' {
                    i += 1;
                } else {
                    break;
                }
            }
            let token = expr[start..i].trim_matches('`').to_string();
            if !token.is_empty() {
                // Function call? (identifier directly followed by '(')
                let mut j = i;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                let is_function = j < bytes.len() && bytes[j] == b'(';
                out.push((token, is_function));
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Keywords that can appear inside aggregate arguments but are not columns.
fn is_sql_keyword(word: &str) -> bool {
    matches!(
        word,
        "NULL" | "TRUE" | "FALSE" | "AND" | "OR" | "NOT" | "CASE" | "WHEN" | "THEN" | "ELSE"
            | "END" | "DIV" | "MOD" | "IS" | "LIKE" | "BETWEEN" | "IN" | "FROM" | "DUAL"
    )
}

/// One column reference → scale: `t.col` resolved through the alias map,
/// bare `col` through unique resolution across the FROM tables.
fn resolve_column_in_tables(
    gate: &super::CmsSqlGate,
    tables: &[(String, String)],
    col: &str,
) -> Option<u8> {
    let col = col.trim();
    match col.rsplit_once('.') {
        Some((q, c)) => {
            let q = q.trim_matches('`');
            let c = c.trim_matches('`');
            match tables.iter().find(|(al, _)| al.eq_ignore_ascii_case(q)) {
                Some((_, table)) => column_scale(gate, table, c),
                None => None,
            }
        }
        None => unqualified_scale(gate, tables, col.trim_matches('`')),
    }
}

/// Byte index of the `)` closing the `(` at position 0 (string-literal aware).
fn balanced_paren_close(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn column_scale(gate: &super::CmsSqlGate, table: &str, col: &str) -> Option<u8> {
    let meta = gate.table_meta(table)?;
    let c = meta
        .columns
        .iter()
        .find(|c| c.field.eq_ignore_ascii_case(col))?;
    decimal_scale(&c.mysql_type)
}

fn unqualified_scale(
    gate: &super::CmsSqlGate,
    tables: &[(String, String)],
    name: &str,
) -> Option<u8> {
    let mut found: Option<u8> = None;
    for (_, table) in tables {
        let Some(s) = column_scale(gate, table, name) else {
            continue;
        };
        match found {
            None => found = Some(s),
            Some(prev) if prev != s => return None, // ambiguous across tables
            _ => {}
        }
    }
    found
}

/// Post-processes a gate SELECT result: re-renders Float/Int values of
/// decimal-resolved output columns with the declared scale and records the
/// per-column wire metadata.
pub(crate) fn apply_decimal_scale(gate: &super::CmsSqlGate, out: &mut CmsQueryResult, sql: &str) {
    if out.columns.is_empty() {
        return;
    }
    let tables = select_from_tables(sql);
    if tables.is_empty() {
        return;
    }
    let scales: Vec<Option<u8>> = out
        .columns
        .iter()
        .map(|name| resolve_scale(gate, &tables, name))
        .collect();
    if scales.iter().all(|s| s.is_none()) {
        out.decimal_scales = scales;
        return;
    }
    for row in out.rows.iter_mut() {
        for (i, name) in out.columns.iter().enumerate() {
            let Some(scale) = scales[i] else { continue };
            let Some(v) = row.get_mut(name) else { continue };
            match v {
                Value::Float(x) => *v = Value::Text(fixed_decimal(*x, scale)),
                Value::Int(x) => *v = Value::Text(fixed_decimal(*x as f64, scale)),
                _ => {}
            }
        }
    }
    out.decimal_scales = scales;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_decimal_renders_mysql_scale() {
        assert_eq!(fixed_decimal(250.0, 2), "250.00");
        assert_eq!(fixed_decimal(250.5, 2), "250.50");
        assert_eq!(fixed_decimal(0.1 + 0.2, 2), "0.30");
        assert_eq!(fixed_decimal(-5.5, 2), "-5.50");
        assert_eq!(fixed_decimal(500.0, 0), "500");
        assert_eq!(fixed_decimal(0.0, 8), "0.00000000");
    }

    #[test]
    fn from_tables_single_and_aliased() {
        assert_eq!(
            select_from_tables("SELECT option_name FROM wp_options WHERE autoload = 'yes'"),
            vec![("wp_options".to_string(), "wp_options".to_string())]
        );
        assert_eq!(
            select_from_tables("SELECT o.option_name FROM `wp_options` AS o LIMIT 5"),
            vec![("o".to_string(), "wp_options".to_string())]
        );
        // Bare alias without AS.
        assert_eq!(
            select_from_tables("SELECT p.ID FROM wp_posts p WHERE p.post_status = 'publish'"),
            vec![("p".to_string(), "wp_posts".to_string())]
        );
    }

    #[test]
    fn from_tables_joins_and_commas() {
        let sql = "SELECT m.meta_value FROM wp_posts p \
                   INNER JOIN wp_postmeta m ON p.ID = m.post_id \
                   LEFT JOIN wp_terms t ON t.term_id = p.ID \
                   WHERE p.post_type = 'product'";
        assert_eq!(
            select_from_tables(sql),
            vec![
                ("p".to_string(), "wp_posts".to_string()),
                ("m".to_string(), "wp_postmeta".to_string()),
                ("t".to_string(), "wp_terms".to_string()),
            ]
        );
        let comma = "SELECT a.x FROM t_a a, t_b b WHERE a.id = b.id";
        assert_eq!(
            select_from_tables(comma),
            vec![
                ("a".to_string(), "t_a".to_string()),
                ("b".to_string(), "t_b".to_string()),
            ]
        );
    }

    #[test]
    fn from_tables_skip_literals_and_subqueries() {
        // Quoted 'FROM' inside a string must not open the FROM clause.
        assert!(select_from_tables(
            "SELECT * FROM t WHERE note = 'send for update from here'"
        )
        .len()
            == 1);
        // Aggregate / FOR UPDATE tails stay out of the table list.
        let sql = "SELECT sku, qty FROM inventory FOR UPDATE";
        assert_eq!(
            select_from_tables(sql),
            vec![("inventory".to_string(), "inventory".to_string())]
        );
    }

    #[test]
    fn from_tables_index_hints_and_db_qualified() {
        // FORCE INDEX tail after the ident must not become the alias.
        let sql = "SELECT p.ID FROM wp_posts p FORCE INDEX (type_status_date) \
                   LEFT JOIN wp_postmeta m ON p.ID = m.post_id";
        assert_eq!(
            select_from_tables(sql),
            vec![
                ("p".to_string(), "wp_posts".to_string()),
                ("m".to_string(), "wp_postmeta".to_string()),
            ]
        );
        // db-qualified table name resolves to the bare table.
        assert_eq!(
            select_from_tables("SELECT v FROM mydb.wp_config v"),
            vec![("v".to_string(), "wp_config".to_string())]
        );
    }
}
