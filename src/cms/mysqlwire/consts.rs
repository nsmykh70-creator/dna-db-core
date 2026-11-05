// SPDX-License-Identifier: AGPL-3.0-only
//! MySQL wire protocol constants: capability flags, column types/flags,
//! collations, COM_* commands, error codes and packet size limits.


pub(crate) const CLIENT_LONG_PASSWORD: u32 = 1;
pub(crate) const CLIENT_FOUND_ROWS: u32 = 2;
pub(crate) const CLIENT_LONG_FLAG: u32 = 4;
pub(crate) const CLIENT_CONNECT_WITH_DB: u32 = 8;
pub(crate) const CLIENT_PROTOCOL_41: u32 = 512;
pub(crate) const CLIENT_TRANSACTIONS: u32 = 8192;
pub(crate) const CLIENT_SECURE_CONNECTION: u32 = 32_768;
pub(crate) const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
pub(crate) const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
pub(crate) const CLIENT_PS_MULTI_RESULTS: u32 = 1 << 18;
pub(crate) const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
pub(crate) const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
pub(crate) const CLIENT_PLUGIN_AUTH_LENENC: u32 = 1 << 21;

/// Capabilities advertised in HandshakeV10. Deliberately WITHOUT:
/// * `CLIENT_SSL`  — no TLS inside the wire yet (TLS termination belongs to a
///   sidecar, exactly like pgwire); clients fall back to plaintext.
/// * `CLIENT_COMPRESS` — compression protocol is not implemented.
/// * `CLIENT_DEPRECATE_EOF` — we send classic EOF packets (every PHP driver
///   accepts both, classic is the safer default).
pub(crate) const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
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
pub(crate) const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub(crate) const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

/// Column types (protocol constants).
pub(crate) const MYSQL_TYPE_DOUBLE: u8 = 5;
pub(crate) const MYSQL_TYPE_NULL: u8 = 6;
pub(crate) const MYSQL_TYPE_LONGLONG: u8 = 8;
pub(crate) const MYSQL_TYPE_NEWDECIMAL: u8 = 246;
pub(crate) const MYSQL_TYPE_VAR_STRING: u8 = 253;

/// Column definition flags.
pub(crate) const COLUMN_FLAG_NOT_NULL: u16 = 1;
pub(crate) const COLUMN_FLAG_BINARY: u16 = 128;
pub(crate) const COLUMN_FLAG_NUM: u16 = 32_768;

/// Collation ids: utf8mb4_general_ci for text, 63 (binary) for numbers.
pub(crate) const COLLATION_UTF8MB4: u16 = 45;
pub(crate) const COLLATION_BINARY: u16 = 63;

/// Commands (client → server).
pub(crate) const COM_QUIT: u8 = 0x01;
pub(crate) const COM_INIT_DB: u8 = 0x02;
pub(crate) const COM_QUERY: u8 = 0x03;
pub(crate) const COM_FIELD_LIST: u8 = 0x04;
pub(crate) const COM_STATISTICS: u8 = 0x09;
pub(crate) const COM_PING: u8 = 0x0e;
pub(crate) const COM_RESET_CONNECTION: u8 = 0x1f;
pub(crate) const COM_STMT_PREPARE: u8 = 0x16;
pub(crate) const COM_STMT_EXECUTE: u8 = 0x17;
pub(crate) const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
pub(crate) const COM_STMT_CLOSE: u8 = 0x19;
pub(crate) const COM_STMT_RESET: u8 = 0x1a;

/// Extra errnos that only the wire layer produces (not in `cms::errno`).
pub(crate) const ER_UNKNOWN_COM_ERROR: u16 = 1047;
pub(crate) const ER_EMPTY_QUERY: u16 = 1065;
pub(crate) const ER_UNKNOWN_STMT_HANDLER: u16 = 1243;
pub(crate) const ER_UNSUPPORTED_PS: u16 = 1295;

pub(crate) const MAX_PACKET_PAYLOAD: usize = 0xFF_FF_FF;

