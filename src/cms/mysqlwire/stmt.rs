// SPDX-License-Identifier: AGPL-3.0-only
//! Prepared statements (COM_STMT_PREPARE / EXECUTE / SEND_LONG_DATA /
//! CLOSE): binary protocol resultsets and parameter decoding/rendering.

use super::*;

// ---------------------------------------------------------------------------
// Prepared statements (COM_STMT_*)
// ---------------------------------------------------------------------------

/// One server-side prepared statement per connection.
pub(crate) struct Prepared {
    pub(super) sql: String,
    pub(super) params: usize,
    /// COM_STMT_SEND_LONG_DATA buffers keyed by param index.
    pub(super) long_data: HashMap<u16, Vec<u8>>,
    /// Binary parameter types `(type, unsigned)` from the first
    /// COM_STMT_EXECUTE. Real drivers (mysqli, Connector/C, Connector/Py)
    /// send the type block only once (`new_params_bound = 1`) and rely on
    /// the server to remember it on every later execute of the same
    /// statement — multi-row prepared INSERT loops depend on this.
    pub(super) param_types: Vec<(u8, u8)>,
}

/// A decoded COM_STMT_EXECUTE parameter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParamValue {
    Null,
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
}

impl ParamValue {
    /// Renders as an SQL literal for placeholder substitution.
    pub(super) fn render(&self) -> String {
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
pub(crate) fn stmt_prepare(
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
pub(crate) fn stmt_execute(
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
pub(crate) fn stmt_send_long_data(body: &[u8], stmts: &mut HashMap<u32, Prepared>) {
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
pub(crate) fn write_binary_resultset(
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
pub(crate) fn decode_binary_param(
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

