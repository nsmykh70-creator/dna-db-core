// SPDX-License-Identifier: AGPL-3.0-only
//! Recursive-descent statement parser: `Vec<STok>` → `Stmt`.
//! Depth-guarded (MAX_PARSE_DEPTH) against hostile nested input.

use super::index_scan::{render_agg_distinct, render_select_sql, render_window, typed_string_literal};
use super::lexer::STok;
use super::lexer::stokenize;
use super::stmt::{AggFunc, JoinSpec, JoinType, OnConflict, SelectItem, Stmt, TxAction, UpsertValue, WindowFunc};
use crate::expr::{ArithOp, CastTy, CmpOp, Expr};
use crate::relational::{ChromosomeTable, ColumnType, ForeignKeyConstraint, SecondaryIndexMeta, Value};

pub(super) struct SParser {
    toks: Vec<STok>,
    pos: usize,
    /// Recursion depth of the expression/select parser. The grammar recurses
    /// once per nested parenthesis (and once per nested subquery), so a
    /// hostile frame full of '(' would overflow the stack and, with the old
    /// panic=abort profile, kill the whole server. Both recursion cycles go
    /// through `parse_or` (expressions) or `parse_select` (subqueries), so
    /// guarding those two bounds the entire grammar.
    depth: usize,
}

/// Maximum nesting accepted by the SQL parser (parenthesised expressions,
/// nested subqueries, or any mix of both). 128 was the original value; the
/// CMS edition added parser branches whose debug-build stack frames made a
/// 128-deep recursion overflow the 2 MiB test-thread stack BEFORE the guard
/// fired, so the limit now sits at a safer 100 (real CMS queries nest < 10).
const MAX_PARSE_DEPTH: usize = 100;

impl SParser {
    /// Tokenize `sql` and build a fresh top-level parser (the constructor
    /// every caller outside this module uses).
    pub(super) fn new(sql: &str) -> Result<Self, String> {
        Ok(Self {
            toks: stokenize(sql)?,
            pos: 0,
            depth: 0,
        })
    }

    /// Enter one recursion level; errors out instead of smashing the stack.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(format!(
                "SQL: expression nested too deeply (limit {})",
                MAX_PARSE_DEPTH
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub(super) fn peek(&self) -> &STok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }
    fn peek_at(&self, n: usize) -> &STok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)]
    }
    fn next(&mut self) -> STok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        self.pos += 1;
        t
    }
    fn kw(&mut self, w: &str) -> bool {
        if let STok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(w) {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn expect_kw(&mut self, w: &str) -> Result<(), String> {
        if self.kw(w) {
            Ok(())
        } else {
            Err(format!("SQL: expected {}", w))
        }
    }
    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            STok::Ident(s) => Ok(s),
            t => Err(format!("SQL: expected identifier, got {:?}", t)),
        }
    }
    fn expect(&mut self, t: STok) -> Result<(), String> {
        if self.peek() == &t {
            self.next();
            Ok(())
        } else {
            Err(format!("SQL: expected {:?}, got {:?}", t, self.peek()))
        }
    }
    /// Consume an optional table alias ('FROM chr_orders o' / 'AS t').
    /// The row model stays single-namespace (qualified refs alias.col are
    /// stripped at parse time); the alias is still RETURNED so `t.*`
    /// projections can resolve it to the underlying table.
    fn maybe_alias(&mut self) -> Option<String> {
        // WordPress always writes explicit aliases: `FROM wp_terms AS t`.
        if self.kw("AS") {
            if let STok::Ident(name) = self.peek().clone() {
                self.next();
                return Some(name);
            }
        }
        if let STok::Ident(s) = self.peek().clone() {
            let lower = s.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "join"
                    | "inner"
                    | "left"
                    | "right"
                    | "full"
                    | "outer"
                    | "cross"
                    | "where"
                    | "limit"
                    | "offset"
                    | "on"
                    | "using"
                    | "group"
                    | "order"
                    | "union"
                    | "all"
                    | "having"
            ) {
                self.next();
                return Some(s.clone());
            }
        }
        None
    }

    fn value(&mut self) -> Result<Value, String> {
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
            t => Err(format!("SQL: expected literal, got {:?}", t)),
        }
    }

    pub(super) fn parse_stmt(&mut self) -> Result<Stmt, String> {
        if self.kw("SELECT") {
            self.parse_select_tail()
        } else if self.kw("WITH") {
            self.parse_cte()
        } else if self.kw("INSERT") {
            self.parse_insert(false)
        } else if self.kw("REPLACE") {
            // CMS edition: MySQL REPLACE INTO t (cols) VALUES … = conflicting
            // rows are deleted and the new row inserted; modelled as a
            // full-row upsert (every listed column updated to EXCLUDED.x).
            self.parse_insert(true)
        } else if self.kw("UPDATE") {
            self.parse_update()
        } else if self.kw("DELETE") {
            self.parse_delete()
        } else if self.kw("CREATE") {
            self.parse_create()
        } else if self.kw("DROP") {
            self.parse_drop()
        } else if self.kw("BEGIN") || self.kw("START") {
            // START TRANSACTION is the fully standard spelling of BEGIN.
            if self.kw("TRANSACTION") {
                self.eat_semi();
            }
            Ok(Stmt::Tx { action: TxAction::Begin })
        } else if self.kw("COMMIT") || self.kw("END") {
            self.eat_semi();
            Ok(Stmt::Tx { action: TxAction::Commit })
        } else if self.kw("ROLLBACK") {
            self.eat_semi();
            Ok(Stmt::Tx { action: TxAction::Rollback })
        } else if self.kw("SHOW") {
            if self.kw("TABLES") || self.kw("CHROMOSOMES") {
                self.eat_semi();
                Ok(Stmt::ShowTables)
            } else if self.kw("STORAGE") {
                // v4.1: SHOW STORAGE FOR <table> [WHERE <pk> = <literal>]
                self.expect_kw("FOR")?;
                let table = self.ident()?;
                let mut pk = None;
                if self.kw("WHERE") {
                    let col = self.ident()?;
                    self.expect(STok::Eq)?;
                    let lit = self.value()?;
                    pk = Some((col, lit));
                }
                self.eat_semi();
                Ok(Stmt::ShowStorage { table, pk })
            } else {
                Err("SQL: unsupported SHOW target (supported: SHOW TABLES / SHOW CHROMOSOMES / SHOW STORAGE FOR <table>)".into())
            }
        } else if self.kw("ALTER") {
            self.expect_kw("TABLE")?;
            let table = self.ident()?;
            if self.kw("DROP") {
                // MySQL revert path: ALTER TABLE t DROP COLUMN c.
                self.kw("COLUMN");
                let column = self.ident()?;
                self.eat_semi();
                return Ok(Stmt::DropColumn { table, column });
            }
            self.expect_kw("ADD")?;
            self.kw("COLUMN");
            let column = self.ident()?;
            let col_type = self.parse_column_type()?;
            // CMS edition: trailing column flags — the DEFAULT literal
            // (or the NOT NULL zero-value) becomes the new column's
            // effective default, exactly like CREATE TABLE.
            #[cfg(feature = "cms")]
            {
                let mut col_default: Option<String> = None;
                let mut not_null = false;
                loop {
                    if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                        continue;
                    }
                    if self.kw("NOT") {
                        if !self.kw("NULL") {
                            return Err("SQL: expected NULL after NOT in column definition".into());
                        }
                        not_null = true;
                        continue;
                    }
                    if self.kw("NULL") {
                        continue;
                    }
                    if self.kw("DEFAULT") {
                        col_default = Some(match self.peek().clone() {
                            STok::Str(s) => {
                                self.next();
                                s
                            }
                            STok::Int(v) => {
                                self.next();
                                v.to_string()
                            }
                            STok::Float(v) => {
                                self.next();
                                v.to_string()
                            }
                            STok::Ident(s) => {
                                self.next();
                                s
                            }
                            t => return Err(format!("SQL: bad DEFAULT value {:?}", t)),
                        });
                        continue;
                    }
                    if self.kw("COMMENT") {
                        self.consume_default_value()?;
                        continue;
                    }
                    break;
                }
                if col_default.is_none() && not_null {
                    col_default = Some(match &col_type {
                        ColumnType::Timestamp => "0000-00-00 00:00:00".to_string(),
                        ColumnType::Text | ColumnType::Dna2Bit => String::new(),
                        ColumnType::Int64 | ColumnType::Float64
                        | ColumnType::ForeignLink { .. } => "0".to_string(),
                    });
                }
                self.eat_semi();
                return Ok(Stmt::AddColumn { table, column, col_type, col_default });
            }
            #[cfg(not(feature = "cms"))]
            {
                let _ = self.kw("NOT") && self.kw("NULL");
                self.eat_semi();
                return Ok(Stmt::AddColumn { table, column, col_type, col_default: None });
            }
        } else if self.kw("EXPLAIN") {
            // kw() already consumed the EXPLAIN token.
            let inner = self.parse_stmt()?;
            Ok(Stmt::Explain(Box::new(inner)))
        } else {
            Err(format!(
                "SQL: unsupported statement starting at {:?}",
                self.peek()
            ))
        }
    }

    /// Consume a statement-terminating `;` if present.
    fn eat_semi(&mut self) {
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
    }

    /// `WITH name AS (SELECT ...) [, name2 AS (...)] body` (v3.6).
    fn parse_cte(&mut self) -> Result<Stmt, String> {
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
    fn parse_select_tail(&mut self) -> Result<Stmt, String> {
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

    fn parse_select(&mut self) -> Result<Stmt, String> {
        self.enter()?;
        let result = self.parse_select_inner();
        self.leave();
        result
    }

    fn parse_select_inner(&mut self) -> Result<Stmt, String> {
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
                    && matches!(
                        self.peek(),
                        STok::Plus | STok::Minus | STok::Star | STok::Slash
                    )
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
    fn parse_select_item(&mut self) -> Result<SelectItem, String> {
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
                    let mut col = self.ident()?;
                    if matches!(self.peek(), STok::Dot) {
                        self.next();
                        col = self.ident()?;
                    }
                    Some(col)
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
    fn maybe_as_alias(&mut self) -> Result<Option<String>, String> {
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

    fn parse_insert(&mut self, replace: bool) -> Result<Stmt, String> {
        // `INSERT IGNORE INTO …` — MySQL skips uniqueness-conflicting rows.
        let ignore = self.kw("IGNORE");
        self.expect_kw("INTO")?;
        let table = self.ident()?;
        self.expect(STok::LParen)?;

        // Form 1 (extended, typed): INSERT INTO t (col = v1, col2 = v2)
        // Form 2 (standard, positional): INSERT INTO t (c1, c2) VALUES (v1, v2), ...
        let first = self.ident()?;
        if matches!(self.peek(), STok::Eq) {
            self.next();
            let v = self.value()?;
            let mut rows = vec![vec![(first, v)]];
            loop {
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
                let col = self.ident()?;
                match self.next() {
                    STok::Eq => {}
                    t => {
                        return Err(format!(
                            "SQL: expected '=' after column '{}', got {:?}",
                            col, t
                        ))
                    }
                }
                let v = self.value()?;
                rows[0].push((col, v));
            }
            self.expect(STok::RParen)?;
            let on_conflict = if replace {
                Some(OnConflict::DoUpdate {
                    assignments: rows[0]
                        .iter()
                        .map(|(c, _)| (c.clone(), UpsertValue::Excluded(c.clone())))
                        .collect(),
                })
            } else {
                self.parse_on_conflict_tail()?
            };
            self.eat_semi();
            return Ok(Stmt::Insert {
                table,
                rows,
                on_conflict,
                ignore,
            });
        }

        // Standard positional form
        let mut cols = vec![first];
        loop {
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
            cols.push(self.ident()?);
        }
        self.expect(STok::RParen)?;
        // CMS edition: INSERT … SELECT — the source rows come from a query
        // (ActionScheduler claim/copy patterns).
        if self.kw("SELECT") {
            let select = self.parse_select_tail()?;
            self.eat_semi();
            return Ok(Stmt::InsertSelect {
                table,
                cols,
                select: Box::new(select),
            });
        }
        self.expect_kw("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(STok::LParen)?;
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
            if values.len() != cols.len() {
                return Err(format!(
                    "SQL: VALUES tuple has {} values but column list has {}",
                    values.len(),
                    cols.len()
                ));
            }
            rows.push(cols.iter().cloned().zip(values).collect::<Vec<_>>());
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        let on_conflict = if replace {
            // REPLACE: the new values replace the conflicting row wholesale.
            Some(OnConflict::DoUpdate {
                assignments: cols
                    .iter()
                    .map(|c| (c.clone(), UpsertValue::Excluded(c.clone())))
                    .collect(),
            })
        } else {
            self.parse_on_conflict_tail()?
        };
        self.eat_semi();
        Ok(Stmt::Insert {
            table,
            rows,
            on_conflict,
            ignore,
        })
    }

    /// v3.6 UPSERT tail: `ON CONFLICT (col) DO NOTHING | DO UPDATE SET ...`.
    fn parse_on_conflict_tail(&mut self) -> Result<Option<OnConflict>, String> {
        if !self.kw("ON") {
            return Ok(None);
        }
        // CMS edition: MySQL `ON DUPLICATE KEY UPDATE col = VALUES(col), …`
        // — the same DO UPDATE machinery as ON CONFLICT, but firing on ANY
        // unique-key conflict (PK or secondary UNIQUE), which exec_insert_tx
        // resolves through the unique-index entry.
        #[cfg(feature = "cms")]
        if self.kw("DUPLICATE") {
            self.expect_kw("KEY")?;
            self.expect_kw("UPDATE")?;
            let mut assignments = Vec::new();
            loop {
                let col = self.ident()?;
                match self.next() {
                    STok::Eq => {}
                    t => {
                        return Err(format!(
                            "SQL: expected '=' in ON DUPLICATE KEY UPDATE, got {:?}",
                            t
                        ))
                    }
                }
                // VALUES(col) — the conflicting row's proposed value.
                if let STok::Ident(s) = self.peek().clone() {
                    if s.eq_ignore_ascii_case("VALUES")
                        && matches!(self.peek_at(1), STok::LParen)
                    {
                        self.next();
                        self.next();
                        let source = self.ident()?;
                        self.expect(STok::RParen)?;
                        assignments.push((col, UpsertValue::Excluded(source)));
                        if matches!(self.peek(), STok::Comma) {
                            self.next();
                            continue;
                        }
                        break;
                    }
                }
                // CMS edition: bare column ref with optional arithmetic —
                // `c = c` or `c = c+1` (the existing row's value; MediaWiki
                // SiteStatsUpdate upserts counters this way).
                if let STok::Ident(src) = self.peek().clone() {
                    self.next();
                    if matches!(self.peek(), STok::Plus) {
                        self.next();
                        let delta = self.value()?;
                        assignments.push((
                            col,
                            UpsertValue::ColumnAdd {
                                column: src,
                                delta,
                            },
                        ));
                    } else {
                        assignments.push((col, UpsertValue::Column(src)));
                    }
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                        continue;
                    }
                    break;
                }
                assignments.push((col, UpsertValue::Literal(self.value()?)));
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            return Ok(Some(OnConflict::DoUpdate { assignments }));
        }
        self.expect_kw("CONFLICT")?;
        self.expect(STok::LParen)?;
        let _target = self.ident()?; // conflict target column (PK expected)
        self.expect(STok::RParen)?;
        self.expect_kw("DO")?;
        if self.kw("NOTHING") {
            return Ok(Some(OnConflict::DoNothing));
        }
        if !self.kw("UPDATE") {
            return Err("SQL: expected DO NOTHING or DO UPDATE after ON CONFLICT".into());
        }
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            match self.next() {
                STok::Eq => {}
                t => return Err(format!("SQL: expected '=' in DO UPDATE SET, got {:?}", t)),
            }
            // Value: EXCLUDED.col (the row that caused the conflict) or a literal.
            if let STok::Ident(s) = self.peek().clone() {
                if s.eq_ignore_ascii_case("EXCLUDED") {
                    self.next();
                    self.expect(STok::Dot)?;
                    let source = self.ident()?;
                    assignments.push((col, UpsertValue::Excluded(source)));
                    if matches!(self.peek(), STok::Comma) {
                        self.next();
                        continue;
                    }
                    break;
                }
            }
            assignments.push((col, UpsertValue::Literal(self.value()?)));
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        Ok(Some(OnConflict::DoUpdate { assignments }))
    }

    fn parse_update(&mut self) -> Result<Stmt, String> {
        let table = self.ident()?;
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            match self.next() {
                STok::Eq => {}
                t => return Err(format!("SQL: expected = in SET, got {:?}", t)),
            }
            // v4.2-cms: rhs may be a column/arithmetic expression — it is
            // evaluated per-row against the pre-update values.
            let v = self.parse_operand()?;
            assignments.push((col, v));
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Update {
            table,
            assignments,
            where_clause,
        })
    }

    fn parse_delete(&mut self) -> Result<Stmt, String> {
        self.expect_kw("FROM")?;
        let table = self.ident()?;
        let where_clause = if self.kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::Delete {
            table,
            where_clause,
        })
    }

    fn parse_create(&mut self) -> Result<Stmt, String> {
        if self.kw("INDEX") {
            return self.parse_create_index(false);
        }
        if self.kw("UNIQUE") {
            self.expect_kw("INDEX")?;
            return self.parse_create_index(true);
        }
        self.expect_kw("TABLE")?;
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let table_name = self.ident()?;
        self.expect(STok::LParen)?;
        let mut t = ChromosomeTable::new(table_name.clone(), "id");
        let mut fks: Vec<ForeignKeyConstraint> = Vec::new();
        loop {
            // CMS edition: MySQL constraint lines inside CREATE TABLE —
            // `KEY name (col(len))`, `INDEX name (col)`, `UNIQUE KEY name (col)`,
            // `FULLTEXT KEY name (col)`. Single-column keys become engine
            // secondary indexes; composite keys index their first column
            // non-uniquely (engine single-PK limitation, CMS_EDITION_RU.md §4).
            #[cfg(feature = "cms")]
            if self.peek_is_cms_constraint() {
                self.parse_cms_table_constraint(&mut t)?;
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                    continue;
                } else {
                    break;
                }
            }
            if self.kw("PRIMARY") {
                self.expect_kw("KEY")?;
                self.expect(STok::LParen)?;
                let pk = self.ident()?;
                // CMS edition: composite PRIMARY KEY (a, b) — the engine keys
                // on the first column for native paths; the full list is
                // retained so row keys encode every key part (MediaWiki
                // l10n_cache, Joomla session tables).
                #[cfg(feature = "cms")]
                {
                    t.primary_key_cols = vec![pk.clone()];
                    while matches!(self.peek(), STok::Comma) {
                        self.next();
                        let part = self.ident()?;
                        self.consume_type_len()?;
                        t.primary_key_cols.push(part);
                    }
                }
                #[cfg(not(feature = "cms"))]
                while matches!(self.peek(), STok::Comma) {
                    self.next();
                    let _ = self.ident()?;
                    self.consume_type_len()?;
                }
                t.primary_locus_column = pk;
                self.expect(STok::RParen)?;
            } else if self.kw("FOREIGN") {
                self.expect_kw("KEY")?;
                self.expect(STok::LParen)?;
                let col = self.ident()?;
                self.expect(STok::RParen)?;
                self.expect_kw("REFERENCES")?;
                let target = self.ident()?;
                self.expect(STok::LParen)?;
                let target_col = self.ident()?;
                self.expect(STok::RParen)?;
                let on_delete_cascade = if self.kw("ON") {
                    self.expect_kw("DELETE")?;
                    if self.kw("CASCADE") {
                        true
                    } else {
                        self.kw("RESTRICT");
                        false
                    }
                } else {
                    false
                };
                fks.push(ForeignKeyConstraint {
                    column: col,
                    target_chromosome: target,
                    target_locus_column: target_col,
                    on_delete_cascade,
                });
            } else {
                let col = self.ident()?;
                let ty = self.parse_column_type()?;
                // CMS edition: MySQL-dialect column flags in any order
                // (UNSIGNED NOT NULL DEFAULT '…' AUTO_INCREMENT …).
                // AUTO_INCREMENT records the sequence column (exec_insert_tx
                // assigns values); lengths/defaults are consumed and dropped.
                #[cfg(feature = "cms")]
                {
                    let mut not_null = false;
                    let mut default: Option<String> = None;
                    loop {
                        if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                            continue;
                        }
                        if self.kw("NOT") {
                            if !self.kw("NULL") {
                                return Err("SQL: expected NULL after NOT in column definition".into());
                            }
                            not_null = true;
                            continue;
                        }
                        if self.kw("NULL") {
                            continue;
                        }
                        if self.kw("AUTO_INCREMENT") {
                            t.auto_increment_column = Some(col.clone());
                            continue;
                        }
                        // MySQL inline key forms after the type: `id BIGINT
                        // AUTO_INCREMENT PRIMARY KEY`, `slug VARCHAR(64)
                        // UNIQUE KEY`, `uid INT KEY`. Inline PRIMARY KEY
                        // promotes the column to the table PK; inline
                        // KEY/UNIQUE KEY go through the secondary-index path
                        // (same as table-level constraints).
                        if self.kw("PRIMARY") {
                            self.expect_kw("KEY")?;
                            t.primary_locus_column = col.clone();
                            #[cfg(feature = "cms")]
                            {
                                t.primary_key_cols = vec![col.clone()];
                            }
                            continue;
                        }
                        if self.kw("UNIQUE") {
                            let _ = self.kw("KEY") || self.kw("INDEX");
                            let idx_name = format!("{}_{}_uniq", t.name, col);
                            t.secondary_indexes.push(SecondaryIndexMeta {
                                name: idx_name,
                                column: col.clone(),
                                unique: true,
                            });
                            continue;
                        }
                        if self.kw("KEY") || self.kw("FULLTEXT") || self.kw("SPATIAL") {
                            let _ = self.kw("KEY") || self.kw("INDEX");
                            let idx_name = format!("{}_{}_idx", t.name, col);
                            t.secondary_indexes.push(SecondaryIndexMeta {
                                name: idx_name,
                                column: col.clone(),
                                unique: false,
                            });
                            continue;
                        }
                        if self.kw("DEFAULT") {
                            default = Some(match self.peek().clone() {
                                STok::Str(s) => {
                                    self.next();
                                    s
                                }
                                STok::Int(v) => {
                                    self.next();
                                    v.to_string()
                                }
                                STok::Float(v) => {
                                    self.next();
                                    v.to_string()
                                }
                                STok::Ident(s) => {
                                    self.next();
                                    s
                                }
                                t => return Err(format!("SQL: bad DEFAULT value {:?}", t)),
                            });
                            continue;
                        }
                        if self.kw("CHARACTER") {
                            self.kw("SET");
                            let _ = self.ident();
                            continue;
                        }
                        if self.kw("COLLATE") {
                            let _ = self.ident();
                            continue;
                        }
                        if self.kw("COMMENT") {
                            self.consume_default_value()?;
                            continue;
                        }
                        break;
                    }
                    // Effective column default (MySQL dialect): the explicit
                    // DEFAULT literal wins; a NOT NULL column without one
                    // gets MySQL's implicit zero-value ('' / 0 / zero-date,
                    // non-strict mode — WordPress hosting default). Nullable
                    // columns stay sparse (NULL) on INSERT. exec_insert_tx
                    // fills omitted columns from this map.
                    let effective = match default {
                        Some(d) => Some(d),
                        None if not_null => Some(match &ty {
                            ColumnType::Timestamp => "0000-00-00 00:00:00".to_string(),
                            ColumnType::Text | ColumnType::Dna2Bit => String::new(),
                            ColumnType::Int64 | ColumnType::Float64
                            | ColumnType::ForeignLink { .. } => "0".to_string(),
                        }),
                        None => None,
                    };
                    if let Some(d) = effective {
                        t.column_defaults.insert(col.clone(), d);
                    }
                }
                #[cfg(not(feature = "cms"))]
                let _ = self.kw("NOT") && self.kw("NULL"); // accepted, not enforced in v2.1
                t.add_column(col, ty);
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(STok::RParen)?;
        // CMS edition: MySQL table options after the column list —
        // `ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4
        // COLLATE=utf8mb4_unicode_520_ci` — consumed and ignored.
        #[cfg(feature = "cms")]
        {
            loop {
                // MySQL separates table options with commas too:
                // `) ENGINE=InnoDB, DEFAULT CHARSET=binary` (MediaWiki's
                // $wgDBTableOptions). Accept both spellings.
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                    continue;
                }
                // `=` is STok::Eq — kw() only matches Ident tokens, so Eq is
                // consumed explicitly everywhere below.
                if self.kw("ENGINE") {
                    self.expect(STok::Eq)?;
                    let _ = self.ident()?;
                } else if self.kw("AUTO_INCREMENT") {
                    self.expect(STok::Eq)?;
                    let _ = self.next();
                } else if self.kw("DEFAULT") {
                    if !(self.kw("CHARSET") || self.kw("COLLATE") || self.kw("CHARACTER")) {
                        return Err("SQL: unsupported DEFAULT table option".into());
                    }
                    self.kw("SET");
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else if self.kw("CHARSET") || self.kw("COLLATE") {
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else if self.kw("CHARACTER") {
                    self.kw("SET");
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.ident()?;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        t.foreign_keys = fks;
        Ok(Stmt::CreateTable {
            table: t,
            if_not_exists,
        })
    }

    /// Column type of a CREATE TABLE column. Without the cms feature this is
    /// the original base-type grammar; with cms it also accepts the
    /// MySQL-dialect names WordPress DDL uses (bigint/tinyint/datetime/
    /// longtext/decimal/...), consuming optional `(n)` lengths.
    fn parse_column_type(&mut self) -> Result<ColumnType, String> {
        if self.kw("INT") || self.kw("INTEGER") || self.kw("INT64") {
            #[cfg(feature = "cms")]
            self.consume_type_len()?;
            Ok(ColumnType::Int64)
        } else if self.kw("FLOAT") || self.kw("FLOAT64") || self.kw("DOUBLE") {
            // MySQL synonym: `DOUBLE PRECISION` (MediaWiki page_random).
            self.kw("PRECISION");
            Ok(ColumnType::Float64)
        } else if self.kw("TEXT") || self.kw("VARCHAR") || self.kw("STRING") {
            #[cfg(feature = "cms")]
            self.consume_type_len()?; // VARCHAR(191), TEXT(65535)
            Ok(ColumnType::Text)
        } else if self.kw("DNA") || self.kw("DNA2BIT") {
            Ok(ColumnType::Dna2Bit)
        } else if self.kw("TIMESTAMP") {
            Ok(ColumnType::Timestamp)
        } else {
            // CMS edition: MySQL-dialect names map onto base engine types.
            #[cfg(feature = "cms")]
            {
                if self.kw("BIGINT")
                    || self.kw("TINYINT")
                    || self.kw("SMALLINT")
                    || self.kw("MEDIUMINT")
                {
                    self.consume_type_len()?;
                    return Ok(ColumnType::Int64);
                }
                if self.kw("DATETIME") || self.kw("DATE") || self.kw("TIME") {
                    return Ok(ColumnType::Timestamp);
                }
                if self.kw("ENUM") || self.kw("SET") {
                    // ENUM('a','b') / SET('x','y') — consume the balanced
                    // value-list parens wholesale (they contain strings).
                    if matches!(self.peek(), STok::LParen) {
                        let mut depth = 0usize;
                        loop {
                            match self.next() {
                                STok::LParen => depth += 1,
                                STok::RParen => {
                                    depth -= 1;
                                    if depth == 0 {
                                        break;
                                    }
                                }
                                STok::Eof => break,
                                _ => {}
                            }
                        }
                    }
                    return Ok(ColumnType::Text);
                }
                if self.kw("LONGTEXT")
                    || self.kw("MEDIUMTEXT")
                    || self.kw("TINYTEXT")
                    || self.kw("CHAR")
                    || self.kw("BLOB")
                    // MediaWiki 1.43 schema is BINARY-first: VARBINARY(32)
                    // keys, BINARY(14) timestamps and *_blob fields all
                    // round-trip as engine text/blob.
                    || self.kw("VARBINARY")
                    || self.kw("BINARY")
                    || self.kw("TINYBLOB")
                    || self.kw("MEDIUMBLOB")
                    || self.kw("LONGBLOB")
                {
                    self.consume_type_len()?;
                    return Ok(ColumnType::Text);
                }
                if self.kw("DECIMAL") || self.kw("NUMERIC") || self.kw("REAL") {
                    self.consume_type_len()?; // DECIMAL(10,2)
                    return Ok(ColumnType::Float64);
                }
            }
            Err("SQL: unknown column type".into())
        }
    }

    /// Consumes an optional `(n)` / `(n,m)` type length (cms only).
    #[cfg(feature = "cms")]
    fn consume_type_len(&mut self) -> Result<(), String> {
        if matches!(self.peek(), STok::LParen) {
            self.next();
            match self.next() {
                STok::Int(_) | STok::Float(_) => {}
                t => return Err(format!("SQL: expected length after '(', got {:?}", t)),
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
                match self.next() {
                    STok::Int(_) | STok::Float(_) => {}
                    t => return Err(format!("SQL: expected precision after ',', got {:?}", t)),
                }
            }
            self.expect(STok::RParen)?;
        }
        Ok(())
    }

    /// Consumes a DEFAULT/COMMENT value literal: string, number, or bare
    /// identifier (NULL / CURRENT_TIMESTAMP) (cms only).
    #[cfg(feature = "cms")]
    fn consume_default_value(&mut self) -> Result<(), String> {
        match self.peek() {
            STok::Str(_) | STok::Int(_) | STok::Float(_) | STok::Ident(_) => {
                let _ = self.next();
                Ok(())
            }
            t => Err(format!("SQL: bad DEFAULT value {:?}", t)),
        }
    }

    /// True when the next token starts a MySQL constraint line inside
    /// CREATE TABLE (cms only): KEY / INDEX / UNIQUE / FULLTEXT / SPATIAL.
    #[cfg(feature = "cms")]
    fn peek_is_cms_constraint(&self) -> bool {
        matches!(self.peek(), STok::Ident(w) if matches!(
            w.to_ascii_uppercase().as_str(),
            "KEY" | "INDEX" | "UNIQUE" | "FULLTEXT" | "SPATIAL"
        ))
    }

    /// Parses one MySQL constraint line into the table's secondary indexes
    /// (cms only). Composite keys keep only the first column (non-unique) —
    /// a unique composite would reject valid rows under the single-column
    /// index model.
    #[cfg(feature = "cms")]
    fn parse_cms_table_constraint(&mut self, t: &mut ChromosomeTable) -> Result<(), String> {
        let unique = self.kw("UNIQUE");
        let fulltext = self.kw("FULLTEXT") || self.kw("SPATIAL");
        // Optional second keyword: UNIQUE KEY / UNIQUE INDEX / FULLTEXT KEY /
        // plain KEY / INDEX.
        let _ = self.kw("KEY") || self.kw("INDEX");
        // Optional index name (absent when the column list starts directly).
        let mut name = String::new();
        if !matches!(self.peek(), STok::LParen) {
            name = self.ident()?;
        }
        self.expect(STok::LParen)?;
        let mut first_col = String::new();
        let mut composite = false;
        loop {
            let c = self.ident()?;
            self.consume_type_len()?; // (191) prefix length — dropped
            if first_col.is_empty() {
                first_col = c;
            } else {
                composite = true;
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(STok::RParen)?;
        if first_col.is_empty() {
            return Err("SQL: empty key column list".into());
        }
        if name.is_empty() {
            name = format!("{}_{}_idx", t.name, first_col);
        }
        let enforced_unique = unique && !composite && !fulltext;
        t.secondary_indexes.push(SecondaryIndexMeta {
            name,
            column: first_col,
            unique: enforced_unique,
        });
        Ok(())
    }

    /// CREATE [UNIQUE handled above] INDEX [IF NOT EXISTS] name ON table (col)
    fn parse_create_index(&mut self, unique: bool) -> Result<Stmt, String> {
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        self.expect_kw("ON")?;
        let table = self.ident()?;
        self.expect(STok::LParen)?;
        let column = self.ident()?;
        self.expect(STok::RParen)?;
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        })
    }

    fn parse_drop(&mut self) -> Result<Stmt, String> {
        if self.kw("INDEX") {
            let if_exists = if self.kw("IF") {
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            // Documented form: DROP INDEX <name> ON <table>. The bare form
            // without ON stays accepted (used by existing scripts/tests).
            let table = if self.kw("ON") {
                Some(self.ident()?)
            } else {
                None
            };
            if matches!(self.peek(), STok::Semi) {
                self.next();
            }
            return Ok(Stmt::DropIndex {
                name,
                if_exists,
                table,
            });
        }
        self.expect_kw("TABLE")?;
        let if_exists = if self.kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        let table = self.ident()?;
        if matches!(self.peek(), STok::Semi) {
            self.next();
        }
        Ok(Stmt::DropTable { table, if_exists })
    }

    // expression parsing (same shape as DnaQL)
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }
    fn parse_or(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let result = self.parse_or_inner();
        self.leave();
        result
    }
    fn parse_or_inner(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.kw("OR") {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_cmp()?;
        while self.kw("AND") {
            let right = self.parse_cmp()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn parse_cmp(&mut self) -> Result<Expr, String> {
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
    fn like_pattern(&mut self) -> Result<String, String> {
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
    fn parse_operand(&mut self) -> Result<Expr, String> {
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
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
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
    fn parse_primary(&mut self) -> Result<Expr, String> {
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

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------
