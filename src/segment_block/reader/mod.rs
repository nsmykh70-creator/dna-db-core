// SPDX-License-Identifier: AGPL-3.0-only
//! DNASST02 segment reader: `BlockSegmentReader` opens, verifies and reads
//! one segment — per-block CRC verification, Reed–Solomon stripe repair,
//! block-index lookup, prefix scans, quarantine and full validation sweeps.

use super::*;

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
    pub(super) interleave_seed: u64,
    pub(super) data_stripes: usize,
    index_stripes: usize,
    data_bytes: usize,
    index_bytes: usize,
    /// Byte offset of the CRC table region (superblock/backup geometry);
    /// exposed for repair tooling and diagnostics (doctor / test harnesses).
    pub crc_table_offset: u64,
    pub(super) crc_table: Vec<u32>,
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

mod read;
mod repair;

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

}
