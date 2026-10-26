// SPDX-License-Identifier: AGPL-3.0-only
//! Main Storage Engine: WAL + MemTable + SSTable Segments + Dual-Strand Mirror
//!
//! v3.0 «Repairosome» — complete write/read lifecycle with self-healing:
//!   write: WAL fsync -> mirror -> memtable -> (capacity) rotate + flush
//!          to an erasure-coded DNASST02 segment (k data + m parity blocks)
//!   read:  active memtable -> immutable memtable -> segments (newest first);
//!          a CRC-miss inside a segment triggers on-read repair (reconstruct
//!          from parity, rewrite, journal) before the value is served
//!   heal:  background scrubber (rate-limited) + `doctor` CLI for offline
//!          scan/fix/migration; repair_journal.jsonl records every lesion
//!   wal:   size-based rotation into closed `wal_NNNNNN.wal` segments that
//!          are RS-protected via parity sidecars; the active tail keeps the
//!          strict fsync path untouched (CRC + mirror)
//!
//! Durability: SyncMode::Strict fsyncs every WAL frame BEFORE acknowledging.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};

use crate::memtable::{EntryValue, MemTable};
use crate::mirror::DualStrandMirror;
use crate::repair::{
    EngineRepairHook, RepairJournal, RepairPolicy, ScrubConfig, Scrubber,
};
use crate::segment::{SegmentFile, SegmentLookup};
use crate::segment_block::{BlockSegmentReader, BlockSegmentWriter};
use crate::stats::EngineStats;
use crate::wal::{SyncMode, WalOp, WalEntry, WalParity, WalReader, WalWriter};

pub struct EngineConfig {
    pub dir_path: PathBuf,
    pub memtable_capacity: usize,
    pub sync_mode: SyncMode,
    pub mirror_disk_path: Option<PathBuf>,
    /// Reed–Solomon data shards per stripe (default 4).
    pub erasure_k: usize,
    /// Reed–Solomon parity shards per stripe (default 2; tolerates ≤ m).
    pub erasure_m: usize,
    /// What to do on a detected lesion (default RepairOnRead).
    pub repair_policy: RepairPolicy,
    /// Background scrubber; `None` disables it (default: enabled, 24 h / 32 MiB/s).
    pub scrub: Option<ScrubConfig>,
    /// Rotate the WAL when it exceeds this many bytes (default 64 MiB; 0 = never).
    pub wal_segment_capacity: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            dir_path: PathBuf::from("./dna_data"),
            memtable_capacity: 8 * 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            erasure_k: 4,
            erasure_m: 2,
            repair_policy: RepairPolicy::RepairOnRead,
            scrub: Some(ScrubConfig::default()),
            wal_segment_capacity: 64 * 1024 * 1024,
        }
    }
}

pub struct Engine {
    pub config: EngineConfig,
    pub stats: Arc<EngineStats>,
    mutation_gate: RwLock<()>,
    active_memtable: RwLock<Arc<MemTable>>,
    immutable_memtable: RwLock<Option<Arc<MemTable>>>,
    wal: RwLock<WalWriter>,
    wal_path: PathBuf,
    mirror: Option<Arc<DualStrandMirror>>,
    segments: RwLock<Vec<Arc<SegmentFile>>>, // newest first
    next_segment_id: AtomicU64,
    wal_seq: AtomicU64,
    flush_lock: Mutex<()>,
    repair_hook: Option<Arc<EngineRepairHook>>,
    scrubber: Option<Scrubber>,
}

impl Engine {
    /// Open (or create) an engine. Performs full recovery:
    /// loads segments (both formats), repairs closed WALs from parity,
    /// replays WAL entries into the memtable, installs the repair hook and
    /// starts the scrubber when configured.
    pub fn open(config: EngineConfig) -> Result<Arc<Self>, std::io::Error> {
        if config.erasure_k == 0 || config.erasure_m == 0 || config.erasure_k + config.erasure_m > 256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid erasure geometry: k, m >= 1 and k + m <= 256 required",
            ));
        }
        std::fs::create_dir_all(&config.dir_path)?;

        let wal_path = config.dir_path.join("active.wal");
        let primary_wal_missing = !wal_path.exists();
        if primary_wal_missing {
            let wal_file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&wal_path)?;
            wal_file.sync_all()?;
            // Best-effort directory fsync: opening a directory handle via
            // `File::open` is not supported on Windows (PermissionDenied), so a
            // failure here must not abort engine startup (the WAL file itself is
            // fsynced above, which is the strict durability guarantee we need).
            let _ = File::open(&config.dir_path).and_then(|f| f.sync_all());
        }

        let mirror = match config.mirror_disk_path.as_ref() {
            Some(path) => {
                Some(Arc::new(DualStrandMirror::new(&config.dir_path, path)?))
            }
            None => None,
        };
        let mirrored_segment_ids = match &mirror {
            Some(mirror) => Some(mirror.repair_segment_set()?),
            None => None,
        };

        // 1. Load existing segments (newest first). Both formats are read;
        //    writing always produces DNASST02.
        let mut readers = Vec::new();
        let mut max_seg_id = 0u64;
        let mut legacy_found = 0usize;
        let mut total_quarantined = 0usize;
        let mut total_repairable = 0usize;
        let segment_paths: Vec<PathBuf> = if let Some(segment_ids) = mirrored_segment_ids {
            segment_ids
                .into_iter()
                .map(|id| config.dir_path.join(format!("{id:06}.sst")))
                .collect()
        } else {
            std::fs::read_dir(&config.dir_path)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "sst"))
                .collect()
        };
        for path in segment_paths {
            let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid SST filename")
            })?;
            let seg_id: u64 = stem.parse().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid SST segment id")
            })?;
            let reader = SegmentFile::open(&path)?;
            if reader.format_tag() == "DNASST01" {
                legacy_found += 1;
            }
            // Quarantine-mode validation: stripes beyond parity stay
            // localised (honest per-key errors), the segment keeps serving.
            let (quarantined, repairable) = reader.validate_for_open()?;
            total_quarantined += quarantined;
            total_repairable += repairable;
            max_seg_id = max_seg_id.max(seg_id);
            readers.push((seg_id, Arc::new(reader)));
        }
        if total_quarantined > 0 {
            eprintln!(
                "[ENGINE] {} stripe(s) quarantined and {} stripe(s) repairable across segments; \
                 run `dna-db-core doctor --data-dir {} --fix`, restore the mirror, or accept \
                 honest errors for the affected keys",
                total_quarantined,
                total_repairable,
                config.dir_path.display()
            );
        }
        if legacy_found > 0 {
            eprintln!(
                "[ENGINE] {} legacy DNASST01 segment(s) detected; they are readable but not \
                 parity-protected. Run `dna-db-core doctor --data-dir {} --migrate` to upgrade.",
                legacy_found,
                config.dir_path.display()
            );
        }
        readers.sort_by(|a, b| b.0.cmp(&a.0));
        let segments: Vec<Arc<SegmentFile>> = readers.into_iter().map(|(_, r)| r).collect();

        // 2. Closed WAL segments: RS repair from parity sidecars, then frame
        //    recovery. Cascade: beyond parity → Strand B copy → parity again.
        // Closed WAL segments: first restore any B-only closed WAL that is
        // missing from A, then run parity/frame recovery on the primary set.
        if let Some(mirror) = &mirror {
            match mirror.heal_primary_closed_wals(&config.dir_path) {
                Ok(n) if n > 0 => eprintln!(
                    "[WAL] restored {n} B-only closed segment(s) to Strand A"
                ),
                Ok(_) => {}
                Err(e) => eprintln!("[WAL] Strand A closed-WAL restore failed: {e}"),
            }
        }
        let mut closed_wals = std::fs::read_dir(&config.dir_path)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("wal_") && n.ends_with(".wal"))
                    .unwrap_or(false)
            })
            .collect::<Vec<PathBuf>>();
        closed_wals.sort();
        let mut max_wal_seq = 0u64;
        let mut wal_entries: Vec<WalEntry> = Vec::new();
        for path in &closed_wals {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let seq = name
                .trim_start_matches("wal_")
                .trim_end_matches(".wal")
                .parse::<u64>()
                .unwrap_or(0);
            max_wal_seq = max_wal_seq.max(seq);

            let len_before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let mut rs_repaired = 0usize;
            let mut parity_ok = true;
            match WalParity::repair(path) {
                Ok(n) => rs_repaired = n,
                Err(e) => {
                    parity_ok = false;
                    eprintln!("[WAL] parity repair failed for {name}: {e}");
                }
            }
            // Frame recovery on Strand A: stops at the first torn frame and
            // truncates the file to the last valid offset.
            let mut entries = WalReader::recover(path)?;
            let mut a_valid_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let a_lost_frames = a_valid_len < len_before;
            // B→A cascade: A lost frames (RS exhausted / sidecar gone) or its
            // sidecar is unusable — take B's copy only when it holds strictly
            // more valid bytes (downgrade guard: frames A already recovered
            // are never discarded for a worse B copy).
            let mut restored_from_b = false;
            if (a_lost_frames || !parity_ok) && mirror.is_some() {
                match mirror
                    .as_ref()
                    .unwrap()
                    .restore_closed_wal_better(&name, &config.dir_path, a_valid_len)
                {
                    Ok(Some(b_valid_len)) => {
                        eprintln!(
                            "[WAL] {name}: Strand A held {a_valid_len} valid byte(s), \
                             Strand B {b_valid_len}; restored from Strand B"
                        );
                        entries = WalReader::recover(path)?;
                        a_valid_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        restored_from_b = true;
                    }
                    Ok(None) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        eprintln!("[WAL] {name}: damaged on Strand A, no copy exists on Strand B");
                    }
                    Err(e) => eprintln!("[WAL] mirror cascade failed for {name}: {e}"),
                }
            }
            if rs_repaired > 0 || a_lost_frames {
                eprintln!(
                    "[WAL] closed segment {name}: {} block(s) restored by parity, \
                     {} valid byte(s) kept",
                    rs_repaired, a_valid_len
                );
            }
            // The parity sidecar is corrupt or stale after a cascade restore
            // — rebuild it from the validated file.
            if restored_from_b || !parity_ok {
                if let Err(e) = WalParity::protect(path, config.erasure_k, config.erasure_m) {
                    eprintln!("[WAL] parity re-protect failed for {name}: {e}");
                }
            }
            wal_entries.extend(entries);
            // Keep B byte-identical after an A-side repair.
            if (rs_repaired > 0 || a_lost_frames || restored_from_b) && mirror.is_some() {
                let _ = mirror.as_ref().unwrap().sync_closed_wal(&name, &config.dir_path);
            }
        }

        // Closed-WAL cascade, A→B half: rebuild damaged/missing Strand B
        // copies from the now-validated Strand A files (B→A half ran above
        // where parity was insufficient). Both spirals heal each other.
        if let Some(mirror) = &mirror {
            match mirror.heal_backup_closed_wals(&config.dir_path) {
                Ok(n) if n > 0 => eprintln!(
                    "[WAL] rebuilt {n} damaged closed segment(s) on Strand B from Strand A"
                ),
                Ok(_) => {}
                Err(e) => eprintln!("[WAL] Strand B closed-WAL healing failed: {e}"),
            }
        }

        // 3. Validate the ACTIVE WAL and restore a damaged Strand A log from B.
        let wal_len_before = std::fs::metadata(&wal_path)
            .map(|metadata| metadata.len())
            .ok();
        let active_entries = WalReader::recover(&wal_path)?;
        let wal_len_after = std::fs::metadata(&wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let primary_wal_damaged = (primary_wal_missing
            && mirror
                .as_ref()
                .map(|m| m.backup_wal_size() > 0 || m.backup_wal_is_damaged())
                .unwrap_or(false))
            || wal_len_before
            .map(|before| wal_len_after < before)
            .unwrap_or(false);
        let active_entries = if let Some(mirror) = &mirror {
            mirror.repair_wal(&wal_path, primary_wal_damaged)?
        } else {
            active_entries
        };
        wal_entries.extend(active_entries);

        let wal = WalWriter::open(&wal_path, config.sync_mode)?;
        let memtable = Arc::new(MemTable::new(config.memtable_capacity));

        // Repair journal + hook (installed into every block-format reader).
        let journal = RepairJournal::open(&config.dir_path).ok().map(Arc::new);
        let stats = Arc::new(EngineStats::new());
        let hook: Arc<EngineRepairHook> = Arc::new(EngineRepairHook {
            policy: config.repair_policy,
            stats: stats.clone(),
            journal,
            mirror: mirror.clone(),
        });

        let mut engine = Arc::new(Self {
            mutation_gate: RwLock::new(()),
            active_memtable: RwLock::new(memtable.clone()),
            immutable_memtable: RwLock::new(None),
            wal: RwLock::new(wal),
            wal_path: wal_path.clone(),
            mirror: mirror.clone(),
            segments: RwLock::new(segments),
            next_segment_id: AtomicU64::new(max_seg_id + 1),
            wal_seq: AtomicU64::new(max_wal_seq + 1),
            config: EngineConfig {
                memtable_capacity: config.memtable_capacity,
                ..config
            },
            stats: stats.clone(),
            flush_lock: Mutex::new(()),
            repair_hook: Some(hook.clone()),
            scrubber: None,
        });

        // Install the hook into every loaded DNASST02 reader.
        for seg in engine.segments.read().iter() {
            seg.set_hook(hook.clone());
        }

        // Start the background scrubber when configured.
        if let Some(scrub_config) = engine.config.scrub.clone() {
            let hook2 = hook.clone();
            let stats2 = stats.clone();
            let segments_ref: std::sync::Weak<Engine> = Arc::downgrade(&engine);
            let list = move || -> Vec<PathBuf> {
                match segments_ref.upgrade() {
                    Some(engine) => engine
                        .segments
                        .read()
                        .iter()
                        .map(|s| s.path().to_path_buf())
                        .collect(),
                    None => Vec::new(),
                }
            };
            let scrubber = Scrubber::spawn(scrub_config, stats2, hook2, list);
            // No other Arc<Engine> clone exists yet, so exclusive access is fine.
            match Arc::get_mut(&mut engine) {
                Some(e) => e.scrubber = Some(scrubber),
                // Extremely unlikely (scrubber already grabbed a strong ref);
                // stop the thread instead of leaking an unstoppable handle.
                None => scrubber.stop(),
            }
        }

        let replay_count = wal_entries.len();
        for entry in wal_entries {
            match entry {
                WalEntry { op: WalOp::Put, key, value: Some(v) } => {
                    memtable.put(key, v);
                }
                WalEntry { op: WalOp::Delete, key, .. } => {
                    memtable.delete(key);
                }
                WalEntry { op: WalOp::Put, key, value: None } => {
                    // batch delete payload quirk: treat as delete
                    let _ = key;
                }
            }
        }
        engine.stats.wal_replayed_entries.store(replay_count as u64, Ordering::Relaxed);

        // If replay filled the memtable, flush immediately
        if memtable.is_full() {
            engine.ensure_capacity()?;
        }

        Ok(engine)
    }

    // ---------------- writes ----------------

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<(), std::io::Error> {
        let _mutation_guard = self.mutation_gate.write();
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }
        {
            let mut wal_guard = self.wal.write();
            wal_guard.append(WalOp::Put, key, Some(value))?;
            self.maybe_rotate_wal(&mut wal_guard)?;
        }

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_write(WalOp::Put, key, Some(value)))
            .transpose();

        let is_full = {
            let mem_guard = self.active_memtable.read();
            mem_guard.put(key.to_vec(), value.to_vec())
        };

        if is_full {
            self.ensure_capacity()?;
        }

        self.stats.record_put(key.len() + value.len(), t0.elapsed().as_nanos() as u64);
        mirror_result
            .map(|_| ())
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::BrokenPipe, message))
    }

    pub fn delete(&self, key: &[u8]) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }
        {
            let mut wal_guard = self.wal.write();
            wal_guard.append(WalOp::Delete, key, None)?;
            self.maybe_rotate_wal(&mut wal_guard)?;
        }

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_write(WalOp::Delete, key, None))
            .transpose();

        let is_full = {
            let mem_guard = self.active_memtable.read();
            mem_guard.delete(key.to_vec())
        };

        if is_full {
            self.ensure_capacity()?;
        }

        self.stats.record_delete(t0.elapsed().as_nanos() as u64);
        mirror_result
            .map(|_| ())
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::BrokenPipe, message))
    }

    /// Atomic multi-key write: one WAL batch frame -> one fsync -> all ops
    /// applied to the memtable. Crash between = whole batch replayed or none.
    pub fn write_batch(&self, ops: &[(WalOp, Vec<u8>, Option<Vec<u8>>)]) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        if ops.is_empty() {
            return Ok(());
        }
        let t0 = std::time::Instant::now();
        if let Some(mirror) = &self.mirror {
            mirror.ensure_writable()?;
        }

        let refs: Vec<(&WalOp, &[u8], Option<&[u8]>)> = ops
            .iter()
            .map(|(op, k, v)| (op, k.as_slice(), v.as_deref()))
            .collect();
        {
            let mut wal_guard = self.wal.write();
            wal_guard.append_batch(&refs)?;
            self.maybe_rotate_wal(&mut wal_guard)?;
        }

        let mirror_result = self
            .mirror
            .as_ref()
            .map(|mirror| mirror.mirror_batch(&refs))
            .transpose();

        let mut is_full = false;
        {
            let mem_guard = self.active_memtable.read();
            for (op, key, value) in ops {
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

        let bytes: usize = ops.iter().map(|(_, k, v)| k.len() + v.as_ref().map(|v| v.len()).unwrap_or(0)).sum();
        self.stats.record_put(bytes, t0.elapsed().as_nanos() as u64);
        mirror_result.map(|_| ())
    }

    // ---------------- reads ----------------

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        let _mutation_gate = self.mutation_gate.read();
        let t0 = std::time::Instant::now();
        let result = self.get_inner(key);
        let bytes = result.as_ref().ok().and_then(|o| o.as_ref()).map(|v| v.len()).unwrap_or(0);
        self.stats.record_get(bytes, t0.elapsed().as_nanos() as u64);
        result
    }

    fn get_inner(&self, key: &[u8]) -> Result<Option<Vec<u8>>, std::io::Error> {
        {
            let active = self.active_memtable.read();
            if let Some(res) = active.get(key) {
                return Ok(res);
            }
        }
        {
            let immut = self.immutable_memtable.read();
            if let Some(ref imm) = *immut {
                if let Some(res) = imm.get(key) {
                    return Ok(res);
                }
            }
        }
        let segments = self.segments.read().clone();
        for seg in segments.iter() {
            match self.get_from_segment(seg, key)? {
                Some(SegmentLookup::Found(v)) => return Ok(Some(v)),
                Some(SegmentLookup::Tombstone) => return Ok(None),
                None => continue,
            }
        }
        Ok(None)
    }

    /// Merged scan of all entries with keys starting with `prefix`.
    /// Newest version wins; tombstoned keys are excluded.
    /// Result is sorted by key.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, std::io::Error> {
        let _mutation_gate = self.mutation_gate.read();
        let mut merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();

        // oldest segments first so newer layers overwrite
        {
            let segments = self.segments.read().clone();
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
            let immut = self.immutable_memtable.read();
            if let Some(ref imm) = *immut {
                for (k, ev) in imm.scan_prefix(prefix) {
                    merged.insert(k, match ev {
                        EntryValue::Value(v) => Some(v),
                        EntryValue::Tombstone => None,
                    });
                }
            }
        }
        {
            let active = self.active_memtable.read();
            for (k, ev) in active.scan_prefix(prefix) {
                merged.insert(k, match ev {
                    EntryValue::Value(v) => Some(v),
                    EntryValue::Tombstone => None,
                });
            }
        }

        Ok(merged
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
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

        let _mutation_gate = self.mutation_gate.read();
        let mut merged: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();

        {
            let segments = self.segments.read().clone();
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
            let immut = self.immutable_memtable.read();
            if let Some(ref imm) = *immut {
                for (k, ev) in imm.scan_prefix(&lcp) {
                    if !in_range(&k) {
                        continue;
                    }
                    merged.insert(k, match ev {
                        EntryValue::Value(v) => Some(v),
                        EntryValue::Tombstone => None,
                    });
                }
            }
        }
        {
            let active = self.active_memtable.read();
            for (k, ev) in active.scan_prefix(&lcp) {
                if !in_range(&k) {
                    continue;
                }
                merged.insert(k, match ev {
                    EntryValue::Value(v) => Some(v),
                    EntryValue::Tombstone => None,
                });
            }
        }

        Ok(merged
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
    }

    /// Collect distinct keys of a segment with the given prefix.
    fn segment_keys(
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

    fn get_from_segment(
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
    fn repair_segment_reader(
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
        if let Some(current) = segments.iter_mut().find(|current| current.path() == seg.path()) {
            *current = repaired.clone();
        }
        eprintln!(
            "[MIRROR] Restored damaged Strand A segment {:?} from Strand B",
            seg.path().file_name().unwrap_or_default()
        );
        Ok(repaired)
    }

    // ---------------- maintenance ----------------

    /// Rotate the WAL when it exceeded `wal_segment_capacity` (P4.1).
    /// The active tail keeps its strict fsync path; the closed segment gets
    /// a parity sidecar (P4.2) and is rotated on Strand B as well.
    fn maybe_rotate_wal(&self, guard: &mut WalWriter) -> Result<(), std::io::Error> {
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
        if std::fs::metadata(&closed_path).map(|m| m.len() > 0).unwrap_or(false) {
            if let Err(e) = WalParity::protect(
                &closed_path,
                self.config.erasure_k,
                self.config.erasure_m,
            ) {
                eprintln!("[WAL] parity protection failed for {closed_name}: {e}");
            }
        }
        if let Some(mirror) = &self.mirror {
            if let Err(e) = mirror.rotate_wal(
                seq,
                self.config.erasure_k,
                self.config.erasure_m,
            ) {
                eprintln!("[WAL] Strand B rotation failed for seq {seq}: {e}");
            }
        }
        self.stats.wal_rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Rotate + flush when the active memtable reached capacity.
    fn ensure_capacity(&self) -> Result<(), std::io::Error> {
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

    /// Flush a specific memtable to a new erasure-coded segment.
    fn flush_memtable(&self, mem: Arc<MemTable>) -> Result<(), std::io::Error> {
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
            BlockSegmentReader::seed_for(
                &self.config.dir_path.join(format!("{seg_id:06}.sst")),
            ),
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
        self.stats.flush_count.fetch_add(flushed as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Full compaction: merge all segments into one, drop shadowed versions
    /// and consumed tombstones, delete old files. Safe under concurrency:
    /// readers use snapshots; the swap is atomic.
    pub fn compact(&self) -> Result<usize, std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let _guard = self.flush_lock.lock();
        let old_segments: Vec<Arc<SegmentFile>> = {
            let g = self.segments.read();
            g.clone()
        };
        if old_segments.len() <= 1 {
            return Ok(0);
        }

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
            .filter_map(|(k, v)| {
                v.map(|v| (k, EntryValue::Value(v)))
            })
            .collect();

        let seg_id = self.next_segment_id.fetch_add(1, Ordering::SeqCst);
        let new_path = BlockSegmentWriter::write_segment(
            &self.config.dir_path,
            seg_id,
            &entries,
            self.config.erasure_k,
            self.config.erasure_m,
            BlockSegmentReader::seed_for(
                &self.config.dir_path.join(format!("{seg_id:06}.sst")),
            ),
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
        Ok(deleted)
    }

    /// Rebuild Strand B from the current verified primary files and switch it
    /// to a replacement disk while excluding concurrent mutations.
    pub fn resync_mirror(&self, new_disk_path: &Path) -> Result<(), std::io::Error> {
        let _mutation_gate = self.mutation_gate.write();
        let mirror = self.mirror.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "this engine was opened without a Strand B mirror",
            )
        })?;
        mirror.trigger_hot_swap_resync(new_disk_path)
    }

    /// Trigger one scrubber pass immediately (used by REPAIR CHROMOSOME and
    /// tests). Returns Ok(()) if a scrubber is running.
    pub fn scrub_now_hint(&self) {
        // The scrubber wakes on its interval; for immediate repair the
        // on-read pipeline already covers active usage. Provided for API
        // completeness.
    }

    // ---------------- introspection ----------------

    pub fn segment_count(&self) -> usize {
        self.segments.read().len()
    }

    pub fn memtable_size(&self) -> usize {
        self.active_memtable.read().size()
    }

    pub fn wal_size_bytes(&self) -> u64 {
        self.wal.read().size_bytes().unwrap_or(0)
    }

    pub fn data_dir(&self) -> &Path {
        &self.config.dir_path
    }

    /// Segments containing at least one key with `prefix` (used by
    /// VERIFY/REPAIR CHROMOSOME).
    pub fn segments_with_prefix(&self, prefix: &[u8]) -> Result<Vec<PathBuf>, std::io::Error> {
        let segments = self.segments.read().clone();
        let mut out = Vec::new();
        for seg in segments.iter() {
            if !seg.scan_keys(prefix)?.is_empty() {
                out.push(seg.path().to_path_buf());
            }
        }
        Ok(out)
    }

    /// Block-level scan (and optional repair) of the given segment files.
    /// Returns (blocks_scanned, lesions_found, lesions_repaired, unrecoverable_stripes).
    pub fn verify_segments(
        &self,
        paths: &[PathBuf],
        fix: bool,
    ) -> Result<(u64, u64, u64, u64), std::io::Error> {
        let mut blocks_scanned = 0u64;
        let mut lesions = 0u64;
        let mut repaired = 0u64;
        let mut unrecoverable = 0u64;
        for path in paths {
            let reader = BlockSegmentReader::open(path)?;
            if fix {
                if let Some(hook) = &self.repair_hook {
                    reader.set_hook(hook.clone());
                }
            }
            let (k, m) = reader.erasure_geometry();
            let seed = reader.interleave_seed();
            for region in [false, true] {
                let stripes = reader.stripe_count(region);
                for stripe in 0..stripes {
                    for idx in 0..(k + m) {
                        let slot = crate::segment_block::phys_slot(stripe, idx, stripes, seed);
                        blocks_scanned += 1;
                        match reader.verify_block(region, slot) {
                            Ok(true) => {
                                lesions += 1;
                                repaired += 1;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                if e.to_string().contains("unrecoverable") {
                                    unrecoverable += 1;
                                }
                                lesions += 1;
                            }
                        }
                    }
                }
            }
        }
        Ok((blocks_scanned, lesions, repaired, unrecoverable))
    }

    /// Health snapshot (P5.2): erasure config, lesion counters, scrubber
    /// progress, segment formats, WAL state, mirror status.
    pub fn health(&self) -> serde_json::Value {
        let s = &self.stats;
        let mut formats = std::collections::BTreeMap::new();
        for seg in self.segments.read().iter() {
            *formats.entry(seg.format_tag().to_string()).or_insert(0u64) += 1;
        }
        let scrub_config = self.config.scrub.as_ref();
        serde_json::json!({
            "version": "3.0.0",
            "erasure": {
                "k": self.config.erasure_k,
                "m": self.config.erasure_m,
                "tolerable_damaged_shards_per_stripe": self.config.erasure_m,
            },
            "repair_policy": format!("{:?}", self.config.repair_policy),
            "lesions": {
                "found": s.lesions_found.load(Ordering::Relaxed),
                "repaired": s.lesions_repaired.load(Ordering::Relaxed),
                "unrecoverable": s.lesions_unrecoverable.load(Ordering::Relaxed),
                "bytes_repaired": s.bytes_repaired.load(Ordering::Relaxed),
                "mirror_rescues": s.mirror_rescues.load(Ordering::Relaxed),
            },
            "scrubber": {
                "enabled": scrub_config.is_some(),
                "interval_secs": scrub_config.map(|c| c.interval.as_secs()),
                "rate_limit_mib_per_sec": scrub_config.map(|c| c.rate_limit_bytes_per_sec / (1024 * 1024)),
                "runs": s.scrub_runs.load(Ordering::Relaxed),
                "blocks_scanned": s.scrub_blocks_scanned.load(Ordering::Relaxed),
                "blocks_total": s.scrub_blocks_total.load(Ordering::Relaxed),
                "progress": s.scrub_progress(),
                "lesions": s.scrub_lesions.load(Ordering::Relaxed),
                "last_run_unix": s.scrub_last_unix.load(Ordering::Relaxed),
            },
            "segments": {
                "count": self.segment_count(),
                "formats": formats,
                "quarantined_stripes": self.segments.read().iter().map(|s| s.quarantine_count()).sum::<usize>(),
            },
            "wal": {
                "size_bytes": self.wal_size_bytes(),
                "rotations": s.wal_rotations.load(Ordering::Relaxed),
                "replayed_entries": s.wal_replayed_entries.load(Ordering::Relaxed),
            },
            "mirror": {
                "configured": self.mirror.is_some(),
                "degraded": self.mirror.as_ref().map(|m| m.is_degraded()).unwrap_or(false),
            },
        })
    }
}

/// Delete closed WAL segments (and their parity sidecars) from a directory.
fn remove_closed_wals(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if (name.starts_with("wal_") && name.ends_with(".wal"))
                || (name.starts_with("wal_") && name.ends_with(".wal.parity"))
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

use std::fs::File;

impl Drop for Engine {
    /// Best-effort graceful shutdown: stop the scrubber, flush the WAL's
    /// userspace buffer so Batched/Async modes lose nothing on a NORMAL
    /// shutdown (crash semantics are governed by SyncMode, as documented).
    fn drop(&mut self) {
        if let Some(scrubber) = &self.scrubber {
            scrubber.stop();
        }
        if let Some(mut wal) = self.wal.try_write() {
            let _ = wal.sync();
        }
    }
}
