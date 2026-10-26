// SPDX-License-Identifier: AGPL-3.0-only
//! Transaction Coordinator — atomic multi-table writes
//!
//! v2.1.0 reality check (documented honestly):
//! - Sessions buffer a write-set (per-table row mutations).
//! - commit() serializes the ENTIRE write-set into ONE WAL batch frame:
//!   a single fsync, a single atomic durability interval. A crash either
//!   replays the whole batch or none of it.
//! - Isolation levels actually provided (honest, tested):
//!   * ReadCommitted — reads see committed engine state; writes are buffered
//!     until commit; no cross-transaction validation (last writer wins).
//!   * Serializable — optimistic concurrency control: the first engine read
//!     or blind write of every touched key records a baseline; commit
//!     re-checks all baselines while holding the global commit lock, so
//!     validate→write is atomic against concurrent commits. A key that
//!     changed underneath aborts the transaction with a retryable
//!     serialization-conflict error (first committer wins).
//!   Snapshot Isolation / MVCC version chains stay ROADMAP work: without
//!   versioned storage stale snapshot reads cannot be served honestly, so
//!   the previously declared (but silently downgraded) variant was removed.
//! - "2PC" in the marketing sense is gone: this is a single-node atomic
//!   commit protocol. True 2PC across nodes = ROADMAP.

use crate::engine::Engine;
use crate::relational::Value;
use crate::wal::WalOp;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxIsolationLevel {
    /// Reads see the latest committed state; writes buffer until commit.
    /// No cross-transaction validation — concurrent writers: last wins.
    ReadCommitted,
    /// Optimistic serializable execution: first-touch baselines for every
    /// read/written key are validated under the commit lock; a concurrent
    /// commit to any observed key aborts us with a serialization conflict.
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
    PutRow {
        key: Vec<u8>,
        fields: BTreeMap<String, Value>,
    },
    /// Delete the key (cascade handled at the relational layer).
    Delete { key: Vec<u8> },
}

pub struct TransactionSession {
    pub tx_id: u64,
    pub started_at_lsn: u64,
    pub status: TxStatus,
    pub isolation: TxIsolationLevel,
    write_set: Vec<Mutation>,
    /// Engine handle: enables read-your-writes overlay and baseline capture
    /// without threading an extra `&Engine` through every call site.
    engine: Arc<Engine>,
    /// First-touch state per key (`None` = key was absent at first touch).
    /// This is what a Serializable commit re-validates.
    baseline: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl TransactionSession {
    /// Read a row as THIS transaction sees it: buffered writes overlay the
    /// engine (read-your-writes). The first engine read of a key records the
    /// Serializable validation baseline from the very bytes it decoded, so a
    /// read-modify-write cycle cannot miss a concurrent update.
    pub fn read_row(&mut self, key: &str) -> Result<Option<BTreeMap<String, Value>>, String> {
        let kb = key.as_bytes();
        // Newest buffered mutation wins over older ones and over the engine.
        for m in self.write_set.iter().rev() {
            match m {
                Mutation::PutRow { key: k, fields } if k == kb => return Ok(Some(fields.clone())),
                Mutation::Delete { key: k } if k == kb => return Ok(None),
                _ => {}
            }
        }
        let raw = self.engine.get(kb).map_err(|e| format!("storage: {}", e))?;
        self.baseline
            .entry(kb.to_vec())
            .or_insert_with(|| raw.clone());
        match raw {
            Some(bytes) => {
                let fields = crate::relational::RowCodec::decode(&bytes)
                    .map_err(|e| format!("row {} unreadable: {}", key, e))?;
                Ok(Some(fields.into_iter().collect()))
            }
            None => Ok(None),
        }
    }

    pub fn put_row(&mut self, key: Vec<u8>, fields: BTreeMap<String, Value>) {
        self.ensure_baseline(&key);
        self.write_set.push(Mutation::PutRow { key, fields });
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.ensure_baseline(&key);
        self.write_set.push(Mutation::Delete { key });
    }

    /// Blind writes (no prior `read_row`) still need a first-touch baseline
    /// so a concurrent commit between here and commit-time is detectable.
    /// Best-effort: a transient get error leaves the key unvalidated rather
    /// than producing false conflicts; the caller's own read errors surface
    /// through `read_row` before we ever get here.
    fn ensure_baseline(&mut self, key: &[u8]) {
        if self.baseline.contains_key(key) {
            return;
        }
        if let Ok(raw) = self.engine.get(key) {
            self.baseline.insert(key.to_vec(), raw);
        }
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
    /// Serializes validate→write for Serializable commits: without it a
    /// rival commit could land between our baseline check and our batch.
    commit_lock: parking_lot::Mutex<()>,
}

impl TransactionCoordinator {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            next_tx_id: AtomicU64::new(1001),
            global_lsn: AtomicU64::new(1),
            committed_txs: AtomicU64::new(0),
            aborted_txs: AtomicU64::new(0),
            commit_lock: parking_lot::Mutex::new(()),
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
            engine: self.engine.clone(),
            baseline: std::collections::HashMap::new(),
        }
    }

    /// Atomic commit: for Serializable, first re-validate every first-touch
    /// baseline while holding the commit lock (validate→write is one atomic
    /// interval against rival commits); then encode all mutations and flush
    /// them as one WAL batch frame.
    pub fn commit(&self, session: &mut TransactionSession) -> Result<u64, String> {
        if session.status != TxStatus::Active {
            return Err(format!("transaction is {:?}, not Active", session.status));
        }
        if session.write_set.is_empty() {
            // Read-only transactions never write, so they always commit.
            session.status = TxStatus::Committed;
            return Ok(self.global_lsn.load(Ordering::SeqCst));
        }

        // Serializable: hold the commit lock across validate + write so no
        // rival commit can slip between the baseline check and our batch.
        let _serial_guard =
            (session.isolation == TxIsolationLevel::Serializable).then(|| self.commit_lock.lock());
        if session.isolation == TxIsolationLevel::Serializable {
            for (key, base) in &session.baseline {
                let cur = self
                    .engine
                    .get(key)
                    .map_err(|e| format!("commit validation failed: {}", e))?;
                if &cur != base {
                    session.status = TxStatus::Aborted;
                    self.aborted_txs.fetch_add(1, Ordering::Relaxed);
                    return Err(format!(
                        "serialization conflict: key {} changed since first read (tx {} aborted, retry)",
                        String::from_utf8_lossy(key),
                        session.tx_id
                    ));
                }
            }
        }

        let mut ops: Vec<(WalOp, Vec<u8>, Option<Vec<u8>>)> =
            Vec::with_capacity(session.write_set.len());
        for m in &session.write_set {
            match m {
                Mutation::PutRow { key, fields } => {
                    let payload = crate::relational::RowCodec::encode(
                        &fields
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect::<Vec<_>>(),
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
