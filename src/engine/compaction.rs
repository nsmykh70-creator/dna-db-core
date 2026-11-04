// SPDX-License-Identifier: AGPL-3.0-only
//! Full compaction: merge all segments, drop shadowed versions and
//! consumed tombstones; record runs in compaction_history.jsonl.

use super::*;

impl Engine {
    /// and consumed tombstones, delete old files. Safe under concurrency:
    /// readers use snapshots; the swap is atomic.
    ///
    /// Returns the number of superseded files removed (0 when there is
    /// nothing to merge). The completed run is appended to the on-disk
    /// compaction history and exported through `dnadb_compaction_*` metrics.
    pub fn compact(&self) -> Result<usize, std::io::Error> {
        Ok(self
            .compact_with_trigger("manual")?
            .map(|record| record.deleted_files)
            .unwrap_or(0))
    }

    /// Like [`Engine::compact`], but tags the history record with `trigger`
    /// and returns the full result. `Ok(None)` means "nothing to compact"
    /// (one or zero segments) — no history entry is written for a no-op.
    pub fn compact_with_trigger(
        &self,
        trigger: &str,
    ) -> Result<Option<CompactionRecord>, std::io::Error> {
        let started = std::time::Instant::now();
        let _mutation_gate = self.mutation_gate.write();
        let _guard = self.flush_lock.lock();
        let old_segments: Vec<Arc<SegmentFile>> = {
            let g = self.segments.read();
            g.clone()
        };
        if old_segments.len() <= 1 {
            return Ok(None);
        }
        // Honest "before" footprint of the whole data directory, taken before
        // any merge/deletion work happens.
        let bytes_before = self.dir_size_bytes();

        // Merge oldest -> newest so newer versions win
        let mut merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();
        for seg in old_segments.iter().rev() {
            for key in self.segment_keys(seg, &[])? {
                match self.get_from_segment(seg, &key)? {
                    Some(SegmentLookup::Found(v)) => {
                        merged.insert(key, Some(v));
                    }
                    Some(SegmentLookup::Tombstone) => {
                        merged.insert(key, None);
                    }
                    None => {}
                }
            }
        }

        // Tombstones with no older live data anywhere below can be dropped
        let entries: Vec<(Vec<u8>, EntryValue)> = merged
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, EntryValue::Value(v))))
            .collect();

        let seg_id = self.next_segment_id.fetch_add(1, Ordering::SeqCst);
        let new_path = BlockSegmentWriter::write_segment(
            &self.config.dir_path,
            seg_id,
            &entries,
            self.config.erasure_k,
            self.config.erasure_m,
            BlockSegmentReader::seed_for(&self.config.dir_path.join(format!("{seg_id:06}.sst"))),
        )?;
        if let Some(mirror) = &self.mirror {
            mirror.mirror_segment(&new_path)?;
            mirror.update_segment_manifest(&[seg_id])?;
        }
        let new_reader = Arc::new(SegmentFile::open(&new_path)?);
        if let Some(hook) = &self.repair_hook {
            new_reader.set_hook(hook.clone());
        }

        // Atomic swap
        {
            let mut g = self.segments.write();
            *g = vec![new_reader];
        }

        // Delete old files (segments + bloom sidecars)
        let mut deleted = 0usize;
        for seg in &old_segments {
            let p = seg.path();
            if p.extension().map(|e| e == "sst").unwrap_or(false) {
                let old_id = p
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<u64>().ok());
                match std::fs::remove_file(p) {
                    Ok(()) => deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!(
                        "[COMPACT] Could not remove superseded segment {:?}: {e}",
                        p.file_name().unwrap_or_default()
                    ),
                }
                let bloom = p.with_extension("bloom");
                if bloom.exists() {
                    match std::fs::remove_file(&bloom) {
                        Ok(()) => deleted += 1,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => eprintln!(
                            "[COMPACT] Could not remove bloom sidecar {:?}: {e}",
                            bloom.file_name().unwrap_or_default()
                        ),
                    }
                }
                if let (Some(mirror), Some(old_id)) = (&self.mirror, old_id) {
                    if let Err(error) = mirror.remove_segment(old_id) {
                        eprintln!(
                            "[MIRROR] Could not remove superseded Strand B segment {old_id}: {error}"
                        );
                    }
                }
            }
        }
        let _ = File::open(&self.config.dir_path).and_then(|f| f.sync_all());

        self.stats.compaction_count.fetch_add(1, Ordering::Relaxed);

        let record = CompactionRecord {
            ts_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            trigger: trigger.to_string(),
            segments_before: old_segments.len(),
            segments_after: 1,
            deleted_files: deleted,
            bytes_before,
            bytes_after: bytes_before, // corrected by record_compaction
            saved_bytes: 0,
            duration_ms: started.elapsed().as_millis() as u64,
        };
        let corrected = self.record_compaction(record);
        Ok(Some(corrected))
    }

    /// Append a completed compaction to the on-disk history, refresh the
    /// last-compaction Prometheus gauges and log one line. Recomputes
    /// `bytes_after` from disk so the record reflects the real post-compaction
    /// footprint (old files are already unlinked) and returns the corrected
    /// record for the caller's response.
    pub(super) fn record_compaction(&self, mut record: CompactionRecord) -> CompactionRecord {
        let after = self.dir_size_bytes();
        record.bytes_after = after;
        record.saved_bytes = record.bytes_before.saturating_sub(after);
        record.duration_ms = record.duration_ms.max(1);
        let s = &self.stats;
        s.compaction_last_bytes_before
            .store(record.bytes_before, Ordering::Relaxed);
        s.compaction_last_bytes_after
            .store(record.bytes_after, Ordering::Relaxed);
        s.compaction_last_duration_ms
            .store(record.duration_ms, Ordering::Relaxed);
        s.compaction_last_unix.store(record.ts_unix, Ordering::Relaxed);
        eprintln!(
            "[COMPACT] done trigger='{}' segments {}→{} files removed {} bytes {}→{} (saved {}) in {} ms",
            record.trigger,
            record.segments_before,
            record.segments_after,
            record.deleted_files,
            record.bytes_before,
            record.bytes_after,
            record.saved_bytes,
            record.duration_ms,
        );
        let path = self.config.dir_path.join(COMPACTION_HISTORY_FILE);
        let mut entries = self.compaction_history();
        entries.push(record.clone());
        // Cap the file: keep the newest COMPACTION_HISTORY_CAP records.
        if entries.len() > COMPACTION_HISTORY_CAP {
            let keep = entries[entries.len() - COMPACTION_HISTORY_CAP..].to_vec();
            entries = keep;
        }
        let rewrite = entries.len() >= COMPACTION_HISTORY_CAP
            || !path.exists()
            || std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(false);
        let payload = entries
            .iter()
            .filter_map(|r| serde_json::to_string(r).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let payload = payload + "\n";
        let result = if rewrite {
            std::fs::write(&path, payload)
        } else {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| f.write_all(payload.as_bytes()))
        };
        if let Err(e) = result {
            eprintln!(
                "[COMPACT] could not update {}: {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                e
            );
        }
        record
    }

    /// Completed compactions, newest first (on-disk history file).
    pub fn compaction_history(&self) -> Vec<CompactionRecord> {
        let path = self.config.dir_path.join(COMPACTION_HISTORY_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut out: Vec<CompactionRecord> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.ts_unix));
        out
    }

}
