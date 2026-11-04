// SPDX-License-Identifier: AGPL-3.0-only
//! Dual-Strand Disk Mirroring (RAID-1-like failover)
//!
//! Strand A (primary dir) + Strand B (secondary dir). Every WAL frame and
//! active segment manifest is durably written to both strands. If B fails,
//! writes are blocked until the mirror is repaired or resynced.
//!
//! Hot-swap resync: copies all primary files (WAL + segments + bloom
//! sidecars and active-set manifest onto the replacement disk and reopens B's WAL.
//!
//! Module map (split from the former mirror.rs monolith, no behaviour change):
//! * [`sync`]     — repair / heal / resync operations on both strands
//! * [`fs_util`]  — manifest codec and crash-safe file copy helpers

use crate::wal::{SyncMode, WalEntry, WalOp, WalReader, WalWriter};
use parking_lot::{Mutex, RwLock};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

mod fs_util;
mod sync;

use fs_util::copy_atomic;

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
}

impl DualStrandMirror {
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

