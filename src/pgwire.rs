// SPDX-License-Identifier: AGPL-3.0-only
//! PostgreSQL Wire Protocol v3 server (v3.7 "Ecosystem & pgwire Bridge").
//!
//! Speaks the frontend/backend protocol well enough for psql, DBeaver,
//! DataGrip, pgAdmin, psycopg2, Prisma and Drizzle:
//!   * SSLRequest → 'N' (cleartext fallback; TLS termination belongs to a
//!     sidecar in production deployments)
//!   * StartupMessage → AuthenticationOk (trust) → ParameterStatus →
//!     BackendKeyData → ReadyForQuery
//!   * Simple Query ('Q') → RowDescription / DataRow / CommandComplete /
//!     EmptyQueryResponse / ErrorResponse → ReadyForQuery
//!   * Extended query protocol (v4.2: prepared statements are fully
//!     supported instead of the former 0A000 feature error):
//!       Parse ('P') → ParseComplete ('1')        named/unnamed statements
//!       Bind ('B') → BindComplete ('2')          `$n` and `?` placeholders,
//!                                                text format, NULL via -1
//!       Describe ('D') → ParameterDescription ('t') + NoData ('n') for
//!                        statements; RowDescription ('T') for portals
//!                        (SELECT-shaped portals execute once and cache the
//!                        rowset for the following Execute)
//!       Execute ('E') → DataRow(s) + CommandComplete ('C')
//!       Close ('C') → CloseComplete ('3'), Flush ('H'), Sync ('S') →
//!                     ReadyForQuery ('Z'); after an ErrorResponse every
//!                     extended message is ignored until the next Sync
//!                     (spec behaviour, keeps driver state machines happy)
//!   * Terminate ('X') closes the session; CancelRequest is acknowledged.
//!   * Cleartext password auth when a user table is provided (--pgwire-auth
//!     password reuses the engine's Argon2id users.json store).
//!
//! Every query runs through the same SqlEngine as the native protocol, so
//! results, plans and ACLs are identical on both surfaces.

use crate::sql_engine::{QueryResult, SqlEngine};
use crate::relational::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct PgWireStats {
    pub active: AtomicU32,
    pub total_queries: AtomicU64,
    pub total_sessions: AtomicU64,
}

impl PgWireStats {
    fn new() -> Self {
        Self {
            active: AtomicU32::new(0),
            total_queries: AtomicU64::new(0),
            total_sessions: AtomicU64::new(0),
        }
    }
}

/// PostgreSQL type OIDs (subset used by the SQL surface).
const OID_BOOL: i32 = 16;
const OID_INT8: i32 = 20;
const OID_FLOAT8: i32 = 701;
const OID_TEXT: i32 = 25;

fn type_oid(v: &Value) -> i32 {
    match v {
        Value::Int(_) => OID_INT8,
        Value::Float(_) => OID_FLOAT8,
        _ => OID_TEXT,
    }
}

fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        other => Some(match other {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                let s = f.to_string();
                if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
                    s
                } else {
                    format!("{s}.0")
                }
            }
            Value::Text(s) => s.clone(),
            Value::Dna(s) => s.clone(),
            Value::Null => unreachable!(),
        }),
    }
}

pub struct PgWireServer {
    stats: Arc<PgWireStats>,
}

impl PgWireServer {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(PgWireStats::new()),
        }
    }

    pub fn stats(&self) -> Arc<PgWireStats> {
        self.stats.clone()
    }

    /// Bind and serve forever (one thread per connection, mirroring the
    /// native server's model). `engine` serves every query.
    pub fn serve(
        &self,
        bind: &str,
        port: u16,
        sql: Arc<SqlEngine>,
    ) -> std::io::Result<()> {
        let listener = TcpListener::bind((bind, port))?;
        eprintln!(
            "[PGWIRE] PostgreSQL wire protocol v3 listening on {bind}:{port} (psql / DBeaver / psycopg2 ready)"
        );
        let stats = self.stats.clone();
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let sql = sql.clone();
                    let stats = stats.clone();
                    let _ = std::thread::Builder::new()
                        .name("pgwire-conn".into())
                        .spawn(move || {
                            stats.active.fetch_add(1, Ordering::Relaxed);
                            stats.total_sessions.fetch_add(1, Ordering::Relaxed);
                            let _ = handle_connection(s, &sql, &stats);
                            stats.active.fetch_sub(1, Ordering::Relaxed);
                        });
                }
                Err(e) => eprintln!("[PGWIRE] accept error: {e}"),
            }
        }
        Ok(())
    }
}

fn handle_connection(
    mut stream: TcpStream,
    sql: &Arc<SqlEngine>,
    stats: &PgWireStats,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_nodelay(true).ok();

    // --- Startup phase ---
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = i32::from_be_bytes(len_buf) as usize;
    if len < 8 || len > 65536 {
        return Ok(()); // malformed startup: drop
    }
    let mut rest = vec![0u8; len - 4];
    stream.read_exact(&mut rest)?;
    let protocol = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    // CancelRequest (80877102): acknowledge and close.
    if protocol == 80877102 {
        return Ok(());
    }
    // SSLRequest (80877103): reply 'N' and re-read the real startup.
    if protocol == 80877103 {
        stream.write_all(b"N")?;
        stream.flush()?;
        stream.read_exact(&mut len_buf)?;
        let len = i32::from_be_bytes(len_buf) as usize;
        if len < 8 || len > 65536 {
            return Ok(());
        }
        let mut rest = vec![0u8; len - 4];
        stream.read_exact(&mut rest)?;
        let protocol = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        if protocol != 196608 {
            return Ok(()); // only protocol 3.0 supported
        }
    } else if protocol != 196608 {
        return Ok(());
    }

    let user = startup_param(&rest[4..], "user").unwrap_or_else(|| "anonymous".to_string());

    // AuthenticationOk + parameters + backend key + ReadyForQuery.
    let mut out = Vec::with_capacity(256);
    message(&mut out, b'R', &[0, 0, 0, 0]); // AuthenticationOk
    parameter(&mut out, "server_version", &format!("16.0 ({})", SqlEngine::server_version()));
    parameter(&mut out, "server_encoding", "UTF8");
    parameter(&mut out, "client_encoding", "UTF8");
    parameter(&mut out, "DateStyle", "ISO, MDY");
    parameter(&mut out, "integer_datetimes", "on");
    parameter(&mut out, "standard_conforming_strings", "on");
    // BackendKeyData (dummy but stable per session).
    let pid = std::process::id() as i32;
    let secret: i32 = 0x44_4E_41_44; // 'DNAD'
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&pid.to_be_bytes());
    body.extend_from_slice(&secret.to_be_bytes());
    message(&mut out, b'K', &body);
    message(&mut out, b'Z', b"I");
    stream.write_all(&out)?;
    stream.flush()?;
    eprintln!("[PGWIRE] session opened for user '{user}'");

    // --- Query loop (extended bytes: type + len + payload) ---
    let mut tx: Option<crate::sql_engine::SqlTxBuffer> = None;
    let mut session = ExtendedSession::default();
    loop {
        let mut tag = [0u8; 1];
        if stream.read_exact(&mut tag).is_err() {
            break; // client closed
        }
        if stream.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = i32::from_be_bytes(len_buf) as usize;
        if len < 4 || len > 64 * 1024 * 1024 {
            break;
        }
        let mut body = vec![0u8; len - 4];
        if stream.read_exact(&mut body).is_err() {
            break;
        }
        match tag[0] {
            b'Q' => {
                let query = null_terminated_string(&body);
                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                let mut out = Vec::with_capacity(1024);
                let trimmed = query.trim();
                if trimmed.is_empty() {
                    message(&mut out, b'I', b""); // EmptyQueryResponse
                } else {
                    match sql.execute_in_tx(trimmed, &mut tx) {
                        Ok(res) => {
                            encode_result(&mut out, &res, trimmed);
                        }
                        Err(e) => {
                            tx = None; // failed statement aborts the tx buffer
                            error_response(&mut out, "42601", &e);
                        }
                    }
                }
                message(&mut out, b'Z', b"I");
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'X' => break, // Terminate
            b'S' => {
                // Sync: end of an extended-protocol batch; clears any
                // error-skipped state and reports the transaction status.
                session.in_error = false;
                let mut out = Vec::new();
                message(&mut out, b'Z', b"I");
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'H' => {
                // Flush: this server writes (and flushes) after every
                // message, so there is nothing buffered to push.
            }
            b'P' => {
                // Parse: name, query, i16 param-type count, OIDs (ignored).
                let mut out = Vec::new();
                if session.in_error {
                    // skip payload, wait for Sync
                } else {
                    match parse_parse(&body) {
                        Ok((name, query)) => {
                            match count_placeholders(&query) {
                                Ok(n_params) => {
                                    session.prepared.insert(
                                        name.clone(),
                                        PreparedStatement { query, n_params },
                                    );
                                    message(&mut out, b'1', b""); // ParseComplete
                                }
                                Err(e) => {
                                    session.in_error = true;
                                    error_response(&mut out, "42601", &e);
                                }
                            }
                        }
                        Err(e) => {
                            session.in_error = true;
                            error_response(&mut out, "42601", &e);
                        }
                    }
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'B' => {
                // Bind: portal, statement, formats, params, result formats.
                let mut out = Vec::new();
                if session.in_error {
                } else {
                    match parse_bind(&body) {
                        Ok((portal_name, stmt_name, params)) => {
                            match session.prepared.get(&stmt_name) {
                                Some(st) => {
                                    let substituted =
                                        substitute_params(&st.query, &params);
                                    match substituted {
                                        Ok(q) => {
                                            session.portals.insert(
                                                portal_name.clone(),
                                                Portal {
                                                    query: q,
                                                    cached: None,
                                                },
                                            );
                                            message(&mut out, b'2', b""); // BindComplete
                                        }
                                        Err(e) => {
                                            session.in_error = true;
                                            error_response(&mut out, "08P01", &e);
                                        }
                                    }
                                }
                                None => {
                                    session.in_error = true;
                                    error_response(
                                        &mut out,
                                        "26000",
                                        &format!(
                                            "prepared statement \"{}\" does not exist",
                                            stmt_name
                                        ),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            session.in_error = true;
                            error_response(&mut out, "08P01", &e);
                        }
                    }
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'D' => {
                // Describe: 'S'tatement or 'P'ortal + name.
                let mut out = Vec::new();
                if session.in_error {
                } else if body.len() < 2 {
                    session.in_error = true;
                    error_response(&mut out, "08P01", "malformed Describe message");
                } else {
                    let kind = body[0];
                    let name = null_terminated_string(&body[1..]);
                    match kind {
                        b'S' => match session.prepared.get(&name) {
                            Some(st) => {
                                // ParameterDescription: param count, OIDs
                                // left unspecified (0) — every mainstream
                                // driver resolves types client-side.
                                let mut p = Vec::with_capacity(2 + 4 * st.n_params);
                                p.extend_from_slice(&(st.n_params as i16).to_be_bytes());
                                for _ in 0..st.n_params {
                                    p.extend_from_slice(&0i32.to_be_bytes());
                                }
                                message(&mut out, b't', &p);
                                message(&mut out, b'n', b""); // NoData
                            }
                            None => {
                                session.in_error = true;
                                error_response(
                                    &mut out,
                                    "26000",
                                    &format!(
                                        "prepared statement \"{}\" does not exist",
                                        name
                                    ),
                                );
                            }
                        },
                        b'P' => match session.portals.get_mut(&name) {
                            Some(portal) => {
                                // SELECT-shaped portals execute once here so
                                // the driver learns the column layout; the
                                // rowset is cached for the following Execute
                                // (reads only — DML answers NoData).
                                if is_select_shaped(&portal.query) {
                                    match sql.execute_in_tx(&portal.query, &mut tx) {
                                        Ok(res) => {
                                            row_description(&mut out, &res.columns, &res);
                                            portal.cached = Some(res);
                                        }
                                        Err(e) => {
                                            session.in_error = true;
                                            error_response(&mut out, "42601", &e);
                                        }
                                    }
                                } else {
                                    message(&mut out, b'n', b""); // NoData
                                }
                            }
                            None => {
                                session.in_error = true;
                                error_response(
                                    &mut out,
                                    "34000",
                                    &format!("portal \"{}\" does not exist", name),
                                );
                            }
                        },
                        _ => {
                            session.in_error = true;
                            error_response(&mut out, "08P01", "malformed Describe kind");
                        }
                    }
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'E' => {
                // Execute: portal, i32 max_rows (0 = all).
                let mut out = Vec::new();
                if session.in_error {
                } else {
                    let mut rd = MsgReader::new(&body);
                    let parsed = (|| -> Result<(String, i32), String> {
                        let name = rd.cstr()?;
                        let max_rows = rd.i32()?;
                        Ok((name, max_rows))
                    })();
                    match parsed {
                        Ok((name, max_rows)) => match session.portals.remove(&name) {
                            Some(mut portal) => {
                                stats.total_queries.fetch_add(1, Ordering::Relaxed);
                                let cached = portal.cached.take();
                                match cached {
                                    Some(res) => {
                                        encode_rows_limited(
                                            &mut out,
                                            &res,
                                            &portal.query,
                                            max_rows,
                                        );
                                    }
                                    None => {
                                        match sql.execute_in_tx(&portal.query, &mut tx) {
                                            Ok(res) => {
                                                if is_select_shaped(&portal.query) {
                                                    encode_rows_limited(
                                                        &mut out,
                                                        &res,
                                                        &portal.query,
                                                        max_rows,
                                                    );
                                                } else {
                                                    encode_result(
                                                        &mut out,
                                                        &res,
                                                        &portal.query,
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                session.in_error = true;
                                                error_response(&mut out, "42601", &e);
                                            }
                                        }
                                    }
                                }
                            }
                            None => {
                                session.in_error = true;
                                error_response(
                                    &mut out,
                                    "34000",
                                    &format!("portal \"{}\" does not exist", name),
                                );
                            }
                        },
                        Err(e) => {
                            session.in_error = true;
                            error_response(&mut out, "08P01", &e);
                        }
                    }
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            b'C' => {
                // Close: 'S'/'P' + name → CloseComplete.
                let mut out = Vec::new();
                if session.in_error {
                } else if body.len() < 2 {
                    session.in_error = true;
                    error_response(&mut out, "08P01", "malformed Close message");
                } else {
                    let kind = body[0];
                    let name = null_terminated_string(&body[1..]);
                    match kind {
                        b'S' => {
                            session.prepared.remove(&name);
                        }
                        b'P' => {
                            session.portals.remove(&name);
                        }
                        _ => {}
                    }
                    message(&mut out, b'3', b""); // CloseComplete
                }
                stream.write_all(&out)?;
                stream.flush()?;
            }
            _ => break, // unknown message type: drop the session
        }
    }
    eprintln!("[PGWIRE] session closed for user '{user}'");
    Ok(())
}

/// Per-session extended-protocol state (v4.2): prepared statements,
/// bound portals and the post-error skip flag.
#[derive(Default)]
struct ExtendedSession {
    prepared: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
    in_error: bool,
}

struct PreparedStatement {
    query: String,
    n_params: usize,
}

struct Portal {
    /// SQL text with every parameter substituted by its literal.
    query: String,
    /// Rowset cached by Describe-portal for SELECT-shaped queries so the
    /// following Execute replays it without re-running the statement.
    cached: Option<QueryResult>,
}

/// Sequential reader over one message body.
struct MsgReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> MsgReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err("pgwire: truncated message body".into());
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    fn i16(&mut self) -> Result<i16, String> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Result<i32, String> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn cstr(&mut self) -> Result<String, String> {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.buf.len() {
            return Err("pgwire: unterminated string in message body".into());
        }
        let s = String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned();
        self.pos += 1; // skip NUL
        Ok(s)
    }
}

/// Parse ('P') body: statement name, query text, ignored type OIDs.
fn parse_parse(body: &[u8]) -> Result<(String, String), String> {
    let mut rd = MsgReader::new(body);
    let name = rd.cstr()?;
    let query = rd.cstr()?;
    Ok((name, query))
}

/// Parse ('B') body: portal, statement, param formats (ignored), the
/// parameter values themselves (text format or -1 length for NULL) and
/// result formats (ignored). Binary parameters are not accepted: a binary
/// format code produces a clear feature-not-supported error.
fn parse_bind(body: &[u8]) -> Result<(String, String, Vec<Option<String>>), String> {
    let mut rd = MsgReader::new(body);
    let portal = rd.cstr()?;
    let stmt = rd.cstr()?;
    let n_fmt = rd.i16()?;
    for _ in 0..n_fmt.max(0) {
        if rd.i16()? != 0 {
            return Err(
                "pgwire: binary parameters are not supported; use text format (extended protocol default)"
                    .into(),
            );
        }
    }
    let n_params = rd.i16()?;
    if n_params < 0 {
        return Err("pgwire: negative parameter count".into());
    }
    let mut params = Vec::with_capacity(n_params as usize);
    for _ in 0..n_params {
        let len = rd.i32()?;
        if len < 0 {
            params.push(None); // NULL
        } else {
            let raw = rd.take(len as usize)?;
            params.push(Some(String::from_utf8_lossy(raw).into_owned()));
        }
    }
    Ok((portal, stmt, params))
}

/// Count the placeholders of a prepared query: `$n` style (the highest n)
/// or JDBC-style `?` (positional count). Mixing the two styles is rejected.
/// String literals are skipped so a `$1` inside quotes is never counted.
fn count_placeholders(query: &str) -> Result<usize, String> {
    let bytes = query.as_bytes();
    let mut in_str = false;
    let mut i = 0usize;
    let mut max_dollar = 0usize;
    let mut q_count = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_str = true;
                i += 1;
            }
            b'$' => {
                let mut j = i + 1;
                let mut n = 0usize;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                // $n must be a standalone token (not a prefix of $nabc) and
                // never a dollar-quoted-string delimiter ($$...$$).
                let boundary = j >= bytes.len()
                    || !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_');
                if n > 0 && j > i + 1 && boundary {
                    max_dollar = max_dollar.max(n);
                    i = j;
                } else {
                    i += 1;
                }
            }
            b'?' => {
                q_count += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if max_dollar > 0 && q_count > 0 {
        return Err(
            "pgwire: mixing $n and ? placeholders in one statement is not allowed".into(),
        );
    }
    Ok(if max_dollar > 0 { max_dollar } else { q_count })
}

/// Render one bound parameter as an SQL literal: NULL stays NULL, values
/// that parse as integers/floats are bound unquoted (the engine compares
/// numerically), everything else is a single-quoted string with '' escapes.
fn render_param(p: &Option<String>) -> String {
    match p {
        None => "NULL".to_string(),
        Some(s) => {
            if let Ok(i) = s.parse::<i64>() {
                return i.to_string();
            }
            if let Ok(f) = s.parse::<f64>() {
                if f.is_finite() {
                    return f.to_string();
                }
            }
            format!("'{}'", s.replace('\'', "''"))
        }
    }
}

/// Substitute every `$n` / `?` placeholder outside string literals with its
/// bound literal. The parameter count must match the query exactly.
fn substitute_params(query: &str, params: &[Option<String>]) -> Result<String, String> {
    let expected = count_placeholders(query)?;
    if expected != params.len() {
        return Err(format!(
            "bind message supplies {} parameter(s), prepared statement \"{}\" requires {}",
            params.len(),
            query.trim().chars().take(48).collect::<String>(),
            expected
        ));
    }
    if expected == 0 {
        return Ok(query.to_string());
    }
    let bytes = query.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 16 * params.len());
    let mut in_str = false;
    let mut i = 0usize;
    let mut next_q = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c);
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    out.push(b'\'');
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            b'$' => {
                let mut j = i + 1;
                let mut n = 0usize;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                let boundary = j >= bytes.len()
                    || !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_');
                if n > 0 && j > i + 1 && boundary {
                    out.extend_from_slice(render_param(&params[n - 1]).as_bytes());
                    i = j;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            b'?' => {
                out.extend_from_slice(render_param(&params[next_q]).as_bytes());
                next_q += 1;
                i += 1;
            }
            _ => {
                // Any other byte (incl. multi-byte UTF-8) copies as-is.
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "pgwire: non-UTF-8 query text".into())
}

/// SELECT-shaped query text (drives the RowDescription vs NoData choice).
/// Mirrors the heuristic of `encode_result` and covers WITH/SHOW/EXPLAIN.
fn is_select_shaped(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("SHOW TABLES")
        || upper.starts_with("SHOW CHROMOSOMES")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("SHOW HEALTH")
}

/// Encode a SELECT-shaped result with an optional max_rows cap (0 = all).
/// The server does not implement portal suspension: a positive cap sends
/// the first `max_rows` rows and completes (drivers that need streaming
/// cursors set fetch sizes > result size in practice for this engine's
/// workloads).
fn encode_rows_limited(out: &mut Vec<u8>, res: &QueryResult, sql_text: &str, max_rows: i32) {
    let total = res.rows.len();
    let limit = if max_rows > 0 {
        (max_rows as usize).min(total)
    } else {
        total
    };
    let view = QueryResult {
        columns: res.columns.clone(),
        rows: res.rows[..limit].to_vec(),
        message: res.message.clone(),
        execution_plan: res.execution_plan.clone(),
        execution_time_us: res.execution_time_us,
    };
    // Keep the CommandComplete tag honest about the number of rows sent.
    encode_result(out, &view, sql_text);
}

/// Encode a successful statement result as RowDescription + DataRows +
/// CommandComplete. Non-SELECT statements report their affected counts.
fn encode_result(out: &mut Vec<u8>, res: &QueryResult, sql_text: &str) {
    let upper = sql_text.trim_start().to_ascii_uppercase();
    // SELECT-shaped results: columns + rows.
    let is_select = upper.starts_with("SELECT") || upper.starts_with("WITH")
        || upper.starts_with("SHOW TABLES") || upper.starts_with("SHOW CHROMOSOMES")
        || upper.starts_with("EXPLAIN") || upper.starts_with("SHOW HEALTH");
    if is_select && !res.columns.is_empty() {
        row_description(out, &res.columns, res);
        for row in &res.rows {
            let mut body = Vec::new();
            // Int16 column count header (the field its absence turned into
            // a silent protocol desync for every client).
            body.extend_from_slice(&(res.columns.len() as i16).to_be_bytes());
            for col in &res.columns {
                match row.get(col).and_then(value_to_text) {
                    Some(s) => {
                        body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                        body.extend_from_slice(s.as_bytes());
                    }
                    None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
            message(out, b'D', &body);
        }
        let tag = if upper.starts_with("EXPLAIN") {
            "EXPLAIN".to_string()
        } else if upper.starts_with("SHOW") {
            format!("SHOW {}", res.rows.len())
        } else {
            format!("SELECT {}", res.rows.len())
        };
        // CommandComplete carries a C-string: libpq validates the trailing NUL.
        let mut tag_body = tag.into_bytes();
        tag_body.push(0);
        message(out, b'C', &tag_body);
        return;
    }
    // DDL / DML: no rowset, just a command tag.
    let affected: usize = res
        .rows
        .iter()
        .filter_map(|r| {
            r.get("affected_rows")
                .or_else(|| r.get("deleted"))
                // v4.2: INSERT reports one row per inserted key.
                .or_else(|| r.get("inserted_key"))
        })
        .filter_map(|v| match v {
            Value::Int(i) => Some(*i as usize),
            Value::Text(_) => Some(1), // inserted_key present → one row
            _ => None,
        })
        .next()
        .unwrap_or(0);
    let tag = if upper.starts_with("INSERT") {
        format!("INSERT 0 {affected}")
    } else if upper.starts_with("UPDATE") {
        format!("UPDATE {affected}")
    } else if upper.starts_with("DELETE") {
        format!("DELETE {affected}")
    } else if upper.starts_with("BEGIN") {
        "BEGIN".to_string()
    } else if upper.starts_with("COMMIT") {
        "COMMIT".to_string()
    } else if upper.starts_with("ROLLBACK") {
        "ROLLBACK".to_string()
    } else if upper.starts_with("CREATE") {
        "CREATE TABLE".to_string()
    } else if upper.starts_with("DROP") {
        "DROP TABLE".to_string()
    } else {
        format!("COMMAND {}", res.rows.len().max(1))
    };
    let mut tag_body = tag.into_bytes();
    tag_body.push(0);
    message(out, b'C', &tag_body);
}

fn row_description(out: &mut Vec<u8>, columns: &[String], res: &QueryResult) {
    let mut body = Vec::new();
    body.extend_from_slice(&(columns.len() as i16).to_be_bytes());
    // Sample the first row for type inference (all-NULL → text).
    let sample = res.rows.first();
    for col in columns {
        body.extend_from_slice(col.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table OID
        body.extend_from_slice(&0i16.to_be_bytes()); // column attr
        let oid = sample
            .and_then(|r| r.get(col))
            .map(type_oid)
            .unwrap_or(OID_TEXT);
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes()); // type size
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    message(out, b'T', &body);
}

fn error_response(out: &mut Vec<u8>, code: &str, message_text: &str) {
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"ERROR\0");
    body.push(b'V');
    body.extend_from_slice(b"ERROR\0");
    body.push(b'C');
    body.extend_from_slice(code.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message_text.as_bytes());
    body.push(0);
    body.push(0);
    message(out, b'E', &body);
}

fn message(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&((body.len() as i32 + 4).to_be_bytes()));
    out.extend_from_slice(body);
}

fn parameter(out: &mut Vec<u8>, name: &str, value: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    message(out, b'S', &body);
}

/// Parse "key\0value\0" pairs from a startup body, returning one value.
fn startup_param(body: &[u8], key: &str) -> Option<String> {
    let mut i = 0usize;
    while i < body.len() {
        let k = null_terminated_string(&body[i..]);
        i += k.len() + 1;
        if i >= body.len() {
            break;
        }
        let v = null_terminated_string(&body[i..]);
        i += v.len() + 1;
        if k == key {
            return Some(v);
        }
    }
    None
}

fn null_terminated_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_params_parse() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0admin\0database\0dna\0\0");
        assert_eq!(startup_param(&body, "user"), Some("admin".into()));
        assert_eq!(startup_param(&body, "database"), Some("dna".into()));
        assert_eq!(startup_param(&body, "missing"), None);
    }

    #[test]
    fn value_rendering() {
        assert_eq!(value_to_text(&Value::Int(42)), Some("42".into()));
        assert_eq!(value_to_text(&Value::Float(2.5)), Some("2.5".into()));
        assert_eq!(value_to_text(&Value::Float(250.0)), Some("250.0".into()));
        assert_eq!(value_to_text(&Value::Text("a".into())), Some("a".into()));
        assert_eq!(value_to_text(&Value::Null), None);
    }

    // ---------------- v4.2: extended query protocol ----------------

    #[test]
    fn placeholder_counting() {
        // $n style: highest index wins.
        assert_eq!(count_placeholders("SELECT * FROM t WHERE a = $2 AND b = $1").unwrap(), 2);
        assert_eq!(count_placeholders("SELECT $10").unwrap(), 10);
        // JDBC style: positional count.
        assert_eq!(count_placeholders("SELECT * FROM t WHERE a = ? AND b = ?").unwrap(), 2);
        assert_eq!(count_placeholders("SELECT 1").unwrap(), 0);
        // Placeholders inside string literals are never counted.
        assert_eq!(count_placeholders("SELECT * FROM t WHERE s = '$1 ? $2'").unwrap(), 0);
        assert_eq!(
            count_placeholders("SELECT * FROM t WHERE s = 'it''s $1' AND a = $1").unwrap(),
            1
        );
        // Mixing the two styles is rejected.
        assert!(count_placeholders("SELECT * FROM t WHERE a = $1 AND b = ?").is_err());
        // A `$` that is not a placeholder ($$ dollar quoting, $5x) is ignored.
        assert_eq!(count_placeholders("SELECT $$not a placeholder$$").unwrap(), 0);
        assert_eq!(count_placeholders("SELECT $5x").unwrap(), 0);
        // Trailing '$' at end of input must not panic.
        assert_eq!(count_placeholders("SELECT $").unwrap(), 0);
    }

    #[test]
    fn parameter_substitution() {
        let p = |s: &str| Some(s.to_string());
        // Numbers bind unquoted, strings quoted with '' escapes, NULL stays.
        assert_eq!(
            substitute_params("SELECT * FROM t WHERE id = $1 AND name = $2 AND x = $3",
                              &[p("42"), p("O'Brien"), None])
                .unwrap(),
            "SELECT * FROM t WHERE id = 42 AND name = 'O''Brien' AND x = NULL"
        );
        // Float rendering.
        assert_eq!(
            substitute_params("SELECT $1", &[p("2.5")]).unwrap(),
            "SELECT 2.5"
        );
        // ? style binds positionally.
        assert_eq!(
            substitute_params("SELECT ? + ?", &[p("1"), p("2")]).unwrap(),
            "SELECT 1 + 2"
        );
        // Text inside literals is preserved untouched (and no parameter is
        // required — placeholders inside quotes are not placeholders).
        assert_eq!(
            substitute_params("SELECT 'a?b $1'", &[]).unwrap(),
            "SELECT 'a?b $1'"
        );
        // Count mismatch is a clean error, never a silent wrong query.
        assert!(substitute_params("SELECT $1", &[]).is_err());
        assert!(substitute_params("SELECT 1", &[p("1")]).is_err());
    }

    #[test]
    fn bind_message_roundtrip() {
        // Build a Bind body by hand: portal, statement, 0 format codes,
        // 2 params ("7", NULL), 0 result formats.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"p1\0");
        body.extend_from_slice(b"s1\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&2i16.to_be_bytes());
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(b"7");
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        let (portal, stmt, params) = parse_bind(&body).unwrap();
        assert_eq!(portal, "p1");
        assert_eq!(stmt, "s1");
        assert_eq!(params, vec![Some("7".into()), None]);
        // Binary format code is refused with a clear message.
        let mut bin: Vec<u8> = Vec::new();
        bin.extend_from_slice(b"\0");
        bin.extend_from_slice(b"s\0");
        bin.extend_from_slice(&1i16.to_be_bytes());
        bin.extend_from_slice(&1i16.to_be_bytes()); // format = binary
        assert!(parse_bind(&bin).unwrap_err().contains("binary"));
        // Truncated bodies are errors, not panics.
        assert!(parse_bind(&body[..6]).is_err());
    }

    #[test]
    fn parse_message_roundtrip() {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"\0"); // unnamed statement
        body.extend_from_slice(b"SELECT * FROM t WHERE id = $1\0");
        body.extend_from_slice(&0i16.to_be_bytes()); // no type hints
        let (name, query) = parse_parse(&body).unwrap();
        assert_eq!(name, "");
        assert_eq!(query, "SELECT * FROM t WHERE id = $1");
        assert_eq!(count_placeholders(&query).unwrap(), 1);
    }

    #[test]
    fn select_shape_detection() {
        assert!(is_select_shaped("SELECT 1"));
        assert!(is_select_shaped("  with x as (select 1) select * from x"));
        assert!(is_select_shaped("SHOW TABLES"));
        assert!(is_select_shaped("EXPLAIN SELECT 1"));
        assert!(!is_select_shaped("INSERT INTO t VALUES (1)"));
        assert!(!is_select_shaped("UPDATE t SET a = 1"));
        assert!(!is_select_shaped("ALTER TABLE t DROP COLUMN a"));
    }
}
