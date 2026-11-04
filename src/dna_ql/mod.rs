// SPDX-License-Identifier: AGPL-3.0-only
//! DnaQL (DNA Query Language) v2.1 — REAL tokenizer, parser and executor.
//!
//! Implemented grammar (subset of DNAQL_SPEC.md, fully functional):
//!
//!   Query      ::= PatternSearch | PipelineQuery | TxBlock | BioAnalysis
//!   QuickSearch::= Ident '?' StringLit ['~' Int | '*' Int]
//!                  ['->' Ident ['(' Ident ')']] ['=>' 'emit' Projection] [';']
//!   StdSearch  ::= 'FIND' ['PATTERN'] StringLit 'IN' ['CHROMOSOME'] Ident
//!                  ['WHERE' ('HAMMING_DISTANCE'|'LEVENSHTEIN_DISTANCE') '<=' Int]
//!                  ['LINK' Ident 'VIA' 'COMPLEMENT' '(' Ident ')']
//!                  ['EMIT' Projection] [';']
//!   Pipeline   ::= 'FROM' Ident ( '|>' Stage )*
//!   Stage      ::= 'LINK' Ident ['VIA' 'COMPLEMENT' '(' Ident ')']
//!               |  'WHERE' Expr
//!               |  'EMIT' Projection
//!   TxBlock    ::= 'TX' '{' Stmt (';' Stmt)* '}'
//!               |  'SYNTHESIZE' 'INTO' IdentList 'BEGIN' StmtList ('LIGATE'|'HYDROLYZE')
//!   Stmt       ::= Ident ':' Int '.' Ident ('='|'+='|'-=') Literal
//!               |  Ident ':' Int '+=' '{' KVList '}'
//!               |  'DROP' 'LOCUS' Ident ':' Int ['CASCADE']
//!   Bio        ::= 'ANALYZE' ('SEQUENCE' StringLit | 'LOCUS' Ident ':' Int)
//!                  ( '|>' BioFn )*
//!   BioFn      ::= 'GC_CONTENT()' | 'MELTING_TEMP()' | 'REV_COMP()'
//!               |  'TRANSCRIBE()' | 'TRANSLATE()'
//!
//! Execution reads/writes the REAL Engine storage:
//!   rows: '<table>:<pk>' -> RowCodec payloads (field 'sequence' = Value::Dna)

use crate::dna::PackedDna;
use crate::engine::Engine;
use crate::expr::{self, ArithOp, CmpOp, Expr};
use crate::relational::{RowCodec, Value};
use crate::simd_bitap::BitapMatcher;
use crate::sql_engine::SqlEngine;
use crate::tx::TransactionCoordinator;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum DistanceMode {
    Hamming,
    Levenshtein,
}

#[derive(Debug, Clone)]
pub struct LinkSpec {
    pub chromosome: String,
    pub column: String,
}

#[derive(Debug, Clone)]
pub enum DnaQlAst {
    PatternSearch {
        chromosome: String,
        pattern: String,
        mode: DistanceMode,
        max_distance: usize,
        link: Option<LinkSpec>,
        projection: Vec<String>,
    },
    PipelineQuery {
        source: String,
        stages: Vec<PipelineStage>,
    },
    Tx {
        statements: Vec<TxStmt>,
    },
    BioAnalysis {
        sequence: Option<String>,
        locus: Option<(String, i64)>,
        functions: Vec<String>,
    },
    /// VERIFY / REPAIR CHROMOSOME — segment integrity check (v3.0)
    HealthCheck {
        chromosome: String,
        fix: bool,
    },
    /// CREATE CHROMOSOME name (col TYPE, ..., PRIMARY KEY (...),
    /// FOREIGN KEY (...) REFERENCES ...) — schema DDL. The parenthesis body
    /// is kept as raw definition clauses and handed verbatim to the SQL
    /// engine, so DnaQL DDL and SQL CREATE TABLE accept exactly the same
    /// grammar (types, PRIMARY KEY, FOREIGN KEY, NOT NULL, lengths).
    CreateChromosome {
        table: String,
        definitions: Vec<String>,
        if_not_exists: bool,
    },
    /// DROP CHROMOSOME name [IF EXISTS]
    DropChromosome {
        table: String,
        if_exists: bool,
    },
}

#[derive(Debug, Clone)]
pub enum PipelineStage {
    Link(Option<LinkSpec>),
    Filter(Expr),
    Emit(Vec<String>),
    /// `|> SORT column [ASC|DESC]` — order the rows (was SQL-only before).
    Sort {
        column: String,
        asc: bool,
    },
    /// `|> LIMIT n` — keep the first n rows after the previous stage.
    Limit(usize),
}

#[derive(Debug, Clone)]
pub enum TxStmt {
    /// chr:locus.field = literal | += literal | -= literal
    Mutate {
        table: String,
        locus: i64,
        field: String,
        op: AssignOp,
        value: Value,
    },
    /// chr:locus += { k: v, ... }
    Append {
        table: String,
        locus: i64,
        fields: Vec<(String, Value)>,
    },
    /// DROP LOCUS chr:locus [CASCADE]
    DropLocus {
        table: String,
        locus: i64,
        cascade: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
}

#[derive(Debug, Clone)]
pub struct DnaQlRow {
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct DnaQlResult {
    pub ast_summary: String,
    pub rows: Vec<DnaQlRow>,
    pub execution_time_us: u64,
    pub engine_note: String,
}

/// Parse a DnaQL query and return every chromosome/table it references. Used
/// by the per-user ACL before execution; this does not touch storage.
pub fn referenced_tables(query: &str) -> Result<Vec<String>, String> {
    let toks = tokenize(query)?;
    let mut parser = Parser::new(toks);
    let ast = parser.parse_query()?;
    let mut tables = Vec::new();
    collect_ast_tables(&ast, &mut tables);
    tables.sort();
    tables.dedup();
    Ok(tables)
}

fn collect_ast_tables(ast: &DnaQlAst, out: &mut Vec<String>) {
    match ast {
        DnaQlAst::PatternSearch {
            chromosome, link, ..
        } => {
            out.push(chromosome.clone());
            if let Some(link) = link {
                out.push(link.chromosome.clone());
            }
        }
        DnaQlAst::PipelineQuery { source, stages } => {
            out.push(source.clone());
            for stage in stages {
                if let PipelineStage::Link(Some(link)) = stage {
                    out.push(link.chromosome.clone());
                }
            }
        }
        DnaQlAst::Tx { statements } => {
            for statement in statements {
                match statement {
                    TxStmt::Mutate { table, .. }
                    | TxStmt::Append { table, .. }
                    | TxStmt::DropLocus { table, .. } => out.push(table.clone()),
                }
            }
        }
        DnaQlAst::BioAnalysis { locus, .. } => {
            // ANALYZE LOCUS <table>:<id> reads one row of <table>, so the
            // per-user table ACL must see it. Returning an empty list here
            // let a table-restricted user read any locus of any table.
            if let Some((table, _)) = locus {
                out.push(table.clone());
            }
        }
        DnaQlAst::HealthCheck { chromosome, .. } => out.push(chromosome.clone()),
        DnaQlAst::CreateChromosome { table, .. }
        | DnaQlAst::DropChromosome { table, .. } => out.push(table.clone()),
    }
}

// re-export for SQL engine reuse
pub use expr::Expr as DnaQlExpr;

mod engine;
mod lexer;
mod parser;
#[cfg(test)]
mod tests;

pub use engine::DnaQlEngine;
#[cfg(test)]
pub(crate) use engine::apply_assign;
pub(crate) use lexer::{float_token, push_clause_token, Tok, tokenize};
pub(crate) use parser::Parser;
