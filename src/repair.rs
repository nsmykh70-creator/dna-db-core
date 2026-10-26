// SPDX-License-Identifier: AGPL-3.0-only
//! Repairosome — the repair engine of DNA-DB v3.0.
//!
//! Implements the DNA-repair analogy end to end:
//!
//! * *checkpoint kinases*  — CRC32C on every 4 KiB block (segment_block.rs)
//! * *excision repair*     — on-read repair: detect → reconstruct from
//!   surviving parity shards → rewrite → journal
//! * *enzyme scanners*     — background scrubber with rate limit
//! * *mutation journal*    — repair_journal (JSONL, human-auditable)
//! * *doctor*              — offline scan / fix / DNASST01→02 migration
//!
//! The [`EngineRepairHook`] implements [`RepairHook`] and is installed into
//! every DNASST02 segment reader by the engine. It counts lesions in
//! [`EngineStats`], appends to the journal file, and escalates to the
//! Strand B mirror when parity is insufficient (homologous recombination).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::mirror::DualStrandMirror;
use crate::segment_block::{phys_slot, RepairHook, BLOCK_SIZE};
use crate::stats::EngineStats;

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

// ---------------------------------------------------------------------------
// Scrubber — the background enzyme scanner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScrubConfig {
    /// Interval between full scrub passes.
    pub interval: Duration,
    /// Upper bound on scrubber read bandwidth.
    pub rate_limit_bytes_per_sec: u64,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(24 * 3600),
            rate_limit_bytes_per_sec: 32 * 1024 * 1024,
        }
    }
}

/// Handle to stop the scrubber and read its progress.
pub struct Scrubber {
    stop: Arc<std::sync::atomic::AtomicBool>,
    wake: Arc<std::sync::atomic::AtomicBool>,
    pub progress: Arc<EngineStats>,
}

impl Scrubber {
    /// Spawn the background scrubber. `segments` is re-snapshotted via
    /// `list_segments` on every pass, so compaction-superseded files are
    /// never scanned (coordination contract, P3.3 / P4.4).
    pub fn spawn<F>(
        config: ScrubConfig,
        stats: Arc<EngineStats>,
        hook: Arc<EngineRepairHook>,
        list_segments: F,
    ) -> Self
    where
        F: Fn() -> Vec<PathBuf> + Send + 'static,
    {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let wake = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake2 = wake.clone();
        let progress = stats.clone();
        std::thread::Builder::new()
            .name("dna-scrubber".into())
            .spawn(move || {
                loop {
                    // Sleep in small slices so shutdown stays responsive.
                    let mut waited = Duration::ZERO;
                    while waited < config.interval {
                        if stop2.load(Ordering::Relaxed) {
                            return;
                        }
                        if wake2.swap(false, Ordering::Relaxed) {
                            break; // immediate pass requested
                        }
                        let slice = Duration::from_secs(1).min(config.interval - waited);
                        std::thread::sleep(slice);
                        waited += slice;
                    }
                    if stop2.load(Ordering::Relaxed) {
                        return;
                    }
                    // Active tier first: engine stores segments newest-first;
                    // the list closure already returns that order.
                    let segments = list_segments();
                    stats.scrub_runs.fetch_add(1, Ordering::Relaxed);
                    // Pre-count blocks so progress is meaningful during the
                    // pass. A segment that cannot be opened is skipped (doctor
                    // reports it); it contributes zero.
                    let mut total_blocks = 0u64;
                    for seg in &segments {
                        if let Ok(reader) = crate::segment_block::BlockSegmentReader::open(seg) {
                            total_blocks += reader.block_count() as u64;
                        }
                    }
                    stats.scrub_blocks_scanned.store(0, Ordering::Relaxed);
                    stats
                        .scrub_blocks_total
                        .store(total_blocks, Ordering::Relaxed);
                    for seg in &segments {
                        if stop2.load(Ordering::Relaxed) {
                            return;
                        }
                        scrub_one_segment(seg, &hook, &stats, config.rate_limit_bytes_per_sec);
                    }
                    stats.scrub_last_unix.store(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                        Ordering::Relaxed,
                    );
                }
            })
            .expect("spawn scrubber");
        Self {
            stop,
            wake,
            progress,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Ask the scrubber to start a pass now instead of waiting for its timer.
    pub fn wake(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }
}

/// Verify every block of one segment (rate-limited), repairing lesions.
/// Returns blocks scanned. Superseded (deleted) files are skipped gracefully.
fn scrub_one_segment(
    path: &Path,
    hook: &Arc<EngineRepairHook>,
    stats: &Arc<EngineStats>,
    rate_limit: u64,
) -> u64 {
    // If compaction deleted this file between snapshot and now — skip.
    if !path.exists() {
        return 0;
    }
    let reader = match crate::segment_block::BlockSegmentReader::open(path) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!(
                "[SCRUB] cannot open {:?}: {} (needs doctor)",
                path.file_name().unwrap_or_default(),
                e
            );
            return 0;
        }
    };
    reader.set_hook(hook.clone());
    let blocks = reader.block_count() as u64;
    stats
        .scrub_blocks_total
        .fetch_add(blocks, Ordering::Relaxed);

    // Rate limit: pace the sweep at `rate_limit` bytes/sec.
    let per_block_delay = if rate_limit == 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos((BLOCK_SIZE as u64) * 1_000_000_000 / rate_limit)
    };

    let seed = reader.interleave_seed();
    for region in [false, true] {
        let stripes = reader.stripe_count(region);
        for stripe in 0..stripes {
            // Coordination: file may vanish mid-sweep (compaction). Re-check.
            if !path.exists() {
                return blocks;
            }
            for idx in 0..(reader.erasure_geometry().0 + reader.erasure_geometry().1) {
                let slot = phys_slot(stripe, idx, stripes, seed);
                match reader.verify_block(region, slot) {
                    Ok(true) => {
                        stats.scrub_lesions.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // Over-parity damage: counted by the hook, honest error.
                        eprintln!(
                            "[SCRUB] {:?} stripe {}: {}",
                            path.file_name().unwrap_or_default(),
                            stripe,
                            e
                        );
                    }
                }
                if !per_block_delay.is_zero() {
                    std::thread::sleep(per_block_delay);
                }
                stats.scrub_blocks_scanned.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// Doctor — offline scan / fix / migrate
// ---------------------------------------------------------------------------

/// Verdict for one segment in a doctor report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentVerdict {
    pub file: String,
    pub format: String,
    pub records: u64,
    pub blocks_total: usize,
    pub damaged_blocks: usize,
    pub affected_stripes: usize,
    pub unrecoverable_stripes: usize,
    /// HEALTHY | REPAIRABLE | UNREPAIRABLE | STRUCTURAL_DAMAGE
    pub verdict: &'static str,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    pub data_dir: String,
    pub scanned_segments: usize,
    pub healthy: usize,
    pub repairable: usize,
    pub unrecoverable: usize,
    pub migrated: usize,
    pub fixed_blocks: usize,
    pub verdicts: Vec<SegmentVerdict>,
    /// Non-fatal failures encountered while fixing (doctor continues past
    /// unrecoverable segments instead of aborting the whole run).
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Recompute aggregate counters from per-segment verdicts. Keeps fix/migrate
/// reports consistent after verdicts change.
fn recount(report: &mut DoctorReport) {
    report.scanned_segments = report.verdicts.len();
    report.healthy = 0;
    report.repairable = 0;
    report.unrecoverable = 0;
    for v in &report.verdicts {
        match v.verdict {
            "HEALTHY" => report.healthy += 1,
            "REPAIRABLE" => report.repairable += 1,
            _ => report.unrecoverable += 1,
        }
    }
}

/// Scan every .sst in `dir` (offline, no engine open). Read-only.
pub fn doctor_scan(dir: &Path) -> io::Result<DoctorReport> {
    let mut report = DoctorReport {
        data_dir: dir.display().to_string(),
        scanned_segments: 0,
        healthy: 0,
        repairable: 0,
        unrecoverable: 0,
        migrated: 0,
        fixed_blocks: 0,
        verdicts: Vec::new(),
        errors: Vec::new(),
    };
    for path in sst_files(dir)? {
        let verdict = scan_one(&path);
        match verdict.verdict {
            "HEALTHY" => report.healthy += 1,
            "REPAIRABLE" => report.repairable += 1,
            _ => report.unrecoverable += 1,
        }
        report.scanned_segments += 1;
        report.verdicts.push(verdict);
    }
    Ok(report)
}

/// Scan + repair every fixable segment. A segment that cannot be fixed is
/// recorded in `errors` and the run continues, so one unrecoverable stripe
/// does not hide the state of the remaining segments.
pub fn doctor_fix(dir: &Path) -> io::Result<DoctorReport> {
    let mut report = doctor_scan(dir)?;
    for (i, v) in report.verdicts.clone().iter().enumerate() {
        if v.verdict != "REPAIRABLE" {
            continue;
        }
        let path = dir.join(&v.file);
        match fix_segment(&path) {
            Ok(fixed) => {
                report.fixed_blocks += fixed;
                report.verdicts[i] = scan_one(&path);
            }
            Err(e) => {
                report.errors.push(format!("{}: {}", v.file, e));
                let mut after = scan_one(&path);
                after.verdict = "UNREPAIRABLE";
                after.detail = Some(format!("fix failed: {e}"));
                report.verdicts[i] = after;
            }
        }
    }
    recount(&mut report);
    Ok(report)
}

/// Migrate all DNASST01 segments to DNASST02 (manual, per roadmap Q2).
/// Damaged legacy records are reported and dropped — parity cannot be
/// conjured for data that was already corrupt.
pub fn doctor_migrate(dir: &Path, erasure_k: usize, erasure_m: usize) -> io::Result<DoctorReport> {
    let mut report = doctor_scan(dir)?;
    for path in sst_files(dir)? {
        // Detect legacy by footer magic.
        if !is_legacy(&path)? {
            continue;
        }
        match migrate_legacy_segment(&path, erasure_k, erasure_m) {
            Ok(migrated) => report.migrated += migrated,
            Err(e) => {
                report.errors.push(format!(
                    "{}: migration failed: {e}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                ));
            }
        }
        let after = scan_one(&path);
        if let Some(slot) = report
            .verdicts
            .iter_mut()
            .find(|v| v.file == path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
        {
            *slot = after;
        }
    }
    recount(&mut report);
    Ok(report)
}

// ---------------------------------------------------------------------------
// Doctor — offline cross-strand cascade (A ↔ B)
// ---------------------------------------------------------------------------

/// What the offline cross-strand cascade did (or would do, without --fix).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CascadeReport {
    pub strand_a: String,
    pub strand_b: String,
    /// Segments healed on Strand A from the verified Strand B copy.
    pub segments_healed_a_from_b: usize,
    /// Segments healed on Strand B from the verified Strand A copy.
    pub segments_healed_b_from_a: usize,
    /// Closed WALs healed on Strand A from the verified Strand B copy.
    pub wal_healed_a_from_b: usize,
    /// Closed WALs healed on Strand B from the verified Strand A copy.
    pub wal_healed_b_from_a: usize,
    /// Files damaged on BOTH strands — cascade refuses to guess.
    pub damaged_on_both: Vec<String>,
    /// Read-only dry run (fix=false): files that would be healed.
    pub would_heal: Vec<String>,
}

/// Offline A↔B cascade: for every segment / closed WAL that exists on BOTH
/// strands, heal the damaged copy from the verified healthy one — in either
/// direction (B←A or A←B). Files missing on one strand are intentionally
/// left alone (obsolete compaction output vs. failed rotation is decided by
/// the engine at startup, which has the manifest context); files damaged on
/// both strands are reported, never guessed. Read-only unless `fix`.
pub fn doctor_cascade_strands(dir_a: &Path, dir_b: &Path, fix: bool) -> io::Result<CascadeReport> {
    let mut report = CascadeReport {
        strand_a: dir_a.display().to_string(),
        strand_b: dir_b.display().to_string(),
        segments_healed_a_from_b: 0,
        segments_healed_b_from_a: 0,
        wal_healed_a_from_b: 0,
        wal_healed_b_from_a: 0,
        damaged_on_both: Vec::new(),
        would_heal: Vec::new(),
    };

    // ---- segments present on both strands ----
    let a_names = file_names(dir_a, |n| n.ends_with(".sst"))?;
    let b_names = file_names(dir_b, |n| n.ends_with(".sst"))?;
    for name in a_names.intersection(&b_names) {
        let path_a = dir_a.join(name);
        let path_b = dir_b.join(name);
        let healthy_a = scan_one(&path_a).verdict == "HEALTHY";
        let healthy_b = scan_one(&path_b).verdict == "HEALTHY";
        if healthy_a == healthy_b {
            // Both healthy (byte divergence is reconciled by the engine at
            // startup, which knows the manifest epoch) or both damaged
            // (already reported UNREPAIRABLE by the per-side doctor scan).
            continue;
        }
        let (source, dest_dir, dest_strand) = if healthy_a {
            (&path_a, dir_b, "B")
        } else {
            (&path_b, dir_a, "A")
        };
        if !fix {
            report
                .would_heal
                .push(format!("segment {name}: strand {dest_strand}"));
            continue;
        }
        copy_across_strand(source, dest_dir)?;
        if scan_one(&dest_dir.join(name)).verdict != "HEALTHY" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cascade copy of {name} to strand {dest_strand} failed verification"),
            ));
        }
        if dest_strand == "A" {
            report.segments_healed_a_from_b += 1;
        } else {
            report.segments_healed_b_from_a += 1;
        }
    }

    // ---- closed WALs: handle the union, not only files present on both ----
    // A closed WAL that exists only on B is real data if its frames validate;
    // ignoring it made B-only data invisible to A (and to the next startup).
    let a_wals = file_names(dir_a, |n| n.starts_with("wal_") && n.ends_with(".wal"))?;
    let b_wals = file_names(dir_b, |n| n.starts_with("wal_") && n.ends_with(".wal"))?;
    let all_wals: std::collections::BTreeSet<String> = a_wals.union(&b_wals).cloned().collect();
    for name in all_wals {
        let path_a = dir_a.join(&name);
        let path_b = dir_b.join(&name);
        let exists_a = path_a.exists();
        let exists_b = path_b.exists();
        let valid_a = exists_a && wal_frames_valid(&path_a);
        let valid_b = exists_b && wal_frames_valid(&path_b);

        let (source, dest_dir, dest_strand) = match (valid_a, valid_b) {
            (true, false) => (&path_a, dir_b, "B"),
            (false, true) => (&path_b, dir_a, "A"),
            (true, true) => continue,
            (false, false) => {
                if exists_a || exists_b {
                    report.damaged_on_both.push(name.clone());
                }
                continue;
            }
        };
        if !fix {
            report
                .would_heal
                .push(format!("closed WAL {name}: strand {dest_strand}"));
            continue;
        }
        copy_across_strand(source, dest_dir)?;
        if !wal_frames_valid(&dest_dir.join(&name)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("cascade copy of {name} to strand {dest_strand} failed verification"),
            ));
        }
        if dest_strand == "A" {
            report.wal_healed_a_from_b += 1;
        } else {
            report.wal_healed_b_from_a += 1;
        }
    }

    Ok(report)
}

fn file_names(
    dir: &Path,
    accept: impl Fn(&str) -> bool,
) -> io::Result<std::collections::BTreeSet<String>> {
    let mut out = std::collections::BTreeSet::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if accept(&name) {
            out.insert(name);
        }
    }
    Ok(out)
}

/// Copy one data file across strands (segment or closed WAL) with its
/// sidecar (bloom / parity), atomically: temp file + fsync + rename. The
/// destination's old sidecar is always replaced or removed — a stale bloom
/// causes false-negative lookups and a stale parity sidecar would "repair"
/// from wrong data.
fn copy_across_strand(source: &Path, dest_dir: &Path) -> io::Result<()> {
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no filename"))?;
    let dest = dest_dir.join(name);
    let sidecar = if source.extension().is_some_and(|e| e == "sst") {
        source.with_extension("bloom")
    } else {
        source.with_extension("wal.parity")
    };
    let dest_sidecar = dest_dir.join(sidecar.file_name().unwrap_or_default());
    // Remove the derived sidecar BEFORE the data file. If the copy fails
    // half-way, a missing bloom/parity sidecar is safe (permissive lookup /
    // no false repair); a stale one paired with new bytes is not.
    if dest_sidecar.exists() {
        std::fs::remove_file(&dest_sidecar)?;
    }
    let tmp = dest_dir.join(format!("{}.cascade.tmp", name.to_string_lossy()));
    std::fs::copy(source, &tmp)?;
    OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, &dest)?;
    if sidecar.exists() {
        copy_atomic(&sidecar, &dest_sidecar)?;
    }
    sync_directory(dest_dir)
}

fn copy_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    let name = destination.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no filename")
    })?;
    let tmp = destination.with_file_name(format!("{}.cascade.tmp", name.to_string_lossy()));
    let result = (|| {
        std::fs::copy(source, &tmp)?;
        OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, destination)?;
        sync_directory(destination.parent().unwrap_or_else(|| Path::new(".")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn sync_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Frame-probe a WAL copy on a temporary clone (never mutates the original):
/// true when frame recovery succeeds and does not truncate the clone.
fn wal_frames_valid(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if before == 0 {
        return false;
    }
    let tmp = path.with_extension("wal.probe.tmp");
    if std::fs::copy(path, &tmp).is_err() {
        return false;
    }
    let recovered = crate::wal::WalReader::recover(&tmp)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false);
    let after = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&tmp);
    recovered && after == before
}

fn sst_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == "sst") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn is_legacy(path: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len < 16 {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(len - 8))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    Ok(&magic == crate::segment_block::LEGACY_MAGIC)
}

fn scan_one(path: &Path) -> SegmentVerdict {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    // Legacy segments have per-record CRC but no block parity. Validate them
    // with the legacy reader; previously they were always reported as
    // STRUCTURAL_DAMAGE simply because the block reader cannot open them.
    if is_legacy(path).unwrap_or(false) {
        return match crate::segment::SegmentReader::open(path) {
            Ok(r) => match r.validate_all() {
                Ok(()) => SegmentVerdict {
                    file: file_name,
                    format: "DNASST01".into(),
                    records: r.entry_count as u64,
                    blocks_total: 0,
                    damaged_blocks: 0,
                    affected_stripes: 0,
                    unrecoverable_stripes: 0,
                    verdict: "HEALTHY",
                    detail: None,
                },
                Err(e) => SegmentVerdict {
                    file: file_name,
                    format: "DNASST01".into(),
                    records: r.entry_count as u64,
                    blocks_total: 0,
                    damaged_blocks: 0,
                    affected_stripes: 0,
                    unrecoverable_stripes: 0,
                    verdict: "STRUCTURAL_DAMAGE",
                    detail: Some(e.to_string()),
                },
            },
            Err(e) => SegmentVerdict {
                file: file_name,
                format: "DNASST01".into(),
                records: 0,
                blocks_total: 0,
                damaged_blocks: 0,
                affected_stripes: 0,
                unrecoverable_stripes: 0,
                verdict: "STRUCTURAL_DAMAGE",
                detail: Some(e.to_string()),
            },
        };
    }
    match crate::segment_block::BlockSegmentReader::open(path) {
        Ok(r) => match r.scan_lesions() {
            Ok((damaged, affected, unrecoverable_stripes)) => {
                let verdict = if damaged == 0 {
                    "HEALTHY"
                } else if unrecoverable_stripes == 0 {
                    "REPAIRABLE"
                } else {
                    "UNREPAIRABLE"
                };
                SegmentVerdict {
                    file: file_name,
                    format: "DNASST02".into(),
                    records: r.record_count(),
                    blocks_total: r.block_count(),
                    damaged_blocks: damaged,
                    affected_stripes: affected,
                    unrecoverable_stripes,
                    verdict,
                    detail: None,
                }
            }
            Err(e) => SegmentVerdict {
                file: file_name,
                format: "DNASST02".into(),
                records: r.record_count(),
                blocks_total: r.block_count(),
                damaged_blocks: 0,
                affected_stripes: 0,
                unrecoverable_stripes: 0,
                verdict: "STRUCTURAL_DAMAGE",
                detail: Some(e.to_string()),
            },
        },
        Err(e) => {
            let format = if is_legacy(path).unwrap_or(false) {
                "DNASST01"
            } else {
                "UNKNOWN"
            };
            SegmentVerdict {
                file: file_name,
                format: format.into(),
                records: 0,
                blocks_total: 0,
                damaged_blocks: 0,
                affected_stripes: 0,
                unrecoverable_stripes: 0,
                verdict: "STRUCTURAL_DAMAGE",
                detail: Some(e.to_string()),
            }
        }
    }
}

/// Rewrite every damaged block of a DNASST02 segment from parity (offline
/// repair — installs a permissive hook locally).
pub fn fix_segment(path: &Path) -> io::Result<usize> {
    // verify_all with a repair-everything hook.
    struct LocalFix;
    impl RepairHook for LocalFix {
        fn repair_on_read(&self) -> bool {
            true
        }
        fn on_repaired(&self, _: &Path, _: usize, _: &[usize], _: u64) {}
        fn on_unrecoverable(&self, _: &Path, _: usize, _: &[usize]) -> bool {
            false
        }
    }
    let reader = crate::segment_block::BlockSegmentReader::open(path)?;
    reader.set_hook(Arc::new(LocalFix));
    reader.validate_all()?;
    let repaired = reader.repairs_done.load(Ordering::Relaxed) as usize;
    Ok(repaired)
}

/// Convert one DNASST01 segment into DNASST02, atomically replacing the file.
/// Returns 1 on success, 0 if skipped. Records that fail their CRC are
/// reported on stderr and dropped (honest limitation: no parity in V1).
fn migrate_legacy_segment(path: &Path, k: usize, m: usize) -> io::Result<usize> {
    use crate::segment::SegmentReader;
    let reader = SegmentReader::open(path)?;
    let mut entries: Vec<(Vec<u8>, crate::memtable::EntryValue)> = Vec::new();
    let mut dropped = 0usize;
    for key in reader.scan_keys(&[])? {
        match reader.get(&key) {
            Ok(Some(crate::segment::SegmentLookup::Found(v))) => {
                entries.push((key, crate::memtable::EntryValue::Value(v)));
            }
            Ok(Some(crate::segment::SegmentLookup::Tombstone)) => {
                entries.push((key, crate::memtable::EntryValue::Tombstone));
            }
            Ok(None) => {}
            Err(e) => {
                dropped += 1;
                eprintln!(
                    "[MIGRATE] dropping unreadable record {:?} from {:?}: {}",
                    String::from_utf8_lossy(&key),
                    path.file_name().unwrap_or_default(),
                    e
                );
            }
        }
    }
    if entries.is_empty() {
        eprintln!(
            "[MIGRATE] {:?}: no readable records; leaving file untouched",
            path.file_name().unwrap_or_default()
        );
        return Ok(0);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let id: u64 = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad segment id"))?;
    // Write new-format file under a temp name, then atomically replace.
    let temp_id = u64::MAX - id;
    // Seed must match the FINAL (post-rename) name, not the temp name.
    let new_path = crate::segment_block::BlockSegmentWriter::write_segment(
        dir,
        temp_id, // high temp id to avoid collision
        &entries,
        k,
        m,
        crate::segment_block::BlockSegmentReader::seed_for(path),
    )?;
    // The temp bloom sidecar is redundant (legacy sidecar already matches
    // the same keys) — remove it.
    let _ = std::fs::remove_file(dir.join(format!("{:06}.bloom", temp_id)));
    let tmp_final = dir.join(format!("{:06}.migrating", id));
    std::fs::rename(&new_path, &tmp_final)?;
    std::fs::rename(&tmp_final, path)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    eprintln!(
        "[MIGRATE] {:?}: {} records → DNASST02 (k={}, m={}), {} dropped",
        path.file_name().unwrap_or_default(),
        entries.len(),
        k,
        m,
        dropped
    );
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::EntryValue;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_rep_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entries(n: usize) -> Vec<(Vec<u8>, EntryValue)> {
        (0..n)
            .map(|i| {
                (
                    format!("k:{:06}", i).into_bytes(),
                    EntryValue::Value(vec![i as u8; 64]),
                )
            })
            .collect()
    }

    #[test]
    fn journal_roundtrip() {
        let dir = tempdir("journal");
        let j = RepairJournal::open(&dir).unwrap();
        j.append(&RepairRecord {
            ts: 1,
            segment: "000001.sst".into(),
            stripe: 3,
            shards: vec![4, 9],
            region: "data".to_string(),
            action: "repaired".into(),
            bytes: 8192,
        });
        j.append(&RepairRecord {
            ts: 2,
            segment: "000001.sst".into(),
            stripe: 7,
            shards: vec![0, 1, 2],
            region: "index".to_string(),
            action: "unrecoverable".into(),
            bytes: 0,
        });
        let recs = j.read_all();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].action, "repaired");
        assert_eq!(recs[1].shards, vec![0, 1, 2]);
    }

    #[test]
    fn policy_default_is_repair_on_read() {
        assert_eq!(RepairPolicy::default(), RepairPolicy::RepairOnRead);
        assert_eq!(ScrubConfig::default().interval, Duration::from_secs(86400));
        assert_eq!(
            ScrubConfig::default().rate_limit_bytes_per_sec,
            32 * 1024 * 1024
        );
    }

    #[test]
    fn doctor_scan_and_fix_roundtrip() {
        let dir = tempdir("doctor");
        let path = crate::segment_block::BlockSegmentWriter::write_segment(
            &dir,
            1,
            &entries(2000),
            4,
            2,
            crate::segment_block::BlockSegmentReader::seed_for(&dir.join("000001.sst")),
        )
        .unwrap();

        // Damage 1 block (repairable).
        let mut raw = std::fs::read(&path).unwrap();
        let off = crate::segment_block::SUPERBLOCK_SIZE + 5 * BLOCK_SIZE + 9;
        raw[off] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        let report = doctor_scan(&dir).unwrap();
        assert_eq!(report.scanned_segments, 1);
        assert_eq!(report.repairable, 1);
        assert_eq!(report.verdicts[0].damaged_blocks, 1);

        let fixed = doctor_fix(&dir).unwrap();
        assert!(fixed.fixed_blocks >= 1);
        let after = doctor_scan(&dir).unwrap();
        assert_eq!(after.healthy, 1);
        assert_eq!(after.repairable, 0);
    }

    #[test]
    fn doctor_migrates_legacy() {
        let dir = tempdir("migrate");
        // Write a legacy DNASST01 segment with the old writer.
        let path = crate::segment::SegmentWriter::write_segment(&dir, 5, &entries(300)).unwrap();
        assert!(is_legacy(&path).unwrap());

        let report = doctor_migrate(&dir, 4, 2).unwrap();
        assert_eq!(report.migrated, 1);
        assert!(!is_legacy(&path).unwrap());

        // Data survived migration.
        let r = crate::segment_block::BlockSegmentReader::open(&path).unwrap();
        assert_eq!(r.record_count(), 300);
        for (i, (key, val)) in entries(300).iter().enumerate() {
            let _ = i;
            assert_eq!(
                r.get(key).unwrap(),
                Some(crate::segment_block::BlockLookup::Found(
                    val.clone().unwrap_value()
                ))
            );
        }
    }

    // ----- cross-strand cascade (A ↔B) -----

    fn write_segment_pair(a: &Path, b: &Path) -> PathBuf {
        let path = crate::segment_block::BlockSegmentWriter::write_segment(
            a,
            1,
            &entries(1200),
            4,
            2,
            crate::segment_block::BlockSegmentReader::seed_for(&a.join("000001.sst")),
        )
        .unwrap();
        std::fs::copy(&path, b.join("000001.sst")).unwrap();
        path
    }

    fn damage_segment(path: &Path, block: usize, byte: usize) {
        let mut raw = std::fs::read(path).unwrap();
        let off = crate::segment_block::SUPERBLOCK_SIZE + block * BLOCK_SIZE + byte;
        raw[off] ^= 0xFF;
        std::fs::write(path, raw).unwrap();
    }

    fn write_closed_wal_pair(a: &Path, b: &Path) -> PathBuf {
        use crate::wal::{SyncMode, WalOp, WalWriter};
        let wal = a.join("wal_000001.wal");
        let mut w = WalWriter::open(&wal, SyncMode::Strict).unwrap();
        w.append(WalOp::Put, b"cascade:k1", Some(b"v1")).unwrap();
        w.append(WalOp::Put, b"cascade:k2", Some(b"v2")).unwrap();
        w.sync().unwrap();
        drop(w);
        std::fs::copy(&wal, b.join("wal_000001.wal")).unwrap();
        wal
    }

    #[test]
    fn cascade_heals_segment_b_from_a() {
        let a = tempdir("casc_a2b");
        let b = tempdir("casc_b_from_a");
        let path = write_segment_pair(&a, &b);

        // Damage B (beyond a single spot: two blocks in one stripe).
        damage_segment(&b.join("000001.sst"), 4, 3);
        damage_segment(&b.join("000001.sst"), 4, 200);
        assert_ne!(
            scan_one(&b.join("000001.sst")).verdict,
            "HEALTHY",
            "B must start damaged"
        );

        // Dry run: reports, does not touch disk.
        let dry = doctor_cascade_strands(&a, &b, false).unwrap();
        assert_eq!(dry.segments_healed_b_from_a, 0);
        assert!(dry.would_heal.iter().any(|w| w.contains("000001.sst")));
        assert_ne!(scan_one(&b.join("000001.sst")).verdict, "HEALTHY");

        // Fix run: B healed from the verified A copy.
        let report = doctor_cascade_strands(&a, &b, true).unwrap();
        assert_eq!(report.segments_healed_b_from_a, 1);
        assert_eq!(report.segments_healed_a_from_b, 0);
        assert_eq!(scan_one(&b.join("000001.sst")).verdict, "HEALTHY");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(b.join("000001.sst")).unwrap(),
            "B must be byte-identical to A after the cascade"
        );
    }

    #[test]
    fn cascade_heals_segment_a_from_b() {
        let a = tempdir("casc_a_from_b");
        let b = tempdir("casc_b2a");
        let path = write_segment_pair(&a, &b);

        // Damage A this time (parity is not consulted here — the mirror
        // cascade is the second line of defence after doctor_fix).
        damage_segment(&a.join("000001.sst"), 7, 11);
        damage_segment(&a.join("000001.sst"), 7, 333);
        assert_ne!(scan_one(&a.join("000001.sst")).verdict, "HEALTHY");

        let report = doctor_cascade_strands(&a, &b, true).unwrap();
        assert_eq!(report.segments_healed_a_from_b, 1);
        assert_eq!(report.segments_healed_b_from_a, 0);
        assert_eq!(scan_one(&a.join("000001.sst")).verdict, "HEALTHY");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(a.join("000001.sst")).unwrap()
        );
    }

    #[test]
    fn cascade_heals_closed_wal_both_directions() {
        use crate::wal::WalReader;

        // B damaged -> healed from A.
        let a = tempdir("cwal_a2b");
        let b = tempdir("cwal_b_from_a");
        let wal = write_closed_wal_pair(&a, &b);
        let healthy = std::fs::read(&wal).unwrap();
        std::fs::write(b.join("wal_000001.wal"), b"garbage not a wal at all").unwrap();
        assert!(WalReader::recover(b.join("wal_000001.wal"))
            .unwrap()
            .is_empty());

        let report = doctor_cascade_strands(&a, &b, true).unwrap();
        assert_eq!(report.wal_healed_b_from_a, 1);
        assert_eq!(std::fs::read(b.join("wal_000001.wal")).unwrap(), healthy);
        let entries = WalReader::recover(&wal).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, b"cascade:k1");

        // A damaged -> healed from B.
        let a2 = tempdir("cwal_a_from_b");
        let b2 = tempdir("cwal_b2a");
        let wal2 = write_closed_wal_pair(&a2, &b2);
        std::fs::write(&wal2, vec![0xEEu8; 64]).unwrap();

        let report = doctor_cascade_strands(&a2, &b2, true).unwrap();
        assert_eq!(report.wal_healed_a_from_b, 1);
        let entries = WalReader::recover(&wal2).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].key, b"cascade:k2");
    }

    #[test]
    fn cascade_refuses_to_guess_when_both_strands_are_damaged() {
        let a = tempdir("casc_both_a");
        let b = tempdir("casc_both_b");
        let wal = write_closed_wal_pair(&a, &b);
        std::fs::write(&wal, b"broken on A").unwrap();
        std::fs::write(b.join("wal_000001.wal"), b"broken on B too").unwrap();

        let report = doctor_cascade_strands(&a, &b, true).unwrap();
        assert_eq!(report.wal_healed_a_from_b, 0);
        assert_eq!(report.wal_healed_b_from_a, 0);
        assert_eq!(report.damaged_on_both, vec!["wal_000001.wal".to_string()]);
    }
}
