// SPDX-License-Identifier: AGPL-3.0-only
//! Mirror test suite (split from the former mirror.rs monolith).

use super::*;
use crate::segment::SegmentReader;

fn tempdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dna_mirror_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn mirror_writes_and_resync() {
    let primary = tempdir("primary");
    let secondary = tempdir("secondary");
    let mirror = DualStrandMirror::new(&primary, &secondary).unwrap();

    mirror.mirror_write(WalOp::Put, b"k1", Some(b"v1")).unwrap();
    mirror.mirror_write(WalOp::Put, b"k2", Some(b"v2")).unwrap();
    mirror.sync().unwrap();
    assert_eq!(mirror.status(), StrandStatus::Healthy);

    // Seed the PRIMARY with real files (as an engine would have)
    {
        let mut pw =
            crate::wal::WalWriter::open(primary.join("active.wal"), SyncMode::Strict).unwrap();
        pw.append(WalOp::Put, b"alpha1", Some(b"value1")).unwrap();
        pw.append(WalOp::Put, b"alpha2", Some(b"value2")).unwrap();
        pw.append(WalOp::Put, b"alpha3", Some(b"value3")).unwrap();
        let segment_entries = vec![(
            b"alpha1".to_vec(),
            crate::memtable::EntryValue::Value(b"value1".to_vec()),
        )];
        crate::segment::SegmentWriter::write_segment(&primary, 1, &segment_entries).unwrap();
    }

    // Resync onto a fresh disk: copies WAL + segments from the primary
    let new_disk = tempdir("replacement");
    mirror.trigger_hot_swap_resync(&new_disk).unwrap();
    assert_eq!(mirror.status(), StrandStatus::Healthy);
    assert!(new_disk.join("active.wal").exists());
    assert!(new_disk.join("000001.sst").exists());
    assert_eq!(
        mirror.secondary_dir.read().as_path(),
        new_disk.as_path(),
        "hot-swap must switch the active Strand B directory"
    );

    // Recovered entries on the new disk (from the copied primary WAL)
    let entries = crate::wal::WalReader::recover(new_disk.join("active.wal")).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].key, b"alpha1");
    let segment = SegmentReader::open(new_disk.join("000001.sst")).unwrap();
    segment.validate_all().unwrap();
}

#[test]
fn corrupt_primary_segment_is_repaired_from_verified_backup() {
    let primary = tempdir("repair_primary");
    let secondary = tempdir("repair_secondary");
    let entries = vec![(
        b"chr1:42".to_vec(),
        crate::memtable::EntryValue::Value(b"ATGC".to_vec()),
    )];
    let primary_segment =
        crate::segment::SegmentWriter::write_segment(&primary, 1, &entries).unwrap();
    let mirror = DualStrandMirror::new(&primary, &secondary).unwrap();
    mirror.repair_segments().unwrap();

    let mut raw = std::fs::read(&primary_segment).unwrap();
    raw[25] ^= 0xff;
    std::fs::write(&primary_segment, raw).unwrap();
    mirror.repair_segments().unwrap();

    let reader = SegmentReader::open(&primary_segment).unwrap();
    reader.validate_all().unwrap();
    assert_eq!(
        reader.get(b"chr1:42").unwrap(),
        Some(crate::segment::SegmentLookup::Found(b"ATGC".to_vec()))
    );
}
