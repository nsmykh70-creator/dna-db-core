// SPDX-License-Identifier: AGPL-3.0-only

//! The `serve` subcommand: engine/security/TLS bootstrap, the TCP server and
//! its sidecars (admin panel, pgwire, MySQL wire listener), SIGINT/SIGTERM
//! handling and demo data seeding.

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};

use dna_db_core::dna_ql::DnaQlEngine;
use dna_db_core::engine::{Engine, EngineConfig};
use dna_db_core::repair::{RepairPolicy, ScrubConfig};
use dna_db_core::server::{DnaDbServer, ServerConfig};
use dna_db_core::sql_engine::SqlEngine;
use dna_db_core::wal::SyncMode;

use crate::cmd_util::acquire_dir_lock;

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_server(
    data_dir: &str,
    port: u16,
    bind_address: std::net::IpAddr,
    mirror_dir: Option<&str>,
    admin_user: &str,
    admin_pass: &str,
    admin_must_change: bool,
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
    no_tls: bool,
    tls_cert: Option<&str>,
    tls_key: Option<&str>,
    tls_client_ca: Option<&str>,
    no_encryption: bool,
    encryption_key_file: Option<&str>,
    proxy_auth: Option<dna_db_core::admin::ProxyAuthConfig>,
    pgwire_port: u16,
    #[allow(unused_variables)]
    mysql_port: u16,
    #[allow(unused_variables)]
    mysql_users: &[String],
) -> i32 {
    let data_path = std::path::Path::new(data_dir);
    if let Err(e) = std::fs::create_dir_all(data_path) {
        eprintln!("[FATAL] cannot create data dir {:?}: {e}", data_path);
        return 2;
    }

    // Advisory exclusive lock on the data directory for the whole server
    // lifetime: `doctor` refuses to operate on a live database, and a second
    // server instance cannot open the same WAL/segments concurrently.
    // The OS releases the flock on process death, so a crashed server never
    // leaves a stale lock behind.
    let _dir_lock = match acquire_dir_lock(data_path, "server") {
        Ok(lock) => lock,
        Err(message) => {
            eprintln!("[FATAL] {}", message);
            return 2;
        }
    };

    let security =
        match dna_db_core::server::SecurityManager::open_with_policy(
            data_path,
            admin_user,
            admin_pass,
            admin_must_change,
        ) {
            Ok(security) => Arc::new(security),
            Err(e) => {
                eprintln!("[FATAL] cannot open user database: {e}");
                return 2;
            }
        };
    eprintln!(
        "[AUTH] user database {:?} (Argon2id, persistent IP+username lockout)",
        data_path.join("users.json")
    );

    let crypto = if no_encryption {
        eprintln!("[CRYPTO] WARNING: payload encryption at rest is disabled");
        None
    } else {
        match dna_db_core::crypto::Crypto::load_or_create(
            data_path,
            encryption_key_file.map(std::path::Path::new),
        ) {
            Ok(crypto) => {
                eprintln!(
                    "[CRYPTO] at-rest payload encryption enabled (key fingerprint {})",
                    crypto.fingerprint()
                );
                Some(crypto)
            }
            Err(e) => {
                eprintln!("[FATAL] cannot load/create encryption key: {e}");
                return 2;
            }
        }
    };

    let tls = if no_tls {
        eprintln!("[TLS] WARNING: TLS disabled by --no-tls");
        None
    } else {
        match dna_db_core::tls::load_or_generate(
            data_path,
            tls_cert.map(std::path::Path::new),
            tls_key.map(std::path::Path::new),
            tls_client_ca.map(std::path::Path::new),
        ) {
            Ok(tls) => Some(tls),
            Err(e) => {
                eprintln!("[FATAL] cannot configure TLS: {e}");
                return 2;
            }
        }
    };

    let scrub = if no_scrub {
        None
    } else {
        Some(ScrubConfig {
            interval: std::time::Duration::from_secs(scrub_interval_secs.max(1)),
            rate_limit_bytes_per_sec: scrub_rate_mib * 1024 * 1024,
        })
    };
    let config = EngineConfig {
        dir_path: data_path.to_path_buf(),
        memtable_capacity: memtable_mb * 1024 * 1024,
        sync_mode: sync,
        mirror_disk_path: mirror_dir.map(std::path::PathBuf::from),
        erasure_k,
        erasure_m,
        repair_policy: RepairPolicy::RepairOnRead,
        scrub,
        wal_segment_capacity: wal_rotate_mb * 1024 * 1024,
        crypto,
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

    // CMS edition (branch cms-edition): one shared MySQL-dialect gate for
    // pgwire and the panel SQL console (sessions + named locks are global).
    #[cfg(feature = "cms")]
    let cms_gate: dna_db_core::CmsGateOpt = {
        let gate = std::sync::Arc::new(dna_db_core::cms::compat::CmsSqlGate::new(sql.clone()));
        eprintln!("[CMS] MySQL-dialect compat gate active (WordPress/Joomla track)");
        Some(gate)
    };
    #[cfg(not(feature = "cms"))]
    let cms_gate: dna_db_core::CmsGateOpt = ();

    // Embedded web admin panel (phpMyAdmin-style). Non-fatal bind errors.
    if admin_port != 0 {
        dna_db_core::admin::spawn(
            bind_address,
            admin_port,
            engine.clone(),
            sql.clone(),
            dnaql.clone(),
            security.clone(),
            tls.clone(),
            proxy_auth.clone(),
            cms_gate.clone(),
        );
        let scheme = if tls.is_some() { "https" } else { "http" };
        eprintln!("[ADMIN] open {scheme}://{bind_address}:{admin_port}/ (log in with the bootstrap admin; the one-time password is in the [SECURITY] line above)");
    }

    // v3.7: PostgreSQL wire protocol v3 bridge (psql / DBeaver / psycopg2 /
    // JDBC / Prisma / Drizzle) on the same engine — same SQL, same ACLs.
    if pgwire_port != 0 {
        let pg = dna_db_core::pgwire::PgWireServer::new();
        #[cfg(feature = "cms")]
        let pg = match cms_gate.clone() {
            Some(g) => pg.with_cms_gate(g),
            None => pg,
        };
        let pg_sql = sql.clone();
        let pg_bind = bind_address.to_string();
        std::thread::Builder::new()
            .name("pgwire-accept".into())
            .spawn(move || {
                if let Err(e) = pg.serve(&pg_bind, pgwire_port, pg_sql) {
                    eprintln!("[PGWIRE] listener stopped: {e}");
                }
            })
            .ok();
    }

    // CMS edition: MySQL wire listener on 3306 — the universal adapter for
    // every MySQL-based CMS (mysqli / PDO / wp-cli / mysqldump). Everything
    // is served by the shared MySQL-dialect gate.
    #[cfg(feature = "cms")]
    if mysql_port != 0 {
        let auth = if mysql_users.is_empty() {
            eprintln!(
                "[MYSQLWIRE] WARNING: no --mysql-user configured; the listener accepts ANY login (--skip-grant-tables mode)"
            );
            dna_db_core::cms::MySqlAuth::Trust
        } else {
            let mut map = std::collections::HashMap::new();
            for spec in mysql_users {
                match spec.split_once(':') {
                    Some((u, p)) => {
                        map.insert(u.to_string(), p.to_string());
                    }
                    None => {
                        map.insert(spec.clone(), String::new());
                    }
                }
            }
            dna_db_core::cms::MySqlAuth::Users(map)
        };
        let mysqld = dna_db_core::cms::MySqlWireServer::new(auth);
        let mysql_gate = match cms_gate.clone() {
            Some(g) => g,
            None => {
                eprintln!("[FATAL] MySQL listener requires the cms feature gate");
                return 2;
            }
        };
        let mysql_bind = bind_address.to_string();
        std::thread::Builder::new()
            .name("mysqlwire-accept".into())
            .spawn(move || {
                let listener = match std::net::TcpListener::bind((mysql_bind.as_str(), mysql_port))
                {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("[MYSQLWIRE] cannot bind {mysql_bind}:{mysql_port}: {e}");
                        return;
                    }
                };
                eprintln!(
                    "[MYSQLWIRE] MySQL wire protocol listening on {mysql_bind}:{mysql_port} (mysqli / PDO / wp-cli ready, reports as {})",
                    dna_db_core::cms::compat::MYSQL_COMPAT_VERSION
                );
                if let Err(e) = mysqld.serve(listener, mysql_gate) {
                    eprintln!("[MYSQLWIRE] listener stopped: {e}");
                }
            })
            .ok();
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
            tls,
            security: Some(security),
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
pub(crate) static SHUTDOWN_SIGNAL: AtomicBool = AtomicBool::new(false);

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

pub(crate) fn ctrlc_watcher<F: Fn() + Send + 'static>(f: F) {
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

pub(crate) fn seed_demo_data(engine: &Arc<Engine>) {
    use dna_db_core::relational::{
        ChromosomeTable, ColumnType, ForeignKeyConstraint, RowCodec, Value,
    };
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
        engine
            .put(format!("__schema:{}", name).as_bytes(), &payload)
            .unwrap();
    }

    let demo_users = [
        (12, "dr_watson", "watson@dna.org"),
        (14, "prof_crick", "crick@cam.ac.uk"),
        (18, "rosalind_f", "franklin@kcl.ac.uk"),
    ];
    let demo_orders = [(1042, 12, 350.00), (1089, 14, 740.00), (1105, 18, 520.00)];
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
            Some(RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            )),
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
            Some(RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            )),
        ));
    }
    for (id, name) in demo_patients {
        let mut row: BTreeMap<String, Value> = BTreeMap::new();
        row.insert("patient_id".into(), Value::Int(id));
        row.insert("name".into(), Value::Text(name.into()));
        batch.push((
            dna_db_core::wal::WalOp::Put,
            format!("chr_patients:{}", id).into_bytes(),
            Some(RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            )),
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
            Some(RowCodec::encode(
                &row.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            )),
        ));
    }
    let _ = engine.write_batch(&batch);
    eprintln!("[DEMO] seeded chr_users (3), chr_orders (3), chr_patients (2), chr_variants (3)");
}
