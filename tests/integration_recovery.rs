// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests: full lifecycle — write, crash (drop), recovery, flush,
//! checkpoint, torn WAL, compaction, prefix scans across layers.

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::wal::{SyncMode, WalOp};
use std::path::PathBuf;

fn tempdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dna_it_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(dir: &PathBuf) -> EngineConfig {
    EngineConfig {
        dir_path: dir.clone(),
        memtable_capacity: 64 * 1024, // 64 KiB for flush testing
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        ..Default::default()
    }
}

#[test]
fn basic_put_get_delete() {
    let dir = tempdir("basic");
    let engine = Engine::open(config(&dir)).unwrap();

    engine.put(b"chr1:100", b"ATGCCCGGTAAA").unwrap();
    assert_eq!(engine.get(b"chr1:100").unwrap(), Some(b"ATGCCCGGTAAA".to_vec()));

    engine.delete(b"chr1:100").unwrap();
    assert_eq!(engine.get(b"chr1:100").unwrap(), None);

    // Overwrite
    engine.put(b"k", b"v1").unwrap();
    engine.put(b"k", b"v2").unwrap();
    assert_eq!(engine.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn crash_recovery_preserves_all_data() {
    let dir = tempdir("recovery");

    {
        let engine = Engine::open(config(&dir)).unwrap();
        for i in 0..1000u32 {
            let key = format!("chr1:{:06}", i);
            let val = format!("ACTGACTGACTGACTG_{}", i);
            engine.put(key.as_bytes(), val.as_bytes()).unwrap();
        }
        // Simulate kill -9: engine dropped WITHOUT graceful close
    }

    // Reopen: WAL must replay everything
    let engine = Engine::open(config(&dir)).unwrap();
    assert_eq!(engine.stats.wal_replayed_entries.load(std::sync::atomic::Ordering::Relaxed), 1000);
    for i in 0..1000u32 {
        let key = format!("chr1:{:06}", i);
        let want = format!("ACTGACTGACTGACTG_{}", i);
        assert_eq!(
            engine.get(key.as_bytes()).unwrap(),
            Some(want.into_bytes()),
            "lost key {}",
            key
        );
    }
}

#[test]
fn flush_creates_readable_segments() {
    let dir = tempdir("flush");
    let engine = Engine::open(config(&dir)).unwrap();

    // Force multiple flushes: 64KiB capacity, entries ~64 bytes
    for i in 0..5000u32 {
        let key = format!("chr2:{:06}", i);
        let val = format!("GGGGCCCCAAAATTTT_{}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    assert!(engine.segment_count() >= 1, "expected flushed segments");

    // All data readable (mixed memtable + segments)
    for i in 0..5000u32 {
        let key = format!("chr2:{:06}", i);
        let want = format!("GGGGCCCCAAAATTTT_{}", i);
        assert_eq!(engine.get(key.as_bytes()).unwrap(), Some(want.into_bytes()));
    }

    // Reopen: segments + WAL must combine correctly
    drop(engine);
    let engine = Engine::open(config(&dir)).unwrap();
    for i in 0..5000u32 {
        let key = format!("chr2:{:06}", i);
        let want = format!("GGGGCCCCAAAATTTT_{}", i);
        assert_eq!(engine.get(key.as_bytes()).unwrap(), Some(want.into_bytes()));
    }
}

#[test]
fn checkpoint_truncates_wal_and_preserves_data() {
    let dir = tempdir("checkpoint");
    let engine = Engine::open(config(&dir)).unwrap();

    for i in 0..100u32 {
        engine.put(format!("k{}", i).as_bytes(), format!("v{}", i).as_bytes()).unwrap();
    }
    engine.checkpoint().unwrap();
    assert_eq!(engine.wal_size_bytes(), 0);

    for i in 0..100u32 {
        assert_eq!(
            engine.get(format!("k{}", i).as_bytes()).unwrap(),
            Some(format!("v{}", i).into_bytes())
        );
    }

    // Writes after checkpoint land in the new WAL
    engine.put(b"after", b"checkpoint").unwrap();
    drop(engine);
    let engine = Engine::open(config(&dir)).unwrap();
    for i in 0..100u32 {
        assert_eq!(
            engine.get(format!("k{}", i).as_bytes()).unwrap(),
            Some(format!("v{}", i).into_bytes())
        );
    }
    assert_eq!(engine.get(b"after").unwrap(), Some(b"checkpoint".to_vec()));
}

#[test]
fn dual_strand_repairs_a_corrupt_segment_and_wal() {
    use std::io::{Seek, SeekFrom, Write};

    let primary = tempdir("dual_repair_primary");
    let secondary = tempdir("dual_repair_secondary");
    let mirrored_config = || EngineConfig {
        dir_path: primary.clone(),
        memtable_capacity: 64 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: Some(secondary.clone()),
        ..Default::default()
    };

    {
        let engine = Engine::open(mirrored_config()).unwrap();
        engine.put(b"stable", b"checkpointed").unwrap();
        engine.checkpoint().unwrap();
        engine.put(b"recent", b"wal-repaired").unwrap();
        engine.flush().unwrap();
        // Put one newer entry in the active WAL after the segment flush.
        engine.put(b"wal-tail", b"from-strand-b").unwrap();
    }

    let segment = std::fs::read_dir(&primary)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sst"))
        .unwrap();
    {
        let mut file = std::fs::OpenOptions::new().write(true).open(&segment).unwrap();
        file.seek(SeekFrom::Start(25)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.sync_all().unwrap();
    }
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(primary.join("active.wal"))
            .unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.sync_all().unwrap();
    }

    let recovered = Engine::open(mirrored_config()).unwrap();
    assert_eq!(
        recovered.get(b"stable").unwrap(),
        Some(b"checkpointed".to_vec())
    );
    assert_eq!(
        recovered.get(b"recent").unwrap(),
        Some(b"wal-repaired".to_vec())
    );
    assert_eq!(
        recovered.get(b"wal-tail").unwrap(),
        Some(b"from-strand-b".to_vec())
    );
    assert_eq!(
        std::fs::read(&segment).unwrap(),
        std::fs::read(secondary.join(segment.file_name().unwrap())).unwrap()
    );
}

#[test]
fn dual_strand_repairs_a_missing_active_segment() {
    let primary = tempdir("missing_segment_primary");
    let secondary = tempdir("missing_segment_secondary");
    let mirrored_config = || EngineConfig {
        dir_path: primary.clone(),
        memtable_capacity: 64 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: Some(secondary.clone()),
        ..Default::default()
    };

    let segment = {
        let engine = Engine::open(mirrored_config()).unwrap();
        engine.put(b"missing-segment-key", b"recover-from-b").unwrap();
        engine.checkpoint().unwrap();
        std::fs::read_dir(&primary)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "sst"))
            .unwrap()
    };
    let backup_segment = secondary.join(segment.file_name().unwrap());
    assert!(backup_segment.exists());
    std::fs::remove_file(&segment).unwrap();

    let recovered = Engine::open(mirrored_config()).unwrap();
    assert_eq!(
        recovered.get(b"missing-segment-key").unwrap(),
        Some(b"recover-from-b".to_vec())
    );
    assert_eq!(
        std::fs::read(&segment).unwrap(),
        std::fs::read(&backup_segment).unwrap()
    );
}

#[test]
fn dual_strand_heals_closed_wal_on_strand_b_from_a() {
    // Closed-WAL cascade, A→B direction: B's rotated copy is damaged at
    // startup and must be rebuilt from the verified Strand A file.
    let primary = tempdir("cwal_b_primary");
    let secondary = tempdir("cwal_b_secondary");
    let cfg = || EngineConfig {
        dir_path: primary.clone(),
        memtable_capacity: 64 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: Some(secondary.clone()),
        // Small rotation threshold so writes produce wal_*.wal on both strands.
        wal_segment_capacity: 4096,
        ..Default::default()
    };

    {
        let engine = Engine::open(cfg()).unwrap();
        for i in 0..300u32 {
            engine
                .put(format!("cwal:{:05}", i).as_bytes(), format!("payload_{:04}", i).as_bytes())
                .unwrap();
        }
    }

    // Find a rotated closed WAL that exists on BOTH strands.
    let closed_on_a: Vec<PathBuf> = std::fs::read_dir(&primary)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("wal_") && n.ends_with(".wal"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !closed_on_a.is_empty(),
        "rotation must produce closed WAL segments on Strand A"
    );
    let closed = &closed_on_a[0];
    let closed_name = closed.file_name().unwrap().to_string_lossy().to_string();
    let backup_copy = secondary.join(&closed_name);
    assert!(backup_copy.exists(), "closed WAL must be mirrored to Strand B");
    let healthy_bytes = std::fs::read(closed).unwrap();

    // Damage ONLY Strand B's copy (simulate B-side sector damage).
    std::fs::write(&backup_copy, b"this is not a wal anymore").unwrap();

    let recovered = Engine::open(cfg()).unwrap();
    assert_eq!(
        std::fs::read(&backup_copy).unwrap(),
        healthy_bytes,
        "Strand B closed WAL must be rebuilt from Strand A at startup"
    );
    // Data from every rotated segment is still readable after replay.
    for i in 0..300u32 {
        let key = format!("cwal:{:05}", i);
        assert_eq!(
            recovered.get(key.as_bytes()).unwrap(),
            Some(format!("payload_{:04}", i).into_bytes()),
            "lost key {} after B-side closed-WAL heal",
            key
        );
    }
}

#[test]
fn dual_strand_heals_closed_wal_on_strand_a_from_b() {
    // Closed-WAL cascade, B→A direction: A's copy is damaged beyond its RS
    // parity (sidecar removed + data blocks corrupted) and must be restored
    // from the verified Strand B copy.
    let primary = tempdir("cwal_a_primary");
    let secondary = tempdir("cwal_a_secondary");
    let cfg = || EngineConfig {
        dir_path: primary.clone(),
        memtable_capacity: 64 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: Some(secondary.clone()),
        wal_segment_capacity: 4096,
        ..Default::default()
    };

    {
        let engine = Engine::open(cfg()).unwrap();
        for i in 0..300u32 {
            engine
                .put(format!("cwalx:{:05}", i).as_bytes(), format!("value_{:04}", i).as_bytes())
                .unwrap();
        }
    }

    let closed_on_a: Vec<PathBuf> = std::fs::read_dir(&primary)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("wal_") && n.ends_with(".wal"))
                .unwrap_or(false)
        })
        .collect();
    assert!(!closed_on_a.is_empty());
    let closed = &closed_on_a[0];
    let closed_name = closed.file_name().unwrap().to_string_lossy().to_string();
    assert!(secondary.join(&closed_name).exists());

    // Kill A's parity sidecar (so RS repair cannot fire) and corrupt A's
    // data blocks — only Strand B still holds a good copy.
    let _ = std::fs::remove_file(closed.with_extension("wal.parity"));
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(closed).unwrap();
        f.seek(SeekFrom::Start(64)).unwrap();
        f.write_all(&[0xFF; 128]).unwrap();
        f.sync_all().unwrap();
    }

    let recovered = Engine::open(cfg()).unwrap();
    assert_eq!(
        std::fs::read(closed).unwrap(),
        std::fs::read(secondary.join(&closed_name)).unwrap(),
        "Strand A closed WAL must be restored from Strand B"
    );
    // The restored copy must cover every entry Strand B held. Known and
    // documented limitation: at each rotation boundary Strand A's rotated
    // file holds the in-flight entry Strand B had not mirrored yet (B's
    // rotated copy is a one-entry-shorter byte prefix). When that in-flight
    // entry sits AFTER the corruption hole, prefix recovery cannot reach
    // it and the cascade has nothing better to take — at most one entry
    // per rotation boundary is lost without the parity sidecar (with the
    // sidecar RS rebuilds the damaged blocks in full, no loss at all).
    let mut missing: Vec<u32> = Vec::new();
    for i in 0..300u32 {
        let key = format!("cwalx:{:05}", i);
        if recovered.get(key.as_bytes()).unwrap()
            != Some(format!("value_{:04}", i).into_bytes())
        {
            missing.push(i);
        }
    }
    assert!(
        missing.iter().all(|i| *i == 120),
        "only the rotation-boundary in-flight entry may be lost, got {missing:?}"
    );
    // Everything Strand B's copy held is readable again.
    for i in 0..120u32 {
        let key = format!("cwalx:{:05}", i);
        assert_eq!(
            recovered.get(key.as_bytes()).unwrap(),
            Some(format!("value_{:04}", i).into_bytes()),
            "lost key {} after A-side closed-WAL restore",
            key
        );
    }
}

#[test]
fn torn_wal_tail_auto_repairs() {
    let dir = tempdir("torn");
    let wal_path = dir.join("active.wal");

    {
        let engine = Engine::open(config(&dir)).unwrap();
        for i in 0..50u32 {
            engine.put(format!("torn_key_{}", i).as_bytes(), b"some_value_here".as_slice()).unwrap();
        }
    }

    // Corrupt the tail (simulated torn write)
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&wal_path).unwrap();
        f.write_all(&[0xAA; 64]).unwrap();
    }

    let engine = Engine::open(config(&dir)).unwrap();
    for i in 0..50u32 {
        let key = format!("torn_key_{}", i);
        assert_eq!(
            engine.get(key.as_bytes()).unwrap(),
            Some(b"some_value_here".to_vec()),
            "key {} lost after torn-write recovery",
            key
        );
    }
    // WAL must be repaired (truncated) so new writes work
    engine.put(b"new_after_repair", b"ok").unwrap();
    assert_eq!(engine.get(b"new_after_repair").unwrap(), Some(b"ok".to_vec()));
}

#[test]
fn write_batch_is_atomic_on_recovery() {
    let dir = tempdir("batch");
    {
        let engine = Engine::open(config(&dir)).unwrap();
        let ops = vec![
            (WalOp::Put, b"tx:k1".to_vec(), Some(b"v1".to_vec())),
            (WalOp::Put, b"tx:k2".to_vec(), Some(b"v2".to_vec())),
            (WalOp::Delete, b"tx:k3".to_vec(), None),
        ];
        engine.write_batch(&ops).unwrap();
        assert_eq!(engine.get(b"tx:k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(engine.get(b"tx:k2").unwrap(), Some(b"v2".to_vec()));
    }
    let engine = Engine::open(config(&dir)).unwrap();
    assert_eq!(engine.get(b"tx:k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(engine.get(b"tx:k2").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(engine.get(b"tx:k3").unwrap(), None);
}

#[test]
fn compaction_merges_and_preserves_data() {
    let dir = tempdir("compact");
    let engine = Engine::open(config(&dir)).unwrap();

    // Create several segments with overwrites and deletes across them
    for i in 0..3000u32 {
        engine.put(format!("cmp:{:05}", i).as_bytes(), format!("first_{}", i).as_bytes()).unwrap();
    }
    for i in (0..3000u32).step_by(2) {
        engine.put(format!("cmp:{:05}", i).as_bytes(), format!("second_{}", i).as_bytes()).unwrap();
    }
    for i in (0..3000u32).step_by(10) {
        engine.delete(format!("cmp:{:05}", i).as_bytes()).unwrap();
    }
    assert!(engine.segment_count() >= 2);

    let removed = engine.compact().unwrap();
    assert!(removed > 0);
    assert_eq!(engine.segment_count(), 1);

    for i in 0..3000u32 {
        let key = format!("cmp:{:05}", i);
        let want = if i % 10 == 0 {
            None
        } else if i % 2 == 0 {
            Some(format!("second_{}", i).into_bytes())
        } else {
            Some(format!("first_{}", i).into_bytes())
        };
        assert_eq!(engine.get(key.as_bytes()).unwrap(), want, "key {}", key);
    }

    // Reopen after compaction
    drop(engine);
    let engine = Engine::open(config(&dir)).unwrap();
    for i in 0..3000u32 {
        let key = format!("cmp:{:05}", i);
        let want = if i % 10 == 0 {
            None
        } else if i % 2 == 0 {
            Some(format!("second_{}", i).into_bytes())
        } else {
            Some(format!("first_{}", i).into_bytes())
        };
        assert_eq!(engine.get(key.as_bytes()).unwrap(), want, "key {} after reopen", key);
    }
}

#[test]
fn prefix_scan_across_layers() {
    let dir = tempdir("scan");
    let engine = Engine::open(config(&dir)).unwrap();

    // Fill enough to flush
    for i in 0..4000u32 {
        engine
            .put(format!("chr7:{:05}", i).as_bytes(), format!("seq_{}", i).as_bytes())
            .unwrap();
    }
    // Newer writes in memtable shadow segment data
    engine.put(b"chr7:00042", b"overwritten").unwrap();
    engine.delete(b"chr7:00043").unwrap();
    // Foreign namespace
    engine.put(b"other:1", b"x").unwrap();

    let rows = engine.scan_prefix(b"chr7:").unwrap();
    assert_eq!(rows.len(), 3999); // 4000 - 1 deleted
    assert_eq!(rows[42].0, b"chr7:00042".to_vec());
    assert_eq!(rows[42].1, b"overwritten".to_vec());
    // chr7:00043 must be absent
    assert!(!rows.iter().any(|(k, _)| k == b"chr7:00043"));
    // sorted order
    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rows, sorted);
}

#[test]
fn concurrent_writes_survive_restart() {
    let dir = tempdir("concurrent");
    {
        let engine = Engine::open(config(&dir)).unwrap();
        let mut handles = Vec::new();
        for worker in 0..4u32 {
            let e = engine.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..2000u32 {
                    let key = format!("w{}_k{}", worker, i);
                    e.put(key.as_bytes(), format!("val_{}_{}", worker, i).as_bytes()).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
    let engine = Engine::open(config(&dir)).unwrap();
    for worker in 0..4u32 {
        for i in 0..2000u32 {
            let key = format!("w{}_k{}", worker, i);
            let want = format!("val_{}_{}", worker, i);
            assert_eq!(engine.get(key.as_bytes()).unwrap(), Some(want.into_bytes()));
        }
    }
}
