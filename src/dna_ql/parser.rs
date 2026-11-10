// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL recursive-descent parser: `Parser` plus the full grammar —
//! quick/standard search, pipelines, TX/SYNTHESIZE, ANALYZE, and the
//! VERIFY/REPAIR / CREATE/DROP CHROMOSOME statements.

use super::*;

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub(crate) struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// Recursion depth of the expression parser. The parser is a
    /// recursive-descent tree walk, so a frame full of nested parentheses
    /// would otherwise recurse once per '(' and smash the stack (SIGSEGV or,
    /// with the old panic=abort profile, a whole-process abort). All
    /// recursion cycles go through `parse_or`, so guarding it bounds the
    /// whole grammar.
    depth: usize,
}

/// Maximum expression nesting accepted by the DnaQL parser.
const MAX_PARSE_DEPTH: usize = 128;

impl Parser {
    pub(crate) fn new(toks: Vec<Tok>) -> Self {
        Self {
            toks,
            pos: 0,
            depth: 0,
        }
    }

    /// Enter one expression-recursion level. Errors out (instead of
    /// overflowing the stack) when the query nests too deeply.
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(format!(
                "DnaQL: expression nested too deeply (limit {})",
                MAX_PARSE_DEPTH
            ));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        self.pos += 1;
        t
    }

    fn kw(&mut self, word: &str) -> bool {
        if let Tok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(word) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn expect_kw(&mut self, word: &str) -> Result<(), String> {
        if self.kw(word) {
            Ok(())
        } else {
            Err(format!("DnaQL: expected '{}', got {:?}", word, self.peek()))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("DnaQL: expected identifier, got {:?}", t)),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        match self.next() {
            Tok::Int(i) => Ok(i),
            t => Err(format!("DnaQL: expected integer, got {:?}", t)),
        }
    }

    fn str_lit(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Str(s) => Ok(s),
            t => Err(format!("DnaQL: expected string literal, got {:?}", t)),
        }
    }

    fn expect(&mut self, t: Tok) -> Result<(), String> {
        if self.peek() == &t {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("DnaQL: expected {:?}, got {:?}", t, self.peek()))
        }
    }

    pub(crate) fn parse_query(&mut self) -> Result<DnaQlAst, String> {
        let ast = match self.peek().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("FIND") => self.parse_standard_search(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("FROM") => self.parse_pipeline(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TX") => self.parse_tx(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SYNTHESIZE") => self.parse_synthesize(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ANALYZE") => self.parse_bio(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SHOW") => self.parse_show(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ALTER") => self.parse_alter_passthrough(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("VERIFY") => self.parse_health_check(false),
            Tok::Ident(s) if s.eq_ignore_ascii_case("REPAIR") => self.parse_health_check(true),
            Tok::Ident(s) if s.eq_ignore_ascii_case("CREATE") => self.parse_create_chromosome(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("DROP") => self.parse_drop_chromosome(),
            Tok::Ident(_) => self.parse_quick_search(),
            t => Err(format!("DnaQL: unexpected token {:?}", t)),
        }?;
        // Strict end-of-query: trailing tokens used to be silently ignored,
        // which could truncate a malformed statement into a different one
        // (`EMIT a, b * 10` losing the arithmetic) — reject instead.
        if !matches!(self.peek(), Tok::Eof) {
            return Err(format!(
                "DnaQL: unexpected trailing tokens starting at {:?}",
                self.peek()
            ));
        }
        Ok(ast)
    }

    /// VERIFY CHROMOSOME <name>;  /  REPAIR CHROMOSOME <name>;
    /// Block-level integrity scan of every segment holding keys of the
    /// chromosome. REPAIR rewrites damaged shards from parity on the fly.
    fn parse_health_check(&mut self, fix: bool) -> Result<DnaQlAst, String> {
        self.next(); // VERIFY / REPAIR
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after {}, got {:?}",
                if fix { "REPAIR" } else { "VERIFY" },
                kw
            ));
        }
        let chromosome = self.ident()?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::HealthCheck { chromosome, fix })
    }

    /// CREATE CHROMOSOME <name> [IF NOT EXISTS] (col TYPE, ...);
    /// The parenthesis body is collected as raw definition clauses at top
    /// comma level: "id INT", "PRIMARY KEY (id)", "FOREIGN KEY ...
    /// REFERENCES ..." with nested parens kept intact. Types and constraints
    /// are validated when the rendered CREATE TABLE runs through the SQL
    /// engine. Without an explicit PRIMARY KEY the first column is the locus.
    fn parse_create_chromosome(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // CREATE
        // v4.3: CREATE [UNIQUE] INDEX name ON chr (col) — SQL DDL passthrough
        // (indexes live in the SQL engine's schema, so the statement renders
        // to SQL and executes there, exactly like CREATE CHROMOSOME).
        if self.kw("UNIQUE") {
            self.expect_kw("INDEX")?;
            return self.parse_index_passthrough(true);
        }
        if self.kw("INDEX") {
            return self.parse_index_passthrough(false);
        }
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after CREATE, got {:?}",
                kw
            ));
        }
        let table = self.ident()?;
        let if_not_exists = if self.kw("IF") {
            self.expect_kw("NOT")?;
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        self.expect(Tok::LParen)?;
        let mut definitions: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut depth = 0usize;
        loop {
            match self.peek().clone() {
                Tok::RParen if depth == 0 => {
                    self.next();
                    break;
                }
                Tok::RParen => {
                    self.next();
                    depth -= 1;
                    push_clause_token(&mut buf, ")");
                }
                Tok::LParen => {
                    self.next();
                    depth += 1;
                    push_clause_token(&mut buf, "(");
                }
                Tok::Comma if depth == 0 => {
                    self.next();
                    let clause = buf.trim().to_string();
                    if !clause.is_empty() {
                        definitions.push(clause);
                    }
                    buf.clear();
                }
                Tok::Ident(s) => {
                    self.next();
                    push_clause_token(&mut buf, &s);
                }
                Tok::Int(i) => {
                    self.next();
                    push_clause_token(&mut buf, &i.to_string());
                }
                Tok::Float(f) => {
                    self.next();
                    push_clause_token(&mut buf, &float_token(&f));
                }
                Tok::Str(s) => {
                    self.next();
                    push_clause_token(&mut buf, &format!("'{}'", s));
                }
                Tok::Star => {
                    self.next();
                    push_clause_token(&mut buf, "*");
                }
                Tok::Dot => {
                    self.next();
                    push_clause_token(&mut buf, ".");
                }
                Tok::Semicolon if depth == 0 => {
                    self.next();
                    break;
                }
                t => {
                    return Err(format!(
                        "DnaQL: unexpected token {:?} in CREATE CHROMOSOME column list",
                        t
                    ))
                }
            }
        }
        let tail = buf.trim().to_string();
        if !tail.is_empty() {
            definitions.push(tail);
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        if definitions.is_empty() {
            return Err("DnaQL: CREATE CHROMOSOME needs at least one column".into());
        }
        Ok(DnaQlAst::CreateChromosome {
            table,
            definitions,
            if_not_exists,
        })
    }

    /// DROP CHROMOSOME <name> [IF EXISTS];
    fn parse_drop_chromosome(&mut self) -> Result<DnaQlAst, String> {
        self.next(); // DROP
        // v4.3: DROP INDEX name [ON chr] — SQL DDL passthrough.
        if self.kw("INDEX") {
            let if_exists = if self.kw("IF") {
                self.expect_kw("EXISTS")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            let mut tables = Vec::new();
            let mut sql = format!("DROP INDEX{} {}", if if_exists { " IF EXISTS" } else { "" }, name);
            if self.kw("ON") {
                let t = self.ident()?;
                tables.push(t.clone());
                sql.push_str(&format!(" ON {}", t));
            }
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            sql.push(';');
            return Ok(DnaQlAst::DdlPassthrough { sql, tables });
        }
        let kw = self.ident()?;
        if !kw.eq_ignore_ascii_case("CHROMOSOME") {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after DROP, got {:?}",
                kw
            ));
        }
        let table = self.ident()?;
        let if_exists = if self.kw("IF") {
            self.expect_kw("EXISTS")?;
            true
        } else {
            false
        };
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::DropChromosome { table, if_exists })
    }

    // Quick: chr ? 'PAT' ~K -> chr2(col) => emit a,b ;
    fn parse_quick_search(&mut self) -> Result<DnaQlAst, String> {
        let chromosome = self.ident()?;
        self.expect(Tok::Question)?;
        let pattern = self.str_lit()?;

        let mode = DistanceMode::Levenshtein;
        let mut max_distance = 1usize;
        if matches!(self.peek(), Tok::Tilde) || matches!(self.peek(), Tok::Star) {
            self.next();
            max_distance = self.int()? as usize;
        }

        let mut link = None;
        if matches!(self.peek(), Tok::Arrow) {
            self.next();
            let chr2 = self.ident()?;
            let mut col = "user_id".to_string();
            if matches!(self.peek(), Tok::LParen) {
                self.next();
                col = self.ident()?;
                self.expect(Tok::RParen)?;
            }
            link = Some(LinkSpec {
                chromosome: chr2,
                column: col,
            });
        }

        let mut projection = Vec::new();
        if matches!(self.peek(), Tok::FatArrow) {
            self.next();
            self.expect_kw("emit")?;
            projection = self.parse_projection()?;
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }

        Ok(DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            projection,
        })
    }

    // Standard: FIND [PATTERN] 'PAT' IN [CHROMOSOME] chr
    //   [WHERE HAMMING_DISTANCE <= K] [LINK chr2 VIA COMPLEMENT(col)] [EMIT ...] ;
    fn parse_standard_search(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("FIND")?;
        self.kw("PATTERN");
        let pattern = self.str_lit()?;
        self.expect_kw("IN")?;
        self.kw("CHROMOSOME");
        let chromosome = self.ident()?;

        let mut mode = DistanceMode::Levenshtein;
        let mut max_distance = 1usize;
        if self.kw("WHERE") {
            if self.kw("HAMMING_DISTANCE") {
                mode = DistanceMode::Hamming;
            } else if self.kw("LEVENSHTEIN_DISTANCE") {
                mode = DistanceMode::Levenshtein;
            } else {
                return Err("DnaQL: expected HAMMING_DISTANCE or LEVENSHTEIN_DISTANCE".into());
            }
            self.expect(Tok::Le)?;
            max_distance = self.int()? as usize;
        }

        let mut link = None;
        if self.kw("LINK") {
            let chr2 = self.ident()?;
            self.expect_kw("VIA")?;
            self.expect_kw("COMPLEMENT")?;
            self.expect(Tok::LParen)?;
            let col = self.ident()?;
            self.expect(Tok::RParen)?;
            link = Some(LinkSpec {
                chromosome: chr2,
                column: col,
            });
        }

        let mut projection = Vec::new();
        if self.kw("EMIT") {
            projection = self.parse_projection()?;
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }

        Ok(DnaQlAst::PatternSearch {
            chromosome,
            pattern,
            mode,
            max_distance,
            link,
            projection,
        })
    }

    /// `EMIT item (, item)*` — items: `*` | column [AS alias] | expr [AS alias].
    /// An identifier not followed by `(` is a column; anything else starts a
    /// computed expression. Without an alias a computed item renders under
    /// its canonical expression text.
    fn parse_projection(&mut self) -> Result<Vec<EmitItem>, String> {
        let mut out = Vec::new();
        if matches!(self.peek(), Tok::Star) {
            self.next();
            out.push(EmitItem::All); // All = every field
            return Ok(out);
        }
        loop {
            // A bare column is an identifier followed by a delimiter (Comma /
            // AS / end) — anything else (`user_id * 10`, `COUNT(*)`, a
            // literal) starts a computed expression.
            let column_shaped = matches!(self.peek(), Tok::Ident(_))
                && matches!(
                    self.toks.get(self.pos + 1),
                    None | Some(Tok::Comma) | Some(Tok::Semicolon) | Some(Tok::Eof)
                )
                || (matches!(self.peek(), Tok::Ident(_))
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("AS")));
            if column_shaped {
                let name = self.ident()?;
                let alias = if self.kw("AS") {
                    Some(self.ident()?)
                } else {
                    None
                };
                out.push(EmitItem::Column { name, alias });
            } else {
                let expr = self.parse_or()?;
                let alias = if self.kw("AS") {
                    Some(self.ident()?)
                } else {
                    None
                };
                out.push(EmitItem::Computed { expr, alias });
            }
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        Ok(out)
    }

    // FROM chr |> LINK chr2 VIA COMPLEMENT(col) |> WHERE expr |> EMIT a,b
    fn parse_pipeline(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("FROM")?;
        let source = self.ident()?;
        let mut stages = Vec::new();
        while matches!(self.peek(), Tok::Pipe) {
            self.next();
            if self.kw("LINK") {
                let chr2 = self.ident()?;
                let mut spec = LinkSpec {
                    chromosome: chr2,
                    column: "user_id".to_string(),
                };
                if self.kw("VIA") {
                    self.expect_kw("COMPLEMENT")?;
                    self.expect(Tok::LParen)?;
                    spec.column = self.ident()?;
                    self.expect(Tok::RParen)?;
                }
                stages.push(PipelineStage::Link(Some(spec)));
            } else if self.kw("WHERE") {
                let e = self.parse_expr()?;
                stages.push(PipelineStage::Filter(e));
            } else if self.kw("EMIT") {
                let p = self.parse_projection()?;
                stages.push(PipelineStage::Emit(p));
            } else if self.kw("SORT") {
                // v4.3: SORT accepts a full expression — a bare identifier
                // stays the classic column sort, anything else becomes a
                // computed sort key (SQL ORDER BY meta_value+0).
                let key = self.parse_or()?;
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                match key {
                    Expr::Column(c) => stages.push(PipelineStage::Sort { column: c, asc }),
                    e => stages.push(PipelineStage::SortBy { expr: e, asc }),
                }
            } else if self.kw("LIMIT") {
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: LIMIT expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Limit(n as usize));
            } else if self.kw("SKIP") {
                // v4.3: SQL OFFSET — drop the first n rows of the stream.
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: SKIP expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Skip(n as usize));
            } else if matches!(self.peek(), Tok::Ident(s)
                if matches!(s.to_ascii_uppercase().as_str(), "LEFT" | "INNER" | "JOIN"))
            {
                // v4.3: JOIN / INNER JOIN / LEFT [OUTER] JOIN chr2
                //   ON left_operand = right_operand [AND extra]*
                let left_outer = self.kw("LEFT");
                if left_outer {
                    self.kw("OUTER");
                } else {
                    self.kw("INNER");
                }
                self.expect_kw("JOIN")?;
                let chromosome = self.ident()?;
                self.expect_kw("ON")?;
                let on_left = self.parse_operand()?;
                self.expect(Tok::Eq)?;
                let on_right = self.parse_operand()?;
                let mut on_extra = Vec::new();
                while self.kw("AND") {
                    on_extra.push(self.parse_or()?);
                }
                stages.push(PipelineStage::Join(JoinStage {
                    chromosome,
                    left_outer,
                    on_left,
                    on_right,
                    on_extra,
                }));
            } else if self.kw("GROUP") {
                // v4.3: GROUP [BY] col, ... , AGG(expr) [AS a], ... [HAVING e]
                // Group columns and aggregate calls share one comma list —
                // an aggregate is IDENT '(' , a group column is a bare ident.
                self.kw("BY");
                let mut by = Vec::new();
                let mut aggs = Vec::new();
                loop {
                    if let Some(spec) = self.try_parse_agg()? {
                        aggs.push(spec);
                    } else {
                        by.push(self.ident()?);
                    }
                    if matches!(self.peek(), Tok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
                let having = if self.kw("HAVING") {
                    Some(self.parse_or()?)
                } else {
                    None
                };
                stages.push(PipelineStage::Group(GroupStage { by, aggs, having }));
            } else {
                return Err(format!("DnaQL: unknown pipeline stage {:?}", self.peek()));
            }
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::PipelineQuery { source, stages })
    }

    /// Try to parse `AGG(arg) [AS alias]` at the current position. Returns
    /// None (without consuming) when the next tokens are not an aggregate
    /// call — a bare group column.
    fn try_parse_agg(&mut self) -> Result<Option<AggSpec>, String> {
        let (name, next_is_paren) = match self.peek().clone() {
            Tok::Ident(s) => (
                s,
                matches!(self.toks.get(self.pos + 1), Some(Tok::LParen)),
            ),
            _ => return Ok(None),
        };
        if !next_is_paren {
            return Ok(None);
        }
        let func = if name.eq_ignore_ascii_case("COUNT") {
            DnaAggFunc::Count
        } else if name.eq_ignore_ascii_case("SUM") {
            DnaAggFunc::Sum
        } else if name.eq_ignore_ascii_case("AVG") {
            DnaAggFunc::Avg
        } else if name.eq_ignore_ascii_case("MIN") {
            DnaAggFunc::Min
        } else if name.eq_ignore_ascii_case("MAX") {
            DnaAggFunc::Max
        } else {
            // Unknown function: not an aggregate, not a group column —
            // fail with a clear message.
            return Err(format!("DnaQL: unknown aggregate function '{}'", name));
        };
        self.next(); // fn name
        self.next(); // (
        let arg = if matches!(self.peek(), Tok::Star) {
            self.next();
            if func != DnaAggFunc::Count {
                return Err(format!(
                    "DnaQL: {}(*) is not defined — only COUNT takes '*'",
                    name
                ));
            }
            None
        } else {
            Some(self.parse_or()?)
        };
        self.expect(Tok::RParen)?;
        let alias = if self.kw("AS") {
            Some(self.ident()?)
        } else {
            None
        };
        Ok(Some(AggSpec { func, arg, alias }))
    }

    /// `SHOW CHROMOSOMES [LIKE 'pattern'];` — chromosome name list from the
    /// shared SQL-engine schema cache (v4.3; the SQL→DnaQL transpiler maps
    /// `SHOW TABLES` here).
    fn parse_show(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("SHOW")?;
        self.expect_kw("CHROMOSOMES")?;
        let pattern = if self.kw("LIKE") {
            Some(self.str_lit()?)
        } else {
            None
        };
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::ShowChromosomes { pattern })
    }

    /// CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON chr (col) — rendered
    /// to the exact SQL statement and executed through the shared engine.
    fn parse_index_passthrough(&mut self, unique: bool) -> Result<DnaQlAst, String> {
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
        self.expect(Tok::LParen)?;
        let column = self.ident()?;
        self.expect(Tok::RParen)?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        let mut sql = String::from("CREATE ");
        if unique {
            sql.push_str("UNIQUE ");
        }
        if if_not_exists {
            sql.push_str("INDEX IF NOT EXISTS ");
        } else {
            sql.push_str("INDEX ");
        }
        sql.push_str(&format!("{} ON {} ({})", name, table, column));
        sql.push(';');
        Ok(DnaQlAst::DdlPassthrough {
            sql,
            tables: vec![table],
        })
    }

    /// ALTER CHROMOSOME (or the SQL spelling ALTER TABLE) — ADD/DROP/
    /// MODIFY/CHANGE COLUMN passthrough. The column tail (type + flags) is
    /// captured verbatim, like the CREATE CHROMOSOME clause list.
    fn parse_alter_passthrough(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("ALTER")?;
        let kw = self.ident()?;
        if !(kw.eq_ignore_ascii_case("CHROMOSOME") || kw.eq_ignore_ascii_case("TABLE")) {
            return Err(format!(
                "DnaQL: expected CHROMOSOME after ALTER, got {:?}",
                kw
            ));
        }
        let table = self.ident()?;
        if self.kw("DROP") {
            self.kw("COLUMN");
            let column = self.ident()?;
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::DdlPassthrough {
                sql: format!("ALTER TABLE {} DROP COLUMN {};", table, column),
                tables: vec![table],
            });
        }
        // Online type change, SQL-spelling parity:
        //   MODIFY [COLUMN] c <type+flags>
        //   CHANGE [COLUMN] old new <type+flags>
        if self.kw("MODIFY") {
            self.kw("COLUMN");
            let column = self.ident()?;
            let tail = self.consume_alter_column_tail()?;
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::DdlPassthrough {
                sql: format!(
                    "ALTER TABLE {} MODIFY COLUMN {} {};",
                    table, column, tail
                ),
                tables: vec![table],
            });
        }
        if self.kw("CHANGE") {
            self.kw("COLUMN");
            let old = self.ident()?;
            let new = self.ident()?;
            let tail = self.consume_alter_column_tail()?;
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
            return Ok(DnaQlAst::DdlPassthrough {
                sql: format!(
                    "ALTER TABLE {} CHANGE COLUMN {} {} {};",
                    table, old, new, tail
                ),
                tables: vec![table],
            });
        }
        self.expect_kw("ADD")?;
        self.kw("COLUMN");
        let column = self.ident()?;
        // Type + flags: raw token text until ';' / end (INT NOT NULL
        // DEFAULT 'x', VARCHAR(191) UNSIGNED, ...).
        let tail = self.consume_alter_column_tail()?;
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::DdlPassthrough {
            sql: format!(
                "ALTER TABLE {} ADD COLUMN {} {};",
                table, column, tail
            ),
            tables: vec![table],
        })
    }

    /// Raw column-definition tail of an ALTER clause: type + flags until
    /// ';' / end (INT NOT NULL DEFAULT 'x', VARCHAR(191) UNSIGNED, ...).
    fn consume_alter_column_tail(&mut self) -> Result<String, String> {
        let mut tail = String::new();
        loop {
            match self.peek().clone() {
                Tok::Semicolon | Tok::Eof => break,
                Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Ident(_)
                | Tok::LParen | Tok::RParen | Tok::Comma => {
                    let t = self.next();
                    if !tail.is_empty() {
                        tail.push(' ');
                    }
                    tail.push_str(&token_text(&t));
                }
                t => {
                    return Err(format!(
                        "DnaQL: unexpected token {:?} in ALTER ... COLUMN tail",
                        t
                    ))
                }
            }
        }
        Ok(tail)
    }

    // TX { stmt; stmt; ... }
    fn parse_tx(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("TX")?;
        self.expect(Tok::LBrace)?;
        let mut statements = Vec::new();
        loop {
            if matches!(self.peek(), Tok::RBrace) {
                self.next();
                break;
            }
            statements.push(self.parse_tx_stmt()?);
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            } else if !matches!(self.peek(), Tok::RBrace) {
                return Err("DnaQL: expected ';' or '}' in TX block".into());
            }
        }
        // The transpiler renders `TX { ... };` — consume the statement
        // terminator so the strict end-of-query check stays satisfied.
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::Tx { statements })
    }

    // SYNTHESIZE INTO a, b BEGIN stmt; stmt LIGATE
    fn parse_synthesize(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("SYNTHESIZE")?;
        self.expect_kw("INTO")?;
        let mut _targets = Vec::new();
        loop {
            _targets.push(self.ident()?);
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect_kw("BEGIN")?;
        let mut statements = Vec::new();
        loop {
            if self.kw("LIGATE") || self.kw("HYDROLYZE") {
                break;
            }
            statements.push(self.parse_tx_stmt()?);
            if matches!(self.peek(), Tok::Semicolon) {
                self.next();
            }
        }
        // The transpiler renders `TX { ... };` — consume the statement
        // terminator so the strict end-of-query check stays satisfied.
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::Tx { statements })
    }

    fn parse_tx_stmt(&mut self) -> Result<TxStmt, String> {
        if self.kw("DROP") {
            self.expect_kw("LOCUS")?;
            let table = self.ident()?;
            self.expect(Tok::Colon)?;
            let locus = self.int()?;
            let cascade = self.kw("CASCADE");
            return Ok(TxStmt::DropLocus {
                table,
                locus,
                cascade,
            });
        }
        let table = self.ident()?;

        if matches!(self.peek(), Tok::PlusEq) {
            // chr += { k: v, ... } — auto-locus from the PK-like field (*_id / id)
            self.next();
            self.expect(Tok::LBrace)?;
            let mut fields = Vec::new();
            loop {
                if matches!(self.peek(), Tok::RBrace) {
                    self.next();
                    break;
                }
                let k = self.ident()?;
                self.expect(Tok::Colon)?;
                let v = self.parse_value()?;
                fields.push((k, v));
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                }
            }
            let locus = fields
                .iter()
                .find(|(k, v)| (k == "id" || k.ends_with("_id")) && matches!(v, Value::Int(_)))
                .and_then(|(_, v)| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .ok_or("DnaQL: append without explicit locus needs an integer '<...>_id' field")?;
            return Ok(TxStmt::Append {
                table,
                locus,
                fields,
            });
        }

        self.expect(Tok::Colon)?;
        let locus = self.int()?;

        if matches!(self.peek(), Tok::PlusEq) {
            // chr:locus += { k: v, ... }
            self.next();
            self.expect(Tok::LBrace)?;
            let mut fields = Vec::new();
            loop {
                if matches!(self.peek(), Tok::RBrace) {
                    self.next();
                    break;
                }
                let k = self.ident()?;
                self.expect(Tok::Colon)?;
                let v = self.parse_value()?;
                fields.push((k, v));
                if matches!(self.peek(), Tok::Comma) {
                    self.next();
                }
            }
            return Ok(TxStmt::Append {
                table,
                locus,
                fields,
            });
        }

        // field path: .field
        self.expect(Tok::Dot)?;
        let field = self.ident()?;
        let op = match self.next() {
            Tok::Eq => AssignOp::Set,
            Tok::PlusEq => AssignOp::Add,
            Tok::MinusEq => AssignOp::Sub,
            t => return Err(format!("DnaQL: expected '=', '+=' or '-=', got {:?}", t)),
        };
        let value = self.parse_value()?;
        Ok(TxStmt::Mutate {
            table,
            locus,
            field,
            op,
            value,
        })
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.next() {
            Tok::Minus => match self.next() {
                Tok::Int(i) => i
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| "DnaQL: integer literal out of range".to_string()),
                Tok::Float(f) => Ok(Value::Float(-f)),
                t => Err(format!("DnaQL: expected number after '-', got {:?}", t)),
            },
            Tok::Int(i) => Ok(Value::Int(i)),
            Tok::Float(f) => Ok(Value::Float(f)),
            Tok::Str(s) => {
                // DNA sequences: all-ACGT strings become Value::Dna
                if !s.is_empty()
                    && s.chars()
                        .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'))
                {
                    Ok(Value::Dna(s.to_uppercase()))
                } else {
                    Ok(Value::Text(s))
                }
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("NULL") => Ok(Value::Null),
            t => Err(format!("DnaQL: expected literal, got {:?}", t)),
        }
    }

    // ANALYZE SEQUENCE 'ACGT' |> GC_CONTENT() |> ...
    fn parse_bio(&mut self) -> Result<DnaQlAst, String> {
        self.expect_kw("ANALYZE")?;
        let (sequence, locus) = if self.kw("SEQUENCE") {
            (Some(self.str_lit()?), None)
        } else if self.kw("LOCUS") {
            let table = self.ident()?;
            self.expect(Tok::Colon)?;
            let id = self.int()?;
            (None, Some((table, id)))
        } else {
            return Err("DnaQL: expected SEQUENCE or LOCUS after ANALYZE".into());
        };
        let mut functions = Vec::new();
        while matches!(self.peek(), Tok::Pipe) {
            self.next();
            let fname = self.ident()?;
            self.expect(Tok::LParen)?;
            self.expect(Tok::RParen)?;
            functions.push(fname.to_uppercase());
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::BioAnalysis {
            sequence,
            locus,
            functions,
        })
    }

    // ---- expressions (WHERE) ----
    pub(crate) fn parse_expr(&mut self) -> Result<Expr, String> {
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
        // Prefix NOT binds above comparisons: NOT x = 5 ≡ NOT (x = 5).
        // (v4.3 — SQL parity for the expression grammar.)
        if self.kw("NOT") {
            let inner = self.parse_cmp()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        let left = self.parse_operand()?;
        // Postfix negation: `x NOT LIKE 'pat'`, `x NOT IN (…)`. (v4.3)
        if self.kw("NOT") {
            if self.kw("LIKE") {
                let pattern = self.str_lit()?;
                return Ok(Expr::Like {
                    operand: Box::new(left),
                    pattern,
                    negated: true,
                });
            }
            if self.kw("IN") {
                let column = match left {
                    Expr::Column(c) => c,
                    other => {
                        return Err(format!(
                            "DnaQL: NOT IN requires a column on the left, got '{}'",
                            other.describe()
                        ))
                    }
                };
                let values = self.parse_in_value_list()?;
                return Ok(Expr::InList {
                    column,
                    values,
                    negated: true,
                });
            }
            return Err("DnaQL: expected LIKE or IN after NOT".into());
        }
        if self.kw("LIKE") {
            // v4.3: SQL LIKE over the shared expression AST (MySQL wildcards
            // % / _, backslash escapes, case-insensitive).
            let pattern = self.str_lit()?;
            return Ok(Expr::Like {
                operand: Box::new(left),
                pattern,
                negated: false,
            });
        }
        let op = match self.peek() {
            Tok::Eq => Some(CmpOp::Eq),
            Tok::Ne => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
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
        if self.kw("IS") {
            if self.kw("NOT") {
                self.expect_kw("NULL")?;
                return Ok(Expr::IsNotNull(Box::new(left)));
            }
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull(Box::new(left)));
        }
        // `col IN (v1, v2, ...)` — same semantics as SQL; subqueries do not
        // exist in DnaQL (the SQL engine materialises those itself).
        if self.kw("IN") {
            let column = match left {
                Expr::Column(c) => c,
                other => {
                    return Err(format!(
                        "DnaQL: IN requires a column on the left, got '{}'",
                        other.describe()
                    ))
                }
            };
            let values = self.parse_in_value_list()?;
            return Ok(Expr::InList { column, values, negated: false });
        }
        Ok(left)
    }

    /// `(v1, v2, ...)` value list after IN — shared by IN and NOT IN.
    fn parse_in_value_list(&mut self) -> Result<Vec<Value>, String> {
        self.expect(Tok::LParen)?;
        let mut values = Vec::new();
        loop {
            values.push(self.parse_value()?);
            if matches!(self.peek(), Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(values)
    }

    /// additive  ::= multiplicative (('+' | '-') multiplicative)*
    fn parse_operand(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => ArithOp::Add,
                Tok::Minus => ArithOp::Sub,
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

    /// multiplicative ::= unary (('*' | '/') unary)*
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => ArithOp::Mul,
                Tok::Slash => ArithOp::Div,
                _ => break,
            };
            self.next();
            let right = self.parse_unary()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// unary ::= '-' primary | primary
    fn parse_unary(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Tok::Minus) {
            self.next();
            let inner = self.parse_unary()?;
            return Ok(Expr::Arith {
                op: ArithOp::Sub,
                left: Box::new(Expr::Literal(Value::Int(0))),
                right: Box::new(inner),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.next();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.next();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.next();
                if !s.is_empty()
                    && s.chars()
                        .all(|c| matches!(c, 'A' | 'C' | 'G' | 'T' | 'a' | 'c' | 'g' | 't'))
                {
                    Ok(Expr::Literal(Value::Dna(s.to_uppercase())))
                } else {
                    Ok(Expr::Literal(Value::Text(s)))
                }
            }
            Tok::Ident(s) => {
                self.next();
                if s.eq_ignore_ascii_case("NULL") {
                    Ok(Expr::Literal(Value::Null))
                } else if s.eq_ignore_ascii_case("CAST") && matches!(self.peek(), Tok::LParen) {
                    // CAST(expr AS SIGNED|UNSIGNED|CHAR) — shared AST node,
                    // evaluated by the shared evaluator (v4.3).
                    self.next(); // (
                    let inner = self.parse_or()?;
                    self.expect_kw("AS")?;
                    let ty = if self.kw("SIGNED") {
                        CastTy::Signed
                    } else if self.kw("UNSIGNED") {
                        CastTy::Unsigned
                    } else if self.kw("CHAR") {
                        CastTy::Char
                    } else {
                        return Err("DnaQL: CAST supports SIGNED, UNSIGNED or CHAR".into());
                    };
                    self.expect(Tok::RParen)?;
                    Ok(Expr::Cast {
                        inner: Box::new(inner),
                        ty,
                    })
                } else if matches!(self.peek(), Tok::LParen)
                    && matches!(
                        s.to_ascii_uppercase().as_str(),
                        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                    )
                {
                    // Aggregate reference (HAVING / ORDER BY over grouped
                    // rows): `COUNT(*)`, `SUM(balance + 1)` become a column
                    // keyed by the canonical rendering — the same key the
                    // GROUP stage materialises the aggregate under.
                    self.next(); // (
                    let arg = if matches!(self.peek(), Tok::Star) {
                        self.next();
                        if !s.eq_ignore_ascii_case("COUNT") {
                            return Err(format!(
                                "DnaQL: {}(*) is not defined — only COUNT takes '*'",
                                s
                            ));
                        }
                        None
                    } else {
                        Some(self.parse_or()?)
                    };
                    self.expect(Tok::RParen)?;
                    let name = match arg {
                        None => format!("{}(*)", s.to_ascii_uppercase()),
                        Some(e) => format!("{}({})", s.to_ascii_uppercase(), e.describe()),
                    };
                    Ok(Expr::Column(name))
                } else if matches!(self.peek(), Tok::Dot)
                    && matches!(self.toks.get(self.pos + 1), Some(Tok::Ident(_)))
                {
                    // Qualified column `chr2.field` — merged Link/Join fields
                    // live under their source chromosome's name (v4.3).
                    self.next(); // Dot
                    let field = self.ident()?;
                    Ok(Expr::Column(format!("{}.{}", s, field)))
                } else {
                    Ok(Expr::Column(s))
                }
            }
            Tok::LParen => {
                self.next();
                let e = self.parse_or()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            t => Err(format!("DnaQL: expected operand, got {:?}", t)),
        }
    }
}

// ---------------------------------------------------------------------------
// Raw token rendering (ALTER ... ADD COLUMN tail)
// ---------------------------------------------------------------------------

/// Render a token back to its DnaQL/SQL text form. Used by the ALTER
/// passthrough to carry the `ADD COLUMN` type+flags tail verbatim into the
/// rendered SQL statement (strings are re-quoted with doubled quotes —
/// the lexer has no escape sequences).
fn token_text(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => s.clone(),
        Tok::Int(i) => i.to_string(),
        Tok::Float(f) => float_token(f),
        Tok::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Tok::Question => "?".into(),
        Tok::Tilde => "~".into(),
        Tok::Arrow => "->".into(),
        Tok::Pipe => "|>".into(),
        Tok::FatArrow => "=>".into(),
        Tok::PlusEq => "+=".into(),
        Tok::MinusEq => "-=".into(),
        Tok::Eq => "=".into(),
        Tok::Ne => "!=".into(),
        Tok::Le => "<=".into(),
        Tok::Ge => ">=".into(),
        Tok::Lt => "<".into(),
        Tok::Gt => ">".into(),
        Tok::Colon => ":".into(),
        Tok::Semicolon => ";".into(),
        Tok::Comma => ",".into(),
        Tok::Dot => ".".into(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBrace => "{".into(),
        Tok::RBrace => "}".into(),
        Tok::Star => "*".into(),
        Tok::Plus => "+".into(),
        Tok::Minus => "-".into(),
        Tok::Slash => "/".into(),
        Tok::Eof => String::new(),
    }
}
