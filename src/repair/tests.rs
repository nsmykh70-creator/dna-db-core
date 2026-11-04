// SPDX-License-Identifier: AGPL-3.0-only
//! Repairosome test suite (split from the former repair.rs monolith).

use super::doctor::{is_legacy, scan_one};
use super::*;
use crate::memtable::EntryValue;
use crate::segment_block::BLOCK_SIZE;
use std::time::Duration;

fn tempdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dna_rep_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn entries(n: usize) -> Vec<(Vec<u8>, EntryValue)> {
    (0..n)
        .map(|i| {
            (
                format!("k:{:06}", i).into_bytes(),
                EntryValue::Value(vec![i as u8; 64]),
            )
        })
        .collect()
}

#[test]
fn journal_roundtrip() {
    let dir = tempdir("journal");
    let j = RepairJournal::open(&dir).unwrap();
    j.append(&RepairRecord {
        ts: 1,
        segment: "000001.sst".into(),
        stripe: 3,
        shards: vec![4, 9],
        region: "data".to_string(),
        action: "repaired".into(),
        bytes: 8192,
    });
    j.append(&RepairRecord {
        ts: 2,
        segment: "000001.sst".into(),
        stripe: 7,
        shards: vec![0, 1, 2],
        region: "index".to_string(),
        action: "unrecoverable".into(),
        bytes: 0,
    });
    let recs = j.read_all();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].action, "repaired");
    assert_eq!(recs[1].shards, vec![0, 1, 2]);
}

#[test]
fn policy_default_is_repair_on_read() {
    assert_eq!(RepairPolicy::default(), RepairPolicy::RepairOnRead);
    assert_eq!(ScrubConfig::default().interval, Duration::from_secs(86400));
    assert_eq!(
        ScrubConfig::default().rate_limit_bytes_per_sec,
        32 * 1024 * 1024
    );
}

#[test]
fn doctor_scan_and_fix_roundtrip() {
    let dir = tempdir("doctor");
    let path = crate::segment_block::BlockSegmentWriter::write_segment(
        &dir,
        1,
        &entries(2000),
        4,
        2,
        crate::segment_block::BlockSegmentReader::seed_for(&dir.join("000001.sst")),
    )
    .unwrap();

    // Damage 1 block (repairable).
    let mut raw = std::fs::read(&path).unwrap();
    let off = crate::segment_block::SUPERBLOCK_SIZE + 5 * BLOCK_SIZE + 9;
    raw[off] ^= 0xFF;
    std::fs::write(&path, &raw).unwrap();

    let report = doctor_scan(&dir).unwrap();
    assert_eq!(report.scanned_segments, 1);
    assert_eq!(report.repairable, 1);
    assert_eq!(report.verdicts[0].damaged_blocks, 1);

    let fixed = doctor_fix(&dir).unwrap();
    assert!(fixed.fixed_blocks >= 1);
    let after = doctor_scan(&dir).unwrap();
    assert_eq!(after.healthy, 1);
    assert_eq!(after.repairable, 0);
}

#[test]
fn doctor_migrates_legacy() {
    let dir = tempdir("migrate");
    // Write a legacy DNASST01 segment with the old writer.
    let path = crate::segment::SegmentWriter::write_segment(&dir, 5, &entries(300)).unwrap();
    assert!(is_legacy(&path).unwrap());

    let report = doctor_migrate(&dir, 4, 2).unwrap();
    assert_eq!(report.migrated, 1);
    assert!(!is_legacy(&path).unwrap());

    // Data survived migration.
    let r = crate::segment_block::BlockSegmentReader::open(&path).unwrap();
    assert_eq!(r.record_count(), 300);
    for (i, (key, val)) in entries(300).iter().enumerate() {
        let _ = i;
        assert_eq!(
            r.get(key).unwrap(),
            Some(crate::segment_block::BlockLookup::Found(
                val.clone().unwrap_value()
            ))
        );
    }
}

// ----- cross-strand cascade (A ↔B) -----

fn write_segment_pair(a: &Path, b: &Path) -> PathBuf {
    let path = crate::segment_block::BlockSegmentWriter::write_segment(
        a,
        1,
        &entries(1200),
        4,
        2,
        crate::segment_block::BlockSegmentReader::seed_for(&a.join("000001.sst")),
    )
    .unwrap();
    std::fs::copy(&path, b.join("000001.sst")).unwrap();
    path
}

fn damage_segment(path: &Path, block: usize, byte: usize) {
    let mut raw = std::fs::read(path).unwrap();
    let off = crate::segment_block::SUPERBLOCK_SIZE + block * BLOCK_SIZE + byte;
    raw[off] ^= 0xFF;
    std::fs::write(path, raw).unwrap();
}

fn write_closed_wal_pair(a: &Path, b: &Path) -> PathBuf {
    use crate::wal::{SyncMode, WalOp, WalWriter};
    let wal = a.join("wal_000001.wal");
    let mut w = WalWriter::open(&wal, SyncMode::Strict).unwrap();
    w.append(WalOp::Put, b"cascade:k1", Some(b"v1")).unwrap();
    w.append(WalOp::Put, b"cascade:k2", Some(b"v2")).unwrap();
    w.sync().unwrap();
    drop(w);
    std::fs::copy(&wal, b.join("wal_000001.wal")).unwrap();
    wal
}

#[test]
fn cascade_heals_segment_b_from_a() {
    let a = tempdir("casc_a2b");
    let b = tempdir("casc_b_from_a");
    let path = write_segment_pair(&a, &b);

    // Damage B (beyond a single spot: two blocks in one stripe).
    damage_segment(&b.join("000001.sst"), 4, 3);
    damage_segment(&b.join("000001.sst"), 4, 200);
    assert_ne!(
        scan_one(&b.join("000001.sst")).verdict,
        "HEALTHY",
        "B must start damaged"
    );

    // Dry run: reports, does not touch disk.
    let dry = doctor_cascade_strands(&a, &b, false).unwrap();
    assert_eq!(dry.segments_healed_b_from_a, 0);
    assert!(dry.would_heal.iter().any(|w| w.contains("000001.sst")));
    assert_ne!(scan_one(&b.join("000001.sst")).verdict, "HEALTHY");

    // Fix run: B healed from the verified A copy.
    let report = doctor_cascade_strands(&a, &b, true).unwrap();
    assert_eq!(report.segments_healed_b_from_a, 1);
    assert_eq!(report.segments_healed_a_from_b, 0);
    assert_eq!(scan_one(&b.join("000001.sst")).verdict, "HEALTHY");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(b.join("000001.sst")).unwrap(),
        "B must be byte-identical to A after the cascade"
    );
}

#[test]
fn cascade_heals_segment_a_from_b() {
    let a = tempdir("casc_a_from_b");
    let b = tempdir("casc_b2a");
    let path = write_segment_pair(&a, &b);

    // Damage A this time (parity is not consulted here — the mirror
    // cascade is the second line of defence after doctor_fix).
    damage_segment(&a.join("000001.sst"), 7, 11);
    damage_segment(&a.join("000001.sst"), 7, 333);
    assert_ne!(scan_one(&a.join("000001.sst")).verdict, "HEALTHY");

    let report = doctor_cascade_strands(&a, &b, true).unwrap();
    assert_eq!(report.segments_healed_a_from_b, 1);
    assert_eq!(report.segments_healed_b_from_a, 0);
    assert_eq!(scan_one(&a.join("000001.sst")).verdict, "HEALTHY");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(a.join("000001.sst")).unwrap()
    );
}

#[test]
fn cascade_heals_closed_wal_both_directions() {
    use crate::wal::WalReader;

    // B damaged -> healed from A.
    let a = tempdir("cwal_a2b");
    let b = tempdir("cwal_b_from_a");
    let wal = write_closed_wal_pair(&a, &b);
    let healthy = std::fs::read(&wal).unwrap();
    std::fs::write(b.join("wal_000001.wal"), b"garbage not a wal at all").unwrap();
    assert!(WalReader::recover(b.join("wal_000001.wal"))
        .unwrap()
        .is_empty());

    let report = doctor_cascade_strands(&a, &b, true).unwrap();
    assert_eq!(report.wal_healed_b_from_a, 1);
    assert_eq!(std::fs::read(b.join("wal_000001.wal")).unwrap(), healthy);
    let entries = WalReader::recover(&wal).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, b"cascade:k1");

    // A damaged -> healed from B.
    let a2 = tempdir("cwal_a_from_b");
    let b2 = tempdir("cwal_b2a");
    let wal2 = write_closed_wal_pair(&a2, &b2);
    std::fs::write(&wal2, vec![0xEEu8; 64]).unwrap();

    let report = doctor_cascade_strands(&a2, &b2, true).unwrap();
    assert_eq!(report.wal_healed_a_from_b, 1);
    let entries = WalReader::recover(&wal2).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].key, b"cascade:k2");
}

#[test]
fn cascade_refuses_to_guess_when_both_strands_are_damaged() {
    let a = tempdir("casc_both_a");
    let b = tempdir("casc_both_b");
    let wal = write_closed_wal_pair(&a, &b);
    std::fs::write(&wal, b"broken on A").unwrap();
    std::fs::write(b.join("wal_000001.wal"), b"broken on B too").unwrap();

    let report = doctor_cascade_strands(&a, &b, true).unwrap();
    assert_eq!(report.wal_healed_a_from_b, 0);
    assert_eq!(report.wal_healed_b_from_a, 0);
    assert_eq!(report.damaged_on_both, vec!["wal_000001.wal".to_string()]);
}
