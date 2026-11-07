// SPDX-License-Identifier: AGPL-3.0-only
//! DNA-DB Core CLI
//!
//!   dna-db-core serve   --data-dir ./dna_data --port 9999 ...
//!   dna-db-core bench   [--data-dir ./bench_tmp]
//!   dna-db-core shell   --host 127.0.0.1 --port 9999
//!   dna-db-core doctor  --data-dir ./dna_data [--fix] [--migrate] [--json]
//!   dna-db-core sql2dnaql --sql "SELECT ..." [--data-dir ./dna_data]

use clap::Parser;

mod cli;
mod cmd_maintenance;
#[cfg(feature = "cms")]
mod cmd_mysqld;
mod cmd_server;
mod cmd_shell;
mod cmd_util;

fn main() {
    let cli = cli::Cli::parse();
    let code = match &cli.command {
        cli::Command::Serve {
            data_dir,
            port,
            bind,
            mirror_dir,
            admin_user,
            admin_pass,
            no_tls,
            tls_cert,
            tls_key,
            tls_client_ca,
            no_encryption,
            encryption_key_file,
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
            trusted_proxy,
            proxy_auth_header,
            proxy_auth_secret_header,
            proxy_auth_secret,
            pgwire_port,
            mysql_port,
            mysql_users,
        } => {
            #[cfg(not(feature = "cms"))]
            let _ = (mysql_port, mysql_users);
            let admin_pass_supplied = admin_pass
                .clone()
                .or_else(|| std::env::var("DNA_DB_ADMIN_PASS").ok())
                .filter(|pass| !pass.is_empty());
            // Bootstrap credential policy (v3.5.0): the well-known
            // admin/password default is gone. Without an explicit password a
            // RANDOM one is generated, printed once and seeded with
            // must_change = true, so a fresh deployment is never reachable
            // with public credentials.
            let (admin_pass, admin_must_change) = match admin_pass_supplied {
                Some(pass) => {
                    if pass == "password" {
                        eprintln!(
                            "[SECURITY] --admin-pass is the well-known default 'password'; \
                             the bootstrap account is seeded with must_change = true. \
                             Change it immediately."
                        );
                        (pass, true)
                    } else {
                        (pass, false)
                    }
                }
                None => {
                    let generated = cmd_util::generate_bootstrap_password();
                    eprintln!(
                        "[SECURITY] no --admin-pass/DNA_DB_ADMIN_PASS supplied; \
                         generating a random bootstrap password."
                    );
                    eprintln!(
                        "[SECURITY] bootstrap admin (shown ONCE, stored only as an \
                         Argon2id hash): admin / {}",
                        generated
                    );
                    eprintln!(
                        "[SECURITY] the account is seeded with must_change = true; \
                         log in and set your own password."
                    );
                    (generated, true)
                }
            };
            let proxy_secret = proxy_auth_secret
                .clone()
                .or_else(|| std::env::var("DNA_DB_PROXY_AUTH_SECRET").ok());
            let proxy = if trusted_proxy.is_empty() {
                None
            } else {
                match cmd_util::build_proxy_auth(
                    trusted_proxy,
                    proxy_auth_header,
                    proxy_auth_secret_header,
                    proxy_secret,
                ) {
                    Ok(config) => Some(config),
                    Err(message) => {
                        eprintln!("[FATAL] invalid trusted-proxy configuration: {message}");
                        std::process::exit(2);
                    }
                }
            };
            cmd_server::run_server(
                data_dir,
                *port,
                *bind,
                mirror_dir.as_deref(),
                admin_user,
                &admin_pass,
                admin_must_change,
                cli::sync_arg_to_mode(sync),
                *memtable_mb,
                *demo_data,
                *erasure_k,
                *erasure_m,
                *no_scrub,
                *scrub_interval_secs,
                *scrub_rate_mib,
                *wal_rotate_mb,
                *admin_port,
                *no_tls,
                tls_cert.as_deref(),
                tls_key.as_deref(),
                tls_client_ca.as_deref(),
                *no_encryption,
                encryption_key_file.as_deref(),
                proxy,
                *pgwire_port,
                *mysql_port,
                mysql_users,
            )
        }
        cli::Command::Bench { data_dir, ops } => cmd_maintenance::run_bench(data_dir, *ops),
        #[cfg(feature = "cms")]
        cli::Command::Mysqld {
            data_dir,
            bind,
            port,
            users,
            trust,
            wordpress,
            admin_port,
            admin_bind,
            sync,
        } => cmd_mysqld::run_mysqld(
            data_dir,
            bind,
            *port,
            users,
            *trust,
            *wordpress,
            *admin_port,
            admin_bind,
            cli::sync_arg_to_mode(sync),
        ),
        cli::Command::Shell {
            host,
            port,
            user,
            pass,
            no_tls,
            tls_ca,
            tls_client_cert,
            tls_client_key,
        } => {
            let pass = pass
                .clone()
                .or_else(|| std::env::var("DNA_DB_ADMIN_PASS").ok())
                .unwrap_or_else(|| "password".to_string());
            cmd_shell::run_shell(
                host,
                *port,
                user,
                &pass,
                *no_tls,
                tls_ca.as_deref(),
                tls_client_cert.as_deref(),
                tls_client_key.as_deref(),
            )
        }
        cli::Command::Doctor {
            data_dir,
            fix,
            migrate,
            json,
            erasure_k,
            erasure_m,
            mirror_dir,
        } => cmd_maintenance::run_doctor(
            data_dir,
            mirror_dir.as_deref(),
            *fix,
            *migrate,
            *json,
            *erasure_k,
            *erasure_m,
        ),
        cli::Command::Sql2Dnaql { data_dir, sql } => cmd_maintenance::run_sql2dnaql(data_dir, sql.as_deref()),
        cli::Command::Compact { data_dir, json } => cmd_maintenance::run_compact(data_dir, *json),
        cli::Command::RotateKey {
            data_dir,
            old_key_file,
            new_key_file,
            memtable_mb,
            erasure_k,
            erasure_m,
            wal_rotate_mb,
            mirror_dir,
        } => cmd_maintenance::run_rotate_key(
            data_dir,
            old_key_file.as_deref(),
            new_key_file,
            *memtable_mb,
            *erasure_k,
            *erasure_m,
            *wal_rotate_mb,
            mirror_dir.as_deref(),
        ),
        cli::Command::Cluster {
            id,
            peers,
            data_dir,
            port,
            bind,
            no_encryption,
            sync,
        } => cmd_maintenance::run_cluster(
            *id,
            peers.clone(),
            data_dir.clone(),
            *port,
            bind.clone(),
            *no_encryption,
            cli::sync_arg_to_mode(sync),
        ),
        cli::Command::Pitr {
            data_dir,
            target,
            restore,
        } => cmd_maintenance::run_pitr(data_dir, target.as_deref(), *restore),
    };
    std::process::exit(code);
}
