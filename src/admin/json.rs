// SPDX-License-Identifier: Apache-2.0

//! JSON and encoding helpers for the admin panel API: row serialization,
//! CSP inline-script hashing and the hex / base64 / percent codecs.

use super::*;
use crate::dna_ql::DnaQlRow;
use crate::sql_engine::QueryResult;
use sha2::{Digest, Sha256};

// ---------------- JSON helpers ----------------

fn value_json(v: &V) -> J {
    match v {
        V::Null => J::Null,
        V::Int(i) => json!(i),
        V::Float(f) if f.is_finite() => json!(f),
        V::Float(_) => J::Null,
        V::Text(s) | V::Dna(s) => json!(s),
    }
}

pub(crate) fn rows_json(rows: &[std::collections::BTreeMap<String, V>]) -> Vec<J> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (k, v) in row {
                obj.insert(k.clone(), value_json(v));
            }
            J::Object(obj)
        })
        .collect()
}

pub(crate) fn dnaql_rows_json(rows: &[DnaQlRow]) -> Vec<J> {
    rows.iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (k, v) in &row.fields {
                obj.insert(k.clone(), value_json(v));
            }
            J::Object(obj)
        })
        .collect()
}

/// CMS edition: JSON shape of a gate result (superset of sql_result_json —
/// affected_rows / last_insert_id let wpdb-style clients skip extra queries).
#[cfg(feature = "cms")]
pub(crate) fn cms_result_json(res: &crate::cms::compat::CmsQueryResult) -> J {
    let rows: Vec<J> = res
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for col in &res.columns {
                let v = match row.get(col) {
                    None | Some(crate::relational::Value::Null) => serde_json::Value::Null,
                    Some(crate::relational::Value::Int(i)) => json!(i),
                    Some(crate::relational::Value::Float(f)) => json!(f),
                    Some(crate::relational::Value::Text(s))
                    | Some(crate::relational::Value::Dna(s)) => json!(s),
                };
                obj.insert(col.clone(), v);
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    json!({
        "kind": "sql",
        "columns": res.columns,
        "rows": rows,
        "message": res.message,
        "plan": res.plan,
        "affected_rows": res.affected_rows,
        "last_insert_id": res.last_insert_id,
        "found_rows": res.found_rows,
    })
}

pub(crate) fn sql_result_json(res: &QueryResult) -> J {
    json!({
        "kind": "sql",
        "columns": res.columns,
        "rows": rows_json(&res.rows),
        "plan": res.execution_plan,
        "time_us": res.execution_time_us,
        "message": res.message,
    })
}

pub(crate) fn is_safe_ident(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// CSP hashes for inline `<script>` blocks. Hash-based allowlisting keeps
/// `script-src` free of `'unsafe-inline'` even though the SPA ships as one
/// embedded HTML document.
pub(crate) fn inline_script_hashes(body: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("<script>") {
        let after = &rest[open + "<script>".len()..];
        let Some(close) = after.find("</script>") else {
            break;
        };
        let after_bytes = after.as_bytes();
        let digest = Sha256::digest(&after_bytes[..close]);
        hashes.push(format!("'sha256-{}'", base64_encode(&digest)));
        rest = &after[close + "</script>".len()..];
    }
    hashes
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub(crate) fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
