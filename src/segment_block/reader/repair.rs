// SPDX-License-Identifier: AGPL-3.0-only
//! Physical block reads with CRC verification and Reed–Solomon stripe
//! repair (the lesion pipeline). Split from the former reader.rs —
//! no behaviour change.

use super::*;

impl BlockSegmentReader {
    /// Read one physical block and verify its CRC. On mismatch, run the
    /// lesion/repair pipeline for the owning stripe, then retry.
    pub(super) fn read_physical_verified(
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
    pub(super) fn repair_stripe(&self, index_region: bool, stripe: usize) -> io::Result<Vec<usize>> {
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

    pub(super) fn unrecoverable_error(
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

}
