// SPDX-License-Identifier: AGPL-3.0-only
//! Crash-safe filesystem helpers for the dual-strand mirror: the
//! `segments.manifest` codec, atomic copy primitives and bloom sidecar
//! syncing. Split from the former mirror.rs monolith — no behaviour change.

use crate::segment::SegmentFile;
use crate::wal::WalReader;
use std::io::{self, Read, Write};
use std::path::Path;

pub(super) const MANIFEST_FILE: &str = "segments.manifest";
const MANIFEST_MAGIC: &[u8; 8] = b"DNAMAN01";

#[derive(Clone)]
pub(super) struct SegmentManifest {
    epoch: u64,
    segment_ids: Vec<u64>,
}

pub(super) fn read_manifest(path: &Path) -> io::Result<Option<SegmentManifest>> {
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

pub(super) fn select_manifest(
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

pub(super) fn encode_manifest(epoch: u64, segment_ids: &[u64]) -> io::Result<Vec<u8>> {
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

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

pub(super) fn validate_segment(path: &Path) -> io::Result<()> {
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
pub(super) fn formats_differ(a: &Path, b: &Path) -> bool {
    match (SegmentFile::open(a), SegmentFile::open(b)) {
        (Ok(left), Ok(right)) => left.format_tag() != right.format_tag(),
        _ => false,
    }
}

pub(super) fn copy_atomic(source: &Path, destination: &Path) -> io::Result<()> {
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

pub(super) fn copy_segment_pair(source_segment: &Path, destination_dir: &Path) -> io::Result<()> {
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

pub(super) fn sync_bloom(source_segment: &Path, destination_segment: &Path) -> io::Result<()> {
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
pub(super) fn wal_copy_valid_len(path: &Path) -> Option<u64> {
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
pub(super) fn wal_copy_holds_valid_frames(path: &Path) -> bool {
    wal_copy_valid_len(path).is_some()
}

pub(super) fn files_equal(left: &Path, right: &Path) -> io::Result<bool> {
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

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
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

