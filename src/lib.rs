//! DNA-DB Core: High-performance storage engine
//! Features:
//! - LSM-Tree architecture with append-only WAL & SSTable segments
//! - Zero-allocation 2-bit DNA sequence encoding (4x compression)
//! - Dual-strand hot-swap disk mirroring (RAID-1-like failover)
//! - DnaQL domain-specific query engine (Standard & Quick/Pipeline syntax)
//! - Anti-tamper authentication & brute-force defense
//! - Full crash resilience under SIGKILL (kill -9)
//!
//! v3.0 «Repairosome» — self-healing storage:
//! - Reed–Solomon erasure coding inside every segment (GF(2^8), k+m blocks)
//! - CRC32C per 4 KiB block; on-read repair; background scrubber
//! - `doctor` offline scan/fix/migration; repair journal; health API

pub mod wal;
pub mod memtable;
pub mod segment;
pub mod segment_block;
pub mod reed_solomon;
pub mod repair;
pub mod engine;
pub mod dna;
pub mod mirror;
pub mod protocol;
pub mod server;
pub mod relational;
pub mod sql_engine;
pub mod tx;
pub mod dna_ql;
pub mod simd_bitap;
pub mod tiered_storage;
pub mod bio_plugins;
pub mod bloom;
pub mod stats;
pub mod complement_index;
pub mod expr;
pub mod admin;

pub use engine::{Engine, EngineConfig};
pub use wal::SyncMode;
pub use dna::PackedDna;
pub use relational::{ChromosomeTable, ColumnType, ForeignKeyConstraint, Value};
pub use sql_engine::{SqlEngine, QueryResult};
pub use tx::{TransactionCoordinator, TransactionSession, TxIsolationLevel, Mutation};
pub use dna_ql::{DnaQlEngine, DnaQlAst, DnaQlResult, DnaQlRow, DistanceMode};
pub use simd_bitap::{BitapMatcher, AlignmentMatch};
pub use tiered_storage::{TieredStorageManager, StorageTier};
pub use complement_index::ComplementIndex;
pub use expr::Expr;
pub use bio_plugins::{BioPluginRegistry, BioPluginFn};
pub use reed_solomon::ErasureCodec;
pub use repair::{RepairPolicy, ScrubConfig};
