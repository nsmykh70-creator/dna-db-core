// SPDX-License-Identifier: AGPL-3.0-only
//! Wire protocol unit tests: codecs, auth scramble, statement splitting,
//! parameter substitution, packet shapes.

use super::*;

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
