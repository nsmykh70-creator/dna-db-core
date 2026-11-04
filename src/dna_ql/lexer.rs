// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL tokenizer: the `Tok` lexeme enum, the `tokenize` scanner and the
//! CREATE CHROMOSOME clause-buffer helpers (`push_clause_token`,
//! `float_token`) shared with the parser.

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Question,  // ?
    Tilde,     // ~
    Arrow,     // ->
    Pipe,      // |>
    FatArrow,  // =>
    PlusEq,    // +=
    MinusEq,   // -=
    Eq,        // =
    Ne,        // != or <>
    Le,        // <=
    Ge,        // >=
    Lt,        // <
    Gt,        // >
    Colon,     // :
    Semicolon, // ;
    Comma,     // ,
    Dot,       // .
    LParen,
    RParen,
    LBrace,
    RBrace,
    Star,
    Plus,  // +
    Minus, // -
    Slash, // /
    Eof,
}

pub(crate) fn tokenize(input: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let b: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '-' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Arrow);
                i += 2;
            }
            '|' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Pipe);
                i += 2;
            }
            '=' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::FatArrow);
                i += 2;
            }
            '+' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::PlusEq);
                i += 2;
            }
            '-' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::MinusEq);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Le);
                i += 2;
            }
            '>' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Ge);
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == '>' => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '!' if i + 1 < b.len() && b[i + 1] == '=' => {
                toks.push(Tok::Ne);
                i += 2;
            }
            '?' => {
                toks.push(Tok::Question);
                i += 1;
            }
            '~' => {
                toks.push(Tok::Tilde);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '=' => {
                toks.push(Tok::Eq);
                i += 1;
            }
            '<' => {
                toks.push(Tok::Lt);
                i += 1;
            }
            '>' => {
                toks.push(Tok::Gt);
                i += 1;
            }
            ':' => {
                toks.push(Tok::Colon);
                i += 1;
            }
            ';' => {
                toks.push(Tok::Semicolon);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                toks.push(Tok::Dot);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            '{' => {
                toks.push(Tok::LBrace);
                i += 1;
            }
            '}' => {
                toks.push(Tok::RBrace);
                i += 1;
            }
            '\'' | '"' => {
                let quote = c;
                let mut s = String::new();
                i += 1;
                let mut closed = false;
                while i < b.len() {
                    if b[i] == quote {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                if !closed {
                    return Err("DnaQL: unterminated string literal".into());
                }
                toks.push(Tok::Str(s));
            }
            '0'..='9' => {
                let mut num = String::new();
                let mut is_float = false;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
                {
                    if b[i] == '.' {
                        if is_float {
                            return Err("DnaQL: malformed number".into());
                        }
                        is_float = true;
                    }
                    num.push(b[i]);
                    i += 1;
                }
                if is_float {
                    toks.push(Tok::Float(num.parse().map_err(|_| "DnaQL: bad float")?));
                } else {
                    toks.push(Tok::Int(num.parse().map_err(|_| "DnaQL: bad int")?));
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                    s.push(b[i]);
                    i += 1;
                }
                toks.push(Tok::Ident(s));
            }
            other => return Err(format!("DnaQL: unexpected character '{}'", other)),
        }
    }
    toks.push(Tok::Eof);
    Ok(toks)
}

/// Append one raw token to a CREATE CHROMOSOME definition clause buffer,
/// separating tokens with a single space (whitespace is irrelevant to the
/// SQL engine that re-parses these clauses).
pub(crate) fn push_clause_token(buf: &mut String, tok: &str) {
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(tok);
}

/// Float rendering that always keeps a decimal point, so re-parsing the
/// token yields a Float (and not an Int) again.
pub(crate) fn float_token(f: &f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}
