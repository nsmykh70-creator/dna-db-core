// SPDX-License-Identifier: AGPL-3.0-only
//! COM_QUERY: gate execution, text resultset serialization and
//! COM_FIELD_LIST handling.

use super::*;

// ---------------------------------------------------------------------------
// COM_QUERY + resultset encoding
// ---------------------------------------------------------------------------

pub(crate) fn handle_query(
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

pub(crate) fn encode_gate_result(
    conn: &mut Conn,
    res: &CmsQueryResult,
    more: bool,
) -> std::io::Result<()> {
    let status = SERVER_STATUS_AUTOCOMMIT | if more { SERVER_MORE_RESULTS_EXISTS } else { 0 };
    if res.is_select && !res.columns.is_empty() {
        write_text_resultset(conn, &res.columns, &res.rows, &res.decimal_scales, status)
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
pub(crate) fn write_text_resultset(
    conn: &mut Conn,
    columns: &[String],
    rows: &[BTreeMap<String, Value>],
    scales: &[Option<u8>],
    status: u16,
) -> std::io::Result<()> {
    // Decimal-resolved columns carry their declared scale: MySQL reports
    // MYSQL_TYPE_NEWDECIMAL with decimals=s and transmits fixed-scale
    // strings (the gate has already rendered the values).
    let types: Vec<(u8, u8)> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| match scales.get(i).copied().flatten() {
            Some(scale) => (MYSQL_TYPE_NEWDECIMAL, scale),
            None => (infer_type(rows, c), 0),
        })
        .collect();
    let mut head = Vec::with_capacity(8);
    write_lenenc_int(&mut head, columns.len() as u64);
    conn.write_packet(&head)?;
    for (i, col) in columns.iter().enumerate() {
        conn.write_packet(&column_def_packet(
            "", "", col, col, types[i].0, 0, types[i].1, false,
        ))?;
    }
    conn.write_packet(&eof_packet(status))?;
    for row in rows {
        let mut p = Vec::with_capacity(columns.len() * 8);
        for (i, col) in columns.iter().enumerate() {
            match row.get(col) {
                None | Some(Value::Null) => p.push(0xfb),
                Some(Value::Int(v)) => write_lenenc_bytes(&mut p, v.to_string().as_bytes()),
                Some(Value::Float(v)) => {
                    // Safety net: the gate formats decimal columns, but a
                    // float value under a decimal column must not leak the
                    // DOUBLE rendering into a NEWDECIMAL field.
                    let text = match scales.get(i).copied().flatten() {
                        Some(scale) => {
                            crate::cms::compat::decimal::fixed_decimal(*v, scale)
                        }
                        None => fmt_float(*v),
                    };
                    write_lenenc_bytes(&mut p, text.as_bytes());
                }
                Some(Value::Text(s) | Value::Dna(s)) => write_lenenc_bytes(&mut p, s.as_bytes()),
            }
        }
        conn.write_packet(&p)?;
    }
    conn.write_packet(&eof_packet(status))?;
    Ok(())
}

/// COM_FIELD_LIST (mysql_list_fields): column definitions for one table.
pub(crate) fn handle_field_list(
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
                let decl = row
                    .get("Type")
                    .map(|v| match v {
                        Value::Text(s) | Value::Dna(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                let ty = mysql_wire_type_of_decl(&decl);
                let decimals = crate::cms::ddl_meta::decimal_scale(&decl).unwrap_or(0);
                let flags = if row.get("Key").and_then(key_flag).unwrap_or(false) {
                    COLUMN_FLAG_NOT_NULL
                } else {
                    0
                };
                conn.write_packet(&column_def_packet(
                    "", &table, &name, &name, ty, flags, decimals, true,
                ))?;
            }
            conn.write_packet(&eof_packet(SERVER_STATUS_AUTOCOMMIT))?;
            Ok(())
        }
        Err(ce) => conn.write_packet(&err_packet(ce.errno, ce.sqlstate(), &ce.message)),
    }
}

pub(crate) fn key_flag(v: &Value) -> Option<bool> {
    match v {
        Value::Text(s) | Value::Dna(s) => Some(!s.is_empty()),
        _ => None,
    }
}

/// MySQL type string (e.g. `bigint(20) unsigned`, `varchar(191)`) → wire type.
pub(crate) fn mysql_wire_type_of_decl(decl: &str) -> u8 {
    let head: String = decl
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    match head.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            MYSQL_TYPE_LONGLONG
        }
        "double" | "float" | "real" => MYSQL_TYPE_DOUBLE,
        "decimal" | "numeric" => MYSQL_TYPE_NEWDECIMAL,
        _ => MYSQL_TYPE_VAR_STRING,
    }
}

