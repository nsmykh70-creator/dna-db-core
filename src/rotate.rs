// SPDX-License-Identifier: AGPL-3.0-only
//! Offline encryption-key rotation.
//!
//! Rotation never rewrites SST/WAL bytes in place. It opens the existing
//! database read-only with the old key, copies every live logical row into a
//! fresh directory using the new key, copies the non-database sidecars
//! (`users.json`, `security_state.json`, `tls/`) and finally swaps the
//! directories. The old directory is retained as
//! `<data-dir>.pre-rotate-<timestamp>` for rollback; delete it together with
//! the old key after verification.
//!
//! This also handles a historical plaintext database (`old_crypto=None`):
//! every row is read as plaintext and written encrypted with the new key.
//!
//! The operation is offline. It must not run while a server has the directory
//! open. Allocation is proportional to the number of live logical rows because
//! the engine exposes a merged scan, not a streaming iterator; the documented
//! rotation procedure therefore requires free RAM for one copy of the logical
//! dataset.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::crypto::Crypto;
use crate::engine::{Engine, EngineConfig};
use crate::repair::RepairPolicy;
use crate::wal::{SyncMode, WalOp};

pub struct RotateOptions {
    pub memtable_capacity: usize,
    pub erasure_k: usize,
    pub erasure_m: usize,
    pub wal_segment_capacity: u64,
    /// Optional external Strand B directory (the server's `--mirror-dir`).
    /// When set, rotation rebuilds it from the rotated primary before the
    /// operation reports success, so the mirror can never keep old-key data.
    pub mirror_dir: Option<PathBuf>,
}

impl Default for RotateOptions {
    fn default() -> Self {
        Self {
            memtable_capacity: 8 * 1024 * 1024,
            erasure_k: 4,
            erasure_m: 2,
            wal_segment_capacity: 64 * 1024 * 1024,
            mirror_dir: None,
        }
    }
}

#[derive(Debug)]
pub struct RotateReport {
    pub keys: usize,
    pub plaintext_bytes: u64,
    pub backup_dir: PathBuf,
    pub new_fingerprint: String,
    pub mirror_backup_dir: Option<PathBuf>,
    pub mirror_rebuilt: bool,
}

fn engine_config(dir: &Path, crypto: Option<Arc<Crypto>>, options: &RotateOptions) -> EngineConfig {
    EngineConfig {
        dir_path: dir.to_path_buf(),
        memtable_capacity: options.memtable_capacity,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        erasure_k: options.erasure_k,
        erasure_m: options.erasure_m,
        repair_policy: RepairPolicy::RepairOnRead,
        scrub: None,
        wal_segment_capacity: options.wal_segment_capacity,
        crypto,
    }
}

/// Rotate the payload key of an offline data directory.
pub fn rotate_key(
    data_dir: &Path,
    old_crypto: Option<Arc<Crypto>>,
    new_crypto: Arc<Crypto>,
    options: RotateOptions,
) -> io::Result<RotateReport> {
    if !data_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("data directory {:?} does not exist", data_dir),
        ));
    }

    if let (Some(old), new) = (&old_crypto, &new_crypto) {
        if old.fingerprint() == new.fingerprint() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "old and new encryption keys are identical",
            ));
        }
    }

    let parent = data_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = data_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dna_data");
    let stamp = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let staging = parent.join(format!(".{name}.rotate-{stamp}"));
    let backup = parent.join(format!("{name}.pre-rotate-{stamp}"));
    if staging.exists() || backup.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "rotation staging/backup directory already exists",
        ));
    }

    // Resolve an optional external mirror before touching the primary. The
    // mirror must not be the data directory or nested inside it (the atomic
    // directory swap would move it into the rollback backup).
    let mirror_layout = if let Some(mirror) = options.mirror_dir.as_ref() {
        if mirror.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mirror directory is empty",
            ));
        }
        let mirror_parent = mirror
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(mirror_parent)?;
        let mirror_name = mirror
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "mirror directory must end in a normal path component",
                )
            })?;
        let canonical_data = data_dir.canonicalize()?;
        let canonical_mirror = if mirror.exists() {
            mirror.canonicalize()?
        } else {
            mirror_parent.canonicalize()?.join(mirror_name)
        };
        if canonical_data == canonical_mirror
            || canonical_mirror.starts_with(&canonical_data)
            || canonical_data.starts_with(&canonical_mirror)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mirror directory must be outside the data directory",
            ));
        }
        if mirror.exists() && !mirror.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mirror path exists but is not a directory",
            ));
        }
        let mirror_staging = mirror_parent.join(format!(".{mirror_name}.rotate-{stamp}"));
        let mirror_backup = mirror_parent.join(format!("{mirror_name}.pre-rotate-{stamp}"));
        if mirror_staging.exists() || mirror_backup.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "mirror staging/backup directory already exists",
            ));
        }
        Some((mirror.clone(), mirror_staging, mirror_backup))
    } else {
        None
    };

    fs::create_dir(&staging)?;

    let result = (|| -> io::Result<(usize, u64)> {
        let old_was_plaintext = old_crypto.is_none();
        let old = Engine::open(engine_config(data_dir, old_crypto, &options))?;
        let new = Engine::open(engine_config(&staging, Some(new_crypto.clone()), &options))?;

        // A merged scan returns every live logical row, including schemas,
        // indexes and user tables. Writing through write_batch produces one
        // WAL fsync per chunk and re-encrypts each value with the new key.
        let rows = old.scan_prefix(b"")?;
        if old_was_plaintext && rows.iter().any(|(_, value)| value.starts_with(b"DNAE1")) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "database contains encrypted values but no old key was supplied; \
                 pass --old-key-file or DNA_DB_ENCRYPTION_KEY",
            ));
        }
        let mut keys = 0usize;
        let mut plaintext_bytes = 0u64;
        for chunk in rows.chunks(1000) {
            let ops: Vec<(WalOp, Vec<u8>, Option<Vec<u8>>)> = chunk
                .iter()
                .map(|(key, value)| {
                    keys += 1;
                    plaintext_bytes += (key.len() + value.len()) as u64;
                    (WalOp::Put, key.clone(), Some(value.clone()))
                })
                .collect();
            if !ops.is_empty() {
                new.write_batch(&ops)?;
            }
        }
        new.flush()?;
        new.checkpoint()?;
        drop(new);
        drop(old);
        Ok((keys, plaintext_bytes))
    })();

    let (keys, plaintext_bytes) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Non-database state is intentionally kept in the same directory by the
    // server. Copy it into the new tree before the atomic directory swap.
    for file in ["users.json", "security_state.json"] {
        let src = data_dir.join(file);
        if src.is_file() {
            fs::copy(&src, staging.join(file))?;
        }
    }
    let tls_src = data_dir.join("tls");
    if tls_src.is_dir() {
        copy_dir_all(&tls_src, &staging.join("tls"))?;
    }

    if let Err(e) = fs::rename(data_dir, &backup) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    if let Err(e) = fs::rename(&staging, data_dir) {
        // Best-effort rollback: put the untouched original back.
        let _ = fs::rename(&backup, data_dir);
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let mut mirror_backup_dir = None;
    let mut mirror_rebuilt = false;
    if let Some((mirror, mirror_staging, mirror_backup)) = mirror_layout {
        match rebuild_mirror(
            data_dir,
            &mirror,
            &mirror_staging,
            &mirror_backup,
            &new_crypto,
            &options,
        ) {
            Ok(had_old_mirror) => {
                mirror_rebuilt = true;
                if had_old_mirror {
                    mirror_backup_dir = Some(mirror_backup);
                }
            }
            Err(mirror_error) => {
                // The primary is already new-key. Put the old primary back so
                // the operator never ends up with a new primary beside an
                // old-key mirror that looks like a valid rollback point.
                let rollback = rollback_primary(data_dir, &backup, &stamp);
                let _ = fs::remove_dir_all(&mirror_staging);
                return Err(match rollback {
                    Ok(()) => io::Error::new(
                        mirror_error.kind(),
                        format!(
                            "mirror rebuild failed ({mirror_error}); primary rotation rolled back"
                        ),
                    ),
                    Err(rollback_error) => io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "mirror rebuild failed ({mirror_error}) and primary rollback failed \
                             ({rollback_error}); old primary is at {:?}",
                            backup
                        ),
                    ),
                });
            }
        }
    }

    Ok(RotateReport {
        keys,
        plaintext_bytes,
        backup_dir: backup,
        new_fingerprint: new_crypto.fingerprint(),
        mirror_backup_dir,
        mirror_rebuilt,
    })
}

/// Rebuild an external Strand B from the already-rotated primary. The new
/// mirror is assembled in a staging directory, then swapped into place; the
/// previous mirror is retained for rollback.
fn rebuild_mirror(
    data_dir: &Path,
    mirror: &Path,
    mirror_staging: &Path,
    mirror_backup: &Path,
    new_crypto: &Arc<Crypto>,
    options: &RotateOptions,
) -> io::Result<bool> {
    let result = (|| -> io::Result<()> {
        let mut config = engine_config(data_dir, Some(new_crypto.clone()), options);
        config.mirror_disk_path = Some(mirror_staging.to_path_buf());
        let engine = Engine::open(config)?;
        engine.flush()?;
        engine.checkpoint()?;
        drop(engine);
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(mirror_staging);
        return Err(error);
    }

    let had_old = mirror.exists();
    if had_old {
        if mirror_backup.exists() {
            let _ = fs::remove_dir_all(mirror_staging);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("mirror backup {:?} already exists", mirror_backup),
            ));
        }
        fs::rename(mirror, mirror_backup)?;
    }
    if let Err(error) = fs::rename(mirror_staging, mirror) {
        if had_old {
            let _ = fs::rename(mirror_backup, mirror);
        }
        let _ = fs::remove_dir_all(mirror_staging);
        return Err(error);
    }
    Ok(had_old)
}

fn rollback_primary(data_dir: &Path, backup: &Path, stamp: &str) -> io::Result<()> {
    let parent = data_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = data_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("dna_data");
    let failed = parent.join(format!(".{name}.rotate-failed-{stamp}"));
    fs::rename(data_dir, &failed)?;
    match fs::rename(backup, data_dir) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&failed);
            Ok(())
        }
        Err(error) => {
            // Try to leave the newly rotated tree at the canonical path rather
            // than hiding both versions behind a non-canonical name.
            let _ = fs::rename(&failed, data_dir);
            Err(error)
        }
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dna_rotate_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_payload_and_preserves_rows() {
        let dir = temp_dir("ok");
        let old = Arc::new(Crypto::from_key([7u8; 32]));
        let new = Arc::new(Crypto::from_key([9u8; 32]));
        {
            let engine = Engine::open(engine_config(
                &dir,
                Some(old.clone()),
                &RotateOptions::default(),
            ))
            .unwrap();
            engine.put(b"chr1:1", b"secret-A").unwrap();
            engine.put(b"chr1:2", b"secret-B").unwrap();
            engine.put(b"index:chr1", b"idx").unwrap();
            engine.flush().unwrap();
            engine.checkpoint().unwrap();
        }

        let report = rotate_key(
            &dir,
            Some(old.clone()),
            new.clone(),
            RotateOptions::default(),
        )
        .unwrap();
        assert_eq!(report.keys, 3);
        assert!(report.backup_dir.is_dir());

        let engine = Engine::open(engine_config(
            &dir,
            Some(new.clone()),
            &RotateOptions::default(),
        ))
        .unwrap();
        assert_eq!(
            engine.get(b"chr1:1").unwrap().as_deref(),
            Some(b"secret-A".as_slice())
        );
        assert_eq!(
            engine.get(b"chr1:2").unwrap().as_deref(),
            Some(b"secret-B".as_slice())
        );
        assert!(engine.get(b"missing").unwrap().is_none());
        drop(engine);

        let old_engine = Engine::open(engine_config(&dir, Some(old), &RotateOptions::default()));
        // The new envelopes are authenticated with the new key; opening with
        // the old key must fail on read, not return garbage.
        if let Ok(old_engine) = old_engine {
            assert!(old_engine.get(b"chr1:1").is_err());
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&report.backup_dir);
    }

    #[test]
    fn rotates_with_external_mirror_and_rebuilds_it() {
        let dir = temp_dir("mirror");
        let mirror = temp_dir("mirror_b");
        let old = Arc::new(Crypto::from_key([3u8; 32]));
        let new = Arc::new(Crypto::from_key([4u8; 32]));
        {
            let mut config = engine_config(&dir, Some(old.clone()), &RotateOptions::default());
            config.mirror_disk_path = Some(mirror.clone());
            let engine = Engine::open(config).unwrap();
            engine.put(b"chr1:1", b"mirrored-secret").unwrap();
            engine.flush().unwrap();
            engine.checkpoint().unwrap();
        }

        let options = RotateOptions {
            mirror_dir: Some(mirror.clone()),
            ..RotateOptions::default()
        };
        let report = rotate_key(&dir, Some(old.clone()), new.clone(), options).unwrap();
        assert!(report.mirror_rebuilt);
        assert!(report.mirror_backup_dir.clone().unwrap().is_dir());

        let mut config = engine_config(&dir, Some(new.clone()), &RotateOptions::default());
        config.mirror_disk_path = Some(mirror.clone());
        let engine = Engine::open(config).unwrap();
        assert_eq!(
            engine.get(b"chr1:1").unwrap().as_deref(),
            Some(b"mirrored-secret".as_slice())
        );
        drop(engine);

        let old_engine = Engine::open(engine_config(&dir, Some(old), &RotateOptions::default()));
        if let Ok(old_engine) = old_engine {
            assert!(old_engine.get(b"chr1:1").is_err());
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&mirror);
        let _ = fs::remove_dir_all(report.backup_dir);
        if let Some(backup) = report.mirror_backup_dir {
            let _ = fs::remove_dir_all(backup);
        }
    }

    #[test]
    fn rejects_same_key() {
        let dir = temp_dir("same");
        let key = Arc::new(Crypto::from_key([1u8; 32]));
        let err = rotate_key(&dir, Some(key.clone()), key, RotateOptions::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let _ = fs::remove_dir_all(&dir);
    }
}
