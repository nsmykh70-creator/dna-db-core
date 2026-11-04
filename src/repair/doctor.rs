// SPDX-License-Identifier: AGPL-3.0-only
//! Offline doctor: `doctor_scan`, `doctor_fix`, DNASST01→02 migration and
//! the strand-A/strand-B cascade repair, plus the segment scan/fix/migrate
//! primitives and the filesystem helpers they share. Split from the former
//! repair.rs monolith — no behaviour change.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::scrub::SegmentVerdict;
use crate::segment_block::RepairHook;

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

pub(super) fn is_legacy(path: &Path) -> io::Result<bool> {
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

pub(super) fn scan_one(path: &Path) -> SegmentVerdict {
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
