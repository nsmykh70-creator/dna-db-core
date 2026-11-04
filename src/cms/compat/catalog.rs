// SPDX-License-Identifier: AGPL-3.0-only
//! information_schema overlay: SCHEMATA / TABLES / COLUMNS / STATISTICS /
//! KEY_COLUMN_USAGE virtualized over the DNA-DB schema store, with MySQL
//! projection filtering (SELECT col FROM information_schema.TABLES WHERE …).

use std::collections::BTreeMap;

use super::*;
use crate::cms::ddl_meta::split_top_level;
use crate::cms::ddl_meta::CmsTableMeta;
use crate::relational::Value;

impl CmsSqlGate {
    // ---------------- SHOW shims ----------------

    // ------------------------------------------------------------------
    // information_schema catalog probes (MediaWiki / Drupal)
    // ------------------------------------------------------------------

    /// Answers `SELECT … FROM information_schema.{schemata,tables,columns,
    /// statistics}` from the metadata overlay. Rows are built with the
    /// standard MySQL catalog columns; the SELECT projection is applied on
    /// top so both `SELECT 1 FROM …` probes and named-column introspection
    /// come back shaped like real MySQL.
    pub(crate) fn try_information_schema(
        &self,
        session: u64,
        sql: &str,
    ) -> Result<Option<CmsQueryResult>, CmsError> {
        let (table, is_columns, is_schemata) = if from_information_schema(sql, "schemata") {
            ("schemata", false, true)
        } else if from_information_schema(sql, "tables") {
            ("tables", false, false)
        } else if from_information_schema(sql, "columns") {
            ("columns", true, false)
        } else if from_information_schema(sql, "statistics") {
            ("statistics", false, false)
        } else {
            return Ok(None);
        };

        let pairs = where_eq_pairs(sql);
        let get = |k: &str| -> Option<String> {
            pairs
                .iter()
                .find(|(c, _)| c.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        let schema = get("table_schema")
            .or_else(|| get("schema_name"))
            .or_else(|| get("index_schema"))
            .or_else(|| {
                self.with_session(session, |s| s.database.clone())
                    .unwrap_or(None)
            })
            .unwrap_or_else(|| "wordpress".to_string());

        let full_rows: Vec<BTreeMap<String, Value>> = if is_schemata {
            let name = get("schema_name").unwrap_or_else(|| schema.clone());
            vec![info_schema_schemata_row(&name)]
        } else if table == "tables" {
            let wanted = get("table_name");
            let mut rows = Vec::new();
            for t in self.engine.list_tables() {
                if let Some(w) = &wanted {
                    if w != &t {
                        continue;
                    }
                }
                rows.push(info_schema_tables_row(&schema, &t));
            }
            rows
        } else {
            // columns / statistics — need the DDL overlay for exact types.
            let tname = match get("table_name") {
                Some(t) => t,
                None => return Ok(None),
            };
            let meta = self
                .meta
                .read()
                .expect("meta poisoned")
                .get(&tname)
                .cloned();
            let Some(meta) = meta else {
                // Unknown table: MySQL answers an empty set (probes treat
                // that as "missing", which is the correct CMS answer).
                return Ok(Some(empty_catalog_result(table)));
            };
            if is_columns {
                info_schema_columns_rows(&schema, &meta)
            } else {
                info_schema_statistics_rows(&schema, &meta)
            }
        };

        apply_catalog_projection(sql, full_rows, table)
    }
}

// ---------------------------------------------------------------------------
// information_schema catalog helpers
// ---------------------------------------------------------------------------

/// True when the FROM clause references `information_schema.<tbl>` in any
/// spelling the drivers produce (`information_schema.schemata`,
/// `information_schema.`SCHEMATA``, `` `information_schema`.`schemata` ``).
pub(crate) fn from_information_schema(sql: &str, tbl: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    [
        format!("information_schema.{}", tbl),
        format!("information_schema.`{}`", tbl),
        format!("`information_schema`.`{}`", tbl),
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Splits a WHERE tail on top-level `AND` (quote- and paren-aware).
pub(crate) fn split_on_and(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_quote: Option<char> = None;
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        match in_quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    in_quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    in_quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    cur.push(c);
                }
                _ if depth == 0 && c.to_ascii_uppercase() == 'A' => {
                    // lookahead for a standalone AND
                    let rest: String = chars.clone().take(3).collect();
                    let before = cur
                        .chars()
                        .last()
                        .map(|c| c.is_whitespace())
                        .unwrap_or(true);
                    let after = rest
                        .chars()
                        .next()
                        .map(|c| c.is_whitespace())
                        .unwrap_or(false);
                    if before && after && rest.to_ascii_uppercase().starts_with("AND") {
                        for _ in 0..3 {
                            chars.next();
                        }
                        out.push(cur.trim().to_string());
                        cur.clear();
                    } else {
                        cur.push(c);
                    }
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Top-level `col = 'literal'` / `col = 123` pairs from the statement's WHERE
/// clause — the shape every catalog probe uses.
pub(crate) fn where_eq_pairs(sql: &str) -> Vec<(String, String)> {
    let lower = sql.to_ascii_lowercase();
    let Some(wpos) = lower.find(" where ") else {
        return Vec::new();
    };
    let mut tail = &sql[wpos + 7..];
    // Cut trailing clause keywords (catalog probes never nest these in strings).
    let lower_tail = tail.to_ascii_lowercase();
    for stop in [" group by", " order by", " limit", " having"] {
        if let Some(p) = lower_tail.find(stop) {
            tail = &tail[..p];
        }
    }
    let mut out = Vec::new();
    for cond in split_on_and(tail) {
        // Catalog probes wrap each condition in parens
        // (`("table_schema" = 'x') AND (...)`) — strip balanced outer
        // parens so the top-level '=' scan sees them.
        let cond = strip_balanced_parens(&cond);
        // top-level '=' only
        let mut depth = 0usize;
        let mut in_quote: Option<char> = None;
        let mut eq: Option<usize> = None;
        for (i, c) in cond.char_indices() {
            match in_quote {
                Some(q) => {
                    if c == q {
                        in_quote = None;
                    }
                }
                None => match c {
                    '\'' | '"' | '`' => in_quote = Some(c),
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    '=' if depth == 0 => {
                        eq = Some(i);
                        break;
                    }
                    _ => {}
                },
            }
        }
        let Some(i) = eq else { continue };
        let col = cond[..i]
            .trim()
            .trim_matches(|c| c == '`' || c == '"')
            .to_ascii_lowercase();
        let val = cond[i + 1..].trim();
        let decoded = if (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
            || (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
        {
            decode_mysql_string(&val[1..val.len() - 1]).to_string()
        } else {
            val.to_string()
        };
        out.push((col, decoded));
    }
    out
}

/// Strips one level of balanced outer parentheses (`( a = 'b' )` → `a = 'b'`).
pub(crate) fn strip_balanced_parens(s: &str) -> &str {
    let t = s.trim();
    if !t.starts_with('(') || !t.ends_with(')') || t.len() < 2 {
        return t;
    }
    let bytes = t.as_bytes();
    let mut depth = 0usize;
    let mut in_q: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match in_q {
            Some(q) => {
                if b == q {
                    in_q = None;
                }
            }
            None => match b {
                b'\'' | b'"' | b'`' => in_q = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    // The opening paren must close at the LAST byte.
                    if depth == 0 && i != bytes.len() - 1 {
                        return t; // parens do not wrap the whole condition
                    }
                }
                _ => {}
            },
        }
    }
    &t[1..t.len() - 1]
}

/// Standard `information_schema.schemata` row.
pub(crate) fn info_schema_schemata_row(name: &str) -> BTreeMap<String, Value> {
    let mut r = BTreeMap::new();
    r.insert("CATALOG_NAME".into(), Value::Text("def".into()));
    r.insert("SCHEMA_NAME".into(), Value::Text(name.to_string()));
    r.insert(
        "DEFAULT_CHARACTER_SET_NAME".into(),
        Value::Text("utf8mb4".into()),
    );
    r.insert(
        "DEFAULT_COLLATION_NAME".into(),
        Value::Text("utf8mb4_general_ci".into()),
    );
    r.insert("SQL_PATH".into(), Value::Null);
    r
}

/// Standard `information_schema.tables` row (static storage facts).
pub(crate) fn info_schema_tables_row(schema: &str, table: &str) -> BTreeMap<String, Value> {
    let mut r = BTreeMap::new();
    r.insert("TABLE_CATALOG".into(), Value::Text("def".into()));
    r.insert("TABLE_SCHEMA".into(), Value::Text(schema.to_string()));
    r.insert("TABLE_NAME".into(), Value::Text(table.to_string()));
    r.insert("TABLE_TYPE".into(), Value::Text("BASE TABLE".into()));
    r.insert("ENGINE".into(), Value::Text("InnoDB".into()));
    r.insert("VERSION".into(), Value::Int(10));
    r.insert("ROW_FORMAT".into(), Value::Text("Dynamic".into()));
    r.insert("TABLE_ROWS".into(), Value::Int(0));
    r.insert("AVG_ROW_LENGTH".into(), Value::Int(0));
    r.insert("DATA_LENGTH".into(), Value::Int(16384));
    r.insert("MAX_DATA_LENGTH".into(), Value::Int(0));
    r.insert("INDEX_LENGTH".into(), Value::Int(0));
    r.insert("DATA_FREE".into(), Value::Int(0));
    r.insert("AUTO_INCREMENT".into(), Value::Null);
    r.insert(
        "TABLE_COLLATION".into(),
        Value::Text("utf8mb4_general_ci".into()),
    );
    r.insert("CREATE_TIME".into(), Value::Null);
    r.insert("UPDATE_TIME".into(), Value::Null);
    r.insert("CHECK_TIME".into(), Value::Null);
    r.insert("TABLE_COMMENT".into(), Value::Text(String::new()));
    r
}

/// Base type name of a raw MySQL type text: `bigint(20) unsigned` → `bigint`.
pub(crate) fn base_type(mysql_type: &str) -> &str {
    mysql_type
        .split(['(', ' '])
        .next()
        .unwrap_or(mysql_type)
        .trim()
}

/// First parenthesised argument of the type text, e.g. `varchar(191)` → 191.
pub(crate) fn type_len(mysql_type: &str) -> Option<u64> {
    let open = mysql_type.find('(')?;
    let close = mysql_type[open..].find(')')? + open;
    mysql_type[open + 1..close]
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Standard `information_schema.columns` rows from the DDL overlay.
pub(crate) fn info_schema_columns_rows(
    schema: &str,
    meta: &CmsTableMeta,
) -> Vec<BTreeMap<String, Value>> {
    let mut rows = Vec::new();
    for (i, c) in meta.columns.iter().enumerate() {
        let mut r = BTreeMap::new();
        r.insert("TABLE_CATALOG".into(), Value::Text("def".into()));
        r.insert("TABLE_SCHEMA".into(), Value::Text(schema.to_string()));
        r.insert("TABLE_NAME".into(), Value::Text(meta.table.clone()));
        r.insert("COLUMN_NAME".into(), Value::Text(c.field.clone()));
        r.insert("ORDINAL_POSITION".into(), Value::Int(i as i64 + 1));
        r.insert(
            "COLUMN_DEFAULT".into(),
            match &c.default {
                Some(d) => {
                    let d = d.trim_matches('\'');
                    Value::Text(d.to_string())
                }
                None => Value::Null,
            },
        );
        r.insert(
            "IS_NULLABLE".into(),
            Value::Text(if c.not_null { "NO" } else { "YES" }.into()),
        );
        let bt = base_type(&c.mysql_type).to_string();
        let char_like = matches!(
            bt.as_str(),
            "char"
                | "varchar"
                | "binary"
                | "varbinary"
                | "text"
                | "tinytext"
                | "mediumtext"
                | "longtext"
                | "blob"
                | "tinyblob"
                | "mediumblob"
                | "longblob"
                | "enum"
                | "set"
        );
        let numeric = matches!(
            bt.as_str(),
            "int"
                | "integer"
                | "bigint"
                | "smallint"
                | "tinyint"
                | "mediumint"
                | "decimal"
                | "numeric"
                | "float"
                | "double"
                | "real"
        );
        if char_like {
            let len = type_len(&c.mysql_type).unwrap_or(255);
            r.insert("CHARACTER_MAXIMUM_LENGTH".into(), Value::Int(len as i64));
            r.insert("CHARACTER_OCTET_LENGTH".into(), Value::Int(len as i64 * 4));
            r.insert("CHARACTER_SET_NAME".into(), Value::Text("utf8mb4".into()));
            r.insert(
                "COLLATION_NAME".into(),
                Value::Text("utf8mb4_general_ci".into()),
            );
        } else {
            r.insert("CHARACTER_MAXIMUM_LENGTH".into(), Value::Null);
            r.insert("CHARACTER_OCTET_LENGTH".into(), Value::Null);
            r.insert("CHARACTER_SET_NAME".into(), Value::Null);
            r.insert("COLLATION_NAME".into(), Value::Null);
        }
        if numeric {
            match bt.as_str() {
                "decimal" | "numeric" => {
                    let open = c.mysql_type.find('(');
                    let prec_scale: Vec<&str> = match open {
                        Some(o) => c.mysql_type[o + 1..c.mysql_type[o..].find(')').unwrap() + o]
                            .split(',')
                            .collect(),
                        None => vec!["10", "0"],
                    };
                    r.insert(
                        "NUMERIC_PRECISION".into(),
                        Value::Int(
                            prec_scale
                                .first()
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(10),
                        ),
                    );
                    r.insert(
                        "NUMERIC_SCALE".into(),
                        Value::Int(
                            prec_scale
                                .get(1)
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(0),
                        ),
                    );
                }
                "float" => {
                    r.insert("NUMERIC_PRECISION".into(), Value::Int(12));
                    r.insert("NUMERIC_SCALE".into(), Value::Null);
                }
                _ => {
                    r.insert("NUMERIC_PRECISION".into(), Value::Int(20));
                    r.insert("NUMERIC_SCALE".into(), Value::Int(0));
                }
            }
        } else {
            r.insert("NUMERIC_PRECISION".into(), Value::Null);
            r.insert("NUMERIC_SCALE".into(), Value::Null);
        }
        r.insert("DATETIME_PRECISION".into(), Value::Null);
        r.insert("COLUMN_TYPE".into(), Value::Text(c.mysql_type.clone()));
        // Key classification, dbDelta-compatible.
        let key = if meta.primary_key.iter().any(|k| k == &c.field) {
            "PRI"
        } else if meta
            .indexes
            .iter()
            .any(|ix| ix.unique && ix.columns.len() == 1 && ix.columns[0].0 == c.field)
        {
            "UNI"
        } else if meta
            .indexes
            .iter()
            .any(|ix| ix.columns.iter().any(|(cn, _)| cn == &c.field))
        {
            "MUL"
        } else {
            ""
        };
        r.insert("COLUMN_KEY".into(), Value::Text(key.into()));
        r.insert(
            "EXTRA".into(),
            Value::Text(
                if c.auto_increment {
                    "auto_increment"
                } else {
                    ""
                }
                .into(),
            ),
        );
        r.insert(
            "PRIVILEGES".into(),
            Value::Text("select,insert,update,references".into()),
        );
        r.insert("COLUMN_COMMENT".into(), Value::Text(String::new()));
        r.insert("GENERATION_EXPRESSION".into(), Value::Text(String::new()));
        rows.push(r);
    }
    rows
}

/// Standard `information_schema.statistics` rows from the DDL overlay
/// (primary key first, then secondary indexes in DDL order).
pub(crate) fn info_schema_statistics_rows(
    schema: &str,
    meta: &CmsTableMeta,
) -> Vec<BTreeMap<String, Value>> {
    let mut rows = Vec::new();
    let mut push_index = |name: &str, unique: bool, cols: &[(String, Option<u16>)]| {
        for (seq, (col, prefix)) in cols.iter().enumerate() {
            let mut r = BTreeMap::new();
            r.insert("TABLE_CATALOG".into(), Value::Text("def".into()));
            r.insert("TABLE_SCHEMA".into(), Value::Text(schema.to_string()));
            r.insert("TABLE_NAME".into(), Value::Text(meta.table.clone()));
            r.insert("NON_UNIQUE".into(), Value::Int(if unique { 0 } else { 1 }));
            r.insert("INDEX_SCHEMA".into(), Value::Text(schema.to_string()));
            r.insert("INDEX_NAME".into(), Value::Text(name.to_string()));
            r.insert("SEQ_IN_INDEX".into(), Value::Int(seq as i64 + 1));
            r.insert("COLUMN_NAME".into(), Value::Text(col.clone()));
            r.insert("COLLATION".into(), Value::Text("A".into()));
            r.insert("CARDINALITY".into(), Value::Int(0));
            r.insert(
                "SUB_PART".into(),
                match prefix {
                    Some(p) => Value::Int(*p as i64),
                    None => Value::Null,
                },
            );
            r.insert("PACKED".into(), Value::Null);
            let not_null = meta
                .columns
                .iter()
                .find(|c| c.field == *col)
                .map(|c| c.not_null)
                .unwrap_or(false);
            r.insert(
                "NULLABLE".into(),
                Value::Text(if not_null { "" } else { "YES" }.into()),
            );
            r.insert("INDEX_TYPE".into(), Value::Text("BTREE".into()));
            r.insert("COMMENT".into(), Value::Text(String::new()));
            r.insert("INDEX_COMMENT".into(), Value::Text(String::new()));
            rows.push(r);
        }
    };
    if !meta.primary_key.is_empty() {
        let pk: Vec<(String, Option<u16>)> =
            meta.primary_key.iter().map(|c| (c.clone(), None)).collect();
        push_index("PRIMARY", true, &pk);
    }
    for ix in &meta.indexes {
        push_index(&ix.name, ix.unique, &ix.columns);
    }
    rows
}

/// Empty catalog result for a table the overlay does not know (MySQL answers
/// an empty set, which is how drivers detect "table missing").
pub(crate) fn empty_catalog_result(table: &str) -> CmsQueryResult {
    CmsQueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: Some(format!("information_schema.{}: empty", table)),
        plan: "cms: information_schema shim (empty)".into(),
        affected_rows: 0,
        last_insert_id: 0,
        found_rows: 0,
        is_select: true,
    }
}

/// What the catalog query asked to see.
pub(crate) enum CatalogProj {
    Star,
    Literal(String),
    Cols(Vec<String>),
}

/// The SELECT list of a catalog probe (between SELECT and top-level FROM).
pub(crate) fn select_projection(sql: &str) -> CatalogProj {
    let lower = sql.to_ascii_lowercase();
    let Some(sel) = lower.find("select") else {
        return CatalogProj::Star;
    };
    let rest_lower = &lower[sel + 6..];
    let Some(from_rel) = rest_lower.find(" from ") else {
        return CatalogProj::Star;
    };
    let list = sql[sel + 6..sel + 6 + from_rel].trim();
    if list == "*" {
        return CatalogProj::Star;
    }
    // Single literal (number / string / NULL) — the MW probe shape.
    let single = list.trim();
    if !(single.contains(',') || single.contains('(')) {
        let low = single.to_ascii_lowercase();
        if low == "1"
            || low.parse::<i64>().is_ok()
            || (low.starts_with('\'') && low.ends_with('\'') && low.len() >= 2)
            || low == "null"
        {
            return CatalogProj::Literal(single.to_string());
        }
    }
    let cols = split_top_level(list)
        .iter()
        .map(|c| {
            let c = c.trim();
            // honour `expr AS alias`
            let up = c.to_ascii_uppercase();
            match up.rfind(" AS ") {
                Some(p) => c[p + 4..].trim().to_string(),
                None => c.to_string(),
            }
        })
        .collect();
    CatalogProj::Cols(cols)
}

/// Applies the projection to full catalog rows and wraps them in a result.
pub(crate) fn apply_catalog_projection(
    sql: &str,
    full_rows: Vec<BTreeMap<String, Value>>,
    table: &str,
) -> Result<Option<CmsQueryResult>, CmsError> {
    match select_projection(sql) {
        CatalogProj::Star => {
            let found = full_rows.len() as u64;
            Ok(Some(CmsQueryResult {
                columns: full_rows
                    .first()
                    .map(|r| r.keys().cloned().collect())
                    .unwrap_or_default(),
                rows: full_rows,
                message: Some(format!("information_schema.{}: {} row(s)", table, found)),
                plan: "cms: information_schema shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }))
        }
        CatalogProj::Literal(lit) => {
            let low = lit.to_ascii_lowercase();
            let (col, val) = if low == "null" {
                (lit.clone(), Value::Null)
            } else if low.starts_with('\'') {
                (
                    lit.clone(),
                    Value::Text(decode_mysql_string(&lit[1..lit.len() - 1]).to_string()),
                )
            } else {
                match lit.parse::<i64>() {
                    Ok(n) => (lit.clone(), Value::Int(n)),
                    Err(_) => (lit.clone(), Value::Text(lit.clone())),
                }
            };
            let rows: Vec<BTreeMap<String, Value>> = full_rows
                .iter()
                .map(|_| {
                    let mut r = BTreeMap::new();
                    r.insert(col.clone(), val.clone());
                    r
                })
                .collect();
            let found = rows.len() as u64;
            Ok(Some(CmsQueryResult {
                columns: vec![col],
                rows,
                message: Some(format!("information_schema.{}: {} row(s)", table, found)),
                plan: "cms: information_schema shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }))
        }
        CatalogProj::Cols(names) => {
            let mut rows = Vec::new();
            for r in &full_rows {
                let mut out = BTreeMap::new();
                for n in &names {
                    let key = n.trim().trim_matches('`').to_string();
                    // MySQL catalog columns are case-insensitive; rows use
                    // the canonical UPPER names.
                    match r.get(&key).or_else(|| {
                        r.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                            .map(|(_, v)| v)
                    }) {
                        Some(v) => {
                            out.insert(key.clone(), v.clone());
                        }
                        None => {
                            out.insert(key.clone(), Value::Null);
                        }
                    }
                }
                rows.push(out);
            }
            let found = rows.len() as u64;
            Ok(Some(CmsQueryResult {
                columns: names
                    .iter()
                    .map(|n| n.trim().trim_matches('`').to_string())
                    .collect(),
                rows,
                message: Some(format!("information_schema.{}: {} row(s)", table, found)),
                plan: "cms: information_schema shim".into(),
                affected_rows: 0,
                last_insert_id: 0,
                found_rows: found,
                is_select: true,
            }))
        }
    }
}
