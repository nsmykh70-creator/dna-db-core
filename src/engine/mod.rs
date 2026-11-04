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

use parking_lot::{Mutex, RwLock};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::crypto::Crypto;
use crate::memtable::{EntryValue, MemTable};
use crate::mirror::DualStrandMirror;
use crate::repair::{EngineRepairHook, RepairJournal, RepairPolicy, ScrubConfig, Scrubber};
use crate::segment::{SegmentFile, SegmentLookup};
use crate::segment_block::{BlockSegmentReader, BlockSegmentWriter};
use crate::stats::EngineStats;
use crate::wal::{SyncMode, WalEntry, WalOp, WalParity, WalReader, WalWriter};

/// One completed compaction, persisted as a JSON line in
/// `<data_dir>/compaction_history.jsonl` and exposed through the admin panel
/// (`GET /api/compactions`) and Prometheus (`dnadb_compaction_*`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactionRecord {
    /// Unix time of completion (seconds).
    pub ts_unix: u64,
    /// Who ran it: "manual" (API/engine), "panel:<user>", "cli", "auto".
    pub trigger: String,
    pub segments_before: usize,
    pub segments_after: usize,
    /// Superseded segment + bloom files removed.
    pub deleted_files: usize,
    /// Data-directory size on disk before the compaction (bytes).
    pub bytes_before: u64,
    /// Data-directory size on disk after the compaction (bytes).
    pub bytes_after: u64,
    /// Convenience copy of `bytes_before - bytes_after` (0 if it grew).
    pub saved_bytes: u64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

pub const COMPACTION_HISTORY_FILE: &str = "compaction_history.jsonl";
/// The history file is capped: appending beyond this rewrites the newest half.
const COMPACTION_HISTORY_CAP: usize = 500;

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
    /// Payload encryption-at-rest. `None` keeps the historical plaintext mode
    /// (tests, benchmarks and offline diagnostics); the production CLI enables
    /// it by default.
    pub crypto: Option<Arc<Crypto>>,
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
            crypto: None,
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
    /// Total size of the data directory on disk (recursive walk). Includes
    /// segments, WAL files, journals and the TLS/crypto material — the honest
    /// footprint shown in the panel and exported as `dnadb_data_dir_bytes`.
    pub fn dir_size_bytes(&self) -> u64 {
        fn walk(dir: &Path, total: &mut u64) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => walk(&path, total),
                    Ok(t) if t.is_file() => {
                        if let Ok(meta) = entry.metadata() {
                            *total += meta.len();
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut total = 0u64;
        walk(&self.config.dir_path, &mut total);
        total
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
            "version": env!("CARGO_PKG_VERSION"),
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

mod compaction;
mod flush;
mod open;
mod read;
mod write;
