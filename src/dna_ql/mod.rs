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
        projection: Vec<EmitItem>,
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
    /// `SHOW CHROMOSOMES [LIKE 'pattern'];` — table introspection over the
    /// shared SQL-engine schema cache (v4.3: closes the gap where the
    /// SQL→DnaQL transpiler advertised a statement DnaQL did not parse).
    ShowChromosomes {
        pattern: Option<String>,
    },
    /// SQL DDL passthrough (v4.3): `CREATE [UNIQUE] INDEX`, `DROP INDEX`,
    /// `ALTER CHROMOSOME ... ADD/DROP COLUMN` render to the exact SQL
    /// statement and execute through the shared SQL engine — the same path
    /// CREATE/DROP CHROMOSOME already take, so the schema cache stays
    /// consistent on both sides. `tables` lists the touched chromosomes so
    /// the per-user ACL can check them without touching storage.
    DdlPassthrough {
        sql: String,
        tables: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub enum PipelineStage {
    Link(Option<LinkSpec>),
    Filter(Expr),
    Emit(Vec<EmitItem>),
    /// `|> SORT column [ASC|DESC]` — order the rows (was SQL-only before).
    Sort {
        column: String,
        asc: bool,
    },
    /// `|> SORT <expr> [ASC|DESC]` — v4.3: sort by a computed key
    /// (`SORT balance * -1 DESC`, SQL `ORDER BY meta_value+0`).
    SortBy {
        expr: Expr,
        asc: bool,
    },
    /// `|> LIMIT n` — keep the first n rows after the previous stage.
    Limit(usize),
    /// `|> SKIP n` — drop the first n rows (SQL OFFSET; v4.3).
    Skip(usize),
    /// `|> [LEFT] JOIN chr2 ON left_expr = right_expr [AND extra]*` —
    /// equi-join against a whole chromosome (v4.3). INNER drops unmatched
    /// left rows; LEFT NULL-extends them (the right side contributes no
    /// fields, which read as NULL).
    Join(JoinStage),
    /// `|> GROUP [BY] cols, AGG(expr) AS a, ... [HAVING expr]` — grouping
    /// with COUNT/SUM/AVG/MIN/MAX aggregates (v4.3). One output row per
    /// group; group columns keep their value, aggregates land under their
    /// aliases. SQL maps GROUP BY + aggregates + HAVING onto this stage.
    Group(GroupStage),
}

/// One item of an `EMIT` projection (v4.3: expressions with aliases joined
/// plain columns and `*`).
#[derive(Debug, Clone)]
pub enum EmitItem {
    /// `*` — every current field.
    All,
    /// `column` or `column AS alias`.
    Column {
        name: String,
        alias: Option<String>,
    },
    /// `expr AS alias` — evaluated against the current row; without an
    /// alias the canonical rendering becomes the output field name.
    Computed {
        expr: Expr,
        alias: Option<String>,
    },
}

/// One equi-join stage (see [`PipelineStage::Join`]).
#[derive(Debug, Clone)]
pub struct JoinStage {
    pub chromosome: String,
    /// LEFT JOIN — keep unmatched left rows (NULL-extended).
    pub left_outer: bool,
    /// Evaluated against the LEFT row.
    pub on_left: Expr,
    /// Evaluated against the RIGHT row.
    pub on_right: Expr,
    /// Non-equi conjuncts of the ON clause, evaluated on the merged row
    /// (SQL `ON a.x = b.y AND a.z = b.w` shape).
    pub on_extra: Vec<Expr>,
}

/// One grouping stage (see [`PipelineStage::Group`]).
#[derive(Debug, Clone)]
pub struct GroupStage {
    /// GROUP BY column list (may be empty = one global group).
    pub by: Vec<String>,
    /// Aggregates: `COUNT(*)`, `COUNT(col)`, `SUM/AVG/MIN/MAX(expr)`.
    pub aggs: Vec<AggSpec>,
    /// HAVING — evaluated on the grouped row (aggregate aliases visible).
    pub having: Option<Expr>,
}

/// Aggregate function of a `GROUP` stage. Mirrors the SQL engine's set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnaAggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl DnaAggFunc {
    /// Canonical SQL rendering used as the default output alias.
    pub fn canonical(&self, arg: Option<&str>) -> String {
        let name = match self {
            DnaAggFunc::Count => "COUNT",
            DnaAggFunc::Sum => "SUM",
            DnaAggFunc::Avg => "AVG",
            DnaAggFunc::Min => "MIN",
            DnaAggFunc::Max => "MAX",
        };
        match arg {
            Some(a) => format!("{}({})", name, a),
            None => format!("{}(*)", name),
        }
    }
}

/// One aggregate call inside a `GROUP` stage.
#[derive(Debug, Clone)]
pub struct AggSpec {
    pub func: DnaAggFunc,
    /// `None` = COUNT(*); otherwise evaluated per row, NULLs skipped.
    pub arg: Option<Expr>,
    /// Output field name (alias, else the canonical rendering).
    pub alias: Option<String>,
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
                match stage {
                    PipelineStage::Link(Some(link)) => out.push(link.chromosome.clone()),
                    PipelineStage::Join(join) => out.push(join.chromosome.clone()),
                    _ => {}
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
        DnaQlAst::DdlPassthrough { tables, .. } => out.extend(tables.iter().cloned()),
        DnaQlAst::ShowChromosomes { .. } => {
            // Schema-level introspection: no chromosome rows are read, the
            // SQL-engine name list only. Nothing to add for the row ACL.
        }
    }
}

// re-export for SQL engine reuse
pub use expr::{CastTy, Expr as DnaQlExpr};

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
