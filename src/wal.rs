// SPDX-License-Identifier: AGPL-3.0-only
//! Crash-Resilient Write-Ahead Log (WAL)
//!
//! Durability contract (documented & tested):
//! 1. `SyncMode::Strict`  — fsync on EVERY append/batch: zero acknowledged-loss
//!    under SIGKILL / power failure.
//! 2. `SyncMode::Batched` — buffered, fsync every 64 KiB or on explicit sync():
//!    may lose up to the unflushed tail on crash (documented tradeoff).
//! 3. `SyncMode::Async`   — relies on OS page cache; for ephemeral workloads.
//!
//! Record formats (all integers little-endian):
//!   Single: [crc32 u32][op u8][klen u32][vlen u32][key][val]
//!   Batch : [crc32 u32][op=0x03 u8][count u32]{ [op u8][klen u32][vlen u32][key][val] } * count
//! Batch records make multi-key transactions atomic: the whole batch becomes
//! durable with a SINGLE fsync and is replayed atomically on recovery.
//!
//! Recovery scans records, verifies CRC32, stops at the first torn/corrupt
//! record and truncates the file to the last valid offset (auto-repair).

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use crc32fast::Hasher;

pub const OP_PUT: u8 = 1;
pub const OP_DELETE: u8 = 2;
pub const OP_BATCH: u8 = 3;

/// Single entry inside the WAL (or a batch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    Put,
    Delete,
}

impl WalOp {
    fn to_byte(&self) -> u8 {
        match self {
            WalOp::Put => OP_PUT,
            WalOp::Delete => OP_DELETE,
        }
    }
    fn from_byte(b: u8) -> Option<WalOp> {
        match b {
            OP_PUT => Some(WalOp::Put),
            OP_DELETE => Some(WalOp::Delete),
            _ => None,
        }
    }
}

/// One replay unit: a key operation recovered from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    pub op: WalOp,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Strict,
    Batched,
    Async,
}

/// Maximum accepted WAL record size (guard against corruption-driven OOM).
pub const MAX_WAL_RECORD: usize = 64 * 1024 * 1024;

pub struct WalWriter {
    file: File,
    writer: BufWriter<File>,
    path: PathBuf,
    sync_mode: SyncMode,
    pending_bytes: usize,
}

impl WalWriter {
    pub fn open(path: impl AsRef<Path>, sync_mode: SyncMode) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let writer = BufWriter::new(file.try_clone()?);
        Ok(Self {
            file,
            writer,
            path,
            sync_mode,
            pending_bytes: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a single operation with per-record CRC32.
    pub fn append(&mut self, op: WalOp, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        self.append_batch(&[(&op, key, value)])
    }

    /// Append a batch of operations as ONE atomic record (one CRC, one fsync).
    /// Either the whole batch becomes durable or none of it does.
    pub fn append_batch(&mut self, ops: &[(&WalOp, &[u8], Option<&[u8]>)]) -> io::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }

        let atomic = ops.len() == 1;
        let mut body: Vec<u8> = Vec::with_capacity(64);
        if atomic {
            let (op, key, value) = ops[0];
            body.push(op.to_byte());
            body.extend_from_slice(&(key.len() as u32).to_le_bytes());
            body.extend_from_slice(&(value.map(|v| v.len()).unwrap_or(0) as u32).to_le_bytes());
            body.extend_from_slice(key);
            if let Some(v) = value {
                body.extend_from_slice(v);
            }
        } else {
            body.push(OP_BATCH);
            body.extend_from_slice(&(ops.len() as u32).to_le_bytes());
            for (op, key, value) in ops {
                body.push(op.to_byte());
                body.extend_from_slice(&(key.len() as u32).to_le_bytes());
                body.extend_from_slice(&(value.map(|v| v.len()).unwrap_or(0) as u32).to_le_bytes());
                body.extend_from_slice(key);
                if let Some(v) = value {
                    body.extend_from_slice(v);
                }
            }
        }

        if body.len() + 4 > MAX_WAL_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL record exceeds MAX_WAL_RECORD",
            ));
        }

        let mut hasher = Hasher::new();
        hasher.update(&body);
        let crc = hasher.finalize();

        self.writer.write_all(&crc.to_le_bytes())?;
        self.writer.write_all(&body)?;
        self.pending_bytes += 4 + body.len();

        match self.sync_mode {
            SyncMode::Strict => {
                self.writer.flush()?;
                self.file.sync_data()?;
                self.pending_bytes = 0;
            }
            SyncMode::Batched if self.pending_bytes >= 64 * 1024 => {
                self.writer.flush()?;
                self.file.sync_data()?;
                self.pending_bytes = 0;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.file.sync_data()?;
        self.pending_bytes = 0;
        Ok(())
    }

    /// Truncate the log to zero bytes (called after a successful full flush).
    pub fn reset(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        // Truncate through a fresh write handle: the writer's own handle is
        // opened in append mode, which carries only FILE_APPEND_DATA on
        // Windows and cannot set_len (AccessDenied). O_APPEND writes always
        // target EOF, so no reposition of the append handles is needed.
        {
            let f = OpenOptions::new().write(true).open(&self.path)?;
            f.set_len(0)?;
            f.sync_all()?;
        }
        self.pending_bytes = 0;
        Ok(())
    }

    pub fn size_bytes(&self) -> io::Result<u64> {
        self.file.metadata().map(|m| m.len())
    }
}

pub struct WalReader;

/// ---------------------------------------------------------------------------
/// Closed-WAL parity ("RS-защита закрытых сегментов", P4.2)
///
/// When the engine rotates `active.wal` into `wal_<seq>.wal`, it freezes a
/// byte stream that will never be appended to again. That stream is split
/// into 4 KiB blocks and Reed–Solomon protected (k data + m parity blocks
/// per stripe, sequential stripe layout). The m parity blocks per stripe
/// live in a sidecar `<wal>.parity`; the WAL file itself is byte-identical
/// to what was mirrored, so frame-level recovery is untouched.
///
/// Sidecar layout (little-endian):
///   [0..8]   magic b"DNAWLP1"
///   [8]      k
///   [9]      m
///   [10..12] reserved
///   [12..20] blocks u64          (data blocks covered)
///   [20..24] table_crc u32       (CRC of the CRC table bytes)
///   [24..4096] zeros
///   [4096..]  CRC table (blocks * u32, per 4 KiB block of the WAL file)
///   then      parity blocks: ceil(blocks / k) stripes * m * 4096 bytes
///
/// The ACTIVE tail is intentionally NOT parity protected (strict fsync path
/// stays untouched); it is covered by frame CRCs + the Strand B mirror.
/// ---------------------------------------------------------------------------

pub const WAL_PARITY_MAGIC: &[u8; 8] = b"DNAWLP01";

pub struct WalParity;

impl WalParity {
    /// Freeze-protect a closed WAL: write the parity sidecar next to it.
    /// Streams the WAL block-by-block instead of loading it into memory, so a
    /// multi-gigabyte closed WAL cannot exhaust RAM during rotation.
    pub fn protect(wal_path: &Path, k: usize, m: usize) -> io::Result<()> {
        use crate::reed_solomon::ErasureCodec;
        let codec = ErasureCodec::new(k, m)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let block = 4096usize;
        let mut wal = File::open(wal_path)?;
        let len = wal.metadata()?.len() as usize;
        if len == 0 {
            return Ok(());
        }
        let blocks = len.div_ceil(block);
        let stripes = blocks.div_ceil(k);

        let read_block = |f: &mut File, b: usize| -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; block];
            f.seek(SeekFrom::Start((b * block) as u64))?;
            let avail = (len - b * block).min(block);
            f.read_exact(&mut buf[..avail])?;
            Ok(buf)
        };

        let mut crc_table = Vec::with_capacity(blocks);
        for b in 0..blocks {
            let data = read_block(&mut wal, b)?;
            let mut h = Hasher::new();
            h.update(&data);
            crc_table.push(h.finalize());
        }

        let mut sidecar = vec![0u8; block];
        sidecar[0..8].copy_from_slice(WAL_PARITY_MAGIC);
        sidecar[8] = k as u8;
        sidecar[9] = m as u8;
        sidecar[12..20].copy_from_slice(&(blocks as u64).to_le_bytes());
        let table_crc = {
            let mut h = Hasher::new();
            for c in &crc_table {
                h.update(&c.to_le_bytes());
            }
            h.finalize()
        };
        sidecar[20..24].copy_from_slice(&table_crc.to_le_bytes());
        // [24..28] is the parity-region CRC32 (0 for sidecars written before
        // this field existed; those are accepted but cannot verify parity).

        let parity_path = wal_path.with_extension("wal.parity");
        let tmp_path = wal_path.with_extension("wal.parity.tmp");
        {
            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            let mut w = BufWriter::new(f);
            w.write_all(&sidecar)?;
            for c in &crc_table {
                w.write_all(&c.to_le_bytes())?;
            }
            let mut parity_hasher = Hasher::new();
            for s in 0..stripes {
                let mut shards = Vec::with_capacity(k);
                for i in 0..k {
                    let b = s * k + i;
                    if b < blocks {
                        shards.push(read_block(&mut wal, b)?);
                    } else {
                        shards.push(vec![0u8; block]);
                    }
                }
                let parity = codec
                    .encode(&shards)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                for p in parity {
                    parity_hasher.update(&p);
                    w.write_all(&p)?;
                }
            }
            let parity_crc = parity_hasher.finalize();
            w.flush()?;
            let mut f = w.into_inner().map_err(|e| e.into_error())?;
            f.seek(SeekFrom::Start(24))?;
            f.write_all(&parity_crc.to_le_bytes())?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp_path, &parity_path)?;
        if let Some(dir) = wal_path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }

    /// Verify a closed WAL block-by-block against its parity sidecar and
    /// reconstruct any damaged blocks (≤ m per stripe). Returns the number
    /// of blocks repaired. No-op when no sidecar exists or nothing is wrong.
    /// Tails beyond the protected byte range are left alone (frame recovery
    /// owns them).
    pub fn repair(wal_path: &Path) -> io::Result<usize> {
        use crate::reed_solomon::ErasureCodec;
        let parity_path = wal_path.with_extension("wal.parity");
        if !parity_path.exists() {
            return Ok(0);
        }
        let raw = std::fs::read(&parity_path)?;
        if raw.len() < 4096 || &raw[0..8] != WAL_PARITY_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL parity sidecar is corrupt",
            ));
        }
        let k = raw[8] as usize;
        let m = raw[9] as usize;
        let blocks_u64 = u64::from_le_bytes(raw[12..20].try_into().unwrap());
        let blocks = usize::try_from(blocks_u64)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "WAL parity block count too large"))?;
        let table_crc = u32::from_le_bytes(raw[20..24].try_into().unwrap());
        let parity_crc = u32::from_le_bytes(raw[24..28].try_into().unwrap());
        if k == 0 || m == 0 || blocks == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad WAL parity geometry"));
        }
        let table_end = 4096usize
            .checked_add(
                blocks
                    .checked_mul(4)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL parity table overflow"))?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL parity table overflow"))?;
        if table_end > raw.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WAL parity CRC table truncated",
            ));
        }
        let table_raw = &raw[4096..table_end];
        let mut th = Hasher::new();
        th.update(table_raw);
        if th.finalize() != table_crc {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "WAL parity CRC table damaged"));
        }
        let crc_table: Vec<u32> = table_raw
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();

        let block = 4096usize;
        let stripes = blocks.div_ceil(k);
        let parity_bytes = stripes
            .checked_mul(m)
            .and_then(|n| n.checked_mul(block))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL parity size overflow"))?;
        let parity_end = table_end
            .checked_add(parity_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL parity size overflow"))?;
        if parity_end > raw.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "WAL parity region truncated"));
        }
        let parity_region = &raw[table_end..parity_end];
        // New sidecars carry a CRC over the parity region. A damaged parity
        // shard must never be used to "repair" data (that would invent bytes).
        if parity_crc != 0 {
            let mut ph = Hasher::new();
            ph.update(parity_region);
            if ph.finalize() != parity_crc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WAL parity region damaged (CRC mismatch)",
                ));
            }
        }

        let wal = OpenOptions::new().read(true).write(true).open(wal_path)?;
        let wal_len = wal.metadata()?.len() as usize;
        let protected_len = blocks
            .checked_mul(block)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL protected size overflow"))?;

        let codec = ErasureCodec::new(k, m)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let mut repaired = 0usize;
        let read_block = |wal: &File, b: usize| -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; block];
            let file_len = wal.metadata()?.len() as usize;
            if b * block >= file_len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "block beyond WAL"));
            }
            // The final block of a healthy WAL is usually partial (the file
            // ends mid-block). `protect` CRCs blocks zero-padded to the full
            // block size, so read the available prefix and leave the tail
            // zeroed — a read_exact across the EOF would falsely flag this
            // healthy tail as damaged and "restore" it every startup.
            let avail = (file_len - b * block).min(block);
            let mut f2 = wal.try_clone()?;
            f2.seek(SeekFrom::Start((b * block) as u64))?;
            f2.read_exact(&mut buf[..avail])?;
            Ok(buf)
        };

        for s in 0..stripes {
            // Gather the stripe: k data blocks from the WAL + m parity.
            let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
            let mut damaged_data: Vec<usize> = Vec::new();
            for i in 0..k {
                let b = s * k + i;
                if b >= blocks {
                    shards.push(Some(vec![0u8; block]));
                    continue;
                }
                let expected = crc_table[b];
                let got = read_block(&wal, b);
                let ok = match &got {
                    Ok(buf) => {
                        let mut h = Hasher::new();
                        h.update(buf);
                        h.finalize() == expected
                    }
                    Err(_) => false,
                };
                if ok && b * block < protected_len {
                    shards.push(Some(got.unwrap()));
                } else {
                    shards.push(None);
                    damaged_data.push(b);
                }
            }
            for pi in 0..m {
                let off = (s * m + pi) * block;
                shards.push(Some(parity_region[off..off + block].to_vec()));
            }
            if damaged_data.is_empty() {
                continue;
            }
            codec
                .reconstruct(&mut shards)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            // Parity can be stale/wrong even when its own region CRC is valid
            // (e.g. a sidecar copied from another generation). Only write a
            // reconstruction that matches the CRC table recorded at protect().
            for &b in &damaged_data {
                let shard = shards[b - s * k].as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "WAL reconstruction missing a shard")
                })?;
                let mut h = Hasher::new();
                h.update(shard);
                if h.finalize() != crc_table[b] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("WAL reconstruction failed CRC check for block {b}"),
                    ));
                }
            }
            // Rewrite damaged blocks (and extend the file if the tail was lost).
            {
                let mut f2 = wal.try_clone()?;
                for &b in &damaged_data {
                    let shard = shards[b - s * k].as_ref().unwrap();
                    f2.seek(SeekFrom::Start((b * block) as u64))?;
                    f2.write_all(shard)?;
                    repaired += 1;
                }
                if wal_len < protected_len {
                    f2.set_len(protected_len as u64)?;
                }
                f2.sync_data()?;
            }
        }
        Ok(repaired)
    }
}

impl WalReader {
    /// Recover all entries. Stops at the first torn/corrupt record and
    /// truncates the file to the last valid offset (auto-repair on crash).
    /// Every frame's CRC32 is verified before its entries are accepted.
    pub fn recover(path: impl AsRef<Path>) -> io::Result<Vec<WalEntry>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = OpenOptions::new().read(true).open(path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut valid_offset = 0u64;

        loop {
            // Frame CRC
            let mut crc_buf = [0u8; 4];
            match reader.read_exact(&mut crc_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let expected_crc = u32::from_le_bytes(crc_buf);

            // Op byte
            let mut op_buf = [0u8; 1];
            if reader.read_exact(&mut op_buf).is_err() {
                break;
            }
            let mut body = op_buf.to_vec();
            let mut frame_entries: Vec<WalEntry> = Vec::new();
            let mut valid = false;

            match op_buf[0] {
                OP_PUT | OP_DELETE => {
                    let mut lens = [0u8; 8];
                    if reader.read_exact(&mut lens).is_ok() {
                        body.extend_from_slice(&lens);
                        let klen = u32::from_le_bytes(lens[0..4].try_into().unwrap()) as usize;
                        let vlen = u32::from_le_bytes(lens[4..8].try_into().unwrap()) as usize;
                        if klen + vlen <= MAX_WAL_RECORD {
                            let mut kv = vec![0u8; klen + vlen];
                            if reader.read_exact(&mut kv).is_ok() {
                                body.extend_from_slice(&kv);
                                let key = kv[..klen].to_vec();
                                let value = if op_buf[0] == OP_PUT {
                                    Some(kv[klen..].to_vec())
                                } else {
                                    None
                                };
                                frame_entries.push(WalEntry {
                                    op: WalOp::from_byte(op_buf[0]).unwrap(),
                                    key,
                                    value,
                                });
                                valid = true;
                            }
                        }
                    }
                }
                OP_BATCH => {
                    let mut cnt_buf = [0u8; 4];
                    if reader.read_exact(&mut cnt_buf).is_ok() {
                        body.extend_from_slice(&cnt_buf);
                        let count = u32::from_le_bytes(cnt_buf) as usize;
                        let mut ok_all = count > 0 && count <= 1_000_000;
                        for _ in 0..count.min(1_000_000) {
                            if !ok_all {
                                break;
                            }
                            let mut hdr = [0u8; 9];
                            if reader.read_exact(&mut hdr).is_err() {
                                ok_all = false;
                                break;
                            }
                            body.extend_from_slice(&hdr);
                            let klen = u32::from_le_bytes(hdr[1..5].try_into().unwrap()) as usize;
                            let vlen = u32::from_le_bytes(hdr[5..9].try_into().unwrap()) as usize;
                            if klen + vlen > MAX_WAL_RECORD {
                                ok_all = false;
                                break;
                            }
                            let mut kv = vec![0u8; klen + vlen];
                            if reader.read_exact(&mut kv).is_err() {
                                ok_all = false;
                                break;
                            }
                            body.extend_from_slice(&kv);
                            let key = kv[..klen].to_vec();
                            let value = if hdr[0] == OP_PUT {
                                Some(kv[klen..].to_vec())
                            } else {
                                None
                            };
                            match WalOp::from_byte(hdr[0]) {
                                Some(op) => frame_entries.push(WalEntry { op, key, value }),
                                None => {
                                    ok_all = false;
                                    break;
                                }
                            }
                        }
                        valid = ok_all;
                    }
                }
                _ => {
                    valid = false;
                }
            }

            if !valid {
                break;
            }
            let mut hasher = Hasher::new();
            hasher.update(&body);
            if hasher.finalize() != expected_crc {
                eprintln!(
                    "[WAL] CRC mismatch at offset {}: torn/corrupt frame, truncating.",
                    valid_offset
                );
                break;
            }

            valid_offset += 4 + body.len() as u64;
            entries.extend(frame_entries);
        }

        // Truncate to last valid offset (auto-repair)
        let file = OpenOptions::new().write(true).open(path)?;
        let cur_len = file.metadata()?.len();
        if cur_len != valid_offset {
            eprintln!(
                "[WAL] Torn or corrupt tail detected: truncating {} -> {} bytes",
                cur_len, valid_offset
            );
            file.set_len(valid_offset)?;
            file.sync_all()?;
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_wal_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn single_roundtrip() {
        let dir = tempdir("single");
        let path = dir.join("active.wal");
        {
            let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
            wal.append(WalOp::Put, b"chr1:1", Some(b"ATGC")).unwrap();
            wal.append(WalOp::Delete, b"chr1:2", None).unwrap();
        }
        let entries = WalReader::recover(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].op, WalOp::Put);
        assert_eq!(entries[0].key, b"chr1:1");
        assert_eq!(entries[0].value.as_deref(), Some(b"ATGC".as_slice()));
        assert_eq!(entries[1].op, WalOp::Delete);
    }

    #[test]
    fn batch_atomic_roundtrip() {
        let dir = tempdir("batch");
        let path = dir.join("active.wal");
        {
            let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
            wal.append_batch(&[
                (&WalOp::Put, b"k1", Some(b"v1".as_slice())),
                (&WalOp::Put, b"k2", Some(b"v2".as_slice())),
                (&WalOp::Delete, b"k3", None),
            ])
            .unwrap();
        }
        let entries = WalReader::recover(&path).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].op, WalOp::Delete);
        assert_eq!(entries[0].value.as_deref(), Some(b"v1".as_slice()));
    }

    #[test]
    fn torn_write_truncated() {
        let dir = tempdir("torn");
        let path = dir.join("active.wal");
        {
            let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
            wal.append(WalOp::Put, b"good1", Some(b"v1")).unwrap();
            wal.append(WalOp::Put, b"good2", Some(b"v2")).unwrap();
        }
        // Simulate torn write: append garbage
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x99, 0x01]).unwrap();
        drop(f);

        let entries = WalReader::recover(&path).unwrap();
        assert_eq!(entries.len(), 2); // garbage ignored
        // File truncated to valid length
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 2 * (4 + 9 + 5 + 2) as u64);
    }

    #[test]
    fn truncated_mid_record() {
        let dir = tempdir("mid");
        let path = dir.join("active.wal");
        {
            let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
            wal.append(WalOp::Put, b"key_ok", Some(b"value_ok")).unwrap();
        }
        // Cut the file in half (torn write in the middle of record 2 that never completed)
        let raw = std::fs::read(&path).unwrap();
        std::fs::write(&path, &raw[..raw.len() / 2]).unwrap();

        let entries = WalReader::recover(&path).unwrap();
        // Half of one record: header may parse but lengths won't fit -> 0 or 1 entries,
        // and file must be truncated to a consistent state (reopen+recover again = same).
        let again = WalReader::recover(&path).unwrap();
        assert_eq!(entries.len(), again.len());
    }

    #[test]
    fn reset_empties_log() {
        let dir = tempdir("reset");
        let path = dir.join("active.wal");
        let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
        wal.append(WalOp::Put, b"k", Some(b"v")).unwrap();
        assert!(wal.size_bytes().unwrap() > 0);
        wal.reset().unwrap();
        assert_eq!(wal.size_bytes().unwrap(), 0);
        assert!(WalReader::recover(&path).unwrap().is_empty());
    }

    #[test]
    fn mixed_single_and_batch() {
        let dir = tempdir("mixed");
        let path = dir.join("active.wal");
        {
            let mut wal = WalWriter::open(&path, SyncMode::Strict).unwrap();
            wal.append(WalOp::Put, b"a", Some(b"1")).unwrap();
            wal.append_batch(&[
                (&WalOp::Put, b"b", Some(b"2".as_slice())),
                (&WalOp::Put, b"c", Some(b"3".as_slice())),
            ])
            .unwrap();
            wal.append(WalOp::Delete, b"a", None).unwrap();
        }
        let entries = WalReader::recover(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[3].op, WalOp::Delete);
        assert_eq!(entries[3].key, b"a");
    }
}
