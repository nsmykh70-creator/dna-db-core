// SPDX-License-Identifier: AGPL-3.0-only
//! DNASST02 tests: interleave geometry, write/read round-trips, on-read
//! repair, quarantine, index rebuild and structural redundancy fallbacks.

use super::*;

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
