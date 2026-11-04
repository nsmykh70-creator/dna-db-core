// SPDX-License-Identifier: AGPL-3.0-only
//! Multi-statement splitting (quotes/comments aware) and `?` placeholder
//! counting/substitution for the text protocol.

use super::*;

// ---------------------------------------------------------------------------
// SQL text walking: split / placeholders / substitution
// ---------------------------------------------------------------------------

pub(crate) fn first_word(sql: &str) -> &str {
    sql.trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
}

/// Splits a COM_QUERY payload into statements on top-level `;`
/// (quote-, backtick- and comment-aware).
pub(crate) fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                cur.push(c);
                copy_quoted('\'', &mut chars, &mut cur);
            }
            '"' => {
                cur.push(c);
                copy_quoted('"', &mut chars, &mut cur);
            }
            '`' => {
                cur.push(c);
                copy_quoted('`', &mut chars, &mut cur);
            }
            '#' => {
                cur.push(c);
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    cur.push(chars.next().unwrap());
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    cur.push(chars.next().unwrap());
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                while let Some(n) = chars.next() {
                    cur.push(n);
                    if n == '*' && chars.peek() == Some(&'/') {
                        cur.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            ';' => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Copies a quoted literal to its closing quote (handling backslash escapes
/// and doubled quotes) into `out`.
pub(crate) fn copy_quoted(quote: char, chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else if c == quote {
            if chars.peek() == Some(&quote) {
                out.push(chars.next().unwrap());
            } else {
                return;
            }
        }
    }
}

/// Counts `?` placeholders outside quotes/comments.
pub(crate) fn count_placeholders(sql: &str) -> usize {
    let mut n = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => copy_quoted(c, &mut chars, &mut String::new()),
            '#' => skip_to_eol(&mut chars),
            '-' if chars.peek() == Some(&'-') => skip_to_eol(&mut chars),
            '/' if chars.peek() == Some(&'*') => skip_block_comment(&mut chars),
            '?' => n += 1,
            _ => {}
        }
    }
    n
}

/// Substitutes every placeholder with NULL (prepare-time probe).
pub(crate) fn substitute_null_params(sql: &str) -> String {
    substitute_params(
        sql,
        std::iter::repeat(ParamValue::Null)
            .take(count_placeholders(sql))
            .collect::<Vec<_>>()
            .as_slice(),
    )
}

/// Replaces each `?` outside quotes/comments with the next rendered literal.
pub(crate) fn substitute_params(sql: &str, values: &[ParamValue]) -> String {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut idx = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                out.push(c);
                copy_quoted(c, &mut chars, &mut out);
            }
            '#' => {
                out.push(c);
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    out.push(chars.next().unwrap());
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(c);
                out.push(chars.next().unwrap());
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    out.push(chars.next().unwrap());
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                out.push(c);
                out.push(chars.next().unwrap());
                while let Some(n) = chars.next() {
                    out.push(n);
                    if n == '*' && chars.peek() == Some(&'/') {
                        out.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            '?' => {
                if idx < values.len() {
                    out.push_str(&values[idx].render());
                    idx += 1;
                } else {
                    out.push('?');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn skip_to_eol(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&n) = chars.peek() {
        if n == '\n' {
            break;
        }
        chars.next();
    }
}

pub(crate) fn skip_block_comment(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(n) = chars.next() {
        if n == '*' && chars.peek() == Some(&'/') {
            chars.next();
            return;
        }
    }
}

