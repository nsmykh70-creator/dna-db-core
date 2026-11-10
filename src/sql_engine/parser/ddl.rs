// SPDX-License-Identifier: AGPL-3.0-only
//! DDL parsing: CREATE TABLE with MySQL column types/attributes and
//! constraint lines, CREATE INDEX, DROP TABLE/DATABASE/INDEX.

use super::*;

impl SParser {
    pub(super) fn parse_create(&mut self) -> Result<Stmt, String> {
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
            // MySQL constraint lines inside CREATE TABLE —
            // `KEY name (col(len))`, `INDEX name (col)`, `UNIQUE KEY name (col)`,
            // `FULLTEXT KEY name (col)`. Single-column keys become engine
            // secondary indexes; composite keys index their first column
            // non-uniquely (engine single-PK limitation, CMS_EDITION_RU.md §4).
            // Grammar is shared with the plain build: the SQL surface must not
            // depend on the cms feature (the wire dialect gate is the only
            // cms-specific layer).
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
                // Prefix index `PRIMARY KEY (id(100))` (Joomla's
                // webauthn_credentials) — the length is dropped; the engine
                // indexes the whole column.
                self.consume_type_len()?;
                // Composite PRIMARY KEY (a, b) — the engine keys
                // on the first column for native paths; the full list is
                // retained so row keys encode every key part (MediaWiki
                // l10n_cache, Joomla session tables).
                t.primary_key_cols = vec![pk.clone()];
                while matches!(self.peek(), STok::Comma) {
                    self.next();
                    let part = self.ident()?;
                    self.consume_type_len()?;
                    t.primary_key_cols.push(part);
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
                // MySQL-dialect column flags in any order
                // (UNSIGNED NOT NULL DEFAULT '…' AUTO_INCREMENT …).
                // AUTO_INCREMENT records the sequence column (exec_insert_tx
                // assigns values); lengths/defaults are consumed and dropped.
                // Shared grammar: accepted (and recorded on the schema) in
                // every build — the cms feature gates wire dialects, not SQL.
                // (DEFAULT-fill / coercion *execution* stays cms-gated in
                // exec_insert_tx by design.)
                {
                    let mut not_null = false;
                    let mut default: Option<String> = None;
                    loop {
                        if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                            continue;
                        }
                        // MySQL attribute form `VARCHAR(n) BINARY` — binary
                        // comparison for the column (Drupal's file_managed.uri).
                        if self.kw("BINARY") {
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
                        // MySQL attribute `ON UPDATE CURRENT_TIMESTAMP` —
                        // the engine applies defaults, not update triggers;
                        // consumed for dialect compatibility (Joomla tags).
                        if self.kw("ON") {
                            self.expect_kw("UPDATE")?;
                            let _ = self.value()?;
                            continue;
                        }
                        // Column-level COMMENT '…' (Drupal schema definitions).
                        if self.kw("COMMENT") {
                            let _ = self.value()?;
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
                            t.primary_key_cols = vec![col.clone()];
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
                                // Negative numeric default (`DEFAULT -1`,
                                // Joomla's banners/track counters).
                                STok::Minus => {
                                    self.next();
                                    match self.next() {
                                        STok::Int(v) => format!("-{v}"),
                                        STok::Float(v) => format!("-{v}"),
                                        t => {
                                            return Err(format!(
                                                "SQL: bad DEFAULT value after '-', got {:?}",
                                                t
                                            ))
                                        }
                                    }
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
                t.add_column(col, ty);
            }
            if matches!(self.peek(), STok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(STok::RParen)?;
        // CMS edition: MySQL tables without PRIMARY KEY are legal — InnoDB
        // keys them by a hidden monotonic row id. Mirror that: no declared
        // PK and no `id` column → rows are keyed by an implicit
        // auto-increment `__rowid` that never joins the declared columns
        // (SELECT * / SHOW COLUMNS / information_schema cannot see it) but
        // stays stable for UPDATE/DELETE row keys (Joomla's
        // finder_terms_common).
        #[cfg(feature = "cms")]
        if t.primary_locus_column == "id"
            && !t.columns.contains_key("id")
            && t.auto_increment_column.is_none()
        {
            t.primary_locus_column = "__rowid".to_string();
            t.auto_increment_column = Some("__rowid".to_string());
        }
        // MySQL table options after the column list —
        // `ENGINE=InnoDB AUTO_INCREMENT=1 DEFAULT CHARSET=utf8mb4
        // COLLATE=utf8mb4_unicode_520_ci` — consumed and ignored.
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
                } else if self.kw("COMMENT") {
                    // Table-level COMMENT[='…'] (Drupal / Joomla tails).
                    if matches!(self.peek(), STok::Eq) {
                        self.next();
                    }
                    let _ = self.value()?;
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

    /// Column type of a CREATE TABLE column: base grammar plus the
    /// MySQL-dialect names WordPress DDL uses (bigint/tinyint/datetime/
    /// longtext/decimal/...), consuming optional `(n)` lengths. The SQL
    /// surface is identical in every build — the cms feature gates wire
    /// dialects and shims, never the grammar.
    pub(super) fn parse_column_type(&mut self) -> Result<ColumnType, String> {
        if self.kw("INT") || self.kw("INTEGER") || self.kw("INT64") {
            self.consume_type_len()?;
            Ok(ColumnType::Int64)
        } else if self.kw("FLOAT") || self.kw("FLOAT64") || self.kw("DOUBLE") {
            // MySQL synonym: `DOUBLE PRECISION` (MediaWiki page_random).
            self.kw("PRECISION");
            // MySQL length/precision spec: `double(15,8)`, `float(10,2)`
            // (OpenCart's currency.value) — precision is advisory, the
            // engine stores IEEE doubles.
            self.consume_type_len()?;
            Ok(ColumnType::Float64)
        } else if self.kw("TEXT") || self.kw("VARCHAR") || self.kw("STRING") {
            self.consume_type_len()?; // VARCHAR(191), TEXT(65535)
            Ok(ColumnType::Text)
        } else if self.kw("DNA") || self.kw("DNA2BIT") {
            Ok(ColumnType::Dna2Bit)
        } else if self.kw("TIMESTAMP") {
            Ok(ColumnType::Timestamp)
        } else {
            // MySQL-dialect names map onto base engine types.
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

    /// Consumes an optional `(n)` / `(n,m)` type length.
    pub(super) fn consume_type_len(&mut self) -> Result<(), String> {
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
    /// identifier (NULL / CURRENT_TIMESTAMP).
    pub(super) fn consume_default_value(&mut self) -> Result<(), String> {
        match self.peek() {
            STok::Str(_) | STok::Int(_) | STok::Float(_) | STok::Ident(_) => {
                let _ = self.next();
                Ok(())
            }
            t => Err(format!("SQL: bad DEFAULT value {:?}", t)),
        }
    }

    /// Trailing flags of an ALTER TABLE column clause (ADD / MODIFY /
    /// CHANGE): UNSIGNED/ZEROFILL, NOT NULL/NULL, DEFAULT <lit>, COMMENT,
    /// AUTO_INCREMENT, ON UPDATE CURRENT_TIMESTAMP. Returns the DEFAULT
    /// literal (if any) and the NOT-NULL flag — the caller synthesizes the
    /// zero default for NOT NULL columns without one (ADD COLUMN).
    pub(super) fn parse_alter_column_tail(
        &mut self,
        _col_type: &ColumnType,
    ) -> Result<(Option<String>, bool), String> {
        let mut col_default: Option<String> = None;
        let mut not_null = false;
        loop {
            if self.kw("UNSIGNED") || self.kw("ZEROFILL") {
                continue;
            }
            if self.kw("ON") {
                // ON UPDATE CURRENT_TIMESTAMP[()] — advisory on the engine
                // side (the CMS gate rewrites UPDATEs for such columns;
                // normalized_engine_ddl drops the flag).
                if self.kw("UPDATE") {
                    if self.kw("CURRENT_TIMESTAMP") {
                        // Optional () tail: current_timestamp().
                        if matches!(self.peek(), STok::LParen) {
                            self.next();
                            self.expect(STok::RParen)?;
                        }
                    }
                    continue;
                }
                return Err("SQL: expected UPDATE after ON in column definition".into());
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
        Ok((col_default, not_null))
    }

    /// True when the next token starts a MySQL constraint line inside
    /// CREATE TABLE (cms only): KEY / INDEX / UNIQUE / FULLTEXT / SPATIAL.
    ///
    /// A column can legitimately be NAMED `key` (backticked, backticks
    /// stripped in preprocessing) — `KEY char(32) …` is a column def whose
    /// name collides with the constraint keyword, while
    /// `KEY idx_name (col)` is a constraint. Disambiguate by lookahead:
    /// after the optional name, a constraint reaches '(' where the name is
    /// NOT a type keyword (a column def's next token is a type keyword
    /// before its length paren).
    pub(super) fn peek_is_cms_constraint(&self) -> bool {
        let type_word = |w: &str| matches!(
            w.to_ascii_uppercase().as_str(),
            "INT" | "INTEGER" | "BIGINT" | "TINYINT" | "SMALLINT" | "MEDIUMINT"
            | "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
            | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "VARBINARY"
            | "BINARY" | "DATETIME" | "DATE" | "TIME" | "TIMESTAMP" | "FLOAT"
            | "DOUBLE" | "DECIMAL" | "DNA" | "DNA2BIT" | "STRING" | "ENUM" | "SET"
        );
        let ident_at = |s: &SParser, i: usize| -> Option<String> {
            match s.toks.get(i) {
                Some(STok::Ident(w)) => Some(w.clone()),
                _ => None,
            }
        };
        let mut i = self.pos;
        // first word: KEY / INDEX / UNIQUE / FULLTEXT / SPATIAL
        let w0 = match ident_at(self, i) {
            Some(w) if matches!(
                w.to_ascii_uppercase().as_str(),
                "KEY" | "INDEX" | "UNIQUE" | "FULLTEXT" | "SPATIAL"
            ) =>
            {
                w
            }
            _ => return false,
        };
        i += 1;
        // optional second keyword: UNIQUE KEY / FULLTEXT KEY / UNIQUE INDEX
        if matches!(
            w0.to_ascii_uppercase().as_str(),
            "UNIQUE" | "FULLTEXT" | "SPATIAL"
        ) {
            match ident_at(self, i) {
                Some(w) if matches!(w.to_ascii_uppercase().as_str(), "KEY" | "INDEX") => {
                    i += 1;
                }
                // `UNIQUE (col)` — nameless constraint
                Some(_) => {}
                None => {}
            }
        }
        // nameless constraint: '(' follows directly
        if matches!(self.toks.get(i), Some(STok::LParen)) {
            return true;
        }
        // Named constraint: after the name a constraint reaches '(' while a
        // column def reaches its TYPE keyword. The name itself may be ANY
        // identifier — Joomla's base.sql has `KEY time (time)`, an index
        // legitimately named with a type keyword. Disambiguation:
        //   - name + '(' → constraint, unless the paren content is a type
        //     length (`KEY char(32)` is a COLUMN named key of type char);
        //     index column lists hold identifiers, type lengths hold
        //     numbers or enum/set string lists;
        //   - otherwise → column def (name = type keyword or bare type).
        match ident_at(self, i) {
            Some(_) if matches!(self.toks.get(i + 1), Some(STok::LParen)) => {
                if !type_word(&ident_at(self, i).unwrap()) {
                    return true;
                }
                matches!(self.toks.get(i + 2), Some(STok::Ident(_)))
            }
            _ => false,
        }
    }

    /// Parses one MySQL constraint line into the table's secondary indexes.
    /// Composite keys keep only the first column (non-unique) —
    /// a unique composite would reject valid rows under the single-column
    /// index model.
    pub(super) fn parse_cms_table_constraint(&mut self, t: &mut ChromosomeTable) -> Result<(), String> {
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
        // Optional index algorithm: `KEY name (cols) USING BTREE` — the
        // engine has one index structure, so the hint is consumed and
        // dropped (Joomla base.sql).
        if self.kw("USING") {
            let _ = self.ident()?;
        }
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
    pub(super) fn parse_create_index(&mut self, unique: bool) -> Result<Stmt, String> {
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

    pub(super) fn parse_drop(&mut self) -> Result<Stmt, String> {
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

}
