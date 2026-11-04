// SPDX-License-Identifier: AGPL-3.0-only
//! Read path: point lookups and prefix/range scans across memtables and
//! segments (newest first), value crypto and on-read segment repair.

use super::*;

impl Engine {
    // ---------------- reads ----------------

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        let t0 = std::time::Instant::now();
        let result = self.get_inner(key);
        let bytes = result
            .as_ref()
            .ok()
            .and_then(|o| o.as_ref())
            .map(|v| v.len())
            .unwrap_or(0);
        self.stats.record_get(bytes, t0.elapsed().as_nanos() as u64);
        result
    }

    /// Consistent read snapshot taken under a SHORT mutation-gate read.
    ///
    /// Readers used to hold `mutation_gate.read()` for the whole lookup, which
    /// meant the writer's WAL fsync (potentially milliseconds on strict sync)
    /// blocked every read on the engine. Now readers only grab the gate to
    /// clone Arcs (active/immutable memtable + segment list) and immediately
    /// release it; the Arcs keep every layer alive even if a concurrent flush
    /// swaps the active memtable mid-read. Tombstone semantics plus the
    /// newest-layer-wins merge order keep the result linearizable: a key that
    /// was already written before the read started is always visible in the
    /// snapshotted layers.
    pub(super) fn read_snapshot(
        &self,
    ) -> (
        Arc<MemTable>,
        Option<Arc<MemTable>>,
        Vec<Arc<SegmentFile>>,
    ) {
        let _gate = self.mutation_gate.read();
        let active = self.active_memtable.read().clone();
        let immutable = self.immutable_memtable.read().clone();
        let segments = self.segments.read().clone();
        (active, immutable, segments)
    }

    pub(super) fn get_inner(&self, key: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        let (active, immutable, segments) = self.read_snapshot();
        {
            if let Some(res) = active.get(key) {
                return self.decrypt_optional(key, res);
            }
        }
        {
            if let Some(ref imm) = immutable {
                if let Some(res) = imm.get(key) {
                    return self.decrypt_optional(key, res);
                }
            }
        }
        for seg in segments.iter() {
            match self.get_from_segment(seg, key)? {
                Some(SegmentLookup::Found(v)) => {
                    return self.decrypt_optional(key, Some(v));
                }
                Some(SegmentLookup::Tombstone) => return Ok(None),
                None => continue,
            }
        }
        Ok(None)
    }

    /// Encrypt a payload for the configured at-rest key (identity when
    /// encryption is disabled).
    pub(super) fn encrypt_value(&self, key: &[u8], value: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        match &self.config.crypto {
            Some(crypto) => crypto.encrypt_value(key, value),
            None => Ok(value.to_vec()),
        }
    }

    pub(super) fn decrypt_optional(
        &self,
        key: &[u8],
        value: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, std::io::Error> {
        match value {
            Some(value) => match &self.config.crypto {
                Some(crypto) => crypto.decrypt_value(key, &value).map(Some),
                None => Ok(Some(value)),
            },
            None => Ok(None),
        }
    }

    pub(super) fn decrypt_pairs(
        &self,
        merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, std::io::Error> {
        let mut out = Vec::with_capacity(merged.len());
        for (key, value) in merged {
            if let Some(value) = self.decrypt_optional(&key, value)? {
                out.push((key, value));
            }
        }
        Ok(out)
    }

    /// Merged scan of all entries with keys starting with `prefix`.
    /// Newest version wins; tombstoned keys are excluded.
    /// Result is sorted by key.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, std::io::Error> {
        let (active, immutable, segments) = self.read_snapshot();
        let mut merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();

        // oldest segments first so newer layers overwrite
        {
            for seg in segments.iter().rev() {
                for key in self.segment_keys(seg, prefix)? {
                    // per-key lookup: exact & correct (bloom + block index)
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
        }
        {
            if let Some(ref imm) = immutable {
                for (k, ev) in imm.scan_prefix(prefix) {
                    merged.insert(
                        k,
                        match ev {
                            EntryValue::Value(v) => Some(v),
                            EntryValue::Tombstone => None,
                        },
                    );
                }
            }
        }
        {
            for (k, ev) in active.scan_prefix(prefix) {
                merged.insert(
                    k,
                    match ev {
                        EntryValue::Value(v) => Some(v),
                        EntryValue::Tombstone => None,
                    },
                );
            }
        }

        self.decrypt_pairs(merged)
    }

    /// Ordered range scan: all live keys k with `start` <= k < `end`.
    /// Merge semantics identical to `scan_prefix` (newest layer wins,
    /// tombstones dropped). Segment pruning uses the longest common prefix
    /// of the bounds, so an index range scan over
    /// `__idx:<t>:<col>:<value>` stays bounded to the relevant stripes.
    pub fn scan_range(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, std::io::Error> {
        if start >= end {
            return Ok(Vec::new());
        }
        let lcp: Vec<u8> = start
            .iter()
            .zip(end.iter())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| *a)
            .collect();
        let in_range = |k: &[u8]| k >= start && k < end;

        let (active, immutable, segments) = self.read_snapshot();
        let mut merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();

        {
            for seg in segments.iter().rev() {
                for key in self.segment_keys(seg, &lcp)? {
                    if !in_range(&key) {
                        continue;
                    }
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
        }
        {
            if let Some(ref imm) = immutable {
                for (k, ev) in imm.scan_prefix(&lcp) {
                    if !in_range(&k) {
                        continue;
                    }
                    merged.insert(
                        k,
                        match ev {
                            EntryValue::Value(v) => Some(v),
                            EntryValue::Tombstone => None,
                        },
                    );
                }
            }
        }
        {
            for (k, ev) in active.scan_prefix(&lcp) {
                if !in_range(&k) {
                    continue;
                }
                merged.insert(
                    k,
                    match ev {
                        EntryValue::Value(v) => Some(v),
                        EntryValue::Tombstone => None,
                    },
                );
            }
        }

        self.decrypt_pairs(merged)
    }

    /// Collect distinct keys of a segment with the given prefix.
    pub(super) fn segment_keys(
        &self,
        seg: &Arc<SegmentFile>,
        prefix: &[u8],
    ) -> Result<Vec<Vec<u8>>, std::io::Error> {
        match seg.scan_keys(prefix) {
            Ok(keys) => Ok(keys),
            Err(original) => {
                let repaired = self.repair_segment_reader(seg, &original)?;
                repaired.scan_keys(prefix)
            }
        }
    }

    pub(super) fn get_from_segment(
        &self,
        seg: &Arc<SegmentFile>,
        key: &[u8],
    ) -> Result<Option<SegmentLookup>, std::io::Error> {
        match seg.get(key) {
            Ok(value) => Ok(value),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // The hook restored this segment from Strand B (new inode);
                // reopen it, swap it into the reader set and retry once.
                let fresh = Arc::new(SegmentFile::open(seg.path())?);
                if let Some(hook) = &self.repair_hook {
                    fresh.set_hook(hook.clone());
                }
                fresh.validate_for_open()?;
                {
                    let mut g = self.segments.write();
                    if let Some(current) = g.iter_mut().find(|c| c.path() == seg.path()) {
                        *current = fresh.clone();
                    }
                }
                fresh.get(key)
            }
            Err(original) => {
                let repaired = self.repair_segment_reader(seg, &original)?;
                repaired.get(key)
            }
        }
    }

    /// Last-resort repair: copy the segment from Strand B and reopen it.
    pub(super) fn repair_segment_reader(
        &self,
        seg: &Arc<SegmentFile>,
        original: &std::io::Error,
    ) -> Result<Arc<SegmentFile>, std::io::Error> {
        let mirror = self.mirror.as_ref().ok_or_else(|| {
            std::io::Error::new(
                original.kind(),
                format!("segment read failed and no Strand B is configured: {original}"),
            )
        })?;
        mirror.restore_segment(seg.path()).map_err(|repair_error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "segment read failed ({original}); automatic repair from Strand B failed: {repair_error}"
                ),
            )
        })?;
        let repaired = Arc::new(SegmentFile::open(seg.path())?);
        if let Some(hook) = &self.repair_hook {
            repaired.set_hook(hook.clone());
        }
        repaired.validate_for_open()?;
        let mut segments = self.segments.write();
        if let Some(current) = segments
            .iter_mut()
            .find(|current| current.path() == seg.path())
        {
            *current = repaired.clone();
        }
        eprintln!(
            "[MIRROR] Restored damaged Strand A segment {:?} from Strand B",
            seg.path().file_name().unwrap_or_default()
        );
        Ok(repaired)
    }

}
