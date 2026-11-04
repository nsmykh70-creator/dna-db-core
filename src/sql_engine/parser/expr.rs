// SPDX-License-Identifier: AGPL-3.0-only
//! Literal values (`value()`), MySQL CURRENT_* pseudo-literals and the
//! recursive-descent expression grammar (OR → AND → cmp → LIKE → additive
//! → multiplicative → primary).

use super::*;

impl SParser {
    pub(super) fn value(&mut self) -> Result<Value, String> {
        match self.next() {
            STok::Minus => match self.next() {
                STok::Int(i) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| "SQL: integer literal out of range".to_string()),
                STok::Float(f) => Ok(Value::Float(-f)),
                t => Err(format!("SQL: expected number after '-', got {:?}", t)),
            },
            STok::Int(i) => Ok(Value::Int(i)),
            STok::Float(f) => Ok(Value::Float(f)),
            STok::Str(s) => Ok(typed_string_literal(&s)),
            STok::Ident(s) if s.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            // MySQL pseudo-literals usable in VALUES / DEFAULT:
            // CURRENT_TIMESTAMP [()], CURRENT_DATE [()], CURRENT_TIME [()].
            // Joomla's seed data (base.sql) writes CURRENT_TIMESTAMP().
            STok::Ident(s) if Self::is_current_pseudo_literal_name(&s) => {
                if matches!(self.peek(), STok::LParen) {
                    self.next();
                    self.expect(STok::RParen)?;
                }
                Ok(Value::Text(Self::current_pseudo_literal_value(&s)))
            }
            t => Err(format!("SQL: expected literal, got {:?}", t)),
        }
    }

    /// True for the MySQL CURRENT_* pseudo-literals accepted by value().
    pub(super) fn is_current_pseudo_literal_name(name: &str) -> bool {
        matches!(
            name.to_ascii_uppercase().as_str(),
            "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME"
        )
    }

    /// Value for a CURRENT_* pseudo-literal (string form, MySQL DATETIME).
    pub(super) fn current_pseudo_literal_value(name: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs() as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days_impl(days);
        let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        match name.to_ascii_uppercase().as_str() {
            "CURRENT_DATE" => format!("{y:04}-{m:02}-{d:02}"),
            "CURRENT_TIME" => format!("{hh:02}:{mm:02}:{ss:02}"),
            _ => format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"),
        }
    }

    // expression parsing (same shape as DnaQL)
    pub(super) fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }
    pub(super) fn parse_or(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let result = self.parse_or_inner();
        self.leave();
        result
    }
    pub(super) fn parse_or_inner(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.kw("OR") {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    pub(super) fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_cmp()?;
        while self.kw("AND") {
            let right = self.parse_cmp()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    pub(super) fn parse_cmp(&mut self) -> Result<Expr, String> {
        // v3.6: `NOT <predicate>` including `[NOT] EXISTS (SELECT ...)`.
        if self.kw("NOT") {
            let inner = self.parse_cmp()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("EXISTS")) {
            self.expect_kw("EXISTS")?;
            self.expect(STok::LParen)?;
            if !self.kw("SELECT") {
                return Err("SQL: EXISTS requires a SELECT subquery".into());
            }
            let sub = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            return Ok(Expr::Exists {
                stmt: Box::new(sub),
                negated: false,
            });
        }
        let left = self.parse_operand()?;
        let op = match self.peek() {
            STok::Eq => Some(CmpOp::Eq),
            STok::Ne => Some(CmpOp::Ne),
            STok::Lt => Some(CmpOp::Lt),
            STok::Le => Some(CmpOp::Le),
            STok::Gt => Some(CmpOp::Gt),
            STok::Ge => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let right = self.parse_operand()?;
            return Ok(Expr::Cmp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        // `col [NOT] IN (v1, v2, ...)` / `col [NOT] IN (SELECT ...)`
        // CMS edition: NOT IN is emitted by WordPress core constantly
        // (`comment_type NOT IN ('note')`, post_status NOT IN, …).
        let mut negated_in = false;
        if self.kw("NOT") {
            if self.kw("IN") {
                negated_in = true;
            } else {
                self.expect_kw("LIKE")?;
                let pattern = self.like_pattern()?;
                // `ESCAPE 'c'` tail — same treatment as the plain LIKE above.
                if self.kw("ESCAPE") {
                    let _ = self.like_pattern()?;
                }
                return Ok(Expr::Like {
                    operand: Box::new(left),
                    pattern,
                    negated: true,
                });
            }
        }
        if negated_in || self.kw("IN") {
            let column = match left {
                Expr::Column(c) => c,
                other => {
                    return Err(format!(
                        "SQL: IN requires a column on the left, got '{}'",
                        other.describe()
                    ))
                }
            };
            self.expect(STok::LParen)?;
            if self.kw("SELECT") {
                // Materialised sub-select: render it back to SQL text and let
                // the executor run it (single-column projection required).
                let sub = self.parse_select_tail()?;
                if matches!(self.peek(), STok::RParen) {
                    self.next();
                } else {
                    return Err("SQL: expected ')' after IN subquery".into());
                }
                let query = render_select_sql(&sub);
                let in_sub = Expr::InSubquery { column, query };
                // NOT IN (SELECT …): negate the materialised IN node.
                return if negated_in {
                    Ok(Expr::Not(Box::new(in_sub)))
                } else {
                    Ok(in_sub)
                };
            }
            let mut values = Vec::new();
            loop {
                values.push(self.value()?);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.expect(STok::RParen)?;
            return Ok(Expr::InList { column, values, negated: negated_in });
        }
        if self.kw("IS") {
            if self.kw("NOT") {
                self.expect_kw("NULL")?;
                return Ok(Expr::IsNotNull(Box::new(left)));
            }
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(left)));
        }
        // MySQL `expr LIKE 'pattern'` (the `NOT` variant is handled in the
        // IN/NOT-IN block above). WordPress core uses LIKE for transient
        // cleanup, slug lookups and meta searches. The optional
        // `ESCAPE 'c'` tail (MediaWiki MessageCache title probes) is
        // consumed and ignored — our matcher always treats backslash as
        // the escape character.
        if self.kw("LIKE") {
            let pattern = self.like_pattern()?;
            if self.kw("ESCAPE") {
                let _ = self.like_pattern()?;
            }
            return Ok(Expr::Like {
                operand: Box::new(left),
                pattern,
                negated: false,
            });
        }
        Ok(left)
    }

    /// Pattern literal of a LIKE predicate (quoted string, optionally
    /// parenthesized — WordPress sends both `LIKE 'x'` and `LIKE ('x')`).
    pub(super) fn like_pattern(&mut self) -> Result<String, String> {
        let paren = matches!(self.peek(), STok::LParen);
        if paren {
            self.next();
        }
        let pattern = match self.next() {
            STok::Str(s) => s,
            t => return Err(format!("SQL: LIKE expects a quoted pattern, got {:?}", t)),
        };
        if paren {
            self.expect(STok::RParen)?;
        }
        Ok(pattern)
    }
    /// Additive level of an arithmetic expression: `a + b - c`.
    pub(super) fn parse_operand(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                STok::Plus => ArithOp::Add,
                STok::Minus => ArithOp::Sub,
                _ => break,
            };
            self.next();
            let right = self.parse_multiplicative()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }
    /// Multiplicative level: `a * b / c`.
    pub(super) fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_primary()?;
        loop {
            let op = match self.peek() {
                STok::Star => ArithOp::Mul,
                STok::Slash => ArithOp::Div,
                _ => break,
            };
            self.next();
            let right = self.parse_primary()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }
    /// Primary: literal, column (qualified `t.col` is stripped), aggregate
    /// call, parenthesised expression or unary minus.
    pub(super) fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            STok::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            STok::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            // Same typing rule as INSERT values: pure-ACGT literals are DNA,
            // so WHERE comparisons match the stored (typed) representation.
            STok::Str(s) => {
                self.next();
                Ok(Expr::Literal(typed_string_literal(&s)))
            }
            STok::Ident(s) => {
                // MySQL `CAST(expr AS type)` (WordPress meta queries:
                // `CAST(meta_value AS SIGNED) > 100`).
                if s.eq_ignore_ascii_case("CAST") && matches!(self.peek_at(1), STok::LParen) {
                    self.next();
                    self.next(); // (
                    let inner = self.parse_or()?;
                    self.expect_kw("AS")?;
                    let ty = match self.next() {
                        STok::Ident(t) if t.eq_ignore_ascii_case("SIGNED") => CastTy::Signed,
                        STok::Ident(t) if t.eq_ignore_ascii_case("UNSIGNED") => CastTy::Unsigned,
                        STok::Ident(t)
                            if t.eq_ignore_ascii_case("CHAR")
                                || t.eq_ignore_ascii_case("NCHAR")
                                || t.eq_ignore_ascii_case("BINARY") =>
                        {
                            CastTy::Char
                        }
                        t => {
                            return Err(format!(
                                "SQL: CAST expects SIGNED/UNSIGNED/CHAR, got {:?}",
                                t
                            ))
                        }
                    };
                    // Optional CHAR(n) width / SIGNED INTEGER keyword.
                    if matches!(self.peek(), STok::LParen) {
                        self.next();
                        let _ = self.next(); // width literal
                        self.expect(STok::RParen)?;
                    } else if matches!(self.peek(), STok::Ident(w) if w.eq_ignore_ascii_case("INTEGER"))
                    {
                        self.next();
                    }
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Cast {
                        inner: Box::new(inner),
                        ty,
                    });
                }
                // aggregate reference (HAVING COUNT(*) > 2): resolves to the
                // rendered aggregate column of the grouped result row.
                if matches!(self.peek_at(1), STok::LParen) && AggFunc::from_name(&s).is_ok() {
                    self.next();
                    self.next(); // (
                    let mut distinct = false;
                    if matches!(self.peek(), STok::Ident(d) if d.eq_ignore_ascii_case("DISTINCT")) {
                        self.next();
                        distinct = true;
                    }
                    let arg = if matches!(self.peek(), STok::Star) {
                        self.next();
                        None
                    } else {
                        Some(self.ident()?)
                    };
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Column(render_agg_distinct(
                        &s,
                        arg.as_deref(),
                        distinct,
                    )));
                }
                // CMS edition: MySQL scalar functions — CONCAT(a, b),
                // UPPER(x), IFNULL(a, b), ROUND(x, d), IF(c, t, e), … Each
                // argument is a full expression; evaluation lives in
                // expr::eval_scalar_fn with MySQL NULL semantics.
                if matches!(self.peek_at(1), STok::LParen)
                    && crate::expr::is_scalar_fn(&s)
                {
                    self.next();
                    self.next(); // (
                    let mut args = Vec::new();
                    if !matches!(self.peek(), STok::RParen) {
                        loop {
                            args.push(self.parse_or()?);
                            if matches!(self.peek(), STok::Comma) {
                                self.next();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(STok::RParen)?;
                    return Ok(Expr::Fn {
                        name: s.to_ascii_uppercase(),
                        args,
                    });
                }
                self.next();
                if s.eq_ignore_ascii_case("NULL") {
                    Ok(Expr::Literal(Value::Null))
                } else if matches!(self.peek(), STok::Dot) {
                    // qualified: alias.col -> col
                    self.next();
                    let col = self.ident()?;
                    Ok(Expr::Column(col))
                } else {
                    Ok(Expr::Column(s))
                }
            }
            STok::LParen => {
                self.next();
                // v3.6: scalar subquery `(SELECT ...)` in operand position
                // (WHERE comparisons, arithmetic, IS NULL checks).
                if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT")) {
                    self.next(); // SELECT (parse_select_tail expects it gone)
                    let sub = self.parse_select_tail()?;
                    self.expect(STok::RParen)?;
                    return Ok(Expr::ScalarSubquery {
                        stmt: Box::new(sub),
                    });
                }
                let e = self.parse_or()?;
                match self.next() {
                    STok::RParen => Ok(e),
                    t => Err(format!("SQL: expected ')', got {:?}", t)),
                }
            }
            STok::Minus => {
                self.next();
                match self.parse_primary()? {
                    Expr::Literal(Value::Int(i)) => Ok(Expr::Literal(Value::Int(-i))),
                    Expr::Literal(Value::Float(f)) => Ok(Expr::Literal(Value::Float(-f))),
                    other => Ok(Expr::Arith {
                        op: ArithOp::Sub,
                        left: Box::new(Expr::Literal(Value::Int(0))),
                        right: Box::new(other),
                    }),
                }
            }
            t => Err(format!("SQL: expected operand, got {:?}", t)),
        }
    }
}
