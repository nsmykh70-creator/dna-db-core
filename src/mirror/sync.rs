// SPDX-License-Identifier: AGPL-3.0-only
//! Mirror repair / heal / hot-swap resync operations: segment-set repair,
//! closed-WAL healing in both directions and the full disk replacement
//! resync. Split from the former mirror.rs monolith — no behaviour change.

use crate::segment::SegmentFile;
use crate::wal::{SyncMode, WalParity, WalReader, WalWriter};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::fs_util::{
    atomic_write, copy_atomic, copy_segment_pair, encode_manifest, read_manifest,
    select_manifest, sync_bloom, sync_directory, validate_segment, wal_copy_valid_len,
    MANIFEST_FILE,
};
use super::{DualStrandMirror, StrandStatus};
use super::fs_util::{files_equal, formats_differ};

impl DualStrandMirror {
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

}
