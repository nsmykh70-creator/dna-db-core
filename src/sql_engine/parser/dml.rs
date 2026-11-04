// SPDX-License-Identifier: AGPL-3.0-only
//! DML parsing: INSERT (incl. IGNORE / REPLACE / ON CONFLICT tails),
//! UPDATE and DELETE.

use super::*;

impl SParser {
    pub(super) fn parse_insert(&mut self, replace: bool) -> Result<Stmt, String> {
        // `INSERT IGNORE INTO …` — MySQL skips uniqueness-conflicting rows.
        let ignore = self.kw("IGNORE");
        self.expect_kw("INTO")?;
        let table = self.ident()?;
        // MySQL form 3: `INSERT INTO t SET col = v, col2 = v2` (OpenCart's
        // installer seeds the admin user this way). Desugared to one row of
        // named values; the ON DUPLICATE KEY UPDATE tail is shared.
        if self.kw("SET") {
            let mut row: Vec<(String, Value)> = Vec::new();
            loop {
                let col = self.ident()?;
                self.expect(STok::Eq)?;
                let v = self.value()?;
                row.push((col, v));
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            let on_conflict = self.parse_on_conflict_tail()?;
            self.eat_semi();
            return Ok(Stmt::Insert {
                table,
                rows: vec![row],
                on_conflict,
                ignore,
            });
        }
        // MySQL allows omitting the column list: `INSERT INTO t VALUES (…)`
        // — values then map to the declared columns positionally. Parse the
        // tuples with empty column names; exec_insert_tx fills them from the
        // schema's declared order (Drupal, plain mysqldump output).
        if !matches!(self.peek(), STok::LParen) {
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
                rows.push(values.into_iter().map(|v| (String::new(), v)).collect());
                if matches!(self.peek(), STok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.eat_semi();
            return Ok(Stmt::Insert {
                table,
                rows,
                on_conflict: None,
                ignore,
            });
        }
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
    pub(super) fn parse_on_conflict_tail(&mut self) -> Result<Option<OnConflict>, String> {
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

    pub(super) fn parse_update(&mut self) -> Result<Stmt, String> {
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

    pub(super) fn parse_delete(&mut self) -> Result<Stmt, String> {
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

}
