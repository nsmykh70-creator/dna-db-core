// SPDX-License-Identifier: AGPL-3.0-only
//! Repairosome — the repair engine of DNA-DB v3.0.
//!
//! Implements the DNA-repair analogy end to end:
//!
//! * *checkpoint kinases*  — CRC-32 (IEEE, crc32fast) on every 4 KiB block (segment_block.rs)
//! * *excision repair*     — on-read repair: detect → reconstruct from
//!   surviving parity shards → rewrite → journal
//! * *enzyme scanners*     — background scrubber with rate limit (see [`scrub`])
//! * *mutation journal*    — repair_journal (JSONL, human-auditable)
//! * *doctor*              — offline scan / fix / DNASST01→02 migration (see [`doctor`])
//!
//! The [`EngineRepairHook`] implements [`RepairHook`] and is installed into
//! every DNASST02 segment reader by the engine. It counts lesions in
//! [`EngineStats`], appends to the journal file, and escalates to the
//! Strand B mirror when parity is insufficient (homologous recombination).
//!
//! Module map (split from the former repair.rs monolith, no behaviour change):
//! * [`scrub`]  — background scrubber: `ScrubConfig`, `Scrubber`, `SegmentVerdict`
//! * [`doctor`] — offline `doctor_scan` / `doctor_fix` / `doctor_migrate` /
//!   `doctor_cascade_strands` and the segment scan/fix/migrate helpers

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::mirror::DualStrandMirror;
use crate::segment_block::RepairHook;
use crate::stats::EngineStats;

mod doctor;
mod scrub;
#[cfg(test)]
mod tests;

pub use doctor::{
    doctor_cascade_strands, doctor_fix, doctor_migrate, doctor_scan, fix_segment, CascadeReport,
    DoctorReport,
};
pub use scrub::{ScrubConfig, Scrubber, SegmentVerdict};

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// What the engine is allowed to do when it detects a lesion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPolicy {
    /// Detect on read, reconstruct from parity, rewrite, then serve the
    /// value (strict semantics: the client never sees damaged data).
    RepairOnRead,
    /// Detect and count during reads, but only the scrubber repairs.
    ScrubOnly,
    /// Detect only; every lesion is an error (offline forensics mode).
    Disabled,
}

impl Default for RepairPolicy {
    fn default() -> Self {
        RepairPolicy::RepairOnRead
    }
}

// ---------------------------------------------------------------------------
// Repair journal ("журнал мутаций")
// ---------------------------------------------------------------------------

/// One journal record, serialised as JSONL: {ts, segment, stripe, shards, action, bytes}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepairRecord {
    /// Unix time, seconds.
    pub ts: u64,
    /// Segment file name (e.g. "000007.sst").
    pub segment: String,
    /// Interleaved stripe index within its region.
    pub stripe: usize,
    /// Damaged shard slots (region-relative).
    pub shards: Vec<usize>,
    /// "data" | "index"
    pub region: String,
    /// "repaired" | "unrecoverable" | "escalated_to_mirror"
    pub action: String,
    /// Bytes rewritten (0 for unrecoverable).
    pub bytes: u64,
}

/// Append-only JSONL journal stored next to the data (`repair_journal.jsonl`).
pub struct RepairJournal {
    file: Mutex<Option<File>>,
    path: PathBuf,
    pub records_written: std::sync::atomic::AtomicU64,
}

impl RepairJournal {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join("repair_journal.jsonl");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file: Mutex::new(Some(file)),
            path,
            records_written: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, rec: &RepairRecord) {
        let line = match serde_json::to_string(rec) {
            Ok(l) => l,
            Err(_) => return, // journal must never break the repair path
        };
        if let Some(f) = self.file.lock().as_mut() {
            let _ = writeln!(f, "{}", line);
            let _ = f.flush();
            self.records_written.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read all records (for `doctor` / health reporting).
    pub fn read_all(&self) -> Vec<RepairRecord> {
        let mut out = Vec::new();
        if let Ok(f) = File::open(&self.path) {
            for line in io::BufReader::new(f).lines().flatten() {
                if let Ok(rec) = serde_json::from_str::<RepairRecord>(&line) {
                    out.push(rec);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Engine hook: policy + stats + journal + mirror cascade
// ---------------------------------------------------------------------------

pub struct EngineRepairHook {
    pub policy: RepairPolicy,
    pub stats: Arc<EngineStats>,
    pub journal: Option<Arc<RepairJournal>>,
    pub mirror: Option<Arc<DualStrandMirror>>,
}

impl EngineRepairHook {
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn journalise(
        &self,
        segment: &Path,
        stripe: usize,
        shards: &[usize],
        region: &str,
        action: &str,
        bytes: u64,
    ) {
        if let Some(j) = &self.journal {
            j.append(&RepairRecord {
                ts: Self::now(),
                segment: segment
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string(),
                stripe,
                shards: shards.to_vec(),
                region: region.to_string(),
                action: action.to_string(),
                bytes,
            });
        }
    }
}

impl RepairHook for EngineRepairHook {
    fn repair_on_read(&self) -> bool {
        self.policy == RepairPolicy::RepairOnRead
    }

    fn on_repaired(&self, segment: &Path, stripe: usize, shards: &[usize], bytes: u64) {
        self.stats
            .lesions_found
            .fetch_add(shards.len() as u64, Ordering::Relaxed);
        self.stats
            .lesions_repaired
            .fetch_add(shards.len() as u64, Ordering::Relaxed);
        self.stats
            .bytes_repaired
            .fetch_add(bytes, Ordering::Relaxed);
        self.journalise(segment, stripe, shards, "data", "repaired", bytes);
    }

    fn on_unrecoverable(&self, segment: &Path, stripe: usize, shards: &[usize]) -> bool {
        self.stats
            .lesions_found
            .fetch_add(shards.len() as u64, Ordering::Relaxed);
        self.journalise(segment, stripe, shards, "data", "escalated_to_mirror", 0);
        // Homologous recombination: pull the segment copy from Strand B.
        // restore_segment overwrites the primary file IN PLACE (same inode),
        // so open readers keep working.
        if let Some(mirror) = &self.mirror {
            match mirror.restore_segment(segment) {
                Ok(()) => {
                    self.stats.mirror_rescues.fetch_add(1, Ordering::Relaxed);
                    self.journalise(segment, stripe, shards, "data", "repaired_from_strand_b", 0);
                    return true; // ask the reader to retry
                }
                Err(e) => {
                    eprintln!(
                        "[REPAIR] mirror escalation failed for {:?}: {}",
                        segment.file_name().unwrap_or_default(),
                        e
                    );
                }
            }
        }
        self.stats
            .lesions_unrecoverable
            .fetch_add(shards.len() as u64, Ordering::Relaxed);
        self.journalise(segment, stripe, shards, "data", "unrecoverable", 0);
        false
    }
}
