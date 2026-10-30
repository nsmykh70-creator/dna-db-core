// SPDX-License-Identifier: AGPL-3.0-only
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

pub mod admin;
pub mod access;
pub mod auth;
pub mod bio_plugins;
pub mod bloom;
pub mod complement_index;
pub mod crypto;
pub mod dna;
pub mod dna_ql;
pub mod engine;
pub mod expr;
pub mod memtable;
pub mod mirror;
pub mod protocol;
pub mod reed_solomon;
pub mod relational;
pub mod repair;
pub mod rotate;
pub mod segment;
pub mod segment_block;
pub mod server;
pub mod simd_bitap;
pub mod sql_engine;
mod sql_to_dnaql;
pub mod stats;
pub mod tiered_storage;
pub mod tls;
pub mod tx;
pub mod wal;

pub use bio_plugins::{BioPluginFn, BioPluginRegistry};
pub use complement_index::ComplementIndex;
pub use dna::PackedDna;
pub use dna_ql::{DistanceMode, DnaQlAst, DnaQlEngine, DnaQlResult, DnaQlRow};
pub use engine::{Engine, EngineConfig};
pub use expr::Expr;
pub use reed_solomon::ErasureCodec;
pub use relational::{ChromosomeTable, ColumnType, ForeignKeyConstraint, Value};
pub use repair::{RepairPolicy, ScrubConfig};
pub use simd_bitap::{AlignmentMatch, BitapMatcher};
pub use sql_engine::{QueryResult, SqlEngine};
pub use tiered_storage::{StorageTier, TieredStorageManager};
pub use tx::{Mutation, TransactionCoordinator, TransactionSession, TxIsolationLevel};
pub use wal::SyncMode;
