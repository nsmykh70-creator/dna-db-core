// SPDX-License-Identifier: AGPL-3.0-only
//! DNA-DB Core CLI
//!
//!   dna-db-core serve   --data-dir ./dna_data --port 9999 ...
//!   dna-db-core bench   [--data-dir ./bench_tmp]
//!   dna-db-core shell   --host 127.0.0.1 --port 9999
//!   dna-db-core doctor  --data-dir ./dna_data [--fix] [--migrate] [--json]
//!   dna-db-core sql2dnaql --sql "SELECT ..." [--data-dir ./dna_data]

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{BufRead, Read, Write};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::dna_ql::DnaQlEngine;
use dna_db_core::repair::{RepairPolicy, ScrubConfig};
use dna_db_core::server::{DnaDbServer, ServerConfig};
use dna_db_core::sql_engine::SqlEngine;
use dna_db_core::wal::SyncMode;

#[derive(ValueEnum, Clone, Debug)]
enum SyncArg {
    Strict,
    Batched,
    Async,
}

impl SyncArg {
    fn to_mode(&self) -> SyncMode {
        match self {
            SyncArg::Strict => SyncMode::Strict,
            SyncArg::Batched => SyncMode::Batched,
            SyncArg::Async => SyncMode::Async,
        }
    }
}

#[derive(Parser)]
#[command(name = "dna-db-core", version, about = "DNA-DB: genomic-relational LSM storage engine with self-healing (v3.0 Repairosome)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the TCP server
    Serve {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        #[arg(long, default_value_t = 9999)]
        port: u16,
        /// Interface to listen on; choose 0.0.0.0 explicitly for remote access.
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        #[arg(long)]
        mirror_dir: Option<String>,
        #[arg(long, default_value = "admin")]
        admin_user: String,
        /// Set directly or through DNA_DB_ADMIN_PASS; there is no default password.
        #[arg(long)]
        admin_pass: Option<String>,
        #[arg(long, value_enum, default_value = "strict")]
        sync: SyncArg,
        #[arg(long, default_value_t = 8)]
        memtable_mb: usize,
        /// Seed small demo tables (chr_users / chr_orders) for experimentation
        #[arg(long, default_value_t = false)]
        demo_data: bool,
        /// Reed–Solomon data shards per stripe (default 4)
        #[arg(long, default_value_t = 4)]
        erasure_k: usize,
        /// Reed–Solomon parity shards per stripe (default 2)
        #[arg(long, default_value_t = 2)]
        erasure_m: usize,
        /// Disable the background scrubber
        #[arg(long, default_value_t = false)]
        no_scrub: bool,
        /// Scrubber interval in seconds (default 86400 = 24 h)
        #[arg(long, default_value_t = 86_400)]
        scrub_interval_secs: u64,
        /// Scrubber rate limit in MiB/s (default 32)
        #[arg(long, default_value_t = 32)]
        scrub_rate_mib: u64,
        /// Rotate the WAL at this size in MiB (default 64; 0 = never)
        #[arg(long, default_value_t = 64)]
        wal_rotate_mb: u64,
        /// HTTP port of the web admin panel (same admin credentials);
        /// 0 disables the panel. Binds to --bind (loopback by default).
        #[arg(long, default_value_t = 8080)]
        admin_port: u16,
    },
    /// Run real throughput/latency benchmarks and print results
    Bench {
        #[arg(long, default_value = "./bench_tmp")]
        data_dir: String,
        #[arg(long, default_value_t = 50_000)]
        ops: usize,
    },
    /// Interactive client shell (SQL + DnaQL)
    Shell {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9999)]
        port: u16,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(long)]
        pass: Option<String>,
    },
    /// Offline genome health check: scan / repair / migrate segments
    Doctor {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        /// Repair every fixable segment (RS reconstruction from parity)
        #[arg(long, default_value_t = false)]
        fix: bool,
        /// Migrate legacy DNASST01 segments to erasure-coded DNASST02
        #[arg(long, default_value_t = false)]
        migrate: bool,
        /// Machine-readable JSON report
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Reed–Solomon geometry used when migrating (default 4+2)
        #[arg(long, default_value_t = 4)]
        erasure_k: usize,
        #[arg(long, default_value_t = 2)]
        erasure_m: usize,
        /// Strand B directory: cascade-repair damaged segments/closed WALs
        /// across the two spirals (A←B and B←A), offline
        #[arg(long)]
        mirror_dir: Option<String>,
    },
    /// Convert SQL statements into the equivalent DnaQL query
    #[command(name = "sql2dnaql")]
    Sql2Dnaql {
        /// Database directory holding the schemas the SQL refers to
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        /// SQL text to convert; reads from stdin when omitted
        #[arg(long)]
        sql: Option<String>,
    },
}

fn sync_arg_to_mode(mode: &SyncArg) -> SyncMode {
    mode.to_mode()
}

fn main() {
    let cli = Cli::parse();
    let code = match &cli.command {
        Command::Serve {
            data_dir,
            port,
            bind,
            mirror_dir,
            admin_user,
            admin_pass,
            sync,
            memtable_mb,
            demo_data,
            erasure_k,
            erasure_m,
            no_scrub,
            scrub_interval_secs,
            scrub_rate_mib,
            wal_rotate_mb,
            admin_port,
        } => {
            let admin_pass = admin_pass
                .clone()
                .or_else(|| std::env::var("DNA_DB_ADMIN_PASS").ok());
            match admin_pass {
                Some(pass) if !pass.is_empty() => run_server(
                    data_dir,
                    *port,
                    *bind,
                    mirror_dir.as_deref(),
                    admin_user,
                    &pass,
                    sync_arg_to_mode(sync),
                    *memtable_mb,
                    *demo_data,
                    *erasure_k,
                    *erasure_m,
                    *no_scrub,
                    *scrub_interval_secs,
                    *scrub_rate_mib,
                    *wal_rotate_mb,
                    *admin_port,
                ),
                _ => {
                    eprintln!(
                        "[FATAL] provide --admin-pass or set DNA_DB_ADMIN_PASS; no default password is configured"
                    );
                    2
                }
            }
        }
        Command::Bench { data_dir, ops } => run_bench(data_dir, *ops),
        Command::Shell {
            host,
            port,
            user,
            pass,
        } => {
            let pass = pass.clone().or_else(|| std::env::var("DNA_DB_ADMIN_PASS").ok());
            match pass {
                Some(pass) => run_shell(host, *port, user, &pass),
                None => {
                    eprintln!("[FATAL] provide --pass or set DNA_DB_ADMIN_PASS");
                    2
                }
            }
        }
        Command::Doctor {
            data_dir,
            fix,
            migrate,
            json,
            erasure_k,
            erasure_m,
            mirror_dir,
        } => run_doctor(
            data_dir,
            mirror_dir.as_deref(),
            *fix,
            *migrate,
            *json,
            *erasure_k,
            *erasure_m,
        ),
        Command::Sql2Dnaql { data_dir, sql } => run_sql2dnaql(data_dir, sql.as_deref()),
    };
    std::process::exit(code);
}

/// `sql2dnaql` — print the DnaQL equivalent of the given SQL statement(s).
/// Reads stdin when --sql is not given, so it composes in pipelines:
///   cat migration.sql | dna-db-core sql2dnaql --data-dir ./dna_data
fn run_sql2dnaql(data_dir: &str, sql: Option<&str>) -> i32 {
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
    let engine = match Engine::open(EngineConfig {
        dir_path: std::path::PathBuf::from(data_dir),
        memtable_capacity: 8 * 1024 * 1024,
        sync_mode: SyncMode::Strict,
        mirror_disk_path: None,
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
            0
        }
        Err(e) => {
            eprintln!("[ERROR] {}", e);
            2
        }
    }
}

fn run_server(
    data_dir: &str,
    port: u16,
    bind_address: std::net::IpAddr,
    mirror_dir: Option<&str>,
    admin_user: &str,
    admin_pass: &str,
    sync: SyncMode,
    memtable_mb: usize,
    demo_data: bool,
    erasure_k: usize,
    erasure_m: usize,
    no_scrub: bool,
    scrub_interval_secs: u64,
    scrub_rate_mib: u64,
    wal_rotate_mb: u64,
    admin_port: u16,
) -> i32 {
    if admin_pass.is_empty() {
        eprintln!("[FATAL] admin password must not be empty");
        return 2;
    }
    let scrub = if no_scrub {
        None
    } else {
        Some(ScrubConfig {
            interval: std::time::Duration::from_secs(scrub_interval_secs.max(1)),
            rate_limit_bytes_per_sec: scrub_rate_mib * 1024 * 1024,
        })
    };
    let config = EngineConfig {
        dir_path: data_dir.into(),
        memtable_capacity: memtable_mb * 1024 * 1024,
        sync_mode: sync,
        mirror_disk_path: mirror_dir.map(std::path::PathBuf::from),
        erasure_k,
        erasure_m,
        repair_policy: RepairPolicy::RepairOnRead,
        scrub,
        wal_segment_capacity: wal_rotate_mb * 1024 * 1024,
    };
    let engine = match Engine::open(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[FATAL] cannot open engine: {}", e);
            eprintln!(
                "[HINT] if segments are damaged, run: dna-db-core doctor --data-dir {} --fix",
                data_dir
            );
            return 1;
        }
    };

    if demo_data {
        seed_demo_data(&engine);
    }

    let sql = Arc::new(SqlEngine::new(engine.clone()));
    let dnaql = Arc::new(DnaQlEngine::new(engine.clone(), sql.clone()));

    // Embedded web admin panel (phpMyAdmin-style). Non-fatal bind errors.
    if admin_port != 0 {
        dna_db_core::admin::spawn(
            bind_address,
            admin_port,
            engine.clone(),
            sql.clone(),
            dnaql.clone(),
            admin_user,
            admin_pass,
        );
    }
    let server = Arc::new(DnaDbServer::new(
        engine,
        dnaql,
        sql,
        ServerConfig {
            bind_address,
            port,
            admin_user: admin_user.into(),
            admin_pass: admin_pass.into(),
            ..Default::default()
        },
    ));

    let signal_server = server.clone();
    ctrlc_watcher(move || signal_server.shutdown());

    match server.serve() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[FATAL] server error: {}", e);
            1
        }
    }
}

#[cfg(unix)]
static SHUTDOWN_SIGNAL: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_shutdown_signal(_: i32) {
    SHUTDOWN_SIGNAL.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" {
    fn signal(
        signal_number: i32,
        handler: Option<extern "C" fn(i32)>,
    ) -> Option<extern "C" fn(i32)>;
}

fn ctrlc_watcher<F: Fn() + Send + 'static>(f: F) {
    #[cfg(unix)]
    {
        SHUTDOWN_SIGNAL.store(false, Ordering::SeqCst);
        unsafe {
            let _ = signal(2, Some(handle_shutdown_signal)); // SIGINT
            let _ = signal(15, Some(handle_shutdown_signal)); // SIGTERM
        }
        std::thread::spawn(move || {
            while !SHUTDOWN_SIGNAL.swap(false, Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            f();
        });
    }
    #[cfg(not(unix))]
    {
        let _ = f;
    }
}

fn seed_demo_data(engine: &Arc<Engine>) {
    use dna_db_core::relational::{ChromosomeTable, ColumnType, ForeignKeyConstraint, RowCodec, Value};
    use std::collections::BTreeMap;

    let mut users = ChromosomeTable::new("chr_users", "user_id");
    users.add_column("user_id", ColumnType::Int64);
    users.add_column("username", ColumnType::Text);
    users.add_column("email", ColumnType::Text);
    let mut orders = ChromosomeTable::new("chr_orders", "order_id");
    orders.add_column("order_id", ColumnType::Int64);
    orders.add_column("user_id", ColumnType::Int64);
    orders.add_column("total_usd", ColumnType::Float64);
    orders.add_column("status", ColumnType::Text);
    orders.add_foreign_key(ForeignKeyConstraint {
        column: "user_id".into(),
        target_chromosome: "chr_users".into(),
        target_locus_column: "user_id".into(),
        on_delete_cascade: true,
    });

    // chr_variants: DNA chromosome for DnaQL pattern search / linkage demos
    let mut patients = ChromosomeTable::new("chr_patients", "patient_id");
    patients.add_column("patient_id", ColumnType::Int64);
    patients.add_column("name", ColumnType::Text);

    let mut variants = ChromosomeTable::new("chr_variants", "variant_id");
    variants.add_column("variant_id", ColumnType::Int64);
    variants.add_column("variant_name", ColumnType::Text);
    variants.add_column("sequence", ColumnType::Dna2Bit);
    variants.add_column("patient_id", ColumnType::Int64);
    variants.add_foreign_key(ForeignKeyConstraint {
        column: "patient_id".into(),
        target_chromosome: "chr_patients".into(),
        target_locus_column: "patient_id".into(),
        on_delete_cascade: true,
    });

    for (name, t) in [
        ("chr_users", users),
        ("chr_orders", orders),
        ("chr_patients", patients),
        ("chr_variants", variants),
    ] {
        let payload = serde_json::to_vec(&t).unwrap();
        engine.put(format!("__schema:{}", name).as_bytes(), &payload).unwrap();
    }

    let demo_users = [
        (12, "dr_watson", "watson@dna.org"),
        (14, "prof_crick", "crick@cam.ac.uk"),
        (18, "rosalind_f", "franklin@kcl.ac.uk"),
    ];
    let demo_orders = [
        (1042, 12, 350.00),
        (1089, 14, 740.00),
        (1105, 18, 520.00),
    ];
    let demo_patients = [(100, "alice"), (101, "bob")];
    let demo_variants: [(i64, &str, &str, i64); 3] = [
        (1, "rs_demo_1", "ATGCGATCGATCCGTA", 100),
        (2, "rs_demo_2", "GATCGATCATGCGATC", 101),
        (3, "rs_demo_3", "TTTTGGGGCCCCAAAA", 100),
    ];

    let mut batch = Vec::new();
    for (id, name, email) in demo_users {
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("user_id".into(), Value::Int(id));
        row.insert("username".into(), Value::Text(name.into()));
        row.insert("email".into(), Value::Text(email.into()));
        batch.push((
            dna_db_core::wal::WalOp::Put,
            format!("chr_users:{}", id).into_bytes(),
            Some(RowCodec::encode(&row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())),
        ));
    }
    for (oid, uid, total) in demo_orders {
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("order_id".into(), Value::Int(oid));
        row.insert("user_id".into(), Value::Int(uid));
        row.insert("total_usd".into(), Value::Float(total));
        row.insert("status".into(), Value::Text("PENDING".into()));
        batch.push((
            dna_db_core::wal::WalOp::Put,
            format!("chr_orders:{}", oid).into_bytes(),
            Some(RowCodec::encode(&row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())),
        ));
    }
    for (id, name) in demo_patients {
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("patient_id".into(), Value::Int(id));
        row.insert("name".into(), Value::Text(name.into()));
        batch.push((
            dna_db_core::wal::WalOp::Put,
            format!("chr_patients:{}", id).into_bytes(),
            Some(RowCodec::encode(&row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())),
        ));
    }
    for (id, vname, seq, pid) in demo_variants {
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("variant_id".into(), Value::Int(id));
        row.insert("variant_name".into(), Value::Text(vname.into()));
        row.insert("sequence".into(), Value::Dna(seq.into()));
        row.insert("patient_id".into(), Value::Int(pid));
        batch.push((
            dna_db_core::wal::WalOp::Put,
            format!("chr_variants:{}", id).into_bytes(),
            Some(RowCodec::encode(&row.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())),
        ));
    }
    let _ = engine.write_batch(&batch);
    eprintln!("[DEMO] seeded chr_users (3), chr_orders (3), chr_patients (2), chr_variants (3)");
}

fn run_bench(data_dir: &str, ops: usize) -> i32 {
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
            seq.push(["A", "C", "G", "T"][(state % 4) as usize].chars().next().unwrap());
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
fn run_doctor(
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

fn run_shell(host: &str, port: u16, user: &str, pass: &str) -> i32 {
    use std::net::TcpStream;
    let mut stream = match TcpStream::connect((host, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to {}:{}: {}", host, port, e);
            return 1;
        }
    };
    stream.set_nodelay(true).ok();

    // Auth
    let msg = format!("\x01{}:{}", user, pass);
    if send_frame(&mut stream, msg.as_bytes()).is_err() {
        return 1;
    }
    let mut ok = [0u8; 2];
    if stream.read_exact(&mut ok).is_err() || &ok != b"OK" {
        eprintln!("authentication failed");
        return 1;
    }
    println!("Connected to {}:{} as {} (type SQL or DnaQL; .quit to exit)", host, port, user);

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("dna-db> ");
        std::io::stdout().flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let q = line.trim();
        if q.is_empty() {
            continue;
        }
        if q == ".quit" || q == ".exit" {
            break;
        }
        if q == ".status" {
            if send_frame(&mut stream, b"\x00").is_err() {
                break;
            }
            match read_frame_resp(&mut stream) {
                Ok(Some(resp)) => println!("{}", String::from_utf8_lossy(&resp)),
                _ => break,
            }
            continue;
        }

        let upper = q.to_uppercase();
        let opcode: u8 = if upper.starts_with("SELECT")
            || upper.starts_with("INSERT")
            || upper.starts_with("UPDATE")
            || upper.starts_with("DELETE")
            || upper.starts_with("CREATE")
            || upper.starts_with("DROP")
        {
            0x11
        } else {
            0x10
        };
        let payload = format!("{}{}", opcode as char, q);
        if send_frame(&mut stream, payload.as_bytes()).is_err() {
            eprintln!("connection lost");
            break;
        }
        match read_frame_resp(&mut stream) {
            Ok(Some(resp)) => println!("{}", String::from_utf8_lossy(&resp)),
            Ok(None) => break,
            Err(e) => {
                eprintln!("recv error: {}", e);
                break;
            }
        }
    }
    0
}

fn send_frame(stream: &mut std::net::TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)
}

fn read_frame_resp(stream: &mut std::net::TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(Some(payload))
}
