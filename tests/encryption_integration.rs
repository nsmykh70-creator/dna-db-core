// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end checks for payload encryption at rest.

use dna_db_core::crypto::Crypto;
use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::wal::SyncMode;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dna_enc_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(dir: &Path, crypto: Option<Arc<Crypto>>) -> EngineConfig {
    EngineConfig {
        dir_path: dir.to_path_buf(),
        memtable_capacity: 32 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        crypto,
        ..Default::default()
    }
}

fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_file() {
            out.extend_from_slice(&std::fs::read(path).unwrap());
        }
    }
    out
}

#[test]
fn legacy_plaintext_and_encrypted_values_coexist() {
    let dir = tempdir("legacy_mixed");
    let legacy = b"LEGACY-PLAINTEXT-VALUE";
    let secret = b"NEW-ENCRYPTED-VALUE-0123456789";

    {
        let engine = Engine::open(config(&dir, None)).unwrap();
        engine.put(b"legacy:1", legacy).unwrap();
        engine.checkpoint().unwrap();
    }
    assert!(all_bytes(&dir).windows(legacy.len()).any(|w| w == legacy));

    let crypto = Arc::new(Crypto::from_key([23u8; 32]));
    {
        let engine = Engine::open(config(&dir, Some(crypto.clone()))).unwrap();
        assert_eq!(
            engine.get(b"legacy:1").unwrap().as_deref(),
            Some(legacy.as_slice())
        );
        engine.put(b"new:1", secret).unwrap();
        engine.checkpoint().unwrap();
    }

    let disk = all_bytes(&dir);
    assert!(
        !disk.windows(secret.len()).any(|w| w == secret),
        "new encrypted value leaked into WAL/SST"
    );
    let engine = Engine::open(config(&dir, Some(crypto))).unwrap();
    assert_eq!(
        engine.get(b"legacy:1").unwrap().as_deref(),
        Some(legacy.as_slice())
    );
    assert_eq!(
        engine.get(b"new:1").unwrap().as_deref(),
        Some(secret.as_slice())
    );
}

#[test]
fn encrypted_payloads_survive_reopen_and_scans() {
    let dir = tempdir("roundtrip");
    let crypto = Arc::new(Crypto::from_key([11u8; 32]));
    let secret = b"PATIENT-SECRET-ATGCATGCATGC-0123456789";

    {
        let engine = Engine::open(config(&dir, Some(crypto.clone()))).unwrap();
        for i in 0..20u32 {
            engine
                .put(format!("patient:{i:02}").as_bytes(), secret)
                .unwrap();
        }
        engine.checkpoint().unwrap();
    }

    let disk = all_bytes(&dir);
    assert!(
        !disk.windows(secret.len()).any(|w| w == secret),
        "plaintext value leaked into WAL/SST"
    );

    let engine = Engine::open(config(&dir, Some(crypto))).unwrap();
    assert_eq!(
        engine.get(b"patient:07").unwrap().as_deref(),
        Some(secret.as_slice())
    );
    let scan = engine.scan_prefix(b"patient:").unwrap();
    assert_eq!(scan.len(), 20);
    assert!(scan.iter().all(|(_, value)| value == secret));

    // A database reopened without the key must not silently expose plaintext.
    drop(engine);
    let no_key = Engine::open(config(&dir, None)).unwrap();
    let raw = no_key.get(b"patient:07").unwrap().unwrap();
    assert_ne!(raw, secret);
    assert!(raw.starts_with(b"DNAE1"));
}
