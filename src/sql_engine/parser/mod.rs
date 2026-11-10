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

}
impl SParser {
    /// Debug helper for "trailing tokens" parse errors: the next few
    /// tokens around the failure point, for the wire error message.
    pub(super) fn debug_context(&self) -> String {
        let end = (self.pos + 8).min(self.toks.len());
        let parts: Vec<String> = self.toks[self.pos..end].iter().map(|t| format!("{:?}", t)).collect();
        parts.join(" ")
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
            // Online type change: ALTER TABLE t MODIFY [COLUMN] c T [flags]
            // and rename+retype: ALTER TABLE t CHANGE [COLUMN] old new T [flags].
            // Shared grammar in every build (cms gates wire dialects, not SQL).
            if self.kw("MODIFY") {
                self.kw("COLUMN");
                let column = self.ident()?;
                let col_type = self.parse_column_type()?;
                let (col_default, _not_null) = self.parse_alter_column_tail(&col_type)?;
                self.eat_semi();
                return Ok(Stmt::ModifyColumn {
                    table,
                    column,
                    new_name: None,
                    col_type,
                    col_default,
                });
            }
            if self.kw("CHANGE") {
                self.kw("COLUMN");
                let column = self.ident()?;
                let new_name = self.ident()?;
                let col_type = self.parse_column_type()?;
                let (col_default, _not_null) = self.parse_alter_column_tail(&col_type)?;
                self.eat_semi();
                return Ok(Stmt::ModifyColumn {
                    table,
                    column,
                    new_name: Some(new_name),
                    col_type,
                    col_default,
                });
            }
            self.expect_kw("ADD")?;
            self.kw("COLUMN");
            let column = self.ident()?;
            let col_type = self.parse_column_type()?;
            // Trailing column flags — the DEFAULT literal
            // (or the NOT NULL zero-value) becomes the new column's
            // effective default, exactly like CREATE TABLE. Shared grammar
            // in every build (cms feature gates wire dialects, not SQL).
            {
                let (col_default, not_null) = self.parse_alter_column_tail(&col_type)?;
                let mut col_default = col_default;
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

}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Days-since-epoch to (year, month, day) — Howard Hinnant's algorithm,
/// duplicated from the compat layer (the core parser has no cms deps).
fn civil_from_days_impl(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

mod ddl;
mod dml;
mod expr;
mod select;
