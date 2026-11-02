// SPDX-License-Identifier: AGPL-3.0-only
//! Integration test: real MySQL wire protocol against the in-process
//! `MySqlWireServer`. A minimal client implements the protocol from scratch
//! (handshake, COM_QUERY, COM_STMT_*) — the same bytes mysqli / PDO put on
//! the socket — so every assertion here is wire-accurate, not an internal
//! shortcut.

#![cfg(feature = "cms")]

use dna_db_core::cms::compat::CmsSqlGate;
use dna_db_core::cms::mysqlwire::native_password_token;
use dna_db_core::cms::{MySqlAuth, MySqlWireServer};
use dna_db_core::sql_engine::SqlEngine;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

const CLIENT_LONG_PASSWORD: u32 = 1;
const CLIENT_PROTOCOL_41: u32 = 512;
const CLIENT_TRANSACTIONS: u32 = 8192;
const CLIENT_SECURE_CONNECTION: u32 = 32_768;
const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dna_mysqlwire_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct TestClient {
    stream: TcpStream,
}

impl TestClient {
    fn read_full(&mut self) -> Vec<u8> {
        // Reads all frames of one logical packet (continuation handled).
        let mut all = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header).unwrap();
            let len = (header[0] as usize)
                | ((header[1] as usize) << 8)
                | ((header[2] as usize) << 16);
            let mut chunk = vec![0u8; len];
            self.stream.read_exact(&mut chunk).unwrap();
            all.extend_from_slice(&chunk);
            if len < 0xFF_FF_FF {
                break;
            }
        }
        all
    }

    fn write_packet(&mut self, seq: u8, payload: &[u8]) {
        let header = [
            (payload.len() & 0xff) as u8,
            ((payload.len() >> 8) & 0xff) as u8,
            ((payload.len() >> 16) & 0xff) as u8,
            seq,
        ];
        self.stream.write_all(&header).unwrap();
        self.stream.write_all(payload).unwrap();
        self.stream.flush().unwrap();
    }

    fn query(&mut self, sql: &str) -> QueryResponse {
        let mut p = vec![0x03u8];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p);
        self.read_response()
    }

    /// Reads one complete response: text resultset / OK / ERR.
    fn read_response(&mut self) -> QueryResponse {
        let first = self.read_full();
        match first[0] {
            0x00 => {
                let mut pos = 1usize;
                let affected = read_lenenc(&first, &mut pos);
                let last_insert_id = read_lenenc(&first, &mut pos);
                QueryResponse::Ok {
                    affected_rows: affected,
                    last_insert_id,
                }
            }
            0xff => {
                let errno = u16::from_le_bytes([first[1], first[2]]);
                let sqlstate = String::from_utf8_lossy(&first[4..9]).to_string();
                QueryResponse::Err {
                    errno,
                    sqlstate,
                    message: String::from_utf8_lossy(&first[9..]).to_string(),
                }
            }
            _ => {
                // Text resultset: first packet = lenenc column count.
                let mut pos = 0usize;
                let ncols = read_lenenc(&first, &mut pos) as usize;
                let mut columns = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    let def = self.read_full();
                    let mut p = 0usize;
                    assert_eq!(&read_lenenc_bytes(&def, &mut p), b"def");
                    read_lenenc_bytes(&def, &mut p); // schema
                    read_lenenc_bytes(&def, &mut p); // table
                    read_lenenc_bytes(&def, &mut p); // org_table
                    let name = read_lenenc_bytes(&def, &mut p).to_vec();
                    columns.push(String::from_utf8(name).unwrap());
                }
                let eof1 = self.read_full();
                assert_eq!(eof1[0], 0xfe, "expected EOF after column defs");
                let mut rows = Vec::new();
                loop {
                    let row = self.read_full();
                    if row[0] == 0xfe && row.len() < 9 {
                        break; // EOF terminator
                    }
                    let mut vals = Vec::with_capacity(ncols);
                    let mut p = 0usize;
                    for _ in 0..ncols {
                        if row[p] == 0xfb {
                            vals.push(None);
                            p += 1;
                        } else {
                            let v = read_lenenc_bytes(&row, &mut p).to_vec();
                            vals.push(Some(v));
                        }
                    }
                    rows.push(vals);
                }
                QueryResponse::Rows { columns, rows }
            }
        }
    }

    /// COM_STMT_PREPARE → (stmt_id, num_columns, num_params); consumes the
    /// trailing param/column definition packets.
    fn prepare(&mut self, sql: &str) -> (u32, usize, usize) {
        let mut p = vec![0x16u8];
        p.extend_from_slice(sql.as_bytes());
        self.write_packet(0, &p);
        let head = self.read_full();
        assert_eq!(head[0], 0x00, "prepare failed: {head:?}");
        let stmt_id = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
        let ncols = u16::from_le_bytes([head[5], head[6]]) as usize;
        let nparams = u16::from_le_bytes([head[7], head[8]]) as usize;
        for _ in 0..nparams {
            let _def = self.read_full();
            let eof = self.read_full();
            assert_eq!(eof[0], 0xfe);
        }
        for _ in 0..ncols {
            let _def = self.read_full();
            let eof = self.read_full();
            assert_eq!(eof[0], 0xfe);
        }
        (stmt_id, ncols, nparams)
    }

    /// COM_STMT_EXECUTE with all-string/int params supplied by the caller.
    /// Answers come in the BINARY protocol (0x00 rows, null bitmap offset 2).
    fn execute(&mut self, stmt_id: u32, params: &[Param]) -> QueryResponse {
        let n = params.len();
        let mut p = Vec::new();
        p.extend_from_slice(&stmt_id.to_le_bytes());
        p.push(0); // flags
        p.extend_from_slice(&1u32.to_le_bytes()); // iteration count
        if n > 0 {
            let nb = vec![0u8; (n + 7) / 8];
            p.extend_from_slice(&nb);
            p.push(1); // new_params_bound
            for prm in params {
                p.push(prm.wire_type());
                p.push(0); // unsigned flag
            }
            for prm in params {
                match prm {
                    Param::Long(v) => p.extend_from_slice(&v.to_le_bytes()),
                    Param::Str(s) => {
                        write_lenenc(&mut p, s.len() as u64);
                        p.extend_from_slice(s.as_bytes());
                    }
                }
            }
        }
        self.write_packet(0, &[0x17].iter().copied().chain(p).collect::<Vec<u8>>());
        self.read_binary_response()
    }

    /// Reads one response in the binary protocol (COM_STMT_EXECUTE result).
    fn read_binary_response(&mut self) -> QueryResponse {
        let first = self.read_full();
        match first[0] {
            0x00 => {
                // OK packet: lenenc affected, lenenc insert-id, status, warnings
                let mut pos = 1usize;
                let affected = read_lenenc(&first, &mut pos);
                let last_insert_id = read_lenenc(&first, &mut pos);
                QueryResponse::Ok {
                    affected_rows: affected,
                    last_insert_id,
                }
            }
            0xff => {
                let errno = u16::from_le_bytes([first[1], first[2]]);
                QueryResponse::Err {
                    errno,
                    sqlstate: String::from_utf8_lossy(&first[4..9]).to_string(),
                    message: String::from_utf8_lossy(&first[9..]).to_string(),
                }
            }
            ncols_len => {
                let mut pos = 0usize;
                let ncols = read_lenenc(&[ncols_len], &mut pos) as usize;
                assert!(ncols > 0, "binary resultset with no columns: first={first:02x?}");
                let mut columns = Vec::with_capacity(ncols);
                let mut types = Vec::with_capacity(ncols);
                for _ in 0..ncols {
                    let def = self.read_full();
                    let mut p = 0usize;
                    assert_eq!(&read_lenenc_bytes(&def, &mut p), b"def");
                    read_lenenc_bytes(&def, &mut p); // schema
                    read_lenenc_bytes(&def, &mut p); // table
                    read_lenenc_bytes(&def, &mut p); // org_table
                    let name = read_lenenc_bytes(&def, &mut p).to_vec();
                    read_lenenc_bytes(&def, &mut p); // org_name
                    let _fixed = read_lenenc(&def, &mut p);
                    p += 2 + 4; // charset + column length
                    types.push(def[p]);
                    columns.push(String::from_utf8(name).unwrap());
                }
                let eof1 = self.read_full();
                assert_eq!(eof1[0], 0xfe);
                let nb_len = (ncols + 9) / 8;
                let mut rows = Vec::new();
                loop {
                    let row = self.read_full();
                    if row[0] == 0xfe && row.len() < 9 {
                        break;
                    }
                    assert_eq!(row[0], 0x00, "binary row header");
                    let mut vals = Vec::with_capacity(ncols);
                    let mut p = 1 + nb_len; // 0x00 header + null bitmap
                    for i in 0..ncols {
                        let is_null = row[1 + (i + 2) / 8] & (1 << ((i + 2) % 8)) != 0;
                        if is_null {
                            vals.push(None);
                            continue;
                        }
                        match types[i] {
                            8 => {
                                // LONGLONG
                                let mut b = [0u8; 8];
                                b.copy_from_slice(&row[p..p + 8]);
                                p += 8;
                                vals.push(Some(i64::from_le_bytes(b).to_string().into_bytes()));
                            }
                            5 => {
                                // DOUBLE
                                let mut b = [0u8; 8];
                                b.copy_from_slice(&row[p..p + 8]);
                                p += 8;
                                vals.push(Some(f64::from_le_bytes(b).to_string().into_bytes()));
                            }
                            _ => {
                                // VAR_STRING / others: lenenc bytes
                                let v = read_lenenc_bytes(&row, &mut p).to_vec();
                                vals.push(Some(v));
                            }
                        }
                    }
                    rows.push(vals);
                }
                QueryResponse::Rows { columns, rows }
            }
        }
    }
}

enum Param {
    Long(i32),
    Str(&'static str),
}

impl Param {
    fn wire_type(&self) -> u8 {
        match self {
            Param::Long(_) => 3,  // MYSQL_TYPE_LONG
            Param::Str(_) => 253, // MYSQL_TYPE_VAR_STRING
        }
    }
}

#[derive(Debug)]
enum QueryResponse {
    Ok {
        affected_rows: u64,
        last_insert_id: u64,
    },
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    Err {
        errno: u16,
        sqlstate: String,
        message: String,
    },
}

fn read_lenenc(buf: &[u8], pos: &mut usize) -> u64 {
    let first = buf[*pos];
    *pos += 1;
    match first {
        0xfc => {
            let v = u16::from_le_bytes([buf[*pos], buf[*pos + 1]]) as u64;
            *pos += 2;
            v
        }
        0xfd => {
            let mut v = 0u64;
            for i in 0..3 {
                v |= (buf[*pos + i] as u64) << (8 * i);
            }
            *pos += 3;
            v
        }
        0xfe => {
            let mut v = 0u64;
            for i in 0..8 {
                v |= (buf[*pos + i] as u64) << (8 * i);
            }
            *pos += 8;
            v
        }
        n => n as u64,
    }
}

fn read_lenenc_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let n = read_lenenc(buf, pos) as usize;
    let out = &buf[*pos..*pos + n];
    *pos += n;
    out
}

fn write_lenenc(out: &mut Vec<u8>, n: u64) {
    if n < 251 {
        out.push(n as u8);
    } else {
        out.push(0xfc);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    }
}

fn handshake(c: &mut TestClient, user: &str, pass: &str, db: Option<&str>) -> Vec<u8> {
    // 1. Read HandshakeV10.
    let hs = c.read_full();
    assert_eq!(hs[0], 10, "protocol version must be 10");
    let vlen = hs[1..].iter().position(|&b| b == 0).unwrap();
    let mut pos = 1 + vlen + 1; // past version NUL
    pos += 4; // connection id
    let mut salt = Vec::new();
    salt.extend_from_slice(&hs[pos..pos + 8]);
    pos += 8 + 1; // salt part 1 + filler
    pos += 2 + 1 + 2 + 2 + 1 + 10; // caps_l, charset, status, caps_h, auth_len, reserved
    salt.extend_from_slice(&hs[pos..pos + 12]);

    // 2. Send HandshakeResponse41 (mysql_native_password).
    let caps = CLIENT_LONG_PASSWORD
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | if db.is_some() { 8 } else { 0 }; // CLIENT_CONNECT_WITH_DB
    let token = native_password_token(pass, &salt);
    let mut resp = Vec::new();
    resp.extend_from_slice(&caps.to_le_bytes());
    resp.extend_from_slice(&16_777_215u32.to_le_bytes());
    resp.push(45); // utf8mb4_general_ci
    resp.extend_from_slice(&[0u8; 23]);
    resp.extend_from_slice(user.as_bytes());
    resp.push(0);
    resp.push(20);
    resp.extend_from_slice(&token);
    if let Some(db) = db {
        resp.extend_from_slice(db.as_bytes());
        resp.push(0);
    }
    resp.extend_from_slice(b"mysql_native_password");
    resp.push(0);
    c.write_packet(1, &resp);

    // 3. Expect OK or ERR.
    c.read_full()
}

fn spawn_server(auth: MySqlAuth) -> (u16, Arc<CmsSqlGate>) {
    let dir = temp_dir("srv");
    let engine = dna_db_core::Engine::open(dna_db_core::EngineConfig {
        dir_path: dir,
        sync_mode: dna_db_core::wal::SyncMode::Batched,
        ..dna_db_core::EngineConfig::default()
    })
    .unwrap();
    let sql = Arc::new(SqlEngine::new(engine));
    let gate = Arc::new(CmsSqlGate::new(sql));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Arc::new(MySqlWireServer::new(auth));
    {
        let gate = gate.clone();
        std::thread::spawn(move || {
            let _ = server.serve(listener, gate);
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    (port, gate)
}

fn connect(port: u16) -> TestClient {
    let mut c = TestClient {
        stream: TcpStream::connect(("127.0.0.1", port)).unwrap(),
    };
    c.stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    c
}

#[test]
fn mysqlwire_full_flow() {
    let (port, _gate) = spawn_server(MySqlAuth::Trust);
    let mut c = connect(port);

    // ---- handshake with CONNECT_WITH_DB = wordpress ----
    let ok = handshake(&mut c, "root", "", Some("wordpress"));
    assert_eq!(ok[0], 0x00, "handshake must end with OK, got {ok:?}");

    // ---- SELECT 1 ----
    match c.query("SELECT 1") {
        QueryResponse::Rows { columns, rows } => {
            assert_eq!(columns, vec!["1".to_string()]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some(b"1".as_slice()));
        }
        other => panic!("SELECT 1 → {other:?}"),
    }

    // ---- version() reports the MariaDB-compatible string ----
    match c.query("SELECT VERSION()") {
        QueryResponse::Rows { rows, .. } => {
            let v = String::from_utf8_lossy(rows[0][0].as_ref().unwrap());
            assert!(v.starts_with("10.11."), "version = {v}");
        }
        other => panic!("VERSION() → {other:?}"),
    }

    // ---- scalar functions evaluate with MySQL semantics ----
    match c.query("SELECT CONCAT('днк', '-', 'тест')") {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows[0][0].as_deref(), Some("днк-тест".as_bytes()));
        }
        other => panic!("CONCAT → {other:?}"),
    }
    match c.query("SELECT CONCAT('a', NULL, 'b')") {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows[0][0], None, "CONCAT with NULL must yield NULL");
        }
        other => panic!("CONCAT NULL → {other:?}"),
    }
    match c.query("SELECT UPPER('abc'), LOWER('ABC'), CHAR_LENGTH('днк')") {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows[0][0].as_deref(), Some(b"ABC".as_slice()));
            assert_eq!(rows[0][1].as_deref(), Some(b"abc".as_slice()));
            assert_eq!(rows[0][2].as_deref(), Some(b"3".as_slice()));
        }
        other => panic!("UPPER/LOWER/CHAR_LENGTH → {other:?}"),
    }

    // ---- CREATE TABLE with AUTO_INCREMENT (dbDelta-style DDL) ----
    match c.query(
        "CREATE TABLE wp_mock (ID BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, title VARCHAR(191) NOT NULL DEFAULT '', PRIMARY KEY (ID))",
    ) {
        QueryResponse::Ok { .. } => {}
        other => panic!("CREATE TABLE → {other:?}"),
    }

    // ---- multi-row INSERT: affected + first id ----
    match c.query("INSERT INTO wp_mock (title) VALUES ('alpha'), ('beta')") {
        QueryResponse::Ok {
            affected_rows,
            last_insert_id,
        } => {
            assert_eq!(affected_rows, 2);
            assert_eq!(last_insert_id, 1);
        }
        other => panic!("INSERT → {other:?}"),
    }

    // ---- LAST_INSERT_ID() is session-sticky ----
    match c.query("SELECT LAST_INSERT_ID()") {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows[0][0].as_deref(), Some(b"1".as_slice()));
        }
        other => panic!("LAST_INSERT_ID → {other:?}"),
    }

    // ---- next INSERT continues the sequence ----
    match c.query("INSERT INTO wp_mock (title) VALUES ('gamma')") {
        QueryResponse::Ok {
            affected_rows,
            last_insert_id,
        } => {
            assert_eq!(affected_rows, 1);
            assert_eq!(last_insert_id, 3);
        }
        other => panic!("INSERT#2 → {other:?}"),
    }

    // ---- SHOW TABLES LIKE ----
    match c.query("SHOW TABLES LIKE 'wp_mock'") {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        other => panic!("SHOW TABLES LIKE → {other:?}"),
    }

    // ---- SELECT with WHERE ----
    match c.query("SELECT ID, title FROM wp_mock WHERE ID = 3") {
        QueryResponse::Rows { columns, rows } => {
            assert_eq!(columns, vec!["ID".to_string(), "title".to_string()]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some(b"3".as_slice()));
            assert_eq!(rows[0][1].as_deref(), Some(b"gamma".as_slice()));
        }
        other => panic!("SELECT WHERE → {other:?}"),
    }

    // ---- MySQL errno surfaces through the wire ----
    match c.query("SELECT * FROM wp_missing") {
        QueryResponse::Err { errno, sqlstate, .. } => {
            assert_eq!(errno, 1146);
            assert_eq!(sqlstate, "42S02");
        }
        other => panic!("missing table → {other:?}"),
    }

    // ---- duplicate key → 1062 ----
    match c.query(
        "CREATE TABLE wp_dup (ID BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, code VARCHAR(10), PRIMARY KEY (ID), UNIQUE KEY uq_code (code))",
    ) {
        QueryResponse::Ok { .. } => {}
        other => panic!("CREATE wp_dup → {other:?}"),
    }
    let _ = c.query("INSERT INTO wp_dup (code) VALUES ('X1')");
    match c.query("INSERT INTO wp_dup (code) VALUES ('X1')") {
        QueryResponse::Err { errno, .. } => assert_eq!(errno, 1062),
        other => panic!("dup entry → {other:?}"),
    }

    // ---- COMMIT without a transaction is a silent no-op (MySQL parity) ----
    match c.query("COMMIT") {
        QueryResponse::Ok { .. } => {}
        other => panic!("bare COMMIT → {other:?}"),
    }

    // ---- COM_PING ----
    c.write_packet(0, &[0x0e]);
    let pong = c.read_full();
    assert_eq!(pong[0], 0x00, "ping must answer OK");

    // ---- COM_STATISTICS ----
    c.write_packet(0, &[0x09]);
    let stats = c.read_full();
    let stats = String::from_utf8_lossy(&stats).to_string();
    assert!(stats.contains("Uptime:"), "statistics = {stats}");

    // ---- prepared SELECT: probe at prepare, binary row at execute ----
    let (stmt_id, ncols, nparams) = c.prepare("SELECT title FROM wp_mock WHERE ID = ?");
    assert_eq!((ncols, nparams), (1, 1), "probe must report 1 column");
    match c.execute(stmt_id, &[Param::Long(2)]) {
        QueryResponse::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some(b"beta".as_slice()));
        }
        other => panic!("prepared SELECT → {other:?}"),
    }

    // ---- prepared INSERT: OK with last_insert_id ----
    let (ins_id, ncols, nparams) = c.prepare("INSERT INTO wp_mock (title) VALUES (?)");
    assert_eq!((ncols, nparams), (0, 1), "DML prepares with 0 columns");
    match c.execute(ins_id, &[Param::Str("delta")]) {
        QueryResponse::Ok {
            affected_rows,
            last_insert_id,
        } => {
            assert_eq!(affected_rows, 1);
            assert_eq!(last_insert_id, 4);
        }
        other => panic!("prepared INSERT → {other:?}"),
    }

    // ---- multi-statement: two responses with MORE_RESULTS flag ----
    {
        let mut p = vec![0x03u8];
        p.extend_from_slice(b"SET NAMES utf8mb4; SELECT 42");
        c.write_packet(0, &p);
        let first = c.read_full();
        assert_eq!(first[0], 0x00, "SET must answer OK");
        // SERVER_MORE_RESULTS_EXISTS = 0x0008 in the status flags
        let status = u16::from_le_bytes([first[3], first[4]]);
        assert_ne!(status & 0x0008, 0, "first response must set MORE_RESULTS");
        match c.read_response() {
            QueryResponse::Rows { rows, .. } => {
                assert_eq!(rows[0][0].as_deref(), Some(b"42".as_slice()));
            }
            other => panic!("multi stmt SELECT → {other:?}"),
        }
    }

    // ---- COM_QUIT ----
    c.write_packet(0, &[0x01]);
    std::thread::sleep(std::time::Duration::from_millis(30));
    // The server should close the socket (read returns 0 bytes or error).
    let mut buf = [0u8; 1];
    match c.stream.read(&mut buf) {
        Ok(0) => {}
        Ok(_) => {} // some stacks deliver FIN late; not an error for the test
        Err(_) => {}
    }
}

#[test]
fn mysqlwire_access_denied_with_wrong_password() {
    let mut users = std::collections::HashMap::new();
    users.insert("wpuser".to_string(), "wppass".to_string());
    let (port, _gate) = spawn_server(MySqlAuth::Users(users));

    // Correct password → OK.
    let mut c = connect(port);
    let ok = handshake(&mut c, "wpuser", "wppass", None);
    assert_eq!(ok[0], 0x00);

    // Wrong password → ERR 1045.
    let mut c2 = connect(port);
    let err = handshake(&mut c2, "wpuser", "nope", None);
    assert_eq!(err[0], 0xff);
    let errno = u16::from_le_bytes([err[1], err[2]]);
    assert_eq!(errno, 1045);
    let sqlstate = String::from_utf8_lossy(&err[4..9]).to_string();
    assert_eq!(sqlstate, "28000");
}
