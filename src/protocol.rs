// SPDX-License-Identifier: AGPL-3.0-only
//! Safe Binary Wire Protocol for DNA-DB
//!
//! Frame: [length u32 LE][payload], length <= MAX_FRAME_SIZE (16 MB).
//!
//! Opcodes (payload[0]):
//!   0x00 PING/STATUS  -> framed UTF-8 stats text
//!   0x01 AUTH         -> payload = "user:pass";  -> raw "OK" | raw "NO" + close
//!   0x02 GET          -> payload = key           -> framed value (len 0 = miss)
//!   0x03 PUT          -> [klen u16 LE][key][value] -> raw "OK"
//!   0x04 DELETE       -> payload = key           -> raw "OK"
//!   0x10 DNAQL        -> payload = query text    -> framed UTF-8 text (JSON rows)
//!   0x11 SQL          -> payload = query text    -> framed UTF-8 text (JSON rows)
//!   0x12 CHECKPOINT   -> raw "OK"
//!
//! Auth failures: 5 strikes per IP -> 60s lockout (SecurityManager).
//! Response format note: PUT/AUTH/DELETE/CHECKPOINT answer with a raw 2-byte
//! "OK" for latency; GET and query responses are length-framed.

pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub const OP_PING: u8 = 0x00;
pub const OP_AUTH: u8 = 0x01;
pub const OP_GET: u8 = 0x02;
pub const OP_PUT: u8 = 0x03;
pub const OP_DELETE: u8 = 0x04;
pub const OP_DNAQL: u8 = 0x10;
pub const OP_SQL: u8 = 0x11;
pub const OP_CHECKPOINT: u8 = 0x12;

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Auth { user: String, pass: String },
    Put { key: Vec<u8>, value: Vec<u8> },
    Get { key: Vec<u8> },
    Delete { key: Vec<u8> },
    DnaQlQuery { query: String },
    SqlQuery { query: String },
    Ping,
    Checkpoint,
}

/// Parse a payload into a Request. Returns Err on malformed frames.
pub fn parse_payload(payload: &[u8]) -> Result<Request, String> {
    if payload.is_empty() {
        return Err("empty payload".into());
    }
    let op = payload[0];
    let rest = &payload[1..];
    match op {
        OP_AUTH => {
            let s = String::from_utf8_lossy(rest);
            let (user, pass) = s
                .split_once(':')
                .ok_or("AUTH payload must be 'user:pass'")?;
            Ok(Request::Auth {
                user: user.to_string(),
                pass: pass.to_string(),
            })
        }
        OP_GET => Ok(Request::Get { key: rest.to_vec() }),
        OP_PUT => {
            if rest.len() < 2 {
                return Err("PUT payload too short".into());
            }
            let klen = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            if rest.len() < 2 + klen {
                return Err("PUT key truncated".into());
            }
            Ok(Request::Put {
                key: rest[2..2 + klen].to_vec(),
                value: rest[2 + klen..].to_vec(),
            })
        }
        OP_DELETE => Ok(Request::Delete { key: rest.to_vec() }),
        OP_DNAQL => Ok(Request::DnaQlQuery {
            query: String::from_utf8_lossy(rest).to_string(),
        }),
        OP_SQL => Ok(Request::SqlQuery {
            query: String::from_utf8_lossy(rest).to_string(),
        }),
        OP_PING => Ok(Request::Ping),
        OP_CHECKPOINT => Ok(Request::Checkpoint),
        other => Err(format!("unknown opcode 0x{:02X}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth() {
        let r = parse_payload(b"\x01admin:password").unwrap();
        assert_eq!(
            r,
            Request::Auth {
                user: "admin".into(),
                pass: "password".into()
            }
        );
    }

    #[test]
    fn parse_put_get_delete() {
        let mut p = vec![OP_PUT];
        p.extend_from_slice(&3u16.to_le_bytes());
        p.extend_from_slice(b"key");
        p.extend_from_slice(b"value");
        assert_eq!(
            parse_payload(&p).unwrap(),
            Request::Put {
                key: b"key".to_vec(),
                value: b"value".to_vec()
            }
        );

        let g = [OP_GET, b'k'];
        assert_eq!(
            parse_payload(&g).unwrap(),
            Request::Get { key: b"k".to_vec() }
        );

        let d = [OP_DELETE, b'k'];
        assert_eq!(
            parse_payload(&d).unwrap(),
            Request::Delete { key: b"k".to_vec() }
        );
    }

    #[test]
    fn reject_unknown_opcode() {
        assert!(parse_payload(&[0x7F]).is_err());
        assert!(parse_payload(&[]).is_err());
    }
}
