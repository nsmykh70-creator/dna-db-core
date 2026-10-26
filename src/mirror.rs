// SPDX-License-Identifier: AGPL-3.0-only
//! Dual-Strand Disk Mirroring (RAID-1-like failover)
//!
//! Strand A (primary dir) + Strand B (secondary dir). Every WAL frame and
//! active segment manifest is durably written to both strands. If B fails,
//! writes are blocked until the mirror is repaired or resynced.
//!
//! Hot-swap resync: copies all primary files (WAL + segments + bloom
//! sidecars and active-set manifest onto the replacement disk and reopens B's WAL.

use crate::segment::SegmentFile;
use crate::wal::{SyncMode, WalEntry, WalOp, WalParity, WalReader, WalWriter};
use parking_lot::{Mutex, RwLock};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const MANIFEST_FILE: &str = "segments.manifest";
const MANIFEST_MAGIC: &[u8; 8] = b"DNAMAN01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandStatus {
    Healthy,
    Degraded,
    Offline,
    Resyncing,
}

pub struct DualStrandMirror {
    pub primary_dir: PathBuf,
    pub secondary_dir: RwLock<PathBuf>,
    pub is_degraded: AtomicBool,
    pub secondary_status: RwLock<StrandStatus>,
    secondary_wal: RwLock<Option<WalWriter>>,
    backup_wal_damaged: AtomicBool,
    manifest_epoch: AtomicU64,
    manifest_lock: Mutex<()>,
}

impl DualStrandMirror {
    pub fn new(primary: impl AsRef<Path>, secondary: impl AsRef<Path>) -> std::io::Result<Self> {
        let p = primary.as_ref().to_path_buf();
        let s = secondary.as_ref().to_path_buf();
        std::fs::create_dir_all(&p)?;
        std::fs::create_dir_all(&s)?;

        if p.canonicalize()? == s.canonicalize()? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "primary and secondary strand directories must be different",
            ));
        }

        let has_backup_data = std::fs::read_dir(&s)?.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == "active.wal" || name.ends_with(".sst") || name.ends_with(".bloom")
        });
        if !has_backup_data {
            // First activation: seed Strand B with the existing Strand A state,
            // rather than starting an empty mirror beside a populated database.
            for entry in std::fs::read_dir(&p)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == "active.wal" || name.ends_with(".sst") || name.ends_with(".bloom") {
                    copy_atomic(&path, &s.join(entry.file_name()))?;
                }
            }
        }

        let backup_wal_path = s.join("active.wal");
        let backup_wal_existed = backup_wal_path.exists();
        let backup_len_before = std::fs::metadata(&backup_wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        WalReader::recover(&backup_wal_path)?;
        let backup_len_after = std::fs::metadata(&backup_wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let wal = WalWriter::open(backup_wal_path, SyncMode::Strict)?;

        Ok(Self {
            primary_dir: p,
            secondary_dir: RwLock::new(s),
            is_degraded: AtomicBool::new(false),
            secondary_status: RwLock::new(StrandStatus::Healthy),
            secondary_wal: RwLock::new(Some(wal)),
            backup_wal_damaged: AtomicBool::new(
                (has_backup_data && !backup_wal_existed) || backup_len_after < backup_len_before,
            ),
            manifest_epoch: AtomicU64::new(0),
            manifest_lock: Mutex::new(()),
        })
    }

    /// Recover a damaged primary WAL from B, or refresh a damaged B WAL from a
    /// validated A WAL. If both tails are damaged, refuse a lossy startup.
    pub fn repair_wal(
        &self,
        primary_wal: &Path,
        primary_damaged: bool,
    ) -> io::Result<Vec<WalEntry>> {
        let backup_damaged = self.backup_wal_damaged.load(Ordering::Acquire);
        if primary_damaged && backup_damaged {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "both Strand A and Strand B WALs have damaged tails; refusing lossy recovery",
            ));
        }

        if primary_damaged {
            let backup = self.secondary_dir.read().join("active.wal");
            // The backup WAL was validated (and repaired only at an incomplete
            // tail) when the mirror opened.
            copy_atomic(&backup, primary_wal)?;
            let entries = WalReader::recover(primary_wal)?;
            eprintln!("[MIRROR] Restored damaged Strand A WAL from Strand B");
            return Ok(entries);
        }

        let entries = WalReader::recover(primary_wal)?;
        if backup_damaged {
            let backup = self.secondary_dir.read().join("active.wal");
            let mut guard = self.secondary_wal.write();
            drop(guard.take());
            copy_atomic(primary_wal, &backup)?;
            *guard = Some(WalWriter::open(&backup, SyncMode::Strict)?);
            self.backup_wal_damaged.store(false, Ordering::Release);
            eprintln!("[MIRROR] Rebuilt damaged Strand B WAL from validated Strand A");
        }
        Ok(entries)
    }

    pub fn backup_wal_size(&self) -> u64 {
        std::fs::metadata(self.secondary_dir.read().join("active.wal"))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }

    pub fn backup_wal_is_damaged(&self) -> bool {
        self.backup_wal_damaged.load(Ordering::Acquire)
    }

    pub fn mirror_write(
        &self,
        op: WalOp,
        key: &[u8],
        val: Option<&[u8]>,
    ) -> Result<(), &'static str> {
        self.mirror_batch(&[(&op, key, val)])
            .map_err(|_| "Strand Beta write failed; writes are blocked until resync")
    }

    /// Replicate one ordered WAL frame to Strand B.
    pub fn mirror_batch(&self, ops: &[(&WalOp, &[u8], Option<&[u8]>)]) -> io::Result<()> {
        self.ensure_writable()?;
        let mut guard = self.secondary_wal.write();
        let Some(wal) = guard.as_mut() else {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "Strand B WAL is unavailable");
            self.mark_degraded(&error);
            return Err(error);
        };
        if let Err(error) = wal.append_batch(ops) {
            self.mark_degraded(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Once B has failed, stop accepting writes until a verified resync.
    pub fn ensure_writable(&self) -> io::Result<()> {
        if self.is_degraded.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Strand B is degraded; writes are blocked until resync",
            ));
        }
        Ok(())
    }

    fn mark_degraded(&self, error: &io::Error) {
        eprintln!("[MIRROR] Strand B write failed: {error}. Writes are blocked until resync.");
        self.is_degraded.store(true, Ordering::Release);
        *self.secondary_status.write() = StrandStatus::Offline;
    }

    /// Reconcile the active segment set from the newest valid manifest, then
    /// repair missing/corrupt files from the other strand. The manifest keeps
    /// obsolete compaction files from being mistaken for active data.
    pub fn repair_segment_set(&self) -> io::Result<Vec<u64>> {
        let _wal_guard = self.secondary_wal.read();
        let _manifest_guard = self.manifest_lock.lock();
        let secondary_dir = self.secondary_dir.read().clone();
        let (manifest_epoch, active_ids, has_manifest) = select_manifest(
            read_manifest(&self.primary_dir.join(MANIFEST_FILE)),
            read_manifest(&secondary_dir.join(MANIFEST_FILE)),
            &self.primary_dir,
        )?;

        for segment_id in &active_ids {
            let file_name = format!("{segment_id:06}.sst");
            let primary = self.primary_dir.join(&file_name);
            let backup = secondary_dir.join(&file_name);
            let primary_valid = validate_segment(&primary).is_ok();
            let backup_valid = validate_segment(&backup).is_ok();

            match (primary_valid, backup_valid) {
                (true, true) if files_equal(&primary, &backup)? => {
                    sync_bloom(&primary, &backup)?;
                }
                (true, true) if formats_differ(&primary, &backup) => {
                    // Never let an older DNASST01 copy overwrite a migrated
                    // DNASST02 copy: `doctor --migrate` must survive restart.
                    let primary_format = SegmentFile::open(&primary)?.format_tag();
                    let backup_format = SegmentFile::open(&backup)?.format_tag();
                    if primary_format == "DNASST02" {
                        copy_segment_pair(&primary, &secondary_dir)?;
                        eprintln!(
                            "[MIRROR] Strand B copy of {file_name} is {backup_format}; migrated from A"
                        );
                    } else {
                        copy_segment_pair(&backup, &self.primary_dir)?;
                        validate_segment(&primary)?;
                        eprintln!(
                            "[MIRROR] Strand A copy of {file_name} is {primary_format}; migrated from B"
                        );
                    }
                }
                (true, true) if !has_manifest => {
                    // Legacy databases have no manifest; A is authoritative
                    // during this one-time migration.
                    copy_segment_pair(&primary, &secondary_dir)?;
                }
                (true, true) => {
                    // Both validate but the bytes differ: the v3.0 format is
                    // damage-tolerant (redundant superblock/footer), so one
                    // side can be silently degraded while still readable.
                    // The verified copy on B is authoritative — restore A.
                    copy_segment_pair(&backup, &self.primary_dir)?;
                    validate_segment(&primary)?;
                    eprintln!(
                        "[MIRROR] Strand A copy of {file_name} diverged from Strand B; restored from B"
                    );
                }
                (true, false) => copy_segment_pair(&primary, &secondary_dir)?,
                (false, true) => {
                    copy_segment_pair(&backup, &self.primary_dir)?;
                    validate_segment(&primary)?;
                    eprintln!("[MIRROR] Repaired Strand A segment {file_name} from Strand B");
                }
                (false, false) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("active segment {file_name} is damaged or missing on both strands"),
                    ));
                }
            }
        }

        let next_epoch = manifest_epoch.saturating_add(1);
        let bytes = encode_manifest(next_epoch, &active_ids)?;
        atomic_write(&secondary_dir.join(MANIFEST_FILE), &bytes)?;
        atomic_write(&self.primary_dir.join(MANIFEST_FILE), &bytes)?;
        self.manifest_epoch.store(next_epoch, Ordering::Release);
        Ok(active_ids)
    }

    /// Compatibility wrapper for callers that only need the repair side effect.
    pub fn repair_segments(&self) -> io::Result<()> {
        self.repair_segment_set().map(|_| ())
    }

    /// Commit the active segment set on B before A. An interrupted update
    /// leaves a newer valid manifest that startup can use to reconcile.
    pub fn update_segment_manifest(&self, segment_ids: &[u64]) -> io::Result<()> {
        self.ensure_writable()?;
        let _wal_guard = self.secondary_wal.read();
        let _manifest_guard = self.manifest_lock.lock();
        let mut ids = segment_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != segment_ids.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "active segment list contains duplicate ids",
            ));
        }

        let epoch = self
            .manifest_epoch
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let bytes = encode_manifest(epoch, &ids)?;
        let secondary_dir = self.secondary_dir.read().clone();
        if let Err(error) = atomic_write(&secondary_dir.join(MANIFEST_FILE), &bytes)
            .and_then(|_| atomic_write(&self.primary_dir.join(MANIFEST_FILE), &bytes))
        {
            self.mark_degraded(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Copy and verify a newly flushed/compacted segment onto Strand B.
    pub fn mirror_segment(&self, primary_segment: &Path) -> io::Result<()> {
        let _wal_guard = self.secondary_wal.read();
        let secondary_dir = self.secondary_dir.read().clone();
        validate_segment(primary_segment)?;
        let result =
            copy_segment_pair(primary_segment, &secondary_dir).and_then(|_| {
                validate_segment(&secondary_dir.join(primary_segment.file_name().ok_or_else(
                    || io::Error::new(io::ErrorKind::InvalidInput, "segment path has no filename"),
                )?))
            });
        if let Err(error) = result {
            self.mark_degraded(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Restore a damaged segment in place from the verified counterpart.
    pub fn restore_segment(&self, primary_segment: &Path) -> io::Result<()> {
        let _wal_guard = self.secondary_wal.read();
        let secondary_dir = self.secondary_dir.read().clone();
        let name = primary_segment.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "segment path has no filename")
        })?;
        let backup = secondary_dir.join(name);
        validate_segment(&backup)?;
        copy_segment_pair(&backup, &self.primary_dir)?;
        validate_segment(primary_segment)
    }

    /// Rotate Strand B's active WAL to `wal_<seq>.wal`, mirroring the
    /// rotation the engine performed on Strand A (same seq number), and
    /// immediately RS-protect the frozen B copy too.
    pub fn rotate_wal(&self, seq: u64, k: usize, m: usize) -> io::Result<()> {
        let secondary_dir = self.secondary_dir.read().clone();
        let backup = secondary_dir.join("active.wal");
        let closed = secondary_dir.join(format!("wal_{seq:06}.wal"));
        let mut guard = self.secondary_wal.write();
        if !backup.exists() {
            return Ok(()); // nothing to rotate (fresh mirror)
        }
        if let Some(wal) = guard.as_mut() {
            if let Err(error) = wal.sync() {
                self.mark_degraded(&error);
                return Err(error);
            }
        }
        drop(guard.take());
        if closed.exists() {
            std::fs::remove_file(&closed)?;
        }
        std::fs::rename(&backup, &closed)?;
        #[cfg(not(windows))]
        {
            let dir_file = std::fs::File::open(&secondary_dir)?;
            let _ = dir_file.sync_all();
        }
        #[cfg(windows)]
        let _ = &secondary_dir;
        // Strand B's closed WAL needs the same erasure protection as A's; a
        // missing B parity sidecar made B-only recovery fall back to copying A.
        if std::fs::metadata(&closed)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            if let Err(e) = WalParity::protect(&closed, k, m) {
                eprintln!(
                    "[MIRROR] Strand B WAL parity protection failed for {:?}: {e}",
                    closed.file_name().unwrap_or_default()
                );
            }
        }
        match WalWriter::open(&backup, SyncMode::Strict) {
            Ok(w) => {
                *guard = Some(w);
                Ok(())
            }
            Err(error) => {
                self.mark_degraded(&error);
                Err(error)
            }
        }
    }

    /// Copy a closed WAL segment from Strand B over Strand A's copy
    /// (cascade repair when parity is insufficient). Validates B's copy by
    /// frame-recovering a temporary clone first — never truncates B itself.
    pub fn restore_closed_wal(&self, name: &str, primary_dir: &Path) -> io::Result<u64> {
        let secondary_dir = self.secondary_dir.read().clone();
        let backup = secondary_dir.join(name);
        if !backup.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no copy of {name} on Strand B"),
            ));
        }
        // Validate B's copy without touching it: recover a temp clone and
        // remember how much valid data it holds.
        let tmp = secondary_dir.join(format!("{name}.probe.tmp"));
        std::fs::copy(&backup, &tmp)?;
        let entries = WalReader::recover(&tmp)?;
        let valid_len = std::fs::metadata(&tmp)?.len();
        let _ = std::fs::remove_file(&tmp);
        if entries.is_empty() && valid_len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Strand B copy of {name} holds no valid frames"),
            ));
        }
        copy_atomic(&backup, &primary_dir.join(name))?;
        Ok(valid_len)
    }

    /// Startup A→B half of the closed-WAL cascade: probe every `wal_*.wal`
    /// copy on Strand B (on a temp clone, never mutating the original) and
    /// rebuild it from the verified Strand A file when missing or damaged.
    /// The B→A half is `restore_closed_wal`, used when Strand A's copy lost
    /// the parity race. Returns the number of rebuilt copies.
    pub fn heal_backup_closed_wals(&self, primary_dir: &Path) -> io::Result<usize> {
        let secondary_dir = self.secondary_dir.read().clone();
        let mut rebuilt = 0usize;
        for entry in std::fs::read_dir(primary_dir)?.flatten() {
            let path = entry.path();
            let name = match path.file_name().map(|n| n.to_string_lossy().to_string()) {
                Some(n) if n.starts_with("wal_") && n.ends_with(".wal") => n,
                _ => continue,
            };
            let backup = secondary_dir.join(&name);
            let backup_valid_len = wal_copy_valid_len(&backup);
            let primary_valid_len = wal_copy_valid_len(&path);
            if let (Some(b_len), Some(a_len)) = (backup_valid_len, primary_valid_len) {
                if b_len >= a_len {
                    // B is valid and at least as complete as A; nothing to do.
                    continue;
                }
            }
            // Validate A's copy before propagating it (probe clone).
            if primary_valid_len.is_none() {
                // Damaged on both strands: the startup loop already logged
                // A's parity/cascade outcome; nothing safe to copy.
                continue;
            }
            copy_atomic(&path, &backup)?;
            // Keep the parity sidecar in lockstep when present.
            let parity = path.with_extension("wal.parity");
            let backup_parity = backup.with_extension("wal.parity");
            if parity.exists() {
                copy_atomic(&parity, &backup_parity)?;
            } else if backup_parity.exists() {
                std::fs::remove_file(&backup_parity)?;
                sync_directory(&secondary_dir)?;
            }
            rebuilt += 1;
            eprintln!("[MIRROR] Rebuilt damaged/stale Strand B closed WAL {name} from Strand A");
        }
        Ok(rebuilt)
    }

    /// Startup B->A half for *missing* closed WALs: a validated closed WAL
    /// that exists only on Strand B is real data (for example, a crash between
    /// B rotation and A rotation). Copy it to Strand A before recovery scans
    /// the primary directory. Damaged A copies are handled by
    /// `restore_closed_wal_better` in the engine loop.
    pub fn heal_primary_closed_wals(&self, primary_dir: &Path) -> io::Result<usize> {
        let secondary_dir = self.secondary_dir.read().clone();
        let mut restored = 0usize;
        let entries = match std::fs::read_dir(&secondary_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(0),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().map(|n| n.to_string_lossy().to_string()) {
                Some(n) if n.starts_with("wal_") && n.ends_with(".wal") => n,
                _ => continue,
            };
            let primary = primary_dir.join(&name);
            if primary.exists() {
                continue;
            }
            if wal_copy_valid_len(&path).is_none() {
                continue;
            }
            copy_atomic(&path, &primary)?;
            let parity = path.with_extension("wal.parity");
            if parity.exists() {
                copy_atomic(&parity, &primary.with_extension("wal.parity"))?;
            }
            restored += 1;
            eprintln!("[MIRROR] Restored B-only closed WAL {name} to Strand A");
        }
        Ok(restored)
    }

    /// B→A cascade with a downgrade guard: restore `name` from Strand B only
    /// when B's copy frame-recovers to MORE valid bytes than Strand A already
    /// holds (`primary_valid_len`) — a worse B copy must never discard frames
    /// A recovered. B's copy is validated on a temp clone, never mutated.
    /// Returns the restored valid length when a copy actually happened.
    pub fn restore_closed_wal_better(
        &self,
        name: &str,
        primary_dir: &Path,
        primary_valid_len: u64,
    ) -> io::Result<Option<u64>> {
        let secondary_dir = self.secondary_dir.read().clone();
        let backup = secondary_dir.join(name);
        if !backup.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no copy of {name} on Strand B"),
            ));
        }
        let tmp = secondary_dir.join(format!("{name}.probe.tmp"));
        std::fs::copy(&backup, &tmp)?;
        let entries = WalReader::recover(&tmp)?;
        let valid_len = std::fs::metadata(&tmp)?.len();
        let _ = std::fs::remove_file(&tmp);
        if entries.is_empty() || valid_len <= primary_valid_len {
            return Ok(None);
        }
        copy_atomic(&backup, &primary_dir.join(name))?;
        Ok(Some(valid_len))
    }

    /// Best-effort copy of a closed WAL segment to Strand B after an A-side
    /// repair (keeps the strands byte-identical).
    pub fn sync_closed_wal(&self, name: &str, primary_dir: &Path) -> io::Result<()> {
        let secondary_dir = self.secondary_dir.read().clone();
        let source = primary_dir.join(name);
        let destination = secondary_dir.join(name);
        copy_atomic(&source, &destination)?;
        let source_parity = source.with_extension("wal.parity");
        let destination_parity = destination.with_extension("wal.parity");
        if source_parity.exists() {
            copy_atomic(&source_parity, &destination_parity)?;
        } else if destination_parity.exists() {
            std::fs::remove_file(&destination_parity)?;
            sync_directory(&secondary_dir)?;
        }
        Ok(())
    }

    /// Delete closed WAL segments (and parity sidecars) on Strand B after a
    /// checkpoint absorbed them into segments.
    pub fn remove_closed_wals(&self, primary_dir: &Path) {
        let _ = primary_dir;
        let secondary_dir = self.secondary_dir.read().clone();
        if let Ok(entries) = std::fs::read_dir(&secondary_dir) {
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

    /// Remove a superseded segment after its replacement is durable on B.
    pub fn remove_segment(&self, segment_id: u64) -> io::Result<()> {
        let _wal_guard = self.secondary_wal.read();
        let secondary_dir = self.secondary_dir.read().clone();
        let name = format!("{:06}.sst", segment_id);
        let segment = secondary_dir.join(&name);
        let bloom = segment.with_extension("bloom");
        if segment.exists() {
            std::fs::remove_file(segment)?;
        }
        if bloom.exists() {
            std::fs::remove_file(bloom)?;
        }
        sync_directory(&secondary_dir)
    }

    /// WAL truncation is safe only after every current segment exists and has
    /// passed CRC validation on both strands.
    pub fn reset_wal(&self) -> io::Result<()> {
        self.ensure_writable()?;
        let mut guard = self.secondary_wal.write();
        let Some(wal) = guard.as_mut() else {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "Strand B WAL is unavailable");
            self.mark_degraded(&error);
            return Err(error);
        };
        if let Err(error) = wal.reset() {
            self.mark_degraded(&error);
            return Err(error);
        }
        Ok(())
    }

    /// Trigger a hot-swap resync onto `new_disk_path`: copies every primary
    /// file (WAL, segments, bloom sidecars), then reopens the Beta WAL there.
    pub fn trigger_hot_swap_resync(&self, new_disk_path: &Path) -> std::io::Result<()> {
        *self.secondary_status.write() = StrandStatus::Resyncing;
        let result = (|| {
            std::fs::create_dir_all(new_disk_path)?;
            if self.primary_dir.canonicalize()? == new_disk_path.canonicalize()? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "replacement strand must not be the primary directory",
                ));
            }

            // Hold the mirror WAL lock for the entire copy. A writer may have
            // already appended to A, but it cannot acknowledge before its B
            // append completes; it will append to the replacement after swap.
            let mut wal_guard = self.secondary_wal.write();
            let primary_wal = self.primary_dir.join("active.wal");
            let wal_len_before = std::fs::metadata(&primary_wal)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            WalReader::recover(&primary_wal)?;
            let wal_len_after = std::fs::metadata(&primary_wal)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if wal_len_after < wal_len_before {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "primary WAL is damaged; repair it before mirror resync",
                ));
            }

            let mut primary_files = std::collections::HashSet::new();
            for entry in std::fs::read_dir(&self.primary_dir)? {
                let entry = entry?;
                let source = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == "active.wal" {
                    copy_atomic(&source, &new_disk_path.join(&name))?;
                    primary_files.insert(name_str.to_string());
                } else if name_str == MANIFEST_FILE {
                    copy_atomic(&source, &new_disk_path.join(&name))?;
                    primary_files.insert(name_str.to_string());
                } else if name_str.starts_with("wal_")
                    && (name_str.ends_with(".wal") || name_str.ends_with(".wal.parity"))
                {
                    // Closed WALs are immutable and may still be needed for
                    // recovery of a crash that happened before checkpoint.
                    copy_atomic(&source, &new_disk_path.join(&name))?;
                    primary_files.insert(name_str.to_string());
                } else if source
                    .extension()
                    .is_some_and(|extension| extension == "sst")
                {
                    validate_segment(&source)?;
                    copy_segment_pair(&source, new_disk_path)?;
                    primary_files.insert(name_str.to_string());
                    primary_files.insert(
                        source
                            .with_extension("bloom")
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .to_string(),
                    );
                }
            }

            // Remove only obsolete database segment files from the replacement.
            for entry in std::fs::read_dir(new_disk_path)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let is_db_file = name_str.ends_with(".sst")
                    || name_str.ends_with(".bloom")
                    || (name_str.starts_with("wal_")
                        && (name_str.ends_with(".wal") || name_str.ends_with(".wal.parity")))
                    || name_str.ends_with(".tmp");
                if is_db_file && !primary_files.contains(name_str.as_ref()) {
                    std::fs::remove_file(path)?;
                }
            }

            let new_wal_path = new_disk_path.join("active.wal");
            WalReader::recover(&new_wal_path)?;
            let new_wal = WalWriter::open(&new_wal_path, SyncMode::Strict)?;
            sync_directory(new_disk_path)?;
            *wal_guard = Some(new_wal);
            *self.secondary_dir.write() = new_disk_path.to_path_buf();
            self.is_degraded.store(false, Ordering::SeqCst);
            self.backup_wal_damaged.store(false, Ordering::Release);
            Ok(())
        })();

        match result {
            Ok(()) => {
                *self.secondary_status.write() = StrandStatus::Healthy;
                Ok(())
            }
            Err(error) => {
                self.is_degraded.store(true, Ordering::Release);
                *self.secondary_status.write() = StrandStatus::Degraded;
                Err(error)
            }
        }
    }

    pub fn status(&self) -> StrandStatus {
        *self.secondary_status.read()
    }

    pub fn is_degraded(&self) -> bool {
        self.is_degraded.load(Ordering::Relaxed)
    }

    pub fn sync(&self) -> std::io::Result<()> {
        let mut guard = self.secondary_wal.write();
        if let Some(ref mut wal) = *guard {
            if let Err(error) = wal.sync() {
                self.mark_degraded(&error);
                return Err(error);
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Strand B WAL is unavailable",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct SegmentManifest {
    epoch: u64,
    segment_ids: Vec<u64>,
}

fn read_manifest(path: &Path) -> io::Result<Option<SegmentManifest>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.len() > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "segment manifest exceeds the 64 MiB safety limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() < 24 || &bytes[..8] != MANIFEST_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid segment manifest header",
        ));
    }
    let checksum_offset = bytes.len() - 4;
    let stored_crc = u32::from_le_bytes(bytes[checksum_offset..].try_into().unwrap());
    if crc32fast::hash(&bytes[..checksum_offset]) != stored_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "segment manifest checksum mismatch",
        ));
    }

    let epoch = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let ids_len = count
        .checked_mul(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "manifest count overflow"))?;
    if 20usize.checked_add(ids_len) != Some(checksum_offset) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "segment manifest length does not match its entry count",
        ));
    }
    let mut segment_ids = Vec::with_capacity(count);
    for id_bytes in bytes[20..checksum_offset].chunks_exact(8) {
        segment_ids.push(u64::from_le_bytes(id_bytes.try_into().unwrap()));
    }
    if segment_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "segment manifest ids must be strictly increasing",
        ));
    }
    Ok(Some(SegmentManifest { epoch, segment_ids }))
}

fn select_manifest(
    primary: io::Result<Option<SegmentManifest>>,
    backup: io::Result<Option<SegmentManifest>>,
    primary_dir: &Path,
) -> io::Result<(u64, Vec<u64>, bool)> {
    let (manifest, has_manifest) = match (primary, backup) {
        (Ok(Some(primary)), Ok(Some(backup))) => {
            if primary.epoch == backup.epoch && primary.segment_ids != backup.segment_ids {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Strand A and B segment manifests conflict at the same epoch",
                ));
            }
            if backup.epoch > primary.epoch {
                (Some(backup), true)
            } else {
                (Some(primary), true)
            }
        }
        (Ok(Some(manifest)), _) | (_, Ok(Some(manifest))) => (Some(manifest), true),
        (Ok(None), Ok(None)) => (None, false),
        // One manifest is corrupt but the other is absent: rebuild the active
        // set from the primary segment directory instead of making the whole
        // database unopenable. (A genuine both-strand conflict still fails.)
        (Err(error), Ok(None)) => {
            eprintln!("[MIRROR] Strand A manifest unreadable ({error}); rebuilding active set from segments");
            (None, false)
        }
        (Ok(None), Err(error)) => {
            eprintln!("[MIRROR] Strand B manifest unreadable ({error}); rebuilding active set from Strand A segments");
            (None, false)
        }
        (Err(a), Err(b)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("both segment manifests are unreadable: A: {a}; B: {b}"),
            ));
        }
    };

    if let Some(manifest) = manifest {
        return Ok((manifest.epoch, manifest.segment_ids, has_manifest));
    }

    // One-time migration for databases created before active-set manifests.
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(primary_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "sst") {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid SST segment filename: {}", path.display()),
                    )
                })?;
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Ok((0, ids, false))
}

fn encode_manifest(epoch: u64, segment_ids: &[u64]) -> io::Result<Vec<u8>> {
    let count = u32::try_from(segment_ids.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many segments for manifest format",
        )
    })?;
    if segment_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest segment ids must be strictly increasing",
        ));
    }
    let mut bytes = Vec::with_capacity(24 + segment_ids.len() * 8);
    bytes.extend_from_slice(MANIFEST_MAGIC);
    bytes.extend_from_slice(&epoch.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    for id in segment_ids {
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "manifest path has no filename")
    })?;
    let temp = path.with_file_name(format!("{}.tmp", file_name.to_string_lossy()));
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn validate_segment(path: &Path) -> io::Result<()> {
    // Reconciliation must heal repairable parity damage on the strand being
    // validated before deciding that it is unusable. Otherwise one missing
    // strand plus one damaged block would abort startup even though the block
    // is fully recoverable from that strand's own parity.
    SegmentFile::open(path)?.validate_repairable()?;
    Ok(())
}

/// True when both files open successfully and use different SSTable formats.
/// A failure to open is ignored here; normal validation reports it and the
/// `(false, true)` / `(true, false)` arms handle it.
fn formats_differ(a: &Path, b: &Path) -> bool {
    match (SegmentFile::open(a), SegmentFile::open(b)) {
        (Ok(left), Ok(right)) => left.format_tag() != right.format_tag(),
        _ => false,
    }
}

fn copy_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    let file_name = destination.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination path has no filename",
        )
    })?;
    let temp = destination.with_file_name(format!("{}.tmp", file_name.to_string_lossy()));
    let result = (|| {
        std::fs::copy(source, &temp)?;
        // FlushFileBuffers on Windows requires write access, so open the temp
        // file for writing before syncing (read-only sync is a no-op on Unix
        // but fails with AccessDenied on Windows).
        std::fs::OpenOptions::new()
            .write(true)
            .open(&temp)?
            .sync_all()?;
        std::fs::rename(&temp, destination)?;
        sync_directory(destination.parent().unwrap_or_else(|| Path::new(".")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn copy_segment_pair(source_segment: &Path, destination_dir: &Path) -> io::Result<()> {
    validate_segment(source_segment)?;
    let file_name = source_segment.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "segment path has no filename")
    })?;
    let destination_segment = destination_dir.join(file_name);
    let source_bloom = source_segment.with_extension("bloom");
    let destination_bloom = destination_segment.with_extension("bloom");

    // Drop the derived sidecar before replacing data. On a crash/failure the
    // segment then opens with a permissive filter; a stale Bloom paired with
    // new segment bytes could hide keys (false negatives).
    if destination_bloom.exists() {
        std::fs::remove_file(&destination_bloom)?;
        sync_directory(destination_dir)?;
    }
    copy_atomic(source_segment, &destination_segment)?;
    if source_bloom.exists() {
        copy_atomic(&source_bloom, &destination_bloom)?;
    }

    validate_segment(&destination_segment)
}

fn sync_bloom(source_segment: &Path, destination_segment: &Path) -> io::Result<()> {
    let source = source_segment.with_extension("bloom");
    let destination = destination_segment.with_extension("bloom");
    if source.exists() && !files_equal(&source, &destination)? {
        copy_atomic(&source, &destination)?;
    } else if !source.exists() && destination.exists() {
        std::fs::remove_file(destination)?;
        sync_directory(
            destination_segment
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        )?;
    }
    Ok(())
}

/// Frame-probe a WAL copy on a temporary clone (never mutates the original):
/// true when frame recovery succeeds and does not truncate the clone — i.e.
/// the copy holds only intact frames. A missing or empty file is not valid.
fn wal_copy_valid_len(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let tmp = path
        .parent()?
        .join(format!("{}.probe.tmp", path.file_name()?.to_string_lossy()));
    if std::fs::copy(path, &tmp).is_err() {
        return None;
    }
    let before = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let recovered = WalReader::recover(&tmp)
        .map(|entries| entries.len())
        .unwrap_or(0);
    let after = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&tmp);
    if recovered > 0 && after == before {
        Some(after)
    } else {
        None
    }
}

#[allow(dead_code)]
fn wal_copy_holds_valid_frames(path: &Path) -> bool {
    wal_copy_valid_len(path).is_some()
}

fn files_equal(left: &Path, right: &Path) -> io::Result<bool> {
    let left_meta = match std::fs::metadata(left) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let right_meta = match std::fs::metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_file = std::fs::File::open(left)?;
    let mut right_file = std::fs::File::open(right)?;
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    loop {
        let left_len = left_file.read(&mut left_buf)?;
        let right_len = right_file.read(&mut right_buf)?;
        if left_len != right_len || left_buf[..left_len] != right_buf[..right_len] {
            return Ok(false);
        }
        if left_len == 0 {
            return Ok(true);
        }
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    // Directory fsync forces rename/copy durability on POSIX. Windows cannot
    // open a directory via File::open (AccessDenied) and NTFS metadata is
    // journaled, so the explicit fsync is skipped there.
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentReader;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_mirror_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn mirror_writes_and_resync() {
        let primary = tempdir("primary");
        let secondary = tempdir("secondary");
        let mirror = DualStrandMirror::new(&primary, &secondary).unwrap();

        mirror.mirror_write(WalOp::Put, b"k1", Some(b"v1")).unwrap();
        mirror.mirror_write(WalOp::Put, b"k2", Some(b"v2")).unwrap();
        mirror.sync().unwrap();
        assert_eq!(mirror.status(), StrandStatus::Healthy);

        // Seed the PRIMARY with real files (as an engine would have)
        {
            let mut pw =
                crate::wal::WalWriter::open(primary.join("active.wal"), SyncMode::Strict).unwrap();
            pw.append(WalOp::Put, b"alpha1", Some(b"value1")).unwrap();
            pw.append(WalOp::Put, b"alpha2", Some(b"value2")).unwrap();
            pw.append(WalOp::Put, b"alpha3", Some(b"value3")).unwrap();
            let segment_entries = vec![(
                b"alpha1".to_vec(),
                crate::memtable::EntryValue::Value(b"value1".to_vec()),
            )];
            crate::segment::SegmentWriter::write_segment(&primary, 1, &segment_entries).unwrap();
        }

        // Resync onto a fresh disk: copies WAL + segments from the primary
        let new_disk = tempdir("replacement");
        mirror.trigger_hot_swap_resync(&new_disk).unwrap();
        assert_eq!(mirror.status(), StrandStatus::Healthy);
        assert!(new_disk.join("active.wal").exists());
        assert!(new_disk.join("000001.sst").exists());
        assert_eq!(
            mirror.secondary_dir.read().as_path(),
            new_disk.as_path(),
            "hot-swap must switch the active Strand B directory"
        );

        // Recovered entries on the new disk (from the copied primary WAL)
        let entries = crate::wal::WalReader::recover(new_disk.join("active.wal")).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, b"alpha1");
        let segment = SegmentReader::open(new_disk.join("000001.sst")).unwrap();
        segment.validate_all().unwrap();
    }

    #[test]
    fn corrupt_primary_segment_is_repaired_from_verified_backup() {
        let primary = tempdir("repair_primary");
        let secondary = tempdir("repair_secondary");
        let entries = vec![(
            b"chr1:42".to_vec(),
            crate::memtable::EntryValue::Value(b"ATGC".to_vec()),
        )];
        let primary_segment =
            crate::segment::SegmentWriter::write_segment(&primary, 1, &entries).unwrap();
        let mirror = DualStrandMirror::new(&primary, &secondary).unwrap();
        mirror.repair_segments().unwrap();

        let mut raw = std::fs::read(&primary_segment).unwrap();
        raw[25] ^= 0xff;
        std::fs::write(&primary_segment, raw).unwrap();
        mirror.repair_segments().unwrap();

        let reader = SegmentReader::open(&primary_segment).unwrap();
        reader.validate_all().unwrap();
        assert_eq!(
            reader.get(b"chr1:42").unwrap(),
            Some(crate::segment::SegmentLookup::Found(b"ATGC".to_vec()))
        );
    }
}
