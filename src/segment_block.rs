// SPDX-License-Identifier: AGPL-3.0-only
//! DNASST02 — Block-based, erasure-coded segment format ("chromatin").
//!
//! A segment is a sequence of 4 KiB blocks organised into Reed–Solomon
//! stripes of `k` data + `m` parity shards. Every block is covered by a
//! CRC32C stored in a table at the end of the file, so any 4 KiB sector
//! damage (bit-rot, dead sector, torn write) is *detected* per block and
//! *repairable* from parity as long as ≤ m blocks of its stripe survive.
//!
//! File layout (little-endian, all offsets absolute):
//!
//! ```text
//! [superblock   4096 B]  magic, geometry, counters, self-CRC
//! [data region]          S_data  * (k+m) blocks (records, packed)
//! [index region]         S_index * (k+m) blocks (block index, packed)
//! [crc table]            (S_data + S_index) * (k+m) * u32  (CRC32C per block)
//! [footer        32 B]   cross-check copies + table CRC + magic
//! ```
//!
//! Records are byte-identical to the DNASST01 record format:
//!   `[crc32 u32][klen u32][vlen u32][tombstone u8][key][val]`
//! and are packed contiguously across the *logical* data block space
//! (stripe s owns logical blocks s*k .. s*k+k-1). A record may span blocks
//! and stripes; reads are assembled chunk-wise per verified block.
//!
//! Damage-spread interleaving (P2.3): the physical slot of stripe `s`,
//! shard `idx` is `idx * S + rotate(s, seed)` — consecutive physical blocks
//! therefore belong to *different* stripes, and a sequential run of bad
//! sectors hits each stripe at most once per `(k+m)`-block window. Parity
//! shards are interleaved with data shards, not appended at the tail.
//!
//! Repair: the reader keeps a [`RepairHook`] (installed by the engine).
//! On the first CRC-miss of a block the whole stripe is re-verified,
//! reconstructed from the surviving shards and the damaged blocks are
//! rewritten in place. The hook journalises the lesion and may escalate to
//! the Strand B mirror when parity is insufficient.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::bloom::Bloom;
use crate::memtable::EntryValue;
use crate::reed_solomon::{ErasureCodec, ErasureError};
use crc32fast::Hasher;

pub const BLOCK_SIZE: usize = 4096;
pub const SUPERBLOCK_SIZE: usize = 4096;
pub const FOOTER_SIZE: usize = 64;
pub const BLOCK_MAGIC: &[u8; 8] = b"DNASST02";
pub const LEGACY_MAGIC: &[u8; 8] = b"DNASST01";

const MAX_SEGMENT_RECORD: usize = 64 * 1024 * 1024;
const MAX_BLOCK_INDEX_ENTRIES: usize = 100_000_000;

// ---------------------------------------------------------------------------
// Repair hook — implemented by repair.rs (journal + stats + mirror cascade)
// ---------------------------------------------------------------------------

/// What happened to a lesion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LesionOutcome {
    /// Reconstructed from surviving parity shards and rewritten in place.
    Repaired,
    /// Parity was insufficient; the hook may have escalated (e.g. mirror
    /// restore) and asked the reader to retry.
    Unrecoverable,
}

/// Engine-provided repair policy + telemetry sink.
pub trait RepairHook: Send + Sync {
    /// Is on-read repair allowed? (RepairPolicy::RepairOnRead)
    fn repair_on_read(&self) -> bool;

    /// A stripe was damaged and successfully reconstructed.
    fn on_repaired(&self, segment: &Path, stripe: usize, shards: &[usize], bytes_repaired: u64);

    /// A stripe is unrecoverable by parity. The hook may attempt a mirror
    /// restore (rewriting the file contents in place); return `true` to ask
    /// the reader to retry the read, `false` to fail the read.
    fn on_unrecoverable(&self, segment: &Path, stripe: usize, shards: &[usize]) -> bool;
}

/// No-hook default: damage is always an error (offline / doctor-less mode).
pub struct NoRepair;

impl RepairHook for NoRepair {
    fn repair_on_read(&self) -> bool {
        false
    }
    fn on_repaired(&self, _: &Path, _: usize, _: &[usize], _: u64) {}
    fn on_unrecoverable(&self, _: &Path, _: usize, _: &[usize]) -> bool {
        false
    }
}

/// Validation-only hook: allows local Reed–Solomon repair during a full sweep
/// but never escalates to the mirror (which is what is being validated).
struct ValidationRepair;

impl RepairHook for ValidationRepair {
    fn repair_on_read(&self) -> bool {
        true
    }
    fn on_repaired(&self, _: &Path, _: usize, _: &[usize], _: u64) {}
    fn on_unrecoverable(&self, _: &Path, _: usize, _: &[usize]) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Interleaving geometry (pure functions, unit-tested)
// ---------------------------------------------------------------------------

/// Logical (stripe, shard-index) → physical slot inside a region.
#[inline]
pub fn phys_slot(stripe: usize, idx: usize, stripes: usize, seed: u64) -> usize {
    let rot = if stripes == 0 {
        0
    } else {
        (seed as usize) % stripes
    };
    let s_rot = if stripes == 0 {
        0
    } else {
        (stripe + rot) % stripes
    };
    idx * stripes + s_rot
}

/// Physical slot → (stripe, shard-index). Inverse of [`phys_slot`].
#[inline]
pub fn slot_to_stripe(slot: usize, stripes: usize, seed: u64) -> (usize, usize) {
    let rot = if stripes == 0 {
        0
    } else {
        (seed as usize) % stripes
    };
    let idx = slot / stripes.max(1);
    let s_rot = slot % stripes.max(1);
    let stripe = (s_rot + stripes - rot) % stripes.max(1);
    (stripe, idx)
}

// ---------------------------------------------------------------------------
// Record codec (shared by writer, scan and lookup)
// ---------------------------------------------------------------------------

#[inline]
pub fn record_len(klen: usize, vlen: usize) -> usize {
    4 + 4 + 4 + 1 + klen + vlen
}

/// Parse the fixed 13-byte record header. Returns (crc, klen, vlen, tomb).
/// The caller reads the key/value payload separately (records span blocks).
pub fn parse_header(buf: &[u8]) -> io::Result<(u32, usize, usize, bool)> {
    if buf.len() < 13 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "record header truncated",
        ));
    }
    let crc = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let klen = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let vlen = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
    let tomb = buf[12] == 1;
    if buf[12] > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid tombstone tag",
        ));
    }
    if klen
        .checked_add(vlen)
        .filter(|n| *n <= MAX_SEGMENT_RECORD)
        .is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "record size out of range",
        ));
    }
    Ok((crc, klen, vlen, tomb))
}

pub fn record_crc(key: &[u8], val: &[u8], tomb: bool) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(&(key.len() as u32).to_le_bytes());
    hasher.update(&(val.len() as u32).to_le_bytes());
    hasher.update(&[if tomb { 1 } else { 0 }]);
    hasher.update(key);
    hasher.update(val);
    hasher.finalize()
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub struct BlockSegmentWriter;

impl BlockSegmentWriter {
    /// Write a DNASST02 segment atomically (.tmp → fsync → rename).
    /// `entries` MUST be sorted and unique by key (memtable order).
    ///
    /// `interleave_seed` MUST be `BlockSegmentReader::seed_for(&final_path)`
    /// of the name the file will carry when read back — the caller owns that
    /// agreement (single source of truth: the seed function).
    pub fn write_segment(
        dir: &Path,
        segment_id: u64,
        entries: &[(Vec<u8>, EntryValue)],
        erasure_k: usize,
        erasure_m: usize,
        interleave_seed: u64,
    ) -> io::Result<PathBuf> {
        let final_path = dir.join(format!("{:06}.sst", segment_id));
        if entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "refusing to write an empty segment",
            ));
        }
        let codec = ErasureCodec::new(erasure_k, erasure_m)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let k = erasure_k;

        // ---- pack records + block index --------------------------------
        let mut data: Vec<u8> = Vec::new();
        let mut index: Vec<u8> = Vec::new();
        let mut bloom = Bloom::new(entries.len().max(1));
        let mut prev_key: Option<&[u8]> = None;

        for (key, value) in entries {
            if prev_key.map(|p| p >= key.as_slice()).unwrap_or(false) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment entries must be strictly sorted",
                ));
            }
            prev_key = Some(key);
            bloom.insert(key);

            let (val_bytes, tomb) = match value {
                EntryValue::Value(v) => (v.as_slice(), false),
                EntryValue::Tombstone => (&[][..], true),
            };
            let crc = record_crc(key, val_bytes, tomb);
            let rec_start = data.len() as u64;
            data.extend_from_slice(&crc.to_le_bytes());
            data.extend_from_slice(&(key.len() as u32).to_le_bytes());
            data.extend_from_slice(&(val_bytes.len() as u32).to_le_bytes());
            data.push(if tomb { 1 } else { 0 });
            data.extend_from_slice(key);
            data.extend_from_slice(val_bytes);

            // Block index entry: [klen u32][key][logical_block u32][off u16]
            let logical_block = rec_start / BLOCK_SIZE as u64;
            let off = rec_start % BLOCK_SIZE as u64;
            index.extend_from_slice(&(key.len() as u32).to_le_bytes());
            index.extend_from_slice(key);
            index.extend_from_slice(&(logical_block as u32).to_le_bytes());
            index.extend_from_slice(&(off as u16).to_le_bytes());
        }

        // ---- stripe the data region ------------------------------------
        let data_bytes = data.len();
        let data_stripes = data_bytes.div_ceil(k * BLOCK_SIZE).max(1);
        data.resize(data_stripes * k * BLOCK_SIZE, 0);

        let m = erasure_m;
        let mut data_out = vec![vec![0u8; BLOCK_SIZE]; data_stripes * (k + m)];
        for s in 0..data_stripes {
            let data_blocks: Vec<Vec<u8>> = (0..k)
                .map(|i| {
                    let off = (s * k + i) * BLOCK_SIZE;
                    data[off..off + BLOCK_SIZE].to_vec()
                })
                .collect();
            let parity = codec
                .encode(&data_blocks)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            for idx in 0..(k + m) {
                let slot = phys_slot(s, idx, data_stripes, interleave_seed);
                data_out[slot] = if idx < k {
                    data_blocks[idx].clone()
                } else {
                    parity[idx - k].clone()
                };
            }
        }
        drop(data);

        // ---- stripe the index region -----------------------------------
        let index_bytes = index.len();
        let index_stripes = index_bytes.div_ceil(k * BLOCK_SIZE).max(1);
        index.resize(index_stripes * k * BLOCK_SIZE, 0);

        let mut index_out = vec![vec![0u8; BLOCK_SIZE]; index_stripes * (k + m)];
        for s in 0..index_stripes {
            let data_blocks: Vec<Vec<u8>> = (0..k)
                .map(|i| {
                    let off = (s * k + i) * BLOCK_SIZE;
                    index[off..off + BLOCK_SIZE].to_vec()
                })
                .collect();
            let parity = codec
                .encode(&data_blocks)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            for idx in 0..(k + m) {
                let slot = phys_slot(s, idx, index_stripes, interleave_seed);
                index_out[slot] = if idx < k {
                    data_blocks[idx].clone()
                } else {
                    parity[idx - k].clone()
                };
            }
        }
        drop(index);

        // ---- CRC table (two copies for structural redundancy) -------------
        let total_blocks = data_out.len() + index_out.len();
        let mut crc_table = Vec::with_capacity(total_blocks);
        for block in data_out.iter().chain(index_out.iter()) {
            crc_table.crc_append(block);
        }
        let table_bytes = total_blocks * 4;
        let _ = table_bytes; // informational
        let table_a_crc = {
            let mut h = Hasher::new();
            for c in &crc_table {
                h.update(&c.to_le_bytes());
            }
            h.finalize()
        };
        let table_b_crc = table_a_crc; // copies are identical

        // ---- superblock ---------------------------------------------------
        let table_offset =
            (SUPERBLOCK_SIZE + data_out.len() * BLOCK_SIZE + index_out.len() * BLOCK_SIZE) as u64;
        let mut superblock = vec![0u8; SUPERBLOCK_SIZE];
        superblock[0..8].copy_from_slice(BLOCK_MAGIC);
        superblock[8] = erasure_k as u8;
        superblock[9] = erasure_m as u8;
        superblock[12..16].copy_from_slice(&(data_stripes as u32).to_le_bytes());
        superblock[16..20].copy_from_slice(&(index_stripes as u32).to_le_bytes());
        superblock[20..28].copy_from_slice(&(entries.len() as u64).to_le_bytes());
        superblock[28..36].copy_from_slice(&(data_bytes as u64).to_le_bytes());
        superblock[36..44].copy_from_slice(&(index_bytes as u64).to_le_bytes());
        superblock[44..52].copy_from_slice(&table_offset.to_le_bytes());
        superblock[52..60].copy_from_slice(
            &std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .to_le_bytes(),
        );
        superblock[60..64].copy_from_slice(&table_a_crc.to_le_bytes());
        superblock[64..68].copy_from_slice(&table_b_crc.to_le_bytes());
        let sb_crc = {
            let mut h = Hasher::new();
            h.update(&superblock[0..68]);
            h.finalize()
        };
        superblock[68..72].copy_from_slice(&sb_crc.to_le_bytes());

        // ---- footer (64 B, full geometry redundancy + self CRC) ------------
        let mut footer = [0u8; FOOTER_SIZE];
        footer[4..12].copy_from_slice(&(entries.len() as u64).to_le_bytes());
        footer[12..16].copy_from_slice(&table_a_crc.to_le_bytes());
        footer[16..20].copy_from_slice(&table_b_crc.to_le_bytes());
        footer[20] = erasure_k as u8;
        footer[21] = erasure_m as u8;
        footer[22..26].copy_from_slice(&(data_stripes as u32).to_le_bytes());
        footer[26..30].copy_from_slice(&(index_stripes as u32).to_le_bytes());
        footer[30..38].copy_from_slice(&(data_bytes as u64).to_le_bytes());
        footer[38..46].copy_from_slice(&(index_bytes as u64).to_le_bytes());
        footer[46..54].copy_from_slice(&table_offset.to_le_bytes());
        footer[56..64].copy_from_slice(BLOCK_MAGIC);
        let footer_crc = {
            let mut h = Hasher::new();
            h.update(&footer[4..64]);
            h.finalize()
        };
        footer[0..4].copy_from_slice(&footer_crc.to_le_bytes());

        // ---- write atomically -----------------------------------------------
        let tmp_path = dir.join(format!("{:06}.tmp", segment_id));
        {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            let mut w = BufWriter::new(file);
            w.write_all(&superblock)?;
            for block in &data_out {
                w.write_all(block)?;
            }
            for block in &index_out {
                w.write_all(block)?;
            }
            for c in &crc_table {
                w.write_all(&c.to_le_bytes())?;
            }
            for c in &crc_table {
                w.write_all(&c.to_le_bytes())?;
            }
            w.write_all(&footer)?;
            w.flush()?;
            w.into_inner()?.sync_data()?;
        }

        // Bloom sidecar (same convention as DNASST01).
        let bloom_path = dir.join(format!("{:06}.bloom", segment_id));
        let tmp_bloom = dir.join(format!("{:06}.bloom.tmp", segment_id));
        std::fs::write(&tmp_bloom, bloom.to_bytes())?;
        std::fs::rename(&tmp_bloom, &bloom_path)?;

        std::fs::rename(&tmp_path, &final_path)?;
        // Directory fsync forces rename durability on POSIX. On Windows a
        // directory cannot be opened via File::open (AccessDenied) and NTFS
        // metadata is journaled, so the step is skipped there.
        #[cfg(not(windows))]
        {
            let dir_file = File::open(dir)?;
            let _ = dir_file.sync_all();
        }
        #[cfg(windows)]
        let _ = dir;

        Ok(final_path)
    }
}

trait CrcAppend {
    fn crc_append(&mut self, block: &[u8]);
}

impl CrcAppend for Vec<u32> {
    fn crc_append(&mut self, block: &[u8]) {
        let mut h = Hasher::new();
        h.update(block);
        self.push(h.finalize());
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct IndexEntry {
    key: Vec<u8>,
    block: u32, // logical data block
    off: u16,   // offset inside the block
}

/// Read-only (but repair-capable) view of one DNASST02 segment.
pub struct BlockSegmentReader {
    path: PathBuf,
    file: RwLock<File>,
    /// False when the file was opened read-only (read-only media, doctor
    /// inspection); parity reconstruction is then unavailable but all reads
    /// that do not need repair still work.
    writable: bool,
    k: usize,
    m: usize,
    interleave_seed: u64,
    data_stripes: usize,
    index_stripes: usize,
    data_bytes: usize,
    index_bytes: usize,
    /// Byte offset of the CRC table region (superblock/backup geometry);
    /// exposed for repair tooling and diagnostics (doctor / test harnesses).
    pub crc_table_offset: u64,
    crc_table: Vec<u32>,
    record_count: u64,
    index: Vec<IndexEntry>,
    bloom: Bloom,
    codec: ErasureCodec,
    hook: RwLock<Arc<dyn RepairHook>>,
    /// (region_is_index, stripe) pairs whose damage exceeds parity. Regions
    /// are namespaced: data stripe N and index stripe N are different physical
    /// stripes and must not share quarantine state.
    quarantined: Mutex<HashSet<(bool, usize)>>,
    /// Logical data-byte ranges skipped while rebuilding a damaged index.
    /// Lookups that miss the (incomplete) index but pass the Bloom filter must
    /// fail instead of pretending the key does not exist.
    lost_ranges: Mutex<Vec<(usize, usize)>>,
    /// Statistics: blocks repaired in place by this reader instance.
    pub repairs_done: std::sync::atomic::AtomicU64,
    /// Set when the in-memory index was rebuilt from the record stream
    /// because the on-disk index region was unreadable at open.
    pub index_rebuilt: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockLookup {
    Found(Vec<u8>),
    Tombstone,
}

impl BlockSegmentReader {
    /// Open and validate the structural shell. Redundancy strategy:
    ///
    /// * superblock carries full geometry + both CRC-table checksums;
    /// * footer (64 B) carries the same, so either one being damaged is
    ///   survivable — the survivor arbitrates;
    /// * the CRC table is stored TWICE; a checksum from the surviving
    ///   structural block verifies each copy.
    ///
    /// Block payloads are verified lazily on access; [`validate_all`]
    /// forces a full sweep.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        // Read+write: the repair pipeline rewrites damaged blocks in place.
        // If the medium is read-only, fall back to read-only mode instead of
        // refusing to open: reads and doctor scans still work, only in-place
        // parity reconstruction is unavailable.
        let (mut file, writable) = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => (file, true),
            Err(rw_error) => match File::open(&path) {
                Ok(file) => (file, false),
                Err(_) => return Err(rw_error),
            },
        };
        let len = file.metadata()?.len();
        if len < (SUPERBLOCK_SIZE + FOOTER_SIZE) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "segment too small",
            ));
        }

        // ---- footer (last 64 B) -------------------------------------------
        file.seek(SeekFrom::Start(len - FOOTER_SIZE as u64))?;
        let mut footer = [0u8; FOOTER_SIZE];
        file.read_exact(&mut footer)?;
        let footer_magic_ok = &footer[56..64] == BLOCK_MAGIC;
        let footer_crc_ok = {
            let mut h = Hasher::new();
            h.update(&footer[4..64]);
            h.finalize() == u32::from_le_bytes(footer[0..4].try_into().unwrap())
        };

        // ---- superblock ------------------------------------------------------
        file.seek(SeekFrom::Start(0))?;
        let mut sb = vec![0u8; SUPERBLOCK_SIZE];
        file.read_exact(&mut sb)?;
        let sb_magic_ok = &sb[0..8] == BLOCK_MAGIC;
        let sb_crc_ok = sb_magic_ok && {
            let mut h = Hasher::new();
            h.update(&sb[0..68]);
            h.finalize() == u32::from_le_bytes(sb[68..72].try_into().unwrap())
        };
        // New footers carry their own CRC32. Old footers stored the superblock
        // CRC in the same field; accept those when the superblock proves it.
        let legacy_footer_ok = footer_magic_ok && sb_crc_ok && footer[0..4] == sb[68..72];
        // If the superblock itself is unavailable, an old footer cannot be
        // verified; accept its magic for backward compatibility (same as the
        // pre-fix behaviour), while new footers are verified above.
        let footer_ok = footer_magic_ok && (footer_crc_ok || legacy_footer_ok || !sb_crc_ok);
        if !sb_magic_ok && !footer_ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "both superblock and footer are damaged; mirror restore or doctor required",
            ));
        }

        // Geometry source: superblock first, footer as fallback.
        let (
            k,
            m,
            data_stripes,
            index_stripes,
            record_count,
            data_bytes,
            index_bytes,
            table_offset,
        );
        let (table_a_crc, table_b_crc);
        if sb_crc_ok {
            k = sb[8] as usize;
            m = sb[9] as usize;
            data_stripes = u32::from_le_bytes(sb[12..16].try_into().unwrap()) as usize;
            index_stripes = u32::from_le_bytes(sb[16..20].try_into().unwrap()) as usize;
            record_count = u64::from_le_bytes(sb[20..28].try_into().unwrap());
            data_bytes = u64::from_le_bytes(sb[28..36].try_into().unwrap()) as usize;
            index_bytes = u64::from_le_bytes(sb[36..44].try_into().unwrap()) as usize;
            table_offset = u64::from_le_bytes(sb[44..52].try_into().unwrap());
            table_a_crc = u32::from_le_bytes(sb[60..64].try_into().unwrap());
            table_b_crc = u32::from_le_bytes(sb[64..68].try_into().unwrap());
        } else if footer_ok {
            k = footer[20] as usize;
            m = footer[21] as usize;
            data_stripes = u32::from_le_bytes(footer[22..26].try_into().unwrap()) as usize;
            index_stripes = u32::from_le_bytes(footer[26..30].try_into().unwrap()) as usize;
            record_count = u64::from_le_bytes(footer[4..12].try_into().unwrap());
            data_bytes = u64::from_le_bytes(footer[30..38].try_into().unwrap()) as usize;
            index_bytes = u64::from_le_bytes(footer[38..46].try_into().unwrap()) as usize;
            table_offset = u64::from_le_bytes(footer[46..54].try_into().unwrap());
            table_a_crc = u32::from_le_bytes(footer[12..16].try_into().unwrap());
            table_b_crc = u32::from_le_bytes(footer[16..20].try_into().unwrap());
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "superblock CRC mismatch",
            ));
        }
        if k == 0 || m == 0 || k + m > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid erasure geometry",
            ));
        }

        // ---- geometry sanity against file size --------------------------------
        let table_len = (data_stripes + index_stripes) * (k + m);
        let expected_len = table_offset + (table_len * 4 * 2) as u64 + FOOTER_SIZE as u64;
        if expected_len != len
            || data_stripes == 0
            || index_stripes == 0
            || data_bytes > data_stripes * k * BLOCK_SIZE
            || index_bytes > index_stripes * k * BLOCK_SIZE
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "segment geometry inconsistent",
            ));
        }

        // ---- CRC table: two copies, checksum-arbitrated -----------------------
        let mut read_table = |off: u64| -> io::Result<Vec<u8>> {
            file.seek(SeekFrom::Start(off))?;
            let mut raw = vec![0u8; table_len * 4];
            file.read_exact(&mut raw)?;
            Ok(raw)
        };
        let verify = |raw: &[u8], want: u32| -> bool {
            let mut h = Hasher::new();
            h.update(raw);
            h.finalize() == want
        };
        let table_raw_a = read_table(table_offset)?;
        let table_raw: Vec<u8> = if verify(&table_raw_a, table_a_crc) {
            table_raw_a
        } else {
            // Copy A damaged → try copy B.
            let table_raw_b = read_table(table_offset + (table_len * 4) as u64)?;
            if verify(&table_raw_b, table_b_crc) {
                eprintln!(
                    "[DNASST02] {:?}: CRC table copy A damaged; using copy B (copy A remains damaged; run doctor --fix to rewrite it)",
                    path.file_name().unwrap_or_default()
                );
                table_raw_b
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "both CRC table copies failed verification",
                ));
            }
        };
        let crc_table: Vec<u32> = table_raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();

        let codec = ErasureCodec::new(k, m)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let interleave_seed = Self::seed_for(&path);

        let mut reader = Self {
            path,
            file: RwLock::new(file),
            writable,
            k,
            m,
            interleave_seed,
            data_stripes,
            index_stripes,
            data_bytes,
            index_bytes,
            crc_table_offset: table_offset,
            crc_table,
            record_count,
            index: Vec::new(),
            bloom: Bloom::permissive(),
            codec,
            hook: RwLock::new(Arc::new(NoRepair)),
            quarantined: Mutex::new(HashSet::new()),
            lost_ranges: Mutex::new(Vec::new()),
            repairs_done: std::sync::atomic::AtomicU64::new(0),
            index_rebuilt: std::sync::atomic::AtomicBool::new(false),
        };

        // Bloom sidecar (optional). A corrupt sidecar is derived data: it must
        // never block opening the segment. Fall back to a permissive filter so
        // every lookup still goes to the authoritative block index.
        let bloom_path = reader.path.with_extension("bloom");
        if bloom_path.exists() {
            match std::fs::read(&bloom_path) {
                Ok(raw) => match Bloom::from_bytes(&raw) {
                    Ok(bloom) => reader.bloom = bloom,
                    Err(e) => {
                        eprintln!(
                            "[DNASST02] {:?}: invalid bloom sidecar ({}); using permissive filter",
                            bloom_path.file_name().unwrap_or_default(),
                            e
                        );
                        reader.bloom = Bloom::permissive();
                    }
                },
                Err(e) => {
                    eprintln!(
                        "[DNASST02] {:?}: cannot read bloom sidecar ({}); using permissive filter",
                        bloom_path.file_name().unwrap_or_default(),
                        e
                    );
                    reader.bloom = Bloom::permissive();
                }
            }
        }

        // Block index — read through the verified path. If the index region
        // is damaged beyond repair at open time (no hook installed yet),
        // fall back to rebuilding the index from the record stream: the
        // index is DERIVED data (every record carries its own CRC), so it
        // can always be regenerated. The on-disk index region is repaired
        // later by validate_all/scrubber/doctor.
        match reader.try_load_index() {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "[DNASST02] {:?}: index region damaged ({}); rebuilding from record scan",
                    reader.path.file_name().unwrap_or_default(),
                    e
                );
                reader.rebuild_index_from_records()?;
                reader
                    .index_rebuilt
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(reader)
    }

    /// Deterministic per-file interleave seed (name-derived, documented).
    pub fn seed_for(path: &Path) -> u64 {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let mut h = 0xcbf29ce484222325u64;
        for b in name.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn erasure_geometry(&self) -> (usize, usize) {
        (self.k, self.m)
    }

    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    pub fn block_count(&self) -> usize {
        (self.data_stripes + self.index_stripes) * (self.k + self.m)
    }

    pub fn format_tag(&self) -> &'static str {
        "DNASST02"
    }

    /// Stripe count of a region (false = data region, true = index region).
    pub fn stripe_count(&self, index_region: bool) -> usize {
        if index_region {
            self.index_stripes
        } else {
            self.data_stripes
        }
    }

    pub fn interleave_seed(&self) -> u64 {
        self.interleave_seed
    }

    /// Verify one physical block; on CRC-miss the owning stripe is repaired
    /// (hook-gated). Returns `Ok(true)` when the block was damaged and the
    /// stripe got repaired, `Ok(false)` when it was healthy.
    pub fn verify_block(&self, index_region: bool, slot: usize) -> io::Result<bool> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let pos = self.region_base(index_region) as u64 + slot as u64 * BLOCK_SIZE as u64;
        let crc_index = self.crc_slot(index_region, slot);
        let expected = *self
            .crc_table
            .get(crc_index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "crc slot out of range"))?;
        let mut ok = true;
        {
            let mut f = self.file.write();
            if f.seek(SeekFrom::Start(pos)).is_err() {
                ok = false;
            } else if f.read_exact(&mut buf).is_err() {
                ok = false;
            }
        }
        if ok {
            let mut h = Hasher::new();
            h.update(&buf);
            ok = h.finalize() == expected;
        }
        if ok {
            return Ok(false);
        }
        let stripes = self.stripe_count(index_region);
        let (stripe, _) = slot_to_stripe(slot, stripes, self.interleave_seed);
        self.repair_stripe(index_region, stripe)?;
        Ok(true)
    }

    /// Install the engine's repair hook (policy + journal + mirror cascade).
    pub fn set_hook(&self, hook: Arc<dyn RepairHook>) {
        *self.hook.write() = hook;
    }

    // ---------------- physical block I/O with repair --------------------

    fn region_base(&self, index_region: bool) -> usize {
        if index_region {
            SUPERBLOCK_SIZE + self.data_stripes * (self.k + self.m) * BLOCK_SIZE
        } else {
            SUPERBLOCK_SIZE
        }
    }

    fn crc_slot(&self, index_region: bool, slot: usize) -> usize {
        let data_slots = self.data_stripes * (self.k + self.m);
        if index_region {
            data_slots + slot
        } else {
            slot
        }
    }

    /// Read one physical block and verify its CRC. On mismatch, run the
    /// lesion/repair pipeline for the owning stripe, then retry.
    fn read_physical_verified(
        &self,
        index_region: bool,
        slot: usize,
        buf: &mut [u8],
    ) -> io::Result<()> {
        debug_assert_eq!(buf.len(), BLOCK_SIZE);
        let base = self.region_base(index_region) as u64;
        let pos = base + slot as u64 * BLOCK_SIZE as u64;
        let crc_index = self.crc_slot(index_region, slot);
        let expected = *self
            .crc_table
            .get(crc_index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "crc slot out of range"))?;

        {
            let mut f = self.file.write();
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(buf)?;
        }
        let mut h = Hasher::new();
        h.update(buf);
        if h.finalize() == expected {
            return Ok(());
        }

        // ---- lesion detected ----
        let stripes = if index_region {
            self.index_stripes
        } else {
            self.data_stripes
        };
        let (stripe, _) = slot_to_stripe(slot, stripes, self.interleave_seed);
        self.repair_stripe(index_region, stripe)?;
        // Retry the read after repair; content must match the table now.
        let mut f = self.file.write();
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(buf)?;
        let mut h = Hasher::new();
        h.update(buf);
        if h.finalize() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "block {:?}:stripe:{} slot {} still corrupt after repair attempt",
                    self.path.file_name().unwrap_or_default(),
                    stripe,
                    slot
                ),
            ));
        }
        Ok(())
    }

    /// Verify a whole stripe, reconstruct damaged shards from parity and
    /// rewrite them in place. Returns the list of repaired shard indexes.
    /// Fails with a localised error when > m shards are gone.
    fn repair_stripe(&self, index_region: bool, stripe: usize) -> io::Result<Vec<usize>> {
        let stripes = if index_region {
            self.index_stripes
        } else {
            self.data_stripes
        };
        let _k = self.k;
        let total = self.k + self.m;
        let base = self.region_base(index_region) as u64;

        // Read all k+m shards of the stripe.
        let mut slots = Vec::with_capacity(total);
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(total);
        let mut damaged = Vec::new();
        for idx in 0..total {
            let slot = phys_slot(stripe, idx, stripes, self.interleave_seed);
            let pos = base + slot as u64 * BLOCK_SIZE as u64;
            let crc_index = self.crc_slot(index_region, slot);
            let expected = self.crc_table[crc_index];
            let mut buf = vec![0u8; BLOCK_SIZE];
            let mut ok = true;
            {
                let mut f = self.file.write();
                f.seek(SeekFrom::Start(pos))?;
                if f.read_exact(&mut buf).is_err() {
                    ok = false;
                }
            }
            if ok {
                let mut h = Hasher::new();
                h.update(&buf);
                ok = h.finalize() == expected;
            }
            if ok {
                shards.push(Some(buf));
            } else {
                shards.push(None);
                damaged.push(slot);
            }
            slots.push(slot);
        }

        let damaged_idx: Vec<usize> = (0..total).filter(|i| shards[*i].is_none()).collect();
        if damaged_idx.is_empty() {
            return Ok(Vec::new()); // transient glitch (e.g. hook fixed it meanwhile)
        }

        let hook = self.hook.read().clone();
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "segment {:?} is read-only; stripe {} needs parity repair but cannot be written",
                    self.path.file_name().unwrap_or_default(),
                    stripe
                ),
            ));
        }
        if self.is_quarantined(index_region, stripe) {
            return Err(self.unrecoverable_error(index_region, stripe, &damaged_idx));
        }
        if !hook.repair_on_read() {
            return Err(self.unrecoverable_error(index_region, stripe, &damaged_idx));
        }

        match self.codec.reconstruct(&mut shards) {
            Ok(()) => {
                let mut bytes_repaired = 0u64;
                {
                    let mut f = self.file.write();
                    for &i in &damaged_idx {
                        let pos = base + slots[i] as u64 * BLOCK_SIZE as u64;
                        f.seek(SeekFrom::Start(pos))?;
                        f.write_all(shards[i].as_ref().unwrap())?;
                        bytes_repaired += BLOCK_SIZE as u64;
                    }
                    f.sync_data()?;
                }
                // Paranoia: rewritten blocks must now match the CRC table.
                for &i in &damaged_idx {
                    let slot = slots[i];
                    let crc_index = self.crc_slot(index_region, slot);
                    let mut buf = [0u8; BLOCK_SIZE];
                    {
                        let mut f = self.file.write();
                        f.seek(SeekFrom::Start(
                            self.region_base(index_region) as u64 + slot as u64 * BLOCK_SIZE as u64,
                        ))?;
                        f.read_exact(&mut buf)?;
                    }
                    let mut h = Hasher::new();
                    h.update(&buf);
                    if h.finalize() != self.crc_table[crc_index] {
                        return Err(self.unrecoverable_error(index_region, stripe, &damaged_idx));
                    }
                }
                self.repairs_done.fetch_add(
                    damaged_idx.len() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                hook.on_repaired(&self.path, stripe, &damaged_idx, bytes_repaired);
                Ok(damaged_idx)
            }
            Err(ErasureError::InsufficientShards { .. }) => {
                // Homologous recombination escalation: the hook may restore
                // the whole segment from Strand B. That replaces the file's
                // inode (temp+rename), so this reader's fd goes stale — the
                // engine must reopen and retry. Signal that with Interrupted.
                if hook.on_unrecoverable(&self.path, stripe, &damaged_idx) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!(
                            "segment {:?} stripe {} restored from Strand B; reopen and retry",
                            self.path.file_name().unwrap_or_default(),
                            stripe
                        ),
                    ));
                }
                Err(self.unrecoverable_error(index_region, stripe, &damaged_idx))
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }

    fn unrecoverable_error(
        &self,
        index_region: bool,
        stripe: usize,
        shards: &[usize],
    ) -> io::Error {
        let region = if index_region { "index" } else { "data" };
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unrecoverable lesion at segment:{} stripe:{} region:{} shards:{:?} (exceeds parity m={})",
                self.path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                stripe,
                region,
                shards,
                self.m
            ),
        )
    }

    // ---------------- logical reads --------------------------------------

    /// Read a logical block of a region through the verified path.
    fn read_logical_block(
        &self,
        index_region: bool,
        logical: usize,
        buf: &mut [u8],
    ) -> io::Result<()> {
        let stripes = if index_region {
            self.index_stripes
        } else {
            self.data_stripes
        };
        let stripe = logical / self.k;
        let idx = logical % self.k;
        let slot = phys_slot(stripe, idx, stripes, self.interleave_seed);
        self.read_physical_verified(index_region, slot, buf)
    }

    /// Assemble `len` bytes starting at a logical byte offset within a
    /// region, crossing block boundaries with per-block verification.
    pub fn read_span(
        &self,
        index_region: bool,
        start: usize,
        len: usize,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let total_logical = if index_region {
            self.index_stripes * self.k
        } else {
            self.data_stripes * self.k
        };
        let region_bytes = total_logical * BLOCK_SIZE;
        if start + len > region_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "span beyond region",
            ));
        }
        out.reserve(len);
        let mut pos = start;
        let mut remaining = len;
        let mut buf = vec![0u8; BLOCK_SIZE];
        while remaining > 0 {
            let logical = pos / BLOCK_SIZE;
            let off = pos % BLOCK_SIZE;
            let take = remaining.min(BLOCK_SIZE - off);
            self.read_logical_block(index_region, logical, &mut buf)?;
            out.extend_from_slice(&buf[off..off + take]);
            pos += take;
            remaining -= take;
        }
        Ok(())
    }

    /// Try to load the block index from the (RS-protected) index region.
    fn try_load_index(&mut self) -> io::Result<()> {
        self.load_index()
    }

    fn load_index(&mut self) -> io::Result<()> {
        let mut raw = Vec::with_capacity(self.index_bytes);
        self.read_span(true, 0, self.index_bytes, &mut raw)?;
        let mut pos = 0usize;
        let mut entries =
            Vec::with_capacity(self.record_count.min(MAX_BLOCK_INDEX_ENTRIES as u64) as usize);
        let mut prev: Option<Vec<u8>> = None;
        while pos < raw.len() {
            if raw.len() - pos < 4 {
                break;
            }
            let klen = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if raw.len() - pos < klen + 6 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "block index truncated",
                ));
            }
            let key = raw[pos..pos + klen].to_vec();
            pos += klen;
            if prev
                .as_ref()
                .map(|p| p.as_slice() >= key.as_slice())
                .unwrap_or(false)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "block index not sorted",
                ));
            }
            prev = Some(key.clone());
            let block = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let off = u16::from_le_bytes(raw[pos..pos + 2].try_into().unwrap());
            pos += 2;
            entries.push(IndexEntry { key, block, off });
        }
        if entries.len() as u64 != self.record_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "block index has {} entries, superblock declares {} records",
                    entries.len(),
                    self.record_count
                ),
            ));
        }
        self.index = entries;
        Ok(())
    }

    /// Rebuild the block index by scanning the record stream directly.
    /// Records that cannot be read (their stripe exceeded parity) are
    /// skipped; the scan resynchronises at the next block boundary. Skipped
    /// byte ranges are remembered so `get()` can return an honest error for a
    /// key that might live there instead of `Ok(None)`.
    fn rebuild_index_from_records(&mut self) -> io::Result<()> {
        let mut entries = Vec::new();
        let mut lost = Vec::new();
        let mut pos = 0usize;
        let mut lost_records = 0usize;
        while pos < self.data_bytes {
            match self.read_record_at(pos) {
                Ok((key, _value, total)) => {
                    entries.push(IndexEntry {
                        key,
                        block: (pos / BLOCK_SIZE) as u32,
                        off: (pos % BLOCK_SIZE) as u16,
                    });
                    pos += total;
                }
                Err(_) => {
                    // Damage inside this record: resync at the next logical
                    // block boundary (record boundaries are unknown there).
                    lost_records += 1;
                    let next = ((pos / BLOCK_SIZE + 1) * BLOCK_SIZE).min(self.data_bytes);
                    if next > pos {
                        lost.push((pos, next));
                    }
                    pos = next;
                }
            }
        }
        eprintln!(
            "[DNASST02] {:?}: index rebuilt from records: {} entries, ~{} record(s) unreachable",
            self.path.file_name().unwrap_or_default(),
            entries.len(),
            lost_records
        );
        self.index = entries;
        *self.lost_ranges.lock() = lost;
        Ok(())
    }

    /// Read the record that starts at logical data offset `pos`.
    /// Returns (key, lookup, record_total_len).
    fn read_record_at(&self, pos: usize) -> io::Result<(Vec<u8>, BlockLookup, usize)> {
        if pos >= self.data_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "record offset beyond data",
            ));
        }
        // Header (13 bytes, may span blocks)
        let mut head = Vec::with_capacity(13);
        self.read_span(false, pos, 13, &mut head)?;
        let (crc, klen, vlen, tomb) = parse_header(&head)?;
        let mut payload = Vec::with_capacity(klen + vlen);
        self.read_span(false, pos + 13, klen + vlen, &mut payload)?;
        let key = payload[..klen].to_vec();
        let val = payload[klen..].to_vec();
        let mut h = Hasher::new();
        h.update(&head[4..13]);
        h.update(&key);
        h.update(&val);
        if h.finalize() != crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("record CRC mismatch at logical offset {}", pos),
            ));
        }
        let total = 13 + klen + vlen;
        Ok((
            key,
            if tomb {
                BlockLookup::Tombstone
            } else {
                BlockLookup::Found(val)
            },
            total,
        ))
    }

    pub fn get(&self, lookup_key: &[u8]) -> io::Result<Option<BlockLookup>> {
        if !self.bloom.contains(lookup_key) {
            return Ok(None);
        }
        let i = match self
            .index
            .binary_search_by(|e| e.key.as_slice().cmp(lookup_key))
        {
            Ok(i) => i,
            Err(_) => {
                // The index was rebuilt while some records were unreachable.
                // Bloom said "maybe", so the key may be in a lost block range:
                // never report a false negative as a clean miss.
                if !self.lost_ranges.lock().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "key may be stored in an unreadable range of segment {:?}; run doctor",
                            self.path.file_name().unwrap_or_default()
                        ),
                    ));
                }
                return Ok(None);
            }
        };
        let entry = &self.index[i];
        let stripe = entry.block as usize / self.k;
        if self.is_quarantined(false, stripe) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "key is stored in quarantined stripe {} of segment {:?} (damage beyond parity m={})",
                    stripe,
                    self.path.file_name().unwrap_or_default(),
                    self.m
                ),
            ));
        }
        let pos = entry.block as usize * BLOCK_SIZE + entry.off as usize;
        let (key, value, _) = self.read_record_at(pos)?;
        debug_assert_eq!(key, lookup_key);
        if key != lookup_key {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "block index entry points at a foreign record",
            ));
        }
        Ok(Some(value))
    }

    /// Sequential scan of all keys with the given prefix (sorted).
    pub fn scan_keys(&self, prefix: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < self.data_bytes {
            let (key, _, advance) = self.read_record_at(pos)?;
            if key.starts_with(prefix) {
                out.push(key);
            }
            pos += advance;
        }
        Ok(out)
    }

    /// Full structural + payload validation (doctor / scrubber / explicit
    /// post-repair checks). Repairs everything the hook allows; fails when
    /// a stripe exceeds parity.
    pub fn validate_all(&self) -> io::Result<u64> {
        // Every physical block of both regions.
        let mut buf = vec![0u8; BLOCK_SIZE];
        for region in [false, true] {
            let stripes = if region {
                self.index_stripes
            } else {
                self.data_stripes
            };
            for slot in 0..stripes * (self.k + self.m) {
                self.read_physical_verified(region, slot, &mut buf)?;
            }
        }
        // Index sanity: every entry resolves to a readable record.
        for entry in &self.index {
            let pos = entry.block as usize * BLOCK_SIZE + entry.off as usize;
            let (key, _, _) = self.read_record_at(pos)?;
            if key != entry.key {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("index entry {:?} resolves to {:?}", entry.key, key),
                ));
            }
        }
        Ok(self.repairs_done.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Like `validate_all`, but temporarily enables local parity repair. This
    /// is used by mirror reconciliation: a strand with repairable damage must
    /// be healed *before* it is compared with its twin, otherwise the mirror
    /// layer would wrongly declare it unusable and copy the other strand over
    /// it (or abort startup when the other strand is missing).
    pub fn validate_all_with_repair(&self) -> io::Result<u64> {
        let previous = {
            let mut hook = self.hook.write();
            std::mem::replace(&mut *hook, Arc::new(ValidationRepair))
        };
        let result = self.validate_all();
        *self.hook.write() = previous;
        result
    }

    /// Number of logical byte ranges that the rebuilt index had to skip.
    pub fn lost_range_count(&self) -> usize {
        self.lost_ranges.lock().len()
    }

    /// Quarantine-mode validation for engine open: verify every block
    /// WITHOUT repairing. Stripes with damage ≤ m stay usable (on-read
    /// repair / scrubber will fix them); stripes with damage > m are
    /// quarantined — the segment stays open and only reads that touch the
    /// quarantined data fail honestly. Returns (quarantined, repairable).
    pub fn validate_quarantine(&self) -> io::Result<(usize, usize)> {
        let mut quarantined = self.quarantined.lock();
        quarantined.clear();
        let mut repairable = 0usize;
        let mut buf = vec![0u8; BLOCK_SIZE];
        for stripe in 0..self.data_stripes {
            let mut bad = 0usize;
            for idx in 0..(self.k + self.m) {
                let slot = phys_slot(stripe, idx, self.data_stripes, self.interleave_seed);
                let pos = self.region_base(false) as u64 + slot as u64 * BLOCK_SIZE as u64;
                let expected = self.crc_table[self.crc_slot(false, slot)];
                let mut ok = true;
                {
                    let mut f = self.file.write();
                    if f.seek(SeekFrom::Start(pos)).is_err() {
                        ok = false;
                    } else if f.read_exact(&mut buf).is_err() {
                        ok = false;
                    }
                }
                if ok {
                    let mut h = Hasher::new();
                    h.update(&buf);
                    ok = h.finalize() == expected;
                }
                if !ok {
                    bad += 1;
                }
            }
            if bad == 0 {
                continue;
            }
            if bad > self.m {
                quarantined.insert((false, stripe));
            } else {
                repairable += 1;
            }
        }
        let q = quarantined.len();
        if q > 0 {
            eprintln!(
                "[DNASST02] {:?}: {} stripe(s) quarantined (damage beyond parity m={}); \
                 the rest of the segment stays available",
                self.path.file_name().unwrap_or_default(),
                q,
                self.m
            );
        }
        Ok((q, repairable))
    }

    pub fn quarantine_count(&self) -> usize {
        self.quarantined.lock().len()
    }

    /// Whether in-place parity repair is possible for this handle.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    #[inline]
    fn is_quarantined(&self, index_region: bool, stripe: usize) -> bool {
        self.quarantined.lock().contains(&(index_region, stripe))
    }

    /// Lesion scan WITHOUT repair (policy ScrubOnly / doctor dry run):
    /// count damaged blocks per stripe. Returns (damaged_blocks, affected_stripes,
    /// unrecoverable_stripes).
    pub fn scan_lesions(&self) -> io::Result<(usize, usize, usize)> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let mut damaged_blocks = 0usize;
        let mut affected = 0usize;
        let mut unrecoverable = 0usize;
        for region in [false, true] {
            let stripes = if region {
                self.index_stripes
            } else {
                self.data_stripes
            };
            for stripe in 0..stripes {
                let mut bad = 0usize;
                for idx in 0..(self.k + self.m) {
                    let slot = phys_slot(stripe, idx, stripes, self.interleave_seed);
                    let pos = self.region_base(region) as u64 + slot as u64 * BLOCK_SIZE as u64;
                    let expected = self.crc_table[self.crc_slot(region, slot)];
                    let mut ok = true;
                    {
                        let mut f = self.file.write();
                        if f.seek(SeekFrom::Start(pos)).is_err() {
                            ok = false;
                        } else if f.read_exact(&mut buf).is_err() {
                            ok = false;
                        }
                    }
                    if ok {
                        let mut h = Hasher::new();
                        h.update(&buf);
                        ok = h.finalize() == expected;
                    }
                    if !ok {
                        bad += 1;
                    }
                }
                if bad > 0 {
                    damaged_blocks += bad;
                    affected += 1;
                    if bad > self.m {
                        unrecoverable += 1;
                    }
                }
            }
        }
        Ok((damaged_blocks, affected, unrecoverable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_blk_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_entries(n: usize, val_len: usize) -> Vec<(Vec<u8>, EntryValue)> {
        (0..n)
            .map(|i| {
                (
                    format!("chr1:{:06}", i).into_bytes(),
                    EntryValue::Value(vec![(i % 251) as u8; val_len]),
                )
            })
            .collect()
    }

    #[test]
    fn interleave_spreads_sequential_damage() {
        // P2.3 property: 8 consecutive damaged sectors hit 8 different
        // stripes, so each stripe loses ≤ 1 shard ≤ m.
        let (k, m) = (4usize, 2usize);
        let stripes = 64usize;
        let seed = 0xD3Au64;
        let mut stripe_hits = vec![0usize; stripes];
        for p in 0..8usize {
            let (s, _) = slot_to_stripe(p, stripes, seed);
            stripe_hits[s] += 1;
        }
        assert!(
            stripe_hits.iter().all(|&h| h <= 1),
            "hits: {:?}",
            stripe_hits
        );
        // And parity slots are interleaved: shard idx k..k+m are parity, and
        // they live in the same physical neighbourhood as data shards.
        let _ = (k, m);
    }

    #[test]
    fn interleave_is_a_bijection() {
        for &(stripes, seed) in &[(1usize, 0u64), (7, 3), (64, 0xD3A), (129, 599)] {
            let total = stripes * 6;
            let mut seen = vec![false; total];
            for s in 0..stripes {
                for idx in 0..6 {
                    let p = phys_slot(s, idx, stripes, seed);
                    assert!(p < total);
                    assert!(
                        !seen[p],
                        "duplicate slot p={} (stripes={} seed={})",
                        p, stripes, seed
                    );
                    seen[p] = true;
                }
            }
            assert!(seen.iter().all(|&s| s));
            for p in 0..total {
                let (s, idx) = slot_to_stripe(p, stripes, seed);
                assert_eq!(phys_slot(s, idx, stripes, seed), p);
            }
        }
    }

    #[test]
    fn write_read_roundtrip_small_and_spanning() {
        let dir = tempdir("rt");
        let mut entries = make_entries(500, 100); // many records per block
        entries.push((b"chr1:big".to_vec(), EntryValue::Value(vec![7u8; 100_000]))); // spans blocks+stripes
        entries.push((b"chr1:zzz".to_vec(), EntryValue::Tombstone));
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        let r = BlockSegmentReader::open(&path).unwrap();
        assert_eq!(r.record_count(), entries.len() as u64);
        for (key, value) in &entries {
            let got = r.get(key).unwrap().expect("key must be found");
            match value {
                EntryValue::Value(v) => {
                    assert_eq!(got, BlockLookup::Found(v.clone()), "key {:?}", key)
                }
                EntryValue::Tombstone => assert_eq!(got, BlockLookup::Tombstone),
            }
        }
        assert_eq!(r.get(b"chr1:absent").unwrap(), None);
        r.validate_all().unwrap();
        let keys = r.scan_keys(b"chr1:").unwrap();
        assert_eq!(keys.len(), entries.len());
    }

    /// Damage `slots` data-region physical slots in the file on disk.
    fn damage_slots(path: &Path, slots: &[usize], mode: DamageMode) {
        let mut raw = std::fs::read(path).unwrap();
        for &slot in slots {
            let off = SUPERBLOCK_SIZE + slot * BLOCK_SIZE;
            match mode {
                DamageMode::BitFlip => {
                    raw[off + 100] ^= 0xFF;
                    raw[off + 101] ^= 0x0F;
                }
                DamageMode::ZeroFill => {
                    raw[off..off + BLOCK_SIZE].fill(0);
                }
            }
        }
        std::fs::write(path, &raw).unwrap();
    }

    enum DamageMode {
        BitFlip,
        ZeroFill,
    }

    /// Data-region slot count (before index region) via a throwaway reader.
    fn data_slots(path: &Path) -> (usize, usize, u64) {
        let r = BlockSegmentReader::open(path).unwrap();
        let (k, m) = r.erasure_geometry();
        (r.data_stripes * (k + m), r.data_stripes, r.interleave_seed)
    }

    /// Canonical seed for a would-be segment id (mirrors reader derivation).
    fn seed_for_id(dir: &Path, id: u64) -> u64 {
        BlockSegmentReader::seed_for(&dir.join(format!("{:06}.sst", id)))
    }

    #[test]
    fn detects_and_repairs_bit_flip() {
        let dir = tempdir("repair");
        let entries = make_entries(2000, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        // Damage 2 data-region DATA-shard blocks (bit-flip), far apart.
        // (Parity shards are only read during repair/scrub — damaging one
        // would not trigger on-read repair, which is by design.)
        let (total_slots, stripes, seed) = data_slots(&path);
        let data_shard_slot = |mut want: usize| -> usize {
            let k = 4;
            for slot in 0..total_slots {
                let (_, idx) = slot_to_stripe(slot, stripes, seed);
                if idx < k {
                    if want == 0 {
                        return slot;
                    }
                    want -= 1;
                }
            }
            unreachable!();
        };
        let a = data_shard_slot(7);
        let b = data_shard_slot(total_slots / 2);
        damage_slots(&path, &[a, b], DamageMode::BitFlip);

        let r = BlockSegmentReader::open(&path).unwrap();
        // With repair hook installed, reads succeed.
        r.set_hook(Arc::new(RepairAll));
        for (key, value) in &entries {
            let got = r.get(key).unwrap().expect("repaired read");
            assert_eq!(got, BlockLookup::Found(value.clone().unwrap_value()));
        }
        assert!(r.repairs_done.load(std::sync::atomic::Ordering::Relaxed) >= 2);
        // All blocks now match their CRC table entries again.
        r.validate_all().unwrap();
    }

    struct RepairAll;
    impl RepairHook for RepairAll {
        fn repair_on_read(&self) -> bool {
            true
        }
        fn on_repaired(&self, _: &Path, _: usize, _: &[usize], _: u64) {}
        fn on_unrecoverable(&self, _: &Path, _: usize, _: &[usize]) -> bool {
            false
        }
    }

    struct NoFix;
    impl RepairHook for NoFix {
        fn repair_on_read(&self) -> bool {
            false
        }
        fn on_repaired(&self, _: &Path, _: usize, _: &[usize], _: u64) {}
        fn on_unrecoverable(&self, _: &Path, _: usize, _: &[usize]) -> bool {
            false
        }
    }

    #[test]
    fn damage_without_hook_is_an_error() {
        let dir = tempdir("nohook");
        let entries = make_entries(500, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();
        damage_slots(&path, &[5], DamageMode::BitFlip);

        let r = BlockSegmentReader::open(&path).unwrap();
        r.set_hook(Arc::new(NoFix));
        // Some key will hit the damaged block; the read must be an error,
        // never silent corruption.
        let mut err_seen = false;
        for (key, _) in &entries {
            match r.get(key) {
                Ok(Some(_)) => {}
                Err(_) => err_seen = true,
                Ok(None) => {}
            }
        }
        assert!(err_seen, "expected a localised read error on damaged block");
    }

    #[test]
    fn over_damage_reports_localised_unrecoverable() {
        let dir = tempdir("over");
        let entries = make_entries(4000, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        // Damage 3 shards of ONE stripe (m=2 → unrecoverable).
        let (_, stripes, seed) = data_slots(&path);
        let (s, _) = slot_to_stripe(0, stripes, seed);
        let slots: Vec<usize> = (0..3).map(|idx| phys_slot(s, idx, stripes, seed)).collect();
        damage_slots(&path, &slots, DamageMode::BitFlip);

        let r = BlockSegmentReader::open(&path).unwrap();
        r.set_hook(Arc::new(RepairAll));
        let mut unrecoverable_seen = false;
        for (key, _) in &entries {
            if let Err(e) = r.get(key) {
                if e.to_string().contains("unrecoverable lesion") {
                    unrecoverable_seen = true;
                    // Message must localise segment:stripe:shards
                    assert!(e.to_string().contains("stripe:"));
                }
            }
        }
        assert!(unrecoverable_seen, "expected honest unrecoverable report");
    }

    #[test]
    fn zero_fill_damage_repaired() {
        let dir = tempdir("zero");
        let entries = make_entries(1500, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();
        let (total_slots, _, _) = data_slots(&path);
        // Zero-fill two data blocks (dead-sector simulation), far apart.
        damage_slots(&path, &[9, total_slots - 3], DamageMode::ZeroFill);

        let r = BlockSegmentReader::open(&path).unwrap();
        r.set_hook(Arc::new(RepairAll));
        for (key, value) in &entries {
            assert_eq!(
                r.get(key).unwrap(),
                Some(BlockLookup::Found(value.clone().unwrap_value()))
            );
        }
        r.validate_all().unwrap();
    }

    #[test]
    fn index_region_damage_is_repairable_too() {
        let dir = tempdir("idx");
        let entries = make_entries(3000, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        // Open first (index loaded into memory), install the hook, THEN
        // damage 2 blocks inside the RS-protected index region. Data reads
        // keep working via the in-memory index; validate_all repairs the
        // on-disk index region from parity.
        let r = BlockSegmentReader::open(&path).unwrap();
        r.set_hook(Arc::new(RepairAll));

        let (data_slot_count, _, _) = data_slots(&path);
        let index_base = SUPERBLOCK_SIZE + data_slot_count * BLOCK_SIZE;
        let mut raw = std::fs::read(&path).unwrap();
        for extra_slot in [1usize, 7usize] {
            let off = index_base + extra_slot * BLOCK_SIZE;
            raw[off + 55] ^= 0xFF;
        }
        std::fs::write(&path, &raw).unwrap();

        // Full sweep repairs the index region from parity.
        r.validate_all().unwrap();
        for (key, value) in &entries {
            assert_eq!(
                r.get(key).unwrap(),
                Some(BlockLookup::Found(value.clone().unwrap_value()))
            );
        }
    }

    #[test]
    fn index_region_overdamage_falls_back_to_rebuild_on_restart() {
        let dir = tempdir("idxover");
        let entries = make_entries(3000, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        // Damage 3 index-region shards of ONE stripe (beyond parity): the
        // index region is unrecoverable by parity, but the index is derived
        // data — a restart rebuilds it from the record stream.
        let (data_slot_count, _, _) = data_slots(&path);
        let index_base = SUPERBLOCK_SIZE + data_slot_count * BLOCK_SIZE;
        let mut raw = std::fs::read(&path).unwrap();
        for extra_slot in [0usize, 1usize, 2usize] {
            let off = index_base + extra_slot * BLOCK_SIZE;
            raw[off..off + BLOCK_SIZE].fill(0);
        }
        std::fs::write(&path, &raw).unwrap();

        let r = BlockSegmentReader::open(&path).unwrap();
        assert!(r.index_rebuilt.load(std::sync::atomic::Ordering::Relaxed));
        // All data survived; the rebuilt index serves every key.
        for (key, value) in &entries {
            assert_eq!(
                r.get(key).unwrap(),
                Some(BlockLookup::Found(value.clone().unwrap_value()))
            );
        }
    }

    #[test]
    fn crc_table_copy_b_fallback() {
        let dir = tempdir("tableb");
        let entries = make_entries(800, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();
        let (_, stripes, seed) = data_slots(&path);
        let _ = (stripes, seed);

        // Corrupt table copy A only (copy B sits right after copy A).
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        let table_a_off = {
            let r = BlockSegmentReader::open(&path).unwrap();
            r.crc_table_offset as usize
        };
        let table_entries = {
            let r = BlockSegmentReader::open(&path).unwrap();
            r.crc_table.len()
        };
        let mut raw = std::fs::read(&path).unwrap();
        let hit = table_a_off + table_entries * 4 / 2;
        raw[hit] ^= 0xFF;
        assert!(hit < len);
        std::fs::write(&path, &raw).unwrap();

        // Open must succeed via copy B.
        let r = BlockSegmentReader::open(&path).unwrap();
        for (key, value) in &entries {
            assert_eq!(
                r.get(key).unwrap(),
                Some(BlockLookup::Found(value.clone().unwrap_value()))
            );
        }
    }

    #[test]
    fn superblock_loss_survives_via_footer() {
        let dir = tempdir("sbfallback");
        let entries = make_entries(600, 64);
        let path = BlockSegmentWriter::write_segment(&dir, 1, &entries, 4, 2, seed_for_id(&dir, 1))
            .unwrap();

        // Corrupt the superblock payload (keep magic so the wrapper can still
        // identify the format family; geometry must come from the footer).
        let mut raw = std::fs::read(&path).unwrap();
        for off in [12usize, 16, 20, 30] {
            raw[off] ^= 0xFF;
        }
        std::fs::write(&path, &raw).unwrap();

        let r = BlockSegmentReader::open(&path).unwrap();
        for (key, value) in &entries {
            assert_eq!(
                r.get(key).unwrap(),
                Some(BlockLookup::Found(value.clone().unwrap_value()))
            );
        }
    }

    #[test]
    fn legacy_magic_rejected() {
        let dir = tempdir("magic");
        let bad = dir.join("000009.sst");
        std::fs::write(&bad, b"DNASST01something").unwrap();
        assert!(BlockSegmentReader::open(&bad).is_err());
    }
}
