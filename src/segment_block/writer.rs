// SPDX-License-Identifier: AGPL-3.0-only
//! DNASST02 segment writer: `BlockSegmentWriter::write_segment` packs
//! records, the block index, Reed–Solomon stripes, the CRC table,
//! superblock and footer, and writes the file atomically (tmp + rename).
//! Also the `CrcAppend` helper used to build the CRC table.

use super::*;

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
