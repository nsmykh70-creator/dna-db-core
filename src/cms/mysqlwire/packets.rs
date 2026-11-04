// SPDX-License-Identifier: AGPL-3.0-only
//! Packet codecs: length-encoded ints/bytes, handshake, OK/ERR/EOF,
//! column definitions and type inference / float formatting.

use super::*;

pub(crate) fn write_lenenc_int(out: &mut Vec<u8>, n: u64) {
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

pub(crate) fn write_lenenc_bytes(out: &mut Vec<u8>, data: &[u8]) {
    write_lenenc_int(out, data.len() as u64);
    out.extend_from_slice(data);
}

pub(crate) fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> Option<u64> {
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

pub(crate) fn read_lenenc_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let n = read_lenenc_int(buf, pos)? as usize;
    let out = buf.get(*pos..*pos + n)?;
    *pos += n;
    Some(out)
}

// ---------------------------------------------------------------------------
// Packet builders
// ---------------------------------------------------------------------------

/// HandshakeV10 (server greeting).
pub(crate) fn build_handshake(conn_id: u32, salt: &[u8; 20]) -> Vec<u8> {
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
pub(crate) fn ok_packet(affected: u64, last_insert_id: u64, status: u16, info: &str) -> Vec<u8> {
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
pub(crate) fn err_packet(errno: u16, sqlstate: &str, message: &str) -> Vec<u8> {
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
pub(crate) fn eof_packet(status: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(5);
    p.push(0xfe);
    p.extend_from_slice(&0u16.to_le_bytes()); // warnings
    p.extend_from_slice(&status.to_le_bytes());
    p
}

/// ColumnDefinition41. `with_default` adds the trailing lenenc default value
/// that only COM_FIELD_LIST responses carry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn column_def_packet(
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
pub(crate) fn infer_type(rows: &[BTreeMap<String, Value>], col: &str) -> u8 {
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
pub(crate) fn fmt_float(f: f64) -> String {
    format!("{f}")
}

/// MySQL text-protocol string escaping (backslash mode, the PHP default).
pub(crate) fn escape_sql_literal(s: &[u8]) -> Vec<u8> {
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

