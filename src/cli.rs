// SPDX-License-Identifier: AGPL-3.0-only

//! CLI definitions: the clap `Cli`/`Command` types, the `SyncArg` enum and
//! its mapping onto the WAL sync mode. `main()` only dispatches.

use clap::{Parser, Subcommand, ValueEnum};

use dna_db_core::wal::SyncMode;

#[derive(ValueEnum, Clone, Debug)]
pub(crate) enum SyncArg {
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
#[command(
    name = "dna-db-core",
    version,
    about = "DNA-DB: DNA-inspired LSM storage engine for ordinary digital data, with self-healing, TLS and encryption at rest (v4.0)"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
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
        /// Set directly or through DNA_DB_ADMIN_PASS. The first start defaults
        /// to admin/password and prints a prominent change-it warning.
        #[arg(long)]
        admin_pass: Option<String>,
        /// Disable TLS (development/legacy clients only)
        #[arg(long, default_value_t = false)]
        no_tls: bool,
        /// PEM certificate chain (default: generated self-signed under DATA/tls)
        #[arg(long)]
        tls_cert: Option<String>,
        /// PEM private key (required together with --tls-cert)
        #[arg(long)]
        tls_key: Option<String>,
        /// Require client certificates signed by this PEM CA (mutual TLS)
        #[arg(long)]
        tls_client_ca: Option<String>,
        /// Disable payload encryption at rest (not recommended)
        #[arg(long, default_value_t = false)]
        no_encryption: bool,
        /// Master key file (32 raw bytes or 64 hex chars). Default:
        /// DNA/data/.dna-db.key, or DNA_DB_ENCRYPTION_KEY.
        #[arg(long)]
        encryption_key_file: Option<String>,
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
        /// Trust this reverse-proxy IP for header-based SSO (repeatable).
        /// The proxy must strip client-supplied identity headers.
        #[arg(long = "trusted-proxy", value_name = "IP")]
        trusted_proxy: Vec<std::net::IpAddr>,
        /// Header injected by the trusted proxy with the authenticated username.
        #[arg(long = "proxy-auth-header", default_value = "X-Auth-Request-User")]
        proxy_auth_header: String,
        /// Shared-secret header name checked in addition to the peer IP.
        #[arg(long = "proxy-auth-secret-header", default_value = "X-Proxy-Auth-Secret")]
        proxy_auth_secret_header: String,
        /// Optional shared secret value; also via DNA_DB_PROXY_AUTH_SECRET.
        #[arg(long = "proxy-auth-secret")]
        proxy_auth_secret: Option<String>,
        /// v3.7: enable the PostgreSQL wire protocol v3 listener on this port
        /// (psql, DBeaver, pgAdmin, psycopg2, JDBC, Prisma, Drizzle). 0 = off.
        #[arg(long, default_value_t = 5432)]
        pgwire_port: u16,
        /// CMS edition: MySQL wire protocol listener (mysqli, PDO, wp-cli,
        /// mysql client, mysqldump — everything that speaks MySQL). 0 = off.
        #[arg(long, default_value_t = 3306)]
        mysql_port: u16,
        /// CMS edition: MySQL listener auth. Repeatable user[:pass] pairs;
        /// when absent the listener trusts any login (like
        /// --skip-grant-tables) and prints a warning.
        #[arg(long = "mysql-user", value_name = "USER[:PASS]")]
        mysql_users: Vec<String>,
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
        /// Connect with plaintext (development only)
        #[arg(long, default_value_t = false)]
        no_tls: bool,
        /// PEM CA/certificate for a private or self-signed server
        #[arg(long)]
        tls_ca: Option<String>,
        /// PEM client certificate chain (for mutual TLS)
        #[arg(long)]
        tls_client_cert: Option<String>,
        /// PEM client private key (required together with --tls-client-cert)
        #[arg(long)]
        tls_client_key: Option<String>,
    },
    /// Offline storage health check: scan / repair / migrate segments (DNA-style self-repair)
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
    /// v4.1: offline full compaction — merge all segments into one, drop
    /// shadowed versions and consumed tombstones. Refuses a live data dir
    /// (advisory lock, same as doctor). Reports the freed disk space.
    Compact {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        /// Machine-readable summary (single JSON line)
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Offline payload-key rotation (re-encrypts every logical row)
    #[command(name = "rotate-key")]    RotateKey {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        /// Old key file (default: DATA/.dna-db.key or DNA_DB_ENCRYPTION_KEY)
        #[arg(long)]
        old_key_file: Option<String>,
        /// New key file; generated when absent. Must be outside DATA.
        #[arg(long)]
        new_key_file: String,
        #[arg(long, default_value_t = 8)]
        memtable_mb: usize,
        #[arg(long, default_value_t = 4)]
        erasure_k: usize,
        #[arg(long, default_value_t = 2)]
        erasure_m: usize,
        #[arg(long, default_value_t = 64)]
        wal_rotate_mb: u64,
        /// Rebuild this external Strand B directory with the new key.
        /// The previous mirror is retained as `<mirror>.pre-rotate-<stamp>`.
        #[arg(long)]
        mirror_dir: Option<String>,
    },
    /// v3.8: join a Raft cluster (3+ nodes, quorum writes, automatic
    /// leader election and failover)
    Cluster {
        /// This node's id (0..N); must be unique within --peers
        #[arg(long)]
        id: u64,
        /// All members as id=host:port, comma-separated (including self)
        #[arg(long = "peers", value_delimiter = ',')]
        peers: Vec<String>,
        #[arg(long, default_value = "./dna_cluster")]
        data_dir: String,
        #[arg(long, default_value_t = 9500)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = false)]
        no_encryption: bool,
        #[arg(long, value_enum, default_value = "batched")]
        sync: SyncArg,
    },
    /// v3.9 PITR: create a consistent physical restore point (closed WALs +
    /// segments + schema rows) under TARGET. Restore with
    /// `pitr --restore --data-dir DATA --target TARGET`.
    Pitr {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        /// Create a restore point here (mutually exclusive with --restore)
        #[arg(long)]
        target: Option<String>,
        /// Restore DATA from the restore point at --target
        #[arg(long, default_value_t = false)]
        restore: bool,
    },
    /// CMS edition: MySQL-compatible server mode. Listens on 3306 like
    /// mysqld and speaks the MySQL wire protocol (mysqli, PDO, wp-cli,
    /// mysqldump, any CMS). The simplest way to serve MySQL-based sites on
    /// DNA-DB — no drop-in files, no driver changes, just point DB_HOST at
    /// this host.
    #[cfg(feature = "cms")]
    Mysqld {
        #[arg(long, default_value = "./dna_data")]
        data_dir: String,
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = 3306)]
        port: u16,
        /// Login accepted by the wire listener. Repeatable user[:pass];
        /// default is root with no password (trust mode with a warning).
        #[arg(long = "user", value_name = "USER[:PASS]", default_value = "root")]
        users: Vec<String>,
        /// Accept ANY user/password (like --skip-grant-tables). Explicit opt-in.
        #[arg(long, default_value_t = false)]
        trust: bool,
        /// Bootstrap the WordPress core schema (12 tables) on startup.
        #[arg(long, default_value_t = false)]
        wordpress: bool,
        /// Embedded web panel (databases + spiral storage management) on
        /// this TCP port. 0 = off. Login reuses the --user credentials.
        #[arg(long, default_value_t = 0)]
        admin_port: u16,
        /// Panel bind address (default loopback; put TLS in front for
        /// remote access).
        #[arg(long, default_value = "127.0.0.1")]
        admin_bind: String,
        /// Batched WAL sync (fast dev mode). Default keeps fsync-per-write.
        #[arg(long, default_value = "strict")]
        sync: SyncArg,
    },
}

pub(crate) fn sync_arg_to_mode(mode: &SyncArg) -> SyncMode {
    mode.to_mode()
}
