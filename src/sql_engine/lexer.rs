// SPDX-License-Identifier: AGPL-3.0-only
//! SQL tokenizer: a character stream becomes a flat `Vec<STok>`; the parser
//! consumes tokens positionally. Quote-, comment- and number-aware.

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(super) enum STok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Comma,
    Semi,
    LParen,
    RParen,
    Star,
    Plus,
    Minus,
    Slash,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Dot,
    Eof,
}

pub(super) fn stokenize(input: &str) -> Result<Vec<STok>, String> {
    let b: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            ',' => {
                toks.push(STok::Comma);
                i += 1;
            }
            ';' => {
                toks.push(STok::Semi);
                i += 1;
            }
            '(' => {
                toks.push(STok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(STok::RParen);
                i += 1;
            }
            '*' => {
                toks.push(STok::Star);
                i += 1;
            }
            '+' => {
                toks.push(STok::Plus);
                i += 1;
            }
            '/' => {
                // Block comments (`/* … */`, incl. `/*_*/` and
                // `/*$wgDBTableOptions*/` hints in MediaWiki/Drupal dumps)
                // are whitespace for the SQL grammar — skip them entirely.
                // A bare `/` stays the division operator.
                if input[i..].len() > 1 && input.as_bytes()[i + 1] == b'*' {
                    i += 2;
                    while i + 1 < input.len()
                        && !(input.as_bytes()[i] == b'*' && input.as_bytes()[i + 1] == b'/')
                    {
                        i += 1;
                    }
                    i = (i + 2).min(input.len());
                } else {
                    toks.push(STok::Slash);
                    i += 1;
                }
            }
            '.' => {
                toks.push(STok::Dot);
                i += 1;
            }
            '=' => {
                toks.push(STok::Eq);
                i += 1;
            }
            '!' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Ne);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Le);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(STok::Ne);
                i += 2;
            }
            '<' => {
                toks.push(STok::Lt);
                i += 1;
            }
            '>' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(STok::Ge);
                i += 2;
            }
            '>' => {
                toks.push(STok::Gt);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    // CMS edition: MySQL string-literal escapes. `\%` and
                    // `\_` stay two characters so LIKE patterns keep their
                    // escaped wildcards ('\_transient\_%'); the rest map per
                    // the MySQL manual, unknown escapes drop the backslash.
                    #[cfg(feature = "cms")]
                    {
                        if b[i] == '\\' {
                            if i + 1 >= b.len() {
                                return Err("SQL: dangling backslash in string literal".into());
                            }
                            let e = b[i + 1];
                            match e {
                                '0' => s.push('\0'),
                                'n' => s.push('\n'),
                                't' => s.push('\t'),
                                'r' => s.push('\r'),
                                'b' => s.push('\u{0008}'),
                                'Z' => s.push('\u{001a}'),
                                '\'' | '"' | '\\' => s.push(e),
                                '%' | '_' => {
                                    s.push('\\');
                                    s.push(e);
                                }
                                other => s.push(other),
                            }
                            i += 2;
                            continue;
                        }
                    }
                    if b[i] == quote {
                        // standard SQL escape: doubled quote inside a string is a literal quote
                        if i + 1 < b.len() && b[i + 1] == quote {
                            s.push(quote);
                            i += 2;
                            continue;
                        }
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                if !closed {
                    return Err("SQL: unterminated string".into());
                }
                // Standard SQL: double quotes delimit an IDENTIFIER, not a
                // string — Drupal's installer emits
                // `CREATE TABLE "drupal_install_test" (...)`. Single quotes
                // stay string literals (MySQL behaviour).
                if quote == '"' {
                    toks.push(STok::Ident(s));
                } else {
                    toks.push(STok::Str(s));
                }
            }
            // `-5` is a negative literal only at an operand start position;
            // after an operand (`a-5`) the same character is the subtraction
            // operator, tokenized below.
            '-' if i + 1 < b.len()
                && b[i + 1].is_ascii_digit()
                && (toks.is_empty()
                    || matches!(
                        toks.last(),
                        Some(
                            STok::Comma
                                | STok::Semi
                                | STok::LParen
                                | STok::Eq
                                | STok::Ne
                                | STok::Lt
                                | STok::Le
                                | STok::Gt
                                | STok::Ge
                                | STok::Plus
                                | STok::Minus
                                | STok::Star
                                | STok::Slash
                        )
                    )) =>
            {
                let mut num = String::new();
                let mut is_float = false;
                num.push('-');
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(STok::Float(num.parse().map_err(|_| "SQL: bad float")?));
                } else {
                    toks.push(STok::Int(num.parse().map_err(|_| "SQL: bad int")?));
                }
            }
            '-' => {
                toks.push(STok::Minus);
                i += 1;
            }
            '0'..='9' => {
                let mut num = String::new();
                let mut is_float = false;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(STok::Float(num.parse().map_err(|_| "SQL: bad float")?));
                } else {
                    toks.push(STok::Int(num.parse().map_err(|_| "SQL: bad int")?));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                    s.push(b[i]);
                    i += 1;
                }
                toks.push(STok::Ident(s));
            }
            other => return Err(format!("SQL: unexpected character '{}'", other)),
        }
    }
    toks.push(STok::Eof);
    Ok(toks)
}

