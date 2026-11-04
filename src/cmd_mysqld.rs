// SPDX-License-Identifier: AGPL-3.0-only

//! CMS edition `mysqld` mode: the minimal MySQL-compatible server (compiled
//! only with the `cms` feature).

#[cfg(feature = "cms")]
use std::sync::Arc;

#[cfg(feature = "cms")]
use crate::cmd_util::acquire_dir_lock;

/// CMS edition `mysqld` mode: the minimal MySQL-compatible server.
///
/// Deliberately has NONE of the full server's ceremony (no bootstrap admin
/// password, no admin panel, no TLS material): one flag set, one listener on
/// 3306, ready for WordPress/WooCommerce/Joomla `DB_HOST`. Storage keeps the
/// production defaults (Strict fsync, Reed–Solomon RS(4+2), scrubber on);
/// encryption at rest is inherited from the data directory key if present.
#[cfg(feature = "cms")]
pub(crate) fn run_mysqld(
    data_dir: &str,
    bind: &str,
    port: u16,
    users: &[String],
    trust: bool,
    wordpress: bool,
    sync_mode: dna_db_core::wal::SyncMode,
) -> i32 {
    use dna_db_core::cms::compat::{CmsSqlGate, MYSQL_COMPAT_VERSION};
    use dna_db_core::cms::{MySqlAuth, MySqlWireServer};
    use dna_db_core::sql_engine::SqlEngine;

    let data_path = std::path::Path::new(data_dir);
    if let Err(e) = std::fs::create_dir_all(data_path) {
        eprintln!("[FATAL] cannot create data dir {data_dir:?}: {e}");
        return 2;
    }
    let _dir_lock = match acquire_dir_lock(data_path, "mysqld") {
        Ok(lock) => lock,
        Err(message) => {
            eprintln!("[FATAL] {message}");
            return 2;
        }
    };

    let engine = match dna_db_core::Engine::open(dna_db_core::EngineConfig {
        dir_path: data_path.to_path_buf(),
        sync_mode,
        ..dna_db_core::EngineConfig::default()
    }) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("[FATAL] cannot open data dir {data_dir:?}: {e}");
            return 2;
        }
    };
    let sql = Arc::new(SqlEngine::new(engine));
    let gate = Arc::new(CmsSqlGate::new(sql));

    eprintln!("[MYSQLD] {}", dna_db_core::cms::edition_banner());
    eprintln!("[MYSQLD] server version {MYSQL_COMPAT_VERSION} (MySQL wire protocol 4.1)");
    eprintln!("[MYSQLD] data directory {data_dir:?}");

    if wordpress {
        let total = dna_db_core::cms::wordpress::WP_CORE_TABLES.len();
        match gate.bootstrap_wordpress() {
            Ok(created) => {
                eprintln!("[MYSQLD] WordPress core schema ready ({created} table(s) created, {total} core tables registered)");
            }
            Err(e) => {
                eprintln!("[FATAL] WordPress bootstrap failed: {e}");
                return 2;
            }
        }
    }

    let auth = if trust || users.is_empty() {
        eprintln!(
            "[MYSQLD] WARNING: accepting ANY login (--skip-grant-tables). \
             Set --user root:secret (repeatable) to require a password."
        );
        MySqlAuth::Trust
    } else {
        let mut map = std::collections::HashMap::new();
        for spec in users {
            match spec.split_once(':') {
                Some((u, p)) => {
                    map.insert(u.to_string(), p.to_string());
                }
                None => {
                    map.insert(spec.clone(), String::new());
                }
            }
        }
        let names: Vec<String> = map.keys().cloned().collect();
        eprintln!("[MYSQLD] auth: users {names:?} (mysql_native_password)");
        MySqlAuth::Users(map)
    };

    let listener = match std::net::TcpListener::bind((bind, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[FATAL] cannot bind {bind}:{port}: {e}");
            return 2;
        }
    };
    let user_hint = if trust || users.is_empty() {
        "anything".to_string()
    } else {
        users
            .first()
            .and_then(|u| u.split(':').next())
            .unwrap_or("root")
            .to_string()
    };
    eprintln!("[MYSQLD] ready for connections on {bind}:{port}");
    eprintln!("[MYSQLD] ---- wp-config.php ----");
    eprintln!("define( 'DB_NAME', 'wordpress' );");
    eprintln!("define( 'DB_USER', '{user_hint}' );");
    eprintln!(
        "define( 'DB_PASSWORD', '{}' );",
        if trust || users.is_empty() { "anything" } else { "***" }
    );
    eprintln!(
        "define( 'DB_HOST', '{}:{port}' );",
        if bind == "0.0.0.0" { "127.0.0.1" } else { bind }
    );
    eprintln!("[MYSQLD] -----------------------");

    let server = Arc::new(MySqlWireServer::new(auth));
    if let Err(e) = server.serve(listener, gate) {
        eprintln!("[FATAL] MySQL listener stopped: {e}");
        return 2;
    }
    0
}
