//! Transaction Coordinator — atomic multi-table writes
//!
//! v2.1.0 reality check (documented honestly):
//! - Sessions buffer a write-set (per-table row mutations).
//! - commit() serializes the ENTIRE write-set into ONE WAL batch frame:
//!   a single fsync, a single atomic durability interval. A crash either
//!   replays the whole batch or none of it.
//! - Isolation level actually provided: Read Committed (reads see committed
//!   engine state; writes are buffered until commit). Snapshot/Serializable
//!   are declared but NOT implemented in v2.1 — see ROADMAP (MVCC).
//! - "2PC" in the marketing sense is gone: this is a single-node atomic
//!   commit protocol. True 2PC across nodes = ROADMAP.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use crate::engine::Engine;
use crate::relational::Value;
use crate::wal::WalOp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxIsolationLevel {
    ReadCommitted,
    /// Declared, not implemented in v2.1 (falls back to ReadCommitted). ROADMAP: MVCC.
    SnapshotIsolation,
    /// Declared, not implemented in v2.1 (falls back to ReadCommitted). ROADMAP: MVCC.
    Serializable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    Active,
    Committed,
    Aborted,
}

/// One buffered mutation inside a transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    /// Replace the whole row payload at this key.
    PutRow { key: Vec<u8>, fields: BTreeMap<String, Value> },
    /// Delete the key (cascade handled at the relational layer).
    Delete { key: Vec<u8> },
}

pub struct TransactionSession {
    pub tx_id: u64,
    pub started_at_lsn: u64,
    pub status: TxStatus,
    pub isolation: TxIsolationLevel,
    write_set: Vec<Mutation>,
}

impl TransactionSession {
    pub fn put_row(&mut self, key: Vec<u8>, fields: BTreeMap<String, Value>) {
        self.write_set.push(Mutation::PutRow { key, fields });
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.write_set.push(Mutation::Delete { key });
    }

    pub fn write_set_len(&self) -> usize {
        self.write_set.len()
    }
}

pub struct TransactionCoordinator {
    engine: Arc<Engine>,
    next_tx_id: AtomicU64,
    global_lsn: AtomicU64,
    committed_txs: AtomicU64,
    aborted_txs: AtomicU64,
}

impl TransactionCoordinator {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            next_tx_id: AtomicU64::new(1001),
            global_lsn: AtomicU64::new(1),
            committed_txs: AtomicU64::new(0),
            aborted_txs: AtomicU64::new(0),
        }
    }

    pub fn begin(&self, isolation: TxIsolationLevel) -> TransactionSession {
        let tx_id = self.next_tx_id.fetch_add(1, Ordering::SeqCst);
        TransactionSession {
            tx_id,
            started_at_lsn: self.global_lsn.load(Ordering::SeqCst),
            status: TxStatus::Active,
            isolation,
            write_set: Vec::new(),
        }
    }

    /// Atomic commit: encode all mutations and flush them as one WAL batch.
    pub fn commit(&self, session: &mut TransactionSession) -> Result<u64, String> {
        if session.status != TxStatus::Active {
            return Err(format!("transaction is {:?}, not Active", session.status));
        }
        if session.write_set.is_empty() {
            session.status = TxStatus::Committed;
            return Ok(self.global_lsn.load(Ordering::SeqCst));
        }

        let mut ops: Vec<(WalOp, Vec<u8>, Option<Vec<u8>>)> =
            Vec::with_capacity(session.write_set.len());
        for m in &session.write_set {
            match m {
                Mutation::PutRow { key, fields } => {
                    let payload = crate::relational::RowCodec::encode(
                        &fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(),
                    );
                    ops.push((WalOp::Put, key.clone(), Some(payload)));
                }
                Mutation::Delete { key } => {
                    ops.push((WalOp::Delete, key.clone(), None));
                }
            }
        }

        self.engine
            .write_batch(&ops)
            .map_err(|e| format!("commit failed: {}", e))?;

        let lsn = self.global_lsn.fetch_add(1, Ordering::SeqCst);
        self.committed_txs.fetch_add(1, Ordering::Relaxed);
        session.status = TxStatus::Committed;
        Ok(lsn)
    }

    pub fn rollback(&self, session: &mut TransactionSession) {
        session.write_set.clear();
        session.status = TxStatus::Aborted;
        self.aborted_txs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.committed_txs.load(Ordering::Relaxed),
            self.aborted_txs.load(Ordering::Relaxed),
            self.global_lsn.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::relational::RowCodec;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dna_tx_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn commit_persists_rows_atomically() {
        let dir = tempdir("commit");
        let engine = Engine::open(EngineConfig {
            dir_path: dir.clone(),
            ..Default::default()
        })
        .unwrap();
        let coord = TransactionCoordinator::new(engine.clone());

        let mut tx = coord.begin(TxIsolationLevel::ReadCommitted);
        let mut row1 = BTreeMap::new();
        row1.insert("user_id".to_string(), Value::Int(14));
        row1.insert("balance".to_string(), Value::Float(540.0));
        tx.put_row(b"chr_users:14".to_vec(), row1);
        let mut row2 = BTreeMap::new();
        row2.insert("order_id".to_string(), Value::Int(9021));
        row2.insert("user_id".to_string(), Value::Int(14));
        tx.put_row(b"chr_orders:9021".to_vec(), row2);
        tx.delete(b"chr_junk:1".to_vec());
        assert_eq!(tx.write_set_len(), 3);
        coord.commit(&mut tx).unwrap();
        assert_eq!(tx.status, TxStatus::Committed);

        // Rows are RowCodec payloads
        let raw = engine.get(b"chr_users:14").unwrap().unwrap();
        let fields = RowCodec::decode(&raw).unwrap();
        assert_eq!(fields[0].0, "balance");
        assert_eq!(fields[0].1, Value::Float(540.0));

        // Survives restart
        drop(engine);
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            ..Default::default()
        })
        .unwrap();
        assert!(engine.get(b"chr_users:14").unwrap().is_some());
        assert!(engine.get(b"chr_orders:9021").unwrap().is_some());
        assert!(engine.get(b"chr_junk:1").unwrap().is_none());
    }

    #[test]
    fn rollback_discards_writes() {
        let dir = tempdir("rollback");
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            ..Default::default()
        })
        .unwrap();
        let coord = TransactionCoordinator::new(engine.clone());

        let mut tx = coord.begin(TxIsolationLevel::ReadCommitted);
        let mut row = BTreeMap::new();
        row.insert("x".to_string(), Value::Int(1));
        tx.put_row(b"chr_x:1".to_vec(), row);
        coord.rollback(&mut tx);
        assert_eq!(tx.status, TxStatus::Aborted);
        assert!(engine.get(b"chr_x:1").unwrap().is_none());
    }

    #[test]
    fn double_commit_rejected() {
        let dir = tempdir("double");
        let engine = Engine::open(EngineConfig {
            dir_path: dir,
            ..Default::default()
        })
        .unwrap();
        let coord = TransactionCoordinator::new(engine.clone());
        let mut tx = coord.begin(TxIsolationLevel::ReadCommitted);
        coord.commit(&mut tx).unwrap();
        assert!(coord.commit(&mut tx).is_err());
    }
}
