// SPDX-License-Identifier: AGPL-3.0-only
//! Connection state: packet framing (read/write with sequence numbers),
//! the per-connection serve loop, handshake-response parsing and the
//! COM_STATISTICS reply.

use super::*;

// ---------------------------------------------------------------------------
// Packet framing
// ---------------------------------------------------------------------------

/// A framed MySQL connection: 3-byte LE payload length + 1-byte sequence id.
/// Payloads larger than 16 MiB − 1 are split across continuation frames.
pub(crate) struct Conn {
    stream: TcpStream,
    /// Sequence number for the NEXT frame we write.
    write_seq: u8,
    /// Sequence number of the last frame received from the client.
    client_seq: u8,
}

impl Conn {
    pub(super) fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            write_seq: 0,
            client_seq: 0,
        }
    }

    pub(super) fn read_packet(&mut self) -> std::io::Result<Vec<u8>> {
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
    pub(super) fn begin_reply(&mut self) {
        self.write_seq = self.client_seq.wrapping_add(1);
    }

    pub(super) fn write_packet(&mut self, payload: &[u8]) -> std::io::Result<()> {
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

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

pub(crate) fn handle_connection(
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

pub(crate) fn statistics_line(stats: &MySqlWireStats, started: Instant) -> String {
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
pub(crate) fn parse_handshake_response(
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

pub(crate) fn read_nul_str(p: &[u8], pos: &mut usize) -> Option<String> {
    let end = p[*pos..].iter().position(|&b| b == 0)? + *pos;
    let s = String::from_utf8_lossy(&p[*pos..end]).into_owned();
    *pos = end + 1;
    Some(s)
}

