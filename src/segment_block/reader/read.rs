// SPDX-License-Identifier: AGPL-3.0-only
//! Logical reads: block lookup, prefix scans, index (re)building and the
//! full-file validation sweeps including quarantine accounting.
//! Split from the former reader.rs — no behaviour change.

use super::*;

impl BlockSegmentReader {
    // ---------------- logical reads --------------------------------------

    /// Read a logical block of a region through the verified path.
    pub(super) fn read_logical_block(
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
    pub(super) fn try_load_index(&mut self) -> io::Result<()> {
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
    pub(super) fn rebuild_index_from_records(&mut self) -> io::Result<()> {
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
    pub(super) fn is_quarantined(&self, index_region: bool, stripe: usize) -> bool {
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
