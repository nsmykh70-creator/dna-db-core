// SPDX-License-Identifier: AGPL-3.0-only
//! SSTable Disk Segment Reader and Writer
//!
//! File layout (little-endian):
//!   [record]* [sparse index][index_offset u64][magic 8B]
//!   record  : [crc32 u32][klen u32][vlen u32][tombstone u8][key][val]
//!             crc over klen..val
//!   sparse  : [count u32]{ [klen u32][key][offset u64] }*  (every 16th record)
//!   magic   : b"DNASST01"
//!
//! v2.1.0:
//! - SegmentReader: bloom pre-pass + sparse index binary search + block scan
//! - Per-record CRC32 (detects bit-rot and torn flushes)
//! - Atomic segment creation via .tmp + rename, dir fsync afterwards

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use crc32fast::Hasher;

use crate::bloom::Bloom;
use crate::memtable::EntryValue;
use crate::segment_block::{BlockLookup, BlockSegmentReader, RepairHook, BLOCK_MAGIC, LEGACY_MAGIC};

pub const SEGMENT_MAGIC: &[u8; 8] = b"DNASST01";
const SPARSE_EVERY: usize = 16;
const MAX_SEGMENT_RECORD: usize = 64 * 1024 * 1024;
const MAX_SPARSE_INDEX_ENTRIES: usize = 10_000_000;

pub struct SegmentWriter;

impl SegmentWriter {
    pub fn write_segment(
        dir: &Path,
        segment_id: u64,
        entries: &[(Vec<u8>, EntryValue)],
    ) -> io::Result<PathBuf> {
        let tmp_path = dir.join(format!("{:06}.tmp", segment_id));
        let final_path = dir.join(format!("{:06}.sst", segment_id));

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        let mut writer = BufWriter::new(file);

        let mut bloom = Bloom::new(entries.len().max(1));
        let mut sparse_index: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut current_offset = 0u64;

        for (idx, (key, value)) in entries.iter().enumerate() {
            bloom.insert(key);

            if idx % SPARSE_EVERY == 0 {
                sparse_index.push((key.clone(), current_offset));
            }

            let is_tombstone = matches!(value, EntryValue::Tombstone);
            let val_bytes = match value {
                EntryValue::Value(v) => v.as_slice(),
                EntryValue::Tombstone => &[],
            };

            // crc over [klen][vlen][tomb][key][val]
            let mut hasher = Hasher::new();
            hasher.update(&(key.len() as u32).to_le_bytes());
            hasher.update(&(val_bytes.len() as u32).to_le_bytes());
            hasher.update(&[if is_tombstone { 1 } else { 0 }]);
            hasher.update(key);
            hasher.update(val_bytes);
            let crc = hasher.finalize();

            writer.write_all(&crc.to_le_bytes())?;
            writer.write_all(&(key.len() as u32).to_le_bytes())?;
            writer.write_all(&(val_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(&[if is_tombstone { 1 } else { 0 }])?;
            writer.write_all(key)?;
            writer.write_all(val_bytes)?;

            current_offset += 4 + 4 + 4 + 1 + key.len() as u64 + val_bytes.len() as u64;
        }

        // Sparse index
        let index_offset = current_offset;
        writer.write_all(&(sparse_index.len() as u32).to_le_bytes())?;
        for (k, off) in &sparse_index {
            writer.write_all(&(k.len() as u32).to_le_bytes())?;
            writer.write_all(k)?;
            writer.write_all(&off.to_le_bytes())?;
        }

        // Footer
        writer.write_all(&index_offset.to_le_bytes())?;
        writer.write_all(SEGMENT_MAGIC)?;
        writer.flush()?;

        let f = writer.into_inner()?;
        f.sync_data()?;

        // Bloom sidecar (written before the .sst rename; if the crash leaves
        // an .sst without its sidecar, the reader degrades to a permissive filter)
        let bloom_path = dir.join(format!("{:06}.bloom", segment_id));
        let tmp_bloom = dir.join(format!("{:06}.bloom.tmp", segment_id));
        fs::write(&tmp_bloom, bloom.to_bytes())?;
        fs::rename(&tmp_bloom, &bloom_path)?;

        fs::rename(&tmp_path, &final_path)?;

        // fsync directory so the rename is durable (POSIX). On Windows a
        // directory cannot be opened via File::open (AccessDenied) and NTFS
        // metadata is already journaled, so the step is skipped there.
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

/// Read-only view of one flushed segment.
pub struct SegmentReader {
    path: PathBuf,
    file: File,
    bloom: Bloom,
    sparse_index: Vec<(Vec<u8>, u64)>,
    data_end: u64,
    pub entry_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentLookup {
    /// Key found with this value.
    Found(Vec<u8>),
    /// Key found as tombstone (deleted).
    Tombstone,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_seg_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_entries() -> Vec<(Vec<u8>, EntryValue)> {
        let mut v = Vec::new();
        for i in 0..100u32 {
            v.push((
                format!("chr1:{:05}", i).into_bytes(),
                EntryValue::Value(format!("ATGC{}", i).into_bytes()),
            ));
        }
        v.push((b"chr1:zzz_del".to_vec(), EntryValue::Tombstone));
        v
    }

    #[test]
    fn write_then_read() {
        let dir = tempdir("wr");
        let entries = make_entries();
        let path = SegmentWriter::write_segment(&dir, 1, &entries).unwrap();
        assert!(path.exists());

        let reader = SegmentReader::open(&path).unwrap();
        for (key, value) in &entries {
            match value {
                EntryValue::Value(v) => {
                    assert_eq!(
                        reader.get(key).unwrap(),
                        Some(SegmentLookup::Found(v.clone())),
                        "key {:?}",
                        String::from_utf8_lossy(key)
                    );
                }
                EntryValue::Tombstone => {
                    assert_eq!(reader.get(key).unwrap(), Some(SegmentLookup::Tombstone));
                }
            }
        }
        // Missing key inside range
        assert_eq!(reader.get(b"chr1:050x").unwrap(), None);
        // Key beyond range
        assert_eq!(reader.get(b"zzzz").unwrap(), None);
        // Key before range
        assert_eq!(reader.get(b"aaa").unwrap(), None);
    }

    #[test]
    fn bad_magic_rejected() {
        let dir = tempdir("magic");
        let bad = dir.join("000001.sst");
        std::fs::write(&bad, b"garbagegarbagegarbage").unwrap();
        assert!(SegmentReader::open(&bad).is_err());
    }

    #[test]
    fn missing_bloom_degrades() {
        let dir = tempdir("nobloom");
        let entries = make_entries();
        let path = SegmentWriter::write_segment(&dir, 1, &entries).unwrap();
        let bloom_path = dir.join("000001.bloom");
        std::fs::remove_file(&bloom_path).unwrap();

        let reader = SegmentReader::open(&path).unwrap();
        assert_eq!(
            reader.get(b"chr1:00042").unwrap(),
            Some(SegmentLookup::Found(b"ATGC42".to_vec()))
        );
    }

    #[test]
    fn corrupted_record_detected() {
        let dir = tempdir("corrupt");
        let entries = make_entries();
        let path = SegmentWriter::write_segment(&dir, 1, &entries).unwrap();

        // Flip a byte inside the first record's value area
        let mut raw = std::fs::read(&path).unwrap();
        raw[25] ^= 0xFF;
        std::fs::write(&path, &raw).unwrap();

        let reader = SegmentReader::open(&path).unwrap();
        assert!(reader.get(b"chr1:00000").is_err());
    }

    #[test]
    fn lookup_scans_more_than_four_megabytes_within_sparse_block() {
        let dir = tempdir("long_probe");
        let entries: Vec<_> = (0..16)
            .map(|i| {
                (
                    format!("key-{i:02}").into_bytes(),
                    EntryValue::Value(vec![i as u8; 300_000]),
                )
            })
            .collect();
        let path = SegmentWriter::write_segment(&dir, 1, &entries).unwrap();
        let reader = SegmentReader::open(&path).unwrap();
        let result = reader.get(b"key-15").unwrap().unwrap();
        assert_eq!(
            result,
            SegmentLookup::Found(vec![15; 300_000]),
            "lookup must not silently stop after the old 4 MiB probe limit"
        );
        reader.validate_all().unwrap();
    }
}

impl SegmentReader {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;

        // Footer
        let len = file.metadata()?.len();
        if len < 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "segment too small"));
        }
        file.seek(SeekFrom::Start(len - 16))?;
        let mut footer = [0u8; 16];
        file.read_exact(&mut footer)?;
        if &footer[8..16] != SEGMENT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad segment magic",
            ));
        }
        let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let footer_start = len - 16;
        if index_offset > footer_start.saturating_sub(4) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse index offset is outside the segment",
            ));
        }

        // Sparse index
        file.seek(SeekFrom::Start(index_offset))?;
        let mut idx_reader = BufReader::new(file.try_clone()?);
        let mut count_buf = [0u8; 4];
        idx_reader.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;
        let index_bytes = footer_start - index_offset - 4;
        if count > MAX_SPARSE_INDEX_ENTRIES || (count as u64) > index_bytes / 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse index entry count exceeds the available index data",
            ));
        }
        let mut sparse_index = Vec::with_capacity(count);
        let mut index_pos = index_offset + 4;
        let mut previous_key: Option<Vec<u8>> = None;
        for _ in 0..count {
            let mut klen_buf = [0u8; 4];
            idx_reader.read_exact(&mut klen_buf)?;
            let klen = u32::from_le_bytes(klen_buf) as usize;
            index_pos += 4;
            if index_pos
                .checked_add(klen as u64)
                .and_then(|pos| pos.checked_add(8))
                .filter(|end| *end <= footer_start)
                .is_none()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sparse index key extends beyond the index region",
                ));
            }
            let mut key = vec![0u8; klen];
            idx_reader.read_exact(&mut key)?;
            let mut off_buf = [0u8; 8];
            idx_reader.read_exact(&mut off_buf)?;
            index_pos += klen as u64 + 8;
            let offset = u64::from_le_bytes(off_buf);
            if offset >= index_offset
                || previous_key.as_ref().map(|prev| prev >= &key).unwrap_or(false)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sparse index is not strictly sorted or contains an invalid offset",
                ));
            }
            previous_key = Some(key.clone());
            sparse_index.push((key, offset));
        }

        // Bloom filter is stored in a sidecar file <id>.bloom written atomically
        // together with the segment. If absent, degrade gracefully (scan anyway).
        let bloom_path = path.with_extension("bloom");
        let bloom = if bloom_path.exists() {
            let raw = fs::read(&bloom_path).map_err(|e| {
                io::Error::new(e.kind(), format!("bloom sidecar unreadable: {}", e))
            })?;
            Bloom::from_bytes(&raw)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        } else {
        // Permissive filter (all bits set) = always probe disk
        Bloom::permissive()
        };

        let entry_count = count * SPARSE_EVERY; // approximation, for stats only

        Ok(Self {
            path,
            file,
            bloom,
            sparse_index,
            data_end: index_offset,
            entry_count,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Full sequential scan of record keys (sorted, deduped, prefix-filtered).
    /// Reads only the key region [0, index_offset).
    pub fn scan_keys(&self, prefix: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let index_offset = self.data_end;
        if index_offset > 0 && self.sparse_index.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "segment has records but an empty sparse index",
            ));
        }
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(file);
        let mut out = Vec::new();
        let mut pos = 0u64;
        let mut previous_key: Option<Vec<u8>> = None;
        let mut record_index = 0usize;

        while pos < index_offset {
            let mut crc_buf = [0u8; 4];
            match reader.read_exact(&mut crc_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let mut hdr = [0u8; 9];
            match reader.read_exact(&mut hdr) {
                Ok(()) => {}
                Err(_) => break,
            }
            let klen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
            let vlen = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
            let payload_len = klen.checked_add(vlen).filter(|n| *n <= MAX_SEGMENT_RECORD);
            let rec_len = payload_len
                .and_then(|n| 13usize.checked_add(n))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid segment record size"))?;
            if (rec_len as u64) > index_offset - pos || hdr[8] > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment record exceeds the data region or has an invalid tombstone tag",
                ));
            }
            let mut rec_key = vec![0u8; klen];
            reader.read_exact(&mut rec_key)?;
            let mut val = vec![0u8; vlen];
            reader.read_exact(&mut val)?;

            let mut hasher = Hasher::new();
            hasher.update(&hdr);
            hasher.update(&rec_key);
            hasher.update(&val);
            if hasher.finalize() != u32::from_le_bytes(crc_buf) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("segment {:?} corrupted during scan at {}", self.path, pos),
                ));
            }

            if previous_key.as_ref().map(|prev| prev >= &rec_key).unwrap_or(false) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("segment {:?} keys are not strictly sorted", self.path),
                ));
            }
            if record_index % SPARSE_EVERY == 0 {
                let sparse = self.sparse_index.get(record_index / SPARSE_EVERY);
                if sparse.map(|(key, offset)| (key.as_slice(), *offset))
                    != Some((rec_key.as_slice(), pos))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sparse index does not match the record stream",
                    ));
                }
            }
            previous_key = Some(rec_key.clone());
            if rec_key.starts_with(prefix) {
                out.push(rec_key);
            }
            pos += rec_len as u64;
            record_index += 1;
        }
        if pos != index_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "segment data does not end at the sparse index",
            ));
        }
        if self.sparse_index.len() != record_index.div_ceil(SPARSE_EVERY) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse index entry count does not match the record stream",
            ));
        }
        Ok(out)
    }

    /// Validate every record and ensure the bloom sidecar cannot hide a key.
    pub fn validate_all(&self) -> io::Result<()> {
        for key in self.scan_keys(&[])? {
            if !self.bloom.contains(&key) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bloom filter has a false negative for key {:?}", key),
                ));
            }
        }
        Ok(())
    }

    /// Look up a key. Returns NotFound-style None if the bloom filter rules
    /// the segment out or the key is absent. Tombstones are distinguished.
    pub fn get(&self, lookup_key: &[u8]) -> io::Result<Option<SegmentLookup>> {
        if !self.bloom.contains(lookup_key) {
            return Ok(None);
        }
        if self.sparse_index.is_empty() {
            return Ok(None);
        }

        // Binary search the sparse index for the greatest entry <= key
        let pos = match self
            .sparse_index
            .binary_search_by(|(k, _)| k.as_slice().cmp(lookup_key))
        {
            Ok(i) => i,
            Err(0) => return Ok(None), // key < smallest indexed key
            Err(i) => i - 1,
        };
        let seek_off = self.sparse_index[pos].1;
        let range_end = self
            .sparse_index
            .get(pos + 1)
            .map(|(_, off)| *off)
            .unwrap_or(self.data_end);
        if seek_off >= range_end || range_end > self.data_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sparse index points outside the record region",
            ));
        }

        // Scan from seek offset; sorted order lets us stop once past the key.
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(seek_off))?;
        let mut reader = BufReader::new(file);
        let mut scanned = 0u64;
        let range_len = range_end - seek_off;

        loop {
            if scanned >= range_len {
                return Ok(None);
            }
            let mut crc_buf = [0u8; 4];
            match reader.read_exact(&mut crc_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated segment record checksum",
                    ))
                }
                Err(e) => return Err(e),
            }
            let expected_crc = u32::from_le_bytes(crc_buf);

            let mut hdr = [0u8; 9];
            match reader.read_exact(&mut hdr) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated segment record header",
                    ))
                }
                Err(e) => return Err(e),
            }
            let klen = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
            let vlen = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
            let payload_len = klen.checked_add(vlen).filter(|n| *n <= MAX_SEGMENT_RECORD);
            let rec_len = payload_len
                .and_then(|n| 13usize.checked_add(n))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid segment record size"))?;
            if hdr[8] > 1 || (rec_len as u64) > range_len - scanned {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "segment record exceeds its sparse-index range",
                ));
            }
            let tomb = hdr[8] == 1;

            let mut rec_key = vec![0u8; klen];
            reader.read_exact(&mut rec_key)?;
            let mut val = vec![0u8; vlen];
            reader.read_exact(&mut val)?;

            let mut hasher = Hasher::new();
            hasher.update(&hdr);
            hasher.update(&rec_key);
            hasher.update(&val);
            if hasher.finalize() != expected_crc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "segment {:?} corrupted record at ~{}",
                        self.path,
                        seek_off + scanned as u64
                    ),
                ));
            }

            scanned += rec_len as u64;

            match rec_key.as_slice().cmp(lookup_key) {
                std::cmp::Ordering::Equal => {
                    return Ok(Some(if tomb {
                        SegmentLookup::Tombstone
                    } else {
                        SegmentLookup::Found(val)
                    }));
                }
                std::cmp::Ordering::Greater => return Ok(None), // sorted: passed it
                std::cmp::Ordering::Less => continue,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SegmentFile — format-dispatching wrapper (DNASST01 legacy / DNASST02 block)
// ---------------------------------------------------------------------------

/// Unified read-only view over a segment file of either format.
/// The magic in the footer decides; the engine never needs to care.
pub enum SegmentFile {
    /// DNASST01 — v2.1 record layout (readable, not repairable by parity).
    Legacy(SegmentReader),
    /// DNASST02 — v3.0 block layout with RS stripes (self-healing).
    Block(BlockSegmentReader),
}

impl SegmentFile {
    /// Open with magic auto-detection.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let path = path.as_ref();
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        if len < 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "segment too small"));
        }
        f.seek(SeekFrom::Start(len - 8))?;
        let mut magic = [0u8; 8];
        f.read_exact(&mut magic)?;
        match &magic {
            BLOCK_MAGIC => Ok(SegmentFile::Block(BlockSegmentReader::open(path)?)),
            LEGACY_MAGIC => Ok(SegmentFile::Legacy(SegmentReader::open(path)?)),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown segment magic {:?}", String::from_utf8_lossy(other)),
            )),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            SegmentFile::Legacy(r) => r.path(),
            SegmentFile::Block(r) => r.path(),
        }
    }

    pub fn get(&self, key: &[u8]) -> io::Result<Option<SegmentLookup>> {
        match self {
            SegmentFile::Legacy(r) => r.get(key),
            SegmentFile::Block(r) => Ok(r.get(key)?.map(|l| match l {
                BlockLookup::Found(v) => SegmentLookup::Found(v),
                BlockLookup::Tombstone => SegmentLookup::Tombstone,
            })),
        }
    }

    pub fn scan_keys(&self, prefix: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            SegmentFile::Legacy(r) => r.scan_keys(prefix),
            SegmentFile::Block(r) => r.scan_keys(prefix),
        }
    }

    /// Full validation. Returns blocks repaired along the way (block format).
    pub fn validate_all(&self) -> io::Result<u64> {
        match self {
            SegmentFile::Legacy(r) => {
                r.validate_all()?;
                Ok(0)
            }
            SegmentFile::Block(r) => r.validate_all(),
        }
    }

    /// Record count (legacy: approximation via sparse index; block: exact).
    pub fn entry_count(&self) -> u64 {
        match self {
            SegmentFile::Legacy(r) => r.entry_count as u64,
            SegmentFile::Block(r) => r.record_count(),
        }
    }

    pub fn format_tag(&self) -> &'static str {
        match self {
            SegmentFile::Legacy(_) => "DNASST01",
            SegmentFile::Block(_) => "DNASST02",
        }
    }

    /// Install the repair hook (block format only; legacy ignores it).
    pub fn set_hook(&self, hook: std::sync::Arc<dyn RepairHook>) {
        if let SegmentFile::Block(r) = self {
            r.set_hook(hook);
        }
    }

    /// Access the block reader if this is a DNASST02 segment.
    pub fn as_block(&self) -> Option<&BlockSegmentReader> {
        match self {
            SegmentFile::Legacy(_) => None,
            SegmentFile::Block(r) => Some(r),
        }
    }

    /// Quarantined (unrecoverable) stripe count; 0 for legacy segments.
    pub fn quarantine_count(&self) -> usize {
        match self {
            SegmentFile::Legacy(_) => 0,
            SegmentFile::Block(r) => r.quarantine_count(),
        }
    }

    /// Open-time validation: block segments use quarantine mode (unfixable
    /// stripes stay localised, the rest of the segment serves); legacy
    /// segments keep the strict v2.1 behaviour.
    pub fn validate_for_open(&self) -> io::Result<(usize, usize)> {
        match self {
            SegmentFile::Legacy(r) => {
                r.validate_all()?;
                Ok((0, 0))
            }
            SegmentFile::Block(r) => r.validate_quarantine(),
        }
    }
}
