// SPDX-License-Identifier: AGPL-3.0-only
//! Write path: put/delete/write_batch (WAL → mirror → memtable), WAL
//! rotation and capacity enforcement.

use super::*;

impl Engine {
    // ---------------- writes ----------------

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<(), std::io::Error> {
        let _mutation_guard = self.mutation_gate.write();
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }
        let stored = self.encrypt_value(key, value)?;
        let wal_mark = {
            let mut wal_guard = self.wal.write();
            let mark = wal_guard.logical_len()?;
            wal_guard.append(WalOp::Put, key, Some(&stored))?;
            self.maybe_rotate_wal(&mut wal_guard)?;
            (wal_guard.path().to_path_buf(), mark)
        };

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_write(WalOp::Put, key, Some(&stored)))
            .transpose();

        // Mirror BEFORE the memtable: a failed mirror write must roll the
        // already-persisted WAL frame back, otherwise a write that returned an
        // error to the caller resurrects itself from the WAL on next restart.
        if let Err(message) = mirror_result {
            self.rollback_wal_mark(&wal_mark);
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, message));
        }

        let is_full = {
            let mem_guard = self.active_memtable.read();
            mem_guard.put(key.to_vec(), stored)
        };

        if is_full {
            self.ensure_capacity()?;
        }

        self.stats
            .record_put(key.len() + value.len(), t0.elapsed().as_nanos() as u64);
        Ok(())
    }

    /// Undo an already-appended WAL record after a mirror failure so a write
    /// that was never acknowledged cannot survive the next restart/replay.
    /// Best-effort: a failed rollback is logged loudly (frame CRCs still make
    /// the tail either fully valid or auto-truncated on open).
    pub(super) fn rollback_wal_mark(&self, mark: &(PathBuf, u64)) {
        let (path, offset) = mark;
        let mut wal_guard = self.wal.write();
        if wal_guard.path() == path.as_path() {
            if let Err(e) = wal_guard.rollback_to(*offset) {
                eprintln!(
                    "[WAL] rollback after mirror failure failed for {}: {e}",
                    path.display()
                );
            }
            return;
        }
        // The WAL was rotated between append and mirror failure: the record
        // now lives at the tail of a closed WAL with no active writer handle.
        drop(wal_guard);
        let outcome = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|f| f.set_len(*offset).and_then(|_| f.sync_all()));
        if let Err(e) = outcome {
            eprintln!(
                "[WAL] rollback of rotated WAL {} failed: {e}",
                path.display()
            );
        }
    }

    pub fn delete(&self, key: &[u8]) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }
        let wal_mark = {
            let mut wal_guard = self.wal.write();
            let mark = wal_guard.logical_len()?;
            wal_guard.append(WalOp::Delete, key, None)?;
            self.maybe_rotate_wal(&mut wal_guard)?;
            (wal_guard.path().to_path_buf(), mark)
        };

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_write(WalOp::Delete, key, None))
            .transpose();

        // See `put`: mirror failure rolls the WAL frame back.
        if let Err(message) = mirror_result {
            self.rollback_wal_mark(&wal_mark);
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, message));
        }

        let is_full = {
            let mem_guard = self.active_memtable.read();
            mem_guard.delete(key.to_vec())
        };

        if is_full {
            self.ensure_capacity()?;
        }

        self.stats.record_delete(t0.elapsed().as_nanos() as u64);
        Ok(())
    }

    /// Atomic multi-key write: one WAL batch frame -> one fsync -> all ops
    /// applied to the memtable. Crash between = whole batch replayed or none.
    pub fn write_batch(
        &self,
        ops: &[(WalOp, Vec<u8>, Option<Vec<u8>>)],
    ) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        if ops.is_empty() {
            return Ok(());
        }
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }

        // Encrypt once, then use the same ciphertext for WAL, Strand B and the
        // memtable. Re-encrypting during replay would produce different
        // nonces for the same logical row and break crash consistency.
        let stored_ops: Vec<(WalOp, Vec<u8>, Option<Vec<u8>>)> = ops
            .iter()
            .map(|(op, key, value)| {
                let stored = match value {
                    Some(value) => Some(self.encrypt_value(key, value)?),
                    None => None,
                };
                Ok((op.clone(), key.clone(), stored))
            })
            .collect::<Result<_, std::io::Error>>()?;
        let refs: Vec<(&WalOp, &[u8], Option<&[u8]>)> = stored_ops
            .iter()
            .map(|(op, k, v)| (op, k.as_slice(), v.as_deref()))
            .collect();
        let wal_mark = {
            let mut wal_guard = self.wal.write();
            let mark = wal_guard.logical_len()?;
            wal_guard.append_batch(&refs)?;
            self.maybe_rotate_wal(&mut wal_guard)?;
            (wal_guard.path().to_path_buf(), mark)
        };

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_batch(&refs))
            .transpose();

        // See `put`: mirror failure rolls the whole batch frame back.
        if let Err(message) = mirror_result {
            self.rollback_wal_mark(&wal_mark);
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, message));
        }

        let mut is_full = false;
        {
            let mem_guard = self.active_memtable.read();
            for (op, key, value) in &stored_ops {
                match op {
                    WalOp::Put => {
                        is_full |= mem_guard.put(key.clone(), value.clone().unwrap_or_default());
                    }
                    WalOp::Delete => {
                        is_full |= mem_guard.delete(key.clone());
                    }
                }
            }
        }
        if is_full {
            self.ensure_capacity()?;
        }

        let bytes: usize = ops
            .iter()
            .map(|(_, k, v)| k.len() + v.as_ref().map(|v| v.len()).unwrap_or(0))
            .sum();
        self.stats.record_put(bytes, t0.elapsed().as_nanos() as u64);
        mirror_result.map(|_| ())
    }

    // ---------------- maintenance ----------------

    /// Rotate the WAL when it exceeded `wal_segment_capacity` (P4.1).
    /// The active tail keeps its strict fsync path; the closed segment gets
    /// a parity sidecar (P4.2) and is rotated on Strand B as well.
    pub(super) fn maybe_rotate_wal(&self, guard: &mut WalWriter) -> Result<(), std::io::Error> {
        let capacity = self.config.wal_segment_capacity;
        if capacity == 0 || guard.size_bytes()? < capacity {
            return Ok(());
        }
        let seq = self.wal_seq.fetch_add(1, Ordering::SeqCst);
        guard.sync()?;
        let closed_name = format!("wal_{seq:06}.wal");
        let closed_path = self.config.dir_path.join(&closed_name);
        std::fs::rename(&self.wal_path, &closed_path)?;
        let fresh = WalWriter::open(&self.wal_path, self.config.sync_mode)?;
        let old = std::mem::replace(guard, fresh);
        drop(old);
        // Best-effort directory fsync (see Engine::open): Windows cannot open
        // directory handles via File::open, so failure is tolerated here.
        let _ = File::open(&self.config.dir_path).and_then(|f| f.sync_all());

        // RS-protect the frozen stream (non-fatal on failure: frame CRCs and
        // the mirror still cover it).
        if std::fs::metadata(&closed_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            if let Err(e) =
                WalParity::protect(&closed_path, self.config.erasure_k, self.config.erasure_m)
            {
                eprintln!("[WAL] parity protection failed for {closed_name}: {e}");
            }
        }
        if let Some(mirror) = &self.mirror {
            if let Err(e) = mirror.rotate_wal(seq, self.config.erasure_k, self.config.erasure_m) {
                eprintln!("[WAL] Strand B rotation failed for seq {seq}: {e}");
            }
        }
        self.stats.wal_rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Rotate + flush when the active memtable reached capacity.
    pub(super) fn ensure_capacity(&self) -> Result<(), std::io::Error> {
        if !self.active_memtable.read().is_full() {
            return Ok(());
        }
        let _guard = self.flush_lock.lock();
        // double-check under the flush lock
        if !self.active_memtable.read().is_full() {
            return Ok(());
        }
        let old = {
            let mut active_guard = self.active_memtable.write();
            let old = active_guard.clone();
            *active_guard = Arc::new(MemTable::new(self.config.memtable_capacity));
            old
        };
        if let Err(error) = self.flush_memtable(old.clone()) {
            *self.active_memtable.write() = old;
            return Err(error);
        }
        self.stats.flush_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

}
