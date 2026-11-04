// SPDX-License-Identifier: AGPL-3.0-only
//! SELECT parsing: CTE heads (`WITH`), select tails, projections,
//! FROM/JOIN/WHERE/GROUP BY/HAVING/ORDER BY/LIMIT and select items.

use super::*;

impl SParser {
    /// `WITH name AS (SELECT ...) [, name2 AS (...)] body` (v3.6).
    pub(super) fn parse_cte(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        let mut ctes = Vec::new();
        loop {
            let name = self.ident()?;
            self.expect_kw("AS")?;
            self.expect(STok::LParen)?;
            if !self.kw("SELECT") {
                return Err("SQL: CTE body must be a SELECT (WITH name AS (SELECT ...))".into());
            }
            let def = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            ctes.push((name, def));
            if matches!(self.peek(), STok::Comma) {
                self.next();
                continue;
            }
            break;
        }
        let body = self.parse_stmt()?;
        self.leave();
        Ok(Stmt::Cte {
            ctes,
            body: Box::new(body),
        })
    }

    /// Body of a SELECT after the SELECT keyword has been consumed.
    /// v3.6: recognises a trailing `UNION [ALL] SELECT ...` and builds a
    /// compound statement (chained left-associatively).
    pub(super) fn parse_select_tail(&mut self) -> Result<Stmt, String> {
        let stmt = self.parse_select()?;
        if !self.kw("UNION") {
            return Ok(stmt);
        }
        let all = self.kw("ALL");
        // Peek WITHOUT consuming: parse_stmt handles SELECT/WITH itself
        // (consuming the keyword here swallowed the projection token).
        let next_is_select = matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT") || s.eq_ignore_ascii_case("WITH"));
        if !next_is_select {
            return Err("SQL: expected SELECT after UNION".into());
        }
        let right = self.parse_stmt()?;
        // ORDER BY / LIMIT / OFFSET parsed into the right-hand select belong
        // to the compound result (standard SQL set-operation semantics).
        let (order_by, limit, offset) = match &right {
            Stmt::Select {
                order_by,
                limit,
                offset,
                ..
            } => (order_by.clone(), *limit, *offset),
            _ => (Vec::new(), None, 0),
        };
        Ok(Stmt::Union {
            left: Box::new(stmt),
            right: Box::new(right),
            all,
            order_by,
            limit,
            offset,
        })
    }

    pub(super) fn parse_select(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        let result = self.parse_select_inner();
        self.leave();
        result
    }

    pub(super) fn parse_select_inner(&mut self) -> Result<Stmt, String> {
        let mut projection = Vec::new();
        if matches!(self.peek(), STok::Star) {
            self.next();
            projection.push(SelectItem::Star);
        } else {
            // CMS edition: bare computed items with identical rendered names
            // (two identical literals — `SELECT 'ts', 'ts'`) must survive:
            // MySQL reports every occurrence, and INSERT..SELECT maps columns
            // positionally. The BTreeMap row model needs unique names, so
            // duplicates get a ` (N)` suffix (MariaDB-style disambiguation).
            let mut seen_computed: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            loop {
                let mut item = self.parse_select_item()?;
                if let SelectItem::Computed { expr, rendered } = &mut item {
                    if !seen_computed.insert(rendered.clone()) {
                        let mut n = 2usize;
                        while seen_computed.contains(&format!("{rendered} ({n})")) {
                            n += 1;
                        }
                        *rendered = format!("{rendered} ({n})");
                        seen_computed.insert(rendered.clone());
                        let _ = expr;
                    }
                }
                projection.push(item);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
        }
        // v4.0: FROM-less SELECT (`SELECT (SELECT MAX(x) FROM t) AS mx`) —
        // the canonical way to evaluate scalar expressions / subqueries.
        // Everything else still requires FROM.
        // CMS edition: derived tables — `FROM ( SELECT … ) AS x` and
        // `JOIN ( SELECT … ) AS y` are materialised as statement-scoped
        // CTEs (same execution semantics, zero executor changes).
        let mut deriveds: Vec<(String, Stmt)> = Vec::new();
        let from = if matches!(self.peek(), STok::Ident(s) if s.eq_ignore_ascii_case("FROM")) {
            self.expect_kw("FROM")?;
            if matches!(self.peek(), STok::LParen) {
                // derived table: `( SELECT … ) [AS] alias`
                self.next();
                if !self.kw("SELECT") {
                    return Err("SQL: expected SELECT in derived table".into());
                }
                let sub = self.parse_select_tail()?;
                self.expect(STok::RParen)?;
                let alias = self
                    .maybe_alias()
                    .ok_or_else(|| "SQL: derived table requires an alias".to_string())?;
                deriveds.push((alias.clone(), sub));
                alias
            } else {
                self.ident()?
            }
        } else {
            String::new()
        };
        let from_alias = self.maybe_alias();
        let mut joins = Vec::new();
        // CMS edition: extra ON predicates of compound join conditions
        // (`ON a = b AND c = d`) — folded into WHERE after the join loop.
        let mut on_extra: Vec<Expr> = Vec::new();
        loop {
            let join_type = if self.kw("INNER") {
                self.expect_kw("JOIN")?;
                JoinType::Inner
            } else if self.kw("LEFT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Left
            } else if self.kw("RIGHT") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Right
            } else if self.kw("FULL") {
                self.kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinType::Full
            } else if self.kw("JOIN") {
                JoinType::Inner
            } else {
                break;
            };
            let (jtable, jalias, jderived) = if matches!(self.peek(), STok::LParen) {
                // derived join table: `JOIN ( SELECT … ) [AS] alias`
                self.next();
                if !self.kw("SELECT") {
                    return Err("SQL: expected SELECT in derived join table".into());
                }
                let sub = self.parse_select_tail()?;
                self.expect(STok::RParen)?;
                let a = self
                    .maybe_alias()
                    .ok_or_else(|| "SQL: derived join table requires an alias".to_string())?;
                (a.clone(), Some(a.clone()), Some((a, sub)))
            } else {
                let t = self.ident()?;
                let a = self.maybe_alias();
                (t.clone(), a, None)
            };
            if let Some(d) = jderived {
                deriveds.push(d);
            }
            let table = jtable;
            // CMS edition: `USING (col)` — natural join shorthand for
            // ON a.col = b.col (WooCommerce term recount). Checked before
            // the mandatory ON keyword.
            if self.kw("USING") {
                self.expect(STok::LParen)?;
                let col = self.ident()?;
                self.expect(STok::RParen)?;
                joins.push(JoinSpec {
                    join_type,
                    table,
                    alias: jalias,
                    left_col: col.clone(),
                    right_col: col,
                });
                continue;
            }
            self.expect_kw("ON")?;
            // WordPress always sends parenthesized join conditions:
            // ON (a.id = b.id) — accept the optional parens.
            let paren = matches!(self.peek(), STok::LParen);
            if paren {
                self.next();
            }
            // CMS edition (branch `cms-edition`): ON 1=1 marks a pure cross
            // join. The compat gate rewrites MySQL comma-FROM lists
            // (`FROM a, b WHERE …`) into `a INNER JOIN b ON 1=1`, keeping the
            // filter predicates in WHERE exactly like MySQL does.
            if matches!(self.peek(), STok::Int(1)) {
                let save = self.pos;
                self.next();
                if matches!(self.next(), STok::Eq) && matches!(self.next(), STok::Int(1)) {
                    if paren {
                        self.expect(STok::RParen)?;
                    }
                    joins.push(JoinSpec {
                        join_type,
                        table,
                        alias: jalias,
                        left_col: "1".to_string(),
                        right_col: "1".to_string(),
                    });
                    continue;
                }
                self.pos = save;
            }
            // CMS edition: the ON predicate is parsed as a full expression.
            // A plain `col = col` equality keeps the hash join; every other
            // shape (column = literal, compound AND chains — WooCommerce
            // refund totals `ON ( t.type = 'refund' AND t.parent = 11 )`)
            // becomes a filtered cross join (ON 1=1 + WHERE) — identical
            // results for INNER joins, which is what WordPress/WooCommerce
            // emit. Parens are handled by the expression grammar itself.
            let cond = self.parse_expr()?;
            let simple = match &cond {
                Expr::Cmp {
                    op: CmpOp::Eq,
                    left,
                    right,
                } => match (&**left, &**right) {
                    (Expr::Column(l), Expr::Column(r)) => Some((l.clone(), r.clone())),
                    _ => None,
                },
                _ => None,
            };
            match simple {
                Some((l, r)) => joins.push(JoinSpec {
                    join_type,
                    table,
                    alias: jalias,
                    left_col: l,
                    right_col: r,
                }),
                None => {
                    joins.push(JoinSpec {
                        join_type,
                        table,
                        alias: jalias,
                        left_col: "1".to_string(),
                        right_col: "1".to_string(),
                    });
                    on_extra.push(cond);
                }
            }
            while self.kw("AND") {
                on_extra.push(self.parse_expr()?);
            }
            if paren {
                self.expect(STok::RParen)?;
            }
        }
        let mut where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        // fold the extra ON predicates into the filter (see the join loop)
        if !on_extra.is_empty() {
            let extra = on_extra
                .into_iter()
                .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
                .expect("on_extra non-empty");
            where_clause = Some(match where_clause {
                Some(w) => Expr::And(Box::new(extra), Box::new(w)),
                None => extra,
            });
        }
        let group_by = if self.kw("GROUP") {
            self.expect_kw("BY")?;
            let mut cols = Vec::new();
            loop {
                let mut col = self.ident()?;
                // qualified reference a.col — the qualifier is stripped.
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    col = self.ident()?;
                }
                cols.push(col);
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            cols
        } else {
            Vec::new()
        };
        let having = if self.kw("HAVING") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let order_by = if self.kw("ORDER") {
            self.expect_kw("BY")?;
            let mut keys = Vec::new();
            loop {
                // order key: a column, or an aggregate call (COUNT(*) ...)
                let start = self.pos;
                let mut key = self.ident()?;
                // qualified reference `t.col` — the qualifier is stripped
                // (single-namespace row model), as in projections.
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    key = self.ident()?;
                }
                if matches!(self.peek(), STok::LParen) {
                    self.next();
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
                    key = render_agg_distinct(&key, arg.as_deref(), distinct);
                }
                // CMS edition: an arithmetic order key — `ORDER BY
                // meta_value+0 DESC` is the canonical WordPress/WooCommerce
                // numeric-sort idiom for varchar meta. Rewind and parse the
                // whole key as an expression; exec_select materialises it
                // per row before sorting and it emits no output column.
                // (Plain builds keep the original strict grammar: the key
                // ends here and the operator surfaces as trailing tokens.)
                else if cfg!(feature = "cms")
                    && (matches!(
                        self.peek(),
                        STok::Plus | STok::Minus | STok::Star | STok::Slash
                    ) || matches!(
                        // `ORDER BY CASE … END [DESC]` — Joomla's frontpage
                        // ordering sorts on a CASE expression; rewind and
                        // parse the whole key as an expression like the
                        // arithmetic idiom above.
                        self.peek(),
                        STok::Ident(w) if w.eq_ignore_ascii_case("WHEN")
                    ))
                {
                    self.pos = start;
                    let expr = self.parse_expr()?;
                    let rendered = expr.describe();
                    projection.push(SelectItem::OrderKeyOnly {
                        expr,
                        rendered: rendered.clone(),
                    });
                    key = rendered;
                }
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                keys.push((key, asc));
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
        };
        let mut limit: Option<usize> = None;
        let mut offset: usize = 0;
        if self.kw("LIMIT") {
            match self.next() {
                STok::Int(i) => {
                    // MySQL pagination form `LIMIT offset, count` — WordPress
                    // (WP_Query) sends it everywhere; == LIMIT count OFFSET
                    // offset. Without the cms feature the comma stays a
                    // parse error (original strict grammar).
                    #[cfg(feature = "cms")]
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                        offset = i as usize;
                        match self.next() {
                            STok::Int(c) => limit = Some(c as usize),
                            t => return Err(format!("SQL: LIMIT expects int, got {:?}", t)),
                        }
                    } else {
                        limit = Some(i as usize);
                    }
                    #[cfg(not(feature = "cms"))]
                    {
                        limit = Some(i as usize);
                    }
                }
                t => return Err(format!("SQL: LIMIT expects int, got {:?}", t)),
            }
        }
        if self.kw("OFFSET") {
            match self.next() {
                STok::Int(i) => offset = i as usize,
                t => return Err(format!("SQL: OFFSET expects int, got {:?}", t)),
            }
        }
        // MySQL row-locking tail: `FOR UPDATE` / `FOR SHARE` [OF …] [NOWAIT
        // | SKIP LOCKED]. The engine has no row-level locks yet (lock wait /
        // deadlock detection is CMS_EDITION_RU.md §4 item 2); inside the
        // gate's transaction buffer the statement is still correct, just
        // not serialized — accept and ignore (WooCommerce stock paths).
        #[cfg(feature = "cms")]
        if self.kw("FOR") {
            if !(self.kw("UPDATE") || self.kw("SHARE")) {
                return Err("SQL: expected UPDATE or SHARE after FOR".into());
            }
            if self.kw("OF") {
                loop {
                    let _ = self.ident()?;
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
            }
            if self.kw("NOWAIT") {
            } else if self.kw("SKIP") {
                self.kw("LOCKED");
            }
        }
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        let stmt = Stmt::Select {
            projection,
            from,
            from_alias,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        };
        if deriveds.is_empty() {
            Ok(stmt)
        } else {
            Ok(Stmt::Cte {
                ctes: deriveds,
                body: Box::new(stmt),
            })
        }
    }

    /// One projection item: `*`, a plain column, an aggregate call, a window
    /// function, a scalar subquery or an arithmetic expression
    /// (`total_usd * 2 AS doub`).
    pub(super) fn parse_select_item(&mut self) -> Result<SelectItem, String> {
        if matches!(self.peek(), STok::Star) {
            self.next();
            return Ok(SelectItem::Star);
        }
        // CMS edition: `t.*` — every column of one FROM/JOIN table
        // (expanded at execution against the alias → table map).
        if let STok::Ident(qual) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::Dot) && matches!(self.peek_at(2), STok::Star) {
                self.next();
                self.next();
                self.next();
                return Ok(SelectItem::AliasStar(qual));
            }
        }
        // v3.6: scalar subquery `(SELECT ...) [AS alias]`.
        if matches!(self.peek(), STok::LParen)
            && matches!(self.peek_at(1), STok::Ident(s) if s.eq_ignore_ascii_case("SELECT"))
        {
            self.next(); // (
            if !self.kw("SELECT") {
                return Err("SQL: expected SELECT in scalar subquery".into());
            }
            let sub = self.parse_select_tail()?;
            self.expect(STok::RParen)?;
            let alias = self.maybe_as_alias()?;
            let canonical = render_select_sql(&sub);
            let rendered = alias.unwrap_or_else(|| format!("({})", canonical));
            return Ok(SelectItem::ScalarSub {
                stmt: Box::new(sub),
                canonical,
                rendered,
            });
        }
        // v3.6: window function ROW_NUMBER()/RANK()/DENSE_RANK() OVER (...).
        if let STok::Ident(s) = self.peek().clone() {
            if matches!(self.peek_at(1), STok::LParen)
                && WindowFunc::from_name(&s).is_some()
            {
                let func = WindowFunc::from_name(&s).unwrap();
                self.next();
                self.next(); // (
                self.expect(STok::RParen)?;
                self.expect_kw("OVER")?;
                self.expect(STok::LParen)?;
                let mut partition = Vec::new();
                if self.kw("PARTITION") {
                    self.expect_kw("BY")?;
                    loop {
                        partition.push(self.ident()?);
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                let mut order = Vec::new();
                if self.kw("ORDER") {
                    self.expect_kw("BY")?;
                    loop {
                        let col = self.ident()?;
                        let asc = if self.kw("DESC") {
                            false
                        } else {
                            self.kw("ASC");
                            true
                        };
                        order.push((col, asc));
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(STok::RParen)?;
                let alias = self.maybe_as_alias()?;
                let canonical = render_window(&func, &partition, &order);
                let rendered = alias.unwrap_or_else(|| canonical.clone());
                return Ok(SelectItem::Window {
                    func,
                    partition,
                    order,
                    canonical,
                    rendered,
                });
            }
        }
        // CMS edition: `EXISTS (SELECT …)` as a projection item — boolean
        // subquery (WooCommerce lookup_table_has_data: SELECT EXISTS (SELECT
        // 1 FROM wp_wc_product_attributes_lookup)).
        if let STok::Ident(s) = self.peek().clone() {
            if s.eq_ignore_ascii_case("EXISTS") && matches!(self.peek_at(1), STok::LParen) {
                self.next();
                self.expect(STok::LParen)?;
                if !self.kw("SELECT") {
                    return Err("SQL: EXISTS requires a SELECT subquery".into());
                }
                let sub = self.parse_select_tail()?;
                self.expect(STok::RParen)?;
                let canonical = format!("EXISTS ({})", render_select_sql(&sub));
                let alias = self.maybe_as_alias()?;
                let rendered = alias.unwrap_or_else(|| canonical.clone());
                return Ok(SelectItem::Computed {
                    expr: Expr::Exists {
                        stmt: Box::new(sub),
                        negated: false,
                    },
                    rendered,
                });
            }
        }
        // aggregate call: COUNT(*) / SUM(col) — recognised before the general
        // expression path so `COUNT(*)` never parses as multiplication.
        if let STok::Ident(s) = self.peek().clone() {
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
                    // CMS edition: qualified aggregate arguments —
                    // SUM(postmeta.meta_value) — the qualifier is stripped
                    // (single-namespace row model, like GROUP BY t.col).
                    let is_plain = match (self.peek(), self.peek_at(1)) {
                        (STok::Ident(_), STok::RParen) => true,
                        (STok::Ident(_), STok::Dot) => {
                            matches!(self.peek_at(2), STok::Ident(_))
                                && matches!(self.peek_at(3), STok::RParen)
                        }
                        _ => false,
                    };
                    if is_plain {
                        let mut col = self.ident()?;
                        if matches!(self.peek(), STok::Dot) {
                            self.next();
                            col = self.ident()?;
                        }
                        Some(col)
                    } else {
                        // Expression argument — `SUM(CASE WHEN … END)`,
                        // `AVG(a.x + a.y)` (Joomla's admin dashboard counts).
                        // The caller receives AggOverExpr: the executor
                        // materialises the expression per row under its
                        // rendered name and aggregates over that column.
                        let expr = self.parse_or()?;
                        let arg_name = expr.describe();
                        self.expect(STok::RParen)?;
                        if distinct {
                            return Err(
                                "SQL: DISTINCT aggregate requires a column argument".into()
                            );
                        }
                        let func = AggFunc::from_name(&s)?;
                        let canonical = render_agg_distinct(&s, Some(&arg_name), false);
                        let alias = self.maybe_as_alias()?;
                        let rendered = alias.unwrap_or_else(|| canonical.clone());
                        return Ok(SelectItem::AggOverExpr {
                            expr,
                            arg_name,
                            func,
                            rendered,
                            canonical,
                        });
                    }
                };
                self.expect(STok::RParen)?;
                if distinct && arg.is_none() {
                    return Err("SQL: DISTINCT aggregate requires a column argument".into());
                }
                let func = AggFunc::from_name(&s)?;
                let canonical = render_agg_distinct(&s, arg.as_deref(), distinct);
                let alias = self.maybe_as_alias()?;
                let rendered = alias.clone().unwrap_or_else(|| canonical.clone());
                return Ok(SelectItem::Agg {
                    func,
                    arg,
                    rendered,
                    canonical,
                });
            }
        }
        let expr = self.parse_operand()?;
        match expr {
            Expr::Column(source) => {
                // Qualified reference t.col — the qualifier is stripped
                // (single-namespace row model), as in ORDER BY / GROUP BY.
                let mut source = source;
                if matches!(self.peek(), STok::Dot) {
                    self.next();
                    source = self.ident()?;
                }
                let alias = self.maybe_as_alias()?;
                Ok(SelectItem::Column { source, alias })
            }
            other => {
                let alias = self.maybe_as_alias()?;
                let rendered = alias.unwrap_or_else(|| other.describe());
                Ok(SelectItem::Computed {
                    expr: other,
                    rendered,
                })
            }
        }
    }

    /// Optional `AS alias` / bare alias after an item.
    pub(super) fn maybe_as_alias(&mut self) -> Result<Option<String>, String> {
        if self.kw("AS") {
            return self.ident().map(Some);
        }
        // bare alias (SELECT COUNT(*) c) — an identifier that starts a new
        // clause only if it is a keyword.
        if let STok::Ident(s) = self.peek() {
            let lower = s.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "from"
                    | "where"
                    | "group"
                    | "having"
                    | "order"
                    | "limit"
                    | "offset"
                    | "join"
                    | "inner"
                    | "left"
                    | "right"
                    | "full"
                    | "outer"
                    | "on"
                    | "and"
                    | "or"
                    | "asc"
                    | "desc"
                    | "union"
                    | "all"
                    | "over"
            ) {
                return Ok(Some(self.ident()?));
            }
        }
        Ok(None)
    }

}
