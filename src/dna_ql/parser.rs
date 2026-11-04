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
        match self.peek().clone() {
            Tok::Ident(s) if s.eq_ignore_ascii_case("FIND") => self.parse_standard_search(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("FROM") => self.parse_pipeline(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("TX") => self.parse_tx(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("SYNTHESIZE") => self.parse_synthesize(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("ANALYZE") => self.parse_bio(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("VERIFY") => self.parse_health_check(false),
            Tok::Ident(s) if s.eq_ignore_ascii_case("REPAIR") => self.parse_health_check(true),
            Tok::Ident(s) if s.eq_ignore_ascii_case("CREATE") => self.parse_create_chromosome(),
            Tok::Ident(s) if s.eq_ignore_ascii_case("DROP") => self.parse_drop_chromosome(),
            Tok::Ident(_) => self.parse_quick_search(),
            t => Err(format!("DnaQL: unexpected token {:?}", t)),
        }
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

    fn parse_projection(&mut self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(out); // empty = all fields
        }
        loop {
            out.push(self.ident()?);
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
                let column = self.ident()?;
                let asc = if self.kw("DESC") {
                    false
                } else {
                    self.kw("ASC");
                    true
                };
                stages.push(PipelineStage::Sort { column, asc });
            } else if self.kw("LIMIT") {
                let n = self.int()?;
                if n < 0 {
                    return Err("DnaQL: LIMIT expects a non-negative integer".into());
                }
                stages.push(PipelineStage::Limit(n as usize));
            } else {
                return Err(format!("DnaQL: unknown pipeline stage {:?}", self.peek()));
            }
        }
        if matches!(self.peek(), Tok::Semicolon) {
            self.next();
        }
        Ok(DnaQlAst::PipelineQuery { source, stages })
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
        let left = self.parse_operand()?;
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
            return Ok(Expr::InList { column, values, negated: false });
        }
        Ok(left)
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
