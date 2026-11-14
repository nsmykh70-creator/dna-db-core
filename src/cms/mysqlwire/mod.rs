// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL wire protocol server — the universal CMS adapter (`dna-db-mysqld`).
//!
//! Instead of adapting DNA-DB to every CMS in turn, this listener speaks the
//! native MySQL client/server protocol on port 3306 so that ANY MySQL-based
//! application (WordPress, WooCommerce, Joomla, PrestaShop, phpMyAdmin,
//! wp-cli, mysqldump, ORMs, …) connects unchanged. The translation happens
//! strictly on the wire:
//!
//! ```text
//! CMS (mysqli / PDO / connector)              DNA-DB
//! ─────────────────────────────              ──────
//! MySQL text/binary protocol  ──mysqlwire──▶ CmsSqlGate (SQL ↔ DNAQL)
//! ◀── MySQL resultsets/OK/ERR ──mysqlwire──  SqlEngine / DnaQlEngine
//! ```
//!
//! Protocol coverage (protocol 4.1, the dialect every PHP driver speaks):
//!   * HandshakeV10 → HandshakeResponse41, `mysql_native_password`
//!     verification (SHA1 scramble) with transparent AuthSwitchRequest for
//!     clients configured for `caching_sha2_password` (pymysql, Connector/Py).
//!   * COM_QUERY with the full text resultset shape (column count,
//!     ColumnDefinition41, EOF, lenenc row values, NULL as 0xfb) and
//!     multi-statement packets when the client negotiates it.
//!   * COM_STMT_PREPARE / EXECUTE / CLOSE / RESET / SEND_LONG_DATA: binary
//!     parameters are decoded and substituted into the statement, which is
//!     executed through the same gate; results go back in the binary row
//!     format. Column metadata comes from a NULL-parameter probe at prepare
//!     time, so clients that bind results (mysqli, Connector/Py) see real
//!     column counts.
//!   * COM_PING / INIT_DB / QUIT / FIELD_LIST / STATISTICS / RESET_CONNECTION
//!     — everything wpdb `mysql_ping`, dbDelta and wp-cli touch.
//!   * OK/ERR/EOF packets with real MySQL errno + SQLSTATE (the CMS gate
//!     supplies the errno; WordPress branches on 1213/1062/1146).
//!
//! Everything below the protocol is the same engine the native server, the
//! admin panel and pgwire use — one storage layer, four surfaces.

use crate::cms::compat::{CmsQueryResult, CmsSqlGate, MYSQL_COMPAT_VERSION};
use crate::relational::Value;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Protocol constants (MySQL wire protocol 4.1)
// ---------------------------------------------------------------------------

/// Capability flags a modern client sends in the handshake response.
// ---------------------------------------------------------------------------
// Server + stats
// ---------------------------------------------------------------------------

/// Wire-server counters shared with the panel's `/metrics` endpoint.
/// `started` is `Instant::now()` at construction so the uptime gauge is
/// honest even for hand-built stats (tests, embedders).
pub struct MySqlWireStats {
    pub active: AtomicU32,
    pub total_queries: AtomicU64,
    pub total_sessions: AtomicU64,
    pub total_errors: AtomicU64,
    /// Process start wall-clock instant; exported as
    /// `dnadb_cms_wire_uptime_seconds` by the panel's `/metrics`.
    pub started: Instant,
}

impl Default for MySqlWireStats {
    fn default() -> Self {
        Self {
            active: AtomicU32::new(0),
            total_queries: AtomicU64::new(0),
            total_sessions: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

impl MySqlWireStats {
    /// Seconds since the listener came up (for the uptime gauge).
    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

pub struct MySqlWireServer {
    stats: Arc<MySqlWireStats>,
    auth: MySqlAuth,
    started: Instant,
    conn_counter: AtomicU32,
}

impl MySqlWireServer {
    pub fn new(auth: MySqlAuth) -> Self {
        Self {
            stats: Arc::new(MySqlWireStats::default()),
            auth,
            started: Instant::now(),
            conn_counter: AtomicU32::new(std::process::id() as u32 & 0x00ff_ffff),
        }
    }

    pub fn stats(&self) -> Arc<MySqlWireStats> {
        self.stats.clone()
    }

    /// Serve on a pre-bound listener (tests bind `127.0.0.1:0` to pick a port).
    pub fn serve(&self, listener: TcpListener, gate: Arc<CmsSqlGate>) -> std::io::Result<()> {
        let stats = self.stats.clone();
        let auth = self.auth.clone();
        let started = self.started;
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let gate = gate.clone();
                    let stats = stats.clone();
                    let auth = auth.clone();
                    let conn_id = self.conn_counter.fetch_add(1, Ordering::Relaxed);
                    let _ = std::thread::Builder::new()
                        .name("mysqlwire-conn".into())
                        .spawn(move || {
                            stats.active.fetch_add(1, Ordering::Relaxed);
                            stats.total_sessions.fetch_add(1, Ordering::Relaxed);
                            let _ = handle_connection(s, &gate, &auth, &stats, started, conn_id);
                            stats.active.fetch_sub(1, Ordering::Relaxed);
                        });
                }
                Err(e) => eprintln!("[MYSQLWIRE] accept error: {e}"),
            }
        }
        Ok(())
    }
}


mod auth;
mod conn;
mod consts;
mod packets;
mod query;
mod split;
mod stmt;
#[cfg(test)]
mod tests;
pub use auth::{MySqlAuth, native_password_token};
#[cfg(test)]
pub(crate) use auth::{check_password, verify_native_password};
pub(crate) use consts::*;
pub(crate) use conn::*;
pub(crate) use packets::*;
pub(crate) use query::*;
pub(crate) use split::*;
pub(crate) use stmt::*;
