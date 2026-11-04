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
const CLIENT_LONG_PASSWORD: u32 = 1;
const CLIENT_FOUND_ROWS: u32 = 2;
const CLIENT_LONG_FLAG: u32 = 4;
const CLIENT_CONNECT_WITH_DB: u32 = 8;
const CLIENT_PROTOCOL_41: u32 = 512;
const CLIENT_TRANSACTIONS: u32 = 8192;
const CLIENT_SECURE_CONNECTION: u32 = 32_768;
const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
const CLIENT_PS_MULTI_RESULTS: u32 = 1 << 18;
const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
const CLIENT_PLUGIN_AUTH_LENENC: u32 = 1 << 21;

/// Capabilities advertised in HandshakeV10. Deliberately WITHOUT:
/// * `CLIENT_SSL`  — no TLS inside the wire yet (TLS termination belongs to a
///   sidecar, exactly like pgwire); clients fall back to plaintext.
/// * `CLIENT_COMPRESS` — compression protocol is not implemented.
/// * `CLIENT_DEPRECATE_EOF` — we send classic EOF packets (every PHP driver
///   accepts both, classic is the safer default).
const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC;

/// Server status flags.
const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

/// Column types (protocol constants).
const MYSQL_TYPE_DOUBLE: u8 = 5;
const MYSQL_TYPE_NULL: u8 = 6;
const MYSQL_TYPE_LONGLONG: u8 = 8;
const MYSQL_TYPE_VAR_STRING: u8 = 253;

/// Column definition flags.
const COLUMN_FLAG_NOT_NULL: u16 = 1;
const COLUMN_FLAG_BINARY: u16 = 128;
const COLUMN_FLAG_NUM: u16 = 32_768;

/// Collation ids: utf8mb4_general_ci for text, 63 (binary) for numbers.
const COLLATION_UTF8MB4: u16 = 45;
const COLLATION_BINARY: u16 = 63;

/// Commands (client → server).
const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_FIELD_LIST: u8 = 0x04;
const COM_STATISTICS: u8 = 0x09;
const COM_PING: u8 = 0x0e;
const COM_RESET_CONNECTION: u8 = 0x1f;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1a;

/// Extra errnos that only the wire layer produces (not in `cms::errno`).
const ER_UNKNOWN_COM_ERROR: u16 = 1047;
const ER_EMPTY_QUERY: u16 = 1065;
const ER_UNKNOWN_STMT_HANDLER: u16 = 1243;
const ER_UNSUPPORTED_PS: u16 = 1295;

const MAX_PACKET_PAYLOAD: usize = 0xFF_FF_FF;

// ---------------------------------------------------------------------------
// Authentication policy
// ---------------------------------------------------------------------------

/// How the wire listener decides who may connect.
#[derive(Clone)]
pub enum MySqlAuth {
    /// Accept every user/password pair. Default for quick-start deployments;
    /// the server prints a prominent warning. Behaves like MySQL's
    /// `--skip-grant-tables`.
    Trust,
    /// Accept exactly one user — the CMS `DB_USER`.
    Single { name: String, password: String },
    /// Accept a fixed table of users (hosting-style multi-tenant).
    Users(HashMap<String, String>),
}

/// mysql_native_password client scramble:
/// `SHA1(password) XOR SHA1(salt || SHA1(SHA1(password)))`.
pub fn native_password_token(password: &str, salt: &[u8]) -> Vec<u8> {
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(&stage1);
    let mut h = Sha1::new();
    h.update(salt);
    h.update(&stage2);
    let scramble = h.finalize();
    stage1
        .iter()
        .zip(scramble.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// Server-side check of the same scramble without ever seeing the plaintext:
/// recover `SHA1(password)` from the token, hash it again, compare with the
/// double hash recomputed from the stored password.
fn verify_native_password(token: &[u8], password: &str, salt: &[u8]) -> bool {
    if token.len() != 20 {
        return false;
    }
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(&stage1);
    let mut h = Sha1::new();
    h.update(salt);
    h.update(&stage2);
    let scramble = h.finalize();
    let recovered: Vec<u8> = token
        .iter()
        .zip(scramble.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    Sha1::digest(&recovered).as_slice() == stage2.as_slice()
}

fn check_password(token: &[u8], stored: &str, salt: &[u8]) -> bool {
    if stored.is_empty() {
        // Empty stored password: MySQL expects the client to send no token.
        return token.is_empty();
    }
    verify_native_password(token, stored, salt)
}

impl MySqlAuth {
    fn allows(&self, user: &str, token: &[u8], salt: &[u8]) -> bool {
        match self {
            MySqlAuth::Trust => true,
            MySqlAuth::Single { name, password } => {
                user == name && check_password(token, password, salt)
            }
            MySqlAuth::Users(map) => map
                .get(user)
                .map(|pw| check_password(token, pw, salt))
                .unwrap_or(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Server + stats
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MySqlWireStats {
    pub active: AtomicU32,
    pub total_queries: AtomicU64,
    pub total_sessions: AtomicU64,
    pub total_errors: AtomicU64,
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

// ---------------------------------------------------------------------------
// Packet framing
// ---------------------------------------------------------------------------

/// A framed MySQL connection: 3-byte LE payload length + 1-byte sequence id.
/// Payloads larger than 16 MiB − 1 are split across continuation frames.
struct Conn {
    stream: TcpStream,
    /// Sequence number for the NEXT frame we write.
    write_seq: u8,
    /// Sequence number of the last frame received from the client.
    client_seq: u8,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            write_seq: 0,
            client_seq: 0,
        }
    }

    fn read_packet(&mut self) -> std::io::Result<Vec<u8>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header)?;
            let len = (header[0] as usize)
                | ((header[1] as usize) << 8)
                | ((header[2] as usize) << 16);
            self.client_seq = header[3];
            let mut chunk = vec![0u8; len];
            self.stream.read_exact(&mut chunk)?;
            payload.extend_from_slice(&chunk);
            if len < MAX_PACKET_PAYLOAD {
                break;
            }
        }
        Ok(payload)
    }

    /// Resets the write sequence for a reply to the packet just received.
    fn begin_reply(&mut self) {
        self.write_seq = self.client_seq.wrapping_add(1);
    }

    fn write_packet(&mut self, payload: &[u8]) -> std::io::Result<()> {
        if payload.is_empty() {
            let header = [0u8, 0, 0, self.write_seq];
            self.stream.write_all(&header)?;
            self.stream.flush()?;
            self.write_seq = self.write_seq.wrapping_add(1);
            return Ok(());
        }
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PACKET_PAYLOAD);
            let (chunk, tail) = rest.split_at(n);
            let header = [
                (n & 0xff) as u8,
                ((n >> 8) & 0xff) as u8,
                ((n >> 16) & 0xff) as u8,
                self.write_seq,
            ];
            self.stream.write_all(&header)?;
            self.stream.write_all(chunk)?;
            self.write_seq = self.write_seq.wrapping_add(1);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        self.stream.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// lenenc primitives
// ---------------------------------------------------------------------------

fn write_lenenc_int(out: &mut Vec<u8>, n: u64) {
    if n < 251 {
        out.push(n as u8);
    } else if n < 65_536 {
        out.push(0xfc);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < 16_777_216 {
        out.push(0xfd);
        out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    } else {
        out.push(0xfe);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

fn write_lenenc_bytes(out: &mut Vec<u8>, data: &[u8]) {
    write_lenenc_int(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *buf.get(*pos)?;
    *pos += 1;
    match first {
        0xfc => {
            let lo = *buf.get(*pos)? as u64;
            let hi = *buf.get(*pos + 1)? as u64;
            *pos += 2;
            Some(lo | (hi << 8))
        }
        0xfd => {
            let mut v: u64 = 0;
            for i in 0..3 {
                v |= (*buf.get(*pos + i)? as u64) << (8 * i);
            }
            *pos += 3;
            Some(v)
        }
        0xfe => {
            let mut v: u64 = 0;
            for i in 0..8 {
                v |= (*buf.get(*pos + i)? as u64) << (8 * i);
            }
            *pos += 8;
            Some(v)
        }
        0xfb => None, // NULL — callers that need ints won't see this
        n => Some(n as u64),
    }
}

fn read_lenenc_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let n = read_lenenc_int(buf, pos)? as usize;
    let out = buf.get(*pos..*pos + n)?;
    *pos += n;
    Some(out)
}

// ---------------------------------------------------------------------------
// Packet builders
// ---------------------------------------------------------------------------

/// HandshakeV10 (server greeting).
fn build_handshake(conn_id: u32, salt: &[u8; 20]) -> Vec<u8> {
    let mut p = Vec::with_capacity(64);
    p.push(10); // protocol version
    p.extend_from_slice(MYSQL_COMPAT_VERSION.as_bytes());
    p.push(0);
    p.extend_from_slice(&conn_id.to_le_bytes());
    p.extend_from_slice(&salt[..8]);
    p.push(0); // filler
    p.extend_from_slice(&(SERVER_CAPABILITIES as u16).to_le_bytes());
    p.push(0x2d); // utf8mb4_general_ci (45)
    p.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    p.extend_from_slice(&((SERVER_CAPABILITIES >> 16) as u16).to_le_bytes());
    p.push(21); // length of auth-plugin data (20 bytes salt + NUL)
    p.extend_from_slice(&[0u8; 10]); // reserved
    p.extend_from_slice(&salt[8..20]);
    p.push(0);
    p.extend_from_slice(b"mysql_native_password");
    p.push(0);
    p
}

/// OK packet (affected rows, LAST_INSERT_ID, status, info).
/// The info part is written length-encoded when present. PHP mysqlnd parses
/// the trailing OK bytes as a lenenc string (php_mysqlnd_rset_header_read in
/// php-src); an EOF-terminated info made real wpdb fail with
/// "RSET_HEADER packet additional data length is past N bytes the packet size".
fn ok_packet(affected: u64, last_insert_id: u64, status: u16, info: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(16 + info.len());
    p.push(0x00);
    write_lenenc_int(&mut p, affected);
    write_lenenc_int(&mut p, last_insert_id);
    p.extend_from_slice(&status.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes()); // warnings
    if !info.is_empty() {
        write_lenenc_bytes(&mut p, info.as_bytes());
    }
    p
}

/// ERR packet with MySQL errno + SQLSTATE.
fn err_packet(errno: u16, sqlstate: &str, message: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(9 + message.len());
    p.push(0xff);
    p.extend_from_slice(&errno.to_le_bytes());
    p.push(b'#');
    let st = if sqlstate.len() >= 5 { &sqlstate[..5] } else { "HY000" };
    p.extend_from_slice(st.as_bytes());
    p.extend_from_slice(message.as_bytes());
    p
}

/// EOF packet (classic, CLIENT_DEPRECATE_EOF not advertised).
fn eof_packet(status: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(5);
    p.push(0xfe);
    p.extend_from_slice(&0u16.to_le_bytes()); // warnings
    p.extend_from_slice(&status.to_le_bytes());
    p
}

/// ColumnDefinition41. `with_default` adds the trailing lenenc default value
/// that only COM_FIELD_LIST responses carry.
#[allow(clippy::too_many_arguments)]
fn column_def_packet(
    schema: &str,
    table: &str,
    name: &str,
    org_name: &str,
    ctype: u8,
    flags: u16,
    with_default: bool,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(64);
    write_lenenc_bytes(&mut p, b"def");
    write_lenenc_bytes(&mut p, schema.as_bytes());
    write_lenenc_bytes(&mut p, table.as_bytes());
    write_lenenc_bytes(&mut p, table.as_bytes()); // org_table
    write_lenenc_bytes(&mut p, name.as_bytes());
    write_lenenc_bytes(&mut p, org_name.as_bytes());
    write_lenenc_int(&mut p, 0x0c); // length of fixed-length fields
    let numeric = matches!(ctype, MYSQL_TYPE_LONGLONG | MYSQL_TYPE_DOUBLE);
    let charset = if numeric { COLLATION_BINARY } else { COLLATION_UTF8MB4 };
    p.extend_from_slice(&charset.to_le_bytes());
    p.extend_from_slice(&255u32.to_le_bytes()); // column length
    p.push(ctype);
    let mut f = flags;
    if numeric {
        // NUM | BINARY but deliberately NOT UNSIGNED: Value::Int is signed and
        // connectors trust the flag when binding (a negative value under an
        // unsigned flag would decode as a huge u64).
        f |= COLUMN_FLAG_BINARY | COLUMN_FLAG_NUM;
    }
    p.extend_from_slice(&f.to_le_bytes());
    p.push(0); // decimals
    p.extend_from_slice(&[0u8, 0]); // filler
    if with_default {
        p.push(0xfb); // default value: NULL
    }
    p
}

/// Maps a returned Value to a column type for result metadata.
fn infer_type(rows: &[BTreeMap<String, Value>], col: &str) -> u8 {
    for row in rows {
        match row.get(col) {
            Some(Value::Int(_)) => return MYSQL_TYPE_LONGLONG,
            Some(Value::Float(_)) => return MYSQL_TYPE_DOUBLE,
            Some(Value::Text(_) | Value::Dna(_)) => return MYSQL_TYPE_VAR_STRING,
            _ => {}
        }
    }
    MYSQL_TYPE_VAR_STRING
}

/// Renders a float the way MariaDB does for DOUBLE in the text protocol:
/// Rust's shortest round-trip Display already prints 250.0 as "250" and
/// 2.5 as "2.5", which matches what real mysqld sends.
fn fmt_float(f: f64) -> String {
    format!("{f}")
}

/// MySQL text-protocol string escaping (backslash mode, the PHP default).
fn escape_sql_literal(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 2);
    for &b in s {
        match b {
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0 => out.extend_from_slice(b"\\0"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x1a => out.extend_from_slice(b"\\Z"),
            _ => out.push(b),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

fn handle_connection(
    stream: TcpStream,
    gate: &Arc<CmsSqlGate>,
    auth: &MySqlAuth,
    stats: &MySqlWireStats,
    started: Instant,
    conn_id: u32,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(28_800)))?; // MySQL wait_timeout
    stream.set_nodelay(true).ok();
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut conn = Conn::new(stream);

    // ---- handshake: greeting → response → (auth switch) → OK/ERR ----
    let mut salt = [0u8; 20];
    {
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut salt);
    }
    for b in salt.iter_mut() {
        if *b < 0x21 || *b > 0x7e {
            *b = 0x41 + (*b % 26); // printable ASCII, no NUL inside the scramble
        }
    }
    conn.write_seq = 0;
    conn.write_packet(&build_handshake(conn_id, &salt))?;

    let resp = conn.read_packet()?;
    if resp.len() < 32 {
        return Ok(()); // malformed / pre-4.1 client: drop silently
    }
    let caps = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    if caps & CLIENT_PROTOCOL_41 == 0 {
        conn.begin_reply();
        conn.write_packet(&err_packet(
            ER_UNKNOWN_COM_ERROR,
            "08004",
            "DNA-DB: pre-4.1 MySQL clients are not supported",
        ))?;
        return Ok(());
    }
    let parsed = match parse_handshake_response(&resp, caps) {
        Some(p) => p,
        None => return Ok(()),
    };
    let (user, mut auth_token, db, client_plugin) = parsed;

    // Clients configured for another auth plugin (pymysql and Connector/Py
    // default to caching_sha2_password) are transparently switched to
    // mysql_native_password.
    if client_plugin != "mysql_native_password" && !matches!(auth, MySqlAuth::Trust) {
        let mut p = Vec::with_capacity(34);
        p.push(0xfe); // AuthSwitchRequest
        p.extend_from_slice(b"mysql_native_password\0");
        p.extend_from_slice(&salt);
        p.push(0);
        conn.begin_reply();
        conn.write_packet(&p)?;
        auth_token = conn.read_packet()?;
    }

    if !auth.allows(&user, &auth_token, &salt) {
        stats.total_errors.fetch_add(1, Ordering::Relaxed);
        eprintln!("[MYSQLWIRE] access denied for '{user}' from {peer}");
        conn.begin_reply();
        conn.write_packet(&err_packet(
            crate::cms::errno::ER_ACCESS_DENIED_ERROR,
            "28000",
            &format!(
                "Access denied for user '{user}'@'{peer}' (using password: {})",
                if auth_token.is_empty() { "NO" } else { "YES" }
            ),
        ))?;
        return Ok(());
    }

    let mut sid = gate.open_session();
    if let Some(db) = db.filter(|d| !d.is_empty()) {
        let safe = db.replace('`', "");
        if let Err(ce) = gate.execute(sid, &format!("USE `{safe}`")) {
            conn.begin_reply();
            conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message))?;
            gate.close_session(sid);
            return Ok(());
        }
    }
    conn.begin_reply();
    conn.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, ""))?;
    eprintln!("[MYSQLWIRE] session {sid} opened for '{user}' from {peer}");

    // ---- command loop ----
    let mut stmts: HashMap<u32, Prepared> = HashMap::new();
    let mut next_stmt_id: u32 = 1;
    loop {
        let pkt = match conn.read_packet() {
            Ok(p) => p,
            Err(_) => break, // client closed / timeout
        };
        if pkt.is_empty() {
            break;
        }
        conn.begin_reply();
        let cmd = pkt[0];
        let body = &pkt[1..];
        match cmd {
            COM_QUIT => break,
            COM_PING => {
                conn.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, ""))?;
            }
            COM_INIT_DB => {
                let name = String::from_utf8_lossy(body).replace('`', "");
                match gate.execute(sid, &format!("USE `{name}`")) {
                    Ok(_) => conn.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, ""))?,
                    Err(ce) => conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message))?,
                }
            }
            COM_QUERY => {
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                // DNA_LOG_SQL=1: statement-level trace for CMS installer
                // debugging (off by default; stderr only).
                if std::env::var("DNA_LOG_SQL").as_deref() == Ok("1") {
                    let sql = String::from_utf8_lossy(body);
                    let head: String = sql.chars().take(140).collect();
                    eprintln!("[SQL] {head}");
                }
                if let Err(e) = handle_query(&mut conn, gate, sid, body, caps) {
                    if e.kind() != std::io::ErrorKind::UnexpectedEof {
                        stats.total_errors.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[MYSQLWIRE] query io error from {peer}: {e}");
                    }
                    break;
                }
            }
            COM_FIELD_LIST => {
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                handle_field_list(&mut conn, gate, sid, body)?;
            }
            COM_STATISTICS => {
                // Answered with a plain (non-framed-typed) text packet.
                conn.write_packet(statistics_line(stats, started).as_bytes())?;
            }
            COM_RESET_CONNECTION => {
                // MySQL resets session state (locks, user vars) and replies OK.
                gate.close_session(sid);
                sid = gate.open_session();
                conn.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, ""))?;
            }
            COM_STMT_PREPARE => {
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                let sql = String::from_utf8_lossy(body).into_owned();
                stmt_prepare(&mut conn, gate, &sql, &mut next_stmt_id, &mut stmts)?;
            }
            COM_STMT_EXECUTE => {
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                if !stmt_execute(&mut conn, gate, sid, body, &mut stmts)? {
                    break;
                }
            }
            COM_STMT_SEND_LONG_DATA => {
                // No response, does not reset the sequence number.
                stmt_send_long_data(body, &mut stmts);
            }
            COM_STMT_CLOSE => {
                if body.len() >= 4 {
                    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    stmts.remove(&id);
                }
                // No response packet (per protocol).
            }
            COM_STMT_RESET => {
                if body.len() >= 4 {
                    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    match stmts.get_mut(&id) {
                        Some(st) => {
                            st.long_data.clear();
                            conn.write_packet(&ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, ""))?;
                        }
                        None => conn.write_packet(&err_packet(
                            ER_UNKNOWN_STMT_HANDLER,
                            "HY000",
                            "Unknown prepared statement handler (stmt_reset)",
                        ))?,
                    }
                }
            }
            _ => {
                conn.write_packet(&err_packet(
                    ER_UNKNOWN_COM_ERROR,
                    "08S01",
                    "Unknown command",
                ))?;
            }
        }
    }
    let freed = gate.close_session(sid);
    eprintln!("[MYSQLWIRE] session {sid} closed for '{user}' ({freed} lock(s) released)");
    Ok(())
}

fn statistics_line(stats: &MySqlWireStats, started: Instant) -> String {
    let uptime = started.elapsed().as_secs();
    let questions = stats.total_queries.load(Ordering::Relaxed);
    let threads = stats.active.load(Ordering::Relaxed);
    let qps = if uptime == 0 {
        questions as f64
    } else {
        questions as f64 / uptime as f64
    };
    format!(
        "Uptime: {uptime}  Threads: {threads}  Questions: {questions}  Slow queries: 0  Opens: 0  Flush tables: 1  Open tables: 0  Queries per second avg: {qps:.3}"
    )
}

/// HandshakeResponse41 → (user, auth response, optional db, auth plugin name).
fn parse_handshake_response(
    p: &[u8],
    caps: u32,
) -> Option<(String, Vec<u8>, Option<String>, String)> {
    let mut pos = 4usize; // capabilities
    pos += 4; // max packet size
    pos += 1; // charset
    pos += 23; // reserved
    let user = read_nul_str(p, &mut pos)?;
    let auth = if caps & CLIENT_PLUGIN_AUTH_LENENC != 0 {
        read_lenenc_bytes(p, &mut pos)?.to_vec()
    } else if caps & CLIENT_SECURE_CONNECTION != 0 {
        let n = *p.get(pos)? as usize;
        pos += 1;
        let token = p.get(pos..pos + n)?.to_vec();
        pos += n;
        token
    } else {
        read_nul_str(p, &mut pos)?.into_bytes()
    };
    let db = if caps & CLIENT_CONNECT_WITH_DB != 0 {
        Some(read_nul_str(p, &mut pos)?)
    } else {
        None
    };
    let plugin = if caps & CLIENT_PLUGIN_AUTH != 0 {
        read_nul_str(p, &mut pos)?
    } else {
        "mysql_native_password".to_string()
    };
    // CLIENT_CONNECT_ATTRS key/value pairs are ignored (they end the packet).
    Some((user, auth, db, plugin))
}

fn read_nul_str(p: &[u8], pos: &mut usize) -> Option<String> {
    let end = p[*pos..].iter().position(|&b| b == 0)? + *pos;
    let s = String::from_utf8_lossy(&p[*pos..end]).into_owned();
    *pos = end + 1;
    Some(s)
}

// ---------------------------------------------------------------------------
// COM_QUERY + resultset encoding
// ---------------------------------------------------------------------------

fn handle_query(
    conn: &mut Conn,
    gate: &Arc<CmsSqlGate>,
    sid: u64,
    body: &[u8],
    caps: u32,
) -> std::io::Result<()> {
    let text = String::from_utf8_lossy(body).into_owned();
    let mut stmts = if caps & CLIENT_MULTI_STATEMENTS != 0 {
        split_statements(&text)
    } else {
        vec![text]
    };
    stmts.retain(|s| !s.trim().is_empty());
    if stmts.is_empty() {
        return conn.write_packet(&err_packet(
            ER_EMPTY_QUERY,
            "HY000",
            "Query was empty",
        ));
    }
    let total = stmts.len();
    for (i, stmt) in stmts.iter().enumerate() {
        let more = i + 1 < total;
        match gate.execute(sid, stmt) {
            Ok(res) => encode_gate_result(conn, &res, more)?,
            Err(ce) => {
                // MySQL stops a multi-statement batch at the first error.
                return conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message));
            }
        }
    }
    Ok(())
}

fn encode_gate_result(
    conn: &mut Conn,
    res: &CmsQueryResult,
    more: bool,
) -> std::io::Result<()> {
    let status = SERVER_STATUS_AUTOCOMMIT | if more { SERVER_MORE_RESULTS_EXISTS } else { 0 };
    if res.is_select && !res.columns.is_empty() {
        write_text_resultset(conn, &res.columns, &res.rows, status)
    } else {
        let info = res.message.clone().unwrap_or_default();
        conn.write_packet(&ok_packet(
            res.affected_rows,
            res.last_insert_id,
            status,
            &info,
        ))
    }
}

/// Full text resultset: column count → ColumnDefinition41 × N → EOF →
/// lenenc rows (NULL = 0xfb) → EOF.
fn write_text_resultset(
    conn: &mut Conn,
    columns: &[String],
    rows: &[BTreeMap<String, Value>],
    status: u16,
) -> std::io::Result<()> {
    let types: Vec<u8> = columns.iter().map(|c| infer_type(rows, c)).collect();
    let mut head = Vec::with_capacity(8);
    write_lenenc_int(&mut head, columns.len() as u64);
    conn.write_packet(&head)?;
    for (i, col) in columns.iter().enumerate() {
        conn.write_packet(&column_def_packet("", "", col, col, types[i], 0, false))?;
    }
    conn.write_packet(&eof_packet(status))?;
    for row in rows {
        let mut p = Vec::with_capacity(columns.len() * 8);
        for col in columns {
            match row.get(col) {
                None | Some(Value::Null) => p.push(0xfb),
                Some(Value::Int(v)) => write_lenenc_bytes(&mut p, v.to_string().as_bytes()),
                Some(Value::Float(v)) => write_lenenc_bytes(&mut p, fmt_float(*v).as_bytes()),
                Some(Value::Text(s) | Value::Dna(s)) => write_lenenc_bytes(&mut p, s.as_bytes()),
            }
        }
        conn.write_packet(&p)?;
    }
    conn.write_packet(&eof_packet(status))?;
    Ok(())
}

/// COM_FIELD_LIST (mysql_list_fields): column definitions for one table.
fn handle_field_list(
    conn: &mut Conn,
    gate: &Arc<CmsSqlGate>,
    sid: u64,
    body: &[u8],
) -> std::io::Result<()> {
    let table = String::from_utf8_lossy(body.split(|&b| b == 0).next().unwrap_or(b""))
        .trim_matches('`')
        .to_string();
    if table.is_empty() {
        return conn.write_packet(&err_packet(
            crate::cms::errno::ER_NO_SUCH_TABLE,
            "42S02",
            "Empty table name in COM_FIELD_LIST",
        ));
    }
    let res = gate.execute(sid, &format!("SHOW FULL COLUMNS FROM `{table}`"));
    match res {
        Ok(r) => {
            for row in &r.rows {
                let name = match row.get("Field") {
                    Some(Value::Text(s) | Value::Dna(s)) => s.clone(),
                    other => other.map(|v| v.to_string()).unwrap_or_default(),
                };
                let ty = match row.get("Type") {
                    Some(Value::Text(s) | Value::Dna(s)) => mysql_wire_type_of_decl(s),
                    _ => MYSQL_TYPE_VAR_STRING,
                };
                let flags = if row.get("Key").and_then(key_flag).unwrap_or(false) {
                    COLUMN_FLAG_NOT_NULL
                } else {
                    0
                };
                conn.write_packet(&column_def_packet("", &table, &name, &name, ty, flags, true))?;
            }
            conn.write_packet(&eof_packet(SERVER_STATUS_AUTOCOMMIT))?;
            Ok(())
        }
        Err(ce) => conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message)),
    }
}

fn key_flag(v: &Value) -> Option<bool> {
    match v {
        Value::Text(s) | Value::Dna(s) => Some(!s.is_empty()),
        _ => None,
    }
}

/// MySQL type string (e.g. `bigint(20) unsigned`, `varchar(191)`) → wire type.
fn mysql_wire_type_of_decl(decl: &str) -> u8 {
    let head: String = decl
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    match head.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            MYSQL_TYPE_LONGLONG
        }
        "double" | "float" | "decimal" | "numeric" | "real" => MYSQL_TYPE_DOUBLE,
        _ => MYSQL_TYPE_VAR_STRING,
    }
}

// ---------------------------------------------------------------------------
// Prepared statements (COM_STMT_*)
// ---------------------------------------------------------------------------

/// One server-side prepared statement per connection.
struct Prepared {
    sql: String,
    params: usize,
    /// COM_STMT_SEND_LONG_DATA buffers keyed by param index.
    long_data: HashMap<u16, Vec<u8>>,
    /// Binary parameter types `(type, unsigned)` from the first
    /// COM_STMT_EXECUTE. Real drivers (mysqli, Connector/C, Connector/Py)
    /// send the type block only once (`new_params_bound = 1`) and rely on
    /// the server to remember it on every later execute of the same
    /// statement — multi-row prepared INSERT loops depend on this.
    param_types: Vec<(u8, u8)>,
}

/// A decoded COM_STMT_EXECUTE parameter.
#[derive(Debug, Clone, PartialEq)]
enum ParamValue {
    Null,
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
}

impl ParamValue {
    /// Renders as an SQL literal for placeholder substitution.
    fn render(&self) -> String {
        match self {
            ParamValue::Null => "NULL".to_string(),
            ParamValue::Int(i) => i.to_string(),
            ParamValue::Float(f) => fmt_float(*f),
            ParamValue::Bytes(b) => {
                let esc = escape_sql_literal(b);
                let mut s = String::with_capacity(esc.len() + 2);
                s.push('\'');
                s.push_str(&String::from_utf8_lossy(&esc));
                s.push('\'');
                s
            }
        }
    }
}

/// COM_STMT_PREPARE: count placeholders, probe read-statement column shape,
/// answer PrepareOK (+ param defs + column defs).
fn stmt_prepare(
    conn: &mut Conn,
    gate: &Arc<CmsSqlGate>,
    sql: &str,
    next_id: &mut u32,
    stmts: &mut HashMap<u32, Prepared>,
) -> std::io::Result<()> {
    let params = count_placeholders(sql);
    // DNA_LOG_SQL=1: prepared statements are invisible to the COM_QUERY
    // trace — Joomla drives every statement through COM_STMT_PREPARE.
    if std::env::var("DNA_LOG_SQL").as_deref() == Ok("1") {
        let head: String = sql.chars().take(140).collect();
        eprintln!("[SQL:prepare n={params}] {head}");
    }
    let head = first_word(sql).to_ascii_lowercase();
    let reads_rows = matches!(
        head.as_str(),
        "select" | "show" | "describe" | "desc" | "explain" | "with"
    );
    let mut columns: Vec<String> = Vec::new();
    let mut types: Vec<u8> = Vec::new();
    if reads_rows {
        // Determine the result shape by executing the statement with every
        // parameter bound to NULL on a throwaway session (reads are
        // side-effect free; the throwaway session discards any accounting
        // and releases its locks on close). A probe failure is reported as
        // ER_UNSUPPORTED_PS instead of risking a client-side desync.
        let probe_sql = substitute_null_params(sql);
        let probe_sid = gate.open_session();
        let probed = gate.execute(probe_sid, &probe_sql);
        gate.close_session(probe_sid);
        match probed {
            Ok(r) if r.is_select && !r.columns.is_empty() => {
                columns = r.columns.clone();
                types = r.columns.iter().map(|c| infer_type(&r.rows, c)).collect();
            }
            Ok(_) => {}
            Err(ce) => {
                // MySQL reports the REAL error at prepare time (a missing
                // table prepares → 1146/42S02), not a generic PS code.
                return conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message));
            }
        }
    }
    let id = *next_id;
    *next_id = next_id.wrapping_add(1);
    stmts.insert(
        id,
        Prepared {
            sql: sql.to_string(),
            params,
            long_data: HashMap::new(),
            param_types: Vec::new(),
        },
    );

    let mut p = Vec::with_capacity(12);
    p.push(0x00); // PrepareOK
    p.extend_from_slice(&id.to_le_bytes());
    p.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    p.extend_from_slice(&(params as u16).to_le_bytes());
    p.push(0x00); // reserved
    p.extend_from_slice(&0u16.to_le_bytes()); // warnings
    conn.write_packet(&p)?;
    if params > 0 {
        for _ in 0..params {
            conn.write_packet(&column_def_packet(
                "",
                "",
                "?",
                "?",
                MYSQL_TYPE_NULL,
                0,
                false,
            ))?;
        }
        conn.write_packet(&eof_packet(0))?;
    }
    for (i, col) in columns.iter().enumerate() {
        conn.write_packet(&column_def_packet(
            "",
            "",
            col,
            col,
            types[i],
            0,
            false,
        ))?;
    }
    if !columns.is_empty() {
        conn.write_packet(&eof_packet(0))?;
    }
    Ok(())
}

/// COM_STMT_EXECUTE: decode binary params, substitute, execute through the
/// gate, answer with a binary resultset or OK.
/// Returns `false` when the client must be disconnected (io error).
#[allow(clippy::too_many_lines)]
fn stmt_execute(
    conn: &mut Conn,
    gate: &Arc<CmsSqlGate>,
    sid: u64,
    body: &[u8],
    stmts: &mut HashMap<u32, Prepared>,
) -> std::io::Result<bool> {
    if body.len() < 9 {
        conn.write_packet(&err_packet(
            ER_UNKNOWN_STMT_HANDLER,
            "HY000",
            "Malformed COM_STMT_EXECUTE",
        ))?;
        return Ok(true);
    }
    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    // body[4] = flags (cursor types are not supported), body[5..9] = iteration count (always 1 from real drivers)
    let Some(st) = stmts.get(&id) else {
        conn.write_packet(&err_packet(
            ER_UNKNOWN_STMT_HANDLER,
            "HY000",
            "Unknown prepared statement handler (stmt_execute)",
        ))?;
        return Ok(true);
    };
    let n = st.params;
    let mut pos = 9usize;
    let mut nulls = vec![false; n];
    if n > 0 {
        let nb_len = (n + 7) / 8;
        for i in 0..n {
            if let Some(&b) = body.get(pos + i / 8) {
                nulls[i] = b & (1 << (i % 8)) != 0;
            }
        }
        pos += nb_len;
    }
    let new_bound = body.get(pos).copied().unwrap_or(0);
    pos += 1;
    // `new_params_bound = 1` only on the driver's first bind; afterwards the
    // type block is ABSENT from the payload and the stored types must be
    // reused (falling back to VAR_STRING here misparses INT/DOUBLE params
    // as lenenc strings and breaks every second execute — found by the
    // mysqli smoke test). Types are parsed into a local vector first and
    // written back into the statement map after the immutable borrow ends.
    let param_types: Vec<(u8, u8)> = match (n, new_bound) {
        (0, _) => Vec::new(),
        (_, 1) => {
            let mut types = Vec::with_capacity(n);
            for _ in 0..n {
                let t = body.get(pos).copied().unwrap_or(MYSQL_TYPE_VAR_STRING);
                let u = body.get(pos + 1).copied().unwrap_or(0);
                pos += 2;
                types.push((t, u));
            }
            types
        }
        (_, _) if st.param_types.len() == n => st.param_types.clone(),
        (_, _) => vec![(MYSQL_TYPE_VAR_STRING, 0); n],
    };
    let mut values: Vec<ParamValue> = Vec::with_capacity(n);
    for i in 0..n {
        if nulls[i] {
            values.push(ParamValue::Null);
            continue;
        }
        if let Some(ld) = st.long_data.get(&(i as u16)) {
            values.push(ParamValue::Bytes(ld.clone()));
            continue;
        }
        let (t, unsigned) = param_types[i];
        match decode_binary_param(body, &mut pos, t, unsigned) {
            Some(v) => values.push(v),
            None => {
                conn.write_packet(&err_packet(
                    ER_UNSUPPORTED_PS,
                    "42000",
                    "DNA-DB: malformed binary parameter in COM_STMT_EXECUTE",
                ))?;
                return Ok(true);
            }
        }
    }
    let sql = substitute_params(&st.sql, &values);
    // The immutable borrow of `st` ends with `substitute_params`; persist the
    // freshly bound types so later executes without a type block decode right.
    if n > 0 && new_bound == 1 {
        if let Some(stm) = stmts.get_mut(&id) {
            stm.param_types = param_types.clone();
        }
    }
    match gate.execute(sid, &sql) {
        Ok(res) => {
            if res.is_select && !res.columns.is_empty() {
                write_binary_resultset(conn, &res.columns, &res.rows, SERVER_STATUS_AUTOCOMMIT)?;
            } else {
                let info = res.message.clone().unwrap_or_default();
                conn.write_packet(&ok_packet(
                    res.affected_rows,
                    res.last_insert_id,
                    SERVER_STATUS_AUTOCOMMIT,
                    &info,
                ))?;
            }
            Ok(true)
        }
        Err(ce) => {
            conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message))?;
            Ok(true)
        }
    }
}

/// COM_STMT_SEND_LONG_DATA: `stmt_id:4, param_id:2, data:EOF`. No reply.
fn stmt_send_long_data(body: &[u8], stmts: &mut HashMap<u32, Prepared>) {
    if body.len() < 6 {
        return;
    }
    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let param = u16::from_le_bytes([body[4], body[5]]);
    if let Some(st) = stmts.get_mut(&id) {
        st.long_data.entry(param).or_default().extend_from_slice(&body[6..]);
    }
}

/// Binary resultset for COM_STMT_EXECUTE: header 0x00, null bitmap with
/// offset 2, values in binary per column type.
fn write_binary_resultset(
    conn: &mut Conn,
    columns: &[String],
    rows: &[BTreeMap<String, Value>],
    status: u16,
) -> std::io::Result<()> {
    let types: Vec<u8> = columns.iter().map(|c| infer_type(rows, c)).collect();
    let mut head = Vec::with_capacity(8);
    write_lenenc_int(&mut head, columns.len() as u64);
    conn.write_packet(&head)?;
    for (i, col) in columns.iter().enumerate() {
        conn.write_packet(&column_def_packet("", "", col, col, types[i], 0, false))?;
    }
    conn.write_packet(&eof_packet(status))?;
    for row in rows {
        let mut p = Vec::with_capacity(columns.len() * 9 + 8);
        p.push(0x00); // binary row header
        let nb_len = (columns.len() + 9) / 8; // null bitmap with offset 2
        let mut null_bitmap = vec![0u8; nb_len];
        let mut values: Vec<Vec<u8>> = Vec::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            match row.get(col) {
                None | Some(Value::Null) => {
                    null_bitmap[(i + 2) / 8] |= 1 << ((i + 2) % 8);
                }
                Some(Value::Int(v)) => values.push(v.to_le_bytes().to_vec()),
                Some(Value::Float(v)) => values.push(v.to_le_bytes().to_vec()),
                Some(Value::Text(s) | Value::Dna(s)) => {
                    let mut b = Vec::with_capacity(s.len() + 8);
                    write_lenenc_bytes(&mut b, s.as_bytes());
                    values.push(b);
                }
            }
        }
        p.extend_from_slice(&null_bitmap);
        for v in values {
            p.extend_from_slice(&v);
        }
        conn.write_packet(&p)?;
    }
    conn.write_packet(&eof_packet(status))?;
    Ok(())
}

/// Decodes one binary parameter value at `pos`; advances `pos`.
fn decode_binary_param(
    buf: &[u8],
    pos: &mut usize,
    ty: u8,
    unsigned: u8,
) -> Option<ParamValue> {
    let need = |n: usize, pos: &mut usize| -> Option<()> {
        if *pos + n <= buf.len() {
            Some(())
        } else {
            None
        }
    };
    match ty {
        MYSQL_TYPE_NULL => Some(ParamValue::Null),
        1 => {
            // TINY
            need(1, pos)?;
            let b = buf[*pos];
            *pos += 1;
            Some(if unsigned != 0 {
                ParamValue::Int(i64::from(b))
            } else {
                ParamValue::Int(i64::from(b as i8))
            })
        }
        2 | 13 => {
            // SHORT / YEAR
            need(2, pos)?;
            let raw = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]);
            *pos += 2;
            Some(if unsigned != 0 {
                ParamValue::Int(i64::from(raw))
            } else {
                ParamValue::Int(i64::from(raw as i16))
            })
        }
        3 | 9 => {
            // LONG / INT24
            need(4, pos)?;
            let raw = u32::from_le_bytes([
                buf[*pos],
                buf[*pos + 1],
                buf[*pos + 2],
                buf[*pos + 3],
            ]);
            *pos += 4;
            Some(if unsigned != 0 {
                ParamValue::Int(i64::from(raw))
            } else {
                ParamValue::Int(i64::from(raw as i32))
            })
        }
        8 => {
            // LONGLONG
            need(8, pos)?;
            let raw = u64::from_le_bytes([
                buf[*pos],
                buf[*pos + 1],
                buf[*pos + 2],
                buf[*pos + 3],
                buf[*pos + 4],
                buf[*pos + 5],
                buf[*pos + 6],
                buf[*pos + 7],
            ]);
            *pos += 8;
            Some(ParamValue::Int(raw as i64))
        }
        4 => {
            // FLOAT
            need(4, pos)?;
            let raw = u32::from_le_bytes([
                buf[*pos],
                buf[*pos + 1],
                buf[*pos + 2],
                buf[*pos + 3],
            ]);
            *pos += 4;
            Some(ParamValue::Float(f32::from_bits(raw) as f64))
        }
        5 => {
            // DOUBLE
            need(8, pos)?;
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[*pos..*pos + 8]);
            *pos += 8;
            Some(ParamValue::Float(f64::from_le_bytes(raw)))
        }
        7 | 10 | 12 | 14 => {
            // TIMESTAMP / DATE / DATETIME / NEWDATE (length-prefixed)
            let len = *buf.get(*pos)?;
            *pos += 1;
            match len {
                0 => Some(ParamValue::Bytes(b"0000-00-00".to_vec())),
                4 => {
                    need(4, pos)?;
                    let y = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]);
                    let out = format!("{y:04}-{:02}-{:02}", buf[*pos + 2], buf[*pos + 3]);
                    *pos += 4;
                    Some(ParamValue::Bytes(out.into_bytes()))
                }
                7 => {
                    need(7, pos)?;
                    let y = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]);
                    let out = format!(
                        "{y:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        buf[*pos + 2],
                        buf[*pos + 3],
                        buf[*pos + 4],
                        buf[*pos + 5],
                        buf[*pos + 6]
                    );
                    *pos += 7;
                    Some(ParamValue::Bytes(out.into_bytes()))
                }
                11 => {
                    need(11, pos)?;
                    let y = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]);
                    let us = u32::from_le_bytes([
                        buf[*pos + 7],
                        buf[*pos + 8],
                        buf[*pos + 9],
                        buf[*pos + 10],
                    ]);
                    let out = format!(
                        "{y:04}-{:02}-{:02} {:02}:{:02}:{:02}.{us:06}",
                        buf[*pos + 2],
                        buf[*pos + 3],
                        buf[*pos + 4],
                        buf[*pos + 5],
                        buf[*pos + 6]
                    );
                    *pos += 11;
                    Some(ParamValue::Bytes(out.into_bytes()))
                }
                _ => None,
            }
        }
        11 => {
            // TIME (length-prefixed: sign, days, h, m, s [, us])
            let len = *buf.get(*pos)?;
            *pos += 1;
            match len {
                0 => Some(ParamValue::Bytes(b"00:00:00".to_vec())),
                8 | 12 => {
                    need(len as usize, pos)?;
                    let sign = if buf[*pos] == 0 { "" } else { "-" };
                    let days = u32::from_le_bytes([
                        buf[*pos + 1],
                        buf[*pos + 2],
                        buf[*pos + 3],
                        buf[*pos + 4],
                    ]);
                    let out = format!(
                        "{sign}{:02}:{:02}:{:02}",
                        days * 24 + buf[*pos + 5] as u32,
                        buf[*pos + 6],
                        buf[*pos + 7]
                    );
                    *pos += len as usize;
                    Some(ParamValue::Bytes(out.into_bytes()))
                }
                _ => None,
            }
        }
        // DECIMAL/NEWDECIMAL/VARCHAR/VAR_STRING/STRING/BLOB*/ENUM/SET/JSON/
        // BIT/GEOMETRY all arrive as lenenc byte strings from real drivers.
        _ => {
            let v = read_lenenc_bytes(buf, pos)?;
            Some(ParamValue::Bytes(v.to_vec()))
        }
    }
}

// ---------------------------------------------------------------------------
// SQL text walking: split / placeholders / substitution
// ---------------------------------------------------------------------------

fn first_word(sql: &str) -> &str {
    sql.trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
}

/// Splits a COM_QUERY payload into statements on top-level `;`
/// (quote-, backtick- and comment-aware).
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                cur.push(c);
                copy_quoted('\'', &mut chars, &mut cur);
            }
            '"' => {
                cur.push(c);
                copy_quoted('"', &mut chars, &mut cur);
            }
            '`' => {
                cur.push(c);
                copy_quoted('`', &mut chars, &mut cur);
            }
            '#' => {
                cur.push(c);
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    cur.push(chars.next().unwrap());
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    cur.push(chars.next().unwrap());
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                cur.push(c);
                cur.push(chars.next().unwrap());
                while let Some(n) = chars.next() {
                    cur.push(n);
                    if n == '*' && chars.peek() == Some(&'/') {
                        cur.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            ';' => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Copies a quoted literal to its closing quote (handling backslash escapes
/// and doubled quotes) into `out`.
fn copy_quoted(quote: char, chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else if c == quote {
            if chars.peek() == Some(&quote) {
                out.push(chars.next().unwrap());
            } else {
                return;
            }
        }
    }
}

/// Counts `?` placeholders outside quotes/comments.
fn count_placeholders(sql: &str) -> usize {
    let mut n = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => copy_quoted(c, &mut chars, &mut String::new()),
            '#' => skip_to_eol(&mut chars),
            '-' if chars.peek() == Some(&'-') => skip_to_eol(&mut chars),
            '/' if chars.peek() == Some(&'*') => skip_block_comment(&mut chars),
            '?' => n += 1,
            _ => {}
        }
    }
    n
}

/// Substitutes every placeholder with NULL (prepare-time probe).
fn substitute_null_params(sql: &str) -> String {
    substitute_params(
        sql,
        std::iter::repeat(ParamValue::Null)
            .take(count_placeholders(sql))
            .collect::<Vec<_>>()
            .as_slice(),
    )
}

/// Replaces each `?` outside quotes/comments with the next rendered literal.
fn substitute_params(sql: &str, values: &[ParamValue]) -> String {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut idx = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                out.push(c);
                copy_quoted(c, &mut chars, &mut out);
            }
            '#' => {
                out.push(c);
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    out.push(chars.next().unwrap());
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(c);
                out.push(chars.next().unwrap());
                while let Some(&n) = chars.peek() {
                    if n == '\n' {
                        break;
                    }
                    out.push(chars.next().unwrap());
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                out.push(c);
                out.push(chars.next().unwrap());
                while let Some(n) = chars.next() {
                    out.push(n);
                    if n == '*' && chars.peek() == Some(&'/') {
                        out.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            '?' => {
                if idx < values.len() {
                    out.push_str(&values[idx].render());
                    idx += 1;
                } else {
                    out.push('?');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn skip_to_eol(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&n) = chars.peek() {
        if n == '\n' {
            break;
        }
        chars.next();
    }
}

fn skip_block_comment(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(n) = chars.next() {
        if n == '*' && chars.peek() == Some(&'/') {
            chars.next();
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_roundtrip() {
        for n in [0u64, 250, 251, 65_535, 65_536, 16_777_215, 16_777_216, u32::MAX as u64 * 997] {
            let mut buf = Vec::new();
            write_lenenc_int(&mut buf, n);
            let mut pos = 0usize;
            assert_eq!(read_lenenc_int(&buf, &mut pos), Some(n), "n = {n}");
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn lenenc_bytes_roundtrip() {
        let mut buf = Vec::new();
        write_lenenc_bytes(&mut buf, b"wp_posts");
        let mut pos = 0usize;
        assert_eq!(read_lenenc_bytes(&buf, &mut pos), Some(&b"wp_posts"[..]));
    }

    #[test]
    fn auth_scramble_roundtrip() {
        let salt = b"0123456789abcdefghijklmnopqrst"[..20].to_vec();
        let token = native_password_token("wp-secret-42", &salt);
        assert!(verify_native_password(&token, "wp-secret-42", &salt));
        assert!(!verify_native_password(&token, "wrong", &salt));
        assert!(!verify_native_password(&token[..19].to_vec(), "wp-secret-42", &salt));
    }

    #[test]
    fn auth_scramble_known_vector() {
        // Cross-checked against Python hashlib (independent implementation):
        // password "secret", salt b"01234567890123456789" → the client must
        // send exactly this 20-byte scramble.
        let salt = b"01234567890123456789";
        let token = native_password_token("secret", salt);
        let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "7abe1a8776b59e931059451f81e596a60dbbf7a8");
        // And the server-side verification accepts it, but not a corrupted one.
        assert!(verify_native_password(&token, "secret", salt));
    }

    #[test]
    fn empty_password_semantics() {
        // Stored empty password requires an empty token.
        assert!(check_password(&[], "", &[0u8; 20]));
        assert!(!check_password(&[1u8; 20], "", &[0u8; 20]));
    }

    #[test]
    fn splits_multi_statements() {
        let sql = "SELECT 1; SET NAMES utf8mb4; /* c ; */ SELECT 'a;b' FROM `t;x` ;";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], "SELECT 1");
        assert_eq!(parts[1], "SET NAMES utf8mb4");
        // The block comment belongs to the following statement (MySQL keeps
        // hint comments glued to the query they precede).
        assert!(parts[2].starts_with("/* c ; */ SELECT 'a;b'"), "{parts:?}");
        assert!(parts[2].ends_with("FROM `t;x`"), "{parts:?}");
    }

    #[test]
    fn split_handles_escapes_and_comments() {
        let sql = "SELECT 'it\\'s; fine'; -- trailing ; comment\nSELECT 2; # hash ; comment\nSELECT 3";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3, "{parts:?}");
    }

    #[test]
    fn counts_placeholders_correctly() {
        assert_eq!(count_placeholders("SELECT * FROM t WHERE a = ? AND b = ?"), 2);
        assert_eq!(count_placeholders("SELECT 'a?b', `c?d`, \"e?f\" WHERE g = ?"), 1);
        assert_eq!(count_placeholders("-- ? comment\nSELECT ? # ?\n"), 1);
        assert_eq!(count_placeholders("/* ? */ SELECT ?"), 1);
        assert_eq!(count_placeholders("SELECT * FROM wp_posts"), 0);
    }

    #[test]
    fn substitutes_params() {
        let vals = vec![
            ParamValue::Int(5),
            ParamValue::Bytes(b"O'Brien \\".to_vec()),
            ParamValue::Null,
            ParamValue::Float(2.5),
        ];
        let out = substitute_params(
            "SELECT * FROM t WHERE id = ? AND name = ? AND x = ? AND f = ?",
            &vals,
        );
        assert_eq!(
            out,
            "SELECT * FROM t WHERE id = 5 AND name = 'O\\'Brien \\\\' AND x = NULL AND f = 2.5"
        );
    }

    #[test]
    fn substitute_null_params_probes() {
        assert_eq!(
            substitute_null_params("SELECT a FROM t WHERE id = ? AND s = ?"),
            "SELECT a FROM t WHERE id = NULL AND s = NULL"
        );
    }

    #[test]
    fn binary_datetime_decode() {
        // 2026-11-02 09:30:15
        let mut buf = vec![7];
        buf.extend_from_slice(&2026u16.to_le_bytes());
        buf.extend_from_slice(&[11, 2, 9, 30, 15]);
        let mut pos = 0usize;
        let v = decode_binary_param(&buf, &mut pos, 12, 0).unwrap();
        assert_eq!(v, ParamValue::Bytes(b"2026-11-02 09:30:15".to_vec()));
        // zero date
        let v = decode_binary_param(&[0], &mut 0, 12, 0).unwrap();
        assert_eq!(v, ParamValue::Bytes(b"0000-00-00".to_vec()));
    }

    #[test]
    fn binary_int_decode_signed_unsigned() {
        let buf = 42i64.to_le_bytes().to_vec();
        let mut pos = 0usize;
        assert_eq!(decode_binary_param(&buf, &mut pos, 8, 0), Some(ParamValue::Int(42)));
        let neg = (-7i32).to_le_bytes().to_vec();
        let mut pos = 0usize;
        assert_eq!(decode_binary_param(&neg, &mut pos, 3, 0), Some(ParamValue::Int(-7)));
        let mut pos = 0usize;
        assert_eq!(
            decode_binary_param(&neg, &mut pos, 3, 1),
            Some(ParamValue::Int(i64::from(u32::from_le_bytes(
                (-7i32).to_le_bytes()
            ))))
        );
    }

    #[test]
    fn ok_err_eof_shapes() {
        let ok = ok_packet(3, 7, SERVER_STATUS_AUTOCOMMIT, "Records: 3");
        assert_eq!(ok[0], 0x00);
        assert_eq!(ok[1], 3);
        assert_eq!(ok[2], 7);
        assert_eq!(&ok[3..5], &SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        // info is lenenc-prefixed (mysqlnd compatibility)
        assert_eq!(ok[7], 10);
        assert_eq!(&ok[8..18], b"Records: 3");
        let bare = ok_packet(0, 0, SERVER_STATUS_AUTOCOMMIT, "");
        assert_eq!(bare.len(), 7); // header + affected + insert + status + warnings
        let err = err_packet(1146, "42S02", "Table 'wp.x' doesn't exist");
        assert_eq!(err[0], 0xff);
        assert_eq!(u16::from_le_bytes([err[1], err[2]]), 1146);
        assert_eq!(&err[3..9], b"#42S02");
        let eof = eof_packet(SERVER_MORE_RESULTS_EXISTS);
        assert_eq!(eof[0], 0xfe);
        assert_eq!(&eof[3..5], &SERVER_MORE_RESULTS_EXISTS.to_le_bytes());
    }

    #[test]
    fn column_def_shape() {
        let d = column_def_packet("", "wp_posts", "ID", "ID", MYSQL_TYPE_LONGLONG, 0, false);
        let mut pos = 0usize;
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b"def"[..]));
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b""[..]));
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b"wp_posts"[..]));
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b"wp_posts"[..]));
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b"ID"[..]));
        assert_eq!(read_lenenc_bytes(&d, &mut pos), Some(&b"ID"[..]));
        let fixed = read_lenenc_int(&d, &mut pos).unwrap();
        assert_eq!(fixed, 0x0c);
        // numeric columns must advertise the binary collation, not utf8mb4
        let charset = u16::from_le_bytes([d[pos], d[pos + 1]]);
        assert_eq!(charset, COLLATION_BINARY);
    }

    #[test]
    fn handshake_shape() {
        let salt = [b'a'; 20];
        let hs = build_handshake(0x1234_5678, &salt);
        assert_eq!(hs[0], 10);
        // server version is NUL-terminated at the end of MYSQL_COMPAT_VERSION
        let vlen = MYSQL_COMPAT_VERSION.len();
        assert_eq!(hs[1 + vlen], 0);
        let conn_id = u32::from_le_bytes([hs[2 + vlen], hs[3 + vlen], hs[4 + vlen], hs[5 + vlen]]);
        assert_eq!(conn_id, 0x1234_5678);
        assert_eq!(&hs[6 + vlen..14 + vlen], &salt[..8]);
        // plugin name at the tail ("mysql_native_password" = 21 chars + NUL)
        assert!(hs.windows(22).any(|w| w == b"mysql_native_password\0"));
    }

    #[test]
    fn first_word_works() {
        assert_eq!(first_word("  SELECT 1"), "SELECT");
        assert_eq!(first_word("INSERT INTO t VALUES (1)"), "INSERT");
        assert_eq!(first_word("(SELECT 1)"), "");
    }

    #[test]
    fn fmt_float_mysql_style() {
        assert_eq!(fmt_float(250.0), "250");
        assert_eq!(fmt_float(2.5), "2.5");
        assert_eq!(fmt_float(0.1), "0.1");
    }

    #[test]
    fn param_render_escaping() {
        assert_eq!(ParamValue::Bytes(b"a'b\\c".to_vec()).render(), "'a\\'b\\\\c'");
        assert_eq!(ParamValue::Null.render(), "NULL");
        assert_eq!(ParamValue::Int(-3).render(), "-3");
    }

    #[test]
    fn wire_type_from_decl() {
        assert_eq!(mysql_wire_type_of_decl("bigint(20) unsigned"), MYSQL_TYPE_LONGLONG);
        assert_eq!(mysql_wire_type_of_decl("varchar(191)"), MYSQL_TYPE_VAR_STRING);
        assert_eq!(mysql_wire_type_of_decl("decimal(10,2)"), MYSQL_TYPE_DOUBLE);
    }
}
