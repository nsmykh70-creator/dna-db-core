// SPDX-License-Identifier: AGPL-3.0-only
//! Flush: memtable → erasure-coded segment, engine-wide `flush()` and
//! checkpoint (closed-WAL cleanup + history rotation).

use super::*;

impl Engine {
    /// Flush a specific memtable to a new erasure-coded segment.
    pub(super) fn flush_memtable(&self, mem: Arc<MemTable>) -> Result<(), std::io::Error> {
        if mem.is_empty() {
            return Ok(());
        }
        let entries: Vec<(Vec<u8>, EntryValue)> = mem.iter().collect();
        let seg_id = self.next_segment_id.fetch_add(1, Ordering::SeqCst);
        let path = BlockSegmentWriter::write_segment(
            &self.config.dir_path,
            seg_id,
            &entries,
            self.config.erasure_k,
            self.config.erasure_m,
            BlockSegmentReader::seed_for(&self.config.dir_path.join(format!("{seg_id:06}.sst"))),
        )?;
        if let Some(mirror) = &self.mirror {
            mirror.mirror_segment(&path)?;
            let mut active_ids: Vec<u64> = self
                .segments
                .read()
                .iter()
                .filter_map(|segment| {
                    segment
                        .path()
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .and_then(|stem| stem.parse().ok())
                })
                .collect();
            active_ids.push(seg_id);
            active_ids.sort_unstable();
            active_ids.dedup();
            mirror.update_segment_manifest(&active_ids)?;
        }
        let reader = Arc::new(SegmentFile::open(&path)?);
        if let Some(hook) = &self.repair_hook {
            reader.set_hook(hook.clone());
        }
        self.segments.write().insert(0, reader);
        Ok(())
    }

    /// Manually rotate the active memtable and flush it.
    pub fn flush(&self) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let _guard = self.flush_lock.lock();
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

    /// Durability checkpoint: flush everything, then truncate the WAL.
    /// After a checkpoint, recovery loads data purely from segments.
    pub fn checkpoint(&self) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let _guard = self.flush_lock.lock();
        // 1. flush immutable if any
        let mut flushed = 0usize;
        let imm = self.immutable_memtable.write().take();
        if let Some(imm) = imm {
            if let Err(error) = self.flush_memtable(imm.clone()) {
                *self.immutable_memtable.write() = Some(imm);
                return Err(error);
            }
            flushed += 1;
        }
        // 2. flush active
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
        flushed += 1;
        // 3. truncate WAL (safe: all data now in segments) and, only after a
        //    successful reset, drop the closed WALs absorbed by segments.
        if let Some(ref mirror) = self.mirror {
            mirror.reset_wal()?;
        }
        {
            let mut wal_guard = self.wal.write();
            wal_guard.reset()?;
        }
        if let Some(ref mirror) = self.mirror {
            mirror.remove_closed_wals(&self.config.dir_path);
        }
        remove_closed_wals(&self.config.dir_path);
        self.stats
            .flush_count
            .fetch_add(flushed as u64, Ordering::Relaxed);
        Ok(())
    }

}
