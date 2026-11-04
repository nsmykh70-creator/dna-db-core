// SPDX-License-Identifier: AGPL-3.0-only
//! Relational SQL Engine — REAL tokenizer + parser + executor, split into
//! submodules: `lexer` (tokens), `stmt` (AST), `parser` (recursive descent),
//! `refs` (ACL table/column extraction), `executor` (statement execution),
//! `index_scan` (secondary-index planning). The public API is unchanged:
//! `SqlEngine`, `QueryResult`, `QueryRow`, `SqlTxBuffer`, `Stmt`, `SelectItem`
//! and `referenced_tables` are re-exported from here.

mod executor;
mod index_scan;
mod lexer;
mod parser;
mod refs;
mod stmt;
#[cfg(test)]
mod tests;

pub use stmt::Stmt;
pub(crate) use index_scan::render_stmt_sql;
pub(crate) use refs::referenced_tables;
pub(crate) use stmt::{AggFunc, JoinSpec, JoinType, OnConflict, SelectItem, TxAction, UpsertValue, WindowFunc};

use crate::complement_index::ComplementIndex;
use crate::engine::Engine;
use crate::relational::{ChromosomeTable, Value};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;


pub struct QueryRow {
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub execution_plan: String,
    pub execution_time_us: u64,
    pub message: Option<String>,
}

pub struct SqlEngine {
    pub engine: Arc<Engine>,
    schemas: RwLock<BTreeMap<String, ChromosomeTable>>,
    pub complement_index: RwLock<ComplementIndex>,
    /// v3.6: statement-scoped CTE materialisation store. Populated while a
    /// `WITH ... SELECT` runs, always emptied before the statement returns.
    /// CTE/derived tables: name -> (output columns, rows). The column list
    /// is kept even for zero-row bodies so `SELECT *` over an empty
    /// derived table still reports a RowDescription (MySQL always sends
    /// column definitions; MediaWiki keys on them).
    cte_store: std::sync::Mutex<std::collections::HashMap<String, (Vec<String>, Vec<BTreeMap<String, Value>>)>>,
    /// CMS edition (branch cms-edition): per-table AUTO_INCREMENT sequences.
    /// Shared Arc so the compat gate (`cms::compat::CmsSqlGate`) and the
    /// engine observe one counter per table. Seeded lazily from MAX(pk).
    #[cfg(feature = "cms")]
    pub cms_sequences: std::sync::Arc<crate::cms::sequence::AutoIncrementRegistry>,
}

/// v3.6 session transaction buffer: DML inside `BEGIN ... COMMIT` is
/// collected here and applied as ONE atomic `write_batch` on COMMIT
/// (a single fsync durability interval, mirroring tx.rs semantics).
#[derive(Default)]
pub struct SqlTxBuffer {
    pub ops: Vec<(crate::wal::WalOp, Vec<u8>, Option<Vec<u8>>)>,
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Tokenizer
