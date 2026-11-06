// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL dialect preprocessing: backtick stripping, SQL_CALC_FOUND_ROWS,
//! INSERT … SELECT dual-table rewrites, comma-JOIN flattening and
//! function-name normalization, applied before the statement reaches
//! the engine.

use super::*;

/// MySQL-dialect preprocessing applied to every statement before dispatch:
/// 1) backtick-quoted identifiers are unquoted (the engine tokenizer has no
///    backtick token; CMS identifiers never need escaping);
/// 2) `DESCRIBE tbl` / `DESC tbl` → `SHOW FULL COLUMNS FROM tbl` (dbDelta
///    actually sends DESCRIBE);
/// 3) `NOW()` inside INSERT/UPDATE → the current timestamp as a literal
///    (engine INSERT values are literals; Woo order timestamps use NOW()).
pub(crate) fn preprocess_mysql_dialect(sql: &str) -> String {
    let unwrapped = unwrap_versioned_comments(sql);
    let stripped = strip_index_hints(&strip_backticks(&unwrapped));
    let upper = stripped.trim_start().to_ascii_uppercase();
    let mut out = if upper.starts_with("DESCRIBE ") || upper.starts_with("DESC ") {
        let off = if upper.starts_with("DESCRIBE ") { 9 } else { 5 };
        let t = stripped.trim_start()[off..]
            .trim()
            .trim_end_matches(';')
            .trim()
            .trim_matches('`')
            .to_string();
        format!("SHOW FULL COLUMNS FROM {}", t)
    } else {
        stripped
    };
    // NB: the statement may carry leading whitespace/newlines (mysqli sends
    // wpdb queries verbatim), so every starts_with check trims first.
    let upper = out.trim_start().to_ascii_uppercase();
    // CMS edition: WooCommerce / ActionScheduler insert pattern —
    // INSERT INTO t (cols) SELECT <literals> FROM DUAL [WHERE (SELECT NULL
    // FROM DUAL) IS NULL]. MySQL inserts one literal row; the guard (when
    // present) is always true. See `rewrite_insert_select_dual`.
    if upper.starts_with("INSERT") && top_level_find_ci(&out, "DUAL").is_some() {
        if let Some(rewritten) = rewrite_insert_select_dual(&out) {
            out = rewritten;
        }
    }
    if upper.starts_with("INSERT") || upper.starts_with("UPDATE") {
        // The literal must be quoted — the engine values are SQL literals.
        out = replace_fn_ci_outside_strings(&out, "NOW()", &format!("'{}'", now_mysql()));
    }
    out
}

/// Removes MySQL index hints - `FORCE INDEX (..)`, `USE INDEX (..)`,
/// `IGNORE INDEX (..)` - including the optional `FOR {JOIN|ORDER BY|GROUP BY}`
/// tail. Hints are advisory: DNA-DB plans from its own complement index
/// metadata, so dropping them never changes results. MediaWiki emits
/// FROM revision FORCE INDEX (rev_page_timestamp) inside derived tables
/// (RevisionStore::findIdenticalRevision), which the core parser rejects.
pub(crate) fn strip_index_hints(s: &str) -> String {
    const WORDS: [&str; 3] = ["FORCE INDEX", "USE INDEX", "IGNORE INDEX"];
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                if i + 1 < chars.len() && chars[i + 1] == q {
                    out.push(q);
                    i += 2;
                    continue;
                }
                in_string = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        let mut hit = false;
        for w in WORDS {
            let wc: Vec<char> = w.chars().collect();
            if i + wc.len() <= chars.len()
                && chars[i..i + wc.len()].iter().zip(wc.iter())
                    .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
                && (i == 0 || !chars[i - 1].is_ascii_alphanumeric()
                    && chars[i - 1] != '_')
            {
                let mut j = i + wc.len();
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let rest: String = chars[j..].iter().collect();
                let upper_rest: String =
                    rest.chars().take(20).collect::<String>().to_ascii_uppercase();
                for tail in ["FOR JOIN", "FOR ORDER BY", "FOR GROUP BY"] {
                    if upper_rest.starts_with(tail)
                        && j + tail.len() <= chars.len()
                        && (j + tail.len() == chars.len()
                            || chars[j + tail.len()].is_whitespace()
                            || chars[j + tail.len()] == '(')
                    {
                        j += tail.len();
                        break;
                    }
                }
                if j < chars.len() && !chars[j].is_whitespace()
                    && chars[j] != '('
                {
                    continue; // e.g. USE INDEXES... - not a hint
                }
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j >= chars.len() || chars[j] != '(' {
                    continue; // no parenthesised list - leave untouched
                }
                let mut depth = 0usize;
                let mut m = j;
                while m < chars.len() {
                    let ch = chars[m];
                    if ch == '\'' || ch == '"' {
                        let q = ch;
                        out.push(ch);
                        m += 1;
                        while m < chars.len() {
                            out.push(chars[m]);
                            if chars[m] == '\\' && m + 1 < chars.len() {
                                out.push(chars[m + 1]);
                                m += 2;
                                continue;
                            }
                            if chars[m] == q {
                                m += 1;
                                break;
                            }
                            m += 1;
                        }
                        continue;
                    }
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            m += 1;
                            break;
                        }
                    }
                    m += 1;
                }
                if depth != 0 {
                    continue;
                }
                i = m;
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                if !out.ends_with(' ') {
                    out.push(' ');
                }
                hit = true;
                break;
            }
        }
        if !hit {
            out.push(c);
            i += 1;
        }
    }
    out
}
/// CMS edition: rewrites the WooCommerce / ActionScheduler INSERT..SELECT
/// pattern into INSERT..VALUES:
///
/// ```text
/// INSERT INTO t (`c1`, `c2`, …) SELECT 'a', 1, NULL, … FROM DUAL
/// [WHERE ( SELECT NULL FROM DUAL ) IS NULL]
/// ```
///
/// MySQL inserts exactly one row of literals; the WHERE guard (when present)
/// is always true. Only this canonical literal-only shape is rewritten;
/// anything else returns None and produces the engine's own clear error.
pub(crate) fn rewrite_insert_select_dual(sql: &str) -> Option<String> {
    // The first top-level DUAL is the SELECT's own FROM; the guard's DUAL
    // sits inside parentheses and is skipped by top_level_find_ci. The cut
    // must remove `FROM DUAL` together — locate the FROM before the DUAL.
    let dual = top_level_find_ci(sql, "DUAL")?;
    let head_full = sql[..dual].trim_end();
    if head_full.len() < 4 || !head_full[head_full.len() - 4..].eq_ignore_ascii_case("FROM") {
        return None;
    }
    let before = head_full[..head_full.len() - 4].trim_end();
    let after = sql[dual + 4..].trim_end_matches(';').trim();
    // normalize whitespace of the optional guard before comparing
    let norm = after.split_whitespace().collect::<Vec<_>>().join(" ");
    let guard_ok = norm.is_empty()
        || norm.eq_ignore_ascii_case("WHERE ( SELECT NULL FROM DUAL ) IS NULL")
        || norm.eq_ignore_ascii_case("WHERE (SELECT NULL FROM DUAL) IS NULL");
    if !guard_ok {
        return None;
    }
    let sel = top_level_find_ci(before, "SELECT")?;
    let head = before[..sel].trim_end();
    let list = before[sel + "SELECT".len()..].trim();
    if head.is_empty() || list.is_empty() {
        return None;
    }
    Some(format!("{head} VALUES ({list})"))
}

/// Removes MySQL backtick quoting from identifiers. String literals are
/// respected (single/double quoted, backslash escapes), doubled backticks
/// inside an identifier decode to a literal backtick.
pub(crate) fn strip_backticks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_string: Option<char> = None;
    let mut in_backtick = false;
    while let Some(c) = chars.next() {
        match in_string {
            Some(q) => {
                out.push(c);
                if c == '\\' {
                    if let Some(&n) = chars.peek() {
                        out.push(n);
                        chars.next();
                    }
                } else if c == q {
                    if chars.peek() == Some(&q) {
                        out.push(q);
                        chars.next();
                    } else {
                        in_string = None;
                    }
                }
            }
            None => {
                if in_backtick {
                    if c == '`' {
                        if chars.peek() == Some(&'`') {
                            out.push('`');
                            chars.next();
                        } else {
                            in_backtick = false;
                        }
                    } else {
                        out.push(c);
                    }
                } else if c == '`' {
                    in_backtick = true;
                } else {
                    if c == '\'' || c == '"' {
                        in_string = Some(c);
                    }
                    out.push(c);
                }
            }
        }
    }
    out
}

/// CMS edition: strips a leading `SELECT DISTINCT` (also `DISTINCTROW`),
/// returns true when the flag was present. The dedupe happens after
/// execution (see `execute`) because the engine has no DISTINCT projection.
pub(crate) fn strip_select_distinct(sql: &mut String) -> bool {
    let trimmed = sql.trim_start();
    let upper_head = trimmed.to_ascii_uppercase();
    for kw in ["SELECT DISTINCTROW", "SELECT DISTINCT"] {
        if upper_head.starts_with(kw) {
            let rest = &trimmed[kw.len()..];
            // Guard against a column actually named DISTINCT…: require the
            // next character to start a new token (space) or nothing.
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                *sql = format!("SELECT{}", rest);
                return true;
            }
        }
    }
    false
}

/// CMS edition: rewrites MySQL comma-FROM lists into the engine's join
/// grammar. `FROM a AS x, b AS y WHERE …` becomes
/// `FROM a AS x INNER JOIN b AS y ON 1=1 WHERE …` — MySQL evaluates the
/// comma form as a cross join filtered by WHERE; the engine does the same
/// with `ON 1=1` (see `SParser` / `cross_join`). Only the first (top-level)
/// FROM of a SELECT is rewritten; commas inside parens/strings are ignored.
pub(crate) fn rewrite_comma_from(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    let mut in_backtick = false;
    let mut depth = 0i32;
    // 1) find the top-level FROM keyword (word boundary, outside literals).
    let mut from_pos: Option<usize> = None;
    while i < chars.len() {
        let c = chars[i];
        match in_string {
            Some(q) => {
                if c == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if c == q {
                    in_string = None;
                }
            }
            None => {
                if in_backtick {
                    if c == '`' {
                        in_backtick = false;
                    }
                } else if c == '`' {
                    in_backtick = true;
                } else if c == '\'' || c == '"' {
                    in_string = Some(c);
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                } else if depth == 0
                    && c.to_ascii_uppercase() == 'F'
                    && is_word_at(&chars, i, "FROM")
                {
                    from_pos = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    let from_start = match from_pos {
        Some(p) => p,
        None => return sql.to_string(),
    };
    // 2) scan the table-ref list; stop at the first top-level keyword that
    //    terminates FROM (or end of statement). Track top-level commas.
    let stops: [&str; 13] = [
        "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "UNION", "JOIN", "INNER", "LEFT",
        "RIGHT", "FULL", "CROSS",
    ];
    let mut j = from_start + 4; // past "FROM"
    let mut depth2 = 0i32;
    let mut in_string2: Option<char> = None;
    let mut in_backtick2 = false;
    let mut commas: Vec<usize> = Vec::new();
    let mut end = chars.len();
    while j < chars.len() {
        let c = chars[j];
        match in_string2 {
            Some(q) => {
                if c == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if c == q {
                    in_string2 = None;
                }
            }
            None => {
                if in_backtick2 {
                    if c == '`' {
                        in_backtick2 = false;
                    }
                } else if c == '`' {
                    in_backtick2 = true;
                } else if c == '\'' || c == '"' {
                    in_string2 = Some(c);
                } else if c == '(' {
                    depth2 += 1;
                } else if c == ')' {
                    if depth2 == 0 {
                        end = j;
                        break;
                    }
                    depth2 -= 1;
                } else if depth2 == 0 {
                    if c == ',' {
                        commas.push(j);
                    } else if c.is_ascii_alphabetic() || c == '_' {
                        if let Some(w) = word_at(&chars, j) {
                            if stops.iter().any(|s| s.eq_ignore_ascii_case(&w)) {
                                end = j;
                                break;
                            }
                            // skip the rest of this word
                            j += w.len() - 1;
                        }
                    } else if c == ';' {
                        end = j;
                        break;
                    }
                }
            }
        }
        j += 1;
    }
    if commas.is_empty() {
        return sql.to_string();
    }
    // 3) rebuild: split on top-level commas inside [from+4, end) and rejoin
    //    every ref after the first with INNER JOIN … ON 1=1. The original
    //    spacing is preserved: refs keep their raw text (only trimmed for
    //    the emptiness check), and explicit separators are inserted where
    //    parts were joined back together.
    let mut refs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for k in from_start + 4..end {
        let c = chars[k];
        if commas.contains(&k) {
            refs.push(cur.clone());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    refs.push(cur);
    if refs.iter().any(|r| r.trim().is_empty()) {
        return sql.to_string();
    }
    let mut rebuilt = String::with_capacity(sql.len() + 24 * refs.len());
    rebuilt.push_str(&chars[..from_start + 4].iter().collect::<String>());
    let first = &refs[0];
    if !first.starts_with(char::is_whitespace) {
        rebuilt.push(' ');
    }
    rebuilt.push_str(first.trim_end());
    for r in &refs[1..] {
        rebuilt.push_str(" INNER JOIN ");
        rebuilt.push_str(r.trim());
        rebuilt.push_str(" ON 1=1");
    }
    let tail: String = chars[end..].iter().collect();
    if !tail.is_empty()
        && !tail.starts_with(char::is_whitespace)
        && !tail.starts_with(';')
        && !tail.starts_with(')')
    {
        rebuilt.push(' ');
    }
    rebuilt.push_str(&tail);
    rebuilt
}

/// The word at `pos` equals `word` (case-insensitive) and has a non-word
/// character (or boundary) on both sides.
pub(crate) fn is_word_at(chars: &[char], pos: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if pos > 0 {
        let prev = chars[pos - 1];
        if prev.is_ascii_alphanumeric() || prev == '_' {
            return false;
        }
    }
    if pos + w.len() > chars.len() {
        return false;
    }
    for (k, wc) in w.iter().enumerate() {
        if chars[pos + k].to_ascii_uppercase() != *wc {
            return false;
        }
    }
    let after = pos + w.len();
    if after < chars.len() {
        let next = chars[after];
        if next.is_ascii_alphanumeric() || next == '_' {
            return false;
        }
    }
    true
}

/// The alphanumeric word starting exactly at `pos`, or None.
pub(crate) fn word_at(chars: &[char], pos: usize) -> Option<String> {
    let mut out = String::new();
    let mut i = pos;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
        out.push(chars[i]);
        i += 1;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Case-insensitive replacement of a function call token (`NOW()`) outside
/// string literals; string contents are never touched.
pub(crate) fn replace_fn_ci_outside_strings(s: &str, needle: &str, replacement: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let nchars: Vec<char> = needle.chars().collect();
    let mut out = String::with_capacity(s.len() + replacement.len());
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                if i + 1 < chars.len() && chars[i + 1] == q {
                    out.push(q);
                    i += 2;
                    continue;
                }
                in_string = None;
            }
            i += 1;
            continue;
        }
        let mut matched = c.to_ascii_lowercase() == nchars[0].to_ascii_lowercase();
        if matched {
            for (k, nc) in nchars.iter().enumerate() {
                if i + k >= chars.len()
                    || chars[i + k].to_ascii_lowercase() != nc.to_ascii_lowercase()
                {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            out.push_str(replacement);
            i += nchars.len();
            continue;
        }
        if c == '\'' || c == '"' {
            in_string = Some(c);
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Current server time in the MySQL literal format `YYYY-MM-DD HH:MM:SS`
/// (UTC; the CMS edition does not model per-server time zones yet).
pub(crate) fn now_mysql() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil_from_days: days since 1970-01-01 → (y, m, d).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// MySQL versioned-comment unwrapping: `/*!NNNNN body */` → `body` when
/// `NNNNN` ≤ the compat server version (10.11.11-MariaDB → 101111), the
/// empty string otherwise. Plain `/* … */` comments and `/*! … */` without a
/// version number are dropped as well (the engine tokenizer has no comment
/// token). String literals (single/double quotes, backslash escapes) are
/// copied verbatim — a dump value may legitimately contain `/*`.
///
/// Real mysqldump / mariadb-dump output is full of these
/// (`/*!40101 SET @saved_cs_client=@@character_set_client */;`,
/// `CREATE DATABASE /*!32312 IF NOT EXISTS*/ db`, `LOCK TABLES t READ
/// /*!32311 LOCAL */`, `SELECT /*!40001 SQL_NO_CACHE */ * …`), and
/// `mariadb < dump.sql` forwards them as statements.
pub(crate) fn unwrap_versioned_comments(s: &str) -> String {
    const SERVER_VERSION_NUM: u64 = 101_111; // 10.11.11-MariaDB
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    let mut in_string: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            in_string = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        // Comment start?
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // `/*!NNNNN … */` (MySQL) and `/*M!NNNNN … */` (MariaDB-specific)
            // share the versioned-comment machinery. The M! form matters for
            // dumps: mariadb-dump ≥ 10.11.18 opens every dump with the
            // sandbox-mode marker `/*M!999999\- enable the sandbox mode */`,
            // which is a standalone statement for the replaying client.
            let m_form = i + 3 < chars.len() && chars[i + 2] == 'M' && chars[i + 3] == '!';
            if (i + 2 < chars.len() && chars[i + 2] == '!') || m_form {
                // Versioned: parse the digits after `/*!` / `/*M!`.
                let mut j = if m_form { i + 4 } else { i + 3 };
                let mut ver: u64 = 0;
                let mut has_digits = false;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    ver = ver * 10 + (chars[j] as u64 - '0' as u64);
                    j += 1;
                    has_digits = true;
                }
                // Find the closing `*/` at the same nesting level (comments
                // do not nest in dump output, but honour `/*` inside anyway).
                let mut depth = 1usize;
                let mut k = j;
                let mut inner = String::new();
                let mut closed_at: Option<usize> = None;
                while k < chars.len() {
                    if k + 1 < chars.len() && chars[k] == '/' && chars[k + 1] == '*' {
                        depth += 1;
                        inner.push_str("/*");
                        k += 2;
                        continue;
                    }
                    if k + 1 < chars.len() && chars[k] == '*' && chars[k + 1] == '/' {
                        depth -= 1;
                        if depth == 0 {
                            closed_at = Some(k + 2);
                            break;
                        }
                        inner.push_str("*/");
                        k += 2;
                        continue;
                    }
                    inner.push(chars[k]);
                    k += 1;
                }
                if let Some(end) = closed_at {
                    let executes = !has_digits || ver <= SERVER_VERSION_NUM;
                    if executes {
                        out.push_str(&inner);
                    }
                    i = end;
                    continue;
                }
                // Unterminated comment — copy the rest verbatim.
                out.push_str(chars[i..].iter().collect::<String>().as_str());
                return out;
            }
            // Plain `/* … */` — drop it entirely (top level, outside strings).
            let mut k = i + 2;
            let mut closed_at: Option<usize> = None;
            while k < chars.len() {
                if k + 1 < chars.len() && chars[k] == '*' && chars[k + 1] == '/' {
                    closed_at = Some(k + 2);
                    break;
                }
                k += 1;
            }
            if let Some(end) = closed_at {
                i = end;
                continue;
            }
            out.push_str(chars[i..].iter().collect::<String>().as_str());
            return out;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Removes `FOREIGN KEY …`, `CONSTRAINT … FOREIGN KEY|CHECK …` and `CHECK …`
/// segments from a CREATE TABLE body before the DDL reaches the engine
/// (the engine grammar has no referential-action parsing; the overlay already
/// skips these lines — this mirrors that for the forwarded DDL). Referential
/// integrity itself is enforced by the engine's declared FK layer when used.
///
/// The input is a full CREATE TABLE statement; the function is a no-op for
/// every other statement shape.
pub(crate) fn strip_fk_constraints(create_table_sql: &str) -> String {
    let s = create_table_sql.trim();
    let upper_head = s.to_ascii_uppercase();
    if !upper_head.starts_with("CREATE TABLE") && !upper_head.starts_with("CREATE TEMPORARY TABLE") {
        return s.to_string();
    }
    let Some(body_start) = s.find('(') else {
        return s.to_string();
    };
    // Balanced end of the column-list paren (string-aware).
    let chars: Vec<char> = s.chars().collect();
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut body_end = chars.len();
    for (i, &c) in chars.iter().enumerate().skip(body_start) {
        if in_quote {
            if c == '\\' {
                continue;
            }
            if c == '\'' {
                in_quote = false;
            }
            continue;
        }
        match c {
            '\'' => in_quote = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    body_end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if body_end > chars.len() {
        return s.to_string();
    }
    let head = s[..body_start].to_string();
    let body = &s[body_start + 1..body_end - 1];
    let tail = s[body_end..].to_string();
    let segments = crate::cms::ddl_meta::split_top_level(body);
    let kept: Vec<String> = segments
        .into_iter()
        .filter(|seg| {
            let first = seg
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            !matches!(first.as_str(), "FOREIGN" | "CONSTRAINT" | "CHECK")
        })
        .collect();
    if kept.is_empty() {
        return s.to_string();
    }
    format!("{} ({}) {}", head.trim_end(), kept.join(", "), tail.trim())
}
