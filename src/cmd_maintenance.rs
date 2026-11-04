// SPDX-License-Identifier: AGPL-3.0-only

//! Offline and batch subcommands: `pitr`, `cluster`, `sql2dnaql`, `compact`,
//! `bench`, `rotate-key` and `doctor`.

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::sql_engine::SqlEngine;
use dna_db_core::wal::SyncMode;

use crate::cmd_util::{acquire_dir_lock, dir_size};

/// v3.9 PITR: physical snapshot restore points. The server holds an
/// advisory flock on the data dir, so a snapshot taken while the server is
/// down is always consistent; closed WALs + segments + schema rows are
/// copied with fsync and recorded in a manifest with their byte lengths.
pub(crate) fn run_pitr(data_dir: &str, target: Option<&str>, restore: bool) -> i32 {
    let src = std::path::Path::new(data_dir);
    if restore {
        let Some(target) = target else {
            eprintln!("[FATAL] --restore requires --target <restore point>");
            return 2;
        };
        let t = std::path::Path::new(target);
        if !t.join("pitr_manifest.json").exists() {
            eprintln!("[FATAL] {} is not a restore point (no manifest)", t.display());
            return 2;
        }
        let manifest = std::fs::read_to_string(t.join("pitr_manifest.json")).unwrap_or_default();
        if src.exists() {
            let backup = src.with_extension("pre-pitr");
            let _ = std::fs::remove_dir_all(&backup);
            if let Err(e) = std::fs::rename(src, &backup) {
                eprintln!("[FATAL] cannot move current data aside: {e}");
                return 1;
            }
            eprintln!("[PITR] current data moved to {:?}", backup);
        }
        if let Err(e) = std::fs::create_dir_all(src) {
            eprintln!("[FATAL] {e}");
            return 1;
        }
        let mut restored = 0usize;
        for name in manifest.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let from = t.join(name);
            let to = src.join(name);
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::copy(&from, &to) {
                Ok(n) => {
                    restored += 1;
                    let _ = n;
                }
                Err(e) => {
                    eprintln!("[FATAL] restore {}: {e}", name);
                    return 1;
                }
            }
        }
        eprintln!(
            "[PITR] restored {} file(s) from {} — WAL replay will bring the engine up to the archived LSN",
            restored,
            t.display()
        );
        return 0;
    }
    let Some(target) = target else {
        eprintln!("[FATAL] pitr needs --target <dir> (create) or --restore --target <dir>");
        return 2;
    };
    if !src.exists() {
        eprintln!("[FATAL] data dir {:?} does not exist", src);
        return 2;
    }
    let t = std::path::Path::new(target);
    let _ = std::fs::remove_dir_all(t);
    if let Err(e) = std::fs::create_dir_all(t) {
        eprintln!("[FATAL] {e}");
        return 1;
    }
    let mut manifest = String::new();
    let mut copied = 0usize;
    // Everything EXCEPT live-scratch state (active.wal is replayed from
    // closed WALs by recovery, and raft/ state is node-local).
    for entry in walk_files(src) {
        let rel = entry.strip_prefix(src).unwrap().to_path_buf();
        let name = rel.to_string_lossy().replace('\\', "/");
        if name.starts_with("raft/") || name.ends_with("active.wal") || name.contains("lock") {
            continue;
        }
        let to = t.join(&rel);
        if let Some(parent) = to.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::copy(&entry, &to).is_ok() {
            if let Ok(f) = std::fs::File::open(&to) {
                let _ = f.sync_all();
            }
            manifest.push_str(&name);
            manifest.push('\n');
            copied += 1;
        }
    }
    std::fs::write(t.join("pitr_manifest.json"), &manifest).unwrap();
    eprintln!("[PITR] restore point created: {} ({} file(s))", t.display(), copied);
    0
}

pub(crate) fn walk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

pub(crate) fn run_cluster(
    id: u64,
    peers: Vec<String>,
    data_dir: String,
    port: u16,
    bind: String,
    _no_encryption: bool,
    sync_mode: SyncMode,
) -> i32 {
    let mut parsed: Vec<(u64, String)> = Vec::new();
    for p in &peers {
        let Some((id_s, addr)) = p.split_once('=') else {
            eprintln!("[FATAL] --peers expects id=host:port entries, got '{p}'");
            return 2;
        };
        match id_s.parse::<u64>() {
            Ok(pid) => parsed.push((pid, addr.to_string())),
            Err(_) => {
                eprintln!("[FATAL] bad peer id '{id_s}'");
                return 2;
            }
        }
    }
    let config = EngineConfig {
        dir_path: std::path::PathBuf::from(&data_dir),
        sync_mode: sync_mode,
        crypto: None,
        ..Default::default()
    };
    let engine = match Engine::open(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[FATAL] cannot open cluster engine: {e}");
            return 1;
        }
    };
    let node = match dna_db_core::raft::RaftNode::open(
        id,
        parsed,
        std::path::PathBuf::from(&data_dir),
        engine,
    ) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[FATAL] raft init: {e}");
            return 1;
        }
    };
    node.spawn_ticker();
    eprintln!("[CLUSTER] node {id} up; peers={peers:?}");
    if let Err(e) = dna_db_core::raft::serve_node(node, &bind, port) {
        eprintln!("[FATAL] cluster listener: {e}");
        return 1;
    }
    0
}

/// `sql2dnaql` — print the DnaQL equivalent of the given SQL statement(s).
/// Reads stdin when --sql is not given, so it composes in pipelines:
///   cat migration.sql | dna-db-core sql2dnaql --data-dir ./dna_data
pub(crate) fn run_sql2dnaql(data_dir: &str, sql: Option<&str>) -> i32 {
    let text = match sql {
        Some(s) => s.to_string(),
        None => {
            let mut buf = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf) {
                eprintln!("[FATAL] cannot read stdin: {}", e);
                return 2;
            }
            buf
        }
    };
    if text.trim().is_empty() {
        eprintln!("[FATAL] no SQL given (use --sql \"...\" or pipe it on stdin)");
        return 2;
    }
    let crypto =
        match dna_db_core::crypto::Crypto::load_existing(std::path::Path::new(data_dir), None) {
            Ok(crypto) => crypto,
            Err(e) => {
                eprintln!("[FATAL] cannot load encryption key: {e}");
                return 2;
            }
        };
    let engine = match Engine::open(EngineConfig {
        dir_path: std::path::PathBuf::from(data_dir),
        memtable_capacity: 8 * 1024 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        crypto,
        ..Default::default()
    }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[FATAL] cannot open data dir '{}': {}", data_dir, e);
            return 2;
        }
    };
    let sql_engine = SqlEngine::new(engine);
    match sql_engine.sql_to_dnaql(&text) {
        Ok(out) => {
            println!("{}", out);
            // v4.1: also show WHERE the data physically lands — chromosome
            // (table), locus (PK), exact storage key and the physical
            // representation of every value. Best effort: the translation
            // itself already succeeded.
            match sql_engine.explain_storage(&text) {
                Ok(map) if !map.trim().is_empty() => {
                    println!("\n-- storage map (where the data physically lives) --");
                    println!("{}", map);
                }
                Ok(_) => {}
                Err(e) => eprintln!("[WARN] storage map unavailable: {}", e),
            }
            0
        }
        Err(e) => {
            eprintln!("[ERROR] {}", e);
            2
        }
    }
}

/// v4.1: offline full compaction (`dna-db-core compact`). Same safety rules
/// as the doctor: an advisory lock refuses to touch a live data dir. The
/// engine is opened read-write (WAL replay brings the memtable back), then
/// checkpoint() flushes everything into segments and compact() merges them.
pub(crate) fn run_compact(data_dir: &str, json: bool) -> i32 {
    let dir = std::path::Path::new(data_dir);
    if !dir.exists() {
        eprintln!("[FATAL] data dir '{}' does not exist", data_dir);
        return 2;
    }
    let _dir_lock = match acquire_dir_lock(dir, "compact") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[FATAL] {}", e);
            return 2;
        }
    };
    let crypto =
        match dna_db_core::crypto::Crypto::load_existing(dir, None) {
            Ok(crypto) => crypto,
            Err(e) => {
                eprintln!("[FATAL] cannot load encryption key: {e}");
                return 2;
            }
        };
    let engine = match Engine::open(EngineConfig {
        dir_path: dir.to_path_buf(),
        memtable_capacity: 8 * 1024 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
        crypto,
        ..Default::default()
    }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[FATAL] cannot open data dir '{}': {}", data_dir, e);
            return 2;
        }
    };
    if let Err(e) = engine.checkpoint() {
        eprintln!("[FATAL] checkpoint before compaction failed: {e}");
        return 1;
    }
    let deleted = match engine.compact() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[FATAL] compaction failed: {e}");
            return 1;
        }
    };
    let bytes = dir_size(dir);
    if json {
        println!(
            "{{\"data_dir\":{},\"deleted_files\":{},\"bytes_after\":{}}}",
            serde_json::to_string(data_dir).unwrap_or_else(|_| "\"?\"".into()),
            deleted,
            bytes
        );
    } else {
        println!(
            "[COMPACT] {} superseded file(s) removed; data dir now {:.1} MiB ({} B)",
            deleted,
            bytes as f64 / (1024.0 * 1024.0),
            bytes
        );
    }
    0
}

pub(crate) fn run_bench(data_dir: &str, ops: usize) -> i32 {
    if ops == 0 {
        eprintln!("[BENCH] --ops must be greater than zero");
        return 2;
    }
    let bench_data_dir = std::path::Path::new(data_dir).join(format!(
        "run-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    println!("=== DNA-DB Core Benchmarks (real measurements) ===");
    println!("data_dir: {}, ops: {}", bench_data_dir.display(), ops);

    // 1. Sequential PUT throughput (Strict fsync)
    {
        let engine = Engine::open(EngineConfig {
            dir_path: bench_data_dir.clone(),
            memtable_capacity: 32 * 1024 * 1024,
            sync_mode: SyncMode::Strict,
            mirror_disk_path: None,
            ..Default::default()
        })
        .unwrap();
        let t0 = std::time::Instant::now();
        for i in 0..ops {
            let key = format!("bench:seq:{:08}", i);
            let val = format!("ACTGACTGACTGACTG_{}", i);
            engine.put(key.as_bytes(), val.as_bytes()).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "[PUT strict-fsync]  {} ops in {:.3}s = {:.0} ops/s, avg {:.2} us/op",
            ops,
            dt.as_secs_f64(),
            ops as f64 / dt.as_secs_f64(),
            dt.as_nanos() as f64 / ops as f64 / 1000.0
        );

        // 2. Random GET latency (mix of memtable + segments)
        let t0 = std::time::Instant::now();
        let gets = (ops / 2).max(1000);
        for i in 0..gets {
            let key = format!("bench:seq:{:08}", (i * 7919) % ops);
            let _ = engine.get(key.as_bytes()).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "[GET random]        {} ops in {:.3}s = {:.0} ops/s, avg {:.2} us/op",
            gets,
            dt.as_secs_f64(),
            gets as f64 / dt.as_secs_f64(),
            dt.as_nanos() as f64 / gets as f64 / 1000.0
        );

        // 3. Prefix scan
        let t0 = std::time::Instant::now();
        let rows = engine.scan_prefix(b"bench:seq:").unwrap();
        let dt = t0.elapsed();
        println!(
            "[SCAN prefix]       {} rows in {:.3}s = {:.2} us total",
            rows.len(),
            dt.as_secs_f64(),
            dt.as_micros()
        );

        // 4. Batch atomic writes
        let t0 = std::time::Instant::now();
        let batches = ops / 10;
        for b in 0..batches {
            let mut ops_b = Vec::new();
            for j in 0..10 {
                ops_b.push((
                    dna_db_core::wal::WalOp::Put,
                    format!("bench:batch:{}:{}", b, j).into_bytes(),
                    Some(b"BATCHVALUE123456".to_vec()),
                ));
            }
            engine.write_batch(&ops_b).unwrap();
        }
        let dt = t0.elapsed();
        println!(
            "[BATCH x10 atomic]  {} batches ({} rows) in {:.3}s = {:.0} batches/s",
            batches,
            batches * 10,
            dt.as_secs_f64(),
            batches as f64 / dt.as_secs_f64()
        );
    }

    // 5. Bitap alignment throughput on 100k bp target
    {
        use dna_db_core::dna::PackedDna;
        use dna_db_core::simd_bitap::BitapMatcher;
        let mut seq = String::with_capacity(100_000);
        let mut state = 0x9E3779B97F4A7C15u64;
        for _ in 0..100_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            seq.push(
                ["A", "C", "G", "T"][(state % 4) as usize]
                    .chars()
                    .next()
                    .unwrap(),
            );
        }
        // embed 3 known occurrences of the pattern
        let pattern = "GATTACA";
        let mut bytes = seq.into_bytes();
        for pos in [10_000, 50_000, 90_000] {
            bytes[pos..pos + pattern.len()].copy_from_slice(pattern.as_bytes());
        }
        let target = PackedDna::from_ascii(&String::from_utf8(bytes).unwrap()).unwrap();

        let t0 = std::time::Instant::now();
        let matches = BitapMatcher::search(&target, pattern, 1);
        let dt = t0.elapsed();
        println!(
            "[BITAP Levenshtein] 100k bp, k<=1: {} matches in {:.3} ms = {:.1} Mbp/s",
            matches.len(),
            dt.as_secs_f64() * 1000.0,
            0.1 / dt.as_secs_f64()
        );

        let t0 = std::time::Instant::now();
        let matches_h = BitapMatcher::search_hamming(&target, pattern, 1);
        let dt = t0.elapsed();
        println!(
            "[BITAP Hamming]     100k bp, k<=1: {} matches in {:.3} ms = {:.1} Mbp/s",
            matches_h.len(),
            dt.as_secs_f64() * 1000.0,
            0.1 / dt.as_secs_f64()
        );
    }

    let _ = std::fs::remove_dir_all(&bench_data_dir);
    0
}

/// Offline doctor: scan / fix / migrate (P5.1).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_rotate_key(
    data_dir: &str,
    old_key_file: Option<&str>,
    new_key_file: &str,
    memtable_mb: usize,
    erasure_k: usize,
    erasure_m: usize,
    wal_rotate_mb: u64,
    mirror_dir: Option<&str>,
) -> i32 {
    let data_path = std::path::Path::new(data_dir);
    let data_abs = match std::fs::canonicalize(data_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[FATAL] cannot resolve data directory {:?}: {e}", data_path);
            return 2;
        }
    };
    let new_path = std::path::Path::new(new_key_file);
    let new_parent = new_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let new_abs = match std::fs::canonicalize(new_parent) {
        Ok(p) => p.join(new_path.file_name().unwrap_or_default()),
        Err(e) => {
            eprintln!("[FATAL] cannot resolve new key directory {new_parent:?}: {e}");
            return 2;
        }
    };
    if new_abs.starts_with(&data_abs) {
        eprintln!(
            "[FATAL] --new-key-file must be outside the data directory; \
             keep key material separate from ciphertext"
        );
        return 2;
    }

    let old_crypto = match dna_db_core::crypto::Crypto::load_existing(
        data_path,
        old_key_file.map(std::path::Path::new),
    ) {
        Ok(crypto) => crypto,
        Err(e) => {
            eprintln!("[FATAL] cannot load old encryption key: {e}");
            return 2;
        }
    };
    match &old_crypto {
        Some(crypto) => eprintln!("[ROTATE] old key fingerprint {}", crypto.fingerprint()),
        None => eprintln!("[ROTATE] no old key found; treating the source as plaintext"),
    }

    let (new_crypto, created) = match dna_db_core::crypto::Crypto::load_or_create_key_file(new_path)
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[FATAL] cannot load/create new key: {e}");
            return 2;
        }
    };
    eprintln!(
        "[ROTATE] new key {} at {:?}",
        if created { "generated" } else { "loaded" },
        new_path
    );

    let options = dna_db_core::rotate::RotateOptions {
        memtable_capacity: memtable_mb * 1024 * 1024,
        erasure_k,
        erasure_m,
        wal_segment_capacity: wal_rotate_mb * 1024 * 1024,
        mirror_dir: mirror_dir.map(std::path::PathBuf::from),
    };
    match dna_db_core::rotate::rotate_key(data_path, old_crypto, new_crypto, options) {
        Ok(report) => {
            println!(
                "[ROTATE] complete: {} live keys, {} plaintext bytes; new fingerprint {}",
                report.keys, report.plaintext_bytes, report.new_fingerprint
            );
            println!("[ROTATE] rollback copy: {:?}", report.backup_dir);
            if report.mirror_rebuilt {
                match &report.mirror_backup_dir {
                    Some(backup) => println!(
                        "[ROTATE] external mirror rebuilt; previous mirror: {:?}",
                        backup
                    ),
                    None => println!("[ROTATE] external mirror created and seeded"),
                }
            }
            println!("[ROTATE] verify the server, then delete the rollback copy and the old key");
            0
        }
        Err(e) => {
            let message = e.to_string();
            if created && !message.contains("rollback failed") {
                let _ = std::fs::remove_file(new_path);
            }
            eprintln!("[FATAL] key rotation failed: {e}");
            1
        }
    }
}

pub(crate) fn run_doctor(
    data_dir: &str,
    mirror_dir: Option<&str>,
    fix: bool,
    migrate: bool,
    json: bool,
    erasure_k: usize,
    erasure_m: usize,
) -> i32 {
    use dna_db_core::repair;

    let dir = std::path::Path::new(data_dir);
    if !dir.exists() {
        eprintln!("[DOCTOR] data dir does not exist: {}", dir.display());
        return 2;
    }
    if let Some(mirror) = mirror_dir {
        let mdir = std::path::Path::new(mirror);
        if !mdir.exists() {
            eprintln!("[DOCTOR] mirror dir does not exist: {}", mdir.display());
            return 2;
        }
        if mdir.canonicalize().ok() == dir.canonicalize().ok() {
            eprintln!("[DOCTOR] --mirror-dir must differ from --data-dir");
            return 2;
        }
    }

    // Refuse to operate on a live database: the server holds an exclusive
    // flock on <data-dir>/.dna-db.lock for its whole lifetime. Two concurrent
    // doctor runs are mutually excluded the same way. (Previously doctor
    // happily mutated WAL/segments under a running server.)
    let _dir_lock = match acquire_dir_lock(dir, "doctor") {
        Ok(lock) => lock,
        Err(message) => {
            eprintln!("[DOCTOR] refusing to run: {}", message);
            eprintln!(
                "[DOCTOR] if a server is running, stop it first; doctor must never \
                 mutate a live database"
            );
            return 2;
        }
    };

    let result: std::io::Result<repair::DoctorReport> = (|| {
        if migrate {
            repair::doctor_migrate(dir, erasure_k, erasure_m)
        } else if fix {
            repair::doctor_fix(dir)
        } else {
            repair::doctor_scan(dir)
        }
    })();

    // Cross-strand cascade (A↔B): heal damaged copies across the spirals.
    // Runs after the per-side RS fix so parity is tried first, mirror second.
    // Repair both strands, not only the primary: a damaged B-only segment or
    // closed WAL must not stay invisible just because the CLI was pointed at A.
    if fix && !migrate {
        if let Some(mirror) = mirror_dir {
            match repair::doctor_fix(std::path::Path::new(mirror)) {
                Ok(b_report) => {
                    if !b_report.errors.is_empty() {
                        eprintln!(
                            "[DOCTOR] Strand B fix completed with {} error(s)",
                            b_report.errors.len()
                        );
                    }
                }
                Err(e) => eprintln!("[DOCTOR] Strand B fix failed: {e}"),
            }
        }
    }

    let cascade = match (mirror_dir, migrate) {
        (Some(mirror), false) => {
            match repair::doctor_cascade_strands(dir, std::path::Path::new(mirror), fix) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[DOCTOR] cross-strand cascade failed: {}", e);
                    None
                }
            }
        }
        _ => None,
    };

    match result {
        Ok(report) => {
            if json {
                let mut out = serde_json::to_value(&report).unwrap_or_default();
                if let Some(c) = &cascade {
                    out["cascade"] = serde_json::to_value(c).unwrap_or_default();
                }
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            } else {
                println!("=== DNA-DB Doctor report — {} ===", report.data_dir);
                println!(
                    "segments: {} scanned, {} healthy, {} repairable, {} unrecoverable, {} migrated",
                    report.scanned_segments, report.healthy, report.repairable,
                    report.unrecoverable, report.migrated
                );
                if report.fixed_blocks > 0 {
                    println!("blocks rewritten from parity: {}", report.fixed_blocks);
                }
                for v in &report.verdicts {
                    println!(
                        "  {:<28} {:<9} {:>8} records  {:>6}/{:<6} blocks damaged  {:>2} unrecoverable stripes  [{}]",
                        v.file,
                        v.format,
                        v.records,
                        v.damaged_blocks,
                        v.blocks_total,
                        v.unrecoverable_stripes,
                        v.verdict,
                    );
                    if let Some(d) = &v.detail {
                        println!("      detail: {}", d);
                    }
                }
                if !report.errors.is_empty() {
                    println!("fix errors ({}):", report.errors.len());
                    for e in &report.errors {
                        println!("  - {}", e);
                    }
                }
                if let Some(c) = &cascade {
                    println!(
                        "cross-strand cascade (A: {} <-> B: {}):",
                        c.strand_a, c.strand_b
                    );
                    println!(
                        "  segments healed: A from B: {}, B from A: {}",
                        c.segments_healed_a_from_b, c.segments_healed_b_from_a
                    );
                    println!(
                        "  closed WALs healed: A from B: {}, B from A: {}",
                        c.wal_healed_a_from_b, c.wal_healed_b_from_a
                    );
                    if !c.damaged_on_both.is_empty() {
                        println!(
                            "  damaged on BOTH strands (not healed): {:?}",
                            c.damaged_on_both
                        );
                    }
                    if !fix && !c.would_heal.is_empty() {
                        println!("  dry run — would heal: {:?}", c.would_heal);
                    }
                }
                let mut advice = Vec::new();
                if report.unrecoverable > 0 && mirror_dir.is_none() {
                    advice.push("some segments exceed parity: re-run with --mirror-dir <strand-b-dir> --fix to cascade from the other spiral");
                }
                if report.migrated == 0 && report.verdicts.iter().any(|v| v.format == "DNASST01") {
                    advice.push("legacy DNASST01 segments present: run again with --migrate");
                }
                if !advice.is_empty() {
                    println!("advice:");
                    for a in advice {
                        println!("  - {}", a);
                    }
                }
            }
            // If the cascade healed files that RS parity could not, re-scan
            // so the exit code reflects the on-disk truth, not the pre-cascade
            // snapshot.
            let unrecoverable = if cascade.is_some() && fix {
                repair::doctor_scan(dir)
                    .map(|fresh| fresh.unrecoverable)
                    .unwrap_or(report.unrecoverable)
            } else {
                report.unrecoverable
            };
            if unrecoverable > 0 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("[DOCTOR] scan failed: {}", e);
            1
        }
    }
}
