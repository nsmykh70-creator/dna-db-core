// SPDX-License-Identifier: AGPL-3.0-only
//! DNASST02 — Block-based, erasure-coded segment format ("chromatin").
//!
//! A segment is a sequence of 4 KiB blocks organised into Reed–Solomon
//! stripes of `k` data + `m` parity shards. Every block is covered by a
//! CRC-32 (IEEE) stored in a table at the end of the file, so any 4 KiB sector
//! damage (bit-rot, dead sector, torn write) is *detected* per block and
//! *repairable* from parity as long as ≤ m blocks of its stripe survive.
//!
//! File layout (little-endian, all offsets absolute):
//!
//! ```text
//! [superblock   4096 B]  magic, geometry, counters, self-CRC
//! [data region]          S_data  * (k+m) blocks (records, packed)
//! [index region]         S_index * (k+m) blocks (block index, packed)
//! [crc table]            (S_data + S_index) * (k+m) * u32  (CRC-32 per block)
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

mod reader;
mod writer;
#[cfg(test)]
mod tests;

pub use reader::{BlockLookup, BlockSegmentReader};
pub use writer::BlockSegmentWriter;
